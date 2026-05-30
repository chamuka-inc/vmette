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
//!   client → daemon : one JSON object on a single line:
//!       { "kernel": "/path", "initramfs": "/path",
//!         "rootfs": "/path/to/dir | alpine:3.20 | tar+https://... | oci://...",
//!         "rootfs_ro": false, "offline": false,
//!         "shares": [{"tag":"host", "path":"/p"}],
//!         "exec": "echo hi",
//!         "net": false, "switch_root": false,
//!         "vsock_port": 0, "guest_vsock_port": 1025,
//!         "timeout_seconds": 0, "vcpus": 1, "mem_mib": 512 }
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

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};

use registry::{Registry, StartParams, DEFAULT_DESKTOP_IMAGE};

/// How many concurrent desktop VMs the daemon will host. Each is a ~2 GB VM.
const MAX_DESKTOP_SESSIONS: usize = 8;
/// Force-stop desktop sessions untouched for this long (orphan/idle eviction).
const DESKTOP_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// How often the background sweeper checks for idle sessions.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct Request {
    kernel: PathBuf,
    initramfs: PathBuf,
    /// Rootfs spec dispatched through the CLI's provider registry.
    /// See `vmette providers` for valid forms (path, image ref, tar+...).
    rootfs: String,
    #[serde(default)]
    rootfs_ro: bool,
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    shares: Vec<ShareMount>,
    #[serde(default)]
    disks: Vec<PathBuf>,
    exec: String,
    #[serde(default)]
    net: bool,
    #[serde(default)]
    switch_root: bool,
    /// -1 disable, 0 auto, >0 fixed
    #[serde(default)]
    vsock_port: i32,
    #[serde(default = "default_guest_vsock_port")]
    guest_vsock_port: u32,
    #[serde(default)]
    timeout_seconds: Option<u32>,
    #[serde(default = "default_vcpus")]
    vcpus: u8,
    #[serde(default = "default_mem_mib")]
    mem_mib: u64,
}

fn default_guest_vsock_port() -> u32 {
    1025
}
fn default_vcpus() -> u8 {
    1
}
fn default_mem_mib() -> u64 {
    512
}

