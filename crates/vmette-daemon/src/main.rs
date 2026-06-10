//! vmetted — long-lived UNIX-socket dispatcher for vmette.
//!
//! v0.1 architecture: one request per connection, line-delimited JSON.
//! Spawns the `vmette` CLI as a subprocess per request. This avoids
//! library churn around output capture (the lib forwards guest stdio
//! straight to the daemon process's stdio); the trade-off is ~50 ms of
//! fork/exec per call.
//!
//! Future (v0.2, Apple Silicon only): in-process pool of warm
//! snapshots restored per request, dispatched via vsock. That requires
//! library changes — see Phase 5 notes in the plan.
//!
//! ## Protocol
//!
//! Per connection:
//!
//!   client → daemon : one JSON object on a single line. Only kernel,
//!   initramfs, rootfs, and exec are required; omitted optional fields take
//!   the `vmette` CLI's own default (the daemon never re-spells them):
//!       { "kernel": "/path", "initramfs": "/path",
//!         "rootfs": "/path/to/dir | alpine:3.20 | tar+https://... | oci://...",
//!         "exec": "echo hi",
//!         "rootfs_ro": false, "offline": false, "net": false,
//!         "shares": [{"tag":"host", "path":"/p"}],
//!         "vcpus": 1, "mem_mib": 512 }
//!
//!   daemon → client : streamed JSON objects, one per line:
//!       { "kind": "stdout", "data": "..." }
//!       { "kind": "stderr", "data": "..." }
//!       { "kind": "exit",   "code": 0 }
//!
//! ## CLI
//!
//!   vmetted [--socket PATH] [--vmette PATH]
//!
//! Defaults:
//!   --socket  $HOME/Library/Caches/vmette/vmette.sock
//!   --vmette  $(dirname argv[0])/vmette  (falls back to PATH lookup)

mod registry;
mod view;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use vmette_proto::agent::{Action, ResponseHeader};
use vmette_proto::daemon::{
    ActionReply, ChangedReply, DesktopReply, DesktopRequest, ErrorReply, Frame, Request,
    SessionReply, SettleReply, ViewReply,
};

use registry::{
    Registry, StartParams, DEFAULT_DESKTOP_MEM_MIB, DEFAULT_DESKTOP_VCPUS, DEFAULT_SETTLE_HOLD_MS,
    DEFAULT_SETTLE_TIMEOUT_MS,
};

/// How many concurrent desktop VMs the daemon will host. Each is a ~2 GB VM.
const MAX_DESKTOP_SESSIONS: usize = 8;
/// Force-stop desktop sessions untouched for this long (orphan/idle eviction).
const DESKTOP_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// How often the background sweeper checks for idle sessions.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

// The run + desktop protocol *types* live in `vmette-proto` (imported above);
// the desktop-session defaults the dispatch applies to unset fields (`vcpus`,
// `mem_mib`, `size`) are owned by the `registry` module (imported above). The
// `image` is resolved client-side, so the dispatch passes it through verbatim.

