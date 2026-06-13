//! C2 stage-2a validation: boot a one-shot `Session` with `capture_output` and
//! confirm `wait_captured()` returns the guest's combined stdout+stderr as a
//! CLEAN stream (no `[init]`/kernel/overlay noise) plus the right exit code.
//!
//! This is the in-process capture the daemon/MCP path (stages 2b–2e) will use to
//! replace forking the `vmette` CLI + marker-scraping. Needs a SIGNED build +
//! assets:
//!   cargo build -p vmette --example capture_spike
//!   codesign -s - --entitlements entitlements.plist --force \
//!     target/debug/examples/capture_spike
//!   VMETTE_KERNEL=assets/x86_64/vmlinuz-virt \
//!   VMETTE_INITRAMFS=assets/x86_64/initramfs-vmette \
//!   VMETTE_ROOTFS=assets/x86_64/alpine-rootfs \
//!     ./target/debug/examples/capture_spike

use std::path::PathBuf;

use vmette::{Config, Rootfs, RootfsShare, Session};

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).map(PathBuf::from)
}

fn main() {
    let (kernel, initramfs, rootfs) = match (
        env_path("VMETTE_KERNEL"),
        env_path("VMETTE_INITRAMFS"),
        env_path("VMETTE_ROOTFS"),
    ) {
        (Some(k), Some(i), Some(r)) => (k, i, r),
        _ => {
            eprintln!("skipped: set VMETTE_KERNEL / _INITRAMFS / _ROOTFS (needs a signed build).");
            return;
        }
    };

    let mut cfg = Config::new(&kernel, &initramfs);
    cfg.rootfs = Some(Rootfs::Share(RootfsShare {
        path: rootfs,
        read_only: false,
    }));
    cfg.exec_cmd =
        Some("echo OUT-LINE-aa; echo ERR-LINE-bb >&2; printf 'no-newline-tail'; exit 7".into());
    cfg.capture_output = true;
    cfg.timeout_seconds = Some(25);
    cfg.quiet = true;

    let session = Session::start(&cfg).expect("session start");
    let out = session.wait_captured();

    println!("== C2 capture spike ==");
    println!("exit_code = {}", out.exit_code);
    println!("---- captured output ----");
    print!("{}", out.output);
    println!("\n---- end ----");

    let has_out = out.output.contains("OUT-LINE-aa");
    let has_err = out.output.contains("ERR-LINE-bb");
    let has_tail = out.output.contains("no-newline-tail");
    let clean = !out.output.contains("[init]")
        && !out.output.contains("reboot:")
        && !out.output.contains("overlay");

    println!("\n== verdict ==");
    println!("stdout captured (OUT-LINE-aa): {has_out}");
    println!("stderr captured (ERR-LINE-bb): {has_err}");
    println!("trailing no-newline captured : {has_tail}");
    println!("clean (no init/kernel noise) : {clean}");
    println!("exit code == 7               : {}", out.exit_code == 7);
    if has_out && has_err && has_tail && clean && out.exit_code == 7 {
        println!("RESULT: PASS → in-process clean capture works; ready for daemon/MCP wiring.");
    } else {
        println!("RESULT: FAIL");
    }
}
