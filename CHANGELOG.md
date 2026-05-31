# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Block-image rootfs (squashfs).** Providers can now hand back a prebuilt
  block image instead of a directory. A `.sqfs` is attached read-only as
  virtio-blk slot 0 and the guest builds a tmpfs overlay over it, so the rootfs
  is immutable and content-addressable.
  - **New `vmette-provider-squashfs` crate.** `--rootfs squashfs+file://…`,
    `squashfs+https://…`, or `squashfs+http://…`. Remote images are cached with
    a TTL and downloaded with a streaming size cap
    (`VMETTE_SQUASHFS_MAX_BYTES`, default 4 GiB); `--offline` resolves from cache
    only.
  - **`RootfsArtifact` provider seam.** `RootfsProvider::provide` /
    `Registry::resolve` now return a `RootfsArtifact` (`Directory` |
    `BlockImage`); `Config::set_rootfs_artifact` wires either form into a boot.
  - **Control-share exit channel.** A block rootfs has no host-writable surface,
    so the guest's exit code is relayed through an auto-attached writable `ctl`
    virtio-fs share; works under both chroot and `--switch-root`.
- **OCI registry authentication.** The OCI provider gained an `AuthResolver`
  (env vars → `~/.docker/config.json` → anonymous), so private images
  (e.g. `ghcr.io`) can be pulled with a username + token. `credsStore` /
  `credHelpers` are not yet supported.
- **Desktop computer use.** vmette can now run a persistent graphical
  Linux desktop inside a microVM and drive it via screenshots +
  synthetic mouse/keyboard — the computer-use agent loop.
  - **`vmette::Session` primitive** with a `WorkloadStrategy` (OneShot |
    Agent). The headless one-shot `run()` is now a thin OneShot wrapper
    over the same primitive; behaviour is unchanged. Agent sessions run
    on a per-session serial dispatch queue (multiple concurrent VMs) and
    expose `Send` `SessionClient` / `StopHandle` handles.
  - **Framed vsock protocol** (`crates/vmette/src/desktop.rs`):
    `[u32 LE header_len][header JSON][optional payload]`, with an
    `Action` vocabulary mirroring Anthropic computer use (screenshot,
    mouse_move, clicks, type, key chords, scroll, exec, …). Screenshots
    return a PNG payload.
  - **Guest desktop agent** (`guest/vmette-desktop-agent.c`): XTEST for
    input, XGetImage + PNG encode for capture, served over vsock. Ships
    inside a Debian-slim desktop rootfs image (Xvfb + openbox), built by
    `scripts/build-desktop-image.sh`; `custom-init.sh` gained a
    `vmette.desktop=1` branch.
  - **Daemon session registry** (`vmette-daemon`): a stateful subsystem,
    kept separate from the stateless subprocess dispatch, that holds live
    sessions across requests with a max-live cap, idle eviction (30 min),
    and shutdown teardown. New `desktop_start` / `desktop_action` /
    `desktop_stop` request kinds.
  - **CLI**: a `vmette desktop` subcommand group (start / screenshot /
    move / click / type / key / scroll / exec / cursor / stop) talking to
    `vmetted` for manual end-to-end testing.
  - **MCP**: `desktop_*` tools routed through the daemon;
    `desktop_screenshot` returns an MCP image content block. The
    `execute` / `workspace_*` tools keep their direct-subprocess path.
  - **`desktop_launch` MCP tool.** Application-agnostic "start a GUI app and
    return its first painted frame": backgrounds the command (stdio → guest
    log so a chatty app can't block before painting), waits for the screen to
    change and then settle, and returns that frame. Carries no app-specific
    knowledge — the software-GL browser flags live in the desktop image
    (`/etc/chromium.d/`), so a bare `chromium <url>` renders.
  - **Settle-and-hold readiness.** `screenshot_when_settled` (and so
    `desktop_launch` and `desktop_screenshot_when_settled`) now requires the
    screen to stay *continuously* settled for a hold window, not just reach a
    single settled frame. A network-bound app — a browser that paints its chrome
    and then sits on a blank page while it fetches — read as "settled" mid-load
    and `desktop_launch` could hand back the half-loaded frame; the hold bridges
    that chrome-then-content gap (content painting re-opens the settle), while a
    video/spinner stays excluded as churn and never blocks it. New optional
    `stable_hold_ms` on the settle request; `desktop_launch` uses a larger hold
    than the per-action default. Also: `--test-type` added to the desktop
    image's Chromium flags to drop the `--no-sandbox` warning infobar from every
    captured frame.
  - New [`docs/DESKTOP.md`](docs/DESKTOP.md).

### Changed

- **Single owner per wire contract (internal restructure).** Extracted a new
  leaf crate **`vmette-proto`** (serde only) that owns every cross-crate wire
  shape: the guest computer-use vocabulary (`Action`, `ResponseHeader`,
  `ScrollDirection`), the `vmetted` UNIX-socket protocol (the run
  `Request`/`Frame` and the desktop `DesktopRequest`/`DesktopReply` tagged
  enums), `Rect`, and `ShareMount`. The daemon, MCP server, and CLI all build
  and parse these as the *same* Rust types — the hand-rolled `json!()` desktop
  clients and the triplicated rectangle type are gone, so a renamed field or new
  `kind` is now a compile error rather than a silent runtime break. The socket
  bytes are unchanged.
- **Provider order has one home.** New **`vmette-providers`** crate exposes
  `default_registry()` (DirProvider→Squashfs→Tar→Oci); the CLI and the daemon's
  desktop registry both call it instead of each hand-building the load-bearing,
  first-match-wins order.
