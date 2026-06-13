# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `Config`'s mutually-exclusive `rootfs_share: Option<RootfsShare>` /
  `rootfs_block: Option<RootfsBlock>` fields are replaced by a single
  `rootfs: Option<Rootfs>`, where `enum Rootfs { Share(RootfsShare),
  Block(RootfsBlock) }` makes "exactly one root" true by construction. Most
  callers use `Config::set_rootfs_artifact` (unchanged) and the C ABI
  `vmette_config_set_rootfs_share` (unchanged); only code that read/wrote the two
  fields directly needs updating.
- `vmette::run` (and the C ABI `vmette_run`) now **returns** with the guest's exit
  code instead of exiting the process. Library/FFI embedders get a `RunOutput`
  (read its `exit_code`) and choose the process exit code themselves; the `vmette`
  CLI forwards it. Previously the happy path called `std::process::exit` and never
  returned, which was hostile to embedders. `RunOutput` gained an `output` field
  (captured guest output; empty for the interactive `run` path) and is no longer
  `Copy`.
- `RootfsArtifact::Directory` gained an `image_env: Vec<(String, String)>` field
  (the OCI image's declared `Env`, empty for dir/tar rootfses). External
  `RootfsProvider` implementors constructing or exhaustively matching this
  variant must account for the new field. `Config::set_rootfs_artifact` now
  prepends a directory artifact's `image_env` to `Config::env`, so an OCI image's
  declared environment (`PATH`, …) is applied with the caller's `--env`
  overriding on key collisions. Behaviorally equivalent to before for consumers;
  the previous in-rootfs `/.vmette-image-env` guest file is gone — image env now
  reaches the guest through the same channel as `--env`.

### Fixed

- The desktop `navigate` action (and `desktop_navigate`) now actually renders the
  page instead of capturing a blank screen. The desktop image launched Chromium
  with no window-size hint, so it opened a tiny (~10×10) window that Openbox left
  unsized; screenshots came back blank. The image now bakes `--start-maximized`
  into the Chromium flags so the window fills the Xvfb screen. (Desktop-image
  change — rebuild with `scripts/build-desktop-image.sh`.)
- A desktop `exec_capture` or `wait` no longer blocks other actions on the same
  session while it runs. The in-guest agent now drives those two actions
  asynchronously in its event loop, so a slow command (or a long wait) issued
  concurrently with input/screenshots/clipboard no longer stalls them — e.g. a
  settle-screenshot poll alongside an `exec_capture` returns promptly instead of
  waiting for the command. A long `exec_capture` also no longer freezes
  clipboard-paste (X selection) serving. Per-action semantics (output, exit code,
  the `timeout_ms` SIGKILL) are unchanged; replies may now arrive out of order
  (the host already routes them by request id).

## [0.10.0] — 2026-06-13

### Added

- Machine-wide host CA-certificate trust for **every** guest, not just desktops.
  When a CA directory is configured it is mounted into each root the binaries
  boot — `execute`, `fetch_url`, `workspace_run`, the `vmette` CLI one-shot, and
  `desktop_*` — and the guest's PID-1 init installs it into the trust store
  before the workload runs, so HTTPS through a TLS-inspecting proxy / enterprise
  CA succeeds across the board (previously only `desktop_start --ca-certs` did,
  and only for the desktop image). The directory is resolved highest-priority
  first: an explicit `--ca-certs DIR` (now also on `vmette-mcp`), then the
  `VMETTE_CA_CERTS` environment variable, then `~/.config/vmette/certs`. Opt-in:
  with none configured, nothing is mounted and isolation is unchanged.
- `scripts/export-macos-ca-certs.sh`: exports the macOS keychain trust store
  into per-cert PEMs under `~/.config/vmette/certs` (or `$VMETTE_CA_CERTS`), so
  a Mac behind a TLS-inspecting proxy can stage its roots for guests in one
  command. Works on both Apple Silicon and Intel.

## [0.9.0] — 2026-06-10

### Added

- Settle-gated screenshots from the CLI: `vmette desktop screenshot` gains
  `--settle`, `--timeout-ms N`, and `--stable-hold-ms N`, which wait for the
  framebuffer to quiesce before capturing (either tuning flag implies
  `--settle`). Exposes the daemon's existing settle capability that previously
  only the MCP `desktop_screenshot_when_settled` tool could reach.
- Deterministic, shell-free browser navigation for desktop sessions: a new
  `navigate` computer-use action opens a URL by handing it straight to the
  guest's browser launcher (no synthetic keystrokes, no shell — the URL is
  never word-split or interpreted). Exposed as `vmette desktop navigate
  SESSION_ID URL` and the MCP `desktop_navigate` tool. The desktop image ships
  a browser-agnostic `vmette-open` launcher.
