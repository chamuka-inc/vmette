# vmette CLI reference

Not running an agent? `vmette` runs a one-off command inside a fresh,
hardware-isolated microVM and propagates its exit code to the host — the same
sandbox the MCP server hands to agents, as a one-liner.

```
vmette --rootfs SPEC [--kernel PATH] [--initramfs PATH] [options]
vmette providers                                # list registered providers
vmette desktop <command> [options]              # drive a persistent desktop session
vmette --version                                # print version (also -V)
```

## Required

| Flag | Argument | Description |
|------|----------|-------------|
| `--rootfs` | SPEC | Rootfs source. Dispatched to a [provider](#rootfs-providers) by the first matching scheme/prefix. |

## Boot assets

`--kernel` and `--initramfs` are optional and auto-discovered. When
omitted, vmette searches, in order: `$VMETTE_ASSETS_DIR/<arch>`,
`./assets/<arch>` (repo checkout), and `<install-prefix>/assets/<arch>`
(the `assets` dir that is a sibling of the binary's `bin/`). The release
tarball ships both under `<prefix>/assets/<arch>`, so a `curl | install.sh`
install boots with no asset flags.

| Flag | Argument | Description |
|------|----------|-------------|
| `--kernel` | PATH | vmlinuz from the alpine `linux-virt` apk. Default: discovered `vmlinuz-virt`. |
| `--initramfs` | PATH | Initramfs built by `scripts/build-initramfs.sh`. Default: discovered `initramfs-vmette`. |

## Rootfs

| Flag | Argument | Description |
|------|----------|-------------|
| `--rootfs-ro` | — | Mount the rootfs share read-only. Disables exit-code propagation (guest can't write `/.vmette-exit`). |
| `--offline` | — | Forbid network access. Cache miss surfaces as an immediate failure; useful on flaky networks or air-gapped environments. Applied to whichever provider resolves the spec. |

## Workload

| Flag | Argument | Description |
|------|----------|-------------|
| `--share` | TAG=PATH | Extra virtio-fs mount at `/mnt/<TAG>` in the guest. Repeatable. |
| `--disk` | PATH | Raw block image attached as virtio-blk. Repeatable. |
| `--scratch` | SIZE | Ephemeral ext4 **scratch disk** used as the guest's writable overlay upper, so the writable root and `/tmp` are bounded by this disk instead of `--mem-mib`. Sizes accept `G`/`g` (GiB), `M`/`m` (MiB), or a bare number of MiB: `8G`, `512M`, `2048`. vmette materializes a sparse image per run and deletes it on teardown (nothing persists). No effect with `--rootfs-ro` (no writable overlay). Without this flag the overlay is a RAM-backed tmpfs — fine for light work, but a big build/extract that exceeds RAM will `No space left on device`. |
| `--env` | KEY=VALUE | Export an env var in the guest before `--exec`. Repeatable. Applied **after** any OCI image `Env`, so it overrides the image's value (like `docker run -e`). Carried base64-encoded on the cmdline (shares the ~3000-char budget with `--exec`), so keep it modest. |
| `--exec` | CMD | Shell command to run in the guest, then `poweroff -f`. Encoded as base64 in `vmette.exec=<b64>` on the kernel cmdline (~3000 char limit). |
| `--net` | — | Attach virtio-net with NAT. `/init` runs `udhcpc` on eth0. |
| `--switch-root` | — | Use `switch_root` instead of `chroot` for the exec environment. Cleaner PID-1 (useful for systemd-style workloads). |

## Runtime

| Flag | Argument | Description |
|------|----------|-------------|
| `--timeout` | N | Force-stop the VM after N seconds; host exits 124. |
| `--cmdline` | STR | Override the base kernel cmdline. Default: `console=hvc0 quiet`. |
| `--vsock-port` | N | `-1`: disable vsock device entirely. `0`: auto-pick 50000–59999 (default). `>0`: explicit port. The chosen port is exported into the guest's exec env as `VMETTE_VSOCK_PORT`. |
| `--vcpus` | N | Default 1. |
| `--mem-mib` | N | Default 512. |
| `--quiet` | | Suppress the `[vmette]` launcher banner and the `guest stopped`/`timeout` status lines on stderr. Errors are still printed, the exit code is unchanged, and guest console output on stdout is untouched. Useful when scripting or capturing output (the MCP server passes this internally). |

## Snapshot (Apple Silicon only)

| Flag | Argument | Description |
|------|----------|-------------|
| `--build-snapshot` | PATH | Boot, wait for guest READY, pause, save VM state to PATH, exit. |
| `--resume-snapshot` | PATH | Restore from PATH, send `--exec` via vsock, drain output. Requires `--exec`. |
| `--guest-vsock-port` | N | Port the guest's vsock-runner listens on (default 1025). |

On Intel, snapshot flags exit 1 with a clear error pointing at Apple's
`#if defined(__arm64__)` gate.

## Desktop sessions (`vmette desktop`)

`vmette desktop` is a thin client for the persistent computer-use sessions
held by `vmetted` — it talks to the daemon's UNIX socket, so `vmetted` must be
running first. It exists for manual end-to-end testing without an MCP host.

```
vmette desktop start [--image REF] [--size WxH] [--net] [--offline]
                     [--kernel PATH] [--initramfs PATH]   boot a desktop; prints SESSION_ID
vmette desktop screenshot SESSION_ID --out FILE [--settle]   capture the framebuffer to a PNG
                          [--timeout-ms N] [--stable-hold-ms N]   (--settle waits for the screen to quiesce)
vmette desktop cursor      SESSION_ID                     print the pointer position
vmette desktop move        SESSION_ID X Y                 move the pointer
vmette desktop click       SESSION_ID X Y                 left-click at X Y
vmette desktop double-click SESSION_ID X Y                double left-click at X Y
vmette desktop right-click SESSION_ID X Y                 right-click at X Y
vmette desktop type        SESSION_ID TEXT                type a string
vmette desktop key         SESSION_ID CHORD              press a chord, e.g. 'ctrl+c'
vmette desktop set-clipboard SESSION_ID TEXT             put TEXT on the clipboard
vmette desktop get-clipboard SESSION_ID                  print the clipboard contents
vmette desktop paste       SESSION_ID TEXT               set clipboard then Ctrl+V
vmette desktop scroll      SESSION_ID X Y DIR AMOUNT      scroll (DIR: up|down|left|right)
vmette desktop exec        SESSION_ID COMMAND             launch a shell command in the guest
vmette desktop exec-capture SESSION_ID COMMAND [--timeout-ms N]   run a command and print its output
vmette desktop navigate    SESSION_ID URL                 open URL in the desktop browser (no shell)
vmette desktop view        SESSION_ID                     open a live VNC view; prints vnc://HOST:PORT
vmette desktop stop        SESSION_ID                     tear the session down
```

`view` opens a live, loopback-only VNC view of the session and prints a
`vnc://127.0.0.1:PORT` URL — open it with `open vnc://…` (macOS Screen Sharing)
or any VNC client to watch and drive the desktop. It is per-session (each gets
its own ephemeral port) and idempotent. See
[`DESKTOP.md`](DESKTOP.md#live-view-watch--drive-the-desktop).

Global: `--socket PATH` overrides the daemon socket (default
`~/Library/Caches/vmette/vmette.sock`). See [`DESKTOP.md`](DESKTOP.md) for the
session model and the MCP-facing tools.

## Rootfs providers

`--rootfs SPEC` is dispatched to a provider by matching the spec's
prefix or scheme. Order is registration order, first-match-wins. The
shipped CLI registers four:

| Provider | Claims | Examples |
|----------|--------|----------|
| `dir` | absolute paths, `./`, `../`, `~/` | `--rootfs /path/to/rootfs`<br>`--rootfs ./assets/aarch64/alpine-rootfs`<br>`--rootfs ~/projects/vmette/rootfs` |
| `squashfs` | `squashfs+http://`, `squashfs+https://`, `squashfs+file://` | `--rootfs squashfs+file:///tmp/base.sqfs`<br>`--rootfs squashfs+https://example.com/base.sqfs` |
| `tar` | `tar+http://`, `tar+https://`, `tar+file://` | `--rootfs tar+https://example.com/rootfs.tar.gz`<br>`--rootfs tar+file:///tmp/rootfs.tar` |
| `oci` | `oci://<ref>`, plus any bare image ref (catch-all) | `--rootfs alpine:3.20`<br>`--rootfs python:3.12-alpine`<br>`--rootfs oci://ghcr.io/foo/bar:tag` |

Run `vmette providers` to print the live registry.

The `dir`/`tar`/`oci` providers deliver a host **directory** shared over
virtio-fs. The `squashfs` provider instead returns a **block image**: the
`.sqfs` is attached read-only as virtio-blk slot 0 (`/dev/vda`) and the
guest mounts it under a tmpfs overlay, so the rootfs is immutable and the
same base can back many concurrent sessions. Because a block rootfs has no
host-writable surface, exit-code propagation rides a small auto-attached
`ctl` virtio-fs share instead of `/.vmette-exit` on the rootfs.

### Provider caches

| Provider | Cache location |
|----------|----------------|
| `dir` | none — your directory is used in place |
| `squashfs` | `squashfs+file://` used in place; `squashfs+http(s)://` cached at `~/Library/Caches/vmette/squashfs/<key>.sqfs` |
| `tar` | `~/Library/Caches/vmette/tar/<sanitized-url>/` |
| `oci` | `~/Library/Caches/vmette/oci/<sanitized-ref>__<digest>/rootfs/` plus `refs/<sanitized-ref>.digest` |

The OCI provider keeps a 1-hour soft TTL on `refs/<ref>.digest` mtime; a
fresh ref entry skips the registry roundtrip entirely. `--offline` short-
circuits that further — no network at all, even for digest verification.
The squashfs provider applies the same offline rule and downloads remote
images with a streaming size cap (`VMETTE_SQUASHFS_MAX_BYTES`, default
4 GiB). The tar provider has the equivalent cap on extracted size
(`VMETTE_TAR_MAX_BYTES`).

### Private OCI registries

The OCI provider resolves credentials per-registry host, in precedence
order: env vars, then `~/.docker/config.json`, then anonymous.

| Var | Effect |
|-----|--------|
| `VMETTE_OCI_TOKEN` | Password/token; sent as `Basic(<user>, token)`. |
| `VMETTE_OCI_USER` | Username paired with `VMETTE_OCI_TOKEN` (default `vmette`). |
| `VMETTE_OCI_AUTH_<HOST>` | Per-host `user:secret` override (e.g. `VMETTE_OCI_AUTH_GHCR_IO`), checked before `VMETTE_OCI_TOKEN`. |

`~/.docker/config.json` is read for `auths[registry].auth` (base64
`user:pass`) only — `credsStore` / `credHelpers` (external credential
binaries) are not supported. With no match, pulls stay anonymous.

Guest helpers (`vsock-send`, `vsock-runner`) are injected into the
extracted rootfs at `/usr/local/bin/` automatically by the OCI and tar
providers, so vsock workflows work uniformly across image sources. The
DirProvider does not touch your directory.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Guest exited 0 (or interactive shell ended cleanly). |
| 1 | Library/runtime error (config invalid, VM start failed, snapshot unsupported, rootfs resolution failed, etc). |
| 2 | CLI usage error. |
| 124 | `--timeout` reached. |
| _N_ | Guest's exit code (1–123 propagated verbatim from the workload). |

Note: `--switch-root --rootfs-ro --exec CMD` is rejected at parse time
(exit 2). The combination would panic the guest — there's no writable
place for `/init` to stage the wrapper script that `switch_root` needs
to exec.

## Guest environment

The guest's exec environment (passed via `/init`) sets:

| Var | Value |
|-----|-------|
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `VMETTE_VSOCK_PORT` | Host port the vsock listener bound to (empty if `--vsock-port -1`). |
| `VMETTE_GUEST_VSOCK_PORT` | Set in snapshot-server mode only. |

## Examples

```sh
# pull a public OCI image and run a command in it
# (kernel + initramfs auto-discovered; pass --kernel/--initramfs to override)
vmette --rootfs python:3.12-alpine --exec 'python3 -c "import sys; print(sys.version)"'

# basic with a local rootfs
vmette --rootfs ./assets/$(uname -m | sed 's/arm64/aarch64/')/alpine-rootfs --exec 'uname -a; exit 0'

# offline cache hit (no network at all)
vmette ... --rootfs alpine:3.20 --offline --exec 'cat /etc/alpine-release'

# tarball over HTTPS, gzip auto-detected
vmette ... --rootfs tar+https://example.com/builds/golden.tar.gz --exec 'make ci'

# prebuilt squashfs block image (read-only base + tmpfs overlay)
vmette ... --rootfs squashfs+file:///tmp/base.sqfs --exec 'cat /etc/os-release'

# private OCI image (token via env)
VMETTE_OCI_TOKEN=ghp_xxx vmette ... --rootfs oci://ghcr.io/me/private:tag --exec '/run.sh'

# extra share + bidirectional file IO
mkdir -p /tmp/scratch
vmette ... --rootfs ./assets/aarch64/alpine-rootfs \
       --share host=/tmp/scratch --exec 'date > /mnt/host/from-guest.txt'

# pinned vsock port + roundtrip
vmette ... --vsock-port 9000 --exec 'echo hi | vsock-send 9000'

# bounded run
vmette ... --timeout 30 --exec 'long_command_here'

# read-only host share
vmette ... --rootfs-ro --exec 'mount | grep rootfs'
```

## Writing a new provider

The CLI registers Dir/Squashfs/Tar/Oci providers from sibling crates. To
add another, implement `vmette::provider::RootfsProvider` in a new crate
and register it before constructing the CLI app — or fork the CLI and add
your provider to `default_registry()`. A provider's `provide()` returns a
`RootfsArtifact` (`Directory` for a virtio-fs share, `BlockImage` for a
block device); see the `vmette-provider-tar` crate (~150 LOC) for a
minimal directory example.
