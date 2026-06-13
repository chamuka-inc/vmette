//! Phase-0 spike (ARCH-V2-PLAN §0, gate G2): prove that
//! Virtualization.framework *accepts* a configuration with
//!
//!   * two virtio console serial ports (stdout on hvc0, stderr on hvc1), and
//!   * a console attachment bound to an arbitrary host pipe fd (capture),
//!
//! WITHOUT booting a VM, by calling `validateWithError:` — pure configuration
//! checking that never instantiates a `VZVirtualMachine`.
//!
//! Phase-0 finding: `validateWithError:` STILL requires the
//! `com.apple.security.virtualization` entitlement (the unsigned baseline fails
//! identically to the two-console case), so this spike must be codesigned to run:
//!
//!   cargo build -p vmette --example serial_capture_spike
//!   codesign -s - --entitlements entitlements.plist --force \
//!     target/debug/examples/serial_capture_spike
//!   ./target/debug/examples/serial_capture_spike   # NOT `cargo run` — it re-links + unsigns
//!
//! What this spike does NOT prove (still gated on a full signed boot + assets):
//! that the guest kernel enumerates hvc1 and routes stderr to it, and that
//! captured bytes round-trip end-to-end through a booted guest. Those are the
//! boot-validation half of G2 — see inproc_soak_spike and the PLAN.

use std::os::fd::RawFd;

use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZFileHandleSerialPortAttachment, VZLinuxBootLoader, VZSerialPortConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtualMachineConfiguration,
};

fn nsstr(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// One virtio console port whose write side is an arbitrary fd (the write end
/// of a host pipe). Mirrors the proposed `SerialSink::Capture` path.
fn capture_port(write_fd: RawFd) -> Retained<VZSerialPortConfiguration> {
    unsafe {
        // closeOnDealloc:false — the spike owns the fd lifetime, not the handle.
        let write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            write_fd,
            false,
        );
        let attach =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,                // no guest stdin on the capture console
                Some(&write_handle), // guest writes here → host reads the pipe
            );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attach.into_super()));
        Retained::into_super(serial)
    }
}

/// The current production path: a console bound to host stdin/stdout (Inherit).
fn inherit_port() -> Retained<VZSerialPortConfiguration> {
    unsafe {
        let attach =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                Some(&NSFileHandle::fileHandleWithStandardInput()),
                Some(&NSFileHandle::fileHandleWithStandardOutput()),
            );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attach.into_super()));
        Retained::into_super(serial)
    }
}

/// Build a minimal-but-valid VM config carrying `ports` serial ports, then
/// validate it. Returns Ok(()) iff VZ accepts the configuration.
fn validate_with_ports(
    kernel: &std::path::Path,
    initramfs: &std::path::Path,
    ports: &[Retained<VZSerialPortConfiguration>],
) -> Result<(), String> {
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();

        let kurl = NSURL::fileURLWithPath(&nsstr(&kernel.to_string_lossy()));
        let iurl = NSURL::fileURLWithPath(&nsstr(&initramfs.to_string_lossy()));
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kurl);
        boot.setInitialRamdiskURL(Some(&iurl));
        boot.setCommandLine(&nsstr("console=hvc0 quiet"));
        cfg.setBootLoader(Some(&boot.into_super()));

        cfg.setCPUCount(1);
        cfg.setMemorySize(512 * 1024 * 1024);

        let arr: Retained<NSArray<VZSerialPortConfiguration>> = NSArray::from_retained_slice(ports);
        cfg.setSerialPorts(&arr);

        cfg.validateWithError()
            .map_err(|e| e.localizedDescription().to_string())
    }
}

fn main() {
    // validateWithError does not stat the kernel/initramfs, but provide real
    // temp files so a future stricter validator still passes.
    let dir = std::env::temp_dir().join(format!("vmette-spike-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let kernel = dir.join("vmlinuz");
    let initramfs = dir.join("initramfs");
    std::fs::write(&kernel, b"\x7fELF-not-real").unwrap();
    std::fs::write(&initramfs, b"\x1f\x8b-not-real").unwrap();

    // A host pipe; the write end feeds a capture console.
    let mut fds: [libc::c_int; 2] = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe() failed");
    let (read_fd, write_fd) = (fds[0], fds[1]);

    println!("== G2 serial-capture spike (config validation only; no boot) ==\n");

    // Case 1: baseline — single inherit console (today's production shape).
    match validate_with_ports(&kernel, &initramfs, &[inherit_port()]) {
        Ok(()) => println!("[1] single inherit console      : VALID   (baseline OK)"),
        Err(e) => println!("[1] single inherit console      : INVALID ({e})"),
    }

    // Case 2: single CAPTURE console (pipe-fd attachment) — proves a non-std
    // filehandle attachment is accepted.
    match validate_with_ports(&kernel, &initramfs, &[capture_port(write_fd)]) {
        Ok(()) => println!("[2] single capture (pipe) console: VALID   (fd attachment OK)"),
        Err(e) => println!("[2] single capture (pipe) console: INVALID ({e})"),
    }

    // Case 3: TWO consoles — hvc0 inherit (stdout), hvc1 capture (stderr).
    // This is the stream-separation shape the SPEC proposes. If VZ's validator
    // rejects >1 serial port, it fails here and we fall back to single-stream.
    match validate_with_ports(
        &kernel,
        &initramfs,
        &[inherit_port(), capture_port(write_fd)],
    ) {
        Ok(()) => {
            println!("[3] two consoles (hvc0 + hvc1)   : VALID   → stream separation feasible")
        }
        Err(e) => println!(
            "[3] two consoles (hvc0 + hvc1)   : INVALID ({e}) → use single-stream fallback"
        ),
    }

    // Case 4: TWO capture consoles — both stdout and stderr to separate pipes,
    // the fully-headless capture shape the daemon/MCP path would use.
    let mut fds2: [libc::c_int; 2] = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds2.as_mut_ptr()) }, 0);
    match validate_with_ports(
        &kernel,
        &initramfs,
        &[capture_port(write_fd), capture_port(fds2[1])],
    ) {
        Ok(()) => {
            println!("[4] two capture consoles         : VALID   → headless dual-capture feasible")
        }
        Err(e) => println!("[4] two capture consoles         : INVALID ({e})"),
    }

    // Case 5: THREE consoles — hvc0 inherit (kernel console + /init `[init]`
    // chatter, which logs to fd2), hvc1 capture (exec stdout), hvc2 capture
    // (exec stderr). This is the CORRECTED C2 topology: the captured consoles
    // are exec-dedicated, so neither kernel boot/shutdown lines (hvc0) nor init
    // logs pollute them. Two consoles alone do NOT achieve this.
    let mut fds3: [libc::c_int; 2] = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds3.as_mut_ptr()) }, 0);
    match validate_with_ports(
        &kernel,
        &initramfs,
        &[inherit_port(), capture_port(fds2[1]), capture_port(fds3[1])],
    ) {
        Ok(()) => println!(
            "[5] three (kernel + exec out/err): VALID   → exec-dedicated clean capture feasible"
        ),
        Err(e) => println!("[5] three (kernel + exec out/err): INVALID ({e})"),
    }

    unsafe {
        libc::close(read_fd);
        libc::close(write_fd);
        libc::close(fds2[0]);
        libc::close(fds2[1]);
        libc::close(fds3[0]);
        libc::close(fds3[1]);
    }
    let _ = std::fs::remove_dir_all(&dir);

    println!("\nNote: VALID here means VZ's *config validator* accepts the shape.");
    println!(
        "Guest-side hvc1 enumeration + stderr routing is still boot-gated (needs a signed build)."
    );
}