- Synchronous in-guest command execution that returns output: a new
  `exec_capture` computer-use action runs a short command to completion and
  returns its combined stdout/stderr plus exit code (the desktop wire
  protocol's `ActionReply` gains an `exit_code` field). Exposed as `vmette
  desktop exec-capture SESSION_ID COMMAND [--timeout-ms N]` and the MCP
  `desktop_exec_capture` tool. Intended for short, terminating commands;
  bounded by a guest-side timeout.
- Desktop CA-certificate injection for browser automation behind TLS-inspecting
  proxies: `vmette desktop start --ca-certs DIR`, MCP `desktop_start.ca_certs`,
  and the desktop wire protocol's optional `shares` field can mount `.crt` /
  `.pem` certificates at `/mnt/certs`; the desktop image installs them into
  Debian trust and Chromium's managed `CACertificates` policy at boot.

### Fixed

- Desktop `get_clipboard` no longer returns a spuriously-empty result on the
  first read right after a copy. A GUI app asserts `CLIPBOARD` ownership
  asynchronously, so a read issued immediately after a Ctrl+C could race ahead
  of that and see "no owner". The guest now retries the selection conversion
  across that ownership handoff (bounded ~1.5s) instead of giving up on the
  first "no owner" reply, so `vmette desktop get-clipboard`, the
  `desktop_get_clipboard` MCP tool, and the `get_clipboard` action read back the
  just-copied text without a manual wait. A genuinely empty clipboard still
  returns empty (after the bounded wait).
- Desktop CA injection now imports **every** certificate in a mounted bundle.
  Previously a multi-certificate `.crt`/`.pem` (a combined bundle or a full
  chain) imported only its first certificate, so an enterprise root behind a
  leaf could silently be skipped.
- Apple Silicon guest support: boot assets, Alpine rootfs, desktop image exports,
  guest helper binaries, and OCI image platform resolution now use the host guest
  architecture (`aarch64` on Apple Silicon, `x86_64` on Intel) with a per-arch
  `assets/<arch>/` layout and legacy flat-asset fallback.

## [0.8.0] — 2026-06-03

### Added

- **Ephemeral ext4 scratch disk** for the guest's writable overlay upper (and
  `/tmp`), so the writable root is bounded by a disk instead of `--mem-mib` —
  lifting the RAM cap that otherwise makes a large build/extract fail with `No
  space left on device`. The image is created sparse per run and deleted on
  teardown — nothing persists. No effect on a read-only rootfs. Exposed across
  every entry point:
  - **CLI**: `vmette --scratch SIZE`, accepting `G`/`g`, `M`/`m`, or a bare
    number of MiB (`8G`, `512M`, `2048`); rejected with `--rootfs-ro`.
  - **Library**: new `Config::scratch_mib: Option<u64>` field.
  - **Daemon wire protocol**: new optional `scratch_mib` (MiB) field on the
    run `Request`.
  - **MCP**: new optional `scratch_mib` (MiB) parameter on the `execute` and
    `workspace_run` tools.
  - **C ABI**: new `vmette_config_set_scratch_mib(cfg, mib)` (`0` disables).

## [0.7.0] — 2026-06-03

### Added

- **`vmette quickstart`** — boots a real hello-world microVM (`alpine:3.20`) to
  verify the install, then prints the next steps (MCP setup for Claude Code, a
  one-off run, a desktop session). One command from install to "it works".
- **`vmette --help` now ends with a docs URL and runnable examples** (the MCP
  one-liner, a one-off `--exec`, `desktop start`, `quickstart`).

### Changed

- **`vmette desktop …` auto-starts `vmetted`** when nothing is listening on the
  default socket, the same lazy-start the MCP server already does — so desktop
  commands work without a manual `vmetted &`. A caller-supplied `--socket` is
  left alone (you manage that daemon). The "is vmetted running?" error is gone
  from the default path.
- **Clearer "daemon closed the connection" error** (CLI and MCP): now names the
  likely cause (crashed / stale build) and the fix (`pgrep vmetted`, restart,
  kill the old PID after a reinstall).

## [0.6.0] — 2026-06-02

### Fixed

- **The mouse pointer is now drawn in desktop screenshots and the live view.**
  `XGetImage` does not include the X cursor (the server draws it as a separate
  overlay), so captures had no pointer — it was invisible in `desktop_view`
  (you saw clicks land but never saw the cursor move) and absent from
  `desktop_screenshot`. The in-guest agent now composites the real cursor
  sprite (its current shape + position, via XFixes) into every captured frame.

## [0.5.0] — 2026-06-02

### Added

- **`--version` / `-V` on all three binaries** (`vmette`, `vmetted`,
  `vmette-mcp`) — prints `<name> <version>` and exits 0.

## [0.4.0] — 2026-06-02

### Added

- **Live desktop view: `desktop_view` (MCP) / `vmette desktop view` (CLI).**
  Open a watchable — and drivable — VNC view of a running desktop session. It
  returns a loopback `vnc://host:port`; point any VNC client at it (on macOS,
  `open vnc://…` launches Screen Sharing). The daemon runs a small built-in RFB
  server that reuses the session's existing capabilities: it streams the screen
  via the `screenshot` action and forwards the viewer's mouse/keyboard back as
  the same computer-use actions the agent uses, so a human and the agent share
  one display. No guest changes — no x11vnc, no extra vsock port. Each session's
  view binds its own ephemeral loopback port (concurrent desktops are
  independent), is loopback-only, idempotent, and torn down with the session.
  The RFB handshake adapts to the client's protocol version (macOS Screen
  Sharing pins to 3.3) and offers VNC Authentication — required by Screen
  Sharing — but does not verify it: type any password, since the loopback bind
  is the access boundary. New `vmetted` wire request
  `DesktopRequest::DesktopView` → `DesktopReply::View { addr }`
  (`vmette-proto::daemon`). See
  [`DESKTOP.md`](docs/DESKTOP.md#live-view-watch--drive-the-desktop).

## [0.3.0] — 2026-06-02

### Changed

- **`vmette-proto::daemon::Request`: `vcpus`, `mem_mib`, `guest_vsock_port`, and
  `vsock_port` are now `Option<…>`** (`None`/omitted = use the `vmette` CLI's
  default), and `Request` gains `to_cli_args()` — the single renderer of a run
  request to the `vmette` argv, shared by `vmetted` and the MCP sandbox so the
  two cannot drift. Behaviour is unchanged (an omitted field resolves to the
  same default it did before), but this is a breaking change for any Rust code
  that constructs or reads those `Request` fields directly. On the `vmetted`
  socket the optional fields are now skipped when unset; sending them still
  works. The one-shot run defaults (1 vCPU, 512 MiB, guest vsock 1025, auto
  host vsock) now live solely in the library `Config` / the CLI.

## [0.2.0] — 2026-06-01

### Added

- **Desktop clipboard: `desktop_get_clipboard` / `desktop_set_clipboard` /
  `desktop_paste` (MCP) and `vmette desktop {get,set}-clipboard / paste`
  (CLI).** The computer-use agent can now read the desktop clipboard exactly
  (instead of OCR'ing a screenshot) and paste text losslessly (instead of
  synthesizing it key-by-key) — a big win for long, non-ASCII, or multi-line
  text. `set` takes ownership of the X `CLIPBOARD` + `PRIMARY` selections in the
  in-guest agent (so paste works via Ctrl+V in GUI apps and Shift+Insert /
  middle-click in terminals); `get` reads `CLIPBOARD`. New `Action::SetClipboard`
  / `Action::GetClipboard` over the agent vsock protocol; the clipboard text
  rides the response payload (like a screenshot's PNG), surfaced as
  `ActionReply.text`.
- **`--env KEY=VALUE` (CLI) / `Config.env` (lib) / `vmette_config_add_env` (C
  ABI).** Set environment variables in the guest before the exec command runs.
  Repeatable. Applied *after* any OCI image `Env`, so `--env` overrides the
  image's values — like `docker run -e`. The caller env rides base64-encoded on
  the kernel cmdline (sharing the exec budget); the guest `/init` applies image
  env then caller env. Caller and image env share one renderer
  (`vmette::render_env_exports`), so their key rules and shell-escaping match.
- **OCI image environment is applied in the guest.** When a rootfs comes from an
  OCI image, the image config's `Env` (notably `PATH`, so `cargo` / `node` /
  etc. are on `PATH`) is now exported before the `--exec` (or MCP `execute` /
  `workspace_run`) command runs — matching how `docker run` applies the image's
  configured env. The OCI provider writes the env into the extracted rootfs and
  the guest `/init` sources it; non-OCI rootfses (dir / tar / squashfs) are
  unaffected. Images cached by an older vmette must be re-pulled to pick this up.

## [0.1.1] — 2026-06-01

### Fixed

- **C ABI: `libvmette.dylib` now carries an `@rpath` install name.** The cdylib
  was stamped with its absolute build-output path as its install name
  (`LC_ID_DYLIB`), so a binary linked against the shipped library failed at
  runtime with `dyld: Library not loaded` on any machine other than the one it
  was built on. It is now `@rpath/libvmette.dylib`; link C consumers with
  `-Wl,-rpath,<dir-holding-the-dylib>` (as `docs/API.md` already shows).

## [0.1.0] — 2026-06-01

Initial release. A headless Linux microVM sandbox for macOS built on Apple's
`Virtualization.framework` — a hardware-isolated guest you can hand to an
untrusted agent, exposed as a CLI, a Rust library, a C ABI, a long-lived daemon,
and an MCP server.

### Added

- **CLI** (`vmette`): thin wrapper over the library. One `--rootfs SPEC`
  flag dispatches to a provider registry; `--rootfs-ro`, `--offline`,
  extra virtio-fs shares, virtio-blk disks, shell command exec,
  virtio-net + NAT, switch_root, configurable vsock port
  (auto/fixed/disabled), timeout (exit 124), vcpus, memory.
  `vmette providers` lists the live registry. `--quiet` suppresses the
  `[vmette]` launcher banner and the `guest stopped`/`timeout` status lines on
  stderr (errors, exit code, and guest stdout are untouched). A
  `vmette desktop` subcommand group (start / screenshot / move / click / type /
  key / scroll / exec / cursor / stop) drives a persistent desktop session via
  `vmetted`.
- **Pluggable rootfs providers** via the `vmette::provider::RootfsProvider`
  trait. `RootfsProvider::provide` / `Registry::resolve` return a
  `RootfsArtifact` (`Directory` for a virtio-fs share | `BlockImage` for a
  read-only block device); `Config::set_rootfs_artifact` wires either form into
  a boot. Four ship by default, in resolution order:
  - `dir` (in `vmette`) — local paths (`/abs`, `./rel`, `~/home`, and bare
    relative dirs that exist on disk). A local dir shadows an OCI image of the
    same name — use `oci://name` to force the image.
  - `squashfs` (in `vmette-provider-squashfs`) — prebuilt squashfs block image
    via `squashfs+file://…`, `squashfs+https://…`, or `squashfs+http://…`. The
    `.sqfs` is attached read-only as virtio-blk slot 0 and the guest overlays a
    tmpfs, so the rootfs is immutable and content-addressable. A block rootfs has
    no host-writable surface, so the guest's exit code is relayed through an
    auto-attached writable `ctl` virtio-fs share (works under chroot and
    `--switch-root`). Remote images are cached with a TTL and a streaming size
    cap (`VMETTE_SQUASHFS_MAX_BYTES`, default 4 GiB); `--offline` resolves from
    cache only.
  - `tar` (in `vmette-provider-tar`) — tarballs over HTTP(S) or local file.
    Gzip / zstd / plain auto-detected by magic bytes. 512 MiB download cap,
    5-min HTTP timeout. Cached at `~/Library/Caches/vmette/tar/<sanitized-url>/`,
    bounded to a size cap (`VMETTE_TAR_CACHE_MAX_BYTES`, default 8 GiB) by LRU
    eviction with an orphan sweep. A `tar+file://` source is re-extracted when
    its mtime is newer than the cached tree, so a local rebuild is picked up on
    the next boot; `--offline` pins to cache.
  - `oci` (in `vmette-provider-oci`) — OCI/Docker images. Cached by manifest
    digest at `~/Library/Caches/vmette/oci/`, 1-hour soft TTL, honours
    `--offline`. Authenticated pulls via an `AuthResolver`
    (env vars → `~/.docker/config.json` → anonymous), so private images
    (e.g. `ghcr.io`) work; `credsStore` / `credHelpers` are not yet supported.
- **Rust library** (`vmette`): `Config` builder, `vmette::run()` one-shot entry
  point, the `Session` primitive with a `WorkloadStrategy` (`OneShot` | `Agent`)
  for long-lived VMs, thiserror-based `Error` enum, arch-gated snapshot module,
  `provider::{Registry, Context, RootfsProvider}` for embedders.
- **C library** (`libvmette.dylib` + `libvmette.a`): opaque-pointer
  C ABI generated by cbindgen from `src/ffi.rs`; header at
  `include/vmette.h` checked in. cbindgen is gated behind the off-by-default
  `regenerate-header` feature, so consumers compile neither it nor `syn`.
- **Daemon** (`vmetted`): long-lived UNIX-socket dispatcher with tokio +
  line-delimited JSON protocol. Stateless requests spawn a vmette subprocess and
  stream stdout/stderr/exit back; a stateful desktop registry holds live
  `vmette::Session` VMs across requests (max-live cap, 30-min idle eviction,
  background sweep), backing the `desktop_start` / `desktop_action` /
  `desktop_stop` protocol kinds. Structured JSON logs via tracing-subscriber.
- **MCP server** (`vmette-mcp`): Model Context Protocol server using `rmcp` 1.7.
  Exposes `execute`, `fetch_url`, a `workspace_*` family (direct subprocess
  path), and a `desktop_*` family (routed through `vmetted` for persistence)
  over stdio. Plugs into Claude Desktop, Cursor, Cline, Zed, and any other
  MCP-aware host. Per-session workspace state with a cap; `--allow-network`
  gate; `O_NOFOLLOW`-and-symlink-walk path safety in
  `workspace_read`/`workspace_write`. Guest exec output is marker-bracketed so
  the server returns exactly the command's stdout/stderr (boot/teardown noise
  stripped, CRLF normalised to LF); `desktop_screenshot` returns an MCP image
  content block.
