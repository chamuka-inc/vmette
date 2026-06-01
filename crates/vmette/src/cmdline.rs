//! Kernel-cmdline assembly. The host smuggles per-invocation state into
//! the guest by appending `vmette.*=…` tokens; the guest's `/init` parses
//! them in pure shell.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::{Config, WorkloadStrategy};

/// Build the full kernel cmdline string passed to `VZLinuxBootLoader`.
///
/// Combines `config.cmdline` (user-supplied / default `console=hvc0 quiet`)
/// with `vmette.*` keys derived from the rest of the config.
pub(crate) fn build(config: &Config, effective_vsock_port: Option<u32>) -> String {
    let mut s = config.cmdline.clone();

    if let Some(cmd) = &config.exec_cmd {
        let b64 = B64.encode(cmd.as_bytes());
        s.push_str(" vmette.exec=");
        s.push_str(&b64);
    }

    // A block rootfs and a virtio-fs rootfs share are mutually exclusive;
    // the block branch wins (Config keeps only one set at a time).
    if let Some(rb) = &config.rootfs_block {
        s.push_str(" vmette.rootfs_block=");
        s.push_str(rb.fstype.as_str());
    } else if let Some(rs) = &config.rootfs_share {
        s.push_str(" vmette.rootfs=1");
        if rs.read_only {
            s.push_str(" vmette.rootfs_ro=1");
        }
    }

    for sh in &config.shares {
        s.push_str(" vmette.share=");
        s.push_str(&sh.tag);
    }

    if config.switch_root {
        s.push_str(" vmette.switch_root=1");
    }

    if config.net {
        s.push_str(" vmette.net=1");
    }

    if let Some(port) = effective_vsock_port {
        s.push_str(&format!(" vmette.vsock_port={}", port));
    }

    if config.build_snapshot.is_some() {
        s.push_str(" vmette.snapshot_mode=server");
        s.push_str(&format!(
            " vmette.guest_vsock_port={}",
            config.guest_vsock_port
        ));
    }

    if config.workload == WorkloadStrategy::Agent {
        let (w, h) = config.display_size;
        s.push_str(" vmette.desktop=1");
        s.push_str(&format!(" vmette.display={}x{}", w, h));
    }

    // Caller-supplied env (`--env`): base64 of shell-sourceable `export` lines.
    // The guest applies it *after* any OCI image env, so `--env` overrides the
    // image's values. (Image env rides in the rootfs, not the cmdline, so it
    // never competes for the ~3000-char cmdline budget.)
    if let Some(env) = crate::render_env_exports(&config.env) {
        s.push_str(" vmette.env=");
        s.push_str(&B64.encode(env.as_bytes()));
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
    fn empty_cmdline_keeps_base() {
        let s = build(&base(), None);
        assert_eq!(s, "console=hvc0 quiet");
    }

    #[test]
    fn exec_cmd_is_base64_encoded() {
        let mut c = base();
        c.exec_cmd = Some("echo hi".into());
        let s = build(&c, None);
        assert!(s.contains("vmette.exec="));
        // "echo hi" base64 is "ZWNobyBoaQ=="
        assert!(s.contains("ZWNobyBoaQ=="));
    }

    #[test]
    fn rootfs_ro_emits_both_keys() {
        let mut c = base();
        c.rootfs_share = Some(crate::RootfsShare {
            path: PathBuf::from("/r"),
            read_only: true,
        });
        let s = build(&c, None);
        assert!(s.contains("vmette.rootfs=1"));
        assert!(s.contains("vmette.rootfs_ro=1"));
    }

    #[test]
    fn rootfs_block_emits_fstype_and_suppresses_share() {
        let mut c = base();
        c.rootfs_block = Some(crate::RootfsBlock {
            path: PathBuf::from("/img.sqfs"),
            fstype: crate::BlockFs::Squashfs,
        });
        let s = build(&c, None);
        assert!(s.contains("vmette.rootfs_block=squashfs"));
        assert!(!s.contains("vmette.rootfs=1"));
    }

    #[test]
    fn desktop_tokens_only_emitted_for_agent() {
        let s = build(&base(), None);
        assert!(!s.contains("vmette.desktop"));

        let mut c = base();
        c.workload = crate::WorkloadStrategy::Agent;
        c.display_size = (1024, 768);
        let s = build(&c, None);
        assert!(s.contains("vmette.desktop=1"));
        assert!(s.contains("vmette.display=1024x768"));
    }

    #[test]
    fn env_emitted_only_when_set_and_base64_encoded() {
        let s = build(&base(), None);
        assert!(!s.contains("vmette.env="));

        let mut c = base();
        c.env = vec![("FOO".into(), "bar".into())];
        let s = build(&c, None);
        // base64 of "export FOO='bar'\n"
        let want = base64::engine::general_purpose::STANDARD.encode("export FOO='bar'\n");
        assert!(s.contains(&format!("vmette.env={want}")));
    }

    #[test]
    fn vsock_port_only_emitted_when_some() {
        let s = build(&base(), Some(55555));
        assert!(s.contains("vmette.vsock_port=55555"));

        let s = build(&base(), None);
        assert!(!s.contains("vmette.vsock_port"));
    }
}
