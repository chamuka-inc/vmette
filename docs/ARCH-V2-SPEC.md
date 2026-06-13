# vmette Architecture v2 — Specification

Status: **proposal**
Scope: internal restructuring of the host↔guest boundary and the daemon execution model. No new product capability.
Audience: vmette maintainers.

This document specifies a set of architectural changes derived from a converged
multi-agent review of the codebase. It describes the **target state** and the
**contracts** that must hold; the sequenced delivery plan lives in
[`ARCH-V2-PLAN.md`](./ARCH-V2-PLAN.md).

---

## 1. Motivation

The Rust core is type-disciplined: `vmette-proto` makes the vsock and daemon
wire shapes single-owner, so protocol drift is a compile error. That discipline
**stops at the host↔guest boundary and at the daemon's execution model**, where
three structural problems have accumulated:

1. **The kernel cmdline is an untyped RPC channel.** `crates/vmette/src/cmdline.rs`
   string-concatenates `vmette.*=` tokens; the guest re-parses them in pure
   POSIX shell (`scripts/custom-init.sh`). The two ends share no schema. The
   contract is enforced only by comments — e.g. `scratch_device_name`
   (`cmdline.rs:16`) must be kept "in lockstep with the attach order in
   `vz::config::build`" (`vz/config.rs:131-198`), a cross-file invariant with no
   compiler behind it. The channel has a hard ~3000-char budget
   (`cmdline.rs:97-99`), already forcing base64 of `exec`/`env` and a *second*
   config channel (the `/.vmette-image-env` rootfs file).

2. **The daemon maintains two execution substrates for one capability.** Desktop
   sessions run `vmette::Session` **in-process** (`registry.rs`); one-shot runs
   **fork the `vmette` CLI** (`main.rs:367 run_workload`) and reconstruct
   structured results by scraping a CRLF console stream with
   `__VMETTE_EXEC_BEGIN__` marker sentinels (`vmette-mcp/src/sandbox.rs:38-77`).
   The MCP server forks the CLI a *third* time, bypassing the daemon entirely
   (`sandbox.rs:1-11`). The serialization contract `Request::to_cli_args`
   (`vmette-proto/src/daemon.rs:84`) exists only to feed our own subprocess. The
   sole stated reason (`main.rs:5-8`) is that the library "forwards guest stdio
   straight to the daemon process's stdio" — a fixable property, not a law.

3. **Snapshot/restore is vestigial but load-bearing in the type surface.**
   `vz/snapshot.rs` is `TODO(phase 5)` stubs returning `SnapshotUnsupported` on
   both arches, yet the scaffolding spans `Config::{build_snapshot,resume_snapshot}`,
   `lifecycle.rs:33-41`, `cmdline.rs:82-88`, the `custom-init.sh` `snapshot_mode=server`
   branch, the `vsock-runner.c` helper baked into every initramfs, and the entire
   `ListenerMode::Echo` + `READY\n` detector in `vz/vsock.rs` that **every**
   one-shot run still instantiates (`session.rs:450-453`).

The root cause shared by (1) and (2) is the same: the one-shot serial port is
hard-wired to host stdio in `vz/config.rs:63-69`
(`fileHandleWithStandardOutput()`), so the library cannot return captured output
and the daemon cannot deliver structured control data to the guest over a clean
channel. Fixing that root cause unlocks both.

### Non-goals

- No change to the `Session`/`SessionClient`/`StopHandle` threading model, the
  `EndSlot` write-once condvar, or the per-session dispatch queue. These are
  correct and are preserved verbatim.
- No change to the provider registry order (`vmette-providers::default_registry`).
- No new user-facing CLI flags, MCP tools, or daemon capabilities. Behavior
  observable to a consumer is held constant except where explicitly noted in §7.
- Snapshot/restore is **not** implemented here; it is removed from the surface
  until a future phase delivers it.

---

## 2. Overview of the target architecture

