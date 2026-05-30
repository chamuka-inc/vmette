# vmette

Local Linux microVM sandbox for macOS, built on Apple's
`Virtualization.framework`. Ships as a CLI, a Rust library, a C-ABI
dynamic library, and a long-lived daemon.

- Boots a Linux guest in ~1 second
- **Pluggable rootfs providers**: local directories, OCI/Docker images
  (`alpine:3.20`, `ghcr.io/...`, public or private), tarballs over
  HTTP/HTTPS/file, or prebuilt squashfs block images — dispatched
  through a single `--rootfs SPEC` flag. Add your own with one trait
  impl in a sibling crate.
- **Immutable block-image rootfs**: a `.sqfs` attaches read-only as
  virtio-blk with a tmpfs overlay, so the base is content-addressable
  and shareable across sessions (`--rootfs squashfs+…`).
- **Private registry auth** for the OCI provider via env vars or
  `~/.docker/config.json` (`VMETTE_OCI_TOKEN`).
- virtio-fs for sharing host dirs, virtio-net (NAT), virtio-blk,
  vsock with bidirectional bytes
- Exit-code propagation, timeout, switch-root, read-only rootfs share
- Universal binary (x86_64 + arm64)
- ~440 KB host binary, 25 KB guest helpers

## Install

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
```

Installs to `~/.local/share/vmette/`, symlinks `~/.local/bin/{vmette,vmetted}`.
macOS-only (any version with VZ — i.e. 11+; tested on 14.7 Intel).

Or build from source:

```sh
git clone https://github.com/chamuka-inc/vmette
cd vmette
make build              # cargo build + codesign
make test               # cargo unit + end-to-end VM smoke
```

## Use it (CLI)

Easiest path — pull an OCI image and run a command in it:

```sh
vmette --rootfs python:3.12-alpine \
       --exec 'python3 -c "print(2**32)"; exit 0'
```

The kernel and initramfs are auto-discovered: the release tarball ships
them under `$PREFIX/assets`, and from a repo checkout vmette finds
`./assets`. Override with `--kernel` / `--initramfs`, or point
`$VMETTE_ASSETS_DIR` at a directory holding `vmlinuz-virt` +
`initramfs-vmette`.

First run pulls + extracts the image (alpine:3.20 ≈ 30 s); subsequent
runs are cache hits (~3 s, mostly VM boot + manifest verification).
Images are cached at `~/Library/Caches/vmette/oci/`.

The same flag accepts a local directory, an OCI ref, or a tarball URL —
each dispatched to a different provider. List them with
`vmette providers`:

```sh
vmette --rootfs ./assets/alpine-rootfs              --exec 'uname -a'
vmette --rootfs alpine:3.20                         --exec 'cat /etc/alpine-release'
vmette --rootfs oci://ghcr.io/foo/bar:v1            --exec '/run-tests.sh'
vmette --rootfs tar+https://h/builds/r.tar.gz       --exec 'make ci'
vmette --rootfs tar+file:///tmp/local-rootfs.tar    --exec 'ls /'
vmette --rootfs squashfs+file:///tmp/base.sqfs      --exec 'ls /'
```

The bundled orchestrator script auto-fetches assets on first run and
uses the locally-built alpine rootfs:

```sh
bash scripts/run.sh 'echo hello; exit 7'                     # → host exit 7
bash scripts/run.sh --net 'wget -O - http://example.com'     # network on
bash scripts/run.sh 'echo hi | vsock-send $VMETTE_VSOCK_PORT'
bash scripts/run.sh --switch-root 'cat /proc/1/comm'
bash scripts/run.sh --timeout 3 'sleep 30'                   # → host exit 124
bash scripts/run.sh --rootfs-ro 'mount | head -1'
```

Full flag list: `vmette --help` or [`docs/CLI.md`](docs/CLI.md).

## Use it (Rust library)

The library accepts a directory path; resolution from a spec (OCI ref,
tarball URL, …) goes through the provider registry first.

```rust
use vmette::provider::{Context, DirProvider, Registry};
use vmette::Config;
use vmette_provider_oci::OciProvider;
use vmette_provider_squashfs::SquashfsProvider;
use vmette_provider_tar::TarProvider;

fn main() {
    let registry = Registry::new()
        .with(DirProvider::new())
        .with(SquashfsProvider::new())
        .with(TarProvider::new())
        .with(OciProvider::new());
    let ctx = Context::new(std::env::var_os("HOME").unwrap_or_default());
    // resolve() returns a RootfsArtifact (a directory share or a block
    // image); set_rootfs_artifact wires whichever form into the config.
    let artifact = registry.resolve("alpine:3.20", &ctx).unwrap();

    let mut cfg = Config::new("./assets/vmlinuz-virt", "./assets/initramfs-vmette");
    cfg.set_rootfs_artifact(artifact, false);
    cfg.exec_cmd = Some("echo hello from rust; exit 42".into());

    // vmette::run() blocks until guest poweroff and process-exits with
    // the guest's code via the VM lifecycle delegate.
    let _ = vmette::run(&cfg);
}
```

`Cargo.toml`:

```toml
[dependencies]
vmette                   = "0.1"
vmette-provider-oci      = "0.1"
vmette-provider-tar      = "0.1"  # optional; drop if you only need oci + dir
vmette-provider-squashfs = "0.1"  # optional; squashfs+ block images
```

See [`crates/vmette/examples/minimal.rs`](crates/vmette/examples/minimal.rs).

## Use it (C ABI)

```c
#include "vmette.h"

