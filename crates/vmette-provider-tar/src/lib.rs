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
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use thiserror::Error;
use tracing::{debug, info, warn};
use vmette::provider::{
    inject_guest_helpers, Context, ProviderError, RootfsArtifact, RootfsProvider,
};

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

/// Default cap on the *total* size of all extracted rootfs trees in the tar
/// cache. The cache key is the URL, so iterating on a `tar+file://` rootfs
/// under changing names (`rootfs.tar`, `rootfs-new.tar.gz`, …) would otherwise
/// accumulate a fresh multi-hundred-MB extraction per name, unbounded. After a
/// fresh extraction the cache is pruned to this cap by evicting least-recently
/// used trees. 8 GiB keeps a healthy working set of images while bounding
/// growth. Override with `VMETTE_TAR_CACHE_MAX_BYTES`.
const DEFAULT_CACHE_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Env var to override [`DEFAULT_CACHE_MAX_BYTES`].
const CACHE_MAX_BYTES_ENV: &str = "VMETTE_TAR_CACHE_MAX_BYTES";
/// Orphaned `*.staging.*` / `*.trash.*` intermediates younger than this are
/// left alone (a concurrent extraction may still own them); older ones are
/// swept as abandoned.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);
/// A ready tree touched more recently than this is never evicted, even over the
/// cap — it may be in active use by a concurrent boot.
const EVICT_MIN_AGE: Duration = Duration::from_secs(60);

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
    /// Cap on the total size of all extracted trees in the tar cache. After a
    /// fresh extraction the cache is pruned to this by evicting least-recently
    /// used trees (and sweeping abandoned staging/trash intermediates). `0`
    /// disables pruning. Default: [`DEFAULT_CACHE_MAX_BYTES`] (8 GiB),
    /// overridable via [`CACHE_MAX_BYTES_ENV`].
    pub cache_max_bytes: u64,
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
        // 0 is a valid override here: it disables cache pruning.
        let cache_max_bytes = std::env::var(CACHE_MAX_BYTES_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CACHE_MAX_BYTES);
        Self {
            timeout: Duration::from_secs(300),
            max_bytes,
            cache_ttl: Some(Duration::from_secs(3600)),
            cache_max_bytes,
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

    fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
        // `matches` already guarantees one of the prefixes, but be explicit
        // so the parser stays valid if `matches` is ever refactored.
        let url = spec
            .strip_prefix("tar+")
            .ok_or_else(|| ProviderError::InvalidSpec(format!("not a tar+ spec: {spec}")))?;

        let cache = ctx.provider_cache(self.name())?;
        let dest = cache.join(cache_key(url));
        let marker = dest.join(READY_MARKER);

        // Cache-hit fast path: marker present AND (offline OR within TTL AND
        // the source hasn't been rebuilt under us).
        if marker.exists() {
            let age = marker
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .unwrap_or(Duration::ZERO);
            let within_ttl = self.cache_ttl.map(|ttl| age <= ttl).unwrap_or(false);
            // The cache key is the URL, not the content, so a `tar+file://`
            // archive rebuilt in place under the same path would otherwise be
            // masked by the prior extraction until the TTL lapses. Treat the
            // cache as stale when the local source is newer than the marker, so
            // a local rebuild is picked up immediately. Offline always pins to
            // cache (better-stale-than-failed), matching the http path.
            let source_changed = source_newer_than(url, &marker);
            let fresh_enough = ctx.is_offline() || (within_ttl && !source_changed);
            if fresh_enough {
                debug!(path = %dest.display(), age_s = age.as_secs(), "tar cache hit");
                // Mark this tree as recently used so cache pruning evicts it
                // last (LRU), not on its original extraction time.
                touch(&marker);
                if let Some(src) = ctx.guest_helpers() {
                    if let Err(e) = inject_guest_helpers(&dest, src) {
                        warn!(error = %e, "guest-helper inject failed on cache hit");
                    }
                }
                return Ok(RootfsArtifact::Directory {
                    path: dest,
                    read_only: false,
                });
            }
            debug!(
                path = %dest.display(),
                age_s = age.as_secs(),
                source_changed,
                "tar cache stale; refetching"
            );
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
        // Prune the cache now that we've added a tree — this is the only path
        // that grows it, so it's the right place to bound it. Best-effort: a
        // prune failure must never fail an otherwise-successful resolve.
        if self.cache_max_bytes > 0 {
            prune_cache(&cache, &dest, self.cache_max_bytes);
        }
        Ok(RootfsArtifact::Directory {
            path: dest,
            read_only: false,
        })
    }
}

// ---- helpers -------------------------------------------------------------

/// Set a file's modified-time to now (best-effort). Marks a cache tree as
/// recently used on a hit so LRU pruning evicts it last. `futimens` works on a
/// read-only handle, so this needs no write permission on the marker.
fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Remove a directory tree, first making every directory under it
/// owner-writable so trees that contain non-writable dirs can still be deleted.
/// An extracted distro rootfs routinely has dirs like a `0555 /etc`; plain
/// `remove_dir_all` fails on those with `EACCES` because it can't unlink the
/// children of a directory it has no write permission on (the mode is on the
/// dir, not the file). We only reach the chmod path on first-attempt failure,
/// so the common case stays a single syscall-batch.
fn force_remove_dir_all(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) => {
            chmod_dirs_writable(path);
            std::fs::remove_dir_all(path)
        }
    }
}

