# vmette — project instructions

`vmette` is a **headless Linux microVM sandbox for macOS**, built on Apple's
Virtualization.framework (VZ) via the `objc2-virtualization` bindings. It boots
a Linux guest in ~1s and ships as a CLI, a Rust library, a C ABI (cbindgen), a
long-lived daemon, and an MCP server for AI agents. macOS-only by design.

## Checks (run before considering work done)

This repo has **no `make ci` target**. Run the cargo commands directly:

```bash
cargo fmt --all --check                  # formatting (use `cargo fmt --all` to fix)
cargo clippy --workspace --all-targets   # lint — keep at ZERO warnings
cargo test --workspace                   # unit + integration tests
```

All three must be clean. `make test` additionally runs the end-to-end VM smoke
(`tests/run.sh`, ~20 gates) — it boots real VMs, so it requires a codesigned
binary and macOS.

## Workspace layout

Eleven crates (workspace `version = 0.1.0`, edition 2021, `rust-version = 1.80`, MIT):

| Crate | Purpose |
|-------|---------|
| `crates/vmette` | Core library (lib + cdylib + staticlib). VZ wrapper + `Session` only; `Config`, `run()`, providers, C ABI. Depends on `vmette-proto`. |
| `crates/vmette-proto` | **Leaf** (serde only, no workspace deps). The wire contracts: `agent::{Action, ResponseHeader, ScrollDirection}` (guest vsock), `daemon::{Request, Frame, DesktopRequest, DesktopReply, …}` (vmetted socket), `geom::Rect`, `ShareMount`. One Rust type per wire shape → drift is a compile error. |
| `crates/vmette-providers` | Aggregator exposing `default_registry()` (DirProvider→Squashfs→Tar→Oci, the single load-bearing order). Used by the CLI + daemon so both resolve specs identically. |
| `crates/vmette-assets` | Shared boot-asset (kernel + initramfs) discovery for the binaries. |
| `crates/vmette-cli` | `vmette` CLI binary. Hand-rolled arg parsing (no clap — keeps the binary small). |
| `crates/vmette-daemon` | `vmetted` — UNIX-socket dispatcher (tokio). In-process stateless run lane + stateful desktop registry; owns the pixel-`settle` perception module. |
| `crates/vmette-daemon-client` | Sync transport for the `vmetted` desktop socket (connect / lazy-auto-spawn / line framing) — the single owner, shared by the CLI (directly) and the MCP server (via `spawn_blocking`). |
| `crates/vmette-mcp` | MCP server (`vmette-mcp`) exposing vmette to AI agents over stdio. |
| `crates/vmette-provider-oci` | OCI/Docker image rootfs provider (catch-all for bare refs + `oci://`); per-registry `AuthResolver` for private images. |
| `crates/vmette-provider-squashfs` | Squashfs block-image rootfs provider (`squashfs+file://`, `squashfs+https://`, `squashfs+http://`). Returns `BlockImage`. |
| `crates/vmette-provider-tar` | Tarball rootfs provider (`tar+https://`, `tar+http://`, `tar+file://`). |

## Core library — `crates/vmette/src/`

- **`lib.rs`** — public API: `Config`, `run()`, `Session`, `WorkloadStrategy`
  (`OneShot` | `Agent`), `RootfsArtifact` (`Directory` | `BlockImage`) +
  `Config::set_rootfs_artifact`, `VsockPort`, `RootfsShare`, `ShareMount`,
  `RunOutput`.
- **`session.rs`** — the **`Session` primitive**: owns a live booted VM + its
  private VZ dispatch queue + teardown. `Session` is `!Send` (holds objc2
  `Retained`), so it runs on its own OS thread and hands out the `Send` handles
  `SessionClient` (desktop requests) and `StopHandle` (graceful stop).
  `SessionEnd` = Exited/TimedOut/Stopped/Error.
- **`lifecycle.rs`** — `run()` is a thin **OneShot** wrapper over `Session`:
  raw-mode + signal handlers, start, block on terminal event, exit with the
  guest's code. Snapshot build/resume dispatch lives here too.
- **`provider.rs`** — `RootfsProvider` trait (`provide` returns a
  `RootfsArtifact`: `Directory` for a virtio-fs share or `BlockImage` for a
  read-only block device) + `Registry` dispatcher + `Context` (cache root,
  offline flag, optional guest-helpers dir). Built-in `DirProvider`;
  OCI/tar/squashfs live in sibling crates.
