//! vmette — local Linux microVM sandbox for macOS via Virtualization.framework.
//!
//! This crate is the host-side library. It wraps Apple's Virtualization
//! framework via `objc2-virtualization` and exposes a Rust API for booting
//! a Linux guest with virtio-fs shares, virtio-blk disks, virtio-net,
//! vsock, and a base64-encoded shell command delivered via the kernel
//! cmdline.
//!
//! See [`Config`] for the configurable surface and [`run`] for the
//! synchronous entry point.

use std::path::PathBuf;

pub mod error;
pub use error::Error;

mod cmdline;
mod lifecycle;
mod session;
mod terminal;
mod vz;

pub mod desktop;
pub mod ffi;
pub mod provider;

pub use desktop::{Action, ResponseHeader, ScrollDirection};
pub use lifecycle::{run, RunOutput};
pub use provider::{BlockFs, RootfsArtifact};
pub use session::{Session, SessionClient, SessionEnd, StopHandle};
/// The one workspace-wide host-directory share descriptor, owned by
/// `vmette-proto` so the daemon's run protocol and this config API share a
/// single type. Re-exported here as part of the core's public surface.
pub use vmette_proto::ShareMount;

/// Selects what the guest does once booted, and therefore which terminal
/// event ends the [`Session`].
///
/// - [`OneShot`](WorkloadStrategy::OneShot): the guest runs the
///   `vmette.exec` command and powers off, writing its code to
///   `.vmette-exit`. The session ends on the lifecycle-delegate poweroff.
///   This is the headless default and the only path the CLI/FFI use.
/// - [`Agent`](WorkloadStrategy::Agent): the guest starts a desktop
///   (Xvfb + WM + `vmette-desktop-agent`) and serves the framed
///   [`crate::desktop`] protocol over vsock. The session stays alive until
///   an explicit [`Session::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkloadStrategy {
    #[default]
    OneShot,
    Agent,
}

/// Per-invocation host vsock port policy.
#[derive(Debug, Clone, Copy, Default)]
pub enum VsockPort {
    /// Don't attach a vsock device at all.
    Disabled,
    /// Pick a random free port in 50000..60000 per invocation.
    #[default]
    Auto,
    /// Use the specified port.
    Fixed(u32),
}

/// Host directory exposed as the guest's `/`.
#[derive(Debug, Clone)]
pub struct RootfsShare {
    pub path: PathBuf,
    pub read_only: bool,
}

/// A filesystem image attached as virtio-blk slot 0 (`/dev/vda`) and
/// mounted read-only as the lower layer of a tmpfs-backed overlay root.
/// Mutually exclusive with [`RootfsShare`].
#[derive(Debug, Clone)]
pub struct RootfsBlock {
    pub path: PathBuf,
    pub fstype: BlockFs,
}

/// One-shot VM configuration. Build with [`Config::new`], populate
/// public fields, then pass to [`run`].
#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    pub rootfs_share: Option<RootfsShare>,
    /// Block-image rootfs (e.g. a squashfs), mutually exclusive with
    /// `rootfs_share`. When set, the image is attached read-only as
    /// `/dev/vda` and the guest overlays a tmpfs for writes.
    pub rootfs_block: Option<RootfsBlock>,
    pub shares: Vec<ShareMount>,
    pub disks: Vec<PathBuf>,
    pub exec_cmd: Option<String>,
    pub switch_root: bool,
    pub net: bool,
    pub vsock_port: VsockPort,
    pub guest_vsock_port: u32,
    pub timeout_seconds: Option<u32>,
    pub vcpus: u8,
    pub mem_mib: u64,
    pub build_snapshot: Option<PathBuf>,
    pub resume_snapshot: Option<PathBuf>,
    /// Guest workload selection. Defaults to
    /// [`WorkloadStrategy::OneShot`]; set to
    /// [`WorkloadStrategy::Agent`] for a persistent desktop session.
    pub workload: WorkloadStrategy,
    /// Xvfb framebuffer size `(width, height)` for the desktop, emitted on
    /// the cmdline only when `workload` is [`WorkloadStrategy::Agent`].
    pub display_size: (u32, u32),
    /// Suppress the human-facing launcher banner + "guest stopped" lines on
    /// stderr (errors still print). Set by the CLI's `--quiet`; used by the
    /// MCP server so an agent's captured output isn't polluted by launcher
    /// chatter. Has no effect on guest console output.
    pub quiet: bool,
    /// Extra environment variables exported in the guest before the exec
    /// command runs (the CLI's `--env KEY=VALUE`). Applied *after* any OCI
    /// image `Env`, so these override the image's values — like `docker run -e`.
    pub env: Vec<(String, String)>,
}