- **Desktop computer use.** A persistent graphical Linux desktop (Xvfb +
  openbox, software-rendered) inside a microVM, driven via screenshots +
  synthetic mouse/keyboard — the computer-use agent loop. The `Action`
  vocabulary mirrors Anthropic computer use (screenshot, mouse_move, clicks,
  type, key chords, scroll, exec, …). Highlights:
  - **`desktop_launch`** — application-agnostic "start a GUI app and return its
    first painted frame": backgrounds the command (stdio → guest log so a chatty
    app can't block before painting), waits for the screen to change and then
    settle, and returns that frame. Carries no app-specific knowledge — the
    software-GL browser flags live in the desktop image (`/etc/chromium.d/`), so
    a bare `chromium <url>` renders.
  - **Settle-and-hold readiness.** `screenshot_when_settled` (and so
    `desktop_launch` / `desktop_screenshot_when_settled`) requires the screen to
    stay *continuously* settled for a hold window, bridging the
    chrome-then-content gap of a network-bound app; a spinner or other small
    persistent animation is treated as a moving region and settle resolves
    *around* it. Optional `stable_hold_ms` on the settle request.
  - **`desktop_drag` and `desktop_middle_click`** join the click family —
    text selection, sliders, drag-and-drop, drawing.
  - **`desktop_what_changed`** crops the PNG to the changed bounding box
    (10–50× fewer bytes than the full framebuffer for a typical local change).
  - **Per-session isolation.** A directory/OCI/tar rootfs is mounted read-only
    on the host and overlaid with a per-session tmpfs in the guest, so writes
    (a chromium profile, DHCP-written `/etc/resolv.conf`, any file) are
    discarded on shutdown and never bleed across sessions. Explicit `--share`
    mounts stay writable.
  - **Desktop rootfs as an auto-discovered boot asset.** The desktop image
    follows the same client-side discovery as the kernel / initramfs:
    explicit `--image` → `$VMETTE_DESKTOP_IMAGE` → discovered
    `assets/vmette-desktop-rootfs.tar` (`tar+file://`) → `ghcr.io/…` registry
    fallback; `vmette` and `vmette-mcp` pass a concrete spec to the daemon.
    `make desktop-image` builds `images/vmette-desktop/` from source and exports
    it to `assets/vmette-desktop-rootfs.tar`. New
    `vmette_assets::{find, default_desktop_image}`. The desktop image ships a
    UTF-8 locale, suppresses Chromium's crash-restore bubble, and passes
    `--test-type` to drop the `--no-sandbox` infobar.
  - New [`docs/DESKTOP.md`](docs/DESKTOP.md).
