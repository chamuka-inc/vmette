//! Squashfs block-image rootfs provider for vmette.
//!
//! Claims specs of the form:
//!
//! * `squashfs+https://host/path/image.sqfs`
//! * `squashfs+http://host/path/image.sqfs`
//! * `squashfs+file:///abs/path/image.sqfs`
//!
//! Unlike the directory-tree providers (`dir`, `oci`, `tar`), this one
//! returns a [`RootfsArtifact::BlockImage`]: the `.sqfs` file is attached
//! verbatim as a read-only virtio-blk device (`/dev/vda`) and the guest
//! overlays a tmpfs for writes. There is **no extraction** — the win is an
//! instant attach of a single, already-built image, shared read-only across
//! sessions.
//!
//! ## Why prebuilt
//!
//! Building a squashfs on the host would need `mksquashfs` (not present on
//! macOS) and would still stage an extracted tree to host disk first,
//! defeating the point. The image is therefore produced on Linux (CI), where
//! `mksquashfs` is native, and consumed here as one artifact.
//!
//! ## Resolution
//!
//! * `squashfs+file://` — used in place; no copy, no cache. The file must
//!   exist and be a regular file.
//! * `squashfs+http(s)://` — downloaded once to `<cache>/squashfs/<key>.sqfs`
//!   with a streaming size cap (this is one large file), then reused while
//!   fresh. `Context::is_offline()` always takes the cache regardless of age.
//!
//! Auth: none (mirrors the tar provider). For private endpoints pre-fetch the
//! image and use `squashfs+file://`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use thiserror::Error;
use tracing::{debug, info};
use vmette::provider::{BlockFs, Context, ProviderError, RootfsArtifact, RootfsProvider};

/// Default cap on the downloaded image size. 4 GiB is generous for a microVM
/// rootfs image while bounding a runaway/hostile download. Override with the
/// `VMETTE_SQUASHFS_MAX_BYTES` env var.
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_BYTES_ENV: &str = "VMETTE_SQUASHFS_MAX_BYTES";

/// On-disk magic of a squashfs superblock (`SQUASHFS_MAGIC` 0x73717368 stored
/// little-endian → ASCII "hsqs"). Used to reject non-squashfs payloads (a
/// soft-404 HTML body, a wrong `file://` target) before they poison the cache
/// or fail to mount in the guest with an opaque error.
const SQUASHFS_MAGIC: [u8; 4] = *b"hsqs";

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("download: {0}")]
    Download(String),

    #[error("image too large: exceeds {0} bytes; raise it via the {MAX_BYTES_ENV} env var")]
    TooLarge(u64),

    #[error("not a squashfs image: {0}")]
    NotSquashfs(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<Error> for ProviderError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidUrl(s) => ProviderError::InvalidSpec(s),
            Error::Download(s) => ProviderError::Network(s),
            Error::TooLarge(_) => ProviderError::Other(e.to_string()),
            Error::NotSquashfs(_) => ProviderError::Other(e.to_string()),
            Error::Io(io) => ProviderError::Io(io),
        }
    }
}

/// Squashfs block-image provider. Honours `squashfs+http://`,
/// `squashfs+https://`, `squashfs+file://`.
pub struct SquashfsProvider {
    /// Per-request HTTP timeout for downloads. Default: 5 minutes.
    pub timeout: Duration,
    /// Hard cap on the downloaded image size, enforced as the body streams.
    /// Default: [`DEFAULT_MAX_BYTES`], overridable via [`MAX_BYTES_ENV`].
    pub max_bytes: u64,
    /// How long a cached download is trusted before re-fetching.
    /// `Context::is_offline()` always overrides this and uses cache.
    /// `None` = always re-fetch when online. Default: 1 hour.
    pub cache_ttl: Option<Duration>,
}

impl Default for SquashfsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SquashfsProvider {
    pub fn new() -> Self {
        let max_bytes = std::env::var(MAX_BYTES_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_BYTES);
        Self {
            timeout: Duration::from_secs(300),
            max_bytes,
            cache_ttl: Some(Duration::from_secs(3600)),
        }
    }
}

impl RootfsProvider for SquashfsProvider {
    fn name(&self) -> &'static str {
        "squashfs"
    }

    fn matches(&self, spec: &str) -> bool {
        spec.starts_with("squashfs+http://")
            || spec.starts_with("squashfs+https://")
            || spec.starts_with("squashfs+file://")
    }

    fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
        let url = spec
            .strip_prefix("squashfs+")
            .ok_or_else(|| ProviderError::InvalidSpec(format!("not a squashfs+ spec: {spec}")))?;

        let path = if let Some(p) = url.strip_prefix("file://") {
            resolve_file(p).map_err(ProviderError::from)?
        } else {
            self.resolve_remote(url, ctx).map_err(ProviderError::from)?
        };

        Ok(RootfsArtifact::BlockImage {
            path,
            fstype: BlockFs::Squashfs,
        })
    }
}