```
                 ┌──────────────────── vmette-proto (leaf) ───────────────────┐
                 │  agent::{Action,…}   daemon::{DesktopRequest,…}            │
                 │  boot::BootParams  ← NEW: the typed host→guest contract    │
                 └────────────────────────────────────────────────────────────┘
                                      ▲                 ▲
                                      │ serialize once  │ deserialize once
   host (Rust)                        │                 │     guest (PID 1)
   ───────────                        │                 │     ───────────
   Config ──► Session::start ─────────┘                 └──── reads ctl/boot.json
        owns serial pipe (capture)                            then mounts/execs
        writes BootParams blob to ctl share
        reads RunOutput{stdout,stderr,code} back over the same ctl share

   ┌─────────────────────────── one execution substrate ──────────────────────┐
   │  CLI run() ───┐                                                            │
   │  daemon /run ─┼──► vmette::Session (in-process) ──► RunOutput              │
   │  MCP execute ─┘     (stateless lane: boot, capture, drop)                  │
   │  daemon desktop ──► vmette::Session (in-process, registry-held, stateful)  │
   └────────────────────────────────────────────────────────────────────────────┘
```

The five concrete changes:

| # | Change | Primary crates touched |
|---|--------|------------------------|
| C1 | Typed, versioned boot contract (`BootParams`) delivered via the `ctl` virtio-fs share; cmdline reduced to kernel-critical tokens | `vmette-proto`, `vmette`, `scripts/`, `guest/` |
| C2 | `Session` captures guest stdout/stderr; collapse all one-shot boot paths onto in-process `Session`; delete subprocess dispatch + marker scraping | `vmette`, `vmette-daemon`, `vmette-mcp`, `vmette-proto` |
| C3 | Remove the vestigial snapshot surface (feature-gate behind `snapshot`, default off) | `vmette`, `scripts/`, `guest/` |
| C4 | Add request-ids to the desktop vsock codec; recover per-request instead of invalidating the whole fd | `vmette-proto`, `vmette`, `guest/` |
| C5 | Lower-tier consolidation: `run()` returns instead of `process::exit`; single daemon-client crate; `CaTrust` owner; `Config` rootfs enum | `vmette`, `vmette-cli`, `vmette-daemon`, `vmette-mcp` |

C1 and C2 are the keystone and are sequenced first because they share the
serial-capture root cause and the `ctl`-share infrastructure. C3 is independent
and cheap. C4 and C5 are incremental and may land in any later order.

---

## 3. C1 — Typed boot contract (`BootParams`)

### 3.1 Current state

`cmdline::build` (`cmdline.rs:32-106`) emits, space-separated onto the kernel
cmdline: `vmette.exec` (base64), `vmette.rootfs_block` | `vmette.rootfs` +
`vmette.rootfs_ro`, `vmette.scratch_dev`, `vmette.share` (repeated),
`vmette.switch_root`, `vmette.net`, `vmette.vsock_port`, `vmette.snapshot_mode`,
`vmette.guest_vsock_port`, `vmette.desktop`, `vmette.display`, `vmette.env`
(base64). The guest parses these with hand-rolled `cmdline_get`/`cmdline_all`
shell loops. Image env is delivered out-of-band via `/.vmette-image-env` in the
rootfs because it does not fit the budget.

The `ctl` virtio-fs share already exists (`session.rs:362-396`) as a writable
per-session host temp dir, currently used **only** in the guest→host direction
to carry `.vmette-exit`.

### 3.2 Target design

Introduce `vmette_proto::boot::BootParams` — the single owner of the host→guest
configuration vocabulary:

```rust
// crates/vmette-proto/src/boot.rs  (new)

/// Bumped on ANY breaking change to the field set or semantics. The guest
/// refuses to boot (loud panic to console, non-zero exit) on mismatch rather
/// than silently ignoring an unknown shape.
pub const BOOT_PROTO_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootParams {
    pub proto_version: u32,
    pub exec: Option<String>,          // was vmette.exec (base64 on cmdline)
    pub env: Vec<(String, String)>,    // was vmette.env + /.vmette-image-env — ONE source
    pub rootfs: RootfsSpec,            // was rootfs / rootfs_block / rootfs_ro tokens
    pub scratch_dev: Option<String>,  // host-ASSIGNED guest device name (vda/vdb/…)
    pub shares: Vec<ShareMount>,       // extra --share tags (ctl excluded; it is implicit)
    pub switch_root: bool,
    pub net: bool,
    pub strategy: Strategy,            // OneShot | Agent { width, height }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RootfsSpec {
    Share { read_only: bool },
    Block { fstype: String },          // e.g. "squashfs"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Strategy {
    OneShot,
    Agent { width: u32, height: u32 },
}
```

