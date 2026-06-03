# vmette

**Run your coding agent on your Mac — without the anxiety.**

Claude Code, Cursor, Cline, and friends `pip install` whatever a README names,
execute model output, and act on web pages that can carry prompt injection. Run
that straight on your laptop and the agent has your files, your tokens, and your
network. vmette gives that work somewhere safe to happen instead: a real,
hardware-isolated Linux VM that boots in ~1 second, sees only what you share in,
and disappears when it's done. Send the agent's untrusted work there — or lock
the agent down so the VM is its *only* way to run code — and nothing it executes
can reach your files, tokens, or network. Your code and secrets never leave the
device.

<p align="center">
  <img src="vmette-demo.gif" alt="vmette booting a Linux guest, propagating its exit code to the host, and enforcing default-deny networking until --net is passed" width="800">
</p>

It's built on Apple's `Virtualization.framework`: the boundary is a hypervisor with
its own kernel, not a container sharing yours. Default-deny — no host filesystem and
no network until you explicitly grant them. Ephemeral — each run is a fresh guest, so
nothing persists. And it's a Model Context Protocol server, so any MCP-aware agent
host gets a sandboxed machine with one line of config.

## Why on-device

|                      | Cloud sandbox (E2B, Vercel, Modal…) | Container (Docker)     | **vmette**                          |
| -------------------- | ----------------------------------- | ---------------------- | ----------------------------------- |
| Isolation boundary   | microVM / gVisor (varies)           | shared host kernel     | **hardware VM, its own kernel**     |
| Where it runs        | someone else's cloud                | your machine           | **your Mac**                        |
| Your code & secrets  | leave the device                    | stay local             | **stay local**                      |
| Network egress       | on by default (policies optional)   | on by default          | **off until you pass `--net`**      |
| Cost                 | usage-metered (per-second / CPU)    | free                   | **free, on-device**                 |
| Boot time            | sub-second + a network round-trip   | ~sub-second            | **~1 second, local**                |

The cloud sandboxes are fast and well-isolated — they boot the same kind of microVM in
milliseconds. The catch isn't speed; it's that the work runs on someone else's machine,
reached over the network, with a usage meter running across a long agent loop. A
container is local and free but shares your kernel — one namespace away from the host.
vmette is the on-device option that keeps the real isolation without the round-trip.

## Install

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
```

Installs to `~/.local/share/vmette/`, symlinks `~/.local/bin/{vmette,vmetted,vmette-mcp}`.
macOS-only (any version with VZ — i.e. 11+; tested on 14.7 Intel).

<details>
<summary>Or build from source</summary>

```sh
git clone https://github.com/chamuka-inc/vmette
cd vmette
make build              # cargo build + codesign
make test               # cargo unit + end-to-end VM smoke
```
</details>

## Give your agent a sandbox (MCP)

`vmette-mcp` is a Model Context Protocol server that hands any MCP-aware agent host a
sandboxed machine as a set of tools (`execute`, `workspace_*`, `desktop_*`). Work the
agent runs *through these tools* happens inside the VM — never on your host filesystem
(unless you share a directory in), with no network egress (unless you start the server
with `--allow-network`). Note it *adds* the sandbox alongside the host's own tools; in
Claude Code the agent still has native Bash that runs on your Mac, so to make the VM
its *only* way to run code, restrict those too (e.g. deny the Bash tool). See
[`docs/MCP.md`](docs/MCP.md).

**Claude Code** — one command, no config file:

```sh
claude mcp add vmette --scope user -- vmette-mcp --allow-network
```

**Claude Desktop, Cursor, Cline, Zed, Goose** — point the host at the `vmette-mcp`
command. JSON example (Claude Desktop's
`~/Library/Application Support/Claude/claude_desktop_config.json`):

```jsonc
{ "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": ["--default-image", "python:3.12-alpine", "--allow-network"]
}}}
```

The server ships an `execute` tool, `fetch_url`, a `workspace_*` family (each call
boots a fresh microVM), and a `desktop_*` family for computer use.

Per-host setup snippets (Claude Code, Claude Desktop, Cursor, Cline, Zed, Goose) plus
the full tool reference and security model: [`docs/MCP.md`](docs/MCP.md).

## Give your agent a desktop (computer use)

Drive a **persistent** graphical Linux desktop inside a microVM — screenshot, click,
type — the way a computer-use agent expects. A headless X server (Xvfb) + window
manager run in the guest, driven by an in-guest agent over vsock; no Apple graphics
window is involved.

```sh
vmetted &                                    # sessions live in the daemon
SID=$(vmette desktop start)                  # first run pulls the desktop rootfs from ghcr
vmette desktop screenshot "$SID" --out shot.png && open shot.png
vmette desktop exec "$SID" 'xterm &'
vmette desktop click "$SID" 640 400
vmette desktop type  "$SID" 'echo hello'
open "$(vmette desktop view "$SID")"        # watch & drive it live over VNC
vmette desktop stop  "$SID"
```

You can **watch — and take over —** a session live: `vmette desktop view` (or the
`desktop_view` MCP tool) returns a loopback `vnc://host:port` you open with any VNC
client (macOS Screen Sharing via `open vnc://…`). The daemon streams the screen and
forwards your mouse/keyboard as the same actions the agent uses, so a human and the
agent share one display. The same capability is exposed to agents through the MCP
`desktop_*` tools (`desktop_screenshot` returns a PNG image block).

