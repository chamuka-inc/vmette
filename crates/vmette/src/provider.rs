//! Pluggable rootfs providers.
//!
//! A [`RootfsProvider`] resolves a string spec (e.g. `"alpine:3.20"`,
//! `"/path/to/dir"`, `"tar+https://…/rootfs.tgz"`) to a host directory
//! tree that the VM mounts as `/` via virtio-fs. The provider trait is
//! the seam third-party code uses to teach `vmette` about new rootfs
//! sources without touching the core crate.
//!
//! ## Built-in providers
//!
//! Only [`DirProvider`] ships in `vmette` itself (zero deps). OCI and
//! tarball providers live in sibling crates that the CLI registers at
//! startup:
//!
//! | crate                    | scheme              | matches                                          |
//! |--------------------------|---------------------|--------------------------------------------------|
//! | `vmette`                 | `dir`               | absolute, `./`, `../`, `~/` paths                |
//! | `vmette-provider-oci`    | `oci` (+ bare refs) | `oci://…`, otherwise any non-path, non-scheme    |
//! | `vmette-provider-tar`    | `tar`               | `tar+http://`, `tar+https://`, `tar+file://`     |
//!
//! ## Registration order
//!
//! [`Registry::resolve`] tries providers in registration order; the first
//! whose [`RootfsProvider::matches`] returns `true` handles the spec.
//! Register the most-specific (path / scheme-prefixed) providers first
//! and any catch-all (bare image refs) last.
//!
//! ## Writing a new provider
//!
//! ```no_run
//! use std::path::PathBuf;
//! use vmette::provider::{Context, ProviderError, RootfsProvider};
//!
//! struct EchoProvider;
//!
//! impl RootfsProvider for EchoProvider {
//!     fn name(&self) -> &'static str { "echo" }
//!     fn matches(&self, spec: &str) -> bool { spec.starts_with("echo://") }
//!     fn provide(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError> {
//!         let rel = spec.strip_prefix("echo://").unwrap_or(spec);
//!         let dest = ctx.provider_cache("echo")?.join(rel);
//!         std::fs::create_dir_all(&dest)?;
//!         Ok(dest)
//!     }
//! }
//! ```

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors a provider may surface from [`RootfsProvider::provide`].
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The spec is syntactically or semantically invalid for this provider.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// `Context::offline` was set and no cache entry exists for this spec.
    #[error("offline mode: {0} not in cache")]
    OfflineCacheMiss(String),

    /// Network failure (DNS, TLS, HTTP non-2xx, registry error, etc).
    #[error("network: {0}")]
    Network(String),

    /// Filesystem error during cache lookup, extraction, or write.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Anything else (registry parse error, decompression failure, etc).
    #[error("{0}")]
    Other(String),
}

/// Resolution context passed to every [`RootfsProvider::provide`] call.
///
/// Carries cache root, offline policy, and an optional pointer to the
/// directory holding `vsock-send` / `vsock-runner` binaries that the OCI
/// provider injects into pulled images. Constructed by callers via
/// [`Context::builder`].
#[derive(Debug, Clone)]
pub struct Context {
    cache_root: PathBuf,
    offline: bool,
    guest_helpers_dir: Option<PathBuf>,
}

impl Context {
    /// Start a new context with a cache root (required) and defaults.
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
            offline: false,
            guest_helpers_dir: None,
        }
    }

    /// Mutating builder-style: short-circuit network access. A cache miss
    /// becomes [`ProviderError::OfflineCacheMiss`].
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Mutating builder-style: directory containing vmette guest helpers
    /// (`vsock-send`, `vsock-runner`). Providers that materialise images
    /// from elsewhere (OCI, tarball) inject these into `/usr/local/bin`
    /// so vsock workflows work uniformly across rootfs sources.
    pub fn guest_helpers_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.guest_helpers_dir = dir;
        self
    }

    /// Idempotently create and return `cache_root/<provider_name>`. The
    /// directory is each provider's private namespace — naming collisions
    /// across providers are impossible.
    pub fn provider_cache(&self, provider_name: &str) -> Result<PathBuf, ProviderError> {
        let dir = self.cache_root.join(provider_name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// True if the caller forbids network access for this resolution.
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// Where vmette guest helpers live on the host, if known.
    pub fn guest_helpers(&self) -> Option<&Path> {
        self.guest_helpers_dir.as_deref()
    }

    /// The root under which provider caches live.
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }
}

/// A registered source of rootfs trees.
///
/// Implementations should be cheap to construct (the provider is held
/// for the lifetime of the registry) and stateless across calls (cache
/// state lives on disk under [`Context::provider_cache`]).
pub trait RootfsProvider: Send + Sync {
    /// Stable identifier, used in logs/errors and as the on-disk cache
    /// subdirectory name. Must be a path-safe ASCII slug.
    fn name(&self) -> &'static str;

