//! OCI / Docker image rootfs provider for vmette.
//!
//! Implements [`vmette::provider::RootfsProvider`] for image references.
//! Accepts:
//!
//! * `oci://<ref>` — explicit scheme, never ambiguous
//! * `<ref>`       — bare image references (e.g. `alpine:3.20`,
//!   `ghcr.io/foo/bar:tag`). The OCI provider is the catch-all
//!   fallback once path-style and other-scheme providers have declined.
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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use oci_client::{
    client::{linux_amd64_resolver, ClientConfig, ImageData},
    secrets::RegistryAuth,
    Client, Reference,
};
use thiserror::Error;
use tracing::{debug, info, warn};
use vmette::provider::{
    inject_guest_helpers, Context, ProviderError, RootfsArtifact, RootfsProvider,
};

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
            Error::InvalidReference(r, msg) => ProviderError::InvalidSpec(format!("{r}: {msg}")),
            Error::Io(io) => ProviderError::Io(io),
            other => ProviderError::Other(other.to_string()),
        }
    }
}

// ---- Authentication ------------------------------------------------------

/// Resolves registry credentials, keyed per-registry host so a token for
/// one registry is never sent to another (or leaked across a redirect).
///
/// Credential **resolution** (this trait) is deliberately separate from
/// credential **use** (the pull): a third party can inject their own
/// resolver via [`OciProvider::with_auth`] without touching pull logic.
pub trait AuthResolver: Send + Sync {
    /// Resolve auth for a specific registry host (e.g. `"ghcr.io"`,
    /// `"docker.io"`). Returning [`RegistryAuth::Anonymous`] means
    /// "no credentials" — the normal default for public images.
    fn resolve(&self, registry: &str) -> RegistryAuth;
}

/// Default credential chain, in precedence order:
///
/// 1. **Programmatic override** — a per-registry map set via
///    [`DefaultAuthResolver::with_registry`].
/// 2. **Environment** — `VMETTE_OCI_AUTH_<HOST>` (`user:secret`) for a
///    specific host, else `VMETTE_OCI_TOKEN` (+ optional `VMETTE_OCI_USER`,
///    default `"vmette"`) as `Basic(user, token)`. `Basic` — not `Bearer` —
///    because `oci_client` performs the `WWW-Authenticate: Bearer` exchange
///    that ghcr requires from a personal access token.
/// 3. **`~/.docker/config.json`** — best-effort `auths[registry].auth`
///    (base64 `user:pass`). `credsStore` / `credHelpers` are out of scope.
/// 4. **Anonymous** — unchanged default; every public pull behaves as before.
#[derive(Debug, Default, Clone)]
pub struct DefaultAuthResolver {
    overrides: std::collections::HashMap<String, RegistryAuth>,
}

impl DefaultAuthResolver {
    /// Empty resolver — relies on env / docker-config / anonymous.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin an explicit credential for `registry` (highest precedence).
    pub fn with_registry(mut self, registry: impl Into<String>, auth: RegistryAuth) -> Self {
        self.overrides.insert(registry.into(), auth);
        self
    }
}

impl AuthResolver for DefaultAuthResolver {
    fn resolve(&self, registry: &str) -> RegistryAuth {
        if let Some(a) = self.overrides.get(registry) {
            return a.clone();
        }
        if let Some(a) = env_auth(registry) {
            return a;
        }
        if let Some(a) = docker_config_auth(registry) {
            return a;
        }
        RegistryAuth::Anonymous
    }
}