**Delivery.** The host serializes `BootParams` to a line-oriented `KEY=VALUE`
envelope (`BootParams::to_env`) and writes it to `<ctl>/boot.env` *before* VM
start, in the same `ctl` temp dir already created by `Session::start`. The guest
mounts `ctl` early (it already must, to write `.vmette-exit`) and reads
`boot.env` once. **Format decision (G1, resolved in Phase 0):** a `KEY=VALUE`
envelope — not JSON — because the guest `/init` already has the only primitive
it needs (busybox `base64 -d`, used today for `vmette.exec`/`vmette.env`), so
multi-line/special fields (`exec`, env values) are base64-encoded per-field and
scalars are plain. This adds **no** parser binary and **no** in-shell JSON
parsing to the initramfs. The typed single-owner contract still lives in
`BootParams`; only the wire *format* differs from JSON.

**Cmdline reduction.** After C1 the kernel cmdline carries only tokens the
*kernel itself* consumes plus one boot marker:

```
console=hvc0 quiet vmette.boot=ctl
```

`vmette.boot=ctl` tells `/init` to read `boot.env` from the `ctl` share.
`vmette.vsock_port` **remains on the cmdline** — it is needed by the guest agent
before the ctl mount in the Agent path and is a single small integer (decision:
keep transport-bootstrap values on the cmdline, configuration values in the
blob). All other `vmette.*` tokens are removed.

**Device-name ownership.** `scratch_dev` is assigned by the host in
`vz/config::build` (which owns the attach order) and travels inside
`BootParams`. `cmdline::scratch_device_name` and its "keep in lockstep" comment
are deleted; the function's logic moves to a method on the storage-device
builder that returns the assigned name alongside the attachment, so the name and
the order have one owner.

### 3.3 Guest read (G1 resolved)

`/init` stays shell. Because the envelope is `KEY=VALUE` lines, the guest reads
it with **no parser binary and no JSON**: a bounded read loop sources the scalar
keys and `base64 -d`s the `exec`/`env` fields — the exact primitive
`custom-init.sh` already uses today (`base64 -d` at lines 381/394). The
`cmdline_get vmette.X` calls are replaced by reads of the parsed `boot.env`
variables.

`custom-init.sh` shrinks from a token-grepping PID-1 to: mount `ctl`, read
`boot.env`, assert `proto_version == BOOT_PROTO_VERSION` (abort loudly to the
console on mismatch), then the existing mount/overlay/exec logic consumes the
typed variables. The host owns the format via `BootParams::to_env`; the guest is
the only consumer of `from_env`-shaped input.

> **Phase-0 note:** the earlier draft proposed a JSON blob parsed by a
> static-musl helper. Phase 0 found the `KEY=VALUE` + reused-`base64` approach
> removes the need for any new artifact, so the helper-binary option is dropped.

### 3.4 Behavior & invariants

- The host writes `boot.json` and creates the `ctl` share for **every** workload
  that previously needed cmdline config — i.e. always, except a truly read-only
  directory rootfs with no exec, which today gets no `ctl` channel
  (`session.rs:372-396`). For C1 the `ctl` share becomes **unconditional**
  (host→guest config always needs it), removing the `needs_ctl` branch's role in
  config delivery while keeping it for the exit-code read-back.
- The `"ctl"` tag reservation check (`session.rs:380-384`) is retained and its
  error message generalized: `ctl` now carries both boot params and the exit code.
- Cmdline budget pressure is eliminated; the base64 `exec`/`env` encodings and
  the `/.vmette-image-env` second channel are removed (env folds into
  `BootParams.env`, with OCI image env merged host-side before serialization so
  the guest sees one ordered list and `--env` override semantics are preserved
  by ordering, not by two channels).

### 3.5 Trade-offs

