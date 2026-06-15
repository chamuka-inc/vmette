# vmette library API

*Embed it.* Most users reach for the CLI or the MCP server; this doc is for
building your own agent host or sandbox tooling on top of the same VM primitive.

Two surfaces:

1. **Rust** — `vmette` crate, idiomatic types, `pub use` re-exports.
2. **C** — `vmette.h` (cbindgen-generated, checked in), opaque
   pointers, paired `*_new` / `*_free`.

Both bind to the same underlying implementation; the C surface is a
mechanical shim over the Rust one, defined in
[`crates/vmette/src/ffi.rs`](../crates/vmette/src/ffi.rs).

## Rust

```toml
[dependencies]
vmette = "0.10"
```

```rust
use vmette::{Config, Rootfs, RootfsShare, ShareMount, VsockPort};

fn main() -> Result<(), vmette::Error> {
    let mut cfg = Config::new("./vmlinuz", "./initramfs");
    cfg.rootfs = Some(Rootfs::Share(RootfsShare {
        path: "./alpine-rootfs".into(),
        read_only: false,
    }));
    cfg.shares.push(ShareMount {
        tag: "host".into(),
        path: "/tmp/scratch".into(),
    });
    cfg.exec_cmd = Some("uname -a; exit 0".into());
    cfg.vsock_port = VsockPort::Auto;
    cfg.timeout_seconds = Some(30);

    // run() blocks until guest poweroff, then returns Ok(RunOutput)
    // carrying the guest's exit code (124 on timeout, 0 on a requested
    // stop, 1 on a guest error). It never exits the process — the caller
    // chooses the process exit code. Err is for setup failures (snapshot
    // unsupported, config invalid, VM failed to start).
    let out = vmette::run(&cfg)?;
    std::process::exit(out.exit_code);
}
```

### Boot assets

