# Rootfs providers

A **rootfs provider** resolves a string spec (`alpine:3.20`, `./rootfs`,
`tar+https://h/r.tgz`, …) into something the guest can boot as `/`. Providers
are the seam third-party code uses to teach vmette about new rootfs sources
without touching the core crate.

This document covers the existing model and the design for two additions:

1. **Registry authentication** for the OCI provider (private images, e.g. ghcr).
2. A **squashfs provider** that attaches a prebuilt filesystem image as a
   read-only block device with a writable overlay — no host-side extraction.

Every non-obvious assumption below was checked against a live VM
(`spikes/custom-init-sqfs-spike.sh`); those findings are tagged **[spike]**
inline and summarised in [Spike evidence](#spike-evidence). At the time of
writing the design carries **no open unknowns** — both spikes passed.

---

## Current model

```
spec ──▶ Registry::resolve ──▶ first provider whose matches() is true
                                      │
                                      ▼
                               provide(spec, ctx) ─▶ host directory
                                      │
                                      ▼
                    Config.rootfs_share ─▶ VZ virtio-fs (tag "rootfs")
                                      │
                                      ▼
              custom-init.sh: mount -t virtiofs rootfs /newroot
```

| crate                    | scheme              | matches                                       |
|--------------------------|---------------------|-----------------------------------------------|
| `vmette` (`DirProvider`) | `dir`               | absolute, `./`, `../`, `~/` paths             |
| `vmette-provider-oci`    | `oci` (+ bare refs) | `oci://…`, otherwise any non-path non-scheme  |
| `vmette-provider-tar`    | `tar`               | `tar+http://`, `tar+https://`, `tar+file://`  |

The trait is intentionally small and pure:

```rust
pub trait RootfsProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, spec: &str) -> bool;                       // pure string check
    fn provide(&self, spec: &str, ctx: &Context) -> Result<PathBuf, ProviderError>;
}
```

The return type — a host **directory** — is the limiting assumption. It bakes in
"the rootfs is delivered over virtio-fs." Registry auth lives entirely inside the
OCI crate and does not touch it. Squashfs does, because a filesystem image is a
*block device*, not a directory share.

---

## Part 0 — The shared seam change: `RootfsArtifact`

`provide() -> PathBuf` can only express "a directory to virtio-fs share." A
squashfs image is a *file* attached as a *block device* and mounted in-guest.
The modular fix is to let a provider describe **what** it produced, leaving the
core to decide **how** it is mounted:

```rust
// vmette::provider
pub enum RootfsArtifact {
    /// Host directory, shared read[-only] over virtio-fs (today's behaviour).
    Directory { path: PathBuf, read_only: bool },
    /// Raw filesystem image, attached as a read-only virtio-blk device and
    /// mounted in-guest with `fstype`, with a tmpfs overlay for writes.
    BlockImage { path: PathBuf, fstype: BlockFs },
}

pub enum BlockFs { Squashfs }   // erofs intentionally absent — see caveats

pub trait RootfsProvider {
    fn provide(&self, spec: &str, ctx: &Context) -> Result<RootfsArtifact, ProviderError>;
}
```

This is the **only** trait break. Migration is mechanical — `dir`/`oci`/`tar`
each change one line (`Ok(path)` → `Ok(RootfsArtifact::Directory { path, read_only: false })`).

The payoff is the modularity guarantee — each concern lives in exactly one place,
and a future format touches one cell, not a vertical slice:

| Concern                              | Owner                         | Never touches            |
|--------------------------------------|-------------------------------|--------------------------|
| "What artifact did we produce?"      | the provider                  | VZ, cmdline, guest init  |
| Artifact → VZ device + cmdline token | core (one mapping function)   | registry/network details |
| In-guest mount + overlay             | `custom-init.sh` (one branch) | host-side anything       |
| Credential resolution                | OCI crate's `AuthResolver`    | the core seam, peers     |

---

## Part A — Registry authentication (OCI crate only)

Today the OCI provider hardcodes `RegistryAuth::Anonymous`, so private images
(e.g. `ghcr.io/chamuka-inc/vmette-desktop:latest`) fail with *Not authorized*.
This is the only reason the desktop workflow ever fell back to `docker export`.

Keep credential **resolution** (where creds come from) separate from credential
**use** (the pull), behind a strategy trait that lives inside
`vmette-provider-oci`:

```rust
pub trait AuthResolver: Send + Sync {
    /// Resolve auth for a specific registry host (e.g. "ghcr.io").
    fn resolve(&self, registry: &str) -> RegistryAuth;
}
```

`DefaultAuthResolver` chains sources in precedence order, **keyed per-registry**
so a ghcr token is never sent to docker.io and never leaks across a redirect:

1. **Programmatic override** — `OciProvider::with_auth(resolver)`. Lets the
   daemon/MCP inject creds with no env at all.
2. **Env (the ghcr fix, no docker):** `VMETTE_OCI_TOKEN` → `Basic("<user>", token)`.
   Optional per-host `VMETTE_OCI_AUTH_<HOST>` for multi-registry setups.
3. **`~/.docker/config.json`** — best-effort read of `auths[registry].auth`
   (base64 `user:pass`). This reads a *file*; it is **not** a docker runtime
   dependency. `credsStore` / `credHelpers` (which shell out to external
   binaries) are out of scope — skip and fall through.
4. **Anonymous** — unchanged default, so every current call behaves identically.

**[spike]** `oci_client 0.17` exposes `RegistryAuth::{Anonymous, Basic(u,p),
Bearer(t)}`. Use **`Basic(user, PAT)`**: `oci_client` automatically performs the
`WWW-Authenticate: Bearer` token exchange that ghcr requires. Raw `Bearer(t)`
sends `Authorization: Bearer t` directly and only works with a pre-scoped
registry token, **not** a personal access token — wrong primitive for the env
path.

**Wiring.** Resolve once at the top of `pull_with_options`
(`let auth = self.auth.resolve(reference.registry());`) and pass that same value
to both `fetch_manifest_digest` and `client.pull`. Nothing else changes — cache
layout, TTL, and offline fallback are untouched. `AuthResolver` is a trait so it
is unit-testable (mock returns `Basic`; assert it is threaded through both calls)
and third-party-overridable.

This part needs **no core changes and no asset rebuild**; it is the smallest,
most self-contained win and removes the only reason to reach for docker.

> The one item not yet exercised against a live registry is the `Basic → bearer`
> handshake against ghcr itself, which needs a real token. The API choice is
> confirmed; only the network round-trip is unverified.

---

## Part B — Squashfs provider

### Why prebuilt, not host-built

The win — "no extraction, instant attach, read-only base shared across sessions,
far less host disk" — only materialises if the image is **already** a squashfs.
Building one on the host would need `mksquashfs` on macOS (**[spike]** not
present) *and* would still extract to host disk first, defeating the point.

So the v1 squashfs provider **consumes a prebuilt image**
(`squashfs+file://`, `squashfs+https://`). The image is produced in CI on Linux
(where `mksquashfs` is native), shipped as one artifact, and attached directly.
**[spike]** building one inside a networked vmette VM works dependency-free, so
CI needs no special tooling beyond a Linux runner.

### Three self-contained layers

```
squashfs+file:///img.sqfs
   │  vmette-provider-squashfs
   ▼
RootfsArtifact::BlockImage { path: <cached .sqfs>, fstype: Squashfs }
   │  core: vz/config.rs + cmdline.rs
   ▼
VZ virtio-blk (read-only, slot 0)  +  vmette.rootfs_block=squashfs  +  control share
   │  custom-init.sh (block branch)
   ▼
modprobe squashfs overlay
mount -t squashfs -o ro /dev/vda /lower
mount -t tmpfs tmpfs /ovl
mount -t overlay overlay -o lowerdir=/lower,upperdir=/ovl/upper,workdir=/ovl/work /newroot
   │  then the existing switch_root / desktop / exec logic, unchanged
```

**1. `crates/vmette-provider-squashfs`** (sibling to tar/oci)
- `matches`: `squashfs+file://`, `squashfs+https://`, `squashfs+http://`.
- `provide`: resolve to a cached local `.sqfs` (file:// → use in place; http →
  download to cache using the **same streaming extracted-size cap** pattern as
  the tar provider, since this is one large file), then return
  `RootfsArtifact::BlockImage { path, fstype: Squashfs }`.
- Knows nothing about VZ. It just hands back a file path + fstype tag.

**2. Core mount-strategy** (`vz/config.rs`, `cmdline.rs`, a `Config.rootfs_block` field)
- `vz/config.rs`: the current `disks` loop hardcodes `readOnly=false`. Add a
  **read-only** virtio-blk attach for the rootfs image
  (`VZDiskImageStorageDeviceAttachment(..., readOnly: true)`), emitted as
  **storage slot 0** so it is deterministically `/dev/vda`; user `--disk`s
  follow. **[spike]** a `.sqfs` attaches fine and enumerates as `/dev/vda`
  (major 253); VZ accepts it because `mksquashfs` pads output to 4 KiB, so the
  image length is block-aligned (observed 4096 and 4 059 136 bytes, both
  `% 4096 == 0`).
- `cmdline.rs`: emit `vmette.rootfs_block=squashfs` instead of `vmette.rootfs=1`,
  **and auto-attach a control share** (see exit codes below). The core picks the
  token from the `RootfsArtifact` variant — providers never do.

**3. Guest init** (`custom-init.sh`, new step-3 branch)
- Add `squashfs` and `overlay` to the modprobe list. **[spike]** both ship as
  modules (`CONFIG_SQUASHFS=m`, `CONFIG_OVERLAY_FS=m`) and `squashfs.ko.gz` +
  `overlay.ko.gz` are **already inside the live initramfs**, so no asset-pipeline
  change is needed — only the modprobe line.
- New branch keyed on `vmette.rootfs_block=<fstype>`: mount `/dev/vda` read-only
  at `/lower`, a tmpfs at `/ovl`, then an overlay at `/newroot`. Fall through to
  the existing share / switch_root / desktop / exec logic **unchanged** — it all
  operates on `/newroot`.
- The branch is **fstype-agnostic** (`mount -t $FS`). The overlay is mandatory:
  the guest needs a writable `/` (it writes `.vmette-exit`, tmp, desktop logs).
  tmpfs-over-squashfs = an ephemeral writable root discarded on shutdown, which
  *is* the sandbox semantic.

### Exit-code propagation — the gap the spike caught

For the virtio-fs rootfs, the host recovers the guest's exit code by reading
`.vmette-exit` back from the **shared rootfs directory** (it is a host directory,
so the write is directly visible). A block/overlay rootfs has **no host-visible
writable surface** — `.vmette-exit` lands in the ephemeral tmpfs upper and is
lost. **[spike]** confirmed: the guest exits 42, but the host reports 1
(`.vmette-exit missing`).

**Fix (proven).** Block-rootfs mode auto-attaches a tiny **writable virtio-fs
control share** (tag `ctl`, backed by a per-session host temp dir). The init
writes `.vmette-exit` there; the host reads it from that dir.

```
--share ctl=<session-tmp>   (added automatically when rootfs is a block image)
guest: echo "$RC" > /ctl/.vmette-exit
host : read <session-tmp>/.vmette-exit   # got "42" in the spike
```

So `lifecycle.rs` / `session.rs` must, when the rootfs is a `BlockImage`, read
the exit code from the control share instead of from the rootfs dir. This is the
one host-side code change beyond device attach.

For the **desktop (switch_root) path**, the control share must be
`mount --move`'d into `/newroot/mnt/ctl` *before* the pivot so the post-pivot
process can still reach it — `switch_root` only relocates `/proc /sys /dev /run`
automatically.

---

## Spike evidence

Every claim a reviewer might doubt was settled against a live x86_64 VM. Both
spikes passed; nothing in the design's scope is left unverified.

| # | Question | Result |
|---|----------|--------|
| 1 | Does the guest kernel support squashfs + overlay? | **Yes** — `CONFIG_SQUASHFS=m`, `CONFIG_OVERLAY_FS=m`; both `.ko.gz` already in the live initramfs (modprobe-line change only). |
| 2 | Does VZ accept a `.sqfs` as virtio-blk, and what's the device? | **Yes** → `/dev/vda` (major 253). `mksquashfs` pads to 4 KiB, satisfying VZ block-alignment. |
| 3 | Does the tmpfs-over-squashfs overlay work (read lower, write upper, copy-up)? | **Yes** — `overlay on / (rw, lowerdir=/lower, upperdir=/ovl/upper, workdir=/ovl/work)`; copy-up of a lower file succeeds, lower untouched. |
| 4 | How does the host recover the exit code with no writable rootfs surface? | **Control-share side-channel** — guest writes `/ctl/.vmette-exit`, host reads `<session-tmp>/.vmette-exit`; round-tripped a `42`. |
| 5 | Does the overlay survive `switch_root` (the desktop path)? | **Yes** — after `exec switch_root /newroot …`, squashfs lower still readable, tmpfs upper still writable, `/proc/mounts` still shows the original overlay. Kernel keeps mount refs alive even though the old mountpoint dirs are gone. Needs the `mount --move` of `ctl` before the pivot. |
| 6 | Can two VMs share **one** `.sqfs` read-only concurrently? | **Yes** — `readOnly: true` attach; two VMs booted the same image simultaneously, both read a baked-in marker, both exited 0 within the same second. VZ takes no exclusive lock. Validates "one base shared across many live sessions." |
| — | OCI auth API for ghcr | `RegistryAuth::Basic(user, PAT)` (oci_client 0.17) performs the bearer exchange. Live ghcr round-trip pending a real token. |
| — | `mksquashfs` on macOS host | Not present — confirms images must be CI-built (or built inside a networked vmette VM, which works dependency-free). |

---

## Implementation checklist

| Area | Change |
|------|--------|
| `vmette` core | `RootfsArtifact` enum; `Registry::resolve` returns it; `Config.rootfs_block: Option<RootfsBlock>` |
| `vz/config.rs` | read-only virtio-blk attach for the rootfs image, emitted as storage slot 0 |
| `cmdline.rs` | emit `vmette.rootfs_block=<fstype>`; auto-add the `ctl` control share |
| `lifecycle.rs` / `session.rs` | read `.vmette-exit` from the control share when rootfs is a block image |
| `scripts/custom-init.sh` | add `squashfs`/`overlay` to modprobe; block-rootfs branch; control-share exit channel; `mount --move` of `ctl` before switch_root |
| `crates/vmette-provider-squashfs` | new crate; `squashfs+{file,https,http}://`; streaming-capped download; returns `BlockImage` |
| `crates/vmette-provider-oci` | `AuthResolver` trait + `DefaultAuthResolver` (env → docker-config → anonymous); resolve once, thread into both calls |
| `dir` / `tar` providers | one-line return-type migration to `RootfsArtifact::Directory` |

Reg auth and squashfs are independent; **do reg auth first** — it is the
smallest change, needs no asset rebuild, and unblocks private desktop images
immediately.

---

## Caveats and scope

- **No erofs.** **[spike]** `CONFIG_EROFS_FS` is not set in the guest kernel, so
  `BlockFs` carries `Squashfs` only. Adding erofs would require rebuilding the
  kernel; defer until there is a reason.
- **Snapshot interaction is out of scope for v1.** Snapshot/restore is
  Apple-Silicon-only and assumes the virtio-fs rootfs; a block-image rootfs plus
  snapshot is untested. Scope squashfs to the non-snapshot path and document it.
- **Initramfs rebuild reminder.** Editing `custom-init.sh` requires
  `bash scripts/build-initramfs.sh` — the live `assets/initramfs-vmette` embeds a
  *copy*, and a stale initramfs silently ignores the new branch.
- **`credsStore` / `credHelpers`** (external credential binaries) are not
  supported by the OCI `AuthResolver` in v1.
- **ghcr live handshake unverified.** The `Basic(user, PAT)` API is confirmed,
  but the actual token exchange against ghcr has not been run (needs a real
  token). Low risk — it is `oci_client`'s standard, documented path.