#[derive(Debug, Deserialize)]
struct ShareMount {
    tag: String,
    path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Frame {
    Stdout { data: String },
    Stderr { data: String },
    Exit { code: i32 },
    Error { message: String },
}

// ---- desktop session protocol (stateful path) ---------------------------

/// `kind: "desktop_start"` — boot a persistent desktop VM. The kernel +
/// initramfs are the ordinary vmette assets; desktop-ness comes from `image`
/// + the Agent workload.
#[derive(Debug, Deserialize)]
struct DesktopStartReq {
    kernel: PathBuf,
    initramfs: PathBuf,
    #[serde(default = "default_desktop_image")]
    image: String,
    /// "WIDTHxHEIGHT"; defaults to 1280x800 when absent/unparseable.
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    net: bool,
    #[serde(default)]
    offline: bool,
    #[serde(default = "default_desktop_vcpus")]
    vcpus: u8,
    #[serde(default = "default_desktop_mem_mib")]
    mem_mib: u64,
}

/// `kind: "desktop_action"` — one computer-use action against a live session.
#[derive(Debug, Deserialize)]
struct DesktopActionReq {
    session_id: String,
    action: vmette::Action,
}

/// `kind: "desktop_screenshot_settled"` — poll until the desktop stops changing,
/// then return that frame plus the regions still moving.
#[derive(Debug, Deserialize)]
struct DesktopSettleReq {
    session_id: String,
    /// Max time to wait for the screen to settle before returning the latest
    /// frame anyway (with `settled: false`). Defaults to 10s.
    #[serde(default = "default_settle_timeout_ms")]
    timeout_ms: u64,
}

/// `kind: "desktop_what_changed"` — capture one frame and report what moved
/// since this session's previous capture.
#[derive(Debug, Deserialize)]
struct DesktopWhatChangedReq {
    session_id: String,
}

/// `kind: "desktop_stop"` — tear a live session down.
#[derive(Debug, Deserialize)]
struct DesktopStopReq {
    session_id: String,
}

fn default_settle_timeout_ms() -> u64 {
    10_000
}

fn default_desktop_image() -> String {
    DEFAULT_DESKTOP_IMAGE.to_string()
}
fn default_desktop_vcpus() -> u8 {
    2
}
fn default_desktop_mem_mib() -> u64 {
    2048
}

/// A rectangle on the wire (pixel coords). Mirror of [`vmette::settle::Rect`],
/// which is intentionally not `Serialize` (the core stays serde-free).
#[derive(Debug, Serialize)]
struct RectJson {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl From<vmette::settle::Rect> for RectJson {
    fn from(r: vmette::settle::Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

/// Single-line JSON reply for the desktop kinds.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DesktopReply {
    Session {
        session_id: String,
    },
    ActionResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
        /// Base64 PNG for `screenshot`; absent otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        png_base64: Option<String>,
    },
    /// Reply for `desktop_screenshot_settled`: the captured frame, whether it
    /// actually settled (vs. timed out), and the regions still moving.
    Settled {
        settled: bool,
        moving: Vec<RectJson>,
        png_base64: String,
    },
    /// Reply for `desktop_what_changed`: a fresh frame and the damage box
    /// (absent when nothing changed since the previous capture).
    Changed {
        #[serde(skip_serializing_if = "Option::is_none")]
        changed: Option<RectJson>,
        png_base64: String,
    },
    Stopped,
    Error {
        message: String,
    },
}

/// Parse "WIDTHxHEIGHT" → (w, h); default 1280x800 on absence/parse error.
fn parse_size(s: Option<&str>) -> (u32, u32) {
    s.and_then(|s| {
        let (w, h) = s.split_once(['x', 'X'])?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    })
    .unwrap_or((1280, 800))
}

fn default_socket_path() -> PathBuf {
    let mut p = default_cache_root();
    p.push("vmette.sock");
    p
}

/// Provider cache root — shared with the CLI (`~/Library/Caches/vmette`) so
/// resolved OCI/tar rootfs trees are reused across both.
fn default_cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/vmette")
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

