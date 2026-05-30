//! Async client for the desktop-session subsystem of `vmetted`.
//!
//! The `execute` / `workspace_*` tools boot their own one-shot microVM via the
//! `vmette` CLI subprocess (see `sandbox.rs`). Desktop computer-use is
//! different: a desktop session is a *persistent* VM that must outlive a single
//! tool call, so it has to be owned by the long-lived daemon, not by a
//! per-call subprocess. These tools therefore route through `vmetted`'s UNIX
//! socket, where the session registry holds the live `vmette::Session`.
//!
//! Protocol: one request line of JSON in, one reply line of JSON out (the
//! daemon's stateful `desktop_*` path). We connect fresh per call — the hop
//! cost is trivial next to a GUI round-trip.
//!
//! Zero-config: if nothing is listening on the socket (first desktop use, or
//! the daemon was never started), [`DaemonClient`] launches a detached
//! `vmetted` on demand and waits for it to come up, so `desktop_*` tools work
//! without the user starting the daemon by hand.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// Handle to the daemon's desktop subsystem. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
    kernel: PathBuf,
    initramfs: PathBuf,
    /// Serializes auto-spawn so concurrent desktop calls don't each fork a
    /// `vmetted`; losers block here, then reuse the winner's socket.
    spawn_lock: Arc<Mutex<()>>,
}

/// The error/position fields plus optional PNG from a `desktop_action` reply.
#[derive(Debug)]
pub struct ActionReply {
    pub ok: bool,
    pub error: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Base64-encoded PNG (present only for `screenshot`).
    pub png_base64: Option<String>,
}

