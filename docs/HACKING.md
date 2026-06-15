# Hacking on vmette

## Toolchain

- macOS 11+ (the Virtualization.framework availability floor; CI runs on
  `macos-14`)
- Xcode Command Line Tools (`xcode-select --install`)
- Rust stable + cross targets:
  ```sh
  rustup target add x86_64-apple-darwin aarch64-apple-darwin
  ```
- Prebuilt Linux musl cross toolchains for the guest helpers and the static
  desktop agent (its X client stack is built from source against them — no
  Docker):
  ```sh
  brew install messense/macos-cross-toolchains/x86_64-unknown-linux-musl
  brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
  ```
  (messense ships prebuilt tarballs that install in seconds; the older
  `FiloSottile/musl-cross` compiles GCC from source, which is far slower.)

## First build

```sh
make build                  # cargo build --release + codesign vmette/vmetted/vmette-mcp
make assets init guest-bin  # assets: pull kernel/initramfs/rootfs · init: repack initramfs · guest-bin: build vsock helpers
make test                   # cargo tests + end-to-end VM smoke
```

`make run` boots a one-shot guest with a default probe; pass a custom command
via `bash scripts/run.sh 'echo hi'`.

`desktop start` defaults to the published image
`ghcr.io/chamuka-inc/vmette-desktop:latest`
(`vmette_assets::DEFAULT_DESKTOP_IMAGE`), pulled automatically on first use — no
build required. The agent is host-injected, so any GUI rootfs (Xvfb + a window
manager) works — bring your own via `--image <ref>` / `$VMETTE_DESKTOP_IMAGE`;
`images/vmette-desktop/` is the reference recipe.

Building the desktop image is **optional** (needs Docker) — only to customize the
rootfs or republish the default:

```sh
make desktop-image                      # local tar+file:// rootfs (host arch) at assets/<arch>/
scripts/build-desktop-image.sh --push   # republish the default — full amd64+arm64 manifest
```