    /// Does this provider claim `spec`? Should be a pure string check —
    /// no I/O. Multiple providers may match; the first registered wins
    /// (see [`Registry::resolve`]).
    fn matches(&self, spec: &str) -> bool;

    /// Resolve `spec` to a host directory that can be mounted as `/`.
    ///
    /// Must be idempotent: re-resolving the same spec should return the
    /// same path (modulo provider-internal cache invalidation policy).
    /// `ctx.guest_helpers()` should be honoured by providers that
    /// materialise the rootfs themselves (OCI, tar) — local-dir
    /// providers leave the user's directory alone.
    fn provide(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError>;
}

/// Ordered collection of providers. Dispatch is first-match-wins.
///
/// The registry holds boxed trait objects so callers can mix built-in
/// and third-party providers in a single resolution pipeline.
#[derive(Default)]
pub struct Registry {
    providers: Vec<Box<dyn RootfsProvider>>,
}

impl Registry {
    /// Construct an empty registry. Use [`Registry::with`] to chain.
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Append a provider. Order matters — earlier entries win.
    pub fn with<P: RootfsProvider + 'static>(mut self, p: P) -> Self {
        self.providers.push(Box::new(p));
        self
    }

    /// Resolve `spec` to a rootfs path via the first matching provider.
    /// Returns [`ProviderError::InvalidSpec`] if no provider claims it,
    /// listing the registered providers for diagnostics.
    pub fn resolve(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError> {
        for p in &self.providers {
            if p.matches(spec) {
                return p.provide(spec, ctx);
            }
        }
        let names: Vec<&str> = self.providers.iter().map(|p| p.name()).collect();
        Err(ProviderError::InvalidSpec(format!(
            "no provider claims {:?}; registered: [{}]",
            spec,
            names.join(", ")
        )))
    }

    /// Names of all registered providers, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// True iff no providers are registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

// ---- shared helper: guest-helper injection ------------------------------

/// Copy `vsock-send` and `vsock-runner` from `src_bin_dir` into
/// `rootfs/usr/local/bin/`, creating the target directory if missing.
///
/// Materialising providers (OCI, tarball, ...) call this after writing
/// the rootfs so vsock workflows work uniformly regardless of image
/// source. Lives in `vmette` core (rather than any one provider crate)
/// so peer providers don't have to depend on each other.
///
/// Idempotent: skips files that are already present with matching size
/// AND mtime no-older-than the source, so cache hits don't pointlessly
/// rewrite shared cache state. Size-alone would silently keep a stale
/// binary across a same-size rebuild; mtime-newer-than-src is the
/// heuristic `make(1)` uses.
///
/// Missing source binaries are logged-and-skipped, not errored, so a
/// minimal install without vsock-runner still injects vsock-send and
/// returns Ok. A real I/O failure (permission denied, disk full)
/// surfaces as `Err`.
pub fn inject_guest_helpers(rootfs: &Path, src_bin_dir: &Path) -> std::io::Result<()> {
    let target_dir = rootfs.join("usr/local/bin");
    std::fs::create_dir_all(&target_dir)?;
    for name in &["vsock-send", "vsock-runner"] {
        let src = src_bin_dir.join(name);
        if !src.exists() {
            tracing::warn!(name = name, "guest helper not found in source; skipping");
            continue;
        }
        let dst = target_dir.join(name);
        if let (Ok(s_meta), Ok(d_meta)) = (std::fs::metadata(&src), std::fs::metadata(&dst)) {
            if s_meta.len() == d_meta.len() {
                if let (Ok(s_mtime), Ok(d_mtime)) = (s_meta.modified(), d_meta.modified()) {
                    if d_mtime >= s_mtime {
                        continue;
                    }
                }
            }
        }
        std::fs::copy(&src, &dst)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// ---- built-in: DirProvider ----------------------------------------------

/// Resolves bare filesystem paths to themselves. Claims any spec that
/// begins with `/`, `./`, `../`, or `~/`. Tilde expansion is shell-style
/// (only a leading `~/` against `$HOME`; `~user` is not supported).
#[derive(Debug, Default, Clone, Copy)]
pub struct DirProvider;

impl DirProvider {
    pub fn new() -> Self {
        Self
    }
}

impl RootfsProvider for DirProvider {
    fn name(&self) -> &'static str {
        "dir"
    }

    fn matches(&self, spec: &str) -> bool {
        spec.starts_with('/')
            || spec.starts_with("./")
            || spec.starts_with("../")
            || spec.starts_with("~/")
            || spec == "."
            || spec == ".."
    }

    fn provide(&self, spec: &str, _ctx: &Context) -> Result<PathBuf, ProviderError> {
        let path = if let Some(rest) = spec.strip_prefix("~/") {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                ProviderError::InvalidSpec("$HOME not set; cannot expand '~/'".into())
            })?;
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(spec)
        };
        // Stat (not symlink_metadata) — follow symlinks for the existence
        // check so a `--rootfs /tmp/current` pointing at a real dir works.
        // We deliberately do NOT canonicalise: symlink-swap deployments
        // (the classic /var/www/current → /var/www/releases/<ts> pattern)
        // rely on the original path being preserved end-to-end so the next
        // run picks up the new target without reconfiguration.
        let meta = std::fs::metadata(&path)
            .map_err(|e| ProviderError::InvalidSpec(format!("{}: {}", path.display(), e)))?;
        if !meta.is_dir() {
            return Err(ProviderError::InvalidSpec(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        Ok(path)
    }
}

// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_matches_path_like_specs() {
        let p = DirProvider;
        assert!(p.matches("/abs/path"));
        assert!(p.matches("./rel"));
        assert!(p.matches("../up"));
        assert!(p.matches("~/home"));
        assert!(p.matches("."));
        assert!(p.matches(".."));
        assert!(!p.matches("alpine:3.20"));
        assert!(!p.matches("oci://alpine"));
        assert!(!p.matches("tar+https://example.com/a.tgz"));
        assert!(!p.matches(""));
    }

    #[test]
    fn dir_rejects_missing_paths() {
        let ctx = Context::new("/tmp/vmette-test-cache");
        let p = DirProvider;
        let err = p
            .provide("/definitely/does/not/exist/vmette", &ctx)
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidSpec(_)));
    }

