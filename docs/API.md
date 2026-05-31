# vmette library API

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
vmette = "0.1"
```

```rust
use vmette::{Config, RootfsShare, ShareMount, VsockPort};

fn main() -> Result<(), vmette::Error> {
    let mut cfg = Config::new("./vmlinuz", "./initramfs");
    cfg.rootfs_share = Some(RootfsShare {
        path: "./alpine-rootfs".into(),
        read_only: false,
    });
    cfg.shares.push(ShareMount {
        tag: "host".into(),
        path: "/tmp/scratch".into(),
    });
    cfg.exec_cmd = Some("uname -a; exit 0".into());
    cfg.vsock_port = VsockPort::Auto;
    cfg.timeout_seconds = Some(30);

    // run() blocks until guest poweroff, then calls process::exit
    // with the guest's exit code via the VM lifecycle delegate. The
    // Result return is for the synchronous error paths (snapshot
    // unsupported, config invalid, etc).
    let _ = vmette::run(&cfg)?;
    Ok(())
}
```

### Types

See [`crates/vmette/src/lib.rs`](../crates/vmette/src/lib.rs).

- `Config` — fields are `pub`; populate directly after `Config::new`.
  `set_rootfs_artifact(artifact, force_read_only)` applies a resolved
  `RootfsArtifact` to the right field for you.
- `VsockPort` — `Disabled` | `Auto` | `Fixed(u32)`.
- `RootfsShare { path, read_only }` — a host directory shared over
  virtio-fs; held in `Config.rootfs_share`.
- `RootfsBlock { path, fstype }` — a filesystem image attached read-only as
  `/dev/vda` with a tmpfs overlay; held in `Config.rootfs_block`, mutually
  exclusive with `rootfs_share`.
- `RootfsArtifact` — what a provider's `provide()` produces:
  `Directory { path, read_only }` (virtio-fs share) or
  `BlockImage { path, fstype }` (read-only block device + tmpfs overlay).
- `BlockFs` — block-image filesystem tag; currently `Squashfs` only.
- `ShareMount { tag, path }`.
- `Error` (thiserror): `InvalidConfig`, `StartFailed`, `RestoreFailed`,
  `SaveFailed`, `SnapshotUnsupported`, `Timeout`, `Vsock`, `Io`.
- `RunOutput { exit_code: i32 }` — populated only by snapshot paths
  (which return synchronously). For normal `run()`, the process exits
  before this type is observed.

### Rootfs providers

For a fixed directory you can set `Config.rootfs_share` directly. For the
same `--rootfs SPEC` ergonomics the CLI offers — which may resolve to a
directory *or* a block image — the `vmette::provider` module exposes a
trait + registry, and `resolve()` returns a `RootfsArtifact`:

```rust
use vmette::provider::{Context, DirProvider, Registry};
use vmette_provider_oci::OciProvider;
use vmette_provider_squashfs::SquashfsProvider;
use vmette_provider_tar::TarProvider;

let registry = Registry::new()
    .with(DirProvider::new())       // claims path-like specs
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
with `OciProvider::with_auth(resolver)`, or rely on the default resolver
(`VMETTE_OCI_TOKEN` / `VMETTE_OCI_AUTH_<HOST>` → `~/.docker/config.json` →
anonymous).

`RootfsProvider` is the trait third-party code implements to teach
vmette about new rootfs sources (S3 buckets, internal artifactories,
custom build pipelines). See [the tar provider crate]
(../crates/vmette-provider-tar/src/lib.rs) for a ~150-line reference
implementation.

### Snapshot (Apple Silicon only)

```rust
#[cfg(target_arch = "aarch64")]
fn warm() -> Result<(), vmette::Error> {
    let cfg = build_config();
    vmette::run(&Config { build_snapshot: Some("snap.bin".into()), ..cfg })?;
    Ok(())
}
```

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
    VmetteStatus rc = vmette_run(cfg, &out);
    /* vmette_run normally never returns — the VM delegate calls
     * process_exit with the guest's exit code. Only snapshot paths
     * return here. */
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
| `void vmette_config_set_exec(cfg, cmd);` | |
| `void vmette_config_set_net(cfg, bool);` | |
| `void vmette_config_set_switch_root(cfg, bool);` | |
| `void vmette_config_set_vsock_port(cfg, int32_t);` | -1 disable / 0 auto / >0 fixed |
| `void vmette_config_set_guest_vsock_port(cfg, uint32_t);` | snapshot mode only |
| `void vmette_config_set_timeout(cfg, uint32_t);` | 0 = no timeout |
| `void vmette_config_set_vcpus(cfg, uint8_t);` | clamped to ≥1 |
| `void vmette_config_set_mem_mib(cfg, uint64_t);` | clamped to ≥64 |
| `VmetteStatus vmette_run(cfg, vmette_run_output_t **out);` | Normally never returns; see above. |
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