/// Parse "WIDTHxHEIGHT" → `Some((w, h))`; `None` on absence or parse error, so
/// the desktop `Config` default applies as the single owner of that geometry.
fn parse_size(s: Option<&str>) -> Option<(u32, u32)> {
    let (w, h) = s?.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Re-encode a desktop action's raw response (the agent's [`ResponseHeader`]
/// plus the optional binary payload) into the wire [`ActionReply`]. `want_text`
/// routes the payload: a clipboard read or `exec_capture` returns it as decoded
/// UTF-8 `text`, every other payload-bearing action (a screenshot) as a base64
/// PNG. The lone owner of the vsock→socket payload encoding — kept pure so it
/// is unit-tested.
fn action_reply(header: ResponseHeader, payload: Option<Vec<u8>>, want_text: bool) -> ActionReply {
    let (png_base64, text) = if want_text {
        (
            None,
            payload.map(|b| String::from_utf8_lossy(&b).into_owned()),
        )
    } else {
        (
            payload.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            None,
        )
    };
    ActionReply {
        ok: header.ok,
        error: header.error,
        x: header.x,
        y: header.y,
        png_base64,
        text,
        exit_code: header.exit_code,
    }
}

fn locate_vmette() -> PathBuf {
    if let Ok(p) = std::env::var("VMETTE_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("vmette");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("vmette")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();

    let mut socket = vmette_assets::default_socket();
    let mut vmette_bin = locate_vmette();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().context("--socket needs PATH")?.into(),
            "--vmette" => vmette_bin = args.next().context("--vmette needs PATH")?.into(),
            "--version" | "-V" => {
                println!("vmetted {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                eprintln!(
                    "vmetted — UNIX socket dispatcher for vmette\n\n\
                     usage: vmetted [--socket PATH] [--vmette PATH] [--version]\n\n\
                     defaults:\n  \
                       --socket  $HOME/Library/Caches/vmette/vmette.sock\n  \
                       --vmette  (next to vmetted, or PATH lookup)\n"
                );
                return Ok(());
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let _ = std::fs::remove_file(&socket); // tolerate stale leftover

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    info!(socket = %socket.display(), vmette = %vmette_bin.display(), "vmetted listening");

    // Stateful subsystem: the desktop session registry. Kept entirely
    // separate from the stateless subprocess dispatch above.
    let registry = Registry::new(
        MAX_DESKTOP_SESSIONS,
        DESKTOP_IDLE_TTL,
        vmette_assets::default_cache_root(),
    );

    // Background idle/orphan sweeper. Eviction is blocking (joins teardown),
    // so it hops off the async thread via spawn_blocking.
    {
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let reg = registry.clone();
                let evicted = tokio::task::spawn_blocking(move || reg.sweep_idle())
                    .await
                    .unwrap_or_default();
                if !evicted.is_empty() {
                    info!(?evicted, "evicted idle desktop sessions");
                }
            }
        });
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let bin = vmette_bin.clone();
                        let registry = registry.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dispatch(stream, bin, registry).await {
                                warn!(error = %e, "handler failed");
                            }
                        });
                    }
                    Err(e) => error!(error = %e, "accept failed"),
                }
            }
            _ = sigterm.recv() => { info!("SIGTERM received; draining"); break; }
            _ = sigint.recv()  => { info!("SIGINT received; draining");  break; }
        }
    }

    // Tear down every live desktop VM before exiting.
    let live = registry.len();
    if live > 0 {
        info!(live, "stopping live desktop sessions on shutdown");
        let reg = registry.clone();
        let _ = tokio::task::spawn_blocking(move || reg.stop_all()).await;
    }

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Per-connection entry point. Reads the single request line, peeks whether it
/// carries a `desktop_*` kind, and routes: desktop requests to the stateful
/// session registry, everything else (the untagged run [`Request`]) to the
/// stateless subprocess path.
async fn dispatch(stream: UnixStream, vmette_bin: PathBuf, registry: Arc<Registry>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let line = line.trim();

    // A liveness probe (the CLI/MCP `ensure_daemon` connects then drops without
    // sending anything) reads back as an empty line — not a malformed request.
    // Return quietly so it doesn't surface as a `handler failed` warning.
    if line.is_empty() {
        return Ok(());
    }

    // Peek only enough to route: a `desktop_*` kind is the stateful path; the
    // untagged run request is everything else. The concrete shape is parsed by
    // the chosen handler against its typed `vmette-proto` enum/struct.
    let is_desktop = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .is_some_and(|k| k.starts_with("desktop_"));

    if is_desktop {
        let reply = handle_desktop(line, registry).await;
        let mut json = serde_json::to_vec(&reply)?;
        json.push(b'\n');
        let _ = write_half.write_all(&json).await;
        let _ = write_half.shutdown().await;
        Ok(())
    } else {
        let req: Request = serde_json::from_str(line).context("parse run request")?;
        run_workload(req, write_half, vmette_bin).await
    }
}

/// Route a parsed desktop request to the registry, mapping results/errors to a
/// single [`DesktopReply`]. Blocking registry calls hop off the async thread
/// via `spawn_blocking`.
async fn handle_desktop(line: &str, registry: Arc<Registry>) -> DesktopReply {
    match desktop_result(line, registry).await {
        Ok(reply) => reply,
        Err(e) => DesktopReply::Error(ErrorReply {
            message: format!("{e:#}"),
        }),
    }
}

