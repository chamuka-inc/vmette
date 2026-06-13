//! Phase-0 spike (ARCH-V2-PLAN §0, gate G2 boot-validation): determine how many
//! virtio console serial ports Virtualization.framework actually DELIVERS to the
//! host, by booting real VMs with N capture consoles and checking which host
//! pipes receive the guest's per-console marker.
//!
//! This decides C2's capture topology. Earlier single-config runs showed: with 3
//! consoles the guest enumerates /dev/hvc0,1,2 and writes to all three succeed
//! (rc=0), but the host received hvc0 + hvc1 only — hvc2 delivered nothing even
//! with `sync; sleep` (so it is NOT a poweroff flush race). This spike confirms
//! the limit parametrically.
//!
//! Needs a SIGNED build + assets (booting requires the virtualization
//! entitlement). Run:
//!   cargo build -p vmette --example three_console_boot_spike
//!   codesign -s - --entitlements entitlements.plist --force \
//!     target/debug/examples/three_console_boot_spike
//!   VMETTE_KERNEL=assets/x86_64/vmlinuz-virt \
//!   VMETTE_INITRAMFS=assets/x86_64/initramfs-vmette \
//!   VMETTE_ROOTFS=assets/x86_64/alpine-rootfs \
//!     ./target/debug/examples/three_console_boot_spike

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZFileHandleSerialPortAttachment, VZLinuxBootLoader,
    VZSerialPortConfiguration, VZSharedDirectory, VZSingleDirectoryShare,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtualMachine, VZVirtualMachineConfiguration,
};

struct QueueBound<T>(Retained<T>);
unsafe impl<T> Send for QueueBound<T> {}
impl<T> std::ops::Deref for QueueBound<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

fn nsstr(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}
fn file_url(p: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&nsstr(&p.to_string_lossy()))
}

fn capture_port(write_fd: RawFd) -> Retained<VZSerialPortConfiguration> {
    unsafe {
        let wh = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            write_fd,
            false,
        );
        let attach =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&wh),
            );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attach.into_super()));
        Retained::into_super(serial)
    }
}

fn drain_until(fd: RawFd, deadline: Instant) -> Vec<u8> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    while Instant::now() < deadline {
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n > 0 {
            out.extend_from_slice(&tmp[..n as usize]);
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    out
}

fn env_path(k: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(k).map(Into::into)
}

/// Boot a VM with `n` capture consoles (hvc0..hvc{n-1}); guest writes
/// `MARK-k` to /dev/hvc{k}. Returns, per port, whether the host pipe received
/// its marker.
fn probe_n_consoles(
    kernel: &std::path::Path,
    initramfs: &std::path::Path,
    rootfs: &std::path::Path,
    n: usize,
) -> Vec<bool> {
    // n host pipes.
    let mut rds = Vec::with_capacity(n);
    let mut wrs = Vec::with_capacity(n);
    for _ in 0..n {
        let mut p: [libc::c_int; 2] = [0; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        rds.push(p[0]);
        wrs.push(p[1]);
    }

    // Guest writes a distinct marker to each console, then settles.
    let mut writes = String::new();
    for k in 0..n {
        writes.push_str(&format!("echo MARK-{k}-ok > /dev/hvc{k}; "));
    }
    let exec = format!("{writes}sync; sleep 1");
    let cmdline = format!(
        "console=hvc0 quiet vmette.exec={} vmette.rootfs=1",
        B64.encode(exec.as_bytes())
    );

    let cfg = unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        let boot =
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(kernel));
        boot.setInitialRamdiskURL(Some(&file_url(initramfs)));
        boot.setCommandLine(&nsstr(&cmdline));
        cfg.setBootLoader(Some(&boot.into_super()));
        cfg.setCPUCount(1);
        cfg.setMemorySize(512 * 1024 * 1024);

        let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &nsstr("rootfs"),
        );
        let dir = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &file_url(rootfs),
            true,
        );
        let share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
        fs.setShare(Some(&share.into_super()));
        let fs_arr: Retained<NSArray<VZDirectorySharingDeviceConfiguration>> =
            NSArray::from_retained_slice(&[fs.into_super()]);
        cfg.setDirectorySharingDevices(&fs_arr);

        let ports_vec: Vec<Retained<VZSerialPortConfiguration>> =
            wrs.iter().map(|&fd| capture_port(fd)).collect();
        let ports: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&ports_vec);
        cfg.setSerialPorts(&ports);

        cfg.validateWithError()
            .unwrap_or_else(|e| panic!("config invalid (n={n}): {}", e.localizedDescription()));
        cfg
    };

    let queue = DispatchQueue::new("com.chamuka.vmette.spike", None);
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &cfg, &queue)
    };
    let vm_start = QueueBound(vm.clone());
    queue.exec_async(move || {
        let cb = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                eprintln!(
                    "start failed (n): {}",
                    unsafe { &*err }.localizedDescription()
                );
            }
        });
        unsafe { vm_start.startWithCompletionHandler(&cb) };
    });

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut delivered = Vec::with_capacity(n);
    for (k, &rd) in rds.iter().enumerate() {
        let bytes = drain_until(rd, deadline);
        let got = String::from_utf8_lossy(&bytes).contains(&format!("MARK-{k}-ok"));
        delivered.push(got);
    }
    for &fd in rds.iter().chain(wrs.iter()) {
        unsafe { libc::close(fd) };
    }
    // Give VZ a moment to fully tear down before the next boot reuses the runtime.
    std::thread::sleep(Duration::from_millis(300));
    delivered
}

