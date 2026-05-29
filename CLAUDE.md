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
(`tests/run.sh`, ~10 gates) — it boots real VMs, so it requires a codesigned
binary and macOS.

## Workspace layout

Six crates (workspace `version = 0.1.0`, edition 2021, `rust-version = 1.80`, MIT):

| Crate | Purpose |
|-------|---------|
| `crates/vmette` | Core library (lib + cdylib + staticlib). VZ wrapper; `Config`, `run()`, `Session`, providers, C ABI. |
| `crates/vmette-cli` | `vmette` CLI binary. Hand-rolled arg parsing (no clap — keeps the binary small). |
| `crates/vmette-daemon` | `vmetted` — UNIX-socket dispatcher (tokio). Stateless subprocess dispatch + stateful desktop registry. |
| `crates/vmette-mcp` | MCP server (`vmette-mcp`) exposing vmette to AI agents over stdio. |
| `crates/vmette-provider-oci` | OCI/Docker image rootfs provider (catch-all for bare refs + `oci://`). |
| `crates/vmette-provider-tar` | Tarball rootfs provider (`tar+https://`, `tar+http://`, `tar+file://`). |

## Core library — `crates/vmette/src/`

- **`lib.rs`** — public API: `Config`, `run()`, `Session`, `WorkloadStrategy`
  (`OneShot` | `Agent`), `VsockPort`, `RootfsShare`, `ShareMount`, `RunOutput`.
- **`session.rs`** — the **`Session` primitive**: owns a live booted VM + its
  private VZ dispatch queue + teardown. `Session` is `!Send` (holds objc2
  `Retained`), so it runs on its own OS thread and hands out the `Send` handles
  `SessionClient` (desktop requests) and `StopHandle` (graceful stop).
  `SessionEnd` = Exited/TimedOut/Stopped/Error.
- **`lifecycle.rs`** — `run()` is a thin **OneShot** wrapper over `Session`:
  raw-mode + signal handlers, start, block on terminal event, exit with the
  guest's code. Snapshot build/resume dispatch lives here too.
- **`provider.rs`** — `RootfsProvider` trait + `Registry` dispatcher + `Context`
  (cache root, offline flag, optional guest-helpers dir). Built-in `DirProvider`;
  OCI/tar live in sibling crates.
- **`desktop.rs`** — framed vsock protocol `[u32 LE header_len][JSON header][optional binary payload]`,
  the `Action` enum (screenshot/clicks/type/key/scroll/… mirroring Anthropic
  computer use) and `ResponseHeader`. Pure types — no VZ/objc2.
- **`cmdline.rs`** — assembles the kernel cmdline, emitting `vmette.*=…` tokens
  (exec base64, rootfs, shares, net, vsock_port, switch_root, snapshot mode,
  `vmette.desktop=1` + `vmette.display=WxH` for the Agent strategy).
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

- **Stateless dispatch** (`main.rs`): the existing per-request path forks a
  `vmette` subprocess and streams stdout/stderr/exit back over the socket.
- **Stateful desktop registry** (`registry.rs`): holds live `vmette::Session`
  VMs (Agent workload) in-process so a desktop persists across many requests.
  Guardrails: max-live cap (8), idle eviction (30 min), background sweep (60 s).
  Each session on its own thread; VM-control hops off the async runtime via
  `spawn_blocking`. Keep statefulness contained here — do **not** interleave it
  into the clean dispatch path.

## MCP server — `crates/vmette-mcp/src/`

`server.rs` registers tools: `execute`, `fetch_url`, `workspace_*` (direct
subprocess path), and `desktop_*` (routed through `vmetted` via
`daemon_client.rs`, since persistence requires the daemon). `desktop_screenshot`
returns an MCP image content block. `--allow-network` gates outbound network.

## Assets, guest, scripts

- **`assets/`** (downloaded/built, gitignored): `vmlinuz-virt`, the alpine
  netboot initramfs, the repacked `initramfs-vmette`, and `alpine-rootfs/`.
- **`guest/`** (C, cross-compiled for Linux x86_64): `vsock-send.c`,
  `vsock-runner.c` (snapshot-mode helpers, static musl, injected into the
  initramfs) and `vmette-desktop-agent.c` (computer-use agent: XTEST input +
  XGetImage capture + stb PNG, links libX11/libXtst **dynamically**, so it ships
  inside the desktop rootfs, not the initramfs).
- **`scripts/`**: `fetch-assets.sh`, `fetch-alpine-rootfs.sh`,
  `build-initramfs.sh` (repacks the initramfs and injects `custom-init.sh` as
  `/init`), `custom-init.sh` (the guest PID-1: parses the cmdline, mounts shares,
  chroots/switch_roots, runs the exec or the desktop branch, writes
  `.vmette-exit`, powers off), `build-vsock-send.sh`, `build-desktop-agent.sh`,
  `build-desktop-image.sh`, `run.sh`, `install.sh`.
- **`images/vmette-desktop/`** (untracked): Dockerfile + entrypoint for the
  Debian-slim desktop rootfs (Xvfb + openbox + the agent).

**Important:** after editing `scripts/custom-init.sh`, rebuild the initramfs
(`bash scripts/build-initramfs.sh`) — the live `assets/initramfs-vmette` embeds a
*copy* of it; a stale initramfs silently ignores the desktop branch.

## Key constraints (mention in docs / be aware of)

- **macOS-only**, requires codesigning with `entitlements.plist`
  (`com.apple.security.virtualization`) to boot a VM. `vmetted` boots desktop VMs
  in-process, so the daemon binary itself must be signed.
- **Guest assets are x86_64-only** (`linux-virt-x86_64.apk`). Desktop image +
  agent must match the guest arch.
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
  `HACKING.md`. Keep `CHANGELOG.md` updated for user-facing changes.
- Do not commit `assets/` or `images/` build outputs.
