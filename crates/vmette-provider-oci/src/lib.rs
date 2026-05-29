//! OCI / Docker image rootfs provider for vmette.
//!
//! Implements [`vmette::provider::RootfsProvider`] for image references.
//! Accepts:
//!
//! * `oci://<ref>` — explicit scheme, never ambiguous
//! * `<ref>`       — bare image references (e.g. `alpine:3.20`,
//!                    `ghcr.io/foo/bar:tag`). The OCI provider is the
//!                    catch-all fallback once path-style and other-scheme
//!                    providers have declined.
//!
//! Pulls the manifest + layers, extracts in order applying OCI whiteouts,
//! caches by manifest digest, and (when [`Context::guest_helpers`] is
//! set) injects `vsock-send` and `vsock-runner` into `/usr/local/bin/` so
//! vsock workflows work uniformly across image sources.
//!
//! Authentication: anonymous only in v0.1. Docker Hub's anonymous token
//! flow is handled by `oci-client` transparently.
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
use vmette::provider::{inject_guest_helpers, Context, ProviderError, RootfsProvider};

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

impl From<Error> for ProviderError {
    fn from(e: Error) -> Self {
        match e {
            Error::OfflineCacheMiss(s) => ProviderError::OfflineCacheMiss(s),
            Error::InvalidReference(r, msg) => {
                ProviderError::InvalidSpec(format!("{r}: {msg}"))
            }
            Error::Io(io) => ProviderError::Io(io),
            other => ProviderError::Other(other.to_string()),
        }
    }
}

// ---- Provider impl -------------------------------------------------------

/// OCI / Docker image provider. Wraps [`pull_with_options`] behind the
/// vmette provider trait. Construct with [`OciProvider::new`] or
/// [`OciProvider::with_options`] for non-default cache TTL.
pub struct OciProvider {
    options: PullOptions,
}

impl Default for OciProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OciProvider {
    /// Construct with [`PullOptions::default`] (1-hour cache TTL, online).
    /// `Context::is_offline()` still overrides `options.offline` on each
    /// call, so a single provider instance serves both online and offline
    /// resolutions.
    pub fn new() -> Self {
        Self { options: PullOptions::default() }
    }

    /// Construct with custom pull options. `options.offline` is treated
    /// as the floor — `Context::is_offline()` can force it on for a
    /// single call but cannot turn it off.
    pub fn with_options(options: PullOptions) -> Self {
        Self { options }
    }
}

impl RootfsProvider for OciProvider {
    fn name(&self) -> &'static str {
        "oci"
    }

    fn matches(&self, spec: &str) -> bool {
        // Explicit scheme.
        if let Some(rest) = spec.strip_prefix("oci://") {
            return !rest.is_empty();
        }
        // Empty or path-like → not us.
        if spec.is_empty()
            || spec.starts_with('/')
            || spec.starts_with('.')
            || spec.starts_with('~')
        {
            return false;
        }
        // Any other URL-like scheme → not us. Use `find` rather than
        // `contains` so we can verify the scheme is well-formed at the
        // start — `image:tag@sha256://...` is contrived, but `contains`
        // would mis-classify any ref with `://` later in the string.
        if let Some(idx) = spec.find("://") {
            let scheme = &spec[..idx];
            // Conservative: a scheme is letters/digits/+/-/. only.
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            {
                return false;
            }
        }
        // Catch-all: looks like a bare image ref.
        true
    }

    fn provide(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError> {
        let image_ref = spec.strip_prefix("oci://").unwrap_or(spec);
        if image_ref.is_empty() {
            return Err(ProviderError::InvalidSpec(
                "empty image reference".into(),
            ));
        }
        let cache = ctx.provider_cache(self.name())?;

        let opts = PullOptions {
            cache_ttl: self.options.cache_ttl,
            offline: self.options.offline || ctx.is_offline(),
        };

        // Note: the tokio runtime is built lazily INSIDE pull_with_options
        // — only when a network roundtrip is actually needed. Cache hits
        // resolve via blocking fs::metadata and pay no runtime cost.
        let rootfs = pull_with_options_sync(image_ref, &cache, &opts).map_err(|e| {
            map_oci_error(spec, image_ref, e)
        })?;

        if let Some(src) = ctx.guest_helpers() {
            if let Err(e) = inject_guest_helpers(&rootfs, src) {
                warn!(error = %e, "guest-helper inject failed; vsock workflows may not work in this image");
            }
        }
        Ok(rootfs)
    }
}