- **Guest tooling**: custom `/init` (busybox-applet bootstrap, virtio
  module load, virtiofs mounts, NAT DHCP, chroot/switch_root, exit
  code propagation), `vsock-send` (AF_VSOCK client, ~25 KB static
  musl), `vsock-runner` (snapshot-mode cmd server, ~30 KB), and
  `vmette-desktop-agent` (XTEST input + XGetImage capture + stb PNG). The
  string-typing path types in non-reusing keycode segments and maps newline/tab
  to Return/Tab, so arbitrarily long, diverse, multi-line text types verbatim.
- **Asset pipeline**: scripts to fetch the alpine netboot initramfs +
  `linux-virt` kernel apk, repack the initramfs with the custom `/init`,
  and cross-compile guest helpers via musl-cross.
- **Universal binary build**: `make universal` produces fat
  x86_64+arm64 binaries via cargo + lipo. `make build` codesigns all three
  binaries (`vmette`, `vmetted`, `vmette-mcp`) so a plain build boots a VM.
- **Distribution**: `make dist` packs a tarball; `scripts/install.sh`
  is the curl-pipe installer; `.github/workflows/release.yml` builds
  + uploads on tag push.
- **Tests**: cargo unit + integration tests across the workspace, plus an
  end-to-end smoke runner that boots a real microVM per gate.

### Known limitations

- Snapshot/restore is Apple-Silicon-only (gated by Apple's SDK).
  Intel hosts get `VmetteStatus::SnapshotUnsupported`.
- `vmetted`'s warm-snapshot pool is deferred to a future release; stateless
  requests spawn a vmette subprocess per request.
- Guest assets are currently x86_64-only. The arm64 path is documented
  but unverified.
- Desktop sessions are software-rendered (no GPU) — fine for agentic GUI
  control and UI testing, not video/WebGL/3D.

[Unreleased]: https://github.com/chamuka-inc/vmette/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/chamuka-inc/vmette/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/chamuka-inc/vmette/releases/tag/v0.1.0
