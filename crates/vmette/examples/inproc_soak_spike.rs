//! Phase-0 spike (ARCH-V2-PLAN §0, gate G2-stability): soak the **in-process**
//! `Session` boot/teardown cycle to gather evidence for whether the daemon can
//! safely host one-shot runs in-process (C2), giving up the subprocess fault
//! isolation it has today.
//!
//! It boots N one-shot `Session`s back-to-back IN ONE PROCESS (no fork),
//! exec-ing a trivial command, and reports: success/failure counts, per-boot
//! timing, and host fd-count drift (a proxy for VZ/objc2 fd leaks across many
//! create→start→teardown cycles). If the host process stays healthy across the
//! soak, that is the evidence base for C2's "no subprocess isolation" trade.
//!
//! Requires a SIGNED build + real assets (this cannot run unsigned; VZ needs the
//! `com.apple.security.virtualization` entitlement even to validate a config —
//! see serial_capture_spike). Provide assets via env:
//!
//!   VMETTE_SOAK_KERNEL=assets/x86_64/vmlinuz-virt \
//!   VMETTE_SOAK_INITRAMFS=assets/x86_64/initramfs-vmette \
//!   VMETTE_SOAK_ROOTFS=assets/x86_64/alpine-rootfs \
//!   VMETTE_SOAK_N=500 \
//!     cargo build -p vmette --example inproc_soak_spike && \
//!     codesign -s - --entitlements entitlements.plist --force \
//!       target/debug/examples/inproc_soak_spike && \
//!     ./target/debug/examples/inproc_soak_spike
//!
//! Abort criterion (PLAN §Risk): if failures > 0 that aren't cleanly reported,
//! or fd drift grows roughly linearly with N (a leak), C2 is reconsidered —
//! keep the subprocess path for one-shot, apply only C1/C3/C5.

use std::path::PathBuf;
use std::time::Instant;

use vmette::{Config, RootfsShare, Session, SessionEnd};

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

/// Count open fds for this process (macOS): entries under /dev/fd.
fn open_fd_count() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

fn main() {
    let kernel = env_path("VMETTE_SOAK_KERNEL");
    let initramfs = env_path("VMETTE_SOAK_INITRAMFS");
    let rootfs = env_path("VMETTE_SOAK_ROOTFS");
    let n: usize = std::env::var("VMETTE_SOAK_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let (kernel, initramfs, rootfs) = match (kernel, initramfs, rootfs) {
        (Some(k), Some(i), Some(r)) => (k, i, r),
        _ => {
            eprintln!(
                "skipped: set VMETTE_SOAK_KERNEL / _INITRAMFS / _ROOTFS to real assets.\n\
                 This spike needs a SIGNED build + booted VMs — see the module docs."
            );
            return;
        }
    };

    println!("== G2-stability in-process soak: {n} boots ==");
    println!(
        "kernel={} initramfs={} rootfs={}",
        kernel.display(),
        initramfs.display(),
        rootfs.display()
    );

    let fd_start = open_fd_count();
    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut total_ms = 0u128;
    let mut worst_ms = 0u128;

    for i in 0..n {
        let mut cfg = Config::new(&kernel, &initramfs);
        cfg.rootfs_share = Some(RootfsShare {
            path: rootfs.clone(),
            read_only: false,
        });
        cfg.exec_cmd = Some("true".into());
        cfg.timeout_seconds = Some(30);
        cfg.quiet = true;

        let t = Instant::now();
        match Session::start(&cfg) {
            Ok(session) => {
                let end = session.wait();
                drop(session);
                let ms = t.elapsed().as_millis();
                total_ms += ms;
                worst_ms = worst_ms.max(ms);
                match end {
                    SessionEnd::Exited(0) => ok += 1,
                    other => {
                        bad += 1;
                        eprintln!("[{i}] unexpected end: {other:?}");
                    }
                }
            }
            Err(e) => {
                bad += 1;
                eprintln!("[{i}] start failed: {e}");
            }
        }

        if i % 50 == 49 {
            println!(
                "  …{}/{n}  ok={ok} bad={bad}  fds now={} (start={fd_start})",
                i + 1,
                open_fd_count()
            );
        }
    }

    let fd_end = open_fd_count();
    println!("\n== soak result ==");
    println!("ok={ok}  bad={bad}  of {n}");
    println!(
        "avg={}ms  worst={worst_ms}ms",
        if ok > 0 { total_ms / ok as u128 } else { 0 }
    );
    println!(
        "fd drift: start={fd_start} end={fd_end} (delta={})",
        fd_end as i64 - fd_start as i64
    );
    if bad == 0 && (fd_end as i64 - fd_start as i64) <= 8 {
        println!("VERDICT: healthy → supports C2 (in-process one-shot).");
    } else {
        println!("VERDICT: investigate (failures or fd growth) → see PLAN abort criterion.");
    }
}
