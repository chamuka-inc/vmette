// Minimal Rust caller of the vmette library. Boots an alpine guest
// from the repo's existing assets and runs a one-shot command.
//
//   cargo run --release --example minimal -- KERNEL INITRAMFS ROOTFS [CMD]
//
// The binary inherits the parent process's entitlements at run time; the
// example is normally launched from `scripts/run-example.sh` which
// codesigns the cargo-built binary first. Running raw works too if you
// codesign the example binary manually after `cargo build --example`.

use std::env;
use std::process::ExitCode;

use vmette::{Config, Rootfs, RootfsShare, VsockPort};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: minimal KERNEL INITRAMFS ROOTFS [CMD]");
        return ExitCode::from(2);
    }
    let cmd = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "uname -a; cat /etc/alpine-release; exit 0".into());

    let mut cfg = Config::new(&args[0], &args[1]);
    cfg.rootfs = Some(Rootfs::Share(RootfsShare {
        path: args[2].clone().into(),
        read_only: false,
    }));
    cfg.exec_cmd = Some(cmd);
    cfg.vsock_port = VsockPort::Auto;

    match vmette::run(&cfg) {
        // The happy path doesn't actually return — vmette::run exits the
        // process from the VM lifecycle delegate. This arm is for the
        // snapshot-build/resume modes which return synchronously.
        Ok(out) => ExitCode::from(out.exit_code as u8),
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::from(1)
        }
    }
}
