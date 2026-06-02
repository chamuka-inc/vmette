//! Thin async wrapper around the `vmette` CLI subprocess.
//!
//! Each [`Sandbox::run`] spawns one `vmette` process which boots one
//! microVM, runs the exec, captures stdout/stderr, and exits. We don't
//! go through `vmetted` — the MCP server is itself the long-lived
//! dispatcher, so adding a second daemon hop would just add latency.
//!
//! Asset discovery: the kernel + initramfs paths are picked up from
//! either an explicit override or the shared [`vmette_assets`] search
//! path (`$VMETTE_ASSETS_DIR`, `./assets`, `<install prefix>/assets`),
//! so the MCP server probes exactly where the `vmette` CLI does.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use vmette_proto::daemon::Request;
use vmette_proto::ShareMount;

/// Hard cap on captured stdout + stderr per call (each stream). A
/// runaway agent (e.g. `yes | head -c 10G`) can otherwise OOM the
/// long-lived MCP server because tokio's Command::output() buffers
/// everything in memory. Past the cap we truncate and append a
/// human-readable marker so the agent knows output was clipped.
const OUTPUT_CAP_BYTES: usize = 1024 * 1024; // 1 MiB

/// Sentinels printed around the user command's output so we can slice the
/// guest *exec* output out of the full guest console, which also carries
/// PID-1 init chatter (`[init] mounted …`, `[init] exit=N`) and the kernel's
/// `reboot: Power down`. We wrap the command (see [`wrap_exec`]) to print
/// these on their own lines, then [`slice_exec_output`] keeps only what lies
/// between them. The tokens are matched bounded by real newlines, so the
/// copy that init echoes in its `[init] exec: …` line — where the surrounding
/// `\n` are literal backslash-n, not newlines — never matches.
const MARKER_BEGIN: &str = "__VMETTE_EXEC_BEGIN__";
const MARKER_END: &str = "__VMETTE_EXEC_END__";

/// Wrap a user command so its stdout/stderr (which share the guest console)
/// are bracketed by [`MARKER_BEGIN`]/[`MARKER_END`]. The command runs in a
/// subshell `( … )` so an inner `exit N` only leaves the subshell — the
/// markers and the real exit code still propagate. Newlines (not quoting)
/// separate the pieces, so no escaping of the user command is needed and
/// multi-line commands pass through verbatim.
fn wrap_exec(cmd: &str) -> String {
    format!(
        "printf '\\n{MARKER_BEGIN}\\n'\n(\n{cmd}\n)\n__vmette_rc=$?\nprintf '\\n{MARKER_END}\\n'\nexit $__vmette_rc\n"
    )
}

/// Keep only the bytes the user command produced, dropping the init/kernel
/// framing outside the markers. Falls back to the raw console when the
/// markers are absent (e.g. the guest died before the exec, or output was
/// truncated past the end marker) so failures are never silently blanked.
///
/// The guest console is CRLF (the hvc0 tty applies ONLCR, so every guest `\n`
/// arrives as `\r\n`). We normalise to LF first — both so the LF-bounded
/// marker match is reliable and so a Linux guest's output doesn't reach the
/// agent with stray carriage returns. Lone `\r` (e.g. a progress bar redraw)
/// is left intact.
fn slice_exec_output(raw: &str) -> String {
    let raw = raw.replace("\r\n", "\n");
    let begin = format!("\n{MARKER_BEGIN}\n");
    let end = format!("\n{MARKER_END}\n");
    match raw.find(&begin) {
        Some(b) => {
            let after = &raw[b + begin.len()..];
            match after.find(&end) {
                Some(e) => after[..e].to_string(),
                None => after.to_string(), // end marker lost (truncated / crash)
            }
        }
        None => raw, // no markers — surface the raw console
    }
}

