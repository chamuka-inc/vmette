# vmette-provider-squashfs

Squashfs block-image rootfs provider for
[vmette](https://github.com/chamuka-inc/vmette). Resolves
`squashfs+file://`, `squashfs+https://`, and `squashfs+http://` specs to a
`.sqfs` image and returns a `BlockImage` artifact — attached read-only as
virtio-blk with a tmpfs overlay in the guest, so the base is immutable and
content-addressable across sessions.

Remote images are cached with a TTL and downloaded under a streaming size cap
(`VMETTE_SQUASHFS_MAX_BYTES`). Usually consumed via
[`vmette-providers`](https://crates.io/crates/vmette-providers) rather than
directly.

> macOS-only (depends on `vmette`). Part of the vmette project. MIT licensed.
