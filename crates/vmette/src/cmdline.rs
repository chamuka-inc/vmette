//! Kernel-cmdline assembly. The host smuggles per-invocation state into
//! the guest by appending `vmette.*=…` tokens; the guest's `/init` parses
//! them in pure shell.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::Config;

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

    if let Some(rs) = &config.rootfs_share {
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
        s.push_str(&format!(" vmette.guest_vsock_port={}", config.guest_vsock_port));
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
        c.rootfs_share = Some(crate::RootfsShare { path: PathBuf::from("/r"), read_only: true });
        let s = build(&c, None);
        assert!(s.contains("vmette.rootfs=1"));
        assert!(s.contains("vmette.rootfs_ro=1"));
    }

    #[test]
    fn vsock_port_only_emitted_when_some() {
        let s = build(&base(), Some(55555));
        assert!(s.contains("vmette.vsock_port=55555"));

        let s = build(&base(), None);
        assert!(!s.contains("vmette.vsock_port"));
    }
}