/// Grace period added on top of the guest's `--timeout` for the
/// host-side wall-clock guard. If vmette itself wedges (segfaults,
/// fails to honour its own --timeout) the host timeout fires after
/// `guest_timeout + GRACE`, kills the child via `kill_on_drop`, and
/// returns a clear error rather than blocking forever.
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
        let kernel =
            vmette_assets::require_asset(kernel, "vmlinuz-virt").map_err(|e| anyhow!(e))?;
        let initramfs =
            vmette_assets::require_asset(initramfs, "initramfs-vmette").map_err(|e| anyhow!(e))?;
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
        // Render the run flags through the single `Request → argv` owner in
        // `vmette-proto` (shared with the daemon). The fields the MCP sandbox
        // never sets (vsock ports, cpu/mem, switch-root, disks) stay `None`, so
        // the CLI applies its own defaults — exactly as before. `--exec` is the
        // marker-wrapped command; `--quiet` is appended below.
        let run = Request {
            kernel: self.kernel.clone(),
            initramfs: self.initramfs.clone(),
            rootfs: req.rootfs.clone(),
            rootfs_ro: false,
            offline: req.offline,
            shares: req.shares.clone(),
            disks: Vec::new(),
            exec: wrap_exec(&req.exec),
            net: req.net,
            switch_root: false,
            vsock_port: None,
            guest_vsock_port: None,
            timeout_seconds: req.timeout_seconds,
            vcpus: None,
            mem_mib: None,
        };
        let mut cmd = Command::new(&self.vmette_bin);
        cmd.args(run.to_cli_args());
        // `--quiet` drops vmette's config banner + "guest stopped" lines from
        // stderr; the agent only wants the guest's output, not the launcher's.
        cmd.arg("--quiet");
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
            stdout: slice_exec_output(&stdout_buf),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruct the console the guest would emit for a given user-command
    /// output: the init `[init] exec:` echo of the wrapped script (with the
    /// markers as *literal* `\n`), the real printf'd markers around the real
    /// output, then trailing init/kernel framing.
    fn fake_console(user_output: &str) -> String {
        let wrapped = wrap_exec("USERCMD");
        format!(
            "[init] mounted virtio-fs 'rootfs' at /newroot\n\
             [init] exec: {echo}\n\
             \n{MARKER_BEGIN}\n{out}\n{MARKER_END}\n\
             [init] exit=0\n[    0.5] reboot: Power down\n",
            // init echoes the script on one logical line; its embedded \n are
            // literal backslash-n, so collapse the real newlines to show that.
            echo = wrapped.replace('\n', "\\n"),
            out = user_output,
        )
    }

    #[test]
    fn slices_between_markers_only() {
        let console = fake_console("hello\nworld");
        assert_eq!(slice_exec_output(&console), "hello\nworld");
    }

    #[test]
    fn ignores_the_init_echo_of_the_markers() {
        // The echoed wrapped script contains the marker tokens; the slice must
        // not latch onto them (they are not bounded by real newlines).
        let console = fake_console("payload");
        assert_eq!(slice_exec_output(&console), "payload");
        assert!(console.contains(MARKER_BEGIN)); // present in the echo too
    }

    #[test]
    fn preserves_trailing_newline() {
        // printf injects a leading \n before END, so a user trailing newline
        // is kept and the injected one dropped.
        let console = fake_console("ends-with-nl\n");
        assert_eq!(slice_exec_output(&console), "ends-with-nl\n");
    }

    #[test]
    fn empty_output_between_markers() {
        let console = fake_console("");
        assert_eq!(slice_exec_output(&console), "");
    }

    #[test]
    fn handles_crlf_console() {
        // The real console is CRLF; the marker match must survive it and the
        // returned output must be LF.
        let console = "[init] x\r\n\r\n__VMETTE_EXEC_BEGIN__\r\nline1\r\nline2\r\n__VMETTE_EXEC_END__\r\n[init] exit=0\r\n";
        assert_eq!(slice_exec_output(console), "line1\nline2");
    }

    #[test]
    fn falls_back_to_raw_without_markers() {
        let raw = "boot failed before exec\nno markers here\n";
        assert_eq!(slice_exec_output(raw), raw);
    }

    #[test]
    fn missing_end_marker_returns_tail() {
        // Truncation can drop the end marker; keep everything after begin.
        let raw = format!("[init] exec: …\n\n{MARKER_BEGIN}\npartial output, then cut");
        assert_eq!(slice_exec_output(&raw), "partial output, then cut");
    }

    #[test]
    fn wrap_runs_user_command_in_isolating_subshell() {
        let w = wrap_exec("exit 9");
        assert!(w.contains("(\nexit 9\n)"));
        assert!(w.contains("exit $__vmette_rc"));
    }
}
