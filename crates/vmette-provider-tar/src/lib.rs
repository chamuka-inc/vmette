//! Tarball rootfs provider for vmette.
//!
//! Claims specs of the form:
//!
//! * `tar+https://host/path/rootfs.tar[.{gz,zst}]`
//! * `tar+http://host/path/rootfs.tar[.{gz,zst}]`
//! * `tar+file:///abs/path/rootfs.tar[.{gz,zst}]`
//!
//! On first use, downloads (or reads, for `file://`) the archive,
//! detects gzip / zstd / plain via magic bytes, extracts into
//! `<cache>/tar/<sanitized-url>__<urlhash>/`, and marks the directory
//! ready with `.vmette-tar-ready`. Subsequent calls within
//! [`TarProvider::cache_ttl`] short-circuit to the cached directory;
//! past TTL, the archive is re-fetched (so URLs whose contents rotate
//! don't return stale rootfs forever). `Context::is_offline()` always
//! takes the cache, regardless of age — better-stale-than-failed when
//! the user explicitly opted out of network.
//!
//! Like the OCI provider, this honours [`Context::guest_helpers`] and
//! injects `vsock-send` / `vsock-runner` into `/usr/local/bin/` after
//! extraction, so vsock workflows work against arbitrary tarballs.
//!
//! Auth: none. URLs are dereferenced as-is. For private endpoints
//! either pre-cache the tarball locally and use `tar+file://`, or
//! roll a custom provider.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use thiserror::Error;
use tracing::{debug, info, warn};
use vmette::provider::{inject_guest_helpers, Context, ProviderError, RootfsProvider};

const READY_MARKER: &str = ".vmette-tar-ready";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Default cap on the *extracted* rootfs size (decompressed bytes). 4 GiB is
/// generous for a microVM rootfs (even a chromium desktop image extracts to
/// well under that) while still bounding a decompression bomb. Override with
/// the `VMETTE_TAR_MAX_BYTES` env var or by setting [`TarProvider::max_bytes`].
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Env var to override [`DEFAULT_MAX_BYTES`] without a code change.
const MAX_BYTES_ENV: &str = "VMETTE_TAR_MAX_BYTES";

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("download: {0}")]
    Download(String),

    #[error("extract: {0}")]
    Extract(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<Error> for ProviderError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidUrl(s) => ProviderError::InvalidSpec(s),
            Error::Download(s) => ProviderError::Network(s),
            Error::Io(io) => ProviderError::Io(io),
            other => ProviderError::Other(other.to_string()),
        }
    }
}

/// Tarball rootfs provider. Honours `tar+http://`, `tar+https://`,
/// `tar+file://`.
pub struct TarProvider {
    /// Per-request HTTP timeout for downloads. Default: 5 minutes.
    pub timeout: Duration,
    /// Hard cap on the *extracted* rootfs size — counted in decompressed
    /// bytes as the archive is streamed, not the on-disk/compressed size of
    /// the source. A 320 MiB `.tar.gz` and the 880 MiB plain `.tar` it
    /// gzips from therefore behave identically: the bound is on what the
    /// rootfs actually costs, not on how the bytes happened to be packed.
    /// Doubles as decompression-bomb protection (extraction aborts once the
    /// decompressed stream passes the cap) and download-size protection (the
    /// counting reader stops pulling the source once the cap is hit).
    /// Default: [`DEFAULT_MAX_BYTES`] (4 GiB), overridable via
    /// [`MAX_BYTES_ENV`].
    pub max_bytes: u64,
    /// How long a cached extracted rootfs is trusted before re-fetching.
    /// `Context::is_offline()` always overrides this and uses cache.
    /// `None` = always re-fetch when online (every call hits the URL).
    /// Default: 1 hour, mirroring the OCI provider's ref-entry TTL.
    pub cache_ttl: Option<Duration>,
}

impl Default for TarProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TarProvider {
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

impl RootfsProvider for TarProvider {
    fn name(&self) -> &'static str {
        "tar"
    }

    fn matches(&self, spec: &str) -> bool {
        spec.starts_with("tar+http://")
            || spec.starts_with("tar+https://")
            || spec.starts_with("tar+file://")
    }

