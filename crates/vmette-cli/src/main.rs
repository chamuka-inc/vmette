//! vmette CLI — thin wrapper over the `vmette` library.
//!
//! Hand-rolled arg parsing (no clap, to keep the binary small and the
//! dep tree shallow). The rootfs source is selected via a single
//! `--rootfs SPEC` flag dispatched to a [`Registry`] of providers; see
//! `vmette providers` for the active list.

mod desktop;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use vmette::provider::{Context, Registry};
use vmette::{Config, RootfsArtifact, ShareMount, VsockPort};
use vmette_providers::default_registry;

/// Append the machine-wide host CA-cert share (`$VMETTE_CA_CERTS` /
/// `~/.config/vmette/certs`) to `shares`, unless the caller already supplied a
/// `certs` share. Lets a guest trust a TLS-inspecting proxy / enterprise CA
/// with no per-call flag; opt-in (no-op when nothing is configured). Shared by
/// the one-shot CLI and `vmette desktop start` so both resolve it identically.
pub(crate) fn ensure_ca_share(shares: &mut Vec<ShareMount>) {
    if shares
        .iter()
        .any(|s| s.tag == vmette_assets::CA_CERTS_SHARE_TAG)
    {
        return;
    }
    if let Some(share) = vmette_assets::resolve_ca_share(None) {
        shares.push(share);
    }
}

