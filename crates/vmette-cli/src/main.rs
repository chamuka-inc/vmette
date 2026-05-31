//! vmette CLI — thin wrapper over the `vmette` library.
//!
//! Hand-rolled arg parsing (no clap, to keep the binary small and the
//! dep tree shallow). The rootfs source is selected via a single
//! `--rootfs SPEC` flag dispatched to a [`Registry`] of providers; see
//! `vmette providers` for the active list.

mod desktop;

use std::path::PathBuf;
use std::process::ExitCode;

use vmette::provider::{Context, Registry};
use vmette::{Config, RootfsArtifact, ShareMount, VsockPort};
use vmette_providers::default_registry;

fn usage() -> ! {
    eprintln!(
        "vmette --rootfs SPEC [--kernel PATH] [--initramfs PATH] [options]\n\
         vmette providers                                  # list registered providers\n\
         vmette desktop <command> [options]                # desktop computer use (via vmetted)\n\
         \n\
         required:\n\
           --rootfs           SPEC      see `vmette providers` for valid forms\n\
         \n\
         boot assets (auto-discovered from $VMETTE_ASSETS_DIR, ./assets, or the install prefix):\n\
           --kernel           PATH      bzImage on x86_64 (default: discovered vmlinuz-virt)\n\
           --initramfs        PATH      built by scripts/build-initramfs.sh (default: discovered initramfs-vmette)\n\
         \n\
         rootfs:\n\
           --rootfs-ro                  mount the rootfs share read-only\n\
           --offline                    forbid network access; resolve from cache only\n\
         \n\
         workload:\n\
           --share            TAG=PATH  extra virtio-fs mount at /mnt/<TAG> (repeatable)\n\
           --disk             PATH      raw block image as virtio-blk (repeatable)\n\
           --exec             CMD       shell command to run in guest, then poweroff\n\
           --net                        attach virtio-net with NAT; /init runs udhcpc on eth0\n\
           --switch-root                use switch_root instead of chroot for the exec env\n\
         \n\
         runtime:\n\
           --timeout          N         force-stop the VM after N seconds, exit 124\n\
           --cmdline          STR       override base kernel cmdline (default 'console=hvc0 quiet')\n\
           --vsock-port       N         -1=disable; 0=auto-pick 50000-59999 (default); >0=explicit\n\
           --vcpus            N         default 1\n\
           --mem-mib          N         default 512\n\
           --quiet                      suppress the launcher banner + status lines on stderr\n\
         \n\
         snapshot (Apple Silicon only):\n\
           --build-snapshot   PATH      boot, wait for guest READY, pause, save\n\
           --resume-snapshot  PATH      restore, send --exec via vsock, drain output\n\
           --guest-vsock-port N         guest vsock-runner listens here (default 1025)\n\
         \n\
         rootfs spec examples:\n\
           --rootfs /path/to/dir                 local directory\n\
           --rootfs alpine:3.20                  OCI image (anonymous registry)\n\
           --rootfs oci://ghcr.io/foo/bar:v1     OCI image, explicit scheme\n\
           --rootfs squashfs+https://h/img.sqfs  prebuilt squashfs (block rootfs)\n\
           --rootfs squashfs+file:///img.sqfs    local squashfs image\n\
           --rootfs tar+https://h/r.tar.gz       tarball download (gzip/zstd auto-detected)\n\
           --rootfs tar+file:///tmp/r.tar        local tarball\n"
    );
    std::process::exit(2);
}

struct ParsedArgs {
    config: Config,
    rootfs_spec: String,
    rootfs_ro: bool,
    offline: bool,
}

/// True for values that look like another CLI flag rather than the
/// expected value: `--anything`, or `-x` where x is a non-digit
/// (so `-1`, `-12` are still allowed as negative-number values).
fn looks_like_flag(v: &str) -> bool {
    if v.starts_with("--") {
        return true;
    }
    if let Some(rest) = v.strip_prefix('-') {
        if let Some(c) = rest.chars().next() {
            return !c.is_ascii_digit();
        }
    }
    false
}