    fn provide(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError> {
        // `matches` already guarantees one of the prefixes, but be explicit
        // so the parser stays valid if `matches` is ever refactored.
        let url = spec
            .strip_prefix("tar+")
            .ok_or_else(|| ProviderError::InvalidSpec(format!("not a tar+ spec: {spec}")))?;

        let cache = ctx.provider_cache(self.name())?;
        let dest = cache.join(cache_key(url));
        let marker = dest.join(READY_MARKER);

        // Cache-hit fast path: marker present AND (offline OR within TTL).
        if marker.exists() {
            let age = marker
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .unwrap_or(Duration::ZERO);
            let fresh_enough =
                ctx.is_offline() || self.cache_ttl.map(|ttl| age <= ttl).unwrap_or(false);
            if fresh_enough {
                debug!(path = %dest.display(), age_s = age.as_secs(), "tar cache hit");
                if let Some(src) = ctx.guest_helpers() {
                    if let Err(e) = inject_guest_helpers(&dest, src) {
                        warn!(error = %e, "guest-helper inject failed on cache hit");
                    }
                }
                return Ok(dest);
            }
            debug!(path = %dest.display(), age_s = age.as_secs(), "tar cache expired; refetching");
        }

        if ctx.is_offline() {
            return Err(ProviderError::OfflineCacheMiss(spec.into()));
        }

        info!(url = %url, dest = %dest.display(), "fetching tarball");
        let source = open_source(url, self.timeout).map_err(ProviderError::from)?;

        // Extract into a sibling staging dir so the ready-marker only
        // appears when the tree is complete. Staging + trash names mix
        // PID with wall-clock nanos so concurrent threads in the same
        // process (PID alone collides) and serial calls (nanos alone
        // can collide under coarse clocks) both get unique paths.
        let nonce = format!(
            "{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let staging = cache.join(format!("{}.staging.{}", cache_key(url), nonce));
        let trash = cache.join(format!("{}.trash.{}", cache_key(url), nonce));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(ProviderError::Io)?;
        }
        std::fs::create_dir_all(&staging).map_err(ProviderError::Io)?;
        if let Err(e) = extract_into(source, &staging, self.max_bytes) {
            // Don't leak a partial (possibly large) staging tree on failure.
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ProviderError::from(e));
        }

        // Swap staging into dest. Concurrency: two racers can interleave
        // the rename-aside + rename-in pair such that the loser's
        // rename(staging→dest) finds dest already populated (winner ran
        // its rename-in between our rename-aside and our rename-in). We
        // handle that by detecting the populated-dest case and accepting
        // the winner's tree as canonical — both racers downloaded the
        // same URL recently, so either tree is equally valid.
        //
        // We do NOT use marker.exists() as a race detector. On the
        // TTL-expired-refetch path the OLD marker survives until our
        // own write below, so it can't distinguish "fresh winner" from
        // "stale leftover".
        let _ = std::fs::remove_dir_all(&trash);
        // The exists-then-rename pair is racy on its own (another racer
        // can move dest aside between our check and our call). Treat
        // ENOENT as "already moved aside by someone else" and continue;
        // any other error is a real I/O failure and we must clean up
        // our staging dir before propagating so we don't leak a
        // potentially-large extracted tree on disk.
        let moved_aside = match std::fs::rename(&dest, &trash) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(ProviderError::Io(e));
            }
        };
        match std::fs::rename(&staging, &dest) {
            Ok(()) => {
                // We won the swap. Write the marker BEFORE removing the
                // trash so a marker-write failure can roll back to the
                // predecessor — without this order, a disk-full or EIO
                // on marker write leaves dest unmarked AND deletes the
                // predecessor, so an offline caller loses access to a
                // rootfs that was working a moment ago.
                let marker_tmp = dest.join(format!("{READY_MARKER}.tmp"));
                let marker_res = std::fs::write(&marker_tmp, "ok\n")
                    .and_then(|_| std::fs::rename(&marker_tmp, &marker));
                match marker_res {
                    Ok(()) => {
                        // Safe to discard predecessor now.
                        let _ = std::fs::remove_dir_all(&trash);
                    }
                    Err(e) => {
                        // Marker failed — restore predecessor so the
                        // cache returns to its prior known-good state.
                        // If restore also fails we leave trash on disk
                        // for manual recovery rather than silently
                        // losing it.
                        let _ = std::fs::remove_file(&marker_tmp);
                        let _ = std::fs::remove_dir_all(&dest);
                        if moved_aside {
                            match std::fs::rename(&trash, &dest) {
                                Ok(()) => {
                                    debug!("marker write failed; restored predecessor");
                                }
                                Err(restore_err) => {
                                    warn!(
                                        error = %restore_err,
                                        trash = %trash.display(),
                                        "marker write failed AND predecessor restore failed; cache hole left at this path"
                                    );
                                }
                            }
                        }
                        return Err(ProviderError::Io(e));
                    }
                }
            }
            Err(_) if dest.exists() => {
                // Lost the race: winner's rename(staging→dest) landed
                // between our rename-aside and ours. Accept theirs;
                // discard our staging + (our) trash predecessor. The
                // winner's flow will write its own marker.
                debug!(
                    path = %dest.display(),
                    "lost concurrent swap race; accepting winner's tree"
                );
                let _ = std::fs::remove_dir_all(&staging);
                let _ = std::fs::remove_dir_all(&trash);
            }
            Err(e) => {
                // Unrecoverable rename error. Try to restore the
                // predecessor. If THAT fails, leave the trash dir on
                // disk for manual recovery rather than silently
                // destroying a previously-working cache entry.
                let _ = std::fs::remove_dir_all(&staging);
                if moved_aside {
                    match std::fs::rename(&trash, &dest) {
                        Ok(()) => {
                            debug!("restored predecessor after rename failure");
                            let _ = std::fs::remove_dir_all(&trash);
                        }
                        Err(restore_err) => {
                            warn!(
                                error = %restore_err,
                                trash = %trash.display(),
                                "rename failed AND predecessor restore failed; cache hole left at this path"
                            );
                        }
                    }
                } else {
                    let _ = std::fs::remove_dir_all(&trash);
                }
                return Err(ProviderError::Io(e));
            }
        }

        if let Some(src) = ctx.guest_helpers() {
            if let Err(e) = inject_guest_helpers(&dest, src) {
                warn!(error = %e, "guest-helper inject failed after extract");
            }
        }
        info!(path = %dest.display(), "tar rootfs ready");
        Ok(dest)
    }
}

