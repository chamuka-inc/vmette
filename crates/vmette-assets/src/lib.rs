//! Locating the boot inputs a vmette binary needs to launch a VM.
//!
//! Every vmette binary that boots a VM (the `vmette` CLI and the
//! `vmette-mcp` server) shares this discovery so they probe the same
//! directories in the same order. `--kernel` / `--initramfs` / `--image`
//! may always be passed explicitly; when omitted we search, highest
//! priority first:
//!
//!   1. `$VMETTE_ASSETS_DIR/<guest-arch>/<name>` — explicit override
//!   2. `./assets/<guest-arch>/<name>`           — running from a repo checkout
//!   3. `<install prefix>/assets/<guest-arch>/<name>` — installed layout
//!
//! The release tarball ships `vmlinuz-virt` and `initramfs-vmette` under
//! `<prefix>/assets`, so a `curl | install.sh` user boots without flags.
//!
//! The desktop (Agent) workload needs one more input: the desktop rootfs
//! image. Unlike the kernel/initramfs it is *provider-resolved* (a `tar+file://`
//! / OCI spec, not a direct path), so [`resolve_desktop_image`] returns a spec
//! string rather than a path — but it is discovered through the *same* search,
//! so a locally built `vmette-desktop-rootfs.tar` in `assets/` takes precedence
//! over the published registry image, letting a dev session reflect the current
//! tree. This lives here, not in the daemon, because both clients already share
//! this crate and the daemon takes a concrete `image` in its request (like
//! kernel/initramfs).

use std::path::{Path, PathBuf};

use vmette_proto::ShareMount;

/// Canonical filename of the locally built desktop rootfs export. Produced by
/// `make desktop-image` (`scripts/build-desktop-image.sh --export`) from the
/// current `images/vmette-desktop/` source, so it always embodies the source
/// in the tree — no stale-registry guessing.
///
/// MUST match `DEFAULT_EXPORT` in `scripts/build-desktop-image.sh` (the script
/// writes the file; this discovers it). Renaming one without the other silently
/// breaks discovery.
pub const DESKTOP_ROOTFS_ASSET: &str = "vmette-desktop-rootfs.tar";

/// Env var that pins the desktop rootfs spec, overriding the discovered local
/// asset (but not an explicit per-call `--image`). Read from the *client*
/// process (the CLI invocation or the `vmette-mcp` server), consistent with how
/// kernel/initramfs are resolved client-side and passed to the daemon.
pub const DESKTOP_IMAGE_ENV: &str = "VMETTE_DESKTOP_IMAGE";

/// Desktop rootfs ref used when no explicit image, no `$VMETTE_DESKTOP_IMAGE`,
/// and no local [`DESKTOP_ROOTFS_ASSET`] is found. This is a public image,
/// published to GHCR by CI on every release tag, so it is the zero-setup default
/// for installed users; a locally built asset (above) takes precedence for devs.
pub const DEFAULT_DESKTOP_IMAGE: &str = "ghcr.io/chamuka-inc/vmette-desktop:latest";

/// Env var pinning a host directory of CA certificates to trust inside *every*
/// guest (a TLS-inspecting proxy root, an enterprise CA, …). Overrides the
/// default location but not an explicit per-call `--ca-certs`. Read client-side
/// (CLI / `vmette-mcp`), like the desktop-image and asset envs.
pub const CA_CERTS_ENV: &str = "VMETTE_CA_CERTS";

/// virtio-fs share tag the guest's PID-1 init looks for to install host CA
/// certificates (mounted at `/mnt/certs`). MUST match the tag handled in
/// `scripts/custom-init.sh` and the desktop image's entrypoint.
pub const CA_CERTS_SHARE_TAG: &str = "certs";

/// Per-arch asset directory holding the host-injected desktop agent: a static
/// `vmette-desktop-agent` plus `vmette-desktop-run.sh`. Built by
/// `scripts/build-desktop-agent-static.sh` into `assets/<arch>/<name>`.
pub const DESKTOP_AGENT_DIR: &str = "desktop-agent";

