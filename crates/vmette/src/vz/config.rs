//! Translate a high-level [`crate::Config`] into a
//! `VZVirtualMachineConfiguration` ready to validate and start.

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

/// Build a fully-configured `VZVirtualMachineConfiguration` from `config`.
/// `vsock_port` is the already-resolved port (None to skip vsock device).
/// `cmdline` is the already-assembled kernel cmdline string.
pub(crate) fn build(
    config: &Config,
    cmdline: &str,
    vsock_port: Option<u32>,
    scratch_path: Option<&std::path::Path>,
) -> Result<Retained<VZVirtualMachineConfiguration>, Error> {
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

        // Serial port → host stdio
        let attach =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                Some(&NSFileHandle::fileHandleWithStandardInput()),
                Some(&NSFileHandle::fileHandleWithStandardOutput()),
            );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attach.into_super()));
        let serial_array: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(serial)]);
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
        if let Some(rs) = &config.rootfs_share {
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
        if let Some(rb) = &config.rootfs_block {
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
        // it enumerates after the rootfs block (slot 0) and user --disks —
        // the order cmdline::scratch_device_name() relies on to name it. The
        // guest formats it ext4 and uses it as the overlay upper layer.
        if let Some(path) = scratch_path {
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

        Ok(cfg)
    }
}
