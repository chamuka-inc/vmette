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
branch `arch-v2-phase0` (2026-06-13).** Spike code lives in
`crates/vmette/examples/{serial_capture_spike,inproc_soak_spike}.rs` (examples,
not shipped lib). Findings below.

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

### [GATE G2] Serial capture + stream separation — RESOLVED (design); boot-validation deferred

`crates/vmette/examples/serial_capture_spike.rs` builds four VZ configs and runs
`validateWithError` (ad-hoc signed). Result:

```
[1] single inherit console      : VALID   (baseline)
[2] single capture (pipe) console: VALID   (NSFileHandle-from-fd attachment OK)
[3] two consoles (hvc0 + hvc1)   : VALID   → stream separation feasible
[4] two capture consoles         : VALID   → headless dual-capture feasible
[5] three (kernel + exec out/err): VALID   → exec-dedicated clean capture feasible
```

**Decision: exec-dedicated 3-console topology — NOT the original "second
console" sketch.** Phase 0 found that two consoles do not yield *clean* capture:
`console=hvc0` carries kernel messages and `/init` logs to fd 2
(`custom-init.sh:36`), so a naive stdout→hvc0/stderr→hvc1 split leaves kernel
lines polluting stdout and `[init]` lines polluting stderr — relocating the
marker-scraping problem rather than deleting it. The corrected design (validated
as case [5]): **hvc0** = kernel + init logs (inherit/drained), **hvc1** = exec
stdout (capture), **hvc2** = exec stderr (capture), with `/init` redirecting the
user command's fds 1/2 to hvc1/hvc2. VZ's validator accepts this shape and the
pipe-fd attachment API (`NSFileHandle::initWithFileDescriptor_closeOnDealloc`),
both compiling against `objc2-virtualization 0.3.2`. **SPEC §4.2 rewritten.**

**[GATE G2-capture] sub-decision, resolve at Phase 2 start:** exec-dedicated
consoles (preserves streaming `Frame::Stdout`, default) vs redirecting exec
output to files on the `ctl` share (perfectly clean, zero console multiplexing,
but loses incremental streaming). See SPEC §4.2.

**Still boot-gated (needs a signed build + assets):** whether the guest kernel
enumerates hvc1/hvc2 and whether `/init`'s fd redirect routes cleanly. If that
boot test fails, the single-stream fallback (captured stdout + structured exit
via `ctl`, shape [2]) is the backstop.

### [GATE G2-stability] In-process VZ fault soak — HARNESS DELIVERED; run gated on signed build

`crates/vmette/examples/inproc_soak_spike.rs` boots N (default 500) one-shot
`Session`s back-to-back in one process (no fork), exec-ing `true`, and reports
ok/bad counts, per-boot timing, and host fd drift. It **compiles against the
public `vmette` API** (itself confirming C2's in-process path has the surface it
needs) and skips cleanly when assets are absent. **Cannot run here** (needs a
booted VM = signed build + fetched assets). Run instructions and the abort
criterion are in the example's module docs. **This is the one Phase-0 item whose
evidence is still outstanding** — it must be run on a signed macOS build before
C2's subprocess deletion (Phase 2e).

### Exit status

| Gate | Status |
|------|--------|
| G1 (parse approach) | ✅ Resolved → `KEY=VALUE` env file, no parser binary |
| G2 design (API feasibility) | ✅ Resolved → two-console + pipe-fd capture validate |
| G2 boot-validation (guest hvc1 routing) | ⏳ Deferred — needs signed build |
| G2-stability soak | ⏳ Harness ready — needs signed build + assets |
| G3 (snapshot delete vs gate) | ✅ Resolved → **delete** (recorded in Phase 3) |
| G4 (C4 scope) | ✅ Resolved → **defer** C4 pending profiling |

Phases 1 and 3 are unblocked and can start now. Phase 2's subprocess *deletion*
(2e) is blocked until the soak runs green on a signed build; Phase 2's additive
work (2a–2c) can proceed in parallel since two-console capture is design-validated.

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
  `<ctl>/boot.json`. Make the `ctl` share **unconditional** for config-bearing
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
  run the parser on `boot.json`, `eval` its `KEY=VALUE` output, assert
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

## Phase 3 — C3: Remove snapshot surface

Depends on: nothing (can run in parallel with Phase 1/2; sequence after C1 only
because C1 already removed the snapshot *cmdline tokens*).

- **[GATE G3]** Confirm delete-vs-feature-gate. Recommendation: **delete**.
- Remove `Config::{build_snapshot,resume_snapshot,guest_vsock_port}`, the
  `lifecycle.rs:33-41` dispatch, `vz/snapshot.rs`, and the CLI flags.
- Remove `ListenerMode::Echo` + the `READY\n` detector (`vz/vsock.rs`); change
  `session.rs:450-453` so `OneShot` instantiates no vsock listener unless an exec
  needs the agent channel.
- Remove the `snapshot_mode=server` branch and `.vmette-runner.sh` heredoc from
  `custom-init.sh`; stop injecting `guest/vsock-runner.c` into the initramfs.
  Rebuild the initramfs.
- `CHANGELOG.md`: record removal of `--build-snapshot`/`--resume-snapshot`.
- Gate: `cargo test --workspace`; clippy clean; `tests/run.sh` green.

Exit criteria: no dead snapshot surface; `OneShot` no longer instantiates an
echo listener.

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
Phase 3  C3 snapshot removal ──┴───── (after C1's token removal; else parallel)
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
- No vestigial snapshot surface.
- `run()` returns; one daemon-client; one `CaTrust` owner; `Config` rootfs enum.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (zero
  warnings), `cargo test --workspace`, and `tests/run.sh` all green.
- `CHANGELOG.md` updated for the snapshot-flag removal and the FFI `vmette_run`
  behavior change; no internal-only entries added.