impl SquashfsProvider {
    /// Resolve an `http(s)` image: cache hit (offline or within TTL) → reuse;
    /// else download with a streaming byte cap and atomically publish.
    fn resolve_remote(&self, url: &str, ctx: &Context) -> Result<PathBuf, Error> {
        let cache = ctx.provider_cache(self.name()).map_err(|e| match e {
            ProviderError::Io(io) => Error::Io(io),
            other => Error::Download(other.to_string()),
        })?;
        let dest = cache.join(format!("{}.sqfs", cache_key(url)));

        if dest.exists() {
            let age = dest
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .unwrap_or(Duration::ZERO);
            let fresh_enough =
                ctx.is_offline() || self.cache_ttl.map(|ttl| age <= ttl).unwrap_or(false);
            if fresh_enough {
                debug!(path = %dest.display(), age_s = age.as_secs(), "squashfs cache hit");
                return Ok(dest);
            }
            debug!(path = %dest.display(), "squashfs cache expired; refetching");
        }

        if ctx.is_offline() {
            return Err(Error::Download(format!("offline: {url} not in cache")));
        }

        info!(url = %url, dest = %dest.display(), "downloading squashfs image");
        download_to(url, &dest, &cache, self.timeout, self.max_bytes)?;
        info!(path = %dest.display(), "squashfs image ready");
        Ok(dest)
    }
}

/// Resolve a `file://` path (RFC 8089 `localhost/` form accepted) and verify
/// it points at a regular squashfs image. A non-empty, non-`localhost`
/// authority (`file://nas01/img.sqfs`) is rejected rather than silently
/// reinterpreted as a CWD-relative path.
fn resolve_file(raw: &str) -> Result<PathBuf, Error> {
    let path = if let Some(rest) = raw.strip_prefix("localhost/") {
        format!("/{rest}")
    } else if raw.starts_with('/') {
        raw.to_string()
    } else {
        return Err(Error::InvalidUrl(format!(
            "file:// authority must be empty or 'localhost': {raw}"
        )));
    };
    let path = PathBuf::from(path);
    let meta = std::fs::metadata(&path)
        .map_err(|e| Error::InvalidUrl(format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Error::InvalidUrl(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    verify_squashfs_magic(&path)?;
    Ok(path)
}

/// Read the first bytes of `path` and confirm the squashfs superblock magic.
/// Cheap (one short read) and runs on cache hits and `file://` targets alike.
fn verify_squashfs_magic(path: &Path) -> Result<(), Error> {
    use std::io::Read as _;
    let mut head = [0u8; SQUASHFS_MAGIC.len()];
    let mut f = std::fs::File::open(path)?;
    f.read_exact(&mut head).map_err(|_| {
        Error::NotSquashfs(format!(
            "{}: too short to be a squashfs image",
            path.display()
        ))
    })?;
    if head != SQUASHFS_MAGIC {
        return Err(Error::NotSquashfs(format!(
            "{}: missing squashfs magic (got {:02x?})",
            path.display(),
            head
        )));
    }
    Ok(())
}

/// Stream `url`'s body into `dest`, aborting if it exceeds `max_bytes`.
/// Downloads to a sibling staging file first and renames into place so a
/// crashed/partial download never gets mistaken for a complete image.
fn download_to(
    url: &str,
    dest: &Path,
    cache: &Path,
    timeout: Duration,
    max_bytes: u64,
) -> Result<(), Error> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(timeout)
        .timeout_write(timeout)
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| Error::Download(e.to_string()))?;
    let mut reader = resp.into_reader();

    let nonce = format!(
        "{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let staging = cache.join(format!("{}.staging.{}", cache_key(url), nonce));

    let result = (|| {
        let mut out = std::fs::File::create(&staging)?;
        copy_capped(&mut reader, &mut out, max_bytes)?;
        out.flush()?;
        out.sync_all()?;
        // Validate before publishing so a soft-404 HTML body or other
        // non-squashfs payload never gets cached as a `.sqfs`.
        verify_squashfs_magic(&staging)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&staging, dest).map_err(Error::Io)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&staging);
            Err(e)
        }
    }
}