- **`desktop.rs`** — the framed vsock **codec** `[u32 LE req_id][u32 LE header_len][JSON header][optional binary payload]`
  (pure, no VZ/objc2). The `req_id` prefix (C4) lets the host demultiplexer
  (`session.rs::Demux`) route responses to the right caller; the guest agent
  echoes it. The `Action`/`ResponseHeader`/`ScrollDirection` *types* it carries
  live in `vmette-proto` and are re-exported here (and as `vmette::Action` etc.);
  `req_id` is framing, not a proto type. The pixel-settle perception module is
  **not** here — it moved to the daemon, its only consumer.
- **`cmdline.rs`** — assembles the kernel cmdline. After the typed-boot-contract
  refactor it emits only `vmette.boot=ctl` (telling `/init` to source the
  `boot.env` envelope) plus `vmette.vsock_port` when vsock is on; everything else
  (exec, env, rootfs mode, shares, scratch device, switch-root, net, workload)
  travels in **`boot.rs`**'s `BootParams` envelope written to the `ctl` share.
- **`boot.rs`** — the host↔guest **boot contract** codec. `to_env` serializes the
  typed `vmette_proto::boot::BootParams` to the `KEY=VALUE` `boot.env` envelope
  `Session` writes to the `ctl` virtio-fs share; the guest `/init` sources it.
  `BootParams` (versioned by `BOOT_PROTO_VERSION`) replaces the old `vmette.*`
  cmdline tokens; `from_config` maps a `Config` to it.
- **`ffi.rs`** — the C ABI (cbindgen → `include/vmette.h`). Opaque
  `#[repr(C)]` handles, paired `*_new`/`*_free`, `VmetteStatus` i32 codes. Every
  fn is `unsafe extern "C"`; safety contracts documented at the module level + a
  short per-fn `# Safety` section (clippy `missing_safety_doc`).
- **`error.rs`** — unified `Error` enum (InvalidConfig, StartFailed,
  RestoreFailed, SaveFailed, SnapshotUnsupported, Timeout, Vsock, Io).
- **`vz/`** — the objc2 bindings: `config.rs` (VM configuration builder),
  `delegate.rs` (lifecycle delegate, exit-code capture), `snapshot.rs`
  (save/restore — **arm64-only**, `cfg(target_arch = "aarch64")`), `vsock.rs`
  (host-side listener; `Echo` mode for snapshot/one-shot, `Agent` mode hands the
  accepted fd to the `Session`).

## Daemon — `crates/vmette-daemon/src/`

Two **deliberately separate** subsystems:

- **Stateless dispatch** (`main.rs`): the per-request path boots a one-shot
  capture-aware `vmette::Session` **in-process** (`run_workload_inproc`), via
  `Config::from_run_request`, and streams the guest's clean captured output back
  as `Frame::Stdout`/`Frame::Exit` over the socket — no forked subprocess, no
  console marker-scraping. It peeks `kind`: a `desktop_*` request deserializes
  into the typed `vmette_proto::daemon::DesktopRequest` enum and is matched (no
  stringly second dispatch); everything else is the untagged run `Request`.
- **Stateful desktop registry** (`registry.rs`): holds live `vmette::Session`
  VMs (Agent workload) in-process so a desktop persists across many requests.
  Resolves rootfs images via `vmette_providers::default_registry()`. Guardrails:
  max-live cap (8), idle eviction (30 min), background sweep (60 s). Each session
  on its own thread; VM-control hops off the async runtime via `spawn_blocking`.
  Keep statefulness contained here — do **not** interleave it into the clean
  dispatch path.
- **`settle.rs`**: tile-based pixel-settle detection (distilled from
  x11vnc/TurboVNC/Playwright/pixelmatch). Pure pixel math, no VZ/objc2; the
  registry feeds it decoded screenshot frames to decide when the screen has
  quiesced and which regions are still moving. Lives here because the daemon is
  its only consumer (`Rect` comes from `vmette-proto`).

## MCP server — `crates/vmette-mcp/src/`

`server.rs` registers tools: `execute`, `fetch_url`, `workspace_*` (booted
**in-process** by `sandbox.rs` via a capture-aware `vmette::Session` — the MCP
server itself carries the virtualization entitlement), and `desktop_*` (routed
through `vmetted` via `daemon_client.rs`, since persistence requires the daemon). `daemon_client.rs`
builds requests and parses replies as `vmette-proto` types (no hand-rolled
`json!`), so a protocol change is a compile error here. `desktop_screenshot`
returns an MCP image content block. `--allow-network` gates outbound network.