// ---- helpers -------------------------------------------------------------

/// Stable cache directory name for a URL: a readable prefix (last ~80
/// chars of the sanitised URL, biased toward the filename) plus a
/// 16-hex hash of the full URL. The hash prevents two URLs that share
/// a long prefix from colliding when truncated.
fn cache_key(url: &str) -> String {
    // Last N rather than first N — for `https://cdn/a/b/c/release.tgz`
    // we'd rather keep `release.tgz` than `https_cdn_a_b_c`.
    const PREFIX_MAX: usize = 80;
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
    let prefix: String = sanitised
        .chars()
        .rev()
        .take(PREFIX_MAX)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    format!("{}__{:016x}", prefix, h.finish())
}

/// Open the archive source as a streaming reader. Unlike the old buffered
/// path, this never reads the whole archive into memory — the size bound is
/// enforced downstream on the *decompressed* stream during extraction (see
/// [`extract_into`]), so neither a large `file://` rootfs nor a long HTTP
/// body is buffered up front.
fn open_source(url: &str, timeout: Duration) -> Result<Box<dyn Read + Send>, Error> {
    if let Some(path) = url.strip_prefix("file://") {
        // RFC 8089: `file://localhost/abs` is equivalent to `file:///abs`.
        // Accept both by stripping a leading `localhost/` if present.
        let path = path
            .strip_prefix("localhost/")
            .map(|p| format!("/{p}"))
            .unwrap_or_else(|| path.to_string());
        let file =
            std::fs::File::open(&path).map_err(|e| Error::Download(format!("open {path}: {e}")))?;
        return Ok(Box::new(file));
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(timeout)
        .timeout_write(timeout)
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| Error::Download(e.to_string()))?;
    // ureq's reader is `Box<dyn Read + Send + Sync>`, which coerces here.
    Ok(Box::new(resp.into_reader()))
}