async fn desktop_result(line: &str, registry: Arc<Registry>) -> Result<DesktopReply> {
    let req: DesktopRequest = serde_json::from_str(line).context("parse desktop request")?;
    match req {
        DesktopRequest::DesktopStart(req) => {
            let params = StartParams {
                kernel: req.kernel,
                initramfs: req.initramfs,
                image: req.image,
                display_size: parse_size(req.size.as_deref()),
                net: req.net,
                offline: req.offline,
                shares: req.shares,
                vcpus: req.vcpus.unwrap_or(DEFAULT_DESKTOP_VCPUS),
                mem_mib: req.mem_mib.unwrap_or(DEFAULT_DESKTOP_MEM_MIB),
            };
            let session_id = tokio::task::spawn_blocking(move || registry.start(params))
                .await
                .context("session start task")??;
            Ok(DesktopReply::Session(SessionReply { session_id }))
        }
        DesktopRequest::DesktopAction(req) => {
            // get_clipboard returns its text as the payload; exec_capture
            // returns its captured stdout/stderr as the payload. Both want the
            // payload decoded as UTF-8 `text`; the only other payload-bearing
            // action (screenshot) returns a PNG. Decide which before the action
            // moves into the blocking task.
            let want_text = matches!(
                req.action,
                Action::GetClipboard | Action::ExecCapture { .. }
            );
            let (header, payload) =
                tokio::task::spawn_blocking(move || registry.action(&req.session_id, &req.action))
                    .await
                    .context("session action task")??;
            Ok(DesktopReply::ActionResult(action_reply(
                header, payload, want_text,
            )))
        }
        DesktopRequest::DesktopScreenshotSettled(req) => {
            let timeout =
                Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_SETTLE_TIMEOUT_MS));
            let hold = Duration::from_millis(req.stable_hold_ms.unwrap_or(DEFAULT_SETTLE_HOLD_MS));
            let res = tokio::task::spawn_blocking(move || {
                registry.screenshot_when_settled(&req.session_id, timeout, hold)
            })
            .await
            .context("settle poll task")??;
            Ok(DesktopReply::Settled(SettleReply {
                settled: res.settled,
                moving: res.moving,
                png_base64: base64::engine::general_purpose::STANDARD.encode(res.png),
            }))
        }
        DesktopRequest::DesktopWhatChanged(req) => {
            let res = tokio::task::spawn_blocking(move || registry.what_changed(&req.session_id))
                .await
                .context("what_changed task")??;
            Ok(DesktopReply::Changed(ChangedReply {
                changed: res.changed,
                png_base64: base64::engine::general_purpose::STANDARD.encode(res.png),
            }))
        }
        DesktopRequest::DesktopView(req) => {
            let addr = tokio::task::spawn_blocking(move || registry.view(&req.session_id))
                .await
                .context("session view task")??;
            Ok(DesktopReply::View(ViewReply {
                addr: addr.to_string(),
            }))
        }
        DesktopRequest::DesktopStop(req) => {
            tokio::task::spawn_blocking(move || registry.stop(&req.session_id))
                .await
                .context("session stop task")??;
            Ok(DesktopReply::Stopped)
        }
    }
}