/// Decisive C2 test: can hvc0 be captured CLEAN and STREAMING by pushing the
/// kernel console + /init chatter to a *second, discarded* console? Two capture
/// ports; `console=hvc1` routes kernel printk to port 1, and /init inherits
/// /dev/console = hvc1, so its `[init]` logs land on port 1 too. The exec writes
/// its markers to /dev/hvc0. We read port 0 and check it carries ONLY the exec
/// output — no `[init]`/kernel noise. Returns (hvc0_got_marker, hvc0_clean).
fn probe_clean_primary(
    kernel: &std::path::Path,
    initramfs: &std::path::Path,
    rootfs: &std::path::Path,
) -> (bool, bool) {
    let mut p0: [libc::c_int; 2] = [0; 2];
    let mut p1: [libc::c_int; 2] = [0; 2];
    assert_eq!(unsafe { libc::pipe(p0.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(p1.as_mut_ptr()) }, 0);

    const MARK: &str = "EXEC-ONLY-CLEAN-5c8e";
    // Exec writes stdout+stderr markers to hvc0 (the captured, delivering port).
    let exec = format!("echo {MARK} > /dev/hvc0; echo {MARK}-err >> /dev/hvc0; sync; sleep 1");
    // console=hvc1 → kernel + /init noise go to the DISCARDED port 1.
    let cmdline = format!(
        "console=hvc1 quiet vmette.exec={} vmette.rootfs=1",
        B64.encode(exec.as_bytes())
    );

    let cfg = unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        let boot =
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(kernel));
        boot.setInitialRamdiskURL(Some(&file_url(initramfs)));
        boot.setCommandLine(&nsstr(&cmdline));
        cfg.setBootLoader(Some(&boot.into_super()));
        cfg.setCPUCount(1);
        cfg.setMemorySize(512 * 1024 * 1024);
        let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &nsstr("rootfs"),
        );
        let dir = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &file_url(rootfs),
            true,
        );
        let share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
        fs.setShare(Some(&share.into_super()));
        let fs_arr: Retained<NSArray<VZDirectorySharingDeviceConfiguration>> =
            NSArray::from_retained_slice(&[fs.into_super()]);
        cfg.setDirectorySharingDevices(&fs_arr);
        let ports: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[capture_port(p0[1]), capture_port(p1[1])]);
        cfg.setSerialPorts(&ports);
        cfg.validateWithError().unwrap_or_else(|e| {
            panic!("clean-primary config invalid: {}", e.localizedDescription())
        });
        cfg
    };

    let queue = DispatchQueue::new("com.chamuka.vmette.spike2", None);
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &cfg, &queue)
    };
    let vm_start = QueueBound(vm.clone());
    queue.exec_async(move || {
        let cb = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                eprintln!(
                    "clean-primary start failed: {}",
                    unsafe { &*err }.localizedDescription()
                );
            }
        });
        unsafe { vm_start.startWithCompletionHandler(&cb) };
    });

    let deadline = Instant::now() + Duration::from_secs(6);
    let bytes = drain_until(p0[0], deadline);
    let s = String::from_utf8_lossy(&bytes);
    println!(
        "\n-- clean-primary hvc0 capture (console=hvc1) --\n{}",
        s.trim_end()
    );
    let got = s.contains(MARK);
    let clean = got && !s.contains("[init]") && !s.contains("reboot:") && !s.contains("overlay");
    for &fd in &[p0[0], p0[1], p1[0], p1[1]] {
        unsafe { libc::close(fd) };
    }
    std::thread::sleep(Duration::from_millis(300));
    (got, clean)
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

    println!("== G2: how many virtio console ports does VZ deliver to the host? ==\n");
    let mut max_delivered = 0usize;
    for n in [1usize, 2, 3, 4] {
        let d = probe_n_consoles(&kernel, &initramfs, &rootfs, n);
        let count = d.iter().filter(|&&x| x).count();
        max_delivered = max_delivered.max(count);
        let marks: Vec<String> = d
            .iter()
            .enumerate()
            .map(|(k, &ok)| format!("hvc{k}={}", if ok { "✓" } else { "·" }))
            .collect();
        println!("n={n}: delivered {count}/{n}   [{}]", marks.join(" "));
    }

    println!(
        "\nVZ delivers host data for at most {max_delivered} console serial port(s) (all-capture)."
    );

    // Decisive C2 test: clean streaming hvc0 with noise pushed to a discarded port.
    let (got, clean) = probe_clean_primary(&kernel, &initramfs, &rootfs);

    println!("\n== conclusion ==");
    println!(
        "multi-console capture (≥2 ports deliver): {}",
        max_delivered >= 2
    );
    println!("clean hvc0 streaming (console=hvc1 sink): got={got} clean={clean}");
    if clean {
        println!(
            "→ C2 capture design: ONE captured streaming console (hvc0) for exec output, with\n\
             \x20 kernel+init pushed to a discarded 2nd console (console=hvc1). stdout/stderr\n\
             \x20 SEPARATION + exit code go via the ctl virtio-fs share (proven reliable in the\n\
             \x20 soak). This deletes marker-scraping AND preserves streaming. SPEC §4.2 revise."
        );
    } else if max_delivered >= 2 {
        println!("→ multi-console works but clean-primary failed; revisit.");
    } else {
        println!(
            "→ multi-console capture unreliable under VZ AND clean-primary failed.\n\
             \x20 C2 fallback: exec stdout/stderr → ctl-share files (clean, non-streaming) +\n\
             \x20 structured exit via ctl. SPEC §4.2 fallback path."
        );
    }
}
