//! vmette CLI — thin wrapper over the `vmette` library.
//!
//! Hand-rolled arg parsing (no clap, to keep the binary small and the
//! dep tree shallow). Mirrors the previous ObjC CLI's flag surface so
//! existing scripts don't need to change.

use std::path::PathBuf;
use std::process::ExitCode;

use vmette::{Config, RootfsShare, ShareMount, VsockPort};
use vmette_image::PullOptions;

fn usage() -> ! {
    eprintln!(
        "vmette --kernel PATH --initramfs PATH [options]\n\
         \n\
         required:\n\
           --kernel           PATH      bzImage on x86_64\n\
           --initramfs        PATH      built by scripts/build-initramfs.sh\n\
         \n\
         workload:\n\
           --image            REF       pull an OCI image (e.g. alpine:3.20) and use\n\
                                        as the rootfs share. Mutex with --rootfs-share.\n\
           --image-offline              never hit the registry; fail if not cached.\n\
           --rootfs-share     PATH      host dir mounted as guest /  (virtio-fs tag 'rootfs')\n\
           --ro-rootfs-share            mount rootfs share read-only\n\
           --share            TAG=PATH  extra virtio-fs mount at /mnt/<TAG> (repeatable)\n\
           --disk             PATH      raw block image as virtio-blk (repeatable)\n\
           --exec             CMD       shell command to run in guest, then poweroff\n\
           --net                        attach virtio-net with NAT; /init runs udhcpc on eth0\n\
           --switch-root                use switch_root instead of chroot for the exec env\n\
         \n\
         runtime:\n\
           --timeout          N         force-stop the VM after N seconds, exit 124\n\
           --cmdline          STR       extra kernel cmdline (default 'console=hvc0 quiet')\n\
           --vsock-port       N         -1=disable; 0=auto-pick 50000-59999 (default); >0=explicit\n\
           --vcpus            N         default 1\n\
           --mem-mib          N         default 512\n\
         \n\
         snapshot (Apple Silicon only):\n\
           --build-snapshot   PATH      boot, wait for guest READY, pause, save\n\
           --resume-snapshot  PATH      restore, send --exec via vsock, drain output\n\
           --guest-vsock-port N         guest vsock-runner listens here (default 1025)\n"
    );
    std::process::exit(2);
}

struct ParsedArgs {
    config: Config,
    image_ref: Option<String>,
    /// The user's --ro-rootfs-share intent, kept separately so the --image
    /// branch (which builds a RootfsShare itself) can honor it.
    rootfs_ro: bool,
    image_offline: bool,
}