- **Core trimmed to VZ mechanism.** The pixel-`settle` perception module moved
  out of the "lean" core (`crates/vmette`) into `vmette-daemon`, its only
  consumer; core `vmette` is now VZ + `Session` plus a thin re-export of the
  proto types.
- **`tar+file://` cache follows the file.** The tar provider keys its cache on
  the URL, so a local archive rebuilt in place under the same path used to be
  masked by the prior extraction until the 1-hour TTL lapsed. The cache is now
  invalidated when the source file's mtime is newer than the cached extraction,
  so a `tar+file://` rebuild is picked up on the next boot. http(s) URLs are
  unchanged (TTL governs); `--offline` still pins to cache.

### Fixed

- **`desktop_type` corrupted long / multi-line input.** The guest agent's
  string-typing path (`guest/vmette-desktop-agent.c`) bound each distinct
  character to a scratch keycode, but reused the last keycode as a per-keystroke
  "overflow" slot — so any string with more distinct characters than free
  keycodes clobbered a bound character and emitted wrong glyphs (e.g.
  `/root/demo.txt` typed as `/r--t/demm.txt`). It now types in **segments**,
  binding each segment's distinct characters up front and never reusing a
  keycode mid-string, so arbitrarily long/diverse text types verbatim. Newlines
  and tabs in typed text now map to Return/Tab (`cp_to_keysym`) instead of the
  raw `0x0A`/`0x09` keysyms, so multi-line input (e.g. a here-doc) advances
  lines as intended.

## [0.1.0] — 2026-05-29

Initial release. Local Linux microVM sandbox for macOS built on
Apple's `Virtualization.framework`.

### Added

- **CLI** (`vmette`): thin wrapper over the library. One `--rootfs SPEC`
  flag dispatches to a provider registry; `--rootfs-ro`, `--offline`,
  extra virtio-fs shares, virtio-blk disks, shell command exec,
  virtio-net + NAT, switch_root, configurable vsock port
  (auto/fixed/disabled), timeout (exit 124), vcpus, memory.
  `vmette providers` lists the live registry.
- **Pluggable rootfs providers** via the `vmette::provider::RootfsProvider`
  trait. Three ship by default:
  - `dir` (in `vmette`) — local paths (`/abs`, `./rel`, `~/home`)
  - `oci` (in `vmette-provider-oci`) — OCI/Docker images. Cached by
    manifest digest at `~/Library/Caches/vmette/oci/`. 1-hour soft TTL
    on `refs/<ref>.digest` mtime. Honours `--offline`. Anonymous
    registry auth only in v0.1.
  - `tar` (in `vmette-provider-tar`) — tarballs over HTTP(S) or local
    file. Gzip / zstd / plain auto-detected by magic bytes. 512 MiB
    download cap, 5-min HTTP timeout. Cached at
    `~/Library/Caches/vmette/tar/<sanitized-url>/`.
- **Rust library** (`vmette`): `Config` builder, `vmette::run()` entry
  point, thiserror-based `Error` enum, arch-gated snapshot module,
  `provider::{Registry, Context, RootfsProvider}` for embedders.
- **C library** (`libvmette.dylib` + `libvmette.a`): opaque-pointer
  C ABI generated by cbindgen from `src/ffi.rs`; header at
  `include/vmette.h` checked in.
- **Daemon** (`vmetted`): long-lived UNIX-socket dispatcher with
  tokio + line-delimited JSON protocol; spawns vmette as a subprocess
  per request. Request schema uses `rootfs` (string) +
  `rootfs_ro` / `offline` flags. Structured JSON logs via
  tracing-subscriber.
- **MCP server** (`vmette-mcp`): Model Context Protocol server using
  `rmcp` 1.7. Exposes seven tools (`execute`, `fetch_url`, plus a
  `workspace_*` family) over stdio. Plugs into Claude Desktop, Cursor,
  Cline, Zed, and any other MCP-aware host. Per-session workspace
  state with a cap; `--allow-network` gate; `O_NOFOLLOW`-and-symlink-
  walk path safety in `workspace_read`/`workspace_write`.
- **Guest tooling**: custom `/init` (busybox-applet bootstrap, virtio
  module load, virtiofs mounts, NAT DHCP, chroot/switch_root, exit
  code propagation), `vsock-send` (AF_VSOCK client, ~25 KB static
  musl), `vsock-runner` (snapshot-mode cmd server, ~30 KB).
- **Asset pipeline**: scripts to fetch alpine netboot initramfs +
  `linux-virt-6.6.141-r0.apk`, repack initramfs with custom `/init`,
  cross-compile guest helpers via musl-cross.
- **Universal binary build**: `make universal` produces fat
  x86_64+arm64 binaries via cargo + lipo.
- **Distribution**: `make dist` packs a tarball; `scripts/install.sh`
  is the curl-pipe installer; `.github/workflows/release.yml` builds
  + uploads on tag push.
- **Tests**: cargo unit tests (21 across vmette + providers) +
  16-gate end-to-end smoke runner that boots a real microVM per gate.

### Known limitations

- Snapshot/restore is Apple-Silicon-only (gated by Apple's SDK).
  Intel hosts get `VmetteStatus::SnapshotUnsupported`.
- vmetted's warm-snapshot pool is deferred to a future release; v0.1
  spawns a vmette subprocess per request.
- Guest assets are currently x86_64-only. arm64 path is documented
  but unverified.

[Unreleased]: https://github.com/chamuka-inc/vmette/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/chamuka-inc/vmette/releases/tag/v0.1.0
