//! The `KEY=VALUE` codec for the [`BootParams`] host→guest boot contract.
//!
//! The *types* live in [`vmette_proto::boot`]; this is the codec that carries
//! them, kept here with its transport (the `ctl` virtio-fs share) by the same
//! convention that puts the vsock frame codec in [`crate::desktop`]. The host
//! writes [`to_env`]'s output to `<ctl>/boot.env`; the guest's `/init` sources
//! that file (every value is single-quoted, so a plain `. boot.env` is safe) and
//! reads typed shell variables instead of grepping `vmette.*` cmdline tokens.
//!
//! ## Envelope format (the contract the guest shell mirrors)
//!
//! One `KEY='VALUE'` line per field; values single-quoted for safe sourcing.
//! `exec` and `env_exports` are base64 (they carry arbitrary multi-line shell);
//! everything else is a bare token. Keys:
//!
//! ```text
//! VMETTE_PROTO_VERSION='1'
//! VMETTE_ROOTFS_MODE='share'|'block'
//! VMETTE_ROOTFS_RO='0'|'1'           # share mode only
//! VMETTE_ROOTFS_FSTYPE='squashfs'    # block mode only
//! VMETTE_SCRATCH_DEV='vdb'           # omitted when no scratch disk
//! VMETTE_SHARES='work data'          # space-separated tags; omitted when none
//! VMETTE_EXEC_B64='…'                # omitted when no exec
//! VMETTE_ENV_B64='…'                 # omitted when no env
//! VMETTE_SWITCH_ROOT='0'|'1'
//! VMETTE_NET='0'|'1'
//! VMETTE_STRATEGY='oneshot'|'agent'
//! VMETTE_DISPLAY='1280x800'          # agent strategy only
//! ```
//!
//! `from_env` parses the same format back; it is the round-trip oracle for the
//! tests (the production guest consumer is the shell `/init`, which sources the
//! file directly), so it lives under `#[cfg(test)]` until a Rust decoder is
//! needed.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use vmette_proto::boot::{BootParams, RootfsSpec, Strategy, BOOT_PROTO_VERSION};

use crate::{render_env_exports, Config, WorkloadStrategy};

/// Map a host [`Config`] to the typed [`BootParams`] handed to the guest.
/// `scratch_dev` is the guest device name of the ephemeral scratch disk (from
/// the virtio-blk attach order), or `None` when none is attached.
///
/// The implicit `ctl` share is *not* listed in `shares` — the guest always knows
/// about it — so this is built from the caller's original `Config`, before the
/// `ctl` share is injected. A `block` rootfs wins over a `share` (matching
/// `cmdline::build`); the no-rootfs case is unreachable via the CLI/daemon/MCP
/// (all require `--rootfs`) and is treated as a writable share defensively.
pub(crate) fn from_config(config: &Config, scratch_dev: Option<&str>) -> BootParams {
    let rootfs = if let Some(rb) = &config.rootfs_block {
        RootfsSpec::Block {
            fstype: rb.fstype.as_str().to_string(),
        }
    } else {
        let read_only = config
            .rootfs_share
            .as_ref()
            .map(|r| r.read_only)
            .unwrap_or(false);
        RootfsSpec::Share { read_only }
    };

    let strategy = match config.workload {
        WorkloadStrategy::OneShot => Strategy::OneShot,
        WorkloadStrategy::Agent => {
            let (width, height) = config.display_size;
            Strategy::Agent { width, height }
        }
    };

    BootParams {
        proto_version: BOOT_PROTO_VERSION,
        rootfs,
        scratch_dev: scratch_dev.map(str::to_string),
        shares: config.shares.iter().map(|s| s.tag.clone()).collect(),
        exec: config.exec_cmd.clone(),
        env_exports: render_env_exports(&config.env),
        switch_root: config.switch_root,
        net: config.net,
        strategy,
        capture: config.capture_output,
    }
}

