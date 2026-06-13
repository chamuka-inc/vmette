# vmette Architecture v2 — Implementation Plan

Companion to [`ARCH-V2-SPEC.md`](./ARCH-V2-SPEC.md). This is the sequenced,
gated delivery plan. Each phase is independently shippable, leaves the workspace
green (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets` at
zero warnings, `cargo test --workspace`), and where it touches the guest, leaves
`tests/run.sh` green on a codesigned macOS build.

## Guiding rules

- **Ship behind green gates.** No phase merges with a failing check or a stale
  initramfs. After any `scripts/custom-init.sh` edit, run
  `bash scripts/build-initramfs.sh` and re-run the affected `tests/run.sh` gates.
- **Decisions before code.** Resolve the gated decisions (G1–G4) at the start of
  the phase that needs them — they are listed inline as `[GATE …]`.
- **Subprocess deletion is last.** In C2, the in-process path lands and soaks
  *alongside* the subprocess path; the fork code is deleted only after the soak.
- **One reviewable change per PR.** Phases are sized to be reviewed in one sitting.

---

## Phase 0 — Decision spikes (no production code)

Resolve the decisions that shape everything downstream. **Status: executed on
branch `arch-v2-phase0` (2026-06-13), driven all the way to real VM boots.**
Spike code lives in `crates/vmette/examples/` (examples, not shipped lib):
`serial_capture_spike` (config validation), `three_console_boot_spike` (real
multi-console boot + delivery + clean-primary probe), `inproc_soak_spike`
(in-process boot/teardown soak). Findings below.

### Environment constraint discovered

This dev box is **Intel macOS with no codesigning identity and an empty
`assets/`**. Critically, the spike proved that **`VZVirtualMachineConfiguration::validateWithError`
itself requires the `com.apple.security.virtualization` entitlement** — even a
config that never instantiates a VM. So *nothing* VZ-touching runs unsigned.
**Workaround found:** ad-hoc codesigning grants the entitlement locally —
`codesign -s - --entitlements entitlements.plist --force <binary>` — after which
validation (and likely booting) works on this box. `cargo run` re-links and
strips the signature, so spikes must be `cargo build` + `codesign` + run-binary.

### [GATE G1] Guest parse approach — RESOLVED

**Decision: line-oriented `KEY=VALUE` envelope on the `ctl` share. No JSON, no
parser binary, no shell JSON.** Rationale discovered during the spike: the guest
`/init` already depends on busybox `base64` (applet list, `custom-init.sh:27`)
and already `base64 -d`s `vmette.exec` and `vmette.env` (`custom-init.sh:381,394`).
So the host serializes the typed `BootParams` to `boot.env` as `KEY=VALUE` lines
(base64 the multi-line/special fields `exec` and `env`, plain scalars otherwise),
and the guest reads it with the *same* primitive it already uses — replacing
`cmdline_get vmette.X` with a read of one file. This is strictly better than the
SPEC's original JSON-blob + helper-binary (§3.3): it adds **no** new artifact to
the initramfs and removes a fragility (in-shell JSON) rather than relocating it.
The typed single-owner contract still lives in `vmette_proto::boot::BootParams`
with `to_env()`/`from_env()`; only the wire *format* changes from JSON to
`KEY=VALUE`. **SPEC §3.2/§3.3 updated accordingly.**

### [GATE G2] Serial capture topology — RESOLVED on real boots (not just validation)

Phase 0 was driven all the way to **booting real VMs** (ad-hoc codesigning grants
the entitlement; assets fetched via the normal scripts). This overturned the
config-validation-only conclusion and produced the final design.

`serial_capture_spike` (config validation) showed multiple consoles + pipe-fd
attachments all *validate*. But `three_console_boot_spike` (real boots) found
they don't *deliver*:

```
n=1: delivered 1/1   [hvc0=✓]
n=2: delivered 1/2   [hvc0=✓ hvc1=·]
n=3: delivered 1/3   [hvc0=✓ hvc1=· hvc2=·]
n=4: delivered 1/4   [hvc0=✓ hvc1=· hvc2=· hvc3=·]
clean-primary (console=hvc1 sink): got=true clean=true
```

**Finding: VZ delivers host data for only ONE virtio console port reliably.** The
guest enumerates `/dev/hvc1…` and writes to them succeed (`rc=0`), but the bytes
never reach the host. So the interim "3 exec-dedicated consoles" design (which
*passed config validation* as case [5]) **fails at boot** — exactly why
boot-validation was a required gate, and a caution against trusting
`validateWithError` as a proxy for behavior.

**Final decision (validated): clean single-console streaming + `ctl` share.**
- **hvc0** = the one captured, streaming console, carrying *only* exec output.
- **`console=hvc1`** = a discard sink: kernel printk + `/init` `[init]` chatter
  (fd 2 = `/dev/console`) go there, off hvc0. Proven clean: clean-primary case
  returned `got=true clean=true` (exec markers only, no init/kernel/overlay noise).
- **`ctl` virtio-fs share** = stdout/stderr separation (when needed) + exit code;
  virtio-fs is rock-solid (300/300 soak boots mount it).

This **deletes marker-scraping AND preserves incremental streaming** — both C2
goals. The fallback (exec stdout/stderr → `ctl` files, non-streaming) is kept as
a backstop only. **SPEC §4.2 rewritten to this design.** The earlier
`[GATE G2-capture]` sub-decision is now moot (multi-console isn't an option).

### [GATE G2-stability] In-process VZ fault soak — RUN, GREEN

`inproc_soak_spike` ran on the signed build against real assets:

```
ok=300  bad=0  of 300
avg=1110ms  worst=1647ms
fd drift: start=4 end=4 (delta=0)
VERDICT: healthy → supports C2 (in-process one-shot).
```

300 consecutive in-process boot/teardown cycles, **zero failures, zero fd leak**.
This is the evidence base for C2 giving up subprocess fault isolation — the
abort criterion (failures or linear fd growth) was not triggered.

### Exit status — all gates closed

| Gate | Status |
|------|--------|
| G1 (parse approach) | ✅ Resolved → `KEY=VALUE` env file, no parser binary |
| G2 design + boot-validation | ✅ Resolved on real boots → clean single-console (hvc0) streaming + `console=hvc1` sink + `ctl` share. Multi-console is NOT viable under VZ. |
| G2-stability soak | ✅ Run green → 300/300, fd drift 0 |
| G3 (snapshot delete vs keep) | ✅ **REVERSED → keep** (real Apple-Silicon Phase-5 feature; integrate into `boot.env`, recorded in Phase 3) |
| G4 (C4 scope) | ✅ Resolved → **defer** C4 pending profiling |

**No open Phase-0 gaps remain.** All boot-gated items were closed by ad-hoc
codesigning + fetched assets on this box. Phase 1 (C1), Phase 2 (C2, including
the now-validated capture design and the subprocess-deletion evidence), and
Phase 3 (C3) are all unblocked.

#### Environment recipe to reproduce the boot spikes

```
bash scripts/fetch-assets.sh && bash scripts/fetch-alpine-rootfs.sh && \
  bash scripts/build-initramfs.sh
cargo build -p vmette --example three_console_boot_spike
codesign -s - --entitlements entitlements.plist --force \
  target/debug/examples/three_console_boot_spike
VMETTE_KERNEL=assets/x86_64/vmlinuz-virt \
VMETTE_INITRAMFS=assets/x86_64/initramfs-vmette \
VMETTE_ROOTFS=assets/x86_64/alpine-rootfs \
  ./target/debug/examples/three_console_boot_spike
# soak: same build+sign for inproc_soak_spike, with VMETTE_SOAK_* env vars.
```

---

## Phase 1 — C1: Typed boot contract

Depends on: G1 resolved. Does **not** depend on C2.

### 1a. `vmette-proto`: define the contract
- Add `crates/vmette-proto/src/boot.rs` with `BootParams`, `RootfsSpec`,
  `Strategy`, `BOOT_PROTO_VERSION = 1` (SPEC §3.2). Re-export from the crate root.
- Tests: serde round-trip; version field present; `RootfsSpec`/`Strategy`
  variants serialize stably.
- Gate: `cargo test -p vmette-proto`.

### 1b. `vmette`: host-side emit
- In `vz/config::build`, make the storage builder return the assigned scratch
  device name alongside the attachment (SPEC §3.2 "Device-name ownership").
- `Session::start`: build a `BootParams` from the (cloned) `Config`, serialize to
  `<ctl>/boot.env`. Make the `ctl` share **unconditional** for config-bearing
  workloads; keep the `"ctl"` tag reservation check (`session.rs:380-384`),
  generalize its message.
- Rewrite `cmdline::build` to emit only `console=… quiet vmette.boot=ctl` plus
  `vmette.vsock_port` when set. Delete `scratch_device_name` and all other
  `vmette.*` token emission. Update/relocate the unit tests in `cmdline.rs:108-223`
  to assert the reduced cmdline.
- Merge OCI image env into `BootParams.env` host-side (replacing the
  `/.vmette-image-env` second channel), preserving `--env`-overrides-image
  ordering.
- Gate: `cargo test -p vmette`; clippy clean.

### 1c. Guest-side parse (`scripts/` + maybe `guest/`)
- Per G1: add the static-musl `vmette-init-parse` helper (new `guest/` C source +
  `scripts/build-*.sh`, injected by `build-initramfs.sh`) **or** the vendored
  shell parser.
- Rewrite the cmdline-parsing head of `scripts/custom-init.sh` to: mount `ctl`,
  run the parser on `boot.env`, `eval` its `KEY=VALUE` output, assert
  `proto_version`, abort loudly on mismatch. The existing mount/overlay/exec body
  now consumes typed variables. Echo parsed params under `[init]`.
- **Rebuild the initramfs** (`bash scripts/build-initramfs.sh`).
- Gate: `tests/run.sh` (real VM boots) — exec, rootfs-block, shares, env,
  scratch-overlay, and exit-code gates all green. This is the load-bearing gate
  for C1.

### 1d. Make initramfs-staleness a checked step
- Add a guard (build script or `tests/run.sh` preflight) that fails if
  `custom-init.sh` is newer than the staged `assets/<arch>/initramfs-vmette`,
  converting the documented footgun (CLAUDE.md) into an error.

Exit criteria: all gates green; a stale initramfs now fails closed; cmdline is
reduced; one env channel; device name single-owned.

---

## Phase 2 — C2: One execution substrate

Depends on: G2 + G2-stability resolved. Builds on Phase 1's `ctl` channel.

### 2a. `vmette`: capture-aware `Session`
- Add `SerialSink { Inherit, Capture(...) }` to `vz/config::build`
  (SPEC §4.2 Step 1) and a `Config` field selecting it (default `Inherit`).
- Per G2: implement either two-console separation or single-stream capture.
- Extend `RunOutput` (`lifecycle.rs:18-23`) with `stdout`/`stderr`; add
  `Session::wait_captured() -> RunOutput`, draining on the owning thread with the
  ported 1 MiB cap + truncation marker (from `sandbox.rs:243-293`).
- Tests: capture exact stdout/stderr/code for a fixture exec; cap truncation.
- Gate: `cargo test -p vmette`; clippy clean.

### 2b. `vmette-proto` / `vmette`: `Request → Config`
- Add the single-owner `Request::to_config` (or `From<Request>`), replacing
  `to_cli_args` semantically. **Do not delete `to_cli_args` yet** (the subprocess
  path still uses it until 2e).
- Tests: a `Request` maps to a `Config` whose fields match the old argv mapping.

### 2c. `vmette-daemon`: in-process run lane (additive)
- Add an in-process `run_workload` implementation behind a runtime/env switch
  (e.g. `VMETTE_INPROC_RUN=1`), leaving the subprocess path as default. It builds
  `Config` via `to_config`, starts a `Capture` `Session` on a `spawn_blocking`
  thread, and emits the same `Frame::Stdout/Stderr/Exit` sequence.
- Tests: golden `Frame` sequence parity between the two implementations for a
  fixture request.

### 2d. Soak + flip the default
- Run the §2c in-process lane under the G2-stability soak in the daemon context.
- Flip the default to in-process; keep the subprocess path reachable for one
  release as a fallback escape hatch.

### 2e. Delete the subprocess machinery
- Remove `run_workload`'s `Command`/`kill_on_drop`/reader tasks, `locate_vmette`
  (`main.rs:114-127,367-492`), `Request::to_cli_args` + its tests
  (`daemon.rs:79-143,349-423`), and the `--vmette` flag plumbing.
- Rewrite `vmette-mcp::Sandbox` to call the in-process helper directly; delete
  `wrap_exec`, `slice_exec_output`, `MARKER_BEGIN/END`, `read_capped`, and the
  sandbox marker tests (`sandbox.rs:38-77,243-293,328-406`). Preserve the
  host-side wall-clock timeout as an in-process guard.
- Update `main.rs` module doc (the "v0.1 … forks the `vmette` CLI" header) to
  describe the single in-process substrate.
- Gate: `cargo test --workspace`; clippy clean; `tests/run.sh` green; MCP
  `RunReply` parity for fixtures.

Exit criteria: one execution path; ~400+ lines of marker/argv/fork code deleted;
output capture single-owned in the library.

---

## Phase 3 — C3: Preserve snapshot; integrate into the boot contract

> **[GATE G3] REVERSED — do NOT delete.** Snapshot/restore is a real, planned
> **Apple-Silicon** feature (Phase 5: `saveMachineStateToURL:` is arm64-gated;
> the daemon warm pool needs it). The Phase-0 "vestigial" reading was literally
> true (both arches stub out today) but the delete conclusion was wrong. C3 keeps
> all snapshot scaffolding and makes it coherent with the C1 `boot.env` contract.

Depends on: C1 (the `boot.env` contract).

- **KEEP** `Config::{build_snapshot,resume_snapshot,guest_vsock_port}`, the CLI
  flags, `lifecycle::run` dispatch, `vz/snapshot.rs` (the `cfg(aarch64)` Phase-5
  stub), `ListenerMode::Echo` + the `READY\n` detector, the `custom-init.sh`
  snapshot branch, and `guest/vsock-runner.c`.
- **Integrate** snapshot into the typed contract (done): `Strategy::Snapshot
  { guest_vsock_port }` in `vmette-proto::boot`; `to_env`/`from_env` carry it
  (`VMETTE_STRATEGY=snapshot` + `VMETTE_GUEST_VSOCK_PORT`); the guest branch keys
  off `VMETTE_STRATEGY=snapshot` and reads the guest port from `boot.env`.
- The **producer** (a `BootParams` with `Strategy::Snapshot`) is Phase 5's, wired
  when `vz::snapshot::build` is implemented. Contract + guest consumer land now so
  C1's cmdline shrink does not strand the feature.
- No CHANGELOG entry (no observable change; flags still return
  `SnapshotUnsupported` until Phase 5).
- Gate: `Strategy::Snapshot` round-trips; `cargo test --workspace`; clippy clean;
  `tests/run.sh` green (snapshot arch-guard + vsock-roundtrip gates).

Exit criteria: snapshot preserved and coherent with `boot.env`; no dead
`VMETTE_SNAPSHOT_MODE` reference; the feature is Phase-5-ready.

---

## Phase 4 — C5: Lower-tier consolidation

Independent items; land in any order, each its own PR.

- **4a. `run()` returns.** Rewrite `lifecycle::run` to return `RunOutput`
  (exit/timeout/stop/error → code) instead of `process::exit`; move exit-code
  selection into `vmette-cli::main`. Update `ffi.rs` `vmette_run` docs/behavior.
  `CHANGELOG.md`: FFI `vmette_run` no longer exits the process. CLI integration
  test for the four end states.
- **4b. `vmette-daemon-client` crate.** Extract one sync transport
  (connect/auto-spawn/write/read/match) from `vmette-cli/src/desktop.rs` and
  `vmette-mcp/src/daemon_client.rs`; MCP wraps it in `spawn_blocking`. Move the
  duplicated unit tests into the new crate.
- **4c. `CaTrust` owner.** One type consumed by every boot path; guest trust
  munging fed a single resolved cert set (SPEC §7.3).
- **4d. `Config` rootfs enum + drop `quiet`.** Collapse
  `rootfs_share`/`rootfs_block` into `enum Rootfs`; thread `quiet` through
  `run()`/banner instead of the library type. Update all construction sites
  (CLI, daemon registry, MCP, providers).

Exit criteria: per-item green gates; workspace clippy-clean.

---

## Phase 5 — C4: Multiplexed desktop codec (optional)

Depends on: **[GATE G4]** contention profiling. Recommendation: **defer** unless
the registry's shared-`SessionClient` contention (VNC view + settle-poll +
actions on one mutex, `session.rs:149-182`) is shown to matter.

- Add `req_id` to the framed codec (`desktop.rs` + `vmette-proto::agent`); host
  demultiplexer; per-request error recovery replacing whole-fd `invalidate_fd`.
- Update `guest/vmette-desktop-agent.c` to echo `req_id`; rebuild the desktop
  image/agent.
- Gate: codec round-trip + out-of-order demux tests; `tests/run.sh` desktop gates.

---

## Sequencing summary

```
Phase 0  spikes ───────────────┐ (G1, G2, G2-stability)
                               ▼
Phase 1  C1 boot contract ─────┬─────────────► (independent)
                               │
Phase 2  C2 one substrate ─────┘ (needs G2 + ctl from C1)
                               │
Phase 3  C3 snapshot KEEP+wire ┴───── (integrate snapshot into boot.env; depends on C1)
                               │
Phase 4  C5 consolidation ─────┴───── (independent items, any order)
                               │
Phase 5  C4 mux codec ─────────┴───── (optional; gated on profiling)
```

Critical path: **0 → 1 → 2**. Everything else parallelizes against it.

## Risk register

| Risk | Phase | Mitigation |
|------|-------|------------|
| Loss of subprocess fault isolation faults the daemon | 2 | G2-stability soak; subprocess path kept one release as fallback; abort criterion defined |
| VZ can't cleanly separate stdout/stderr consoles | 0/2 | G2 spike decides; single-stream + structured-exit fallback |
| Stale initramfs silently ignores new boot contract | 1 | `BOOT_PROTO_VERSION` fails closed; staleness becomes a checked build step (1d) |
| Guest JSON parsing in shell is fragile | 1 | G1 prefers a static-musl helper, unit-tested in isolation |
| `tests/run.sh` requires codesigned macOS | all guest phases | run on the maintainer's signed build before merge; CI cannot gate the e2e |
| Scope creep into a full Rust PID-1 rewrite | 1 | explicit non-goal (SPEC §3.5); `/init` stays shell |

## Definition of done (whole effort)

- One execution substrate; `to_cli_args`, marker-slicing, and the MCP subprocess
  fork deleted.
- Host→guest config is a single typed, versioned `BootParams` blob; cmdline
  reduced to kernel-critical tokens; one env channel; device name single-owned.
- Snapshot preserved (real Apple-Silicon Phase-5 feature) and integrated into the
  `boot.env` contract (`Strategy::Snapshot`) — not deleted.
- `run()` returns; one daemon-client; one `CaTrust` owner; `Config` rootfs enum.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (zero
  warnings), `cargo test --workspace`, and `tests/run.sh` all green.
- `CHANGELOG.md` updated for the FFI `vmette_run` behavior change (and the C1
  `RootfsArtifact::Directory` field); no internal-only entries added. Snapshot is
  NOT in the changelog — it is preserved with no observable change.