/// Translate OCI puller errors to ProviderError, with a friendly hint
/// when an `InvalidReference` looks like the user mistook a local path
/// for an image ref (e.g. `--rootfs assets/alpine-rootfs` without the
/// `./` prefix DirProvider needs to claim it).
fn map_oci_error(spec: &str, image_ref: &str, e: Error) -> ProviderError {
    if let Error::InvalidReference(_, ref msg) = e {
        // A typical OCI ref has either a `:` (tag), a `.` in the first
        // slash-component (registry hostname), or is a single bareword.
        // None of those → could be a relative path the user forgot to
        // prefix with `./`. Verify on disk before suggesting the path
        // form so we don't misfire on legitimately-malformed OCI refs
        // (e.g. `MyOrg/MyRepo` — uppercase, no path on disk).
        let shape_pathlike = image_ref.contains('/')
            && !image_ref.contains(':')
            && image_ref
                .split('/')
                .next()
                .map(|first| !first.contains('.'))
                .unwrap_or(false);
        let exists_on_disk = shape_pathlike
            && std::fs::metadata(image_ref).map(|m| m.is_dir()).unwrap_or(false);
        if exists_on_disk {
            // "may be" leaves room for the rare case where a workspace
            // coincidentally contains a directory at the same relative
            // path as a malformed OCI ref (e.g. a vendored mirror at
            // ./MyOrg/MyRepo when the user actually meant the OCI ref
            // and just forgot lowercase-ascii). The OCI parse error is
            // still printed so the user has both leads.
            return ProviderError::InvalidSpec(format!(
                "{spec:?} may be a local directory rather than an OCI image reference. \
                 If so, write `./{image_ref}` so the dir provider claims it. \
                 (OCI parse error: {msg})"
            ));
        }
    }
    e.into()
}

/// Synchronous wrapper around [`pull_with_options`]. Builds the tokio
/// runtime only when it is actually needed (network branch); cache-hit
/// fast-paths do blocking fs IO and skip runtime construction entirely.
fn pull_with_options_sync(
    image_ref: &str,
    cache_root: &Path,
    options: &PullOptions,
) -> Result<PathBuf, Error> {
    // Probe the on-disk cache first using blocking fs IO. Sharing the
    // same `fast_path_lookup` as pull_with_options means the TTL +
    // offline-precedence rules can't drift between sync and async
    // entry paths.
    if let Some(rootfs) = fast_path_lookup(cache_root, image_ref, options) {
        return Ok(rootfs);
    }
    if options.offline {
        // Offline + no fast-path hit → either the fallback scan finds
        // something, or we bail. Neither path needs async.
        if let Some(rootfs) = scan_offline_fallback(cache_root, image_ref) {
            return Ok(rootfs);
        }
        return Err(Error::OfflineCacheMiss(image_ref.into()));
    }
    // Network branch — build the runtime now, not earlier. Cache-hit
    // resolutions skipped this entire block above.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::Other, format!("tokio init: {e}"))))?;
    rt.block_on(pull_with_options(image_ref, cache_root, options))
}

/// Blocking-fs equivalent of pull_with_options's cache-hit branch.
/// Returns Some(rootfs) iff a within-TTL (or offline) ready marker is
/// present on disk; otherwise None (caller must consult the network
/// or the offline-fallback scanner).
fn fast_path_lookup(
    cache_root: &Path,
    image_ref: &str,
    options: &PullOptions,
) -> Option<PathBuf> {
    let ref_file = cache_root
        .join("refs")
        .join(format!("{}.digest", sanitize_ref(image_ref)));
    let (digest, age) = read_ref_entry(&ref_file)?;
    let rootfs = extracted_path(cache_root, image_ref, &digest);
    let marker = rootfs.join(".vmette-image-ready");
    if !marker.exists() {
        return None;
    }
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
            "cache hit (sync fast-path); skipping registry round-trip"
        );
        Some(rootfs)
    } else {
        None
    }
}

