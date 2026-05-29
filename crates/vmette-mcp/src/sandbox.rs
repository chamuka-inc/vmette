//! Thin async wrapper around the `vmette` CLI subprocess.
//!
//! Each [`Sandbox::run`] spawns one `vmette` process which boots one
//! microVM, runs the exec, captures stdout/stderr, and exits. We don't
//! go through `vmetted` — the MCP server is itself the long-lived
//! dispatcher, so adding a second daemon hop would just add latency.
//!
//! Asset discovery: the kernel + initramfs paths are picked up from
//! either an explicit override or the standard install layout
//! (`~/.local/share/vmette/assets/`) or the repo layout
//! (`./assets/`) — whichever exists first.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Hard cap on captured stdout + stderr per call (each stream). A
/// runaway agent (e.g. `yes | head -c 10G`) can otherwise OOM the
/// long-lived MCP server because tokio's Command::output() buffers
/// everything in memory. Past the cap we truncate and append a
/// human-readable marker so the agent knows output was clipped.
const OUTPUT_CAP_BYTES: usize = 1024 * 1024; // 1 MiB

/// Grace period added on top of the guest's `--timeout` for the
/// host-side wall-clock guard. If vmette itself wedges (segfaults,
/// fails to honour its own --timeout) the host timeout fires after
/// `guest_timeout + GRACE`, kills the child via `kill_on_drop`, and
/// returns a clear error rather than blocking forever.
const HOST_TIMEOUT_GRACE_SECS: u64 = 5;
/// Used when the caller passed no per-request timeout.
const HOST_TIMEOUT_DEFAULT_SECS: u64 = 60;

/// A virtio-fs share to mount in the guest: `<tag>` → `<path>` (rw).
#[derive(Debug, Clone)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
}

/// Per-call request describing how to boot the microVM.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub rootfs: String,
    pub exec: String,
    pub shares: Vec<Share>,
    pub net: bool,
    pub timeout_seconds: Option<u32>,
    /// Force `--offline` even when network would otherwise be allowed.
    /// Used by tools that should never hit the registry.
    pub offline: bool,
}

/// What the guest produced.
#[derive(Debug, Clone)]
pub struct RunReply {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// Configured handle to the `vmette` CLI. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Sandbox {
    vmette_bin: PathBuf,
    kernel: PathBuf,
    initramfs: PathBuf,
}

impl Sandbox {
    /// Construct from explicit paths (CLI overrides) or auto-discover.
    pub fn new(
        vmette_bin: Option<PathBuf>,
        kernel: Option<PathBuf>,
        initramfs: Option<PathBuf>,
    ) -> Result<Self> {
        let vmette_bin = vmette_bin
            .or_else(locate_vmette_bin)
            .ok_or_else(|| anyhow!("`vmette` binary not found on PATH or next to vmette-mcp"))?;
        let assets = locate_assets_dir()
            .ok_or_else(|| anyhow!("could not locate vmette assets dir (looked under ~/.local/share/vmette/assets and ./assets)"))?;
        let kernel = kernel.unwrap_or_else(|| assets.join("vmlinuz-virt"));
        let initramfs = initramfs.unwrap_or_else(|| assets.join("initramfs-vmette"));
        if !kernel.exists() {
            return Err(anyhow!("kernel not found at {}", kernel.display()));
        }
        if !initramfs.exists() {
            return Err(anyhow!("initramfs not found at {}", initramfs.display()));
        }
        Ok(Self {
            vmette_bin,
            kernel,
            initramfs,
        })
    }

    /// Kernel image path (shared with the daemon client for desktop sessions).
    pub fn kernel(&self) -> &Path {
        &self.kernel
    }

    /// Initramfs path (shared with the daemon client for desktop sessions).
    pub fn initramfs(&self) -> &Path {
        &self.initramfs
    }

