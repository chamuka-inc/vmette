# vmette CLI reference

```
vmette --kernel PATH --initramfs PATH --rootfs SPEC [options]
vmette providers                                # list registered providers
```

## Required

| Flag | Argument | Description |
|------|----------|-------------|
| `--kernel` | PATH | bzImage on x86_64; vmlinuz from alpine `linux-virt` apk. |
| `--initramfs` | PATH | Initramfs built by `scripts/build-initramfs.sh`. |
| `--rootfs` | SPEC | Rootfs source. Dispatched to a [provider](#rootfs-providers) by the first matching scheme/prefix. |

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

## Snapshot (Apple Silicon only)

| Flag | Argument | Description |
|------|----------|-------------|
| `--build-snapshot` | PATH | Boot, wait for guest READY, pause, save VM state to PATH, exit. |
| `--resume-snapshot` | PATH | Restore from PATH, send `--exec` via vsock, drain output. Requires `--exec`. |
| `--guest-vsock-port` | N | Port the guest's vsock-runner listens on (default 1025). |

On Intel, snapshot flags exit 1 with a clear error pointing at Apple's
`#if defined(__arm64__)` gate.

## Rootfs providers

`--rootfs SPEC` is dispatched to a provider by matching the spec's
prefix or scheme. Order is registration order, first-match-wins. The
shipped CLI registers three:

| Provider | Claims | Examples |
|----------|--------|----------|
| `dir` | absolute paths, `./`, `../`, `~/` | `--rootfs /path/to/rootfs`<br>`--rootfs ./assets/alpine-rootfs`<br>`--rootfs ~/projects/vmette/rootfs` |
| `tar` | `tar+http://`, `tar+https://`, `tar+file://` | `--rootfs tar+https://example.com/rootfs.tar.gz`<br>`--rootfs tar+file:///tmp/rootfs.tar` |
| `oci` | `oci://<ref>`, plus any bare image ref (catch-all) | `--rootfs alpine:3.20`<br>`--rootfs python:3.12-alpine`<br>`--rootfs oci://ghcr.io/foo/bar:tag` |

Run `vmette providers` to print the live registry.

### Provider caches

| Provider | Cache location |
|----------|----------------|
| `dir` | none — your directory is used in place |
| `tar` | `~/Library/Caches/vmette/tar/<sanitized-url>/` |
| `oci` | `~/Library/Caches/vmette/oci/<sanitized-ref>__<digest>/rootfs/` plus `refs/<sanitized-ref>.digest` |

The OCI provider keeps a 1-hour soft TTL on `refs/<ref>.digest` mtime; a
fresh ref entry skips the registry roundtrip entirely. `--offline` short-
circuits that further — no network at all, even for digest verification.

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
vmette --kernel ./assets/vmlinuz-virt --initramfs ./assets/initramfs-vmette \
       --rootfs python:3.12-alpine --exec 'python3 -c "import sys; print(sys.version)"'

# basic with a local rootfs
vmette --kernel ./assets/vmlinuz-virt --initramfs ./assets/initramfs-vmette \
       --rootfs ./assets/alpine-rootfs --exec 'uname -a; exit 0'

# offline cache hit (no network at all)
vmette ... --rootfs alpine:3.20 --offline --exec 'cat /etc/alpine-release'

# tarball over HTTPS, gzip auto-detected
vmette ... --rootfs tar+https://example.com/builds/golden.tar.gz --exec 'make ci'

# extra share + bidirectional file IO
mkdir -p /tmp/scratch
vmette ... --rootfs ./assets/alpine-rootfs \
       --share host=/tmp/scratch --exec 'date > /mnt/host/from-guest.txt'

# pinned vsock port + roundtrip
vmette ... --vsock-port 9000 --exec 'echo hi | vsock-send 9000'

# bounded run
vmette ... --timeout 30 --exec 'long_command_here'

# read-only host share
vmette ... --rootfs-ro --exec 'mount | grep rootfs'
```

## Writing a new provider

The CLI registers Dir/Tar/Oci providers from sibling crates. To add a
fourth, implement `vmette::provider::RootfsProvider` in a new crate and
register it before constructing the CLI app — or fork the CLI and add
your provider to `default_registry()`. See the `vmette-provider-tar`
crate (~150 LOC) for a minimal example.