/// A rectangle in pixel coordinates (a moving region or a damage box).
#[derive(Debug, Clone, Copy)]
pub struct RectReply {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Reply to `desktop_screenshot_settled`: the settled (or timed-out) frame plus
/// the regions still moving.
#[derive(Debug)]
pub struct SettleReply {
    pub settled: bool,
    pub moving: Vec<RectReply>,
    pub png_base64: String,
}

/// Reply to `desktop_what_changed`: a fresh frame and the damage box (absent
/// when nothing changed since the previous capture).
#[derive(Debug)]
pub struct ChangedReply {
    pub changed: Option<RectReply>,
    pub png_base64: String,
}

impl DaemonClient {
    /// `socket` defaults to `~/Library/Caches/vmette/vmette.sock` when `None`.
    /// `kernel`/`initramfs` are the ordinary vmette assets (reuse the
    /// Sandbox's already-discovered paths).
    pub fn new(socket: Option<PathBuf>, kernel: PathBuf, initramfs: PathBuf) -> Self {
        Self {
            socket: socket.unwrap_or_else(default_socket),
            kernel,
            initramfs,
            spawn_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Boot a desktop session; returns its id.
    pub async fn start(
        &self,
        image: Option<String>,
        size: Option<String>,
        net: bool,
        offline: bool,
    ) -> Result<String> {
        let mut req = json!({
            "kind": "desktop_start",
            "kernel": self.kernel,
            "initramfs": self.initramfs,
            "net": net,
            "offline": offline,
        });
        if let Some(img) = image {
            req["image"] = json!(img);
        }
        if let Some(sz) = size {
            req["size"] = json!(sz);
        }
        let reply = self.call(&req).await?;
        reply
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("daemon did not return a session_id"))
    }

    /// Run one computer-use action against a live session. `action` is the
    /// `vmette::Action` JSON body (e.g. `{"action":"left_click"}`).
    pub async fn action(&self, session_id: &str, action: Value) -> Result<ActionReply> {
        let req = json!({
            "kind": "desktop_action",
            "session_id": session_id,
            "action": action,
        });
        let reply = self.call(&req).await?;
        Ok(ActionReply {
            ok: reply.get("ok").and_then(Value::as_bool).unwrap_or(false),
            error: reply
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned),
            x: reply.get("x").and_then(Value::as_i64).map(|v| v as i32),
            y: reply.get("y").and_then(Value::as_i64).map(|v| v as i32),
            png_base64: reply
                .get("png_base64")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Poll the desktop until it settles (or `timeout_ms` elapses) and return
    /// that frame plus the regions still moving.
    pub async fn screenshot_when_settled(
        &self,
        session_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<SettleReply> {
        let mut req = json!({
            "kind": "desktop_screenshot_settled",
            "session_id": session_id,
        });
        if let Some(ms) = timeout_ms {
            req["timeout_ms"] = json!(ms);
        }
        let reply = self.call(&req).await?;
        let moving = reply
            .get("moving")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_rect).collect())
            .unwrap_or_default();
        Ok(SettleReply {
            settled: reply
                .get("settled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            moving,
            png_base64: reply
                .get("png_base64")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("settle reply missing png_base64"))?,
        })
    }

    /// Capture one frame and report what changed since this session's previous
    /// capture.
    pub async fn what_changed(&self, session_id: &str) -> Result<ChangedReply> {
        let req = json!({ "kind": "desktop_what_changed", "session_id": session_id });
        let reply = self.call(&req).await?;
        Ok(ChangedReply {
            changed: reply.get("changed").and_then(parse_rect),
            png_base64: reply
                .get("png_base64")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("what_changed reply missing png_base64"))?,
        })
    }

    /// Tear a session down.
    pub async fn stop(&self, session_id: &str) -> Result<()> {
        let req = json!({ "kind": "desktop_stop", "session_id": session_id });
        self.call(&req).await?;
        Ok(())
    }

    /// Send one request line, read one reply line, and map a `kind:"error"`
    /// reply to an `Err`.
    async fn call(&self, req: &Value) -> Result<Value> {
        let stream = self.connect().await?;
        let (read_half, mut write_half) = stream.into_split();

        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        write_half.write_all(&line).await?;
        let _ = write_half.shutdown().await;

        let mut reply = String::new();
        BufReader::new(read_half)
            .read_line(&mut reply)
            .await
            .context("reading daemon reply")?;
        let reply = reply.trim();
        if reply.is_empty() {
            bail!("daemon closed the connection without replying");
        }
        let value: Value =
            serde_json::from_str(reply).with_context(|| format!("bad reply: {reply}"))?;

        if value.get("kind").and_then(Value::as_str) == Some("error") {
            let msg = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("{msg}");
        }
        Ok(value)
    }

    /// Connect to the daemon socket, lazily starting `vmetted` if nothing is
    /// listening yet. A connect error of `NotFound` (socket absent — never
    /// started) or `ConnectionRefused` (present but dead — crashed without
    /// cleanup) both mean "no daemon up", and (re)starting it is the fix.
    async fn connect(&self) -> Result<UnixStream> {
        use std::io::ErrorKind::{ConnectionRefused, NotFound};
        match UnixStream::connect(&self.socket).await {
            Ok(s) => Ok(s),
            Err(e) if matches!(e.kind(), NotFound | ConnectionRefused) => {
                self.start_and_connect().await
            }
            Err(e) => Err(e).with_context(|| format!("connect {} failed", self.socket.display())),
        }
    }

    /// Spawn a detached `vmetted`, wait for it to start accepting, and return
    /// the live connection. The spawn lock means only one task forks the
    /// daemon; concurrent desktop calls block, then find it already up.
    async fn start_and_connect(&self) -> Result<UnixStream> {
        let _guard = self.spawn_lock.lock().await;
        // Another task may have started it while we waited for the lock.
        if let Ok(s) = UnixStream::connect(&self.socket).await {
            return Ok(s);
        }
        let bin = locate_vmetted().ok_or_else(|| {
            anyhow!(
                "vmetted binary not found (needed for desktop_* tools); \
                 install it alongside vmette-mcp or start it manually"
            )
        })?;
        let mut cmd = Command::new(&bin);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid() is async-signal-safe and is the only call made in
        // the forked child before exec. Detaching into a new session lets the
        // daemon outlive this MCP server and survives signals sent to the
        // server's process group, matching vmetted's shared-daemon model.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
            .with_context(|| format!("spawning {}", bin.display()))?;

        // vmetted clears any stale socket and binds during startup; poll until
        // it accepts a connection, or give up after ~5s.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(s) = UnixStream::connect(&self.socket).await {
                return Ok(s);
            }
        }
        bail!(
            "vmetted did not start listening on {} within 5s",
            self.socket.display()
        );
    }
}

/// Locate `vmetted`: next to this binary (install + repo layouts put
/// vmette-mcp and vmetted side by side), else on `$PATH`. Canonicalize so a
/// symlinked `vmette-mcp` resolves to the real bin dir that holds vmetted.
fn locate_vmetted() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let real = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = real.parent() {
            let candidate = dir.join("vmetted");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join("vmetted");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Parse a `{x,y,w,h}` rect object from a reply (all four fields required).
fn parse_rect(v: &Value) -> Option<RectReply> {
    let f = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| n as u32);
    Some(RectReply {
        x: f("x")?,
        y: f("y")?,
        w: f("w")?,
        h: f("h")?,
    })
}

fn default_socket() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join("Library/Caches/vmette/vmette.sock")
}
