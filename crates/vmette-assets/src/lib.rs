//! Locating the boot inputs a vmette binary needs to launch a VM.
//!
//! Every vmette binary that boots a VM (the `vmette` CLI and the
//! `vmette-mcp` server) shares this discovery so they probe the same
//! directories in the same order. `--kernel` / `--initramfs` / `--image`
//! may always be passed explicitly; when omitted we search, highest
//! priority first:
//!
//!   1. `$VMETTE_ASSETS_DIR/<name>`       — explicit override
//!   2. `./assets/<name>`                 — running from a repo checkout
//!   3. `<install prefix>/assets/<name>`  — sibling of the binary's `bin/`
//!
//! The release tarball ships `vmlinuz-virt` and `initramfs-vmette` under
//! `<prefix>/assets`, so a `curl | install.sh` user boots without flags.
//!
//! The desktop (Agent) workload needs one more input: the desktop rootfs
//! image. Unlike the kernel/initramfs it is *provider-resolved* (a `tar+file://`
//! / OCI spec, not a direct path), so [`default_desktop_image`] returns a spec
//! string rather than a path — but it is discovered through the *same* search,
//! so a locally built `vmette-desktop-rootfs.tar` in `assets/` takes precedence
//! over the published registry image, letting a dev session reflect the current
//! tree. This lives here, not in the daemon, because both clients already share
//! this crate and the daemon takes a concrete `image` in its request (like
//! kernel/initramfs).

use std::path::PathBuf;

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

/// Probe [`asset_dirs`] for `name`, returning the first match. Non-erroring
/// sibling of [`require_asset`]: used where a missing asset is a soft fallback
/// (e.g. the optional local desktop rootfs) rather than a hard failure.
pub fn find(name: &str) -> Option<PathBuf> {
    asset_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.exists())
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
    let searched = asset_dirs()
        .into_iter()
        .map(|d| format!("    {}", d.join(name).display()))
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
pub fn default_desktop_image(explicit: Option<String>) -> String {
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