fn parse_args() -> ParsedArgs {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut kernel: Option<PathBuf> = None;
    let mut initramfs: Option<PathBuf> = None;
    let mut cfg_cmdline: Option<String> = None;
    let mut rootfs_spec: Option<String> = None;
    let mut rootfs_ro = false;
    let mut offline = false;
    let mut shares: Vec<ShareMount> = Vec::new();
    let mut disks: Vec<PathBuf> = Vec::new();
    let mut exec_cmd: Option<String> = None;
    let mut switch_root = false;
    let mut net = false;
    let mut quiet = false;
    let mut vsock_port = VsockPort::Auto;
    let mut guest_vsock_port: u32 = 1025;
    let mut timeout_seconds: Option<u32> = None;
    let mut vcpus: u8 = 1;
    let mut mem_mib: u64 = 512;
    let mut build_snapshot: Option<PathBuf> = None;
    let mut resume_snapshot: Option<PathBuf> = None;

    // Consumes the token after `--flag`, refusing values that look like
    // a forgotten value (next token is itself a `--flag` or `-x`).
    // `--exec` and `--cmdline` opt out (`allow_dash_prefix = true`)
    // since shell commands and kernel cmdlines can legitimately
    // contain leading `-`. `-1`, `-2` etc. are allowed even when not
    // opted in (negative numbers).
    let take = |i: usize, flag: &str, allow_dash_prefix: bool| -> String {
        if i + 1 >= raw.len() {
            eprintln!("error: {flag} needs a value");
            usage();
        }
        let v = &raw[i + 1];
        if !allow_dash_prefix && looks_like_flag(v) {
            eprintln!(
                "error: {flag} expects a value but got '{v}' (looks like another flag). \
                 If you meant the literal string '{v}', this flag does not currently accept it."
            );
            usage();
        }
        v.clone()
    };
    // Strict numeric parse: bad input is a usage error, not a silent
    // fallback to the default (which would mask user typos).
    fn parse_num<T: std::str::FromStr>(flag: &str, v: &str) -> T {
        v.parse().unwrap_or_else(|_| {
            eprintln!("error: {flag} expects a number, got '{v}'");
            usage();
        })
    }

    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        match arg.as_str() {
            "--kernel" => {
                kernel = Some(take(i, "--kernel", false).into());
                i += 2;
            }
            "--initramfs" => {
                initramfs = Some(take(i, "--initramfs", false).into());
                i += 2;
            }
            "--cmdline" => {
                cfg_cmdline = Some(take(i, "--cmdline", true));
                i += 2;
            }
            "--rootfs" => {
                rootfs_spec = Some(take(i, "--rootfs", false));
                i += 2;
            }
            "--rootfs-ro" => {
                rootfs_ro = true;
                i += 1;
            }
            "--offline" => {
                offline = true;
                i += 1;
            }
            "--share" => {
                let s = take(i, "--share", false);
                let (tag, path) = s.split_once('=').unwrap_or_else(|| {
                    eprintln!("error: --share expects TAG=PATH, got '{}'", s);
                    usage();
                });
                shares.push(ShareMount {
                    tag: tag.into(),
                    path: path.into(),
                });
                i += 2;
            }
            "--disk" => {
                disks.push(take(i, "--disk", false).into());
                i += 2;
            }
            // --exec is a shell command; leading `-` is plausible.
            "--exec" => {
                exec_cmd = Some(take(i, "--exec", true));
                i += 2;
            }
            "--net" => {
                net = true;
                i += 1;
            }
            "--switch-root" => {
                switch_root = true;
                i += 1;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            "--timeout" => {
                let v = take(i, "--timeout", false);
                timeout_seconds = Some(parse_num::<u32>("--timeout", &v));
                i += 2;
            }
            "--vsock-port" => {
                let v = take(i, "--vsock-port", false);
                let n: i64 = parse_num::<i64>("--vsock-port", &v);
                vsock_port = match n {
                    n if n < 0 => VsockPort::Disabled,
                    0 => VsockPort::Auto,
                    n => VsockPort::Fixed(n as u32),
                };
                i += 2;
            }
            "--guest-vsock-port" => {
                let v = take(i, "--guest-vsock-port", false);
                guest_vsock_port = parse_num::<u32>("--guest-vsock-port", &v);
                i += 2;
            }
            "--vcpus" => {
                let v = take(i, "--vcpus", false);
                vcpus = parse_num::<u8>("--vcpus", &v);
                i += 2;
            }
            "--mem-mib" => {
                let v = take(i, "--mem-mib", false);
                mem_mib = parse_num::<u64>("--mem-mib", &v);
                i += 2;
            }
            "--build-snapshot" => {
                build_snapshot = Some(take(i, "--build-snapshot", false).into());
                i += 2;
            }
            "--resume-snapshot" => {
                resume_snapshot = Some(take(i, "--resume-snapshot", false).into());
                i += 2;
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown arg: {}", other);
                usage();
            }
        }
    }

    let kernel = vmette_assets::require_asset(kernel, "vmlinuz-virt").unwrap_or_else(|e| {
        eprintln!("error: {e}");
        usage();
    });
    let initramfs =
        vmette_assets::require_asset(initramfs, "initramfs-vmette").unwrap_or_else(|e| {
            eprintln!("error: {e}");
            usage();
        });
    let rootfs_spec = rootfs_spec.unwrap_or_else(|| {
        eprintln!("error: --rootfs required (try `vmette providers` for examples)");
        usage();
    });

    if build_snapshot.is_some() && resume_snapshot.is_some() {
        eprintln!("error: --build-snapshot and --resume-snapshot are mutually exclusive");
        usage();
    }
    if resume_snapshot.is_some() && exec_cmd.is_none() {
        eprintln!("error: --resume-snapshot requires --exec");
        usage();
    }
    // --switch-root + --rootfs-ro + --exec is a panic combo at the guest:
    // /init can't write the runner script onto the RO rootfs and
    // switch_root execs a nonexistent file → PID 1 dies → kernel panic.
    // Reject at parse time; users with this need can either drop one
    // flag or pre-bake their workload into the image as the actual init.
    if switch_root && rootfs_ro && exec_cmd.is_some() {
        eprintln!(
            "error: --switch-root + --rootfs-ro + --exec would panic the guest \
             (no writable place for /init's runner script). Drop --rootfs-ro \
             or --switch-root, or bake the workload into the image as PID 1."
        );
        usage();
    }

    let mut c = Config::new(kernel, initramfs);
    if let Some(s) = cfg_cmdline {
        c.cmdline = s;
    }
    c.shares = shares;
    c.disks = disks;
    c.exec_cmd = exec_cmd;
    c.switch_root = switch_root;
    c.net = net;
    c.quiet = quiet;
    c.vsock_port = vsock_port;
    c.guest_vsock_port = guest_vsock_port;
    c.timeout_seconds = timeout_seconds;
    c.vcpus = vcpus;
    c.mem_mib = mem_mib;
    c.build_snapshot = build_snapshot;
    c.resume_snapshot = resume_snapshot;
    ParsedArgs {
        config: c,
        rootfs_spec,
        rootfs_ro,
        offline,
    }
}

fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/vmette")
}

fn guest_helpers_dir() -> Option<PathBuf> {
    // Look for vsock-send / vsock-runner under common locations:
    // 1. Next to the vmette binary (installed layout, share/vmette/guest)
    // 2. assets/alpine-rootfs/usr/local/bin (repo layout)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(p) = dir.parent() {
                let candidate = p.join("share/vmette/guest");
                if candidate.join("vsock-send").exists() {
                    return Some(candidate);
                }
            }
        }
    }
    let repo = std::env::current_dir()
        .ok()?
        .join("assets/alpine-rootfs/usr/local/bin");
    if repo.join("vsock-send").exists() {
        return Some(repo);
    }
    None
}

fn print_providers(registry: &Registry) {
    println!("registered rootfs providers (first-match-wins order):");
    for name in registry.names() {
        let example = match name {
            "dir" => "  --rootfs /path/to/dir",
            "squashfs" => {
                "  --rootfs squashfs+file:///img.sqfs   |   squashfs+https://host/img.sqfs"
            }
            "tar" => "  --rootfs tar+https://host/rootfs.tar.gz   |   tar+file:///tmp/r.tar",
            "oci" => "  --rootfs alpine:3.20   |   oci://ghcr.io/foo/bar:v1",
            _ => "  (third-party provider)",
        };
        println!("  - {name}\n    {example}");
        if name == "oci" {
            println!("    private images: VMETTE_OCI_AUTH_<HOST>=user:secret, or VMETTE_OCI_TOKEN");
            println!("    (+ optional VMETTE_OCI_USER); else falls back to ~/.docker/config.json");
        }
    }
}

fn main() -> ExitCode {
    // Light tracing so the OCI puller and tar fetcher can log to stderr.
    // Disable ANSI colours when stderr isn't a terminal (e.g. the vmette-mcp
    // server captures it into a pipe and returns it to the agent verbatim —
    // raw escape codes there are noise). Keep colour for interactive use.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vmette_provider_oci=info,vmette_provider_tar=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .without_time()
        .with_target(false)
        .try_init();

    // Sub-commands take precedence over the run flow.
    let mut argv = std::env::args().skip(1);
    if let Some(first) = argv.next() {
        if first == "providers" {
            print_providers(&default_registry());
            return ExitCode::SUCCESS;
        }
        if first == "desktop" {
            return desktop::run(argv.collect());
        }
    }

    let parsed = parse_args();
    let mut config = parsed.config;

    // Resolve --rootfs SPEC through the provider registry, then plug the
    // returned path into the VM config. Providers handle their own
    // caching, network access, and idempotency.
    let registry = default_registry();
    let ctx = Context::new(cache_root())
        .offline(parsed.offline)
        .guest_helpers_dir(guest_helpers_dir());
    let artifact = match registry.resolve(&parsed.rootfs_spec, &ctx) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[vmette] rootfs resolution failed: {}", e);
            eprintln!("[vmette] try `vmette providers` for registered providers and examples");
            return ExitCode::from(1);
        }
    };
    // Block images (e.g. squashfs) are inherently read-only — the guest builds
    // a tmpfs overlay for writes — so --rootfs-ro is a silent no-op there. Warn
    // rather than let the user believe the flag changed anything.
    if parsed.rootfs_ro && matches!(artifact, RootfsArtifact::BlockImage { .. }) {
        eprintln!(
            "[vmette] note: --rootfs-ro is redundant for block images; \
             they are always mounted read-only (writes go to a tmpfs overlay)"
        );
    }
    config.set_rootfs_artifact(artifact, parsed.rootfs_ro);

    match vmette::run(&config) {
        Ok(out) => {
            // Note: vmette::run normally exits via the VM's stop delegate
            // and never returns here. This branch only fires for snapshot
            // ops which return without going through the runloop.
            ExitCode::from(out.exit_code as u8)
        }
        Err(e) => {
            eprintln!("[vmette] error: {}", e);
            ExitCode::from(1)
        }
    }
}
