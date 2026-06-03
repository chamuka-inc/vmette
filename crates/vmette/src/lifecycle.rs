//! Top-level [`run`] orchestration. `run` is a thin, CLI-facing wrapper
//! over a one-shot [`Session`]: it owns the terminal (raw mode + signal
//! handlers) and the user-visible banner, starts a session, blocks until
//! it ends, and exits the process with the guest's code.
//!
//! The VM lifecycle itself — building the config, the delegate, the vsock
//! listener, the timeout, and pumping the run loop — lives in
//! [`crate::session`] so it is reusable in-process (the daemon hosts many
//! sessions). `run` is the only place that calls `std::process::exit`, so
//! its happy path still never returns; the `Result<RunOutput, Error>`
//! shape is for setup errors (invalid config, snapshot unsupported, …).

use crate::error::Error;
use crate::session::{Session, SessionEnd};
use crate::terminal::{enter_raw_mode, install_signal_handlers, restore_terminal};
use crate::Config;

/// Result of a completed [`run`]. Currently only carries the exit code,
/// but kept as a struct so we can grow it without breaking callers.
#[derive(Debug, Clone, Copy)]
pub struct RunOutput {
    pub exit_code: i32,
}

/// Boot the configured guest, exec the command, block until poweroff,
/// then exit the process with the guest's exit code.
///
/// Note: on success the function does not return — it calls
/// `std::process::exit` once the session ends. The `Result<RunOutput, Error>`
/// shape is for error paths (config invalid, VM failed to start, snapshot
/// unsupported, etc).
pub fn run(config: &Config) -> Result<RunOutput, Error> {
    // Snapshot dispatch — both build and resume go through here.
    if let Some(p) = &config.build_snapshot {
        crate::vz::snapshot::build(config, p)?;
        return Ok(RunOutput { exit_code: 0 });
    }
    if let Some(p) = &config.resume_snapshot {
        let code = crate::vz::snapshot::resume(config, p)?;
        return Ok(RunOutput { exit_code: code });
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
    // Drop the session before std::process::exit, which skips destructors:
    // this runs the Session's teardown guards (the ephemeral `--scratch` disk
    // image and the block-rootfs `ctl` temp dir) so they don't leak on the
    // CLI path. `end` is owned, so dropping the session here is safe; the
    // guest has already stopped. (The daemon path drops Session normally.)
    drop(session);
    match end {
        SessionEnd::Exited(code) => {
            if !config.quiet {
                eprintln!("\r\n[vmette] guest stopped (exit {})\r", code);
            }
            std::process::exit(code);
        }
        SessionEnd::TimedOut => {
            if !config.quiet {
                eprintln!(
                    "\r\n[vmette] timeout {}s reached; guest force-stopped (exit 124)\r",
                    config.timeout_seconds.unwrap_or(0)
                );
            }
            std::process::exit(124);
        }
        SessionEnd::Stopped => {
            if !config.quiet {
                eprintln!("\r\n[vmette] guest stopped (exit 0)\r");
            }
            std::process::exit(0);
        }
        SessionEnd::Error(msg) => {
            // An error is always worth surfacing, even under --quiet.
            eprintln!("\r\n[vmette] guest stopped with error: {}\r", msg);
            std::process::exit(1);
        }
    }
}

fn eprint_banner(config: &Config, cmdline: &str, vsock_port: Option<u32>) {
    let rootfs = if let Some(rb) = &config.rootfs_block {
        format!("{} ({} block, ro)", rb.path.display(), rb.fstype)
    } else if let Some(r) = &config.rootfs_share {
        format!(
            "{}{}",
            r.path.display(),
            if r.read_only { " (ro)" } else { "" }
        )
    } else {
        "(none)".into()
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
