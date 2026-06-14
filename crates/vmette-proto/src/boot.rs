//! The **host→guest boot contract**. `BootParams` is the single owner of the
//! configuration the host hands the guest's PID-1 (`/init`) at boot: the exec
//! command, environment, rootfs mode, extra shares, and workload strategy.
//!
//! Today this state is smuggled as `vmette.*=` tokens on the kernel cmdline and
//! re-parsed by hand in pure shell — an untyped channel with a ~3000-char budget
//! and a cross-file `scratch_dev`/attach-order invariant held together only by a
//! comment. `BootParams` replaces that: the host serializes it to a small
//! `KEY=VALUE` envelope written onto the `ctl` virtio-fs share, and the guest
//! reads that one file. The cmdline shrinks to the handful of tokens the *kernel*
//! itself consumes plus `vmette.boot=ctl`.
//!
//! Like the rest of `vmette-proto`, this module owns the **types**, not the I/O.
//! The `KEY=VALUE` codec (`to_env`/`from_env`) lives with its transport in the
//! `vmette` crate — the same convention by which the vsock frame codec lives in
//! `vmette::desktop` and the daemon's JSON loop lives in `vmette-daemon`, not
//! here. Keeping the codec out of this leaf preserves its minimal dependency
//! surface (the encoding of `exec`/`env` needs base64, which belongs with the
//! transport, not the contract).
//!
//! [`BOOT_PROTO_VERSION`] is carried in every envelope; the guest refuses to
//! boot on a mismatch (a stale initramfs fails *closed*, loudly, instead of
//! silently ignoring an unknown shape — today's failure mode).

use serde::{Deserialize, Serialize};

/// The version of the boot contract this build speaks. Bump on ANY breaking
/// change to the field set or its semantics. The guest asserts the envelope's
/// `proto_version` equals its own and aborts the boot otherwise.
pub const BOOT_PROTO_VERSION: u32 = 1;

/// How the guest root is provided. Mirrors the host's mutually-exclusive
/// `RootfsShare` / `RootfsBlock`, but as one closed enum so "which rootfs" is a
/// single value the guest matches on rather than two booleans it has to
/// reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootfsSpec {
    /// virtio-fs directory share (tag `rootfs`), overlaid in the guest. The host
    /// always mounts it read-only; `read_only` here is whether the *guest*
    /// presents a read-only root (`--rootfs-ro`) vs. a writable tmpfs/scratch
    /// overlay.
    Share { read_only: bool },
    /// Block image (e.g. squashfs) on `/dev/vda`, overlaid in the guest.
    /// `fstype` is the filesystem to mount it as (e.g. `"squashfs"`).
    Block { fstype: String },
}

/// The guest workload, replacing the cmdline's `vmette.desktop`/`vmette.display`
/// and `vmette.snapshot_mode`/`vmette.guest_vsock_port` tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    /// Run the exec command and power off.
    OneShot,
    /// Boot the persistent desktop (Xvfb at `width`x`height` + the computer-use
    /// agent).
    Agent { width: u32, height: u32 },
    /// Snapshot-build "server" mode — an Apple-Silicon feature (Phase 5;
    /// `saveMachineStateToURL:` is arm64-gated). The guest execs `vsock-runner`,
    /// which signals READY and blocks on `accept()` so the host can save the
    /// VM's memory state, then runs the command delivered on resume.
    /// `guest_vsock_port` is the in-guest listen port; the host-side vsock port
    /// rides the kernel cmdline (`vmette.vsock_port`).
    Snapshot { guest_vsock_port: u32 },
}