/// `VMETTE_OCI_AUTH_<HOST>` (per-host `user:secret`) then `VMETTE_OCI_TOKEN`.
fn env_auth(registry: &str) -> Option<RegistryAuth> {
    let host_key = format!("VMETTE_OCI_AUTH_{}", env_host_suffix(registry));
    if let Ok(v) = std::env::var(&host_key) {
        if let Some(a) = parse_userpass(&v) {
            return Some(a);
        }
    }
    let token = std::env::var("VMETTE_OCI_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())?;
    let user = std::env::var("VMETTE_OCI_USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "vmette".to_string());
    Some(RegistryAuth::Basic(user, token))
}

/// `"user:secret"` → `Basic(user, secret)`. A bare value (no colon) is
/// treated as a token under the conventional `vmette` username.
fn parse_userpass(v: &str) -> Option<RegistryAuth> {
    if v.is_empty() {
        return None;
    }
    match v.split_once(':') {
        Some((u, p)) => Some(RegistryAuth::Basic(u.to_string(), p.to_string())),
        None => Some(RegistryAuth::Basic("vmette".to_string(), v.to_string())),
    }
}

/// Registry host → env-var suffix: uppercase, non-alphanumeric → `_`.
/// `"ghcr.io"` → `"GHCR_IO"`.
fn env_host_suffix(registry: &str) -> String {
    registry
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Best-effort `~/.docker/config.json` lookup of `auths[registry].auth`
/// (base64 `user:pass`). Returns None on any miss — missing file, missing
/// entry, `credsStore`/`credHelpers` (which we deliberately do not shell out
/// to), or a malformed entry.
fn docker_config_auth(registry: &str) -> Option<RegistryAuth> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".docker").join("config.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let auths = json.get("auths")?;
    // `docker login` (and `registry_of`) normalise Docker Hub to `docker.io`,
    // but the config file keys the credential under the legacy v1 URL. Try the
    // conventional Docker Hub aliases and take the first key that yields a
    // usable `auth` (not merely the first key that exists).
    docker_config_keys(registry)
        .into_iter()
        .find_map(|k| auths.get(k).and_then(parse_auth_entry))
}

/// Decode an `auths[<key>]` object's `auth` field (base64 `user:pass`) into a
/// [`RegistryAuth::Basic`]. None on any miss (no `auth`, bad base64, no colon).
fn parse_auth_entry(entry: &serde_json::Value) -> Option<RegistryAuth> {
    use base64::Engine;
    let b64 = entry.get("auth")?.as_str()?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let pair = String::from_utf8(decoded).ok()?;
    let (u, p) = pair.split_once(':')?;
    Some(RegistryAuth::Basic(u.to_string(), p.to_string()))
}

/// Candidate `auths` keys for a registry host, in lookup order. Docker Hub is
/// stored under `https://index.docker.io/v1/` (and historically
/// `index.docker.io`) rather than the normalised `docker.io` that
/// `registry_of` yields.
fn docker_config_keys(registry: &str) -> Vec<&str> {
    if registry == "docker.io" {
        vec![
            "docker.io",
            "https://index.docker.io/v1/",
            "index.docker.io",
        ]
    } else {
        vec![registry]
    }
}

/// The registry host an image ref resolves to (for keying auth). Falls back
/// to an empty string if the ref doesn't parse; the pull then surfaces the
/// real parse error.
fn registry_of(image_ref: &str) -> String {
    image_ref
        .parse::<Reference>()
        .map(|r| r.registry().to_string())
        .unwrap_or_default()
}

// ---- Provider impl -------------------------------------------------------

/// OCI / Docker image provider. Wraps [`pull_with_options`] behind the
/// vmette provider trait. Construct with [`OciProvider::new`] or
/// [`OciProvider::with_options`] for non-default cache TTL.
pub struct OciProvider {
    options: PullOptions,
    auth: Arc<dyn AuthResolver>,
}

impl Default for OciProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OciProvider {
    /// Construct with [`PullOptions::default`] (1-hour cache TTL, online)
    /// and the [`DefaultAuthResolver`] credential chain.
    /// `Context::is_offline()` still overrides `options.offline` on each
    /// call, so a single provider instance serves both online and offline
    /// resolutions.
    pub fn new() -> Self {
        Self {
            options: PullOptions::default(),
            auth: Arc::new(DefaultAuthResolver::new()),
        }
    }

    /// Construct with custom pull options. `options.offline` is treated
    /// as the floor — `Context::is_offline()` can force it on for a
    /// single call but cannot turn it off.
    pub fn with_options(options: PullOptions) -> Self {
        Self {
            options,
            auth: Arc::new(DefaultAuthResolver::new()),
        }
    }

    /// Replace the credential resolver (highest-precedence source). Lets the
    /// daemon / MCP inject credentials with no environment at all.
    pub fn with_auth(mut self, auth: Arc<dyn AuthResolver>) -> Self {
        self.auth = auth;
        self
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

    fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
        let image_ref = spec.strip_prefix("oci://").unwrap_or(spec);
        if image_ref.is_empty() {
            return Err(ProviderError::InvalidSpec("empty image reference".into()));
        }
        let cache = ctx.provider_cache(self.name())?;

        let opts = PullOptions {
            cache_ttl: self.options.cache_ttl,
            offline: self.options.offline || ctx.is_offline(),
        };

        // Note: the tokio runtime is built lazily INSIDE pull_with_options
        // — only when a network roundtrip is actually needed. Cache hits
        // resolve via blocking fs::metadata and pay no runtime cost.
        let auth = self.auth.resolve(&registry_of(image_ref));
        let rootfs = pull_with_options_sync(image_ref, &cache, &opts, &auth)
            .map_err(|e| map_oci_error(spec, image_ref, e))?;

        if let Some(src) = ctx.guest_helpers() {
            if let Err(e) = inject_guest_helpers(&rootfs, src) {
                warn!(error = %e, "guest-helper inject failed; vsock workflows may not work in this image");
            }
        }
        Ok(RootfsArtifact::Directory {
            path: rootfs,
            read_only: false,
        })
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
            && std::fs::metadata(image_ref)
                .map(|m| m.is_dir())
                .unwrap_or(false);
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
    auth: &RegistryAuth,
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
        .map_err(|e| Error::Io(std::io::Error::other(format!("tokio init: {e}"))))?;
    rt.block_on(pull_with_options(image_ref, cache_root, options, auth))
}

/// Blocking-fs equivalent of pull_with_options's cache-hit branch.
/// Returns Some(rootfs) iff a within-TTL (or offline) ready marker is
/// present on disk; otherwise None (caller must consult the network
/// or the offline-fallback scanner).
fn fast_path_lookup(cache_root: &Path, image_ref: &str, options: &PullOptions) -> Option<PathBuf> {
    let ref_file = cache_root
        .join("refs")
        .join(format!("{}.digest", sanitize_ref(image_ref)));
    let (digest, age) = read_ref_entry(&ref_file)?;
    let rootfs = extracted_path(cache_root, image_ref, &digest);
    let marker = rootfs.join(".vmette-image-ready");
    if !marker.exists() {
        return None;
    }
    let fresh_enough = options.offline || options.cache_ttl.map(|ttl| age <= ttl).unwrap_or(false);
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
    auth: &RegistryAuth,
) -> Result<PathBuf, Error> {
    // Cache fast-path is identical to the sync wrapper's probe — share
    // it via `fast_path_lookup` so the TTL / offline-precedence rules
    // live in one place. The sync wrapper also calls this; the second
    // call here is a no-op (returns None immediately) when the sync
    // wrapper already filtered out the easy case.
    if let Some(rootfs) = fast_path_lookup(cache_root, image_ref, options) {
        return Ok(rootfs);
    }

    let reference: Reference = image_ref.parse().map_err(|e: oci_client::ParseError| {
        Error::InvalidReference(image_ref.into(), e.to_string())
    })?;

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

    // Resolve manifest digest cheaply — single HEAD/GET, no blob downloads.
    let manifest_digest = client.fetch_manifest_digest(&reference, auth).await?;

    let rootfs = extracted_path(cache_root, image_ref, &manifest_digest);
    let ready_marker = rootfs.join(".vmette-image-ready");

    if ready_marker.exists() {
        debug!(path = %rootfs.display(), "image already extracted; refreshing ref entry");
        write_ref_entry(&ref_file, &manifest_digest)?;
        return Ok(rootfs);
    }

    info!(digest = %manifest_digest, "cache miss; pulling layers");

    let image: ImageData = client.pull(&reference, auth, MEDIA_TYPES.to_vec()).await?;

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

    // Surface the image's configured environment (PATH, etc.) to the guest so
    // a `docker`-style toolchain image works without the caller re-deriving
    // PATH — the guest `/init` sources this before running the exec.
    write_image_env(&rootfs, &image.config.data);

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

/// Write the image config's `Env` entries into the extracted rootfs as a
/// shell-sourceable `/.vmette-image-env` (one `export KEY='VALUE'` per line).
/// The guest `/init` sources it before the exec, so an image's configured
/// `PATH` (cargo, node, …) and other env are in scope — matching how
/// `docker run` applies the image's env. Best-effort: a malformed config or a
/// write failure is silently ignored (the env is a convenience, not required).
fn write_image_env(rootfs: &Path, config_blob: &[u8]) {
    if let Some(out) = render_image_env(config_blob) {
        let _ = std::fs::write(rootfs.join(".vmette-image-env"), out);
    }
}

/// Render an OCI image config's `Env` array into shell `export KEY='VALUE'`
/// lines. Returns `None` when the blob is unparseable or carries no usable
/// entries. Keys must be shell identifiers (`[A-Za-z0-9_]`); values are
/// single-quoted with embedded quotes escaped, so metacharacters are inert
/// when the guest sources the file. Pure — unit-tested.
fn render_image_env(config_blob: &[u8]) -> Option<String> {
    let cfg = serde_json::from_slice::<serde_json::Value>(config_blob).ok()?;
    let env = cfg
        .get("config")
        .and_then(|c| c.get("Env"))
        .and_then(|e| e.as_array())?;
    let mut out = String::new();
    for entry in env {
        let Some((key, val)) = entry.as_str().and_then(|kv| kv.split_once('=')) else {
            continue;
        };
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let escaped = val.replace('\'', "'\\''");
        out.push_str("export ");
        out.push_str(key);
        out.push_str("='");
        out.push_str(&escaped);
        out.push_str("'\n");
    }
    (!out.is_empty()).then_some(out)
}

fn extracted_path(cache_root: &Path, image_ref: &str, digest: &str) -> PathBuf {
    cache_root
        .join(format!(
            "{}__{}",
            sanitize_ref(image_ref),
            digest_to_dir(digest)
        ))
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
        let Ok(meta) = std::fs::metadata(&marker) else {
            continue;
        };
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
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
        format!(
            "{}{}",
            &cleaned[..prefix_end],
            &cleaned[prefix_end..prefix_end + 16]
        )
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

    for entry in archive
        .entries()
        .map_err(|e| Error::Extract(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| Error::Extract(e.to_string()))?;
        let path_in_tar = entry
            .path()
            .map_err(|e| Error::Extract(e.to_string()))?
            .into_owned();

        // Skip absolute paths / path traversal — tar entries should be
        // relative; oci-client images shouldn't include traversals.
        if path_in_tar.is_absolute()
            || path_in_tar
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
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
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
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
    fn image_env_renders_exports_and_escapes() {
        let blob = br#"{"config":{"Env":["PATH=/usr/local/cargo/bin:/bin","RUST_VERSION=1.96.0","BAD KEY=x","WEIRD=it's"]}}"#;
        let out = render_image_env(blob).expect("some env");
        assert!(out.contains("export PATH='/usr/local/cargo/bin:/bin'\n"));
        assert!(out.contains("export RUST_VERSION='1.96.0'\n"));
        // Non-identifier key is dropped.
        assert!(!out.contains("BAD KEY"));
        // Single quotes in the value are escaped so sourcing stays inert.
        assert!(out.contains(r"export WEIRD='it'\''s'"));
    }

    #[test]
    fn image_env_none_when_absent_or_unparseable() {
        assert!(render_image_env(b"{}").is_none());
        assert!(render_image_env(br#"{"config":{}}"#).is_none());
        assert!(render_image_env(br#"{"config":{"Env":[]}}"#).is_none());
        assert!(render_image_env(b"not json at all").is_none());
    }

    #[test]
    fn digest_to_dir_strips_colon_and_truncates() {
        let d = digest_to_dir(
            "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
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
    fn env_host_suffix_uppercases_and_sanitizes() {
        assert_eq!(env_host_suffix("ghcr.io"), "GHCR_IO");
        assert_eq!(env_host_suffix("docker.io"), "DOCKER_IO");
        assert_eq!(
            env_host_suffix("registry-1.example.com"),
            "REGISTRY_1_EXAMPLE_COM"
        );
    }

    #[test]
    fn parse_userpass_splits_on_first_colon() {
        match parse_userpass("alice:s3cr:et") {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "alice");
                assert_eq!(p, "s3cr:et");
            }
            other => panic!("expected Basic, got {other:?}"),
        }
        match parse_userpass("just-a-token") {
            Some(RegistryAuth::Basic(u, p)) => {
                assert_eq!(u, "vmette");
                assert_eq!(p, "just-a-token");
            }
            other => panic!("expected Basic, got {other:?}"),
        }
        assert!(parse_userpass("").is_none());
    }

    #[test]
    fn default_resolver_override_takes_precedence() {
        let r = DefaultAuthResolver::new()
            .with_registry("ghcr.io", RegistryAuth::Basic("u".into(), "p".into()));
        match r.resolve("ghcr.io") {
            RegistryAuth::Basic(u, p) => {
                assert_eq!(u, "u");
                assert_eq!(p, "p");
            }
            other => panic!("expected Basic override, got {other:?}"),
        }
        // A registry with no override and (assuming a clean env) no creds
        // falls through to Anonymous.
        if std::env::var_os("VMETTE_OCI_TOKEN").is_none()
            && std::env::var_os("VMETTE_OCI_AUTH_DOCKER_IO").is_none()
            && docker_config_auth("docker.io").is_none()
        {
            assert!(matches!(r.resolve("docker.io"), RegistryAuth::Anonymous));
        }
    }

    #[test]
    fn registry_of_extracts_host() {
        assert_eq!(registry_of("ghcr.io/foo/bar:tag"), "ghcr.io");
        // Bare refs normalise to Docker Hub.
        assert_eq!(registry_of("alpine:3.20"), "docker.io");
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