- **Loses** the "debug the guest by reading the cmdline on the serial console"
  ergonomic — config now lives in a file on a share. Mitigation: `/init` echoes
  the parsed params to the console under the existing `[init]` log prefix.
- **Adds** a single `boot.env` read + `base64 -d` of two fields to the boot path
  (no new binary — G1). The ~1s boot budget is not measurably affected (one
  virtio-fs read of a <4 KB file).
- **Keeps** shell as PID 1 (no full Rust-init rewrite) — deliberately, to
  preserve busybox-only robustness and the fast-boot story.

### 3.6 Tests

- `vmette-proto`: round-trip `BootParams` serde; `proto_version` mismatch is
  representable.
- `vmette`: `cmdline::build` emits exactly `console=… vmette.boot=ctl
  [vmette.vsock_port=…]` and nothing else; `Session::start` writes a
  `boot.env` whose `from_env` round-trips to the `Config`-derived `BootParams`.
- `BootParams::{to_env,from_env}` round-trip incl. base64'd `exec`/`env` and the
  `proto_version` mismatch case.
- `tests/run.sh`: existing end-to-end gates must stay green (real VM boot).

---

## 4. C2 — One execution substrate

### 4.1 Current state

Three fork paths, one in-process path:

- `lifecycle::run` → `Session` (in-process; but serial = host stdio, so output
  is *streamed to the terminal*, never captured).
- `run_workload` (`main.rs:367`) → forks `vmette` CLI, reads child stdout/stderr
  line-framed into `Frame::Stdout/Stderr/Exit`.
- `Sandbox::run` (`sandbox.rs:164`) → forks `vmette` CLI, captures bounded
  output, slices between markers.
- `registry::start` → `Session` (in-process; Agent workload, vsock control).

### 4.2 Target design

**Step 1 — capture-aware serial.** `vz/config::build` gains a serial-attachment
mode. Today (`config.rs:63-69`) it always binds `VZFileHandleSerialPortAttachment`
to `fileHandleWithStandardInput/Output`. Target: a `SerialSink` parameter:

```rust
pub(crate) enum SerialSink {
    Inherit,                 // current behavior: host stdin/stdout (interactive CLI)
    Capture(RawFd, RawFd),   // write end of a host pipe for stdout; /dev/null stdin
}
```

`Session::start` chooses the sink from a new `Config` field (default `Inherit`).
In `Capture` mode the `Session` owns the read ends and exposes them.

**Step 2 — `RunOutput` carries captured streams.** Extend the existing
`RunOutput` (`lifecycle.rs:18-23`, today only `exit_code`) and add a blocking
`Session::wait_captured() -> RunOutput { exit_code, stdout, stderr }`. Output is
drained on the Session's owning thread, bounded by a cap (port the
`OUTPUT_CAP_BYTES` logic from `sandbox.rs:243-293` into the library as the single
owner).

**Console topology (REVISED after Phase 0 — this corrects the original
"add a second console" sketch).** Phase 0 found that two consoles are *not*
sufficient for clean capture. `console=hvc0` carries **kernel** boot/shutdown
messages, and `/init` logs its `[init] …` chatter to **fd 2**
(`custom-init.sh:36 log() { echo "[init] $*" >&2; }`); the exec inherits that
console for both streams. So "stdout→hvc0, stderr→hvc1" would leave kernel lines
polluting captured stdout and `[init]` lines polluting captured stderr — i.e. it
just relocates the marker-scraping problem instead of deleting it. To actually
delete `slice_exec_output`, the captured consoles must be **exec-dedicated**:

- **hvc0** — kernel console + `/init` logs (Inherit on the CLI path; discarded /
  drained on the headless daemon/MCP path). The kernel and init keep this.
- **hvc1** — exec **stdout** (Capture). `/init` redirects the user command's
  fd 1 here (`exec >/dev/hvc1`) just before running it.
- **hvc2** — exec **stderr** (Capture). Likewise fd 2 → `/dev/hvc2`.

The host reads hvc1/hvc2 as clean, separated streams. **Phase 0 spike result:
this 3-console shape validates** (`serial_capture_spike` case [5] VALID:
`three (kernel + exec out/err) → exec-dedicated clean capture feasible`). The
`SerialSink` enum in Step 1 generalizes to a per-console list rather than a
single `Capture(RawFd, RawFd)`.