`Config::new(kernel, initramfs)` takes paths to a Linux kernel
(`vmlinuz-virt`) and vmette's repacked initramfs (`initramfs-vmette`).
These are **not distributed on crates.io** — the crate is the code, not the
~10 MB boot blobs. They live under `assets/<arch>/` (Apple Silicon uses
`aarch64`, Intel `x86_64`). Obtain them either from a
[GitHub release](https://github.com/chamuka-inc/vmette/releases) tarball or
from a checkout (`git clone https://github.com/chamuka-inc/vmette && make assets init`).

Rather than hard-coding paths, let the `vmette-assets` crate find them —
`vmette_assets::find("vmlinuz-virt")` searches `$VMETTE_ASSETS_DIR/<arch>`,
then `./assets/<arch>`, then the install prefix — and feed the results to
`Config::new`.

### Types

See [`crates/vmette/src/lib.rs`](../crates/vmette/src/lib.rs).

- `Config` — fields are `pub`; populate directly after `Config::new`.
  `set_rootfs_artifact(artifact, force_read_only)` applies a resolved
  `RootfsArtifact` to the right field for you. `env: Vec<(String, String)>`
  carries guest env vars (the CLI's `--env`), applied *after* any OCI image
  `Env` so they override the image's values.
- `VsockPort` — `Disabled` | `Auto` | `Fixed(u32)`.
- `Rootfs` — `Share(RootfsShare)` | `Block(RootfsBlock)`; the type of
  `Config.rootfs: Option<Rootfs>`. The two forms are mutually exclusive *by
  construction*.
- `RootfsShare { path, read_only }` — a host directory shared over
  virtio-fs; the payload of `Rootfs::Share`.
- `RootfsBlock { path, fstype }` — a filesystem image attached read-only as
  `/dev/vda` with a tmpfs overlay; the payload of `Rootfs::Block`.
- `RootfsArtifact` — what a provider's `provide()` produces:
  `Directory { path, read_only, image_env }` (virtio-fs share; `image_env`
  carries the image's declared env, merged into the run as described under
  `Config` above) or `BlockImage { path, fstype }` (read-only block device +
  tmpfs overlay).
- `BlockFs` — block-image filesystem tag; currently `Squashfs` only.
- `ShareMount { tag, path }`.
- `Error` (thiserror): `InvalidConfig`, `StartFailed`, `RestoreFailed`,
  `SaveFailed`, `SnapshotUnsupported`, `Timeout`, `Vsock`, `Io`.
- `RunOutput { exit_code: i32, output: String }` — returned by `run()` (and
  `Session::wait_captured`). `output` carries the guest's captured combined
  stdout+stderr when the session ran with `Config::capture_output` (empty for
  the interactive `run()` path, which streams to the terminal; truncated past
  ~1 MiB with a marker).

### Rootfs providers

For a fixed directory you can set `Config.rootfs` to `Rootfs::Share(..)`
directly. For the same `--rootfs SPEC` ergonomics the CLI offers — which may
resolve to a directory *or* a block image — the `vmette::provider` module
exposes a trait + registry; the registry's `resolve()` dispatches to the first
matching provider's `provide()`, returning a `RootfsArtifact`:

```rust
use vmette::provider::{Context, DirProvider, Registry};
use vmette_provider_oci::OciProvider;
use vmette_provider_squashfs::SquashfsProvider;
use vmette_provider_tar::TarProvider;

let registry = Registry::new()
    .with(DirProvider::new())       // claims path-like specs + bare-relative existing dirs
    .with(SquashfsProvider::new())  // claims squashfs+{file,http,https}://
    .with(TarProvider::new())       // claims tar+http(s)://, tar+file://
    .with(OciProvider::new());      // catch-all for bare refs + oci://

let ctx = Context::new("/Users/me/Library/Caches/vmette")
    .offline(false)
    .guest_helpers_dir(Some("/usr/local/share/vmette/guest".into()));

let artifact = registry.resolve("alpine:3.20", &ctx)?;   // RootfsArtifact
cfg.set_rootfs_artifact(artifact, /*force_read_only=*/ false);
```

To pull from a private OCI registry, inject credentials programmatically
with `OciProvider::with_auth(Arc::new(resolver))` (it takes
`Arc<dyn AuthResolver>`), or rely on the default resolver
(per-host `VMETTE_OCI_AUTH_<HOST>` → `VMETTE_OCI_TOKEN` → `~/.docker/config.json`
→ anonymous).

`RootfsProvider` is the trait third-party code implements to teach
vmette about new rootfs sources (S3 buckets, internal artifactories,
custom build pipelines). See [the tar provider crate]
(../crates/vmette-provider-tar/src/lib.rs) for a ~150-line reference
implementation.

### Snapshot — not yet implemented

Snapshot save/restore is **not implemented**. The `Config` fields
(`build_snapshot`, `resume_snapshot`) and C setters exist, but the underlying
objc2 save/restore flow hasn't landed: both paths unconditionally return
`Err(Error::SnapshotUnsupported)` (`SnapshotUnsupported` status in C) on **every
architecture**, including Apple Silicon. Calling `run()` with either field set
always errors today. Do not build against this surface yet.

## C ABI

```c
#include <stdio.h>
#include "vmette.h"

int main(int argc, char **argv) {
    vmette_config_t *cfg = vmette_config_new(argv[1], argv[2]);
    if (!cfg) return 1;

    vmette_config_set_rootfs_share(cfg, argv[3], /*read_only=*/false);
    vmette_config_set_exec(cfg, "echo hi from C; exit 7");
    vmette_config_set_vsock_port(cfg, 0);          /* auto */
    vmette_config_set_timeout(cfg, 30);
    vmette_config_set_vcpus(cfg, 1);
    vmette_config_set_mem_mib(cfg, 512);

    vmette_run_output_t *out = NULL;
    VmetteStatus rc = vmette_run(cfg, &out);   /* same blocking contract as Rust run() above */
    if (rc != Ok) {
        fprintf(stderr, "vmette_run: status %d\n", (int)rc);
        return 1;
    }
    int code = vmette_run_output_exit_code(out);
    vmette_run_output_free(out);
    vmette_config_free(cfg);
    return code;
}
```

### Build

```sh
clang -I ${PREFIX}/include -L ${PREFIX}/lib -lvmette \
      -Wl,-rpath,${PREFIX}/lib \
      -o my_app my_app.c

codesign --sign - --force \
    --entitlements ${PREFIX}/entitlements.plist \
    --options=runtime my_app
```

The entitlement *must* be applied to the executable that loads
libvmette — VZ checks the calling process's entitlement set, not the
dylib's.

### Reference

| C signature | Notes |
|-------------|-------|
| `vmette_config_t *vmette_config_new(const char *kernel, const char *initramfs);` | Returns NULL on null args / invalid UTF-8. |
| `void vmette_config_free(vmette_config_t *);` | No-op on NULL. |
| `void vmette_config_set_cmdline(cfg, str);` | Override default `console=hvc0 quiet`. |
| `void vmette_config_set_rootfs_share(cfg, path, bool ro);` | |
| `void vmette_config_add_share(cfg, tag, path);` | Repeatable. |
| `void vmette_config_add_disk(cfg, path);` | Repeatable. |
| `void vmette_config_add_env(cfg, key, value);` | Append a guest env var (overrides OCI image `Env`). Repeatable. |
| `void vmette_config_set_exec(cfg, cmd);` | |
| `void vmette_config_set_net(cfg, bool);` | |
| `void vmette_config_set_switch_root(cfg, bool);` | |
| `void vmette_config_set_vsock_port(cfg, int32_t);` | -1 disable / 0 auto / >0 fixed |
| `void vmette_config_set_guest_vsock_port(cfg, uint32_t);` | snapshot mode only |
| `void vmette_config_set_timeout(cfg, uint32_t);` | 0 = no timeout |
| `void vmette_config_set_vcpus(cfg, uint8_t);` | not clamped; a value VZ rejects (e.g. 0) surfaces as `InvalidConfig` from `vmette_run` |
| `void vmette_config_set_mem_mib(cfg, uint64_t);` | not clamped; a value VZ rejects surfaces as `InvalidConfig` from `vmette_run` |
| `void vmette_config_set_scratch_mib(cfg, uint64_t);` | ephemeral ext4 scratch disk (MiB) for the writable overlay upper; `0` disables (RAM-backed tmpfs). No effect with a read-only rootfs. |
| `void vmette_config_set_build_snapshot(cfg, path);` | Not yet implemented (see Snapshot section). |
| `void vmette_config_set_resume_snapshot(cfg, path);` | Not yet implemented (see Snapshot section); `vmette_run` returns `SnapshotUnsupported` before validating the config. |
| `VmetteStatus vmette_run(cfg, vmette_run_output_t **out);` | Same blocking contract as Rust `run()` (see top of this doc); on `Ok` writes `*out` — read the exit code via `vmette_run_output_exit_code`. |
| `int32_t vmette_run_output_exit_code(out);` | |
| `void vmette_run_output_free(out);` | |
| `const char *vmette_version(void);` | Static; do not free. |

### `VmetteStatus`

```c
typedef enum VmetteStatus {
    Ok = 0,
    InvalidConfig = 1,
    StartFailed = 2,
    RestoreFailed = 3,
    SaveFailed = 4,
    SnapshotUnsupported = 5,
    Timeout = 6,
    Vsock = 7,
    Io = 8,
    NullArg = 9,
    InvalidUtf8 = 10,
} VmetteStatus;
```