/// The complete host→guest boot configuration. Built host-side from the
/// `vmette::Config`, serialized to the `ctl` share's `boot.env`, and consumed
/// once by the guest's `/init`.
///
/// `exec` and `env_exports` carry *raw* shell text (a possibly multi-line
/// command; pre-rendered `export KEY='VALUE'` lines). The `KEY=VALUE` codec
/// base64-encodes them so they survive the line-oriented envelope intact — the
/// guest already has busybox `base64 -d`. Transport-bootstrap values that the
/// guest may need before (or independent of) the `ctl` mount — the vsock port —
/// deliberately stay on the kernel cmdline, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootParams {
    /// Contract version; checked against [`BOOT_PROTO_VERSION`] by the guest.
    pub proto_version: u32,
    /// How the root filesystem is provided and overlaid.
    pub rootfs: RootfsSpec,
    /// Guest device name of the ephemeral scratch disk (`vda`/`vdb`/…), assigned
    /// host-side from the virtio-blk attach order; `None` when no scratch disk is
    /// attached (RAM-backed tmpfs overlay).
    pub scratch_dev: Option<String>,
    /// Extra user share tags (the `--share` mounts), mounted at `/mnt/<tag>`. The
    /// implicit `ctl` share is excluded — the guest already knows about it.
    pub shares: Vec<String>,
    /// The raw exec command (possibly multi-line); `None` drops to an
    /// interactive shell.
    pub exec: Option<String>,
    /// Pre-rendered shell `export KEY='VALUE'` lines (caller `--env` merged over
    /// image env, by the host's single env renderer); `None` when empty.
    pub env_exports: Option<String>,
    /// `switch_root` into the new root instead of `chroot` (block/large rootfs).
    pub switch_root: bool,
    /// Bring up guest networking (NAT).
    pub net: bool,
    /// One-shot exec vs. persistent desktop agent.
    pub strategy: Strategy,
    /// Capture mode: the host wired a dedicated clean console (`hvc0`) for the
    /// exec's combined stdout+stderr and moved the kernel console + `/init`
    /// chatter to a discarded second console (`hvc1`). The guest runs the user
    /// command with its output redirected to `/dev/hvc0`, so the host reads a
    /// clean stream with no kernel/init noise (replacing marker-scraping). When
    /// `false`, the exec inherits the single console as before.
    pub capture: bool,
}

impl BootParams {
    /// Construct with [`BOOT_PROTO_VERSION`] and the given rootfs, leaving the
    /// rest at their empty/false defaults. Callers set the remaining fields.
    pub fn new(rootfs: RootfsSpec) -> Self {
        Self {
            proto_version: BOOT_PROTO_VERSION,
            rootfs,
            scratch_dev: None,
            shares: Vec::new(),
            exec: None,
            env_exports: None,
            switch_root: false,
            net: false,
            strategy: Strategy::OneShot,
            capture: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_carries_current_version_and_defaults() {
        let p = BootParams::new(RootfsSpec::Share { read_only: false });
        assert_eq!(p.proto_version, BOOT_PROTO_VERSION);
        assert_eq!(p.rootfs, RootfsSpec::Share { read_only: false });
        assert!(p.scratch_dev.is_none());
        assert!(p.shares.is_empty());
        assert!(p.exec.is_none());
        assert!(p.env_exports.is_none());
        assert!(!p.switch_root);
        assert!(!p.net);
        assert_eq!(p.strategy, Strategy::OneShot);
        assert!(!p.capture);
    }

    #[test]
    fn json_round_trips() {
        // serde is not the wire format (that's the env codec in `vmette`), but a
        // round-trip guards the type shape and is the proto crate's convention.
        let p = BootParams {
            proto_version: BOOT_PROTO_VERSION,
            rootfs: RootfsSpec::Block {
                fstype: "squashfs".into(),
            },
            scratch_dev: Some("vdb".into()),
            shares: vec!["work".into(), "data".into()],
            exec: Some("echo hi\nuname -a".into()),
            env_exports: Some("export FOO='bar'\n".into()),
            switch_root: true,
            net: true,
            strategy: Strategy::Agent {
                width: 1280,
                height: 800,
            },
            capture: true,
        };
        let j = serde_json::to_string(&p).unwrap();
        let back: BootParams = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rootfs_and_strategy_variants_are_distinct() {
        assert_ne!(
            RootfsSpec::Share { read_only: true },
            RootfsSpec::Share { read_only: false }
        );
        assert_ne!(
            Strategy::Agent {
                width: 1,
                height: 2
            },
            Strategy::OneShot
        );
    }
}