> **Alternative under consideration (decide in PLAN, gate G2-capture):** instead
> of exec-dedicated consoles, have `/init` redirect the user command's
> stdout/stderr to two files on the **`ctl` share** (which C1 already mounts) and
> let the host read them after exit. This gives perfectly clean separation with
> *zero* console multiplexing and no kernel/init pollution — at the cost of
> losing **incremental streaming** (output is available only at exit). The
> console path preserves the daemon's current streaming `Frame::Stdout` behavior;
> the ctl-file path does not. The daemon streams today, so the console topology
> is the default; the ctl-file option is the fallback if hvc1/hvc2 guest-side
> routing proves troublesome.

**Fallback (single-stream).** If neither clean-separation option holds up under a
real boot, surface one captured stream as `stdout` with `stderr` empty — still
strictly better than marker-scraping, since the clean **exit code** already
arrives via the `.vmette-exit`/`ctl` channel independent of the console.

**Step 3 — collapse the paths.**

- `vmette-daemon`: `run_workload` is rewritten to call an in-process helper that
  builds a `Config` from the `Request` (the daemon already builds `Config` for
  desktops in `registry.rs`), starts a `Capture` `Session`, and streams/returns
  output as `Frame`s. The subprocess `Command`, `kill_on_drop`, the two reader
  tasks, and `locate_vmette` are deleted. The `dispatch` router keeps its
  `desktop_*` vs run split, but both arms now terminate in-process.
- `vmette-mcp`: `Sandbox` is rewritten to call the same in-process helper
  directly (no daemon hop, as today). `wrap_exec`, `slice_exec_output`,
  `MARKER_BEGIN/END`, and `read_capped` are deleted.
- `vmette-proto`: `Request::to_cli_args` is deleted. `Request` remains the
  daemon wire type but now maps to a `Config` via a new `Request::to_config`
  (or a `From<Request> for Config`-style builder) owned in `vmette-proto` or
  `vmette` — the single owner of `Request → Config`, replacing the single owner
  of `Request → argv`.
- `vmette-cli`: `run()` becomes the only place serial stays `Inherit`; the CLI
  `main` builds `Config` and calls `run()`.

### 4.3 Behavior & invariants

- Output semantics observable to MCP/daemon clients are preserved: a
  `Frame::Stdout`/`Stderr` stream followed by `Frame::Exit { code }`; the MCP
  `RunReply { stdout, stderr, exit }` shape is unchanged.
- The 1 MiB output cap and its truncation marker are preserved (moved into the
  library).
- The host-side wall-clock guard in `sandbox.rs:164-179` is preserved as an
  in-process timeout around `Session::wait_captured`.

### 4.4 Trade-offs

- **Loses** free OS-level fault isolation: a VZ crash in a one-shot run now
  faults the daemon/MCP process rather than a throwaway child. **This is the
  central risk of the whole proposal.** Mitigation and acceptance: the desktop
  registry already runs VZ in-process and already accepts this risk; C2 makes
  the one-shot path consistent with a decision already made. The PLAN gates C2
  behind a stability spike (§Risk-G2) and a soak test before deleting the
  subprocess path.
- **Removes** ~50 ms fork/exec per one-shot call, the argv round-trip, and three
  copies of output handling (~400 lines of `sandbox.rs` + the `run_workload`
  reader machinery + `to_cli_args`).
- **Requires** the daemon and MCP binaries to be codesigned for one-shot work.
  They already must be (the daemon boots desktops in-process; the MCP server can
  too) — no new signing constraint.

### 4.5 Tests

- `vmette`: `Capture` session over a known exec returns exact stdout/stderr/code;
  output cap truncates at the boundary with the marker.
- `vmette-daemon`: a run request returns the same `Frame` sequence as before
  (golden test against recorded frames).
- `vmette-mcp`: `Sandbox::run` returns identical `RunReply` for a fixture exec.
- Delete `sandbox.rs` marker tests and `daemon.rs` `to_cli_args` tests as their
  subjects are removed.