/// Render [`BootParams`] to the single-quoted `KEY='VALUE'` envelope written to
/// `<ctl>/boot.env`. The single owner of the host→guest boot wire format.
pub(crate) fn to_env(p: &BootParams) -> String {
    let mut s = String::new();
    let mut line = |k: &str, v: &str| {
        // Values are base64 or controlled tokens (no single quotes), so plain
        // single-quoting yields a safely-sourceable line.
        s.push_str(k);
        s.push_str("='");
        s.push_str(v);
        s.push_str("'\n");
    };

    line("VMETTE_PROTO_VERSION", &p.proto_version.to_string());

    match &p.rootfs {
        RootfsSpec::Share { read_only } => {
            line("VMETTE_ROOTFS_MODE", "share");
            line("VMETTE_ROOTFS_RO", if *read_only { "1" } else { "0" });
        }
        RootfsSpec::Block { fstype } => {
            line("VMETTE_ROOTFS_MODE", "block");
            line("VMETTE_ROOTFS_FSTYPE", fstype);
        }
    }

    if let Some(dev) = &p.scratch_dev {
        line("VMETTE_SCRATCH_DEV", dev);
    }
    if !p.shares.is_empty() {
        line("VMETTE_SHARES", &p.shares.join(" "));
    }
    if let Some(exec) = &p.exec {
        line("VMETTE_EXEC_B64", &B64.encode(exec.as_bytes()));
    }
    if let Some(env) = &p.env_exports {
        line("VMETTE_ENV_B64", &B64.encode(env.as_bytes()));
    }
    line("VMETTE_SWITCH_ROOT", if p.switch_root { "1" } else { "0" });
    line("VMETTE_NET", if p.net { "1" } else { "0" });
    line("VMETTE_CAPTURE", if p.capture { "1" } else { "0" });
    match &p.strategy {
        Strategy::OneShot => line("VMETTE_STRATEGY", "oneshot"),
        Strategy::Agent { width, height } => {
            line("VMETTE_STRATEGY", "agent");
            line("VMETTE_DISPLAY", &format!("{width}x{height}"));
        }
        Strategy::Snapshot { guest_vsock_port } => {
            line("VMETTE_STRATEGY", "snapshot");
            line("VMETTE_GUEST_VSOCK_PORT", &guest_vsock_port.to_string());
        }
    }

    s
}

/// Why parsing a `boot.env` envelope failed. Carried so a malformed envelope is
/// a typed error, not a silent default. Test-only (the production decoder is the
/// guest shell); promote out of `cfg(test)` when a Rust consumer needs it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootEnvError {
    /// A required key was absent.
    MissingKey(&'static str),
    /// A key held a value the codec can't interpret.
    BadValue { key: &'static str, value: String },
}

#[cfg(test)]
impl std::fmt::Display for BootEnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootEnvError::MissingKey(k) => write!(f, "boot.env: missing key {k}"),
            BootEnvError::BadValue { key, value } => {
                write!(f, "boot.env: bad value for {key}: {value:?}")
            }
        }
    }
}

#[cfg(test)]
impl std::error::Error for BootEnvError {}

