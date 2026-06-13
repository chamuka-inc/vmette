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

3. ~~Snapshot/restore is vestigial.~~ **(Corrected — see C3/§5.)** Snapshot is a
   real, planned **Apple-Silicon** feature (Phase 5): `vz/snapshot.rs` is
   `TODO(phase 5)` *stubs* today (both arches return `SnapshotUnsupported`), but
   VZ's `saveMachineStateToURL:` is arm64-gated and the daemon's warm pool depends
   on it. The scaffolding (`Config::{build_snapshot,resume_snapshot}`,
   `lifecycle.rs:33-41`, `vz/snapshot.rs`, the `custom-init.sh` snapshot branch,
   `vsock-runner.c`, `ListenerMode::Echo`) is **kept**; C3 integrates snapshot into
   the `boot.env` contract rather than deleting it.

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
- Snapshot/restore is **not** implemented here (Phase 5), but it is **preserved**
  and integrated into the boot contract — not removed. It is a real
  Apple-Silicon feature, not vestigial.

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
   Config ──► Session::start ─────────┘                 └──── reads ctl/boot.env
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
| C3 | **Preserve** snapshot (real Apple-Silicon Phase-5 feature); integrate it into the `boot.env` contract (`Strategy::Snapshot`) | `vmette-proto`, `vmette`, `scripts/` |
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

- The host writes `boot.env` and creates the `ctl` share for **every** workload
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

**Console topology — FINAL, empirically validated on real boots (Phase 0).**
This section went through three drafts; only the last is correct, and it is
backed by booting VMs (`three_console_boot_spike`), not just config validation.

Phase 0 discovered a hard Virtualization.framework constraint: **VZ delivers
host data for only ONE virtio console serial port reliably.** Booting with N
all-capture consoles and writing a distinct marker to each `/dev/hvc{k}`, only
`hvc0` ever reached the host (`n=1/2/3/4` → delivered `1/N` every time), even
though the guest enumerates `/dev/hvc1…` and writes to them succeed (`rc=0`).
So **multi-console stdout/stderr separation is NOT viable** — this rules out both
the original "second console for stderr" sketch and the interim "3 exec-dedicated
consoles" design (the latter passed *config validation* but failed at *boot*,
which is exactly why boot-validation was a required gate).

The validated design instead uses **one clean streaming console plus the `ctl`
share**:

- **hvc0 — the single captured, streaming console**, carrying *only* the exec's
  output. Validated clean: `three_console_boot_spike` clean-primary case returned
  `got=true clean=true` (the exec markers, with no `[init]`/kernel/overlay noise).
- **`console=hvc1` — a discard sink.** A second console port the host does not
  read; the kernel cmdline sets `console=hvc1` so kernel printk and `/init`'s
  `[init]` chatter (`custom-init.sh:36 log(){ echo … >&2; }`, fd 2 = `/dev/console`)
  land there, off hvc0. This is what makes hvc0 clean — and it works *because*
  the discard port doesn't need host delivery (which VZ wouldn't provide anyway).
- **`ctl` virtio-fs share — stdout/stderr separation + exit code.** virtio-fs is
  rock-solid (300/300 boots in the soak, every boot mounts `ctl`). The clean exit
  code already arrives via `.vmette-exit` here. If a consumer needs stdout and
  stderr *separated* (the daemon `Frame::Stdout`/`Stderr` distinction), `/init`
  tees the exec's fd 2 to a `ctl` file while fd 1+2 stream combined on hvc0; the
  host merges the post-exit stderr file with the stream. Most agent use cases
  want combined terminal-style output, so separated streams are the exception.

**Guest change (C2):** the host adds the 2nd (ignored) console and sets
`console=hvc1`; `/init` runs the user command with stdout+stderr redirected to
`/dev/hvc0` (`sh -c "$CMD" >/dev/hvc0 2>&1`), keeping its own logs and the kernel
on hvc1. This **deletes `slice_exec_output`/marker-scraping AND preserves
incremental streaming** — both C2 goals met.

**Step 1 — capture-aware serial.** `vz/config::build` gains a `SerialSink`:

```rust
pub(crate) enum SerialSink {
    Inherit,         // current behavior: host stdin/stdout (interactive CLI, hvc0)
    Capture(RawFd),  // hvc0 write end → host pipe; plus a discard 2nd console
}
```

In `Capture` mode `Session` adds the hvc0 capture port + the discard port, sets
`console=hvc1` on the cmdline, owns the hvc0 read end, and exposes it.

**Step 2 — `RunOutput` carries captured streams.** Extend `RunOutput`
(`lifecycle.rs:18-23`, today only `exit_code`) and add
`Session::wait_captured() -> RunOutput { exit_code, stdout, stderr }`, draining
hvc0 on the Session's owning thread, bounded by a cap (port `OUTPUT_CAP_BYTES`
from `sandbox.rs:243-293` into the library as the single owner). `stderr` is the
post-exit `ctl` file when separation is requested, else empty (combined on
`stdout`).

**Fallback (no consoles at all).** If the clean-primary console proves
problematic in some guest, redirect the exec's stdout/stderr to two `ctl` files
and read them after exit — perfectly clean, but **non-streaming**. Kept as a
backstop only; the clean-hvc0 path above is the validated default.

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

## 5. C3 — Preserve snapshot; integrate it into the boot contract