impl Config {
    /// Construct a config with the minimum required fields. All other
    /// fields take sensible defaults.
    pub fn new(kernel: impl Into<PathBuf>, initramfs: impl Into<PathBuf>) -> Self {
        Self {
            kernel: kernel.into(),
            initramfs: initramfs.into(),
            cmdline: "console=hvc0 quiet".into(),
            rootfs_share: None,
            rootfs_block: None,
            shares: Vec::new(),
            disks: Vec::new(),
            exec_cmd: None,
            switch_root: false,
            net: false,
            vsock_port: VsockPort::Auto,
            guest_vsock_port: 1025,
            timeout_seconds: None,
            vcpus: 1,
            mem_mib: 512,
            build_snapshot: None,
            resume_snapshot: None,
            workload: WorkloadStrategy::OneShot,
            display_size: (1280, 800),
            quiet: false,
            env: Vec::new(),
        }
    }

    /// Apply a resolved [`RootfsArtifact`] to this config, populating the
    /// matching rootfs field. `force_read_only` upgrades a `Directory`
    /// share to read-only (e.g. the CLI's `--rootfs-ro`); it has no effect
    /// on a block image, which is always attached read-only.
    pub fn set_rootfs_artifact(&mut self, artifact: RootfsArtifact, force_read_only: bool) {
        match artifact {
            RootfsArtifact::Directory { path, read_only } => {
                self.rootfs_block = None;
                self.rootfs_share = Some(RootfsShare {
                    path,
                    read_only: read_only || force_read_only,
                });
            }
            RootfsArtifact::BlockImage { path, fstype } => {
                self.rootfs_share = None;
                self.rootfs_block = Some(RootfsBlock { path, fstype });
            }
        }
    }
}

/// Render environment `(key, value)` pairs into a shell-sourceable string of
/// `export KEY='VALUE'` lines (one per valid pair), or `None` if no pair has a
/// usable key. Keys must be POSIX shell identifiers (`[A-Za-z_][A-Za-z0-9_]*`);
/// a value is single-quoted with embedded quotes escaped, so the result is safe
/// to `source`/`eval` in the guest with no further escaping.
///
/// This is the single renderer behind both env sources: the `--env` cmdline
/// channel (caller-supplied) and the OCI rootfs provider (an image's configured
/// `Env`). Keeping one renderer keeps their escaping and key rules identical.
///
/// Cross-crate internal helper (used by `vmette-cli` and `vmette-provider-oci`);
/// `#[doc(hidden)]` — not a stability-guaranteed public API.
#[doc(hidden)]
pub fn render_env_exports(pairs: &[(String, String)]) -> Option<String> {
    let mut out = String::new();
    for (key, val) in pairs {
        if !is_valid_env_key(key) {
            continue;
        }
        let escaped = val.replace('\'', "'\\''");
        out.push_str("export ");
        out.push_str(key);
        out.push_str("='");
        out.push_str(&escaped);
        out.push_str("'\n");
    }
    (!out.is_empty()).then_some(out)
}

/// True if `key` is a POSIX shell identifier (`[A-Za-z_][A-Za-z0-9_]*`) — the
/// rule an env var name must satisfy for `export KEY=…` to accept it. Shared so
/// the `--env` CLI can reject a bad key up front (clear error) using the *same*
/// rule [`render_env_exports`] uses to skip one (a silently-dropped var is a
/// confusing way to learn the key was invalid).
///
/// Cross-crate internal helper; `#[doc(hidden)]`.
#[doc(hidden)]
pub fn is_valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod env_tests {
    use super::{is_valid_env_key, render_env_exports};

    #[test]
    fn valid_env_keys() {
        assert!(is_valid_env_key("PATH"));
        assert!(is_valid_env_key("_x"));
        assert!(is_valid_env_key("A1_B2"));
        assert!(!is_valid_env_key("")); // empty
        assert!(!is_valid_env_key("1LEAD")); // leading digit
        assert!(!is_valid_env_key("FOO-BAR")); // dash
        assert!(!is_valid_env_key("FOO BAR")); // space
        assert!(!is_valid_env_key("a=b")); // contains '='
    }

    #[test]
    fn render_escapes_and_skips_invalid() {
        let pairs = vec![
            ("PATH".into(), "/a:/b".into()),
            ("WEIRD".into(), "it's".into()),
            ("HAS".into(), "a=b".into()),   // value may contain '='
            ("BAD-KEY".into(), "x".into()), // dropped
        ];
        let out = render_env_exports(&pairs).expect("some env");
        assert!(out.contains("export PATH='/a:/b'\n"));
        assert!(out.contains("export HAS='a=b'\n"));
        assert!(out.contains(r"export WEIRD='it'\''s'"));
        assert!(!out.contains("BAD-KEY"));
        // All-invalid renders to None.
        assert!(render_env_exports(&[("1BAD".into(), "x".into())]).is_none());
        assert!(render_env_exports(&[]).is_none());
    }
}
