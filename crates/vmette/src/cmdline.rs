//! Kernel-cmdline assembly. The per-invocation configuration the guest needs
//! travels in the typed `boot.env` envelope on the `ctl` share (see
//! [`crate::boot`]); the cmdline carries only what the *kernel* consumes plus
//! `vmette.boot=ctl` (telling `/init` to source that envelope) and, when vsock
//! is enabled, `vmette.vsock_port` (a transport-bootstrap value the guest may
//! need independent of the `ctl` mount).

use crate::Config;

/// The guest device name (`vda`, `vdb`, …) a virtio-blk device at attach
/// `index` enumerates as. Pure index→name mapping; the *order* (which slot a
/// disk lands in) is owned by [`crate::vz::config::build`], which assigns the
/// scratch disk's name from its actual position in the storage array — so the
/// name and the attach order have a single owner there, not a formula here.
pub(crate) fn blk_device_name(index: usize) -> String {
    // 26 virtio-blk devices is far more than any real config; the simple
    // single-letter form covers every case we can actually attach.
    let letter = (b'a' + index as u8) as char;
    format!("vd{letter}")
}

/// Build the full kernel cmdline string passed to `VZLinuxBootLoader`.
///
/// Combines `config.cmdline` (user-supplied / default `console=hvc0 quiet`)
/// with the two vmette tokens the boot still needs on the cmdline:
///
/// * `vmette.boot=ctl` — tells `/init` to mount the `ctl` virtio-fs share and
///   source its `boot.env` (the typed envelope holding exec, env, rootfs mode,
///   shares, scratch device, switch-root, net, and workload). Everything that
///   used to be a `vmette.*` token now lives there.
/// * `vmette.vsock_port=N` — a transport-bootstrap value (the guest agent may
///   need it independent of the `ctl` mount), emitted only when vsock is on.
pub(crate) fn build(config: &Config, effective_vsock_port: Option<u32>) -> String {
    let mut s = config.cmdline.clone();
    s.push_str(" vmette.boot=ctl");
    if let Some(port) = effective_vsock_port {
        s.push_str(&format!(" vmette.vsock_port={}", port));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> Config {
        Config::new("/k", "/i")
    }

    #[test]
    fn empty_cmdline_appends_only_boot_token() {
        // Everything per-invocation now lives in boot.env; the cmdline keeps the
        // kernel base + the one boot marker.
        let s = build(&base(), None);
        assert_eq!(s, "console=hvc0 quiet vmette.boot=ctl");
    }

    #[test]
    fn no_legacy_vmette_tokens_emitted() {
        // A fully-populated config must NOT leak any of the old tokens onto the
        // cmdline — they all moved to boot.env.
        let mut c = base();
        c.exec_cmd = Some("echo hi".into());
        c.rootfs = Some(crate::Rootfs::Share(crate::RootfsShare {
            path: PathBuf::from("/r"),
            read_only: true,
        }));
        c.shares = vec![crate::ShareMount {
            tag: "work".into(),
            path: PathBuf::from("/w"),
        }];
        c.env = vec![("FOO".into(), "bar".into())];
        c.switch_root = true;
        c.net = true;
        c.workload = crate::WorkloadStrategy::Agent;
        let s = build(&c, Some(5000));
        for token in [
            "vmette.exec",
            "vmette.rootfs",
            "vmette.rootfs_block",
            "vmette.rootfs_ro",
            "vmette.scratch_dev",
            "vmette.share=",
            "vmette.switch_root",
            "vmette.net",
            "vmette.desktop",
            "vmette.display",
            "vmette.env",
            "vmette.snapshot_mode",
        ] {
            assert!(!s.contains(token), "leaked legacy token {token}: {s}");
        }
        // Only the boot marker + vsock port remain.
        assert!(s.contains("vmette.boot=ctl"));
        assert!(s.contains("vmette.vsock_port=5000"));
    }

    #[test]
    fn vsock_port_only_emitted_when_some() {
        assert!(build(&base(), Some(55555)).contains("vmette.vsock_port=55555"));
        assert!(!build(&base(), None).contains("vmette.vsock_port"));
    }

    #[test]
    fn blk_device_name_maps_index_to_letter() {
        // Pure index→name: slot 0 = vda, 1 = vdb, … The attach-order logic that
        // decides the scratch disk's index lives in vz::config::build.
        assert_eq!(blk_device_name(0), "vda");
        assert_eq!(blk_device_name(1), "vdb");
        assert_eq!(blk_device_name(3), "vdd");
    }
}