int main(int argc, char **argv) {
    vmette_config_t *cfg = vmette_config_new(argv[1], argv[2]);
    vmette_config_set_rootfs_share(cfg, argv[3], false);
    vmette_config_set_exec(cfg, "echo hello from C; exit 11");
    vmette_run_output_t *out = NULL;
    vmette_run(cfg, &out);                /* exits on guest poweroff */
    return vmette_run_output_exit_code(out);
}
```

Link with `-L lib -lvmette`. The header is auto-generated from
`crates/vmette/src/ffi.rs` via cbindgen and checked in at
`crates/vmette/include/vmette.h`.

See [`crates/vmette/examples/minimal.c`](crates/vmette/examples/minimal.c)
and [`docs/API.md`](docs/API.md).

## Use it (daemon)

```sh
vmetted &
```

Listens on `~/Library/Caches/vmette/vmette.sock`. Speaks line-delimited
JSON: client sends one request object, daemon streams `stdout` / `stderr`
/ `exit` frames. Useful for amortizing per-invocation cost or driving
many runs from a long-lived caller.

```python
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/Users/me/Library/Caches/vmette/vmette.sock")
s.sendall((json.dumps({
    "kernel": "/abs/path/vmlinuz-virt",
    "initramfs": "/abs/path/initramfs-vmette",
    "rootfs": "/abs/path/alpine-rootfs",   # also accepts alpine:3.20, tar+https://..., etc.
    "exec": "echo from daemon; exit 17",
}) + "\n").encode())
s.shutdown(socket.SHUT_WR)
print(s.recv(65536).decode())
```

Output:

```
{"kind":"stdout","data":"from daemon\r\n"}
{"kind":"exit","code":17}
```

See [`docs/DAEMON.md`](docs/DAEMON.md).

## Use it (AI agents via MCP)

```jsonc
// ~/Library/Application Support/Claude/claude_desktop_config.json
{ "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": ["--default-image", "python:3.12-alpine", "--allow-network"]
}}}
```

`vmette-mcp` is a Model Context Protocol server that exposes vmette to
any MCP-aware agent host — Claude Desktop, Cursor, Cline, Zed, Goose,
etc. It ships an `execute` tool, `fetch_url`, a `workspace_*` family
(each call boots a fresh microVM), and a `desktop_*` family for
computer use. The agent never touches your real filesystem unless you
explicitly shared a directory into it.

See [`docs/MCP.md`](docs/MCP.md) for the full tool reference, security
model, and client configs.

## Use it (desktop computer use)

Drive a **persistent** graphical Linux desktop inside a microVM —
screenshot, click, type — the way a computer-use agent expects. A
headless X server (Xvfb) + window manager run in the guest, driven by an
in-guest agent over vsock; no Apple graphics window is involved.

```sh
vmetted &                                    # sessions live in the daemon
bash scripts/build-desktop-image.sh          # build the desktop rootfs image