/// virtio-fs share tag for the host-injected desktop agent (mounted at
/// `/mnt/agent`). When present, the guest init's desktop branch runs the
/// injected `vmette-desktop-run.sh` instead of requiring an agent baked into
/// the rootfs — so any GUI rootfs (Xvfb + a WM) works. MUST match the tag and
/// path handled in `scripts/custom-init.sh`. Reserved (a caller share may not
/// reuse it).
pub const AGENT_SHARE_TAG: &str = "agent";

/// Architecture name used by Alpine's release directories and by vmette's
/// per-guest-arch asset layout under `assets/`.
pub fn guest_arch() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        std::env::consts::ARCH
    }
}

/// Directories that may hold the boot assets, highest priority first.
pub fn asset_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(d) = std::env::var_os("VMETTE_ASSETS_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("assets"));
    }
    // The installed layout is `<prefix>/bin/<binary>` + `<prefix>/assets`.
    // Canonicalize so a symlinked `~/.local/bin/vmette` resolves to the
    // real binary, making this correct for any `$PREFIX`.
    if let Ok(exe) = std::env::current_exe() {
        let real = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(prefix) = real.parent().and_then(|bin| bin.parent()) {
            dirs.push(prefix.join("assets"));
        }
    }

    dirs
}

fn asset_candidate(base: &Path, name: &str) -> PathBuf {
    base.join(guest_arch()).join(name)
}

/// Probe [`asset_dirs`] for `name`, returning every path that would be checked
/// (the per-arch `assets/<arch>/<name>` under each search root).
pub fn candidate_paths(name: &str) -> Vec<PathBuf> {
    asset_dirs()
        .into_iter()
        .map(|base| asset_candidate(&base, name))
        .collect()
}

/// Probe [`asset_dirs`] for `name`, returning the first match. Non-erroring
/// sibling of [`require_asset`]: used where a missing asset is a soft fallback
/// (e.g. the optional local desktop rootfs) rather than a hard failure.
pub fn find(name: &str) -> Option<PathBuf> {
    candidate_paths(name).into_iter().find(|p| p.exists())
}

/// Root of vmette's on-disk cache (`~/Library/Caches/vmette`): resolved
/// provider rootfs trees, the daemon socket, and friends. Single source of
/// truth shared by the `vmette` CLI, `vmetted`, and `vmette-mcp`, so all three
/// read and write the same cache (e.g. OCI/tar trees are reused across them).
pub fn default_cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/vmette")
}

/// Path to the daemon's UNIX socket (`<cache root>/vmette.sock`) — the single
/// source of truth shared by the `vmette` CLI (`vmette desktop …`), the
/// `vmette-mcp` server, and `vmetted` itself: clients connect here (and
/// auto-start the daemon), the daemon binds here.
pub fn default_socket() -> PathBuf {
    default_cache_root().join("vmette.sock")
}

/// Locate the `vmetted` daemon binary: next to the current executable (install
/// and repo layouts put `vmette` / `vmette-mcp` beside `vmetted`), else on
/// `$PATH`. Canonicalize so a symlinked launcher resolves to the real bin dir
/// that holds `vmetted`. Shared by the CLI and the MCP server, which both
/// lazily start the daemon when none is listening.
pub fn locate_vmetted() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let real = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = real.parent() {
            let candidate = dir.join("vmetted");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join("vmetted");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve a boot asset. An explicit `--kernel` / `--initramfs` path wins;
/// otherwise probe [`asset_dirs`] for `name`. The error lists every
/// location searched so the user knows where to drop the file.
pub fn require_asset(explicit: Option<PathBuf>, name: &str) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(found) = find(name) {
        return Ok(found);
    }
    let searched = candidate_paths(name)
        .into_iter()
        .map(|p| format!("    {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "{name} not found. Pass an explicit path, set $VMETTE_ASSETS_DIR, \
         or place {name} in one of:\n{searched}"
    ))
}

