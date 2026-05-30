//! Locating the kernel + initramfs the guest boots from.
//!
//! Every vmette binary that boots a VM (the `vmette` CLI and the
//! `vmette-mcp` server) shares this discovery so they probe the same
//! directories in the same order. `--kernel` / `--initramfs` may always
//! be passed explicitly; when omitted we search, highest priority first:
//!
//!   1. `$VMETTE_ASSETS_DIR/<name>`       — explicit override
//!   2. `./assets/<name>`                 — running from a repo checkout
//!   3. `<install prefix>/assets/<name>`  — sibling of the binary's `bin/`
//!
//! The release tarball ships `vmlinuz-virt` and `initramfs-vmette` under
//! `<prefix>/assets`, so a `curl | install.sh` user boots without flags.

use std::path::PathBuf;

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

/// Resolve a boot asset. An explicit `--kernel` / `--initramfs` path wins;
/// otherwise probe [`asset_dirs`] for `name`. The error lists every
/// location searched so the user knows where to drop the file.
pub fn require_asset(explicit: Option<PathBuf>, name: &str) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let candidates: Vec<PathBuf> = asset_dirs().into_iter().map(|d| d.join(name)).collect();
    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        return Ok(found.clone());
    }
    let searched = candidates
        .iter()
        .map(|p| format!("    {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "{name} not found. Pass an explicit path, set $VMETTE_ASSETS_DIR, \
         or place {name} in one of:\n{searched}"
    ))
}
