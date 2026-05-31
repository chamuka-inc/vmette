# vmette-provider-tar

Tarball rootfs provider for
[vmette](https://github.com/chamuka-inc/vmette). Resolves `tar+https://`,
`tar+http://`, and `tar+file://` specs: streams the (optionally gzip/zstd
compressed) tarball, unpacks it into a cached directory rootfs, and hands it to
the boot path. Usually consumed via
[`vmette-providers`](https://crates.io/crates/vmette-providers) rather than
directly.

> macOS-only (depends on `vmette`). Part of the vmette project. MIT licensed.