> **The desktop rootfs.** The desktop needs a Debian-slim image (Xvfb + openbox + the
> agent) — a separate, larger rootfs from the headless paths. You don't have to build
> it: `vmette desktop start` pulls the published image from
> `ghcr.io/chamuka-inc/vmette-desktop` automatically on first use (then cached under
> `~/Library/Caches/vmette/oci/`), so the MCP and CLI desktop paths work out of the
> box — no Docker needed. Building locally is optional, for hacking on the image or
> working offline: `make desktop-image` (needs Docker; builds `linux/amd64` since the
> guest is x86_64-only) writes `assets/vmette-desktop-rootfs.tar`, which is
> auto-discovered and **takes precedence** over the registry so a dev session reflects
> your source. Resolution order: `--image` → `$VMETTE_DESKTOP_IMAGE` → local
> `assets/vmette-desktop-rootfs.tar` → the `ghcr.io/chamuka-inc/vmette-desktop` image.

See [`docs/DESKTOP.md`](docs/DESKTOP.md) for the session lifecycle, protocol, action
reference, and image build.

## Run a one-off command (CLI)

Not running an agent? The same sandbox is a one-liner. Pull an OCI image and run a
command in it:

```sh
vmette --rootfs python:3.12-alpine \
       --exec 'python3 -c "print(2**32)"; exit 0'
```

The exit code propagates to the host. The kernel and initramfs are auto-discovered
(the release tarball ships them under `$PREFIX/assets`; from a checkout vmette finds
`./assets`). Override with `--kernel` / `--initramfs` or `$VMETTE_ASSETS_DIR`. First
run pulls + extracts the image (alpine:3.20 ≈ 30 s); subsequent runs are cache hits
(~3 s), cached at `~/Library/Caches/vmette/oci/`.

One `--rootfs` flag, four sources — a local directory, an OCI ref, a tarball URL, or a
squashfs block image. List the providers with `vmette providers`:

```sh
vmette --rootfs ./assets/alpine-rootfs              --exec 'uname -a'
vmette --rootfs alpine:3.20                         --exec 'cat /etc/alpine-release'
vmette --rootfs oci://ghcr.io/foo/bar:v1            --exec '/run-tests.sh'
vmette --rootfs tar+https://h/builds/r.tar.gz       --exec 'make ci'
vmette --rootfs tar+file:///tmp/local-rootfs.tar    --exec 'ls /'
vmette --rootfs squashfs+file:///tmp/base.sqfs      --exec 'ls /'
```

Network is off until you ask (`--net`), virtio-fs shares only the host dirs you name,
and the rootfs can attach read-only. Private OCI registries authenticate via env vars
or `~/.docker/config.json` (`VMETTE_OCI_TOKEN`). Full flag list: `vmette --help` or
[`docs/CLI.md`](docs/CLI.md).

The writable root is a RAM-backed overlay by default, so a heavy build or extract can
outgrow `--mem-mib` and hit `No space left on device`. Add `--scratch SIZE` (e.g.
`--scratch 8G`) to back it with an ephemeral ext4 disk instead — sized independently of
RAM, created sparse per run, and discarded on teardown:

```sh
vmette --rootfs rust:1.80 --net --mem-mib 1024 --scratch 8G \
       --share src=$PWD --exec 'cd /mnt/src && cargo build'
```

## How it works

1. `vmette` builds a `VZVirtualMachineConfiguration` (kernel, initramfs, virtio
   devices, vsock).
