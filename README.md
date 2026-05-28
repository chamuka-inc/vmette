# vmette

A local Linux microVM sandbox for macOS, built on Apple's
Virtualization.framework.

> **Status:** mid-port. Phase 1 (foundation + rename) complete. The
> working host implementation is still ObjC (`vmette/main.m`); a Rust
> port (`crates/vmette/` + `crates/vmette-cli/` + `crates/vmette-daemon/`)
> is in progress. See `/Users/p.munaawa/.claude/plans/logical-hugging-porcupine.md`
> for the phased plan. A full README rewrite lands in Phase 8.

## Quick start

```sh
make run                                           # default probe command
bash scripts/run.sh 'exit 42'                      # exit code → host
bash scripts/run.sh --net 'wget -O - http://example.com | head -5'
bash scripts/run.sh 'echo hi | vsock-send $VMETTE_VSOCK_PORT'
bash scripts/run.sh --switch-root 'cat /proc/1/comm'
bash scripts/run.sh --timeout 3 'sleep 30'         # → exit 124
bash scripts/run.sh --ro-rootfs-share 'mount | head -1'
```

Wall time for a no-op: ~1 s.

## Layout (Phase 1)

```
vmette/                   ObjC host implementation (to be ported to Rust)
  main.m                  VZ config, vsock listener, snapshot flow
  vsock-send.c            static-musl AF_VSOCK client (guest)
  vsock-runner.c          snapshot-mode command server (guest)
  entitlements.plist      com.apple.security.virtualization + .hypervisor
scripts/
  fetch-assets.sh         alpine netboot initramfs + linux-virt apk
  fetch-alpine-rootfs.sh  alpine-minirootfs → assets/alpine-rootfs/
  build-initramfs.sh      repack initramfs: busybox + apk modules + custom /init
  build-vsock-send.sh     cross-compile vsock-send + vsock-runner
  custom-init.sh          PID-1: depmod, modprobe, virtiofs, net, chroot/switch_root
  run.sh                  compile + sign + run with sensible defaults
tests/fixtures/share/     stable host dir used as --share target
Makefile                  help | assets | init | guest-bin | run | shell | clean
```

## Notes

- Snapshot/restore code path is present but Apple gates the VZ
  save/restore APIs to Apple Silicon (`#if defined(__arm64__)` in the
  SDK). On Intel hosts, passing `--build-snapshot` / `--resume-snapshot`
  exits with a clear error.
- Networking, vsock, switch_root, RO rootfs, timeout, exit-code
  propagation, custom initramfs are all wired and verified on Intel.