- `tests/run.sh`: full end-to-end stays green.

---

## 5. C3 — Remove vestigial snapshot surface

### 5.1 Scope of removal (feature-gated, default off)

Place all of the following behind `#[cfg(feature = "snapshot")]` (a non-default
cargo feature), or delete outright pending the PLAN decision:

- `Config::{build_snapshot, resume_snapshot, guest_vsock_port}` and their CLI
  flags (`--build-snapshot`, `--resume-snapshot`, `--guest-vsock-port`).
- `lifecycle::run` snapshot dispatch (`lifecycle.rs:33-41`).
- `cmdline.rs:82-88` snapshot tokens (already removed by C1, but the Config
  fields feeding them go here).
- `vz/snapshot.rs` (the stub module).
- `ListenerMode::Echo` and the `READY\n` sliding-window detector in
  `vz/vsock.rs` — with snapshot gone, `WorkloadStrategy::OneShot` no longer needs
  a vsock listener at all unless an exec wants the agent channel. The
  `session.rs:450-453` `OneShot => ListenerMode::Echo` arm is removed; OneShot
  with vsock disabled instantiates no listener.
- `scripts/custom-init.sh` `snapshot_mode=server` branch and the
  `.vmette-runner.sh` heredoc machinery.
- `guest/vsock-runner.c` injection into the initramfs (the source stays in-tree
  but is no longer built into the default initramfs).

### 5.2 Behavior

- `--build-snapshot`/`--resume-snapshot` either disappear from `--help` (deletion)
  or return a clear "built without snapshot support" error (feature-gated). The
  current behavior is `SnapshotUnsupported` on all arches, so no working
  capability is lost.
