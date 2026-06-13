//! Top-level [`run`] orchestration. `run` is a thin, CLI-facing wrapper
//! over a one-shot [`Session`]: it owns the terminal (raw mode + signal
//! handlers) and the user-visible banner, starts a session, blocks until
//! it ends, restores the terminal, and **returns** a [`RunOutput`] with the
//! guest's exit code. It never exits the process — the caller (the `vmette`
//! CLI's `main`, an FFI embedder) chooses the process exit code. A library
//! that owns the process is hostile to embedders, so the exit decision lives
//! with the binary.
//!
//! The VM lifecycle itself — building the config, the delegate, the vsock
//! listener, the timeout, and pumping the run loop — lives in
//! [`crate::session`] so it is reusable in-process (the daemon hosts many
//! sessions).

use crate::error::Error;
use crate::session::{Session, SessionEnd};
use crate::terminal::{enter_raw_mode, install_signal_handlers, restore_terminal};
use crate::Config;

/// Result of a completed [`run`] or [`Session::wait_captured`](crate::Session::wait_captured).
#[derive(Debug, Clone)]
pub struct RunOutput {
    pub exit_code: i32,
    /// Captured guest output (combined stdout+stderr) when the session ran with
    /// [`Config::capture_output`](crate::Config::capture_output); empty otherwise
    /// (the interactive `run` path streams to the terminal). Bounded — truncated
    /// past 1 MiB with a marker.
    pub output: String,
}

/// Boot the configured guest, exec the command, block until poweroff, restore
/// the terminal, and return a [`RunOutput`] carrying the guest's exit code
/// (124 on timeout, 0 on a requested stop, 1 on a guest error). `Err` is for
/// setup failures (config invalid, VM failed to start, snapshot unsupported).
/// The process exit code is the caller's to choose.
pub fn run(config: &Config) -> Result<RunOutput, Error> {
    // Snapshot dispatch — both build and resume go through here.
    if let Some(p) = &config.build_snapshot {
        crate::vz::snapshot::build(config, p)?;
        return Ok(RunOutput {
            exit_code: 0,
            output: String::new(),
        });
    }
    if let Some(p) = &config.resume_snapshot {
        let code = crate::vz::snapshot::resume(config, p)?;
        return Ok(RunOutput {
            exit_code: code,
            output: String::new(),
        });
    }

    install_signal_handlers();
    enter_raw_mode();

    // Start the session (creates + starts the VM but does not yet pump the
    // run loop). Print the banner before wait(): nothing services the VM's
    // queue between start() and wait(), so no guest serial output can race
    // ahead of the banner.
    let session = match Session::start(config) {
        Ok(s) => s,
        Err(e) => {
            // enter_raw_mode() already ran; don't leave the user's terminal raw.
            restore_terminal();
            return Err(e);
        }
    };
    if !config.quiet {
        eprint_banner(config, session.cmdline(), session.vsock_port());
    }

    let end = session.wait();
    restore_terminal();
    // Drop the session (runs its teardown guards — the ephemeral `--scratch`
    // disk image and the block-rootfs `ctl` temp dir) before returning. `end`
    // is owned, so this is safe; the guest has already stopped.
    drop(session);
    let exit_code = match end {
        SessionEnd::Exited(code) => {
            if !config.quiet {
                eprintln!("\r\n[vmette] guest stopped (exit {})\r", code);
            }
            code
        }
        SessionEnd::TimedOut => {
            if !config.quiet {
                eprintln!(
                    "\r\n[vmette] timeout {}s reached; guest force-stopped (exit 124)\r",
                    config.timeout_seconds.unwrap_or(0)
                );
            }
            124
        }
        SessionEnd::Stopped => {
            if !config.quiet {
                eprintln!("\r\n[vmette] guest stopped (exit 0)\r");
            }
            0
        }
        SessionEnd::Error(msg) => {
            // An error is always worth surfacing, even under --quiet.
            eprintln!("\r\n[vmette] guest stopped with error: {}\r", msg);
            1
        }
    };
    Ok(RunOutput {
        exit_code,
        output: String::new(),
    })
}

fn eprint_banner(config: &Config, cmdline: &str, vsock_port: Option<u32>) {
    let rootfs = match &config.rootfs {
        Some(crate::Rootfs::Block(rb)) => {
            format!("{} ({} block, ro)", rb.path.display(), rb.fstype)
        }
        Some(crate::Rootfs::Share(r)) => format!(
            "{}{}",
            r.path.display(),
            if r.read_only { " (ro)" } else { "" }
        ),
        None => "(none)".into(),
    };
    let vsock = match vsock_port {
        None => "(disabled)".into(),
        Some(p) => p.to_string(),
    };
    let overlay = match config.scratch_mib {
        Some(mib) => format!("{} MiB ephemeral ext4 disk", mib),
        None => "tmpfs (RAM-backed)".into(),
    };
    eprintln!(
        "[vmette] kernel       {}\n\
         [vmette] initramfs    {}\n\
         [vmette] cmdline      {}\n\
         [vmette] rootfs       {}\n\
         [vmette] shares       {}\n\
         [vmette] disks        {}\n\
         [vmette] overlay      {}\n\
         [vmette] exec         {}\n\
         [vmette] vsock-port   {}\n\
         [vmette] switch-root  {}\n\
         [vmette] net          {}\n\
         [vmette] timeout      {}s\n\
         [vmette] vcpus        {}, memMiB {}\n",
        config.kernel.display(),
        config.initramfs.display(),
        cmdline,
        rootfs,
        config.shares.len(),
        config.disks.len(),
        overlay,
        config.exec_cmd.as_deref().unwrap_or("(none — interactive)"),
        vsock,
        if config.switch_root { "yes" } else { "no" },
        if config.net { "yes (NAT)" } else { "no" },
        config.timeout_seconds.unwrap_or(0),
        config.vcpus,
        config.mem_mib,
    );
}