async fn run_workload(
    req: Request,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    vmette_bin: PathBuf,
) -> Result<()> {
    // Translate Request → vmette CLI flags via the single owner of that
    // mapping in `vmette-proto`; the MCP sandbox path renders the same way.
    let mut cmd = Command::new(&vmette_bin);
    cmd.args(req.to_cli_args());

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Kill the vmette subprocess (and its VZ microVM) if this handler
    // is dropped — e.g. client disconnected mid-stream and a write_frame
    // returned BrokenPipe. Without this, the VM keeps running until its
    // natural exit (potentially --timeout = hours), leaking VZ state.
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("spawn vmette")?;
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    // Spawn one task per stream so each `read_line` runs to completion
    // and owns its BufReader. Frames flow to a single mpsc channel and
    // the main task forwards them to the socket. Avoids tokio::select!
    // cancelling read_line mid-call — AsyncBufReadExt::read_line is
    // documented NOT cancel-safe (bytes already in the BufReader can
    // be lost when the future is dropped).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);

    // read_until + from_utf8_lossy tolerates non-UTF-8 bytes (binary
    // output from xxd/tar/etc.) by replacing them with U+FFFD instead
    // of erroring out. read_line would have killed the reader task on
    // the first invalid sequence and silently truncated all subsequent
    // guest output.
    let tx_out = tx.clone();
    let out_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let data = String::from_utf8_lossy(&buf).into_owned();
                    if tx_out.send(Frame::Stdout { data }).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if !buf.is_empty() {
                        let data = String::from_utf8_lossy(&buf).into_owned();
                        let _ = tx_out.send(Frame::Stdout { data }).await;
                    }
                    let _ = tx_out
                        .send(Frame::Error {
                            message: format!("stdout: {e}"),
                        })
                        .await;
                    break;
                }
            }
        }
    });

    let tx_err = tx.clone();
    let err_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stderr);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let data = String::from_utf8_lossy(&buf).into_owned();
                    if tx_err.send(Frame::Stderr { data }).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if !buf.is_empty() {
                        let data = String::from_utf8_lossy(&buf).into_owned();
                        let _ = tx_err.send(Frame::Stderr { data }).await;
                    }
                    let _ = tx_err
                        .send(Frame::Error {
                            message: format!("stderr: {e}"),
                        })
                        .await;
                    break;
                }
            }
        }
    });

    // Drop our copy so the channel closes once both reader tasks finish.
    drop(tx);

    // Forward frames until both reader tasks finish (channel closes).
    while let Some(frame) = rx.recv().await {
        if write_frame(&mut write_half, &frame).await.is_err() {
            // Client gone — abandon the stream. kill_on_drop will tear
            // down the subprocess when this handler returns.
            return Ok(());
        }
    }
    let _ = out_task.await;
    let _ = err_task.await;

    // Always emit a terminal frame so the client can stop reading.
    // child.wait() errors get surfaced as Frame::Error rather than
    // swallowed via ?-propagation, which would leave the client
    // hanging on a socket with no exit marker.
    let exit_frame = match child.wait().await {
        Ok(status) => Frame::Exit {
            code: status.code().unwrap_or(-1),
        },
        Err(e) => Frame::Error {
            message: format!("wait: {e}"),
        },
    };
    let _ = write_frame(&mut write_half, &exit_frame).await;
    let _ = write_half.shutdown().await;
    Ok(())
}

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let mut json = serde_json::to_vec(frame)?;
    json.push(b'\n');
    w.write_all(&json).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_parses_and_rejects() {
        assert_eq!(parse_size(Some("1024x768")), Some((1024, 768)));
        assert_eq!(parse_size(Some("800X600")), Some((800, 600))); // capital X
        assert_eq!(parse_size(Some(" 1280 x 800 ")), Some((1280, 800))); // trimmed
                                                                         // Absent or unparseable → None, so the desktop Config default applies
                                                                         // as the single owner of the geometry (no daemon-side 1280x800 literal).
        assert_eq!(parse_size(None), None);
        assert_eq!(parse_size(Some("garbage")), None);
        assert_eq!(parse_size(Some("1024x")), None);
        assert_eq!(parse_size(Some("x768")), None);
    }

    #[test]
    fn action_reply_screenshot_payload_is_base64_png() {
        let r = action_reply(ResponseHeader::ok(), Some(vec![1, 2, 3]), false);
        assert!(r.ok);
        assert_eq!(r.text, None);
        assert_eq!(r.png_base64.as_deref(), Some("AQID")); // base64 of [1,2,3]
    }

    #[test]
    fn action_reply_clipboard_payload_is_text() {
        let r = action_reply(ResponseHeader::ok(), Some(b"hello".to_vec()), true);
        assert_eq!(r.text.as_deref(), Some("hello"));
        assert_eq!(r.png_base64, None);
    }

    #[test]
    fn action_reply_without_payload_carries_neither() {
        let r = action_reply(ResponseHeader::ok(), None, false);
        assert!(r.ok);
        assert_eq!(r.png_base64, None);
        assert_eq!(r.text, None);
    }

    #[test]
    fn action_reply_forwards_header_fields() {
        let header = ResponseHeader {
            ok: false,
            error: Some("boom".into()),
            x: Some(640),
            y: Some(400),
            exit_code: None,
            payload_len: 0,
        };
        let r = action_reply(header, None, false);
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert_eq!((r.x, r.y), (Some(640), Some(400)));
    }

    #[test]
    fn action_reply_exec_capture_carries_output_and_exit_code() {
        let header = ResponseHeader {
            ok: true,
            error: None,
            x: None,
            y: None,
            exit_code: Some(0),
            payload_len: 5,
        };
        let r = action_reply(header, Some(b"done\n".to_vec()), true);
        assert!(r.ok);
        assert_eq!(r.text.as_deref(), Some("done\n"));
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.png_base64, None);
    }
}