## Assets, guest, scripts

- **`assets/`** (downloaded/built, gitignored): `vmlinuz-virt`, the alpine
  netboot initramfs, the repacked `initramfs-vmette`, and `alpine-rootfs/`.
- **`guest/`** (C, cross-compiled for Linux x86_64): `vsock-send.c`,
  `vsock-runner.c` (snapshot-mode helpers, static musl, injected into the
  initramfs) and `vmette-desktop-agent.c` (computer-use agent: XTEST input +
  XGetImage capture + stb PNG). It is built two ways: **dynamically** baked into
  the bundled desktop image (`build-desktop-agent.sh` / the image Dockerfile),
  and **fully static** (musl + a from-source X client stack) by
  `build-desktop-agent-static.sh` for the host-injected path below.
- **`scripts/`**: `fetch-assets.sh`, `fetch-alpine-rootfs.sh`,
  `build-initramfs.sh` (repacks the initramfs and injects `custom-init.sh` as
  `/init`), `custom-init.sh` (the guest PID-1: parses the cmdline, mounts shares,
  chroots/switch_roots, runs the exec or the desktop branch — which prefers a
  host-**injected** agent at `/mnt/agent` over an in-image entrypoint — writes
  `.vmette-exit`, powers off), `build-vsock-send.sh`, `build-desktop-agent.sh`,
  `build-desktop-agent-static.sh` (builds the static agent + `vmette-desktop-run.sh`
  into `assets/<arch>/desktop-agent/`, which the daemon discovers via
  `vmette_assets::resolve_agent_share` and injects as the `agent` virtio-fs share
  so any GUI rootfs works), `run.sh`, `install.sh`.
- **`images/vmette-desktop/`**: the **reference recipe** for the published default
  desktop image (`vmette_assets::DEFAULT_DESKTOP_IMAGE`,
  `ghcr.io/chamuka-inc/vmette-desktop:latest`) — `Dockerfile` + `entrypoint.sh` +
  `vmette-open` for a Debian-slim rootfs (Xvfb + openbox + a baked-in agent). This
  repo no longer auto-builds it (the published image is frozen and pulled on first
  use); the recipe doubles as a bring-your-own template, since the agent is
  host-injected and any GUI rootfs works. `vmette-desktop-run.sh` is the
  host-**injected** startup (shipped in the `agent` share, run by the init's
  desktop branch) that brings up Xvfb + a WM and execs the injected static agent,
  so a vmette-specific image isn't required.

**Important:** after editing `scripts/custom-init.sh`, rebuild the initramfs
(`bash scripts/build-initramfs.sh`) — the live `assets/<arch>/initramfs-vmette` embeds a
*copy* of it; a stale initramfs silently ignores the desktop branch.

## Key constraints (mention in docs / be aware of)

- **macOS-only**, requires codesigning with `entitlements.plist`
  (`com.apple.security.virtualization`) to boot a VM. `vmetted` boots desktop VMs
  in-process, so the daemon binary itself must be signed.
- **Guest assets are architecture-specific** (`assets/x86_64` or `assets/aarch64`).
  Desktop image + agent must match the guest arch.
- **Snapshot/restore is Apple-Silicon-only** (returns `SnapshotUnsupported` on
  Intel).
- **Desktop sessions are software-rendered (Xvfb, no GPU)** — fine for agentic
  GUI control / UI testing, not video/WebGL/3D. Each is a live ~1–2 GB VM.
- The guest connects **out** to the host vsock listener (same direction as
  `vsock-runner.c`), avoiding the arm64-only host→guest connect.

## Conventions

- Zero clippy warnings; `cargo fmt --all` clean. Hand-rolled CLI parsing (avoid
  pulling in clap-scale deps — small binary is a goal).
- Prefer structurally correct solutions over pragmatic band-aids.
- Docs live in `docs/`: `CLI.md`, `API.md`, `DAEMON.md`, `MCP.md`, `DESKTOP.md`,
  `HACKING.md`.
- `CHANGELOG.md` records **only external-API changes and bug fixes** — anything a
  consumer calls, passes, or observes (CLI flags, MCP tools, library
  types/signatures, daemon wire protocol, env vars, build/make targets) or a
  behavioral fix. Internal restructures (crate extractions, module moves,
  refactors with unchanged behavior) do **not** belong, even under "Changed".
  Fold internal sub-points into the external-facing entry they support.
- Do not commit `assets/` or `images/` build outputs.