/// Copy `reader` into `writer`, failing with [`Error::TooLarge`] once the
/// running total passes `max`.
fn copy_capped<R: Read, W: Write>(reader: &mut R, writer: &mut W, max: u64) -> Result<(), Error> {
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(Error::TooLarge(max));
        }
        writer.write_all(&buf[..n])?;
    }
    Ok(())
}

/// Stable cache file stem for a URL: a readable suffix (favouring the
/// filename) plus a hash of the full URL so distinct URLs never collide.
fn cache_key(url: &str) -> String {
    const PREFIX_MAX: usize = 64;
    let sanitised: String = url
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let suffix: String = sanitised
        .chars()
        .rev()
        .take(PREFIX_MAX)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    format!("{}__{:016x}", suffix, h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_squashfs_schemes() {
        let p = SquashfsProvider::new();
        assert!(p.matches("squashfs+http://example.com/a.sqfs"));
        assert!(p.matches("squashfs+https://example.com/a.sqfs"));
        assert!(p.matches("squashfs+file:///tmp/a.sqfs"));
        assert!(!p.matches("tar+https://example.com/a.tgz"));
        assert!(!p.matches("https://example.com/a.sqfs"));
        assert!(!p.matches("alpine:3.20"));
        assert!(!p.matches("/abs/path"));
        assert!(!p.matches(""));
    }

    #[test]
    fn cache_key_is_stable_and_disambiguates() {
        let a = "https://cdn.example.com/builds/alpine.sqfs";
        let b = "https://cdn.example.com/builds/debian.sqfs";
        assert_eq!(cache_key(a), cache_key(a));
        assert_ne!(cache_key(a), cache_key(b));
    }

    #[test]
    fn cache_key_keeps_filename() {
        let k = cache_key("https://example.com/a/b/c/vmette-desktop.sqfs");
        assert!(
            k.contains("vmette-desktop.sqfs"),
            "filename not preserved: {k}"
        );
    }

    #[test]
    fn copy_capped_rejects_oversize() {
        let data = vec![0u8; 4096];
        let mut out = Vec::new();
        let err = copy_capped(&mut data.as_slice(), &mut out, 1024).unwrap_err();
        assert!(matches!(err, Error::TooLarge(1024)));
    }

    #[test]
    fn copy_capped_passes_within_cap() {
        let data = vec![7u8; 512];
        let mut out = Vec::new();
        copy_capped(&mut data.as_slice(), &mut out, 1024).unwrap();
        assert_eq!(out.len(), 512);
    }

    #[test]
    fn resolve_file_rejects_missing() {
        let err = resolve_file("/definitely/not/here/x.sqfs").unwrap_err();
        assert!(matches!(err, Error::InvalidUrl(_)));
    }

    #[test]
    fn provide_file_returns_block_image() {
        // Create a real regular file to stand in for the .sqfs image.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file = std::env::temp_dir().join(format!(
            "vmette-sqfs-provide-{}-{}.sqfs",
            std::process::id(),
            nanos
        ));
        // Body must start with the squashfs magic to pass validation.
        let mut body = SQUASHFS_MAGIC.to_vec();
        body.extend_from_slice(b" not a real squashfs, just the magic");
        std::fs::write(&file, &body).unwrap();
        let abs = std::fs::canonicalize(&file).unwrap();
        let spec = format!("squashfs+file://{}", abs.display());
        let p = SquashfsProvider::new();
        let ctx = Context::new(std::env::temp_dir().join("vmette-sqfs-test"));
        let result = p.provide(&spec, &ctx);
        std::fs::remove_file(&file).ok();
        match result.unwrap() {
            RootfsArtifact::BlockImage { path, fstype } => {
                assert_eq!(path, abs);
                assert_eq!(fstype, BlockFs::Squashfs);
            }
            other => panic!("expected BlockImage, got {other:?}"),
        }
    }

    #[test]
    fn resolve_file_rejects_non_squashfs_body() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file = std::env::temp_dir().join(format!(
            "vmette-sqfs-bad-{}-{}.sqfs",
            std::process::id(),
            nanos
        ));
        std::fs::write(&file, b"<html>404 Not Found</html>").unwrap();
        let abs = std::fs::canonicalize(&file).unwrap();
        let err = resolve_file(&format!("{}", abs.display())).unwrap_err();
        std::fs::remove_file(&file).ok();
        assert!(matches!(err, Error::NotSquashfs(_)));
    }

    #[test]
    fn resolve_file_rejects_non_localhost_authority() {
        let err = resolve_file("nas01/exports/img.sqfs").unwrap_err();
        assert!(matches!(err, Error::InvalidUrl(_)));
    }
}
