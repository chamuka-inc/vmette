//! Pull OCI / Docker images and extract them as plain directory trees
//! suitable for vmette's virtio-fs rootfs share.
//!
//! Public entry point: [`pull`]. Given an image reference (e.g.
//! `"alpine:3.20"`, `"python:3.12-alpine"`, `"ghcr.io/foo/bar:tag"`)
//! and a cache root, pulls the manifest + layers, extracts in order
//! applying OCI whiteouts, and returns the path to the assembled
//! rootfs. Idempotent — cached by manifest digest.
//!
//! Authentication: anonymous only in v0.1. Docker Hub's anonymous
//! token flow is handled by `oci-client` transparently.
//!
//! Layer formats supported:
//!   - application/vnd.oci.image.layer.v1.tar
//!   - application/vnd.oci.image.layer.v1.tar+gzip
//!   - application/vnd.oci.image.layer.v1.tar+zstd
//!   - application/vnd.docker.image.rootfs.diff.tar.gzip
//!   - application/vnd.docker.image.rootfs.diff.tar (rare)
//!
//! Whiteouts: AUFS-style as defined in the OCI image-spec.
//!   - `.wh.foo`        → delete `foo` from prior layers
//!   - `.wh..wh..opq`   → mark containing dir as opaque (clear children)

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use oci_client::{
    client::{linux_amd64_resolver, ClientConfig, ImageData},
    secrets::RegistryAuth,
    Client, Reference,
};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Pull-time policy. Controls how aggressively we revalidate the cached
/// manifest digest against the registry.
#[derive(Debug, Clone)]
pub struct PullOptions {
    /// When set, a cached ref younger than this TTL skips the registry
    /// roundtrip. `None` = always revalidate.
    pub cache_ttl: Option<Duration>,
    /// Never hit the network. Cache miss returns
    /// [`Error::OfflineCacheMiss`].
    pub offline: bool,
}

impl Default for PullOptions {
    /// Production-friendly defaults: 1-hour soft TTL, online.
    fn default() -> Self {
        Self {
            cache_ttl: Some(Duration::from_secs(3600)),
            offline: false,
        }
    }
}