/// A reader that counts every byte it yields into a shared counter and fails
/// hard once the running total passes `max`. Wrapped around the *decompressed*
/// stream, it bounds the extracted rootfs size, defuses decompression bombs,
/// and — because tar stops pulling once this errors — bounds the source read.
struct CountingReader<R> {
    inner: R,
    read: Arc<AtomicU64>,
    max: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let total = self.read.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        if total > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "extracted size cap exceeded",
            ));
        }
        Ok(n)
    }
}

/// Stream `source` (optionally gzip/zstd-compressed, sniffed by magic bytes),
/// counting decompressed bytes against `max_bytes` and unpacking each safe
/// entry into `dest`. Returns an explicit cap error if the rootfs would
/// exceed `max_bytes`.
fn extract_into(source: Box<dyn Read + Send>, dest: &Path, max_bytes: u64) -> Result<(), Error> {
    // Sniff the compression magic without consuming it: BufReader::fill_buf
    // peeks, then the chosen decoder reads from the same BufReader starting
    // at those still-buffered bytes.
    let mut buffered = BufReader::with_capacity(64 * 1024, source);
    let head = buffered.fill_buf().map_err(Error::Io)?;
    let decoder: Box<dyn Read> = if head.starts_with(&GZIP_MAGIC) {
        Box::new(flate2::read::GzDecoder::new(buffered))
    } else if head.starts_with(&ZSTD_MAGIC) {
        Box::new(
            zstd::stream::read::Decoder::new(buffered)
                .map_err(|e| Error::Extract(format!("zstd: {e}")))?,
        )
    } else {
        Box::new(buffered)
    };

    let read = Arc::new(AtomicU64::new(0));
    let counted = CountingReader {
        inner: decoder,
        read: read.clone(),
        max: max_bytes,
    };
    // Map any extraction error to the cap error when the counter shows we
    // tripped the limit — the underlying io error is just the symptom.
    let cap_err = || {
        Error::Extract(format!(
            "extracted rootfs exceeds max {max_bytes} bytes (decompressed); \
             raise it via the {MAX_BYTES_ENV} env var"
        ))
    };

    let mut archive = tar::Archive::new(counted);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);

    let entries = archive.entries().map_err(|e| {
        if read.load(Ordering::Relaxed) > max_bytes {
            cap_err()
        } else {
            Error::Extract(e.to_string())
        }
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            if read.load(Ordering::Relaxed) > max_bytes {
                cap_err()
            } else {
                Error::Extract(e.to_string())
            }
        })?;
        let path_in_tar = entry
            .path()
            .map_err(|e| Error::Extract(e.to_string()))?
            .into_owned();

        // Refuse absolute paths and ..-traversal. `unpack_in` enforces
        // this too, but checking up-front lets us log + skip instead of
        // bailing on the whole archive.
        if path_in_tar.is_absolute()
            || path_in_tar
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            warn!(path = %path_in_tar.display(), "skipping unsafe path");
            continue;
        }

        if let Err(e) = entry.unpack_in(dest) {
            // A cap breach surfaces as an unpack io error; distinguish it
            // from a benign per-entry skip (e.g. an odd special file) so the
            // caller learns the real reason instead of a silently-truncated
            // rootfs.
            if read.load(Ordering::Relaxed) > max_bytes {
                return Err(cap_err());
            }
            warn!(path = %path_in_tar.display(), error = %e, "extract skipped");
        }
    }

    // Belt and suspenders: if the cap tripped on the final read but tar
    // happened not to surface it as an entry error, still fail loudly.
    if read.load(Ordering::Relaxed) > max_bytes {
        return Err(cap_err());
    }
    Ok(())
}

// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_matches_only_tar_schemes() {
        let p = TarProvider::new();
        assert!(p.matches("tar+http://example.com/a.tar"));
        assert!(p.matches("tar+https://example.com/a.tar.gz"));
        assert!(p.matches("tar+file:///tmp/a.tar"));
        assert!(!p.matches("https://example.com/a.tar"));
        assert!(!p.matches("alpine:3.20"));
        assert!(!p.matches("/abs/path"));
        assert!(!p.matches("oci://alpine"));
        assert!(!p.matches(""));
    }

    #[test]
    fn cache_key_disambiguates_urls_that_share_a_prefix() {
        // Two URLs sharing a long prefix used to collide when sanitize
        // truncated to first-N chars; the hash suffix now distinguishes.
        let a = format!("https://cdn.example.com/{}/alpine.tar.gz", "x".repeat(200));
        let b = format!("https://cdn.example.com/{}/debian.tar.gz", "x".repeat(200));
        let ka = cache_key(&a);
        let kb = cache_key(&b);
        assert_ne!(ka, kb, "different URLs must not collide");
    }

    #[test]
    fn cache_key_is_stable() {
        // Same input → same key across calls (used to invalidate caches).
        let url = "https://example.com/r.tar.gz";
        assert_eq!(cache_key(url), cache_key(url));
    }

    #[test]
    fn cache_key_caps_length() {
        let long = format!("https://example.com/{}", "x".repeat(2000));
        let k = cache_key(&long);
        // prefix(80) + "__" + hex(16) = 98 chars max
        assert!(k.len() <= 100, "cache_key too long: {} chars", k.len());
    }

    #[test]
    fn cache_key_keeps_filename_in_prefix() {
        // The readable prefix should favour the URL tail (filename)
        // over the scheme, since the scheme is rarely distinguishing.
        let k =
            cache_key("https://example.com/builds/2026/05/29/release-channel/alpine-3.20.tar.gz");
        assert!(
            k.contains("alpine-3.20.tar.gz"),
            "filename not preserved: {k}"
        );
    }

    /// Build an in-memory tar holding one file of `size` zero bytes.
    fn tar_with_file(name: &str, size: usize) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(size as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, std::io::repeat(0).take(size as u64))
            .unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, bytes).unwrap();
        enc.finish().unwrap()
    }

    /// A unique scratch dir under the system temp root; removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "vmette-tar-test-{}-{}-{}",
                tag,
                std::process::id(),
                SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // A one-file tar is 512 (header) + the data block padded to 512 + a
    // 1024-byte end-of-archive marker, so the decompressed stream the cap
    // counts is ~2 KiB even for a 16-byte file. Caps below use that headroom.

    #[test]
    fn extract_plain_tar_within_cap() {
        let tar = tar_with_file("hello.txt", 16);
        let dir = TmpDir::new("plain");
        extract_into(Box::new(std::io::Cursor::new(tar)), &dir.0, 8192).unwrap();
        assert_eq!(
            std::fs::metadata(dir.0.join("hello.txt")).unwrap().len(),
            16
        );
    }

    #[test]
    fn extract_gzip_tar_within_cap() {
        let gz = gzip(&tar_with_file("hello.txt", 16));
        let dir = TmpDir::new("gzip");
        extract_into(Box::new(std::io::Cursor::new(gz)), &dir.0, 8192).unwrap();
        assert!(dir.0.join("hello.txt").exists());
    }

    #[test]
    fn cap_is_on_extracted_not_compressed_size() {
        // A file far larger than the cap: the *decompressed* size is what's
        // bounded, so even a tiny gzip that expands past the cap must fail.
        let tar = tar_with_file("big.bin", 4096);
        let gz = gzip(&tar); // compresses to a few hundred bytes
        assert!(
            (gz.len() as u64) < 512,
            "gzip of zeros should be well under the cap"
        );
        let dir = TmpDir::new("bomb");
        let err = extract_into(Box::new(std::io::Cursor::new(gz)), &dir.0, 512).unwrap_err();
        match err {
            Error::Extract(msg) => assert!(msg.contains("exceeds max"), "got: {msg}"),
            other => panic!("expected Extract cap error, got {other:?}"),
        }
    }

    #[test]
    fn plain_tar_over_cap_fails() {
        let tar = tar_with_file("big.bin", 8192);
        let dir = TmpDir::new("overcap");
        let err = extract_into(Box::new(std::io::Cursor::new(tar)), &dir.0, 1024).unwrap_err();
        assert!(matches!(err, Error::Extract(_)));
    }
}
