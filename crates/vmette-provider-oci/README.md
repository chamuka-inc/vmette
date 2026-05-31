# vmette-provider-oci

OCI/Docker image rootfs provider for
[vmette](https://github.com/chamuka-inc/vmette). The catch-all for bare image
references (`alpine:3.20`, `ghcr.io/...`) and explicit `oci://` specs: pulls the
image, unpacks its layers into a cached directory rootfs, and hands it to the
boot path.

Private registries are supported through a per-registry `AuthResolver` (env vars
or `~/.docker/config.json`). Usually consumed via
[`vmette-providers`](https://crates.io/crates/vmette-providers) rather than
directly.

> macOS-only (depends on `vmette`). Part of the vmette project. MIT licensed.