pub const MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.oci.image.layer.v1.tar+zstd",
    "application/vnd.docker.image.rootfs.diff.tar",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
];

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid image reference '{0}': {1}")]
    InvalidReference(String, String),

    #[error("OCI registry: {0}")]
    Oci(#[from] oci_client::errors::OciDistributionError),

    #[error("missing manifest digest")]
    NoDigest,

    #[error("layer extraction: {0}")]
    Extract(String),

    #[error("offline mode: '{0}' not in cache")]
    OfflineCacheMiss(String),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Backwards-compatible entry point. Uses [`PullOptions::default`].
pub async fn pull(image_ref: &str, cache_root: &Path) -> Result<PathBuf, Error> {
    pull_with_options(image_ref, cache_root, &PullOptions::default()).await
}

/// Pull (or look up cached) an OCI image and return the path to its
/// extracted rootfs.
///
/// Cache layout under `cache_root`:
/// ```text
/// refs/<sanitized-ref>.digest       digest of the last-fetched manifest
///                                    for that ref. File mtime = fetched_at.
/// <sanitized-ref>__<digest>/rootfs   extracted image tree, idempotent
///                                    via .vmette-image-ready marker.
/// ```
pub async fn pull_with_options(
    image_ref: &str,
    cache_root: &Path,
    options: &PullOptions,
) -> Result<PathBuf, Error> {
    let reference: Reference = image_ref
        .parse()
        .map_err(|e: oci_client::ParseError| Error::InvalidReference(image_ref.into(), e.to_string()))?;

    let refs_dir = cache_root.join("refs");
    let ref_file = refs_dir.join(format!("{}.digest", sanitize_ref(image_ref)));

    // Fast path: cache hit + within-TTL (or offline) → no network at all.
    if let Some((digest, age)) = read_ref_entry(&ref_file) {
        let rootfs = extracted_path(cache_root, image_ref, &digest);
        let marker = rootfs.join(".vmette-image-ready");
        if marker.exists() {
            let fresh_enough = options.offline
                || options
                    .cache_ttl
                    .map(|ttl| age <= ttl)
                    .unwrap_or(false);
            if fresh_enough {
                debug!(
                    path = %rootfs.display(),
                    age_s = age.as_secs(),
                    offline = options.offline,
                    "cache hit; skipping registry round-trip"
                );
                return Ok(rootfs);
            }
        }
    }

    // Offline + no usable cache → fail fast.
    if options.offline {
        return Err(Error::OfflineCacheMiss(image_ref.into()));
    }

    info!(image = %image_ref, "resolving image");

    // Pick the linux/amd64 variant from multi-arch manifest lists. The
    // guest is always x86_64 Linux in v0.1; revisit when arm64 guest
    // assets land.
    let cfg = ClientConfig {
        platform_resolver: Some(Box::new(linux_amd64_resolver)),
        ..ClientConfig::default()
    };
    let client = Client::new(cfg);
    let auth = RegistryAuth::Anonymous;

    // Resolve manifest digest cheaply — single HEAD/GET, no blob downloads.
    let manifest_digest = client.fetch_manifest_digest(&reference, &auth).await?;

    let rootfs = extracted_path(cache_root, image_ref, &manifest_digest);
    let ready_marker = rootfs.join(".vmette-image-ready");

    if ready_marker.exists() {
        debug!(path = %rootfs.display(), "image already extracted; refreshing ref entry");
        write_ref_entry(&ref_file, &manifest_digest)?;
        return Ok(rootfs);
    }

    info!(digest = %manifest_digest, "cache miss; pulling layers");

    let image: ImageData = client
        .pull(&reference, &auth, MEDIA_TYPES.to_vec())
        .await?;

    info!(
        path = %rootfs.display(),
        layers = image.layers.len(),
        "extracting image"
    );

    if rootfs.exists() {
        std::fs::remove_dir_all(&rootfs)?;
    }
    std::fs::create_dir_all(&rootfs)?;

    for (i, layer) in image.layers.iter().enumerate() {
        let media = layer.media_type.as_str();
        debug!(
            i = i + 1,
            of = image.layers.len(),
            size = layer.data.len(),
            media_type = %media,
            "applying layer"
        );
        extract_layer(&layer.data, media, &rootfs)?;
    }

    std::fs::write(&ready_marker, format!("{}\n", manifest_digest))?;
    write_ref_entry(&ref_file, &manifest_digest)?;
    info!(path = %rootfs.display(), "image ready");
    Ok(rootfs)
}

fn extracted_path(cache_root: &Path, image_ref: &str, digest: &str) -> PathBuf {
    cache_root
        .join(format!("{}__{}", sanitize_ref(image_ref), digest_to_dir(digest)))
        .join("rootfs")
}

fn read_ref_entry(path: &Path) -> Option<(String, Duration)> {
    let digest = std::fs::read_to_string(path).ok()?;
    let digest = digest.trim().to_string();
    if digest.is_empty() {
        return None;
    }
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO);
    Some((digest, age))
}

fn write_ref_entry(path: &Path, digest: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", digest))?;
    Ok(())
}

