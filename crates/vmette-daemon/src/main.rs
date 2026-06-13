//! vmetted — long-lived UNIX-socket dispatcher for vmette.
//!
//! One request per connection, line-delimited JSON. Two subsystems share the
//! socket, both running VZ **in-process** (no forked `vmette` subprocess):
//!
//!   * stateless run — boots a one-shot capture-aware `vmette::Session` per
//!     request and streams its clean guest output back;
//!   * stateful desktop — the session registry holds live Agent-workload VMs
//!     across many `desktop_*` requests.
//!
//! ## Protocol
//!
//! Per connection:
//!
//!   client → daemon : one JSON object on a single line. Only kernel,
//!   initramfs, rootfs, and exec are required; omitted optional fields take the
//!   one true default (the daemon never re-spells them):
//!       { "kernel": "/path", "initramfs": "/path",
//!         "rootfs": "/path/to/dir | alpine:3.20 | tar+https://... | oci://...",
//!         "exec": "echo hi",
//!         "rootfs_ro": false, "offline": false, "net": false,
//!         "shares": [{"tag":"host", "path":"/p"}],
//!         "vcpus": 1, "mem_mib": 512 }
//!
//!   daemon → client : streamed JSON objects, one per line:
//!       { "kind": "stdout", "data": "..." }
//!       { "kind": "exit",   "code": 0 }
//!
//! ## CLI
//!
//!   vmetted [--socket PATH]
//!
//! Defaults:
//!   --socket  $HOME/Library/Caches/vmette/vmette.sock

mod registry;
mod view;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().context("--socket needs PATH")?.into(),
            "--version" | "-V" => {
                println!("vmetted {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                eprintln!(
                    "vmetted — UNIX socket dispatcher for vmette\n\n\
                     usage: vmetted [--socket PATH] [--version]\n\n\
                     defaults:\n  \
                       --socket  $HOME/Library/Caches/vmette/vmette.sock\n"
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
    info!(socket = %socket.display(), "vmetted listening");

    // Stateful subsystem: the desktop session registry. Kept entirely
    // separate from the stateless in-process run lane.
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
                        let registry = registry.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dispatch(stream, registry).await {
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
/// stateless in-process run lane.
async fn dispatch(stream: UnixStream, registry: Arc<Registry>) -> Result<()> {
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
        // C2: run the one-shot workload in-process (capture-aware `Session`),
        // the same substrate the desktop registry uses — no fork, no argv
        // round-trip, no console marker-scraping.
        run_workload_inproc(req, write_half).await
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

/// Run a one-shot workload IN-PROCESS via a capture-aware `vmette::Session` —
/// the same substrate the desktop registry uses. The rootfs is resolved through
/// the shared provider registry, the request maps to a `Config` via
/// `Config::from_run_request`, and the guest's output is captured on a dedicated
/// clean console (no init/kernel noise, no marker-scraping) and streamed as
/// `Frame::Stdout` followed by `Frame::Exit`. Replaces forking the `vmette` CLI.
async fn run_workload_inproc(
    req: Request,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
    // Lets the async side force-stop the VM if the client disconnects mid-run
    // (the subprocess path's `kill_on_drop` equivalent). The worker fills it
    // once the session is live.
    let stop_slot: Arc<std::sync::Mutex<Option<vmette::StopHandle>>> =
        Arc::new(std::sync::Mutex::new(None));
    let stop_for_worker = stop_slot.clone();

    // The VM runs on a blocking thread (`Session` is `!Send` and does blocking
    // VZ work); it streams frames back over the channel.
    let worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let provider = vmette_providers::default_registry();
        let ctx = vmette::provider::Context::new(vmette_assets::default_cache_root())
            .offline(req.offline)
            .guest_helpers_dir(registry::locate_guest_helpers());
        let artifact = provider
            .resolve(&req.rootfs, &ctx)
            .map_err(|e| anyhow!("resolving rootfs {}: {e}", req.rootfs))?;
        let cfg = vmette::Config::from_run_request(&req, artifact, true);
        let session = vmette::Session::start(&cfg).map_err(|e| anyhow!("session start: {e}"))?;
        *stop_for_worker.lock().unwrap() = Some(session.stop_handle());
        if let Some(chunks) = session.capture_rx() {
            for chunk in chunks {
                let data = String::from_utf8_lossy(&chunk).into_owned();
                if tx.blocking_send(Frame::Stdout { data }).is_err() {
                    break; // client gone
                }
            }
        }
        let code = match session.wait() {
            vmette::SessionEnd::Exited(c) => c,
            vmette::SessionEnd::TimedOut => 124,
            vmette::SessionEnd::Stopped => 0,
            vmette::SessionEnd::Error(_) => 1,
        };
        let _ = tx.blocking_send(Frame::Exit { code });
        Ok(())
    });

    // Forward frames to the socket as they stream.
    let mut client_gone = false;
    while let Some(frame) = rx.recv().await {
        if write_frame(&mut write_half, &frame).await.is_err() {
            client_gone = true;
            break; // client disconnected
        }
    }
    // If the client vanished mid-run, force-stop the VM so it doesn't keep
    // running to its timeout (mirrors the subprocess `kill_on_drop`).
    if client_gone {
        if let Some(h) = stop_slot.lock().unwrap().take() {
            h.stop();
        }
    }
    // Surface a setup error (resolve/start) as a terminal Error frame; the happy
    // path already sent Exit.
    match worker.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = write_frame(
                &mut write_half,
                &Frame::Error {
                    message: format!("{e:#}"),
                },
            )
            .await;
        }
        Err(e) => {
            let _ = write_frame(
                &mut write_half,
                &Frame::Error {
                    message: format!("run task panicked: {e}"),
                },
            )
            .await;
        }
    }
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