    /// Run one microVM and return the captured output.
    ///
    /// The vmette CLI's banner + delegate messages go to stderr; the
    /// guest's exec stdout/stderr go to stdout. We expose both so the
    /// model can see both — `RunReply.stdout` is the guest's output,
    /// `RunReply.stderr` is the vmette banner plus any guest stderr.
    ///
    /// Both streams are read concurrently into bounded buffers so a
    /// pathological guest (e.g. `yes | head -c 10G`) can't OOM the
    /// long-lived MCP server. The whole run is wrapped in a host-side
    /// wall-clock guard (`guest_timeout + 5s`) so a wedged vmette can't
    /// hang the agent indefinitely.
    pub async fn run(&self, req: &RunRequest) -> Result<RunReply> {
        let host_timeout = Duration::from_secs(
            req.timeout_seconds
                .map(|s| (s as u64).saturating_add(HOST_TIMEOUT_GRACE_SECS))
                .unwrap_or(HOST_TIMEOUT_DEFAULT_SECS),
        );
        match tokio::time::timeout(host_timeout, self.run_inner(req)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "vmette wedged: no result within {}s (guest_timeout + {}s host grace). \
                 child killed via kill_on_drop.",
                host_timeout.as_secs(),
                HOST_TIMEOUT_GRACE_SECS
            )),
        }
    }

    async fn run_inner(&self, req: &RunRequest) -> Result<RunReply> {
        let mut cmd = Command::new(&self.vmette_bin);
        cmd.arg("--kernel").arg(&self.kernel);
        cmd.arg("--initramfs").arg(&self.initramfs);
        cmd.arg("--rootfs").arg(&req.rootfs);
        if req.offline {
            cmd.arg("--offline");
        }
        if req.net {
            cmd.arg("--net");
        }
        if let Some(t) = req.timeout_seconds {
            cmd.arg("--timeout").arg(t.to_string());
        }
        for s in &req.shares {
            cmd.arg("--share")
                .arg(format!("{}={}", s.tag, s.path.display()));
        }
        cmd.arg("--exec").arg(&req.exec);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // If the MCP client disconnects mid-call we want the microVM to
        // die with us, not keep burning a vCPU until --timeout.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", self.vmette_bin.display()))?;
        // .stdout/.stderr are Some by construction (Stdio::piped above).
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Read both streams concurrently — bounded — so the child
        // never blocks on a full pipe and we never buffer unbounded
        // memory. We must read concurrently with wait(); otherwise a
        // verbose child fills its pipe buffer and deadlocks.
        let stdout_task = tokio::spawn(read_capped(stdout, OUTPUT_CAP_BYTES));
        let stderr_task = tokio::spawn(read_capped(stderr, OUTPUT_CAP_BYTES));

        let status = child.wait().await.context("waiting on vmette")?;
        let stdout_buf = stdout_task.await.context("stdout reader panicked")??;
        let stderr_buf = stderr_task.await.context("stderr reader panicked")??;

        Ok(RunReply {
            stdout: stdout_buf,
            stderr: stderr_buf,
            exit: status.code().unwrap_or(-1),
        })
    }
}

/// Read from `reader` to EOF, keeping at most `cap` bytes and
/// discarding the rest. CRITICAL: we must read to EOF rather than
/// dropping the pipe at cap+1 — if we drop early, the child's next
/// write hits EPIPE/SIGPIPE and vmette dies before reporting its
/// exit code, so the agent sees a confusing `exit: -1` instead of
/// the guest's real status. Draining keeps the pipe writable until
/// the child finishes naturally.
///
/// After truncation we pop bytes until the buffer is valid UTF-8 so
/// the appended `[output truncated…]` marker isn't preceded by a
/// U+FFFD from a split codepoint. Worst case: 3 trailing bytes lost
/// from a 1 MiB buffer; functionally negligible.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> std::io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(16 * 1024));
    let mut tmp = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        if !truncated {
            let room = cap.saturating_sub(buf.len());
            if n <= room {
                buf.extend_from_slice(&tmp[..n]);
            } else {
                buf.extend_from_slice(&tmp[..room]);
                truncated = true;
            }
        }
        // After the cap is reached, fall through and keep reading
        // (without buffering) so the child's pipe stays drained and
        // it doesn't SIGPIPE.
    }
    if truncated {
        // Trim to the last valid UTF-8 prefix so from_utf8_lossy doesn't
        // emit a trailing U+FFFD from a split codepoint. Bounded by the
        // longest UTF-8 sequence (4 bytes), so at most 3 pops.
        while !buf.is_empty() && std::str::from_utf8(&buf).is_err() {
            buf.pop();
        }
    }
    let mut out = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        out.push_str(&format!(
            "\n[output truncated at {} bytes — guest produced more, the rest was discarded]\n",
            cap
        ));
    }
    Ok(out)
}

// --- discovery helpers ---------------------------------------------------

fn locate_vmette_bin() -> Option<PathBuf> {
    // Honour an env override first (matches vmetted's locate_vmette).
    if let Ok(p) = std::env::var("VMETTE_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    // Sibling-of-current-exe (the install layout puts both binaries in
    // ~/.local/bin via symlinks pointing into share/vmette/).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("vmette");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // $PATH lookup. We resolve via `which` semantics manually to avoid
    // pulling in another dep — split PATH and stat each entry.
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join("vmette");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn locate_assets_dir() -> Option<PathBuf> {
    // Install layout.
    if let Ok(home) = std::env::var("HOME") {
        let install = PathBuf::from(home).join(".local/share/vmette/assets");
        if install.join("vmlinuz-virt").exists() {
            return Some(install);
        }
    }
    // Repo layout (running from the source checkout).
    let repo = std::env::current_dir().ok()?.join("assets");
    if repo.join("vmlinuz-virt").exists() {
        return Some(repo);
    }
    // Next-to-binary (dist tarball layout: bin/ + share/vmette/assets/).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            let candidate = prefix.join("share/vmette/assets");
            if candidate.join("vmlinuz-virt").exists() {
                return Some(candidate);
            }
        }
    }
    None
}