fn parse_args() -> ParsedArgs {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut kernel: Option<PathBuf> = None;
    let mut initramfs: Option<PathBuf> = None;
    let mut cfg_cmdline: Option<String> = None;
    let mut rootfs_share: Option<PathBuf> = None;
    let mut rootfs_ro = false;
    let mut image_ref: Option<String> = None;
    let mut image_offline = false;
    let mut shares: Vec<ShareMount> = Vec::new();
    let mut disks: Vec<PathBuf> = Vec::new();
    let mut exec_cmd: Option<String> = None;
    let mut switch_root = false;
    let mut net = false;
    let mut vsock_port = VsockPort::Auto;
    let mut guest_vsock_port: u32 = 1025;
    let mut timeout_seconds: Option<u32> = None;
    let mut vcpus: u8 = 1;
    let mut mem_mib: u64 = 512;
    let mut build_snapshot: Option<PathBuf> = None;
    let mut resume_snapshot: Option<PathBuf> = None;

    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        let take_next = || -> String {
            if i + 1 >= raw.len() { usage(); }
            raw[i + 1].clone()
        };
        match arg.as_str() {
            "--kernel"            => { kernel = Some(take_next().into()); i += 2; }
            "--initramfs"         => { initramfs = Some(take_next().into()); i += 2; }
            "--cmdline"           => { cfg_cmdline = Some(take_next()); i += 2; }
            "--rootfs-share"      => { rootfs_share = Some(take_next().into()); i += 2; }
            "--ro-rootfs-share"   => { rootfs_ro = true; i += 1; }
            "--image"             => { image_ref = Some(take_next()); i += 2; }
            "--image-offline"     => { image_offline = true; i += 1; }
            "--share"             => {
                let s = take_next();
                let (tag, path) = s.split_once('=').unwrap_or_else(|| {
                    eprintln!("error: --share expects TAG=PATH, got '{}'", s);
                    usage();
                });
                shares.push(ShareMount { tag: tag.into(), path: path.into() });
                i += 2;
            }
            "--disk"              => { disks.push(take_next().into()); i += 2; }
            "--exec"              => { exec_cmd = Some(take_next()); i += 2; }
            "--net"               => { net = true; i += 1; }
            "--switch-root"       => { switch_root = true; i += 1; }
            "--timeout"           => { timeout_seconds = Some(take_next().parse().unwrap_or(0)); i += 2; }
            "--vsock-port"        => {
                let n: i64 = take_next().parse().unwrap_or(0);
                vsock_port = match n {
                    n if n < 0 => VsockPort::Disabled,
                    0          => VsockPort::Auto,
                    n          => VsockPort::Fixed(n as u32),
                };
                i += 2;
            }
            "--guest-vsock-port"  => { guest_vsock_port = take_next().parse().unwrap_or(1025); i += 2; }
            "--vcpus"             => { vcpus = take_next().parse().unwrap_or(1); i += 2; }
            "--mem-mib"           => { mem_mib = take_next().parse().unwrap_or(512); i += 2; }
            "--build-snapshot"    => { build_snapshot = Some(take_next().into()); i += 2; }
            "--resume-snapshot"   => { resume_snapshot = Some(take_next().into()); i += 2; }
            "-h" | "--help"       => usage(),
            other                 => { eprintln!("unknown arg: {}", other); usage(); }
        }
    }

    let kernel = kernel.unwrap_or_else(|| { eprintln!("error: --kernel required"); usage(); });
    let initramfs = initramfs.unwrap_or_else(|| { eprintln!("error: --initramfs required"); usage(); });

    if build_snapshot.is_some() && resume_snapshot.is_some() {
        eprintln!("error: --build-snapshot and --resume-snapshot are mutually exclusive");
        usage();
    }
    if resume_snapshot.is_some() && exec_cmd.is_none() {
        eprintln!("error: --resume-snapshot requires --exec");
        usage();
    }
    if image_ref.is_some() && rootfs_share.is_some() {
        eprintln!("error: --image and --rootfs-share are mutually exclusive");
        usage();
    }
    // --switch-root + --ro-rootfs-share + --exec is a panic combo at the
    // guest: /init can't write the runner script onto the RO rootfs and
    // switch_root execs a nonexistent file → PID 1 dies → kernel panic.
    // Reject at parse time; users with this need can either drop one
    // flag or pre-bake their workload into the image as the actual init.
    if switch_root && rootfs_ro && exec_cmd.is_some() {
        eprintln!(
            "error: --switch-root + --ro-rootfs-share + --exec would panic the guest \
             (no writable place for /init's runner script). Drop --ro-rootfs-share \
             or --switch-root, or bake the workload into the image as PID 1."
        );
        usage();
    }

    let mut c = Config::new(kernel, initramfs);
    if let Some(s) = cfg_cmdline { c.cmdline = s; }
    if let Some(p) = rootfs_share {
        c.rootfs_share = Some(RootfsShare { path: p, read_only: rootfs_ro });
    }
    c.shares = shares;
    c.disks = disks;
    c.exec_cmd = exec_cmd;
    c.switch_root = switch_root;
    c.net = net;
    c.vsock_port = vsock_port;
    c.guest_vsock_port = guest_vsock_port;
    c.timeout_seconds = timeout_seconds;
    c.vcpus = vcpus;
    c.mem_mib = mem_mib;
    c.build_snapshot = build_snapshot;
    c.resume_snapshot = resume_snapshot;
    ParsedArgs { config: c, image_ref, rootfs_ro, image_offline }
}

fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/vmette/images")
}

fn guest_helpers_dir() -> Option<PathBuf> {
    // Look for vsock-send / vsock-runner under common locations:
    // 1. Next to the vmette binary (installed layout)
    // 2. assets/alpine-rootfs/usr/local/bin (repo layout)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.parent().map(|p| p.join("share/vmette/guest"));
            if let Some(c) = candidate {
                if c.join("vsock-send").exists() {
                    return Some(c);
                }
            }
        }
    }
    // Repo layout: cwd/assets/alpine-rootfs/usr/local/bin
    let repo = std::env::current_dir()
        .ok()?
        .join("assets/alpine-rootfs/usr/local/bin");
    if repo.join("vsock-send").exists() {
        return Some(repo);
    }
    None
}

fn main() -> ExitCode {
    // Light tracing so vmette-image can log to stderr while pulling.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vmette_image=info".into()),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .try_init();

    let parsed = parse_args();
    let mut config = parsed.config;

    // OCI image flow: pull + extract before handing off to vmette::run.
    if let Some(image_ref) = parsed.image_ref {
        let cache = cache_root();
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[vmette] tokio init: {}", e);
                return ExitCode::from(1);
            }
        };
        let opts = PullOptions {
            offline: parsed.image_offline,
            ..PullOptions::default()
        };
        let rootfs = match rt.block_on(vmette_image::pull_with_options(&image_ref, &cache, &opts)) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[vmette] image pull failed: {}", e);
                return ExitCode::from(1);
            }
        };
        // Inject vmette guest helpers (vsock-send, vsock-runner) so vsock
        // workflows work against any pulled image.
        if let Some(src) = guest_helpers_dir() {
            if let Err(e) = vmette_image::inject_guest_helpers(&rootfs, &src) {
                eprintln!("[vmette] warning: helper inject: {}", e);
            }
        }
        config.rootfs_share = Some(vmette::RootfsShare {
            path: rootfs,
            read_only: parsed.rootfs_ro,
        });
    }

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