/// Resolve the desktop rootfs spec for a desktop session, highest priority
/// first: an explicit per-call `--image`, then `$VMETTE_DESKTOP_IMAGE`, then a
/// locally built [`DESKTOP_ROOTFS_ASSET`] discovered in [`asset_dirs`] (as a
/// `tar+file://` spec), then the [`DEFAULT_DESKTOP_IMAGE`] registry fallback.
///
/// Always returns a spec (never fails): the registry fallback is the floor.
/// Mirrors [`require_asset`]'s "explicit wins" shape but yields a provider spec
/// string, since the desktop rootfs is provider-resolved rather than a path the
/// VM boots directly.
pub fn resolve_desktop_image(explicit: Option<String>) -> String {
    if let Some(img) = explicit.filter(|s| !s.trim().is_empty()) {
        return img;
    }
    if let Ok(env) = std::env::var(DESKTOP_IMAGE_ENV) {
        if !env.trim().is_empty() {
            return env;
        }
    }
    if let Some(path) = find(DESKTOP_ROOTFS_ASSET) {
        // Canonicalize so the `file://` URI is absolute regardless of which
        // search dir (cwd-relative `./assets`, `$VMETTE_ASSETS_DIR`, …) matched.
        let abs = std::fs::canonicalize(&path).unwrap_or(path);
        return format!("tar+file://{}", abs.display());
    }
    DEFAULT_DESKTOP_IMAGE.to_string()
}

/// Default host directory scanned for CA certificates when neither an explicit
/// path nor [`CA_CERTS_ENV`] is set: `$HOME/.config/vmette/certs`. Returned
/// only when it exists and is a directory.
pub fn default_ca_certs_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".config/vmette/certs");
    dir.is_dir().then_some(dir)
}

/// Resolve the host CA-certificate directory to trust inside guests, highest
/// priority first: an explicit per-call path, then [`CA_CERTS_ENV`], then the
/// machine-wide [`default_ca_certs_dir`]. Returns `None` when none apply (the
/// common case — no proxy CA configured), so callers add no share.
///
/// Unlike [`resolve_desktop_image`] this has no registry floor: trusting an
/// extra CA is opt-in (it weakens isolation), so it only activates when the
/// user explicitly points at certs or stages them in the default location.
pub fn resolve_ca_certs(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Some(v) = std::env::var_os(CA_CERTS_ENV) {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    default_ca_certs_dir()
}

/// Resolve the host CA-cert directory ([`resolve_ca_certs`]) and wrap it as the
/// virtio-fs [`ShareMount`] the guest mounts (tag [`CA_CERTS_SHARE_TAG`]). The
/// single owner of "the CA share to inject" — every boot path (CLI one-shot, the
/// MCP run/desktop tools) builds it through here, so the tag and resolution
/// order can't drift. `None` when no CA is configured (the common case).
pub fn resolve_ca_share(explicit: Option<PathBuf>) -> Option<ShareMount> {
    resolve_ca_certs(explicit).map(|path| ShareMount {
        tag: CA_CERTS_SHARE_TAG.to_string(),
        path,
    })
}

/// Locate the host-injected desktop-agent directory ([`DESKTOP_AGENT_DIR`]) and
/// wrap it as the virtio-fs [`ShareMount`] (tag [`AGENT_SHARE_TAG`]) the daemon
/// mounts into Agent-workload guests. `None` when the asset isn't present (then
/// the guest falls back to an agent baked into the rootfs image). Validates the
/// dir actually carries the run script so a stray empty dir isn't injected.
pub fn resolve_agent_share() -> Option<ShareMount> {
    let dir = find(DESKTOP_AGENT_DIR)?;
    // Require both halves so a half-populated dir isn't injected (it would mount
    // fine but the run script would FATAL at boot for a missing agent binary).
    if !dir.join("vmette-desktop-run.sh").is_file() || !dir.join("vmette-desktop-agent").is_file() {
        return None;
    }
    Some(ShareMount {
        tag: AGENT_SHARE_TAG.to_string(),
        path: dir,
    })
}

#[cfg(test)]
mod ca_share_tests {
    use super::*;

    #[test]
    fn resolve_ca_share_wraps_with_the_tag() {
        // An explicit, non-empty path is returned as the certs share verbatim.
        let dir = std::env::temp_dir();
        let share = resolve_ca_share(Some(dir.clone())).expect("explicit dir resolves");
        assert_eq!(share.tag, CA_CERTS_SHARE_TAG);
        assert_eq!(share.path, dir);
    }
}