    #[test]
    fn dir_rejects_files() {
        let ctx = Context::new("/tmp/vmette-test-cache");
        let p = DirProvider;
        // /etc/hosts exists on every macOS system as a regular file.
        let err = p.provide("/etc/hosts", &ctx).unwrap_err();
        match err {
            ProviderError::InvalidSpec(m) => assert!(m.contains("not a directory")),
            other => panic!("expected InvalidSpec, got {other:?}"),
        }
    }

    #[test]
    fn dir_accepts_real_directory() {
        let ctx = Context::new("/tmp/vmette-test-cache");
        let p = DirProvider;
        // /tmp exists on every macOS box. We assert the path is returned
        // VERBATIM (not canonicalised to /private/tmp) so symlink-swap
        // deploy strategies keep working.
        let resolved = p.provide("/tmp", &ctx).expect("resolve /tmp");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp"));
        assert!(resolved.is_dir());
    }

    #[test]
    fn registry_dispatches_first_match() {
        struct A;
        impl RootfsProvider for A {
            fn name(&self) -> &'static str {
                "a"
            }
            fn matches(&self, s: &str) -> bool {
                s.starts_with("a:")
            }
            fn provide(&self, _: &str, _: &Context) -> Result<PathBuf, ProviderError> {
                Ok(PathBuf::from("/a"))
            }
        }
        struct B;
        impl RootfsProvider for B {
            fn name(&self) -> &'static str {
                "b"
            }
            fn matches(&self, _: &str) -> bool {
                true
            }
            fn provide(&self, _: &str, _: &Context) -> Result<PathBuf, ProviderError> {
                Ok(PathBuf::from("/b"))
            }
        }
        let reg = Registry::new().with(A).with(B);
        let ctx = Context::new("/tmp");
        assert_eq!(reg.resolve("a:x", &ctx).unwrap(), PathBuf::from("/a"));
        assert_eq!(reg.resolve("z", &ctx).unwrap(), PathBuf::from("/b"));
        assert_eq!(reg.names(), vec!["a", "b"]);
    }

    #[test]
    fn registry_reports_missing_provider() {
        let reg = Registry::new().with(DirProvider);
        let ctx = Context::new("/tmp");
        let err = reg.resolve("alpine:3.20", &ctx).unwrap_err();
        match err {
            ProviderError::InvalidSpec(m) => {
                assert!(m.contains("alpine:3.20"));
                assert!(m.contains("dir"));
            }
            other => panic!("expected InvalidSpec, got {other:?}"),
        }
    }

    #[test]
    fn context_provider_cache_creates_subdir() {
        let tmp = std::env::temp_dir().join(format!("vmette-prov-test-{}", std::process::id()));
        let ctx = Context::new(&tmp);
        let sub = ctx.provider_cache("foo").unwrap();
        assert!(sub.is_dir());
        assert_eq!(sub, tmp.join("foo"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
