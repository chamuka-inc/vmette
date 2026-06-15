# vmette

**Run your coding agent on your Mac — without the anxiety.**

Claude Code, Cursor, Cline, and friends `pip install` whatever a README names,
execute model output, and act on web pages that can carry prompt injection. Run
that straight on your laptop and the agent has your files, your tokens, and your
network. vmette gives that work somewhere safe to happen instead: a real,
hardware-isolated Linux VM that boots in ~1 second, sees only what you share in,
and disappears when it's done. Send the agent's untrusted work there — or lock
the agent down so the VM is its *only* way to run code — and nothing it executes
ever touches your real machine.

<p align="center">
  <img src="vmette-demo.gif" alt="vmette booting a Linux guest, propagating its exit code to the host, and enforcing default-deny networking until --net is passed" width="800">
</p>

It's built on Apple's `Virtualization.framework`: the boundary is a hypervisor with
its own kernel, not a container sharing yours. And it's a Model Context Protocol
server, so any MCP-aware agent host gets a sandboxed machine with one line of config.

## Why on-device

|                      | Cloud sandbox (E2B, Vercel, Modal…) | Container (Docker)     | **vmette**                          |
| -------------------- | ----------------------------------- | ---------------------- | ----------------------------------- |
| Isolation boundary   | microVM / gVisor (varies)           | shared host kernel     | **hardware VM, its own kernel**     |
| Where it runs        | someone else's cloud                | your machine           | **your Mac**                        |
| Your code & secrets  | leave the device                    | stay local             | **stay local**                      |
| Network egress       | on by default (policies optional)   | on by default          | **off until you pass `--net`**      |
| Cost                 | usage-metered (per-second / CPU)    | free                   | **free, on-device**                 |
| Boot time            | sub-second + a network round-trip   | ~sub-second            | **~1 second, local**                |

The cloud sandboxes are just as well-isolated — the difference is keeping that
isolation on-device: no round-trip, no meter.

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
sandboxed machine as a set of tools (`execute`, `fetch_url`, `workspace_*`, `desktop_*`). Work the
agent runs *through these tools* happens inside the VM, confined as above. Note it
*adds* the sandbox alongside the host's own tools; in Claude Code the agent still has
native Bash that runs on your Mac, so to make the VM its *only* way to run code, restrict
those too (e.g. deny the Bash tool).

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

Each `execute` and `workspace_run` call boots a fresh microVM; a workspace is a
persistent host directory mounted into each run, so files survive across calls
(`workspace_create`/`write`/`read`/`destroy` are host-side directory ops).

Full tool reference, per-host configs, and the security model:
[`docs/MCP.md`](docs/MCP.md).

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

> **The desktop rootfs.** The desktop needs a GUI rootfs — an X server (Xvfb) and a
> window manager. You don't have to build one: `vmette desktop start` auto-pulls the
> published default on first use (no Docker), and the computer-use agent is
> host-injected (a static binary vmette ships), so any GUI image works. See
> [`docs/DESKTOP.md`](docs/DESKTOP.md) for the resolution order and bring-your-own recipe.

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
run pulls + extracts the image (python:3.12-alpine ≈ 30 s); subsequent runs are cache hits
(~3 s), cached at `~/Library/Caches/vmette/oci/`.

One `--rootfs` flag, four sources — a local directory, an OCI ref, a tarball URL, or a
squashfs block image. List the providers with `vmette providers`:

```sh
vmette --rootfs ./assets/aarch64/alpine-rootfs      --exec 'uname -a'
vmette --rootfs alpine:3.20                         --exec 'cat /etc/alpine-release'
vmette --rootfs oci://ghcr.io/foo/bar:v1            --exec '/run-tests.sh'
vmette --rootfs tar+https://h/builds/r.tar.gz       --exec 'make ci'
vmette --rootfs tar+file:///tmp/local-rootfs.tar    --exec 'ls /'
vmette --rootfs squashfs+file:///tmp/base.sqfs      --exec 'ls /'
```

Network is off until you ask (`--net`), virtio-fs shares only the host dirs you name,
and the rootfs can attach read-only. Private OCI registries authenticate via env vars
or `~/.docker/config.json` (`VMETTE_OCI_AUTH_<HOST>=user:secret` or `VMETTE_OCI_TOKEN`). Full flag list: `vmette --help` or
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
2. The kernel command line carries only `vmette.boot=ctl` (plus `vmette.vsock_port`
   when vsock is on). Everything per-invocation — exec (base64 as `VMETTE_EXEC_B64`),
   env, rootfs mode, shares, scratch device, switch-root, and net — travels in a typed
   `boot.env` envelope written to a `ctl` virtio-fs share. The guest's `/init`
   ([`scripts/custom-init.sh`](scripts/custom-init.sh)) sources that envelope in pure
   shell, mounts virtio-fs shares, brings up the network if requested, then `chroot` /
   `switch_root` into the rootfs and runs the command.
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
use vmette::provider::Context;
use vmette::Config;

fn main() {
    // The standard registry, in the load-bearing resolution order the CLI and
    // daemon use. To customize, hand-build one instead:
    //   use vmette::provider::{DirProvider, Registry};
    //   Registry::new().with(DirProvider::new()).with(/* … */);
    let registry = vmette_providers::default_registry();
    let ctx = Context::new(vmette_assets::default_cache_root()); // $HOME/Library/Caches/vmette
    let artifact = registry.resolve("alpine:3.20", &ctx).unwrap();

    let mut cfg = Config::new("./assets/aarch64/vmlinuz-virt", "./assets/aarch64/initramfs-vmette");
    cfg.set_rootfs_artifact(artifact, false);
    cfg.exec_cmd = Some("echo hello from rust; exit 42".into());

    // run() blocks until guest poweroff and process-exits with the guest's code.
    let _ = vmette::run(&cfg);
}
```

```toml
[dependencies]
vmette           = "0.10"
vmette-providers = "0.10"  # default_registry(); pulls in the oci/tar/squashfs providers
vmette-assets    = "0.10"  # default_cache_root() + boot-asset discovery helpers
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
sends one request object, daemon streams `stdout` frames (guest stdout and stderr
combined) followed by a terminal `exit` frame. Useful
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
- **Guest assets are architecture-specific.** Apple Silicon uses Alpine
  `aarch64`; Intel uses `x86_64`. Runtime discovery checks the per-arch
  `assets/<arch>/` directory under each search root.
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
