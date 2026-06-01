# vmette

**A security boundary for the age of local agents.** Run untrusted agents on
the machines your employees already have — without trusting the agent with the
machine.

vmette is a headless Linux microVM sandbox for macOS, built on Apple's
`Virtualization.framework`. It boots a hardware-isolated Linux guest in ~1
second and hands the agent a real machine to work on — a shell, a filesystem, a
network — that is *not* your machine. The isolation boundary is the hypervisor,
not a container: the guest cannot reach the host filesystem, your SSH keys, your
credentials, or your network unless you explicitly grant it.

## The problem

Coding agents and computer-use agents increasingly run untrusted code and take
untrusted actions. They `pip install` whatever a README names, execute model
output, follow links, and act on web content that can carry prompt injection.
Run that straight on a laptop and you've handed the agent — and everything it
pulls in — the whole machine: your files, your tokens, your network.

The usual answers are a fleet of cloud sandboxes (new infrastructure, new cost,
added latency, and data leaving the device) or a container (a shared kernel, one
namespace away from the host). vmette instead puts a fast, disposable,
hardware-isolated VM on the Mac the employee is already using:

- **Default-deny.** No host filesystem and no network out of the box. The agent
  gets exactly the directories you share into it and egress only when you turn
  it on (`--net` / `--allow-network`).
- **Hypervisor isolation.** The boundary is Apple's Virtualization.framework — a
  real VM with its own kernel, not a `chroot` or a shared-kernel container.
- **Ephemeral.** Each run is a fresh guest; an immutable squashfs rootfs keeps
  the base unmodifiable and content-addressable. Tear it down and nothing
  persists.
- **On-device.** No separate fleet to provision, no code or data leaving the
  laptop, and ~1 s to boot — cheap enough to spin one up per task.

## What you get

A sandbox that ships as a CLI, a Rust library, a C-ABI dynamic library, a
long-lived daemon, and an MCP server for agent hosts.

- Boots a hardware-isolated Linux guest in ~1 second
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
- **Grant, don't expose**: virtio-fs shares only the host dirs you name,
  virtio-net (NAT) is off until you ask, plus virtio-blk and bidirectional
  vsock
- Exit-code propagation, timeout, switch-root, read-only rootfs share
- Universal binary (x86_64 + arm64)
- ~440 KB host binary, 25 KB guest helpers

## Install

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
```

Installs to `~/.local/share/vmette/`, symlinks `~/.local/bin/{vmette,vmetted,vmette-mcp}`.
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

Compile and link against the shipped library and header:

```sh
cc -I include -L lib -lvmette -Wl,-rpath,lib -o demo demo.c
```

The `-Wl,-rpath,lib` matters: `libvmette.dylib` has the install name
`@rpath/libvmette.dylib`, so the binary needs an rpath pointing at the directory
that holds the dylib (here `lib`) to find it at runtime. The header is
auto-generated from `crates/vmette/src/ffi.rs` via cbindgen and checked in at
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

(The stream also carries the launcher banner as `stderr` frames and the guest's
`[init]`/boot lines as `stdout` frames; only the meaningful frames are shown
here. The MCP server slices these down to the command's own output.)

See [`docs/DAEMON.md`](docs/DAEMON.md).

## Use it (AI agents via MCP)

`vmette-mcp` is a Model Context Protocol server that hands any MCP-aware agent
host a sandboxed machine. Install it, then point your agent at it:

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
```

**Claude Code** — one command, no config file:

```sh
claude mcp add vmette --scope user -- vmette-mcp --allow-network
```

**Claude Desktop, Cursor, Cline, Zed, Goose** — point the host at the
`vmette-mcp` command. JSON example (Claude Desktop's
`~/Library/Application Support/Claude/claude_desktop_config.json`):

```jsonc
{ "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": ["--default-image", "python:3.12-alpine", "--allow-network"]
}}}
```

It ships an `execute` tool, `fetch_url`, a `workspace_*` family (each call
boots a fresh microVM), and a `desktop_*` family for computer use. This is the
agent's whole world: it runs inside the VM, so it never touches your real
filesystem unless you explicitly share a directory into it, and it has no
network egress unless you start the server with `--allow-network`.

Per-host setup snippets (Claude Code, Claude Desktop, Cursor, Cline, Zed,
Goose) plus the full tool reference and security model:
[`docs/MCP.md`](docs/MCP.md).

## Use it (desktop computer use)

Drive a **persistent** graphical Linux desktop inside a microVM —
screenshot, click, type — the way a computer-use agent expects. A
headless X server (Xvfb) + window manager run in the guest, driven by an
in-guest agent over vsock; no Apple graphics window is involved.

> **Prerequisite — the desktop rootfs image.** Unlike the headless paths, the
> desktop needs a Debian-slim image (Xvfb + openbox + the agent) that does
> **not** ship in the install tarball. From a source checkout, build it with
> `make desktop-image` (needs Docker; builds `linux/amd64` since the guest is
> x86_64-only) → it lands at `assets/vmette-desktop-rootfs.tar` and is
> auto-discovered. Resolution order: `--image` → `$VMETTE_DESKTOP_IMAGE` →
> `assets/vmette-desktop-rootfs.tar` → the `ghcr.io/chamuka-inc/vmette-desktop`
> registry fallback. The headless CLI / MCP tools above do not need this.

```sh
vmetted &                                    # sessions live in the daemon
make desktop-image                           # build the desktop rootfs (Docker; from a checkout)

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