/// Parse the `KEY='VALUE'` envelope back into [`BootParams`]. The round-trip
/// oracle for [`to_env`]; the production guest consumer is the shell `/init`.
#[cfg(test)]
pub(crate) fn from_env(text: &str) -> Result<BootParams, BootEnvError> {
    use std::collections::HashMap;
    let mut kv: HashMap<&str, String> = HashMap::new();
    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let Some((k, v)) = raw.split_once('=') else {
            continue;
        };
        // Strip one layer of surrounding single quotes.
        let v = v
            .strip_prefix('\'')
            .and_then(|v| v.strip_suffix('\''))
            .unwrap_or(v);
        kv.insert(k.trim(), v.to_string());
    }

    let get = |k: &'static str| kv.get(k).cloned().ok_or(BootEnvError::MissingKey(k));
    let b64 = |k: &'static str, v: &str| {
        B64.decode(v)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or(BootEnvError::BadValue {
                key: k,
                value: v.to_string(),
            })
    };
    let flag = |k: &'static str| -> Result<bool, BootEnvError> {
        match kv.get(k).map(String::as_str) {
            Some("1") => Ok(true),
            Some("0") | None => Ok(false),
            Some(other) => Err(BootEnvError::BadValue {
                key: k,
                value: other.to_string(),
            }),
        }
    };

    let proto_version =
        get("VMETTE_PROTO_VERSION")?
            .parse::<u32>()
            .map_err(|_| BootEnvError::BadValue {
                key: "VMETTE_PROTO_VERSION",
                value: kv.get("VMETTE_PROTO_VERSION").cloned().unwrap_or_default(),
            })?;

    let rootfs = match get("VMETTE_ROOTFS_MODE")?.as_str() {
        "share" => RootfsSpec::Share {
            read_only: flag("VMETTE_ROOTFS_RO")?,
        },
        "block" => RootfsSpec::Block {
            fstype: get("VMETTE_ROOTFS_FSTYPE")?,
        },
        other => {
            return Err(BootEnvError::BadValue {
                key: "VMETTE_ROOTFS_MODE",
                value: other.to_string(),
            })
        }
    };

    let scratch_dev = kv.get("VMETTE_SCRATCH_DEV").cloned();
    let shares = kv
        .get("VMETTE_SHARES")
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let exec = match kv.get("VMETTE_EXEC_B64") {
        Some(v) => Some(b64("VMETTE_EXEC_B64", v)?),
        None => None,
    };
    let env_exports = match kv.get("VMETTE_ENV_B64") {
        Some(v) => Some(b64("VMETTE_ENV_B64", v)?),
        None => None,
    };

    let strategy = match get("VMETTE_STRATEGY")?.as_str() {
        "oneshot" => Strategy::OneShot,
        "agent" => {
            let disp = get("VMETTE_DISPLAY")?;
            let (w, h) = disp.split_once(['x', 'X']).ok_or(BootEnvError::BadValue {
                key: "VMETTE_DISPLAY",
                value: disp.clone(),
            })?;
            let parse = |s: &str| {
                s.trim().parse::<u32>().map_err(|_| BootEnvError::BadValue {
                    key: "VMETTE_DISPLAY",
                    value: disp.clone(),
                })
            };
            Strategy::Agent {
                width: parse(w)?,
                height: parse(h)?,
            }
        }
        "snapshot" => {
            let p = get("VMETTE_GUEST_VSOCK_PORT")?;
            Strategy::Snapshot {
                guest_vsock_port: p.parse().map_err(|_| BootEnvError::BadValue {
                    key: "VMETTE_GUEST_VSOCK_PORT",
                    value: p.clone(),
                })?,
            }
        }
        other => {
            return Err(BootEnvError::BadValue {
                key: "VMETTE_STRATEGY",
                value: other.to_string(),
            })
        }
    };

    Ok(BootParams {
        proto_version,
        rootfs,
        scratch_dev,
        shares,
        exec,
        env_exports,
        switch_root: flag("VMETTE_SWITCH_ROOT")?,
        net: flag("VMETTE_NET")?,
        strategy,
        capture: flag("VMETTE_CAPTURE")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_maps_share_oneshot_and_round_trips() {
        let mut c = Config::new("/k", "/i");
        c.rootfs_share = Some(crate::RootfsShare {
            path: "/r".into(),
            read_only: false,
        });
        c.exec_cmd = Some("echo hi".into());
        c.shares = vec![crate::ShareMount {
            tag: "work".into(),
            path: "/w".into(),
        }];
        c.env = vec![("FOO".into(), "bar".into())];
        c.switch_root = true;

        let bp = from_config(&c, Some("vdb"));
        assert_eq!(bp.proto_version, BOOT_PROTO_VERSION);
        assert_eq!(bp.rootfs, RootfsSpec::Share { read_only: false });
        assert_eq!(bp.scratch_dev.as_deref(), Some("vdb"));
        assert_eq!(bp.shares, vec!["work".to_string()]); // ctl excluded by construction
        assert_eq!(bp.exec.as_deref(), Some("echo hi"));
        assert!(bp.env_exports.unwrap().contains("export FOO='bar'"));
        assert!(bp.switch_root);
        assert_eq!(bp.strategy, Strategy::OneShot);
        // The mapped params survive the env codec intact.
        assert_eq!(
            from_env(&to_env(&from_config(&c, Some("vdb"))))
                .unwrap()
                .exec,
            c.exec_cmd
        );
    }

    #[test]
    fn from_config_maps_block_agent() {
        let mut c = Config::new("/k", "/i");
        c.rootfs_block = Some(crate::RootfsBlock {
            path: "/img.sqfs".into(),
            fstype: crate::BlockFs::Squashfs,
        });
        c.workload = WorkloadStrategy::Agent;
        c.display_size = (1024, 768);

        let bp = from_config(&c, None);
        assert_eq!(
            bp.rootfs,
            RootfsSpec::Block {
                fstype: "squashfs".into()
            }
        );
        assert_eq!(
            bp.strategy,
            Strategy::Agent {
                width: 1024,
                height: 768
            }
        );
        assert!(bp.scratch_dev.is_none());
    }

    fn sample() -> BootParams {
        BootParams {
            proto_version: BOOT_PROTO_VERSION,
            rootfs: RootfsSpec::Block {
                fstype: "squashfs".into(),
            },
            scratch_dev: Some("vdb".into()),
            shares: vec!["work".into(), "data".into()],
            exec: Some("echo hi\nuname -a".into()),
            env_exports: Some("export FOO='bar baz'\n".into()),
            switch_root: true,
            net: true,
            strategy: Strategy::Agent {
                width: 1280,
                height: 800,
            },
            capture: true,
        }
    }

    #[test]
    fn round_trips_full() {
        let p = sample();
        assert_eq!(from_env(&to_env(&p)).unwrap(), p);
    }

    #[test]
    fn round_trips_minimal_oneshot_share() {
        let p = BootParams::new(RootfsSpec::Share { read_only: false });
        assert_eq!(from_env(&to_env(&p)).unwrap(), p);
    }

    #[test]
    fn round_trips_snapshot_strategy() {
        let mut p = BootParams::new(RootfsSpec::Block {
            fstype: "squashfs".into(),
        });
        p.strategy = Strategy::Snapshot {
            guest_vsock_port: 1025,
        };
        assert_eq!(from_env(&to_env(&p)).unwrap(), p);
    }

    #[test]
    fn round_trips_readonly_share_no_exec() {
        let mut p = BootParams::new(RootfsSpec::Share { read_only: true });
        p.net = true;
        assert_eq!(from_env(&to_env(&p)).unwrap(), p);
    }

    #[test]
    fn every_value_is_single_quoted() {
        // Guarantees `. boot.env` is safe to source (no unquoted word-splitting
        // or metacharacter execution).
        for line in to_env(&sample()).lines() {
            let (_k, v) = line.split_once('=').expect("KEY=VALUE");
            assert!(
                v.starts_with('\'') && v.ends_with('\''),
                "value not single-quoted: {line}"
            );
        }
    }

    #[test]
    fn exec_and_env_survive_multiline_and_quotes() {
        let mut p = BootParams::new(RootfsSpec::Share { read_only: false });
        p.exec = Some("printf 'a\\tb'\nfor i in 1 2; do echo \"$i\"; done".into());
        p.env_exports = Some("export A='x'\\''y'\n".into());
        let back = from_env(&to_env(&p)).unwrap();
        assert_eq!(back.exec, p.exec);
        assert_eq!(back.env_exports, p.env_exports);
    }

    #[test]
    fn missing_required_key_errors() {
        // Drop the rootfs mode line.
        let env = to_env(&BootParams::new(RootfsSpec::Share { read_only: false }));
        let stripped: String = env
            .lines()
            .filter(|l| !l.starts_with("VMETTE_ROOTFS_MODE"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(
            from_env(&stripped),
            Err(BootEnvError::MissingKey("VMETTE_ROOTFS_MODE"))
        );
    }

    #[test]
    fn bad_flag_value_errors() {
        let env = "VMETTE_PROTO_VERSION='1'\nVMETTE_ROOTFS_MODE='share'\nVMETTE_ROOTFS_RO='0'\n\
                   VMETTE_SWITCH_ROOT='maybe'\nVMETTE_NET='0'\nVMETTE_STRATEGY='oneshot'\n";
        assert_eq!(
            from_env(env),
            Err(BootEnvError::BadValue {
                key: "VMETTE_SWITCH_ROOT",
                value: "maybe".into()
            })
        );
    }

    #[test]
    fn tolerates_comments_and_blank_lines() {
        let mut env = String::from("# vmette boot envelope\n\n");
        env.push_str(&to_env(&BootParams::new(RootfsSpec::Share {
            read_only: false,
        })));
        assert!(from_env(&env).is_ok());
    }
}