    let mut socket = default_socket_path();
    let mut vmette_bin = locate_vmette();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().context("--socket needs PATH")?.into(),
            "--vmette" => vmette_bin = args.next().context("--vmette needs PATH")?.into(),
            "-h" | "--help" => {
                eprintln!(
                    "vmetted — UNIX socket dispatcher for vmette\n\n\
                     usage: vmetted [--socket PATH] [--vmette PATH]\n\n\
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
    let registry = Registry::new(MAX_DESKTOP_SESSIONS, DESKTOP_IDLE_TTL, default_cache_root());

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

/// Per-connection entry point. Reads the single request line, peeks its
/// `kind`, and routes: desktop kinds to the stateful session registry,
/// everything else (no kind / `"run"`) to the stateless subprocess path.
async fn dispatch(stream: UnixStream, vmette_bin: PathBuf, registry: Arc<Registry>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let line = line.trim().to_string();

    // Peek the kind without committing to a concrete request shape.
    let kind = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned));

    match kind.as_deref() {
        Some("desktop_start")
        | Some("desktop_action")
        | Some("desktop_screenshot_settled")
        | Some("desktop_what_changed")
        | Some("desktop_stop") => {
            let reply = handle_desktop(kind.as_deref().unwrap(), &line, registry).await;
            let mut json = serde_json::to_vec(&reply)?;
            json.push(b'\n');
            let _ = write_half.write_all(&json).await;
            let _ = write_half.shutdown().await;
            Ok(())
        }
        // None (legacy untagged) or explicit "run": stateless subprocess path.
        _ => {
            let req: Request = serde_json::from_str(&line).context("parse run request")?;
            run_workload(req, write_half, vmette_bin).await
        }
    }
}

/// Route a parsed desktop request to the registry, mapping results/errors to a
/// single [`DesktopReply`]. Blocking registry calls hop off the async thread
/// via `spawn_blocking`.
async fn handle_desktop(kind: &str, line: &str, registry: Arc<Registry>) -> DesktopReply {
    match desktop_result(kind, line, registry).await {
        Ok(reply) => reply,
        Err(e) => DesktopReply::Error {
            message: format!("{e:#}"),
        },
    }
}

async fn desktop_result(kind: &str, line: &str, registry: Arc<Registry>) -> Result<DesktopReply> {
    match kind {
        "desktop_start" => {
            let req: DesktopStartReq = serde_json::from_str(line).context("parse desktop_start")?;
            let (width, height) = parse_size(req.size.as_deref());
            let params = StartParams {
                kernel: req.kernel,
                initramfs: req.initramfs,
                image: req.image,
                width,
                height,
                net: req.net,
                offline: req.offline,
                vcpus: req.vcpus,
                mem_mib: req.mem_mib,
            };
            let session_id = tokio::task::spawn_blocking(move || registry.start(params))
                .await
                .context("session start task")??;
            Ok(DesktopReply::Session { session_id })
        }
        "desktop_action" => {
            let req: DesktopActionReq =
                serde_json::from_str(line).context("parse desktop_action")?;
            let res =
                tokio::task::spawn_blocking(move || registry.action(&req.session_id, &req.action))
                    .await
                    .context("session action task")??;
            Ok(DesktopReply::ActionResult {
                ok: res.ok,
                error: res.error,
                x: res.x,
                y: res.y,
                png_base64: res
                    .png
                    .map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            })
        }
        "desktop_screenshot_settled" => {
            let req: DesktopSettleReq =
                serde_json::from_str(line).context("parse desktop_screenshot_settled")?;
            let timeout = Duration::from_millis(req.timeout_ms);
            let res = tokio::task::spawn_blocking(move || {
                registry.screenshot_when_settled(&req.session_id, timeout)
            })
            .await
            .context("settle poll task")??;
            Ok(DesktopReply::Settled {
                settled: res.settled,
                moving: res.moving.into_iter().map(RectJson::from).collect(),
                png_base64: base64::engine::general_purpose::STANDARD.encode(res.png),
            })
        }
        "desktop_what_changed" => {
            let req: DesktopWhatChangedReq =
                serde_json::from_str(line).context("parse desktop_what_changed")?;
            let res = tokio::task::spawn_blocking(move || registry.what_changed(&req.session_id))
                .await
                .context("what_changed task")??;
            Ok(DesktopReply::Changed {
                changed: res.changed.map(RectJson::from),
                png_base64: base64::engine::general_purpose::STANDARD.encode(res.png),
            })
        }
        "desktop_stop" => {
            let req: DesktopStopReq = serde_json::from_str(line).context("parse desktop_stop")?;
            tokio::task::spawn_blocking(move || registry.stop(&req.session_id))
                .await
                .context("session stop task")??;
            Ok(DesktopReply::Stopped)
        }
        other => Err(anyhow!("unknown desktop kind: {other}")),
    }
}

async fn run_workload(
    req: Request,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    vmette_bin: PathBuf,
) -> Result<()> {
    // Translate Request → vmette CLI flags
    let mut cmd = Command::new(&vmette_bin);
    cmd.arg("--kernel").arg(&req.kernel);
    cmd.arg("--initramfs").arg(&req.initramfs);
    cmd.arg("--rootfs").arg(&req.rootfs);
    if req.rootfs_ro {
        cmd.arg("--rootfs-ro");
    }
    if req.offline {
        cmd.arg("--offline");
    }
    for s in &req.shares {
        cmd.arg("--share")
            .arg(format!("{}={}", s.tag, s.path.display()));
    }
    for d in &req.disks {
        cmd.arg("--disk").arg(d);
    }
    cmd.arg("--exec").arg(&req.exec);
    if req.net {
        cmd.arg("--net");
    }
    if req.switch_root {
        cmd.arg("--switch-root");
    }
    cmd.arg("--vsock-port").arg(req.vsock_port.to_string());
    cmd.arg("--guest-vsock-port")
        .arg(req.guest_vsock_port.to_string());
    if let Some(t) = req.timeout_seconds {
        cmd.arg("--timeout").arg(t.to_string());
    }
    cmd.arg("--vcpus").arg(req.vcpus.to_string());
    cmd.arg("--mem-mib").arg(req.mem_mib.to_string());

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