/// Recursively add owner `rwx` to every directory under `path` (so its entries
/// can be unlinked). Files are left alone — unlink needs write on the *parent*,
/// not the file. Symlinks are never followed. Best-effort.
fn chmod_dirs_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return; // a file or symlink: nothing to traverse, no chmod needed
    }
    let mode = meta.permissions().mode();
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700));
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            chmod_dirs_writable(&e.path());
        }
    }
}

/// Total bytes of all regular files under `dir` (recursive, symlinks counted as
/// links, not followed). Best-effort: unreadable entries are skipped.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(_) => {
                    if let Ok(md) = entry.metadata() {
                        total = total.saturating_add(md.len());
                    }
                }
                Err(_) => {}
            }
        }
    }
    total
}

/// Bound the total size of extracted trees in `cache` to `cap` bytes by evicting
/// least-recently-used trees, and sweep abandoned `*.staging.*` / `*.trash.*`
/// intermediates from interrupted extractions. The just-resolved tree (`keep`)
/// is never evicted, nor is any tree touched within [`EVICT_MIN_AGE`] (it may be
/// in active use by a concurrent boot). Best-effort; runs only on the
/// extraction path, so the directory walk is amortised against a fetch.
fn prune_cache(cache: &Path, keep: &Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(cache) else {
        return;
    };
    let now = SystemTime::now();
    let mut ready: Vec<(std::path::PathBuf, u64, SystemTime)> = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(".staging.") || name.contains(".trash.") {
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .unwrap_or(Duration::ZERO);
            if age >= ORPHAN_GRACE {
                debug!(path = %path.display(), "sweeping abandoned tar intermediate");
                let _ = force_remove_dir_all(&path);
            }
            continue;
        }
        // Only count complete trees (those carrying the ready marker); leave
        // anything else untouched.
        if !path.join(READY_MARKER).exists() {
            continue;
        }
        let used = path
            .join(READY_MARKER)
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(now);
        ready.push((path.clone(), dir_size(&path), used));
    }

    let mut total: u64 = ready.iter().map(|(_, sz, _)| *sz).sum();
    if total <= cap {
        return;
    }
    ready.sort_by_key(|(_, _, used)| *used); // least-recently-used first
    for (path, size, used) in ready {
        if total <= cap {
            break;
        }
        if path == keep {
            continue;
        }
        let recent = now
            .duration_since(used)
            .map(|age| age < EVICT_MIN_AGE)
            .unwrap_or(false);
        if recent {
            continue;
        }
        debug!(path = %path.display(), size, "evicting LRU tar cache tree");
        if force_remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// The local filesystem path a `tar+file://` URL refers to, or `None` for a
/// non-file URL. RFC 8089: `file://localhost/abs` is equivalent to
/// `file:///abs`, so a leading `localhost/` is stripped.
fn file_url_path(url: &str) -> Option<String> {
    let path = url.strip_prefix("file://")?;
    Some(
        path.strip_prefix("localhost/")
            .map(|p| format!("/{p}"))
            .unwrap_or_else(|| path.to_string()),
    )
}

/// True only for a `file://` URL whose source archive is strictly newer than
/// the cached extraction's ready-marker — i.e. the local tarball was rebuilt in
/// place and the cache must be re-extracted. `false` for http(s) URLs (no local
/// file to compare; the TTL governs those) and whenever either mtime is
/// unreadable (degrade to trusting the cache rather than thrashing it).
fn source_newer_than(url: &str, marker: &Path) -> bool {
    let Some(path) = file_url_path(url) else {
        return false;
    };
    let src = std::fs::metadata(&path).and_then(|m| m.modified());
    let mark = std::fs::metadata(marker).and_then(|m| m.modified());
    matches!((src, mark), (Ok(s), Ok(m)) if s > m)
}

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
    if let Some(path) = file_url_path(url) {
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
    use std::path::PathBuf;

    fn unique_cache(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "vmette-tar-prune-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Create a ready tree `cache/<name>/` with a `data` payload of `bytes` and
    /// a ready marker whose mtime is `age` in the past (drives LRU ordering).
    fn mk_tree(cache: &Path, name: &str, bytes: usize, age: Duration) {
        let dir = cache.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data"), vec![0u8; bytes]).unwrap();
        let marker = dir.join(READY_MARKER);
        std::fs::write(&marker, "ok\n").unwrap();
        std::fs::File::open(&marker)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let c = unique_cache("size");
        std::fs::create_dir_all(c.join("a/b")).unwrap();
        std::fs::write(c.join("a/x"), vec![0u8; 100]).unwrap();
        std::fs::write(c.join("a/b/y"), vec![0u8; 250]).unwrap();
        assert_eq!(dir_size(&c.join("a")), 350);
        std::fs::remove_dir_all(&c).ok();
    }

    #[test]
    fn prune_evicts_least_recently_used_over_cap() {
        let c = unique_cache("lru");
        // Three trees ~1000B each; keep is recent, the others are old enough to
        // be evictable (past EVICT_MIN_AGE).
        mk_tree(&c, "keep", 1000, Duration::from_secs(1));
        mk_tree(&c, "old1", 1000, Duration::from_secs(600));
        mk_tree(&c, "old2", 1000, Duration::from_secs(300));
        let keep = c.join("keep");
        // cap fits ~2 trees; the LRU one (old1) must go, old2 + keep survive.
        prune_cache(&c, &keep, 2200);
        assert!(keep.exists(), "just-resolved tree must survive");
        assert!(c.join("old2").exists(), "newer tree must survive");
        assert!(!c.join("old1").exists(), "LRU tree must be evicted");
        std::fs::remove_dir_all(&c).ok();
    }

    #[test]
    fn prune_never_evicts_keep_or_recent_even_over_cap() {
        let c = unique_cache("protect");
        mk_tree(&c, "keep", 1000, Duration::from_secs(1));
        mk_tree(&c, "fresh", 1000, Duration::from_secs(5)); // within EVICT_MIN_AGE
        let keep = c.join("keep");
        // Cap is below either tree, but both are protected (keep / recent).
        prune_cache(&c, &keep, 10);
        assert!(keep.exists());
        assert!(c.join("fresh").exists());
        std::fs::remove_dir_all(&c).ok();
    }

    #[test]
    fn force_remove_handles_non_writable_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let c = unique_cache("force-rm");
        // Mimic an extracted distro rootfs: a file inside a 0555 (no-write) dir,
        // which plain remove_dir_all cannot delete (can't unlink under it).
        let etc = c.join("tree/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("passwd.dpkg-new"), b"x").unwrap();
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Plain removal fails…
        assert!(std::fs::remove_dir_all(c.join("tree")).is_err());
        // …force removal succeeds by making dirs writable first.
        force_remove_dir_all(&c.join("tree")).expect("force remove");
        assert!(!c.join("tree").exists());
        std::fs::remove_dir_all(&c).ok();
    }

    #[test]
    fn prune_under_cap_is_a_noop() {
        let c = unique_cache("noop");
        mk_tree(&c, "a", 1000, Duration::from_secs(600));
        let keep = c.join("a");
        prune_cache(&c, &keep, 1024 * 1024);
        assert!(c.join("a").exists());
        std::fs::remove_dir_all(&c).ok();
    }

    #[test]
    fn prune_sweeps_old_orphan_intermediates_only() {
        let c = unique_cache("orphan");
        // An abandoned trash dir older than the grace period → swept.
        let old_trash = c.join("img.tar__abc.trash.111.222");
        std::fs::create_dir_all(&old_trash).unwrap();
        std::fs::File::open(&old_trash)
            .unwrap()
            .set_modified(SystemTime::now() - ORPHAN_GRACE - Duration::from_secs(60))
            .unwrap();
        // A fresh staging dir (a concurrent extraction may own it) → kept.
        let fresh_staging = c.join("img.tar__abc.staging.333.444");
        std::fs::create_dir_all(&fresh_staging).unwrap();
        mk_tree(&c, "keep", 100, Duration::from_secs(1));
        prune_cache(&c, &c.join("keep"), 1024 * 1024);
        assert!(!old_trash.exists(), "stale orphan must be swept");
        assert!(fresh_staging.exists(), "fresh intermediate must be kept");
        std::fs::remove_dir_all(&c).ok();
    }

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
    fn file_url_path_handles_localhost_form() {
        assert_eq!(
            file_url_path("file:///tmp/a.tar").as_deref(),
            Some("/tmp/a.tar")
        );
        assert_eq!(
            file_url_path("file://localhost/tmp/a.tar").as_deref(),
            Some("/tmp/a.tar")
        );
        assert_eq!(file_url_path("https://example.com/a.tar"), None);
    }

    #[test]
    fn source_newer_than_invalidates_on_in_place_rebuild() {
        let dir = TmpDir::new("source-newer");
        let tar = dir.0.join("rootfs.tar");
        let marker = dir.0.join(READY_MARKER);
        let url = format!("file://{}", tar.display());

        // Marker first, then (later) the source: a rebuilt-in-place archive is
        // newer than the cached extraction → must invalidate.
        std::fs::write(&marker, "ok\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&tar, b"new-content").unwrap();
        assert!(
            source_newer_than(&url, &marker),
            "a source newer than the marker must invalidate the cache"
        );

        // Re-touch the marker after the source: cache is now up to date.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&marker, "ok\n").unwrap();
        assert!(
            !source_newer_than(&url, &marker),
            "a marker newer than the source must keep the cache"
        );

        // A missing source file degrades to trusting the cache (no thrash).
        std::fs::remove_file(&tar).unwrap();
        assert!(!source_newer_than(&url, &marker));

        // http(s) URLs never compare against a local file.
        assert!(!source_newer_than("https://example.com/r.tar.gz", &marker));
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
