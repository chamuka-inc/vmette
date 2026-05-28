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
//!         "rootfs_share": {"path": "/p", "read_only": false},
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

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct Request {
    kernel: PathBuf,
    initramfs: PathBuf,
    #[serde(default)]
    rootfs_share: Option<RootfsShare>,
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

fn default_guest_vsock_port() -> u32 { 1025 }
fn default_vcpus() -> u8 { 1 }
fn default_mem_mib() -> u64 { 512 }

#[derive(Debug, Deserialize)]
struct RootfsShare {
    path: PathBuf,
    #[serde(default)]
    read_only: bool,
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
    Exit   { code: i32 },
    Error  { message: String },
}

fn default_socket_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Caches");
    p.push("vmette");
    p.push("vmette.sock");
    p
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
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

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind {}", socket.display()))?;
    info!(socket = %socket.display(), vmette = %vmette_bin.display(), "vmetted listening");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let bin = vmette_bin.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, bin).await {
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

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle(stream: UnixStream, vmette_bin: PathBuf) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let req: Request = serde_json::from_str(line.trim()).context("parse request")?;

    // Translate Request → vmette CLI flags
    let mut cmd = Command::new(&vmette_bin);
    cmd.arg("--kernel").arg(&req.kernel);
    cmd.arg("--initramfs").arg(&req.initramfs);
    if let Some(rs) = &req.rootfs_share {
        cmd.arg("--rootfs-share").arg(&rs.path);
        if rs.read_only {
            cmd.arg("--ro-rootfs-share");
        }
    }
    for s in &req.shares {
        cmd.arg("--share").arg(format!("{}={}", s.tag, s.path.display()));
    }
    for d in &req.disks {
        cmd.arg("--disk").arg(d);
    }
    cmd.arg("--exec").arg(&req.exec);
    if req.net { cmd.arg("--net"); }
    if req.switch_root { cmd.arg("--switch-root"); }
    cmd.arg("--vsock-port").arg(req.vsock_port.to_string());
    cmd.arg("--guest-vsock-port").arg(req.guest_vsock_port.to_string());
    if let Some(t) = req.timeout_seconds { cmd.arg("--timeout").arg(t.to_string()); }
    cmd.arg("--vcpus").arg(req.vcpus.to_string());
    cmd.arg("--mem-mib").arg(req.mem_mib.to_string());

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn vmette")?;
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    // Stream stdout + stderr concurrently as JSON frames.
    let mut out_reader = BufReader::new(child_stdout);
    let mut err_reader = BufReader::new(child_stderr);
    let mut out_buf = String::new();
    let mut err_buf = String::new();
    let mut out_done = false;
    let mut err_done = false;

    while !out_done || !err_done {
        tokio::select! {
            n = out_reader.read_line(&mut out_buf), if !out_done => {
                match n {
                    Ok(0) => { out_done = true; }
                    Ok(_) => {
                        let frame = Frame::Stdout { data: std::mem::take(&mut out_buf) };
                        write_frame(&mut write_half, &frame).await?;
                    }
                    Err(e) => {
                        // Real I/O error — surface it instead of pretending EOF.
                        let frame = Frame::Error { message: format!("stdout: {e}") };
                        let _ = write_frame(&mut write_half, &frame).await;
                        out_done = true;
                    }
                }
            }
            n = err_reader.read_line(&mut err_buf), if !err_done => {
                match n {
                    Ok(0) => { err_done = true; }
                    Ok(_) => {
                        let frame = Frame::Stderr { data: std::mem::take(&mut err_buf) };
                        write_frame(&mut write_half, &frame).await?;
                    }
                    Err(e) => {
                        let frame = Frame::Error { message: format!("stderr: {e}") };
                        let _ = write_frame(&mut write_half, &frame).await;
                        err_done = true;
                    }
                }
            }
        }
    }

    let status = child.wait().await?;
    let code = status.code().unwrap_or(-1);
    write_frame(&mut write_half, &Frame::Exit { code }).await?;
    write_half.shutdown().await?;
    Ok(())
}

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let mut json = serde_json::to_vec(frame)?;
    json.push(b'\n');
    w.write_all(&json).await?;
    Ok(())
}
