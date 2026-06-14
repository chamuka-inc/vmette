//! Async client for the desktop-session subsystem of `vmetted`.
//!
//! The `execute` / `workspace_*` tools boot their own one-shot microVM
//! in-process (see `sandbox.rs`). Desktop computer-use is different: a desktop
//! session is a *persistent* VM that must outlive a single tool call, so it has
//! to be owned by the long-lived daemon. These tools therefore route through
//! `vmetted`'s UNIX socket, where the session registry holds the live
//! `vmette::Session`.
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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
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
    /// The shared synchronous transport (connect / auto-spawn / framing). Async
    /// methods drive it via `spawn_blocking`. `autostart` is on, so the daemon
    /// is launched on demand on first desktop use.
    inner: Arc<vmette_daemon_client::DaemonClient>,
    kernel: PathBuf,
    initramfs: PathBuf,
}

impl DaemonClient {
    /// `socket` defaults to `~/Library/Caches/vmette/vmette.sock` when `None`.
    /// `kernel`/`initramfs` are the ordinary vmette assets (reuse the
    /// Sandbox's already-discovered paths).
    pub fn new(socket: Option<PathBuf>, kernel: PathBuf, initramfs: PathBuf) -> Self {
        let socket = socket.unwrap_or_else(vmette_assets::default_socket);
        Self {
            inner: Arc::new(vmette_daemon_client::DaemonClient::new(socket, true)),
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
        shares: Vec<ShareMount>,
    ) -> Result<String> {
        // Resolve the desktop rootfs spec client-side, the same way the
        // kernel/initramfs assets are resolved: explicit per-call `image` →
        // `$VMETTE_DESKTOP_IMAGE` → locally built `vmette-desktop-rootfs.tar` →
        // registry fallback. The daemon receives a concrete spec.
        let image = vmette_assets::resolve_desktop_image(image);
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

    /// Send one request and read the single reply, mapping a daemon
    /// [`DesktopReply::Error`] reply to an `Err`. The connect / auto-spawn /
    /// framing live in the shared synchronous transport; this just hops it onto
    /// a blocking thread (a desktop round-trip already blocks on GUI work).
    async fn call(&self, req: &DesktopRequest) -> Result<DesktopReply> {
        let inner = self.inner.clone();
        let req = req.clone();
        tokio::task::spawn_blocking(move || inner.request(&req))
            .await
            .context("daemon request task")?
            .map_err(|e| anyhow!("{e}"))
    }
}
