//! Translate a high-level [`crate::Config`] into a
//! `VZVirtualMachineConfiguration` ready to validate and start.

use std::os::fd::RawFd;

use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZDiskImageStorageDeviceAttachment,
    VZEntropyDeviceConfiguration, VZFileHandleSerialPortAttachment, VZLinuxBootLoader,
    VZMemoryBalloonDeviceConfiguration, VZNATNetworkDeviceAttachment, VZNetworkDeviceAttachment,
    VZNetworkDeviceConfiguration, VZSerialPortConfiguration, VZSharedDirectory,
    VZSingleDirectoryShare, VZSocketDeviceConfiguration, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketDeviceConfiguration,
    VZVirtioTraditionalMemoryBalloonDeviceConfiguration, VZVirtualMachineConfiguration,
};

use crate::error::Error;
use crate::{Config, VsockPort};

fn nsstr(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&nsstr(&path.to_string_lossy()))
}

/// Resolves the effective vsock port (auto-allocates if requested).
pub(crate) fn resolve_vsock_port(policy: VsockPort) -> Option<u32> {
    match policy {
        VsockPort::Disabled => None,
        VsockPort::Auto => Some(50000 + (rand::random::<u32>() % 10000)),
        VsockPort::Fixed(n) => Some(n),
    }
}

/// Where the guest's serial console(s) connect on the host side.
pub(crate) enum SerialSink {
    /// One virtio console (`hvc0`) bound to the host's stdin/stdout — the
    /// interactive CLI path, where guest output streams straight to the terminal.
    Inherit,
    /// Two consoles for clean capture (validated against VZ's one-deliverable-
    /// console limit): `hvc0` is bound to `write_fd` (the write end of a host
    /// pipe) and carries *only* the exec's redirected output; `hvc1` is a null
    /// sink that absorbs the kernel console + `/init` chatter (the cmdline sets
    /// `console=hvc1`). The host reads `write_fd` as a clean stream.
    Capture { write_fd: RawFd },
}