fn usage() -> ! {
    eprintln!(
        "vmette --rootfs SPEC [--kernel PATH] [--initramfs PATH] [options]\n\
         vmette quickstart                                 # boot a hello-world VM to verify the install\n\
         vmette providers                                  # list registered providers\n\
         vmette desktop <command> [options]                # desktop computer use (via vmetted)\n\
         \n\
         required:\n\
           --rootfs           SPEC      see `vmette providers` for valid forms\n\
         \n\
                 boot assets (auto-discovered from $VMETTE_ASSETS_DIR, ./assets, or the install prefix):\n\
                     --kernel           PATH      vmlinuz from alpine linux-virt (default: discovered vmlinuz-virt)\n\
           --initramfs        PATH      built by scripts/build-initramfs.sh (default: discovered initramfs-vmette)\n\
         \n\
         rootfs:\n\
           --rootfs-ro                  mount the rootfs share read-only\n\
           --offline                    forbid network access; resolve from cache only\n\
         \n\
         workload:\n\
           --share            TAG=PATH  extra virtio-fs mount at /mnt/<TAG> (repeatable)\n\
           --disk             PATH      raw block image as virtio-blk (repeatable)\n\
           --scratch          SIZE      ephemeral ext4 scratch disk (e.g. 8G, 512M) as the writable overlay; lifts the RAM cap on big builds\n\
           --env              KEY=VALUE set a guest env var, overrides image env (repeatable)\n\
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
           --rootfs tar+file:///tmp/r.tar        local tarball\n\
         \n\
         docs: https://github.com/chamuka-inc/vmette/tree/main/docs  (CLI.md · MCP.md · DESKTOP.md)\n\
         \n\
         examples:\n\
           vmette quickstart                                                # verify your install\n\
           vmette --rootfs alpine:3.20 --exec 'cat /etc/alpine-release'     # one-off command\n\
           vmette desktop start                                            # boot a GUI desktop\n\
           claude mcp add vmette --scope user -- vmette-mcp --allow-network  # sandbox for Claude Code\n"
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

/// Parse a `--scratch` size into MiB. Accepts a bare number (MiB) or a
/// `G`/`g` (GiB) / `M`/`m` (MiB) suffix: `8G`, `512M`, `2048`. Rejects zero,
/// overflow, and anything unparseable as a usage error (no silent fallback —
/// the same strictness as `parse_num`, so a typo can't quietly shrink the disk).
fn parse_size_mib(flag: &str, v: &str) -> u64 {
    let s = v.trim();
    let (num_str, mult): (&str, u64) = match s.chars().last() {
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let n: u64 = num_str.trim().parse().unwrap_or_else(|_| {
        eprintln!("error: {flag} expects a size like 8G, 512M, or a number of MiB, got '{v}'");
        usage();
    });
    let mib = n.checked_mul(mult).unwrap_or_else(|| {
        eprintln!("error: {flag} size '{v}' is too large");
        usage();
    });
    if mib == 0 {
        eprintln!("error: {flag} size must be greater than zero, got '{v}'");
        usage();
    }
    mib
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
    let mut scratch_mib: Option<u64> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut exec_cmd: Option<String> = None;
    let mut switch_root = false;
    let mut net = false;
    let mut quiet = false;
    // `None` = flag absent → defer to Config::new, the single owner of these
    // defaults (so the literal 1/512/1025/Auto live in exactly one place).
    let mut vsock_port: Option<VsockPort> = None;
    let mut guest_vsock_port: Option<u32> = None;
    let mut timeout_seconds: Option<u32> = None;
    let mut vcpus: Option<u8> = None;
    let mut mem_mib: Option<u64> = None;
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
            "--scratch" => {
                let v = take(i, "--scratch", false);
                scratch_mib = Some(parse_size_mib("--scratch", &v));
                i += 2;
            }
            // --env VALUE may legitimately contain a leading '-' (e.g.
            // `FOO=-x`), so allow a dash-prefixed argument.
            "--env" => {
                let s = take(i, "--env", true);
                let (k, val) = s.split_once('=').unwrap_or_else(|| {
                    eprintln!("error: --env expects KEY=VALUE, got '{}'", s);
                    usage();
                });
                // Reject a bad key here rather than silently dropping it at
                // render time (the guest would just never see the var).
                if !vmette::is_valid_env_key(k) {
                    eprintln!(
                        "error: --env key must be a shell identifier \
                         ([A-Za-z_][A-Za-z0-9_]*), got '{}'",
                        k
                    );
                    usage();
                }
                env.push((k.into(), val.into()));
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
                vsock_port = Some(match n {
                    n if n < 0 => VsockPort::Disabled,
                    0 => VsockPort::Auto,
                    n => VsockPort::Fixed(n as u32),
                });
                i += 2;
            }
            "--guest-vsock-port" => {
                let v = take(i, "--guest-vsock-port", false);
                guest_vsock_port = Some(parse_num::<u32>("--guest-vsock-port", &v));
                i += 2;
            }
            "--vcpus" => {
                let v = take(i, "--vcpus", false);
                vcpus = Some(parse_num::<u8>("--vcpus", &v));
                i += 2;
            }
            "--mem-mib" => {
                let v = take(i, "--mem-mib", false);
                mem_mib = Some(parse_num::<u64>("--mem-mib", &v));
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
    // --scratch backs the writable overlay upper, but --rootfs-ro mounts the
    // rootfs read-only with no overlay — so the disk would be attached and
    // never used. Reject loudly rather than silently ignoring an explicit
    // request (the guest gives no scratch_dev token in this case anyway).
    if rootfs_ro && scratch_mib.is_some() {
        eprintln!(
            "error: --scratch has no effect with --rootfs-ro (a read-only rootfs \
             has no writable overlay to back). Drop one of the two."
        );
        usage();
    }

    let mut c = Config::new(kernel, initramfs);
    if let Some(s) = cfg_cmdline {
        c.cmdline = s;
    }
    // Trust a machine-wide host CA (TLS-inspecting proxy / enterprise root) in
    // the guest when one is configured (`$VMETTE_CA_CERTS` /
    // `~/.config/vmette/certs`) and the caller didn't already pass an explicit
    // `--share certs=…`. The guest's PID-1 init installs the `certs` share into
    // its trust store before exec; same resolution every vmette root uses.
    ensure_ca_share(&mut shares);
    c.shares = shares;
    c.disks = disks;
    c.scratch_mib = scratch_mib;
    c.env = env;
    c.exec_cmd = exec_cmd;
    c.switch_root = switch_root;
    c.net = net;
    c.quiet = quiet;
    // Only override Config::new's defaults when the flag was actually given.
    if let Some(v) = vsock_port {
        c.vsock_port = v;
    }
    if let Some(v) = guest_vsock_port {
        c.guest_vsock_port = v;
    }
    c.timeout_seconds = timeout_seconds;
    if let Some(v) = vcpus {
        c.vcpus = v;
    }
    if let Some(v) = mem_mib {
        c.mem_mib = v;
    }
    c.build_snapshot = build_snapshot;
    c.resume_snapshot = resume_snapshot;
    ParsedArgs {
        config: c,
        rootfs_spec,
        rootfs_ro,
        offline,
    }
}

fn guest_helpers_dir() -> Option<PathBuf> {
    // Look for vsock-send / vsock-runner under common locations:
    // 1. Next to the vmette binary (installed layout, share/vmette/guest/<arch>)
    // 2. assets/<arch>/alpine-rootfs/usr/local/bin (repo layout)
    let arch = vmette_assets::guest_arch();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(p) = dir.parent() {
                for candidate in [
                    p.join("share/vmette/guest").join(arch),
                    p.join("share/vmette/guest"),
                ] {
                    if candidate.join("vsock-send").exists() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    let repo = std::env::current_dir()
        .ok()?
        .join("assets")
        .join(arch)
        .join("alpine-rootfs/usr/local/bin");
    if repo.join("vsock-send").exists() {
        return Some(repo);
    }
    let legacy_repo = std::env::current_dir()
        .ok()?
        .join("assets/alpine-rootfs/usr/local/bin");
    if legacy_repo.join("vsock-send").exists() {
        return Some(legacy_repo);
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

/// `vmette quickstart` — prove the install works by booting a real microVM,
/// then point the user at the things that matter (MCP for agents, a one-off
/// run, a desktop). Boots in a child `vmette` rather than calling `run()`
/// inline, because `run()` process-exits with the guest's code and never
/// returns — the child lets us print the next-steps afterward.
fn quickstart() -> ExitCode {
    eprintln!("vmette quickstart — verifying your install by booting a real microVM\n");

    // Boot assets must be discoverable, or nothing can boot. Fail early with
    // the same "where I looked" message the run path uses.
    for name in ["vmlinuz-virt", "initramfs-vmette"] {
        if let Err(e) = vmette_assets::require_asset(None, name) {
            eprintln!("✗ {e}");
            eprintln!(
                "\n  Boot assets ship in the release tarball under <prefix>/assets, or point\n  \
                 $VMETTE_ASSETS_DIR at a dir holding them. From a source checkout:\n  \
                  bash scripts/fetch-assets.sh && bash scripts/build-initramfs.sh"
            );
            return ExitCode::from(1);
        }
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ cannot locate the vmette binary: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!("→ booting alpine:3.20 (first run pulls the image; usually a few seconds)…\n");
    let status = Command::new(exe)
        .args([
            "--rootfs",
            "alpine:3.20",
            "--quiet",
            "--exec",
            "echo '  ✓ vmette works — a hardware-isolated Linux guest booted and ran this'; exit 0",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("\n✓ Your install works. Next:\n");
            println!("  • Sandbox for Claude Code (and other MCP hosts):");
            println!("      claude mcp add vmette --scope user -- vmette-mcp --allow-network");
            println!("  • Run a one-off command in a fresh VM:");
            println!("      vmette --rootfs alpine:3.20 --exec 'uname -a'");
            println!("  • Boot a GUI desktop for computer use:");
            println!("      vmette desktop start");
            println!("  • Docs: https://github.com/chamuka-inc/vmette/tree/main/docs");
            ExitCode::SUCCESS
        }
        Ok(s) => {
            let code = s.code().unwrap_or(1);
            eprintln!("\n✗ the hello-world VM exited with status {code}.");
            eprintln!(
                "  If you're offline, the first run needs network to pull alpine:3.20 — retry\n  \
                 online, or boot a local rootfs instead: vmette --rootfs /path/to/dir --exec true\n  \
                 See https://github.com/chamuka-inc/vmette/tree/main/docs/CLI.md"
            );
            ExitCode::from(code as u8)
        }
        Err(e) => {
            eprintln!("✗ could not run the vmette binary: {e}");
            ExitCode::from(1)
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
        if first == "--version" || first == "-V" || first == "version" {
            println!("vmette {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        if first == "providers" {
            print_providers(&default_registry());
            return ExitCode::SUCCESS;
        }
        if first == "quickstart" {
            return quickstart();
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
    let ctx = Context::new(vmette_assets::default_cache_root())
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
        // `run` returns the guest's exit code (it no longer exits the process);
        // the CLI owns the exit decision and forwards it.
        Ok(out) => ExitCode::from(out.exit_code as u8),
        Err(e) => {
            eprintln!("[vmette] error: {}", e);
            ExitCode::from(1)
        }
    }
}