/// Inject the contents of `src_bin_dir/{vsock-send,vsock-runner}` into
/// the given rootfs at `/usr/local/bin/`. Used after [`pull`] so vmette
/// guest helpers are available in any pulled image.
pub fn inject_guest_helpers(rootfs: &Path, src_bin_dir: &Path) -> Result<(), Error> {
    let target_dir = rootfs.join("usr/local/bin");
    std::fs::create_dir_all(&target_dir)?;
    for name in &["vsock-send", "vsock-runner"] {
        let src = src_bin_dir.join(name);
        if !src.exists() {
            warn!(name = name, "guest helper not found in source; skipping");
            continue;
        }
        let dst = target_dir.join(name);
        std::fs::copy(&src, &dst)?;
        // chmod 755
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// -----------------------------------------------------------------------------

fn sanitize_ref(image_ref: &str) -> String {
    image_ref
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

fn digest_to_dir(digest: &str) -> String {
    // "sha256:abc..." → "sha256-abc..." (no colon, valid path char)
    let cleaned: String = digest
        .chars()
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    // Trim to a manageable length; full digest is 64 hex chars + prefix.
    if cleaned.len() > 24 {
        // Keep prefix + first 16 hex of digest for human readability.
        let prefix_end = cleaned.find('-').map(|i| i + 1).unwrap_or(0);
        format!("{}{}", &cleaned[..prefix_end], &cleaned[prefix_end..prefix_end + 16])
    } else {
        cleaned
    }
}

/// Extract a single layer tarball into `dest`, applying OCI whiteouts.
fn extract_layer(data: &[u8], media_type: &str, dest: &Path) -> Result<(), Error> {
    let decompressed = decompress(data, media_type)?;
    let mut archive = tar::Archive::new(decompressed.as_slice());
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);

    for entry in archive.entries().map_err(|e| Error::Extract(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Extract(e.to_string()))?;
        let path_in_tar = entry.path().map_err(|e| Error::Extract(e.to_string()))?.into_owned();

        // Skip absolute paths / path traversal — tar entries should be
        // relative; oci-client images shouldn't include traversals.
        if path_in_tar.is_absolute() || path_in_tar.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            warn!(path = %path_in_tar.display(), "skipping unsafe path");
            continue;
        }

        let basename = path_in_tar.file_name().and_then(|s| s.to_str());

        match basename {
            Some(".wh..wh..opq") => {
                // Opaque dir marker: clear the parent dir in dest.
                if let Some(parent) = path_in_tar.parent() {
                    let target = dest.join(parent);
                    clear_dir_contents(&target);
                }
                continue;
            }
            Some(name) if name.starts_with(".wh.") => {
                // Whiteout: remove the named entry in dest.
                let stripped = &name[".wh.".len()..];
                let parent = path_in_tar.parent().unwrap_or_else(|| Path::new(""));
                let target = dest.join(parent).join(stripped);
                remove_anything(&target);
                continue;
            }
            _ => {}
        }

        // Regular extraction. unpack_in is safer than unpack (refuses
        // to escape dest).
        if let Err(e) = entry.unpack_in(dest) {
            // Common case: hardlink/symlink targets that don't exist
            // yet because later in the same tar. tar crate may not
            // resolve forward refs. Log + skip rather than abort.
            warn!(path = %path_in_tar.display(), error = %e, "extract skipped");
        }
    }
    Ok(())
}

fn decompress(data: &[u8], media_type: &str) -> Result<Vec<u8>, Error> {
    let mt = media_type.to_ascii_lowercase();
    if mt.contains("zstd") {
        let mut out = Vec::with_capacity(data.len() * 4);
        zstd::stream::copy_decode(Cursor::new(data), &mut out)
            .map_err(|e| Error::Extract(format!("zstd: {e}")))?;
        Ok(out)
    } else if mt.contains("gzip") {
        let mut out = Vec::with_capacity(data.len() * 4);
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut out)
            .map_err(|e| Error::Extract(format!("gzip: {e}")))?;
        Ok(out)
    } else {
        Ok(data.to_vec())
    }
}

/// Remove a file, symlink, or directory at `target`. Silent on absence.
fn remove_anything(target: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(target) {
        let _ = if meta.file_type().is_dir() {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
    }
}

/// Remove every entry inside `dir` but keep `dir` itself.
fn clear_dir_contents(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        remove_anything(&entry.path());
    }
}

// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_handles_slash_and_colon() {
        assert_eq!(sanitize_ref("alpine:3.20"), "alpine_3.20");
        assert_eq!(sanitize_ref("ghcr.io/foo/bar:tag"), "ghcr.io_foo_bar_tag");
        assert_eq!(sanitize_ref("python:3.12-alpine"), "python_3.12-alpine");
    }

    #[test]
    fn digest_to_dir_strips_colon_and_truncates() {
        let d = digest_to_dir("sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        assert!(d.starts_with("sha256-"));
        assert!(d.len() <= 24);
    }
}