- `CHANGELOG.md` records the removal of the `--build-snapshot`/`--resume-snapshot`
  flags (observable surface, per the repo's changelog policy).

### 5.3 Trade-offs

- **Decision (PLAN §Risk-G3):** delete vs feature-gate. Deletion is cleaner and
  removes the most code; feature-gating preserves the scaffolding for the eventual
  Phase 5 at the cost of carrying `cfg` noise. Recommendation: **delete**, and
  reintroduce in the PR that actually implements snapshots — the git history is
  the archive.

### 5.4 Tests

- Removal is validated by green `cargo test --workspace` with the snapshot tests
  deleted, and `tests/run.sh` green (snapshot was never exercised there).

---

## 6. C4 — Multiplexed desktop vsock codec

### 6.1 Current state

`AgentConn::request` (`session.rs:155-182`) holds `io: Mutex<()>` across a full
send-action/read-response round-trip on **one** vsock fd with no request-id. Any
I/O error invalidates the entire fd (`invalidate_fd`, `session.rs:186-190`),
killing the session's GUI channel. The registry shares one `SessionClient` across
the VNC view, `screenshot_when_settled` polling (`registry.rs`), and direct
actions — all serialized on that mutex.

### 6.2 Target design

Extend the framed codec (`desktop.rs`, types in `vmette-proto::agent`) with a
4-byte request-id prefix:

```
[u32 req_id][u32 header_len][JSON header][optional binary payload]
```

The host assigns monotonically increasing `req_id`s; the guest echoes the
`req_id` in its response header. A small host-side demultiplexer maps responses
to waiting callers, so capture/settle-polling and input no longer serialize, and
a malformed/timed-out single response is dropped per-request instead of
desyncing the stream. The guest agent (`guest/vmette-desktop-agent.c`) is updated
to read and echo `req_id`.

This is **lower priority** and explicitly optional for the first delivery; it is
specified here for completeness and may land after C1–C3.

### 6.3 Trade-offs

- Adds complexity to a wire format whose current simplicity is a feature.
  Justified only by the concurrent-access contention the registry already
  exhibits. If profiling shows the contention is not material, C4 may be
  deferred indefinitely.

### 6.4 Tests

- `vmette-proto`/`desktop.rs`: codec round-trip with `req_id`; out-of-order
  responses demultiplex correctly; a dropped response does not block other ids.

---

## 7. C5 — Lower-tier consolidation

Each item is independent and low-risk.

1. **`run()` returns instead of `process::exit`.** `lifecycle::run`
   (`lifecycle.rs:32-98`) currently calls `std::process::exit` and documents an
   explicit `drop(session)` to run teardown guards exit would skip
   (`lifecycle.rs:64-69`). Target: `run()` restores the terminal, drops the
   session, and **returns** `RunOutput`; `vmette-cli::main` maps `RunOutput` to
   the process exit code. The FFI `vmette_run` (`ffi.rs`) loses its "never
   returns" contract.

2. **Single daemon-client crate.** The CLI (`vmette-cli/src/desktop.rs`, sync)
   and MCP (`vmette-mcp/src/daemon_client.rs`, async) each hand-roll
   connect/auto-spawn/write-line/read-reply/match-`DesktopReply`. Extract one
   sync transport into a new `vmette-daemon-client` crate; MCP wraps it in
   `spawn_blocking` (it already hops threads for the round-trip).

3. **`CaTrust` owner.** CA-cert trust policy is re-derived in ≥3 Rust call sites
   plus multi-distro shell in `custom-init.sh`. Introduce one `CaTrust` type
   (in `vmette-assets` or `vmette`) consumed by every boot path; the guest-side
   trust-store munging stays in shell but is fed a single resolved cert set via a
   share, applied once.

4. **`Config` rootfs enum.** Collapse the mutually-exclusive
   `Config::{rootfs_share, rootfs_block}` (`lib.rs`) into one
   `enum Rootfs { Share(RootfsShare), Block(RootfsBlock) }`, mirroring the
   existing `RootfsArtifact` that is currently flattened back into two nullable
   fields on the way into `Config`. Remove `Config::quiet` from the library type
   (it is a CLI presentation concern; pass it to `run()`/banner separately).

### 7.1 Tests

- `run()` returning is covered by a CLI integration test asserting exit codes for
  exit/timeout/stop/error.
- The daemon-client crate gets the connect/auto-spawn unit tests currently
  duplicated.
- `Config` rootfs enum: a compile-time exhaustiveness check replaces the runtime
  "block branch wins" invariant.

---

## 8. Compatibility & migration

- **Wire protocols:** the `vmetted` socket `Request`/`Frame`/`DesktopRequest`/
  `DesktopReply` shapes are unchanged on the wire (C2 changes the daemon's
  *internal* execution, not the socket contract; `to_cli_args` was never on the
  wire). C4's `req_id` is a vsock-internal change between host and guest agent,
  both shipped together — no external compatibility surface.
- **Guest/host lockstep:** C1 and C4 change the host↔guest contract. Because the
  guest (`initramfs-vmette`, desktop agent) and host are built and shipped
  together, and `BOOT_PROTO_VERSION` makes a mismatch a loud failure, a stale
  initramfs fails closed instead of silently misbehaving (today's failure mode,
  per CLAUDE.md). **The `build-initramfs.sh` rebuild after `custom-init.sh`
  edits becomes a checked step (PLAN), not a documented footgun.**
- **CHANGELOG:** C3 (snapshot flag removal) and C5.1 (FFI `vmette_run` no longer
  exits) are observable and must be recorded. C1, C2, C4 are internal and do not
  belong in `CHANGELOG.md` per repo policy.

---

## 9. Decisions (resolved in Phase 0 — see PLAN §0)

| ID | Decision | Outcome |
|----|----------|---------|
| G1 | Guest boot-param format | ✅ **`KEY=VALUE` envelope on `ctl` share**, base64 for `exec`/`env`, no parser binary (reuses guest's existing `base64 -d`). JSON+helper-binary dropped. |
| G2 | Stream separation | ✅ **Two-console (hvc0/hvc1)** — VZ validator accepts multiple consoles + pipe-fd attachments (spike). Single-stream+structured-exit is the backstop. Guest-side `hvc1` routing still boot-gated. |
| G3 | Snapshot: delete vs gate | ✅ **Delete** (reintroduce when implemented). |
| G4 | C4 scope | ✅ **Defer** pending contention profiling. |

Outstanding evidence (not a decision): the G2-stability in-process soak must run
green on a signed build before C2's subprocess deletion (PLAN Phase 2e).