A bare `--push` rebuilds both architectures into one manifest (arm64 under qemu),
so a publish can't leave one arch stale. See
[DESKTOP.md](DESKTOP.md#bring-your-own-desktop-rootfs) for the full recipe.

## Cutting a release

```sh
make release VERSION=0.5.0 DRY_RUN=1   # preflight + gates + plan; change nothing
make release VERSION=0.5.0             # cut it (prompts before the irreversible publish)
make release VERSION=0.5.0 YES=1       # skip the prompt (unattended)
```

`scripts/release.sh` runs the whole playbook as a pipeline:

- **preflight** — on `main`, clean tree, in sync with origin, version newer, tag
  free, `[Unreleased]` non-empty, crates.io creds present.
- **bump + CHANGELOG** — lockstep bump of the workspace version and all 8
  internal `path` pins (the 7 published libs plus the `publish = false`
  `vmette-daemon-client`); the script's progress line counts only the 7 it
  publishes. Then `cargo update -w` and promote CHANGELOG
  `[Unreleased]` → `[VERSION]`.
- **gates** — `fmt` / `clippy -D warnings` / `test`.
- **commit + tag** — the `release: vX.Y.Z` commit + tag (local, reversible).
- **confirm** — pause for an explicit `y` (skipped with `YES=1`) before the
  irreversible steps.
- **publish** — `cargo publish` of the 7 published libs in dep order.
- **push** — `main` + tag push, which fires `release.yml` (tarball/GitHub
  Release).

Everything before publish is local and reversible; a declined or failed run
leaves the commit + tag for inspection (undo with
`git reset --hard HEAD~1 && git tag -d vX.Y.Z`).

## Layout reminder

```
crates/vmette/                   library (rlib + cdylib + staticlib): VZ wrapper, Config, run(), Session, C ABI
crates/vmette-proto/             leaf wire contracts (serde only): agent + daemon protocol types
crates/vmette-providers/         aggregator exposing default_registry() (Dir→Squashfs→Tar→Oci)
crates/vmette-assets/            shared boot-asset (kernel + initramfs) discovery
crates/vmette-cli/               `vmette` CLI binary
crates/vmette-daemon/            `vmetted` UNIX-socket daemon + stateful desktop registry + live VNC view (RFB)
crates/vmette-daemon-client/     sync transport for the `vmetted` desktop socket (shared by the CLI + MCP server)
crates/vmette-mcp/               `vmette-mcp` MCP server for AI agents
crates/vmette-provider-oci/      OCI/Docker image rootfs provider
crates/vmette-provider-squashfs/ squashfs block-image rootfs provider
crates/vmette-provider-tar/      tarball rootfs provider
guest/                           C sources cross-compiled with musl for the alpine guest
images/vmette-desktop/           reference recipe for the published desktop image (+ the injected agent's run script)
scripts/                         asset pipeline + dev wrappers + installer
tests/                           cargo unit tests live in-crate; smoke runner here
```

## Building the library for non-Rust consumers

```sh
cargo build --release -p vmette
# Produces (release profile sets `strip = true`, so these are stripped):
#   target/release/libvmette.dylib   (cdylib)
#   target/release/libvmette.a       (staticlib)
#   target/release/libvmette.rlib    (Rust-callable)
# Header is regenerated only under the `regenerate-header` feature
# (cargo build -p vmette --features regenerate-header, or `make header`):
#   crates/vmette/include/vmette.h
```

## Universal binary

```sh
make universal              # cargo build for both targets + lipo + codesign
```

Output at `target/universal/release/{vmette,vmetted,vmette-mcp,libvmette.dylib}`.
The three binaries (`vmette`, `vmetted`, `vmette-mcp`) are all codesigned with
the virtualization entitlement.

## Distribution tarball

```sh
make dist                   # universal build + bundle into dist/*.tar.gz
```

Override the version with `VERSION=v0.1.0 make dist`. Defaults to
`git describe --tags --abbrev=0` (or `v0.1.0-dev` if no tag).

## Codesigning

VZ refuses to run without `com.apple.security.virtualization`. The
canonical entitlements are in `entitlements.plist`.

The Makefile ad-hoc signs (`codesign --sign -`) which is sufficient
for local use. For a distributed tarball/brew bottle/PKG installer you
need a Developer ID ($99/yr) and notarization; the shipped releases are
ad-hoc signed.

Any cargo invocation invalidates the existing signature, so the
smoke runner re-codesigns unconditionally. If you see
> Invalid virtual machine configuration. The process doesn't have the
> "com.apple.security.virtualization" entitlement.

re-run `codesign --sign - --force --entitlements entitlements.plist --options=runtime target/release/vmette`.

## Asset pipeline

`scripts/fetch-assets.sh` pulls:
- Alpine netboot `initramfs-virt` (busybox + base tree source)
- Alpine `linux-virt` apk (kernel version resolved dynamically from the Alpine
  3.20 APKINDEX) — matched kernel + complete module tree including vsock +
  virtiofs

`scripts/build-initramfs.sh` extracts busybox from the netboot
initramfs, swaps in the apk's module tree, injects
`scripts/custom-init.sh` as `/init`, and repacks via `cpio + gzip`.

`scripts/build-vsock-send.sh` cross-compiles `guest/vsock-send.c` and
`guest/vsock-runner.c` statically with musl, drops them at
`assets/<arch>/alpine-rootfs/usr/local/bin/`.

## Guest architectures

The asset pipeline follows the host architecture by default: Apple Silicon maps
to Alpine `aarch64`, Intel maps to `x86_64`. Override with `ARCH=x86_64` or
`ARCH=aarch64` when you need to build a different guest set.

Per-arch assets live under `assets/{x86_64,aarch64}/`. Runtime discovery checks
the matching per-arch directory under each search root.

## Trusting a host CA in every guest

Behind a TLS-inspecting proxy (or with an enterprise CA), HTTPS from inside a
guest fails with `CERTIFICATE_VERIFY_FAILED` because the intercepting root lives
only on the host. Stage that root where vmette looks for it and **every** root
the binaries boot — `execute`, `fetch_url`, `workspace_run`, the `vmette` CLI
one-shot, and `desktop_*` — trusts it:

```bash
# macOS: export the keychain trust store into per-cert PEMs (Apple Silicon + Intel)
scripts/export-macos-ca-certs.sh            # → ~/.config/vmette/certs
```

Resolution order (highest first): an explicit `--ca-certs DIR` flag, the
`VMETTE_CA_CERTS` environment variable, then `~/.config/vmette/certs`. When a
directory resolves, the host mounts it as the `certs` virtio-fs share and the
guest's PID-1 init (`scripts/custom-init.sh`) installs it before the workload
runs: it appends the PEMs to the image's existing system trust bundle(s) and
drops them into the distro anchor dirs, running `update-ca-certificates` /
`update-ca-trust` opportunistically when present. The mechanism is
distro-agnostic. It is **opt-in**: with no directory configured, nothing is
mounted and guest isolation is unchanged. The same step runs for the desktop
image too, which layers its own Chromium managed-policy (`CACertificates`) on
top of this shared system-trust step.

After editing `scripts/custom-init.sh`, rebuild the initramfs
(`bash scripts/build-initramfs.sh`) — the live `initramfs-vmette` embeds a copy.

## Common issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| `error: vm.start failed: ... not permitted on this machine` | binary not codesigned with entitlement | re-sign per above |
| Smoke test "exit code N" always passes as exit 1 | binary unsigned, errors out before booting | re-run `bash tests/run.sh` (always re-signs) |
| `vsock-send: ... failed: Connection reset` | host VsockLogger not registered for that port | ensure `--vsock-port` matches; auto mode wires automatically |
| Snapshot ops error 5 (`SnapshotUnsupported`) | running on Intel | move to Apple Silicon; Apple gates the API |
| `libvmette.dylib` not found at runtime | C consumer didn't set rpath | add `-Wl,-rpath,${PREFIX}/lib` at link time |

## CI

`.github/workflows/ci.yml` runs on every push to `main` and on pull requests
(macos-14 runner — vmette is macOS-only):
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. **C header is up to date** — rebuilds `crates/vmette/include/vmette.h` under
   the `regenerate-header` feature and fails on any `git diff`

`.github/workflows/release.yml` runs on tag push (`v*`):
1. macos-14 runner, Rust stable + both cross targets, plus the prebuilt messense
   `x86_64-`/`aarch64-unknown-linux-musl` toolchains (for the static guest
   helpers + desktop agent)
2. `make universal` → fat binaries
3. `make dist` → tarball + SHA256SUMS
4. `softprops/action-gh-release` uploads artifacts + `scripts/install.sh`

This pipeline is live: releases are cut through `scripts/release.sh` (latest
`v0.10.0`), with the library crates also published to crates.io.