SID=$(vmette desktop start)
vmette desktop screenshot "$SID" --out shot.png && open shot.png
vmette desktop exec "$SID" 'xterm &'
vmette desktop click "$SID" 640 400
vmette desktop type  "$SID" 'echo hello'
vmette desktop stop  "$SID"
```

The same capability is exposed to agents through the MCP `desktop_*`
tools (`desktop_screenshot` returns a PNG image block). See
[`docs/DESKTOP.md`](docs/DESKTOP.md) for the session lifecycle, protocol,
action reference, and image build.

## How it works

1. `vmette` builds a `VZVirtualMachineConfiguration` (kernel, initramfs,
   virtio devices, vsock).
2. The kernel command line includes `vmette.exec=<base64(cmd)>` plus
   `vmette.*` flags. The guest's `/init`
   ([`scripts/custom-init.sh`](scripts/custom-init.sh)) parses these in
   pure shell, mounts virtio-fs shares, brings up the network if
   requested, then `chroot` / `switch_root` into the rootfs share and
   runs the command.
3. After the command exits, the guest writes the code to `.vmette-exit`
   on the (writable) rootfs share, syncs, and `poweroff -f`. VZ fires
   the lifecycle delegate; the host reads the file and exits with that
   code.
4. vsock is wired both ways: a host listener accepts guest-initiated
   connections (echoing bytes back), and the snapshot-resume path uses
   an outgoing host→guest connect (arm64 only).

## Layout

```
crates/
  vmette/                Rust library (lib + cdylib + staticlib)
    src/lib.rs           public API: Config, run(), RootfsArtifact, VsockPort, RootfsShare, ShareMount
    src/provider.rs      RootfsProvider trait + Registry + Context + DirProvider + RootfsArtifact
    src/ffi.rs           #[no_mangle] extern "C" shims → cbindgen
    src/vz/              objc2 bindings to VZ (config, delegate, vsock, snapshot)
    src/lifecycle.rs     run() orchestration + timeout + signal handlers
    src/session.rs       Session primitive (OneShot + Agent workloads) + Send handles
    src/desktop.rs       desktop computer-use protocol (framed wire format + actions)
    src/cmdline.rs       base64 vmette.* cmdline assembly
    include/vmette.h     generated header (checked in)
    examples/            minimal.rs + minimal.c
  vmette-provider-oci/      OCI/Docker image provider (alpine:3.20, oci://…, private-registry auth)
  vmette-provider-tar/      Tarball provider (tar+https://, tar+file://)
  vmette-provider-squashfs/ Squashfs block-image provider (squashfs+file://, squashfs+https://)
  vmette-cli/            `vmette` CLI binary (registers dir/squashfs/tar/oci providers)
  vmette-daemon/         `vmetted` UNIX-socket dispatcher (tokio + JSON)
                         + desktop session registry (stateful, in-process VMs)
  vmette-mcp/            `vmette-mcp` Model Context Protocol server for AI agents
guest/                   C sources cross-compiled for the Linux guest
  vsock-send.c           pipe stdin → AF_VSOCK → host listener
  vsock-runner.c         snapshot-mode cmd server
  vmette-desktop-agent.c desktop agent: XTEST input + XGetImage capture over vsock
images/
  vmette-desktop/        Dockerfile + entrypoint for the desktop rootfs OCI image
entitlements.plist       com.apple.security.virtualization + .hypervisor
scripts/
  fetch-assets.sh        alpine netboot initramfs + linux-virt apk
  fetch-alpine-rootfs.sh alpine-minirootfs → assets/alpine-rootfs/
  build-initramfs.sh     repack initramfs: busybox + apk modules + /init
  build-vsock-send.sh    cross-compile guest helpers via musl-cross
  build-desktop-agent.sh compile vmette-desktop-agent against the image libc
  build-desktop-image.sh build/push the desktop rootfs OCI image
  custom-init.sh         PID-1 inside the guest
  run.sh                 dev wrapper: ensures assets, builds, runs vmette
  install.sh             curl-pipe end-user installer
tests/
  run.sh                 end-to-end smoke (~20 gates)
  fixtures/share/        committed sample for --share TAG=PATH
.github/workflows/
  release.yml            tag-triggered make universal + dist + upload
Makefile                 help | build | universal | dist | test | run | shell | clean
```

## Constraints

- **macOS only.** VZ is Apple-private. No Linux/Windows port planned.
- **Snapshot/restore is Apple-Silicon-only.** Apple gates
  `saveMachineStateToURL:` and `restoreMachineStateFromURL:` behind
  `#if defined(__arm64__)` in the SDK headers. On Intel, attempting
  `--build-snapshot` / `--resume-snapshot` returns
  `VmetteStatus::SnapshotUnsupported` (CLI exits 1 with a clear message).
- **Daemon's snapshot pool is also Apple-Silicon-only.** v0.1 of vmetted
  spawns a fresh `vmette` subprocess per request; the snapshot-warm-pool
  optimization is on the roadmap for v0.2 once it lands on aarch64.
- **Guest assets are currently x86_64-only.** The repack pipeline
  references `linux-virt-x86_64.apk`. arm64 needs a parallel
  `linux-virt-aarch64.apk` + `aarch64-linux-musl-gcc` install. Plumbing
  is documented in [`docs/HACKING.md`](docs/HACKING.md); verification
  awaits arm64 hardware.
- **Desktop sessions are software-rendered and live in the daemon.**
  Desktop computer use uses headless Xvfb (no GPU) and requires `vmetted`
  to hold the persistent VM; each session is a ~2 GB VM, capped and
  idle-evicted. Fine for agentic GUI control and UI testing, not for
  video / WebGL / 3D. See [`docs/DESKTOP.md`](docs/DESKTOP.md).

## Docs

- [`docs/CLI.md`](docs/CLI.md) — full flag reference
- [`docs/API.md`](docs/API.md) — Rust + C library API
- [`docs/DAEMON.md`](docs/DAEMON.md) — vmetted protocol spec
- [`docs/MCP.md`](docs/MCP.md) — vmette-mcp server tool reference + client configs
- [`docs/DESKTOP.md`](docs/DESKTOP.md) — desktop computer use: sessions, protocol, image build
- [`docs/HACKING.md`](docs/HACKING.md) — build, test, debug
- [`CHANGELOG.md`](CHANGELOG.md) — release notes

## License

MIT. See [LICENSE](LICENSE).
