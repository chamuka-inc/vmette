//! In-process microVM runner for the MCP server's one-shot tools.
//!
//! Each [`Sandbox::run`] boots one microVM via a capture-aware
//! [`vmette::Session`] — the same in-process substrate the daemon and the
//! desktop registry use — runs the exec, and returns its captured output. The
//! guest's combined stdout+stderr arrives on a dedicated clean console (no
//! kernel/init noise), so there is no console marker-scraping and no forked
//! `vmette` subprocess.
//!
//! Asset discovery: the kernel + initramfs paths are picked up from either an
//! explicit override or the shared [`vmette_assets`] search path
//! (`$VMETTE_ASSETS_DIR`, `./assets`, `<install prefix>/assets`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use vmette_proto::ShareMount;

/// Grace period added on top of the guest's `--timeout` for the host-side
/// wall-clock guard. If VZ itself wedges (fails to honour the guest timeout) the
/// host guard fires after `guest_timeout + GRACE`, force-stops the VM, and
/// returns a clear error rather than blocking the long-lived MCP server forever.
const HOST_TIMEOUT_GRACE_SECS: u64 = 5;
/// Used when the caller passed no per-request timeout.
const HOST_TIMEOUT_DEFAULT_SECS: u64 = 60;

/// Per-call request describing how to boot the microVM. A virtio-fs share is
/// the workspace-wide [`ShareMount`] (`<tag>` → `<path>`, mounted rw).
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub rootfs: String,
    pub exec: String,
    pub shares: Vec<ShareMount>,
    pub net: bool,
    pub timeout_seconds: Option<u32>,
    /// Force `--offline` even when network would otherwise be allowed.
    /// Used by tools that should never hit the registry.
    pub offline: bool,
    /// Ephemeral ext4 scratch disk size in MiB for the writable overlay
    /// (`--scratch`). `None` → RAM-backed tmpfs overlay (the default).
    pub scratch_mib: Option<u64>,
}

/// What the guest produced. `stdout` is the exec's combined stdout+stderr
/// (captured clean); `stderr` is empty for the in-process path (there is no
/// launcher process whose stderr could carry a banner).
#[derive(Debug, Clone)]
pub struct RunReply {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// Configured handle for booting one-shot microVMs in-process. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Sandbox {
    kernel: PathBuf,
    initramfs: PathBuf,
}

impl Sandbox {
    /// Construct from explicit asset paths (overrides) or auto-discover.
    pub fn new(kernel: Option<PathBuf>, initramfs: Option<PathBuf>) -> Result<Self> {
        let kernel =
            vmette_assets::require_asset(kernel, "vmlinuz-virt").map_err(|e| anyhow!(e))?;
        let initramfs =
            vmette_assets::require_asset(initramfs, "initramfs-vmette").map_err(|e| anyhow!(e))?;
        Ok(Self { kernel, initramfs })
    }

    /// Kernel image path (shared with the daemon client for desktop sessions).
    pub fn kernel(&self) -> &Path {
        &self.kernel
    }

    /// Initramfs path (shared with the daemon client for desktop sessions).
    pub fn initramfs(&self) -> &Path {
        &self.initramfs
    }

    /// Boot one microVM in-process, run the exec, and return its captured
    /// output. The guest's output is bounded inside `vmette::Session` (1 MiB),
    /// so a pathological guest (e.g. `yes | head -c 10G`) can't OOM the
    /// long-lived MCP server. The whole run is wrapped in a host-side wall-clock
    /// guard (`guest_timeout + 5s`); if it fires, the VM is force-stopped and an
    /// error returned so a wedged VZ can't hang the agent.
    pub async fn run(&self, req: &RunRequest) -> Result<RunReply> {
        let host_timeout = Duration::from_secs(
            req.timeout_seconds
                .map(|s| (s as u64).saturating_add(HOST_TIMEOUT_GRACE_SECS))
                .unwrap_or(HOST_TIMEOUT_DEFAULT_SECS),
        );
        let kernel = self.kernel.clone();
        let initramfs = self.initramfs.clone();
        let req = req.clone();
        // Lets the host guard force-stop the VM if it wedges past its own
        // --timeout, instead of leaving it running.
        let stop_slot: Arc<std::sync::Mutex<Option<vmette::StopHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let stop_for_worker = stop_slot.clone();

        // The VM runs on a blocking thread (`Session` is `!Send` and does
        // blocking VZ work). Rootfs resolution (registry / network) happens here
        // too. Output is buffered via `wait_captured` and returned whole.
        let worker = tokio::task::spawn_blocking(move || -> Result<RunReply> {
            let provider = vmette_providers::default_registry();
            let ctx = vmette::provider::Context::new(vmette_assets::default_cache_root())
                .offline(req.offline);
            let artifact = provider
                .resolve(&req.rootfs, &ctx)
                .map_err(|e| anyhow!("resolving rootfs {}: {e}", req.rootfs))?;

            let mut cfg = vmette::Config::new(&kernel, &initramfs);
            cfg.exec_cmd = Some(req.exec.clone());
            cfg.shares = req.shares.clone();
            cfg.net = req.net;
            cfg.timeout_seconds = req.timeout_seconds;
            cfg.scratch_mib = req.scratch_mib;
            cfg.capture_output = true;
            cfg.set_rootfs_artifact(artifact, false);

            let session =
                vmette::Session::start(&cfg).map_err(|e| anyhow!("vmette session start: {e}"))?;
            *stop_for_worker.lock().unwrap() = Some(session.stop_handle());
            let out = session.wait_captured();
            Ok(RunReply {
                stdout: out.output,
                stderr: String::new(),
                exit: out.exit_code,
            })
        });

        match tokio::time::timeout(host_timeout, worker).await {
            Ok(joined) => joined.context("sandbox run task")?,
            Err(_) => {
                // Host guard fired. Force-stop the VM (its own --timeout should
                // have already; this covers a wedged VZ) and surface an error.
                if let Some(h) = stop_slot.lock().unwrap().take() {
                    h.stop();
                }
                Err(anyhow!(
                    "vmette wedged: no result within {}s (guest_timeout + {}s host grace)",
                    host_timeout.as_secs(),
                    HOST_TIMEOUT_GRACE_SECS
                ))
            }
        }
    }
}