// ---- pull + extract ------------------------------------------------------

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
    // Cache fast-path is identical to the sync wrapper's probe — share
    // it via `fast_path_lookup` so the TTL / offline-precedence rules
    // live in one place. The sync wrapper also calls this; the second
    // call here is a no-op (returns None immediately) when the sync
    // wrapper already filtered out the easy case.
    if let Some(rootfs) = fast_path_lookup(cache_root, image_ref, options) {
        return Ok(rootfs);
    }

    let reference: Reference = image_ref
        .parse()
        .map_err(|e: oci_client::ParseError| Error::InvalidReference(image_ref.into(), e.to_string()))?;

    let refs_dir = cache_root.join("refs");
    let ref_file = refs_dir.join(format!("{}.digest", sanitize_ref(image_ref)));

    // Offline + no usable ref entry → scan disk for ANY extracted rootfs
    // matching this image_ref. Salvages caches written by older binaries
    // (which never created the refs/ entry) and partial state where the
    // ref file was lost but the extracted tree survived.
    if options.offline {
        if let Some(rootfs) = scan_offline_fallback(cache_root, image_ref) {
            debug!(path = %rootfs.display(), "offline fallback: found cached rootfs without ref entry");
            return Ok(rootfs);
        }
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

    // Atomic marker write: stage to a temp file then rename, so a crash
    // between writes can't leave a truncated marker that future runs
    // mistake for a complete extraction.
    let staging = rootfs.join(".vmette-image-ready.tmp");
    std::fs::write(&staging, format!("{}\n", manifest_digest))?;
    std::fs::rename(&staging, &ready_marker)?;
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

/// Write the ref entry. If the file already contains the same digest,
/// just bump mtime instead of rewriting bytes — keeps cache-hit
/// reconfirmations metadata-only.
fn write_ref_entry(path: &Path, digest: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing.trim() == digest {
            // Touch mtime without rewriting content. File::set_modified
            // is stable since Rust 1.75.
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                let _ = f.set_modified(SystemTime::now());
                return Ok(());
            }
        }
    }
    std::fs::write(path, format!("{}\n", digest))?;
    Ok(())
}

/// Best-effort scan of `cache_root` for an extracted rootfs matching
/// `image_ref` when no refs/<ref>.digest entry exists. Returns the
/// most-recently-modified ready rootfs that matches the sanitized-
/// ref prefix, or None if none found.
fn scan_offline_fallback(cache_root: &Path, image_ref: &str) -> Option<PathBuf> {
    let prefix = format!("{}__", sanitize_ref(image_ref));
    let read = std::fs::read_dir(cache_root).ok()?;
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let rootfs = entry.path().join("rootfs");
        let marker = rootfs.join(".vmette-image-ready");
        let Ok(meta) = std::fs::metadata(&marker) else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        match best {
            Some((_, ref ts)) if *ts >= mtime => {}
            _ => best = Some((rootfs, mtime)),
        }
    }
    best.map(|(p, _)| p)
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

    #[test]
    fn oci_matches_bare_refs_and_oci_scheme() {
        let p = OciProvider::new();
        assert!(p.matches("alpine"));
        assert!(p.matches("alpine:3.20"));
        assert!(p.matches("python:3.12-alpine"));
        assert!(p.matches("ghcr.io/foo/bar:tag"));
        assert!(p.matches("oci://alpine:3.20"));
    }

    #[test]
    fn oci_rejects_paths_and_other_schemes() {
        let p = OciProvider::new();
        assert!(!p.matches("/abs/path"));
        assert!(!p.matches("./rel"));
        assert!(!p.matches("../up"));
        assert!(!p.matches("~/home"));
        assert!(!p.matches("tar+https://example.com/a.tgz"));
        assert!(!p.matches("tar+file:///tmp/a.tar"));
        assert!(!p.matches("https://example.com"));
        assert!(!p.matches(""));
        assert!(!p.matches("oci://"));
    }
}
