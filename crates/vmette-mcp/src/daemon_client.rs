//! Async client for the desktop-session subsystem of `vmetted`.
//!
//! The `execute` / `workspace_*` tools boot their own one-shot microVM via the
//! `vmette` CLI subprocess (see `sandbox.rs`). Desktop computer-use is
//! different: a desktop session is a *persistent* VM that must outlive a single
//! tool call, so it has to be owned by the long-lived daemon, not by a
//! per-call subprocess. These tools therefore route through `vmetted`'s UNIX
//! socket, where the session registry holds the live `vmette::Session`.
//!
//! Protocol: one [`DesktopRequest`] line of JSON in, one [`DesktopReply`] line
//! of JSON out (the daemon's stateful `desktop_*` path). Both are the shared
//! [`vmette_proto`] wire types, so this client and the daemon cannot drift. We
//! connect fresh per call — the hop cost is trivial next to a GUI round-trip.
//!
//! Zero-config: if nothing is listening on the socket (first desktop use, or
//! the daemon was never started), [`DaemonClient`] launches a detached
//! `vmetted` on demand and waits for it to come up, so `desktop_*` tools work
//! without the user starting the daemon by hand.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use vmette_proto::agent::Action;
use vmette_proto::daemon::{
    ActionReply, ChangedReply, DesktopAction, DesktopReply, DesktopRequest,
    DesktopScreenshotSettled, DesktopStart, DesktopStop, DesktopView, DesktopWhatChanged,
    SettleReply,
};
use vmette_proto::ShareMount;

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

impl DaemonClient {
    /// `socket` defaults to `~/Library/Caches/vmette/vmette.sock` when `None`.
    /// `kernel`/`initramfs` are the ordinary vmette assets (reuse the
    /// Sandbox's already-discovered paths).
    pub fn new(socket: Option<PathBuf>, kernel: PathBuf, initramfs: PathBuf) -> Self {
        Self {
            socket: socket.unwrap_or_else(vmette_assets::default_socket),
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
        shares: Vec<ShareMount>,
    ) -> Result<String> {
        // Resolve the desktop rootfs spec client-side, the same way the
        // kernel/initramfs assets are resolved: explicit per-call `image` →
        // `$VMETTE_DESKTOP_IMAGE` → locally built `vmette-desktop-rootfs.tar` →
        // registry fallback. The daemon receives a concrete spec.
        let image = vmette_assets::default_desktop_image(image);
        // `vcpus`/`mem_mib` unset → the daemon applies its desktop defaults.
        let reply = self
            .call(&DesktopRequest::DesktopStart(DesktopStart {
                kernel: self.kernel.clone(),
                initramfs: self.initramfs.clone(),
                image,
                size,
                net,
                offline,
                shares,
                vcpus: None,
                mem_mib: None,
            }))
            .await?;
        match reply {
            DesktopReply::Session(s) => Ok(s.session_id),
            other => bail!("daemon did not return a session_id: {other:?}"),
        }
    }

    /// Run one computer-use action against a live session.
    pub async fn action(&self, session_id: &str, action: Action) -> Result<ActionReply> {
        let reply = self
            .call(&DesktopRequest::DesktopAction(DesktopAction {
                session_id: session_id.to_string(),
                action,
            }))
            .await?;
        match reply {
            DesktopReply::ActionResult(r) => Ok(r),
            other => bail!("unexpected reply to desktop_action: {other:?}"),
        }
    }

    /// Poll the desktop until it has been continuously settled for
    /// `stable_hold_ms` (or `timeout_ms` elapses) and return that frame plus the
    /// regions still moving. `None` for either lets the daemon apply its
    /// default.
    pub async fn screenshot_when_settled(
        &self,
        session_id: &str,
        timeout_ms: Option<u64>,
        stable_hold_ms: Option<u64>,
    ) -> Result<SettleReply> {
        let reply = self
            .call(&DesktopRequest::DesktopScreenshotSettled(
                DesktopScreenshotSettled {
                    session_id: session_id.to_string(),
                    timeout_ms,
                    stable_hold_ms,
                },
            ))
            .await?;
        match reply {
            DesktopReply::Settled(s) => Ok(s),
            other => bail!("unexpected reply to desktop_screenshot_settled: {other:?}"),
        }
    }

    /// Capture one frame and report what changed since this session's previous
    /// capture.
    pub async fn what_changed(&self, session_id: &str) -> Result<ChangedReply> {
        let reply = self
            .call(&DesktopRequest::DesktopWhatChanged(DesktopWhatChanged {
                session_id: session_id.to_string(),
            }))
            .await?;
        match reply {
            DesktopReply::Changed(c) => Ok(c),
            other => bail!("unexpected reply to desktop_what_changed: {other:?}"),
        }
    }

    /// Start (or look up) a live VNC view of the session, returning the
    /// loopback `host:port` a VNC client connects to.
    pub async fn view(&self, session_id: &str) -> Result<String> {
        let reply = self
            .call(&DesktopRequest::DesktopView(DesktopView {
                session_id: session_id.to_string(),
            }))
            .await?;
        match reply {
            DesktopReply::View(v) => Ok(v.addr),
            other => bail!("unexpected reply to desktop_view: {other:?}"),
        }
    }

    /// Tear a session down.
    pub async fn stop(&self, session_id: &str) -> Result<()> {
        self.call(&DesktopRequest::DesktopStop(DesktopStop {
            session_id: session_id.to_string(),
        }))
        .await?;
        Ok(())
    }

    /// Send one request line, read one reply line, and map a
    /// [`DesktopReply::Error`] reply to an `Err`.
    async fn call(&self, req: &DesktopRequest) -> Result<DesktopReply> {
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
            bail!(
                "daemon closed the connection without replying — vmetted likely crashed or is \
                 running a stale build. Check it's alive (`pgrep vmetted`) and restart it; if you \
                 just reinstalled, kill the old PID first. See docs/DAEMON.md."
            );
        }
        let value: DesktopReply =
            serde_json::from_str(reply).with_context(|| format!("bad reply: {reply}"))?;
        match value {
            DesktopReply::Error(e) => bail!("{}", e.message),
            other => Ok(other),
        }
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
        let bin = vmette_assets::locate_vmetted().ok_or_else(|| {
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
