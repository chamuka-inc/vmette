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

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Handle to the daemon's desktop subsystem. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
    kernel: PathBuf,
    initramfs: PathBuf,
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

impl DaemonClient {
    /// `socket` defaults to `~/Library/Caches/vmette/vmette.sock` when `None`.
    /// `kernel`/`initramfs` are the ordinary vmette assets (reuse the
    /// Sandbox's already-discovered paths).
    pub fn new(socket: Option<PathBuf>, kernel: PathBuf, initramfs: PathBuf) -> Self {
        Self {
            socket: socket.unwrap_or_else(default_socket),
            kernel,
            initramfs,
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

    /// Tear a session down.
    pub async fn stop(&self, session_id: &str) -> Result<()> {
        let req = json!({ "kind": "desktop_stop", "session_id": session_id });
        self.call(&req).await?;
        Ok(())
    }

    /// Send one request line, read one reply line, and map a `kind:"error"`
    /// reply to an `Err`.
    async fn call(&self, req: &Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "connect {} failed (is vmetted running?)",
                self.socket.display()
            )
        })?;
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
}

fn default_socket() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join("Library/Caches/vmette/vmette.sock")
}
