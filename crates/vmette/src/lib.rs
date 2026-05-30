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
pub mod settle;

pub use desktop::{Action, ResponseHeader, ScrollDirection};
pub use lifecycle::{run, RunOutput};
pub use session::{Session, SessionClient, SessionEnd, StopHandle};

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

/// Extra host directory mounted at `/mnt/<tag>` in the guest.
#[derive(Debug, Clone)]
pub struct ShareMount {
    pub tag: String,
    pub path: PathBuf,
}

/// One-shot VM configuration. Build with [`Config::new`], populate
/// public fields, then pass to [`run`].
#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    pub rootfs_share: Option<RootfsShare>,
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
        }
    }
}