/// Build a fully-configured `VZVirtualMachineConfiguration` from `config`.
/// `vsock_port` is the already-resolved port (None to skip vsock device).
/// `cmdline` is the already-assembled kernel cmdline string. `sink` selects the
/// serial console wiring (inherit the host terminal, or capture to a pipe).
/// Returns the built configuration plus the guest device name assigned to the
/// ephemeral scratch disk (`Some("vdb")`, …), or `None` when no scratch disk is
/// attached. The name is derived from the disk's actual slot in the storage
/// array, so the caller can write it into `boot.env` without re-deriving the
/// attach order.
pub(crate) fn build(
    config: &Config,
    cmdline: &str,
    vsock_port: Option<u32>,
    scratch_path: Option<&std::path::Path>,
    sink: SerialSink,
) -> Result<(Retained<VZVirtualMachineConfiguration>, Option<String>), Error> {
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();

        // Bootloader
        let boot = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &file_url(&config.kernel),
        );
        boot.setInitialRamdiskURL(Some(&file_url(&config.initramfs)));
        boot.setCommandLine(&nsstr(cmdline));
        cfg.setBootLoader(Some(&boot.into_super()));

        cfg.setCPUCount(config.vcpus as usize);
        cfg.setMemorySize(config.mem_mib * 1024 * 1024);

        // Serial console wiring. A virtio console port becomes `hvc{index}` in
        // the guest in array order.
        let serial_ports: Vec<Retained<VZSerialPortConfiguration>> = match sink {
            SerialSink::Inherit => {
                let attach =
                    VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                        VZFileHandleSerialPortAttachment::alloc(),
                        Some(&NSFileHandle::fileHandleWithStandardInput()),
                        Some(&NSFileHandle::fileHandleWithStandardOutput()),
                    );
                let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
                serial.setAttachment(Some(&attach.into_super()));
                vec![Retained::into_super(serial)]
            }
            SerialSink::Capture { write_fd } => {
                // hvc0: the captured console — guest exec output → host pipe.
                let write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    write_fd,
                    false,
                );
                let cap_attach =
                    VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                        VZFileHandleSerialPortAttachment::alloc(),
                        None,
                        Some(&write_handle),
                    );
                let cap = VZVirtioConsoleDeviceSerialPortConfiguration::new();
                cap.setAttachment(Some(&cap_attach.into_super()));

                // hvc1: discard sink for kernel console + /init chatter (cmdline
                // sets console=hvc1). VZ reliably delivers only the first console
                // to the host, which is fine: nothing reads hvc1. We wrap a real
                // /dev/null fd (closeOnDealloc) — VZ accepts a file-descriptor
                // attachment; `fileHandleWithNullDevice` is NOT a usable serial
                // attachment (it throws an Obj-C exception when started).
                let null_fd = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if null_fd < 0 {
                    return Err(Error::Io(std::io::Error::last_os_error()));
                }
                let null_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    null_fd,
                    true,
                );
                let null_attach =
                    VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                        VZFileHandleSerialPortAttachment::alloc(),
                        None,
                        Some(&null_handle),
                    );
                let null_port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
                null_port.setAttachment(Some(&null_attach.into_super()));

                vec![Retained::into_super(cap), Retained::into_super(null_port)]
            }
        };
        let serial_array: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&serial_ports);
        cfg.setSerialPorts(&serial_array);

        // Entropy + balloon
        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        let entropy_array: Retained<NSArray<VZEntropyDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(entropy)]);
        cfg.setEntropyDevices(&entropy_array);

        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
        let balloon_array: Retained<NSArray<VZMemoryBalloonDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(balloon)]);
        cfg.setMemoryBalloonDevices(&balloon_array);

        // virtio-fs shares (rootfs + extra)
        let mut fs_devices: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        if let Some(crate::Rootfs::Share(rs)) = &config.rootfs {
            let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &nsstr("rootfs"),
            );
            // The rootfs share is ALWAYS mounted read-only on the host. The
            // guest never writes through to the shared host directory: when the
            // rootfs is writable it overlays a per-session tmpfs upper over this
            // read-only lower (writes discarded on shutdown), and when it's
            // `--rootfs-ro` it mounts read-only directly. Host writability would
            // leak one session's writes (chromium profile, /etc/resolv.conf, …)
            // into every other session sharing the same extracted rootfs dir.
            // Explicit `--share` mounts remain writable; this is only the root.
            let dir = VZSharedDirectory::initWithURL_readOnly(
                VZSharedDirectory::alloc(),
                &file_url(&rs.path),
                true,
            );
            let share =
                VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
            fs.setShare(Some(&share.into_super()));
            fs_devices.push(fs.into_super());
        }
        for sh in &config.shares {
            let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &nsstr(&sh.tag),
            );
            let dir = VZSharedDirectory::initWithURL_readOnly(
                VZSharedDirectory::alloc(),
                &file_url(&sh.path),
                false,
            );
            let share =
                VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
            fs.setShare(Some(&share.into_super()));
            fs_devices.push(fs.into_super());
        }
        let fs_array: Retained<NSArray<VZDirectorySharingDeviceConfiguration>> =
            NSArray::from_retained_slice(&fs_devices);
        cfg.setDirectorySharingDevices(&fs_array);

        // virtio-blk disks. A block rootfs (e.g. squashfs) is attached
        // FIRST and read-only, so it deterministically enumerates as
        // slot 0 = /dev/vda; user `--disk`s follow on /dev/vdb…
        let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
        if let Some(crate::Rootfs::Block(rb)) = &config.rootfs {
            let url = file_url(&rb.path);
            let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url,
                true,
            )
            .map_err(|e| {
                Error::InvalidConfig(format!(
                    "rootfs block image {}: {}",
                    rb.path.display(),
                    e.localizedDescription()
                ))
            })?;
            let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &att.into_super(),
            );
            storage.push(blk.into_super());
        }
        for path in &config.disks {
            let url = file_url(path);
            let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url,
                false,
            )
            .map_err(|e| {
                Error::InvalidConfig(format!(
                    "disk {}: {}",
                    path.display(),
                    e.localizedDescription()
                ))
            })?;
            let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &att.into_super(),
            );
            storage.push(blk.into_super());
        }
        // Ephemeral scratch disk (--scratch), attached LAST and read-write so
        // it enumerates after the rootfs block (slot 0) and user --disks. The
        // guest formats it ext4 and uses it as the overlay upper layer. Its
        // guest device name is taken from its *actual* position in `storage`
        // (the count of preceding virtio-blk devices), so the name and the
        // attach order have a single owner here — `boot.env` carries it to the
        // guest. `None` when no scratch disk is attached.
        let mut scratch_dev = None;
        if let Some(path) = scratch_path {
            scratch_dev = Some(crate::cmdline::blk_device_name(storage.len()));
            let url = file_url(path);
            let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url,
                false,
            )
            .map_err(|e| {
                Error::InvalidConfig(format!(
                    "scratch disk {}: {}",
                    path.display(),
                    e.localizedDescription()
                ))
            })?;
            let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &att.into_super(),
            );
            storage.push(blk.into_super());
        }
        let storage_array: Retained<NSArray<VZStorageDeviceConfiguration>> =
            NSArray::from_retained_slice(&storage);
        cfg.setStorageDevices(&storage_array);

        // virtio-net (NAT) if --net
        if config.net {
            let net = VZVirtioNetworkDeviceConfiguration::new();
            let nat: Retained<VZNetworkDeviceAttachment> =
                Retained::into_super(VZNATNetworkDeviceAttachment::new());
            net.setAttachment(Some(&nat));
            let net_array: Retained<NSArray<VZNetworkDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(net)]);
            cfg.setNetworkDevices(&net_array);
        }

        // vsock device (if not disabled)
        if vsock_port.is_some() {
            let sock_dev = VZVirtioSocketDeviceConfiguration::new();
            let sock_array: Retained<NSArray<VZSocketDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(sock_dev)]);
            cfg.setSocketDevices(&sock_array);
        }

        // Validate before returning.
        cfg.validateWithError()
            .map_err(|e| Error::InvalidConfig(e.localizedDescription().to_string()))?;

        Ok((cfg, scratch_dev))
    }
}
