# vmette-assets

Shared boot-asset discovery for the
[vmette](https://github.com/chamuka-inc/vmette) binaries. Locates the guest
kernel (`vmlinuz-virt`) and initramfs (`initramfs-vmette`) across the install
layouts — a release tarball's `$PREFIX/assets`, a repo checkout's `./assets`, or
a `$VMETTE_ASSETS_DIR` override — so the CLI, daemon, and MCP server all resolve
them identically.

Part of the vmette project. MIT licensed.
