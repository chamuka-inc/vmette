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
//! use vmette::provider::{Context, ProviderError, RootfsArtifact, RootfsProvider};
//!
//! struct EchoProvider;
//!
//! impl RootfsProvider for EchoProvider {
//!     fn name(&self) -> &'static str { "echo" }
//!     fn matches(&self, spec: &str) -> bool { spec.starts_with("echo://") }
//!     fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
//!         let rel = spec.strip_prefix("echo://").unwrap_or(spec);
//!         let dest = ctx.provider_cache("echo")?.join(rel);
//!         std::fs::create_dir_all(&dest)?;
//!         Ok(RootfsArtifact::Directory { path: dest, read_only: false })
//!     }
//! }
//! ```

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Filesystem type of a block-image rootfs. Determines the `-t <fstype>`
/// the guest init passes to `mount` and the `vmette.rootfs_block=<fstype>`
/// cmdline token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFs {
    /// A read-only `squashfs` image, mounted as the overlay lower layer.
    Squashfs,
}

impl BlockFs {
    /// The kernel filesystem name (as used by `mount -t` and the cmdline).
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockFs::Squashfs => "squashfs",
        }
    }
}

impl std::fmt::Display for BlockFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a provider materialised for use as the guest root filesystem.
///
/// Most providers hand back a host [`directory`](RootfsArtifact::Directory)
/// shared into the guest over virtio-fs. A provider may instead hand back a
/// [`block image`](RootfsArtifact::BlockImage) (e.g. a `squashfs` file)
/// attached as a virtio-blk device and overlaid with a tmpfs upper for
/// writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsArtifact {
    /// A host directory tree, mounted as `/` via virtio-fs.
    Directory {
        /// Host path to share.
        path: PathBuf,
        /// Mount the share read-only inside the guest.
        read_only: bool,
    },
    /// A filesystem image file, attached as a virtio-blk device and
    /// mounted read-only as the lower layer of a tmpfs-backed overlay.
    BlockImage {
        /// Host path to the image file.
        path: PathBuf,
        /// Image filesystem type.
        fstype: BlockFs,
    },
}

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
/// [`Context::new`].
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

    /// Does this provider claim `spec`? Normally a pure string check, but a
    /// provider may stat the filesystem to disambiguate (e.g. [`DirProvider`]
    /// claims a bare-relative spec that is an existing directory, so it isn't
    /// mistaken for an image ref). Keep any such I/O cheap and infallible —
    /// errors belong in [`provide`](Self::provide). Multiple providers may
    /// match; the first registered wins (see [`Registry::resolve`]).
    fn matches(&self, spec: &str) -> bool;

    /// Resolve `spec` to a [`RootfsArtifact`] the VM can mount as `/`.
    ///
    /// Must be idempotent: re-resolving the same spec should return the
    /// same artifact (modulo provider-internal cache invalidation policy).
    /// `ctx.guest_helpers()` should be honoured by providers that
    /// materialise the rootfs themselves (OCI, tar) — local-dir
    /// providers leave the user's directory alone.
    fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError>;
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

    /// Resolve `spec` to a [`RootfsArtifact`] via the first matching
    /// provider. Returns [`ProviderError::InvalidSpec`] if no provider
    /// claims it, listing the registered providers for diagnostics.
    pub fn resolve(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
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

/// Resolves bare filesystem paths to themselves. Claims any spec that begins
/// with `/`, `./`, `../`, or `~/`, plus any bare-relative spec that resolves to
/// an existing directory (so `--rootfs assets/foo` works without a `./`).
/// Tilde expansion is shell-style (only a leading `~/` against `$HOME`;
/// `~user` is not supported).
///
/// A bare-relative existing directory shadows an OCI image of the same name —
/// e.g. a local `./node` directory wins over the Docker `node` image. Force the
/// OCI provider with an explicit scheme (`oci://node`) to disambiguate.
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
        // Path-shaped specs are claimed even if missing, so the user gets a
        // "no such file" error from `provide` rather than a confusing OCI
        // pull failure.
        if spec.starts_with('/')
            || spec.starts_with("./")
            || spec.starts_with("../")
            || spec.starts_with("~/")
            || spec == "."
            || spec == ".."
        {
            return true;
        }
        // Bare-relative specs that resolve to an existing directory (e.g.
        // `assets/alpine-rootfs`) are real local rootfs dirs, not image refs.
        // Claim them here so they don't fall through to the OCI catch-all,
        // which would treat them as a Docker repo and fail with a 401. A spec
        // that isn't an existing dir (e.g. `alpine:3.20`, `node`) is left for
        // the scheme/OCI providers. Disambiguate a dir that shadows an image
        // name with an explicit `oci://` scheme.
        std::path::Path::new(spec).is_dir()
    }

    fn provide(&self, spec: &str, _ctx: &Context) -> Result<RootfsArtifact, ProviderError> {
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
        Ok(RootfsArtifact::Directory {
            path,
            read_only: false,
        })
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
    fn dir_matches_bare_relative_existing_dir() {
        let p = DirProvider;
        // `cargo test` runs with cwd = the crate root, which has a `src/` dir.
        // A bare-relative spec that resolves to an existing directory must be
        // claimed (so it never falls through to OCI and 401s).
        assert!(p.matches("src"));
        // A bare-relative spec that isn't an existing dir is left for the
        // scheme/OCI providers (so real image refs still resolve).
        assert!(!p.matches("definitely-not-a-dir-xyz"));
        // A regular file is not a directory, so it's not claimed either.
        assert!(!p.matches("Cargo.toml"));
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
        match resolved {
            RootfsArtifact::Directory { path, read_only } => {
                assert_eq!(path, std::path::PathBuf::from("/tmp"));
                assert!(path.is_dir());
                assert!(!read_only);
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn registry_dispatches_first_match() {
        fn dir_path(a: RootfsArtifact) -> PathBuf {
            match a {
                RootfsArtifact::Directory { path, .. } => path,
                other => panic!("expected Directory, got {other:?}"),
            }
        }
        struct A;
        impl RootfsProvider for A {
            fn name(&self) -> &'static str {
                "a"
            }
            fn matches(&self, s: &str) -> bool {
                s.starts_with("a:")
            }
            fn provide(&self, _: &str, _: &Context) -> Result<RootfsArtifact, ProviderError> {
                Ok(RootfsArtifact::Directory {
                    path: PathBuf::from("/a"),
                    read_only: false,
                })
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
            fn provide(&self, _: &str, _: &Context) -> Result<RootfsArtifact, ProviderError> {
                Ok(RootfsArtifact::Directory {
                    path: PathBuf::from("/b"),
                    read_only: false,
                })
            }
        }
        let reg = Registry::new().with(A).with(B);
        let ctx = Context::new("/tmp");
        assert_eq!(
            dir_path(reg.resolve("a:x", &ctx).unwrap()),
            PathBuf::from("/a")
        );
        assert_eq!(
            dir_path(reg.resolve("z", &ctx).unwrap()),
            PathBuf::from("/b")
        );
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