2. The kernel command line carries `vmette.exec=<base64(cmd)>` plus `vmette.*` flags.
   The guest's `/init` ([`scripts/custom-init.sh`](scripts/custom-init.sh)) parses
   them in pure shell, mounts virtio-fs shares, brings up the network if requested,
   then `chroot` / `switch_root` into the rootfs and runs the command.
3. After the command exits, the guest writes the code to `.vmette-exit`, syncs, and
   `poweroff -f`. VZ fires the lifecycle delegate; the host reads the file and exits
   with that code.
4. An immutable squashfs rootfs attaches read-only as virtio-blk with a tmpfs overlay,
   so the base stays content-addressable and shareable across sessions.

## Embed it

vmette is also a library. The same VM primitive is available as a Rust crate, a C-ABI
dynamic library, and a long-lived daemon — for building your own agent host or sandbox
tooling on top.

<details>
<summary><b>Rust library</b></summary>

The library accepts a directory path; resolution from a spec (OCI ref, tarball URL, …)
goes through the provider registry first.

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
    let artifact = registry.resolve("alpine:3.20", &ctx).unwrap();

    let mut cfg = Config::new("./assets/vmlinuz-virt", "./assets/initramfs-vmette");
    cfg.set_rootfs_artifact(artifact, false);
    cfg.exec_cmd = Some("echo hello from rust; exit 42".into());

    // run() blocks until guest poweroff and process-exits with the guest's code.
    let _ = vmette::run(&cfg);
}
```

```toml
[dependencies]
vmette                   = "0.2"
vmette-provider-oci      = "0.2"
vmette-provider-tar      = "0.2"  # optional
vmette-provider-squashfs = "0.2"  # optional
```

See [`crates/vmette/examples/minimal.rs`](crates/vmette/examples/minimal.rs) and
[`docs/API.md`](docs/API.md).
</details>

<details>
<summary><b>C ABI</b></summary>

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

```sh
cc -I include -L lib -lvmette -Wl,-rpath,lib -o demo demo.c
```

The `-Wl,-rpath,lib` matters: `libvmette.dylib` has the install name
`@rpath/libvmette.dylib`, so the binary needs an rpath pointing at the directory that
holds the dylib. The header is auto-generated from `crates/vmette/src/ffi.rs` via
cbindgen and checked in at `crates/vmette/include/vmette.h`. See
[`crates/vmette/examples/minimal.c`](crates/vmette/examples/minimal.c) and
[`docs/API.md`](docs/API.md).
</details>

<details>
<summary><b>Daemon (vmetted)</b></summary>

```sh
vmetted &
```

Listens on `~/Library/Caches/vmette/vmette.sock`. Speaks line-delimited JSON: client
sends one request object, daemon streams `stdout` / `stderr` / `exit` frames. Useful
for amortizing per-invocation cost or driving many runs from a long-lived caller; it
also owns the stateful desktop session registry and the live VNC view.

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

See [`docs/DAEMON.md`](docs/DAEMON.md).
</details>

## Constraints

- **macOS only.** VZ is Apple-private. No Linux/Windows port planned.
- **Guest assets are currently x86_64-only.** The repack pipeline references
  `linux-virt-x86_64.apk`. arm64 plumbing is documented in
  [`docs/HACKING.md`](docs/HACKING.md); verification awaits arm64 hardware.
- **Snapshot/restore is Apple-Silicon-only.** Apple gates the save/restore calls
  behind `#if defined(__arm64__)`. On Intel, `--build-snapshot` / `--resume-snapshot`
  return `VmetteStatus::SnapshotUnsupported`. The daemon's snapshot-warm-pool is a
  planned optimization, not yet implemented.
- **Desktop sessions are software-rendered and live in the daemon.** Headless Xvfb
  (no GPU); each session is a ~2 GB VM, capped and idle-evicted. Fine for agentic GUI
  control and UI testing, not for video / WebGL / 3D. See
  [`docs/DESKTOP.md`](docs/DESKTOP.md).

## Docs

- [`docs/MCP.md`](docs/MCP.md) — vmette-mcp server tool reference + client configs
- [`docs/DESKTOP.md`](docs/DESKTOP.md) — desktop computer use: sessions, protocol, image build
- [`docs/CLI.md`](docs/CLI.md) — full flag reference
- [`docs/API.md`](docs/API.md) — Rust + C library API
- [`docs/DAEMON.md`](docs/DAEMON.md) — vmetted protocol spec
- [`docs/HACKING.md`](docs/HACKING.md) — build, test, debug, repo layout
- [`CHANGELOG.md`](CHANGELOG.md) — release notes

## License

MIT. See [LICENSE](LICENSE).
</content>
</invoke>