> **G3 REVERSED.** The Phase-0 reading was literally accurate — `vz/snapshot.rs`
> returns `SnapshotUnsupported` on both arches today — but the conclusion ("delete
> the vestigial surface") was wrong. **Snapshot/restore is a real, planned
> Apple-Silicon feature** (Phase 5): VZ's `saveMachineStateToURL:` /
> `restoreMachineStateFromURL:` are arm64-gated, the module already mirrors that
> with `cfg(target_arch = "aarch64")`, and the daemon's warm-snapshot pool depends
> on it. So nothing snapshot-related is deleted. Instead C3 makes snapshot
> **coherent with the C1 `boot.env` contract** so it isn't stranded by the cmdline
> shrink.

### 5.1 What is kept (NOT removed)

- `Config::{build_snapshot, resume_snapshot, guest_vsock_port}` + the CLI flags.
- `lifecycle::run` snapshot dispatch (routes to `vz::snapshot` before `Session`).
- `vz/snapshot.rs` (the `cfg(aarch64)` module; Phase 5 fills in the real flow).
- `ListenerMode::Echo` + the `READY\n` detector in `vz/vsock.rs` (the snapshot
  vsock-runner handshake; also serves the general OneShot vsock roundtrip).
- `scripts/custom-init.sh` snapshot branch and `guest/vsock-runner.c`.

### 5.2 What C1 changed, and the fix

C1's cmdline shrink removed the `vmette.snapshot_mode`/`vmette.guest_vsock_port`
tokens. Those were already **dead at that call site** — `lifecycle::run` returns
to `vz::snapshot` *before* `Session::start`, so `cmdline::build` was never invoked
on a snapshot config — so no working behavior broke. But it left the guest's
snapshot branch reading variables nothing set. The fix integrates snapshot into
the typed contract, the same channel as every other mode:

- `vmette_proto::boot::Strategy` gains a `Snapshot { guest_vsock_port }` variant.
- `vmette::boot::to_env`/`from_env` carry it (`VMETTE_STRATEGY=snapshot` +
  `VMETTE_GUEST_VSOCK_PORT`); the host vsock port stays on the cmdline.
- The guest snapshot branch keys off `VMETTE_STRATEGY=snapshot` and reads the
  guest port from `boot.env`.

The **producer** side (a `BootParams` with `Strategy::Snapshot`) is Phase 5's to
wire when `vz::snapshot::build` is implemented; the contract + guest consumer are
in place now so the feature is not stranded.

### 5.3 Behavior

- `--build-snapshot`/`--resume-snapshot` are unchanged: `SnapshotUnsupported`
  until Phase 5. No CHANGELOG entry (no observable change).

### 5.4 Tests

- `Strategy::Snapshot` round-trips through `to_env`/`from_env`.
- `tests/run.sh` stays green (the snapshot arch-guard + vsock-roundtrip gates).

### 5.5 Feasibility validated from Apple docs (not yet run on Apple silicon)

Snapshot has never been exercised on Apple silicon, so the Phase-5 feasibility was
checked against Apple's documentation — WWDC23 **"Create seamless experiences with
Virtualization"** (session 10007) and the `VZVirtualMachine` save/restore API
reference. The documented workflow is:

- **Save:** `pause()` → `saveMachineStateTo(url:)`; copy external resources (disk
  images) separately.
- **Restore:** create a `VZVirtualMachine` **from the same configuration** →
  `restoreMachineStateFrom(url:)` → `resume()`.
- macOS **14 (Sonoma)+**, **arm64 only** (`#if defined(__arm64__)`); save files are
  hardware-encrypted (bound to the Mac + user account) and versioned.

vmette's design (boot → guest `vsock-runner` signals READY and blocks on
`accept()` → host pauses + saves at that blocker → restore + `resume()` → runner
reads the per-request command) maps **1:1** onto this workflow — the `accept()`
blocker is exactly the "pause at a stable point" the docs call for. Consistency
check:

| Apple requirement | vmette | Status |
|---|---|---|
| Pause at a stable point before save | `accept()` blocker | ✅ matches |
| Restore VM built from the **same** configuration | resume must rebuild the identical `VZVirtualMachineConfiguration` (kernel, mem, device set, vsock port) | ⚠️ **Phase-5 must honor** — `resume_snapshot` has to reconstruct the matching config, not just the save path |
| External disk images copied separately | read-only rootfs (dir share / squashfs) is external + stable; writable overlay is tmpfs, captured in the memory state | ✅ fine for a warm pool |
| arm64 only | `cfg(target_arch = "aarch64")` already gates `vz/snapshot.rs` | ✅ matches |
| macOS 14+ | not currently gated | ⚠️ **Phase-5 must require macOS 14+** for the save/restore symbols |
| Hardware-encrypted, machine-bound save files | save + restore happen on the same host (in-process warm pool) | ✅ fine; rules out shipping prebuilt snapshots |

No documented restriction prevents saving a config with a `VZVirtioSocketDevice`
(vsock), virtio-fs share, or NAT network — the WWDC session states no device
restrictions, and a targeted search surfaced none. **The one residual unknown the
docs cannot close is device-level save/restore compatibility itself** (esp. vsock,
which the design depends on); that needs a real Apple-silicon boot in Phase 5.
Nothing in the documentation contraindicates the design.

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
| G2 | Capture topology | ✅ **Clean single-console (hvc0) streaming + `console=hvc1` discard sink + `ctl` share** — boot-validated. VZ delivers only ONE console port to the host, so multi-console separation is NOT viable; `ctl` virtio-fs carries stdout/stderr separation + exit. Soak: 300/300, fd drift 0. |
| G3 | Snapshot: delete vs keep | ✅ **REVERSED → keep.** Snapshot is a real Apple-Silicon Phase-5 feature, not vestigial. C3 preserves it and integrates it into the `boot.env` contract (`Strategy::Snapshot`) instead of deleting it. |
| G4 | C4 scope | ✅ **Defer** pending contention profiling. |

All Phase-0 gates closed with empirical evidence (boots + soak ran locally via
ad-hoc codesigning + fetched assets); no outstanding evidence remains.
