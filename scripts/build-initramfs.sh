#!/usr/bin/env bash
# Build the vmette initramfs.
#
# Starts from Alpine's netboot initramfs (for busybox + base tree), swaps
# the modules tree for the apk's (which includes vsock + virtiofs), and
# injects our /init.
#
# The kernel from the apk (assets/vmlinuz-virt after fetch-assets.sh runs)
# matches the modules tree we install, so modprobe at runtime finds them.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NETBOOT_INITRD="$HERE/assets/initramfs-virt"
LIBVIRT_DIR="$HERE/assets/linux-virt-extract"
OUT="$HERE/assets/initramfs-vmette"
INIT="$HERE/scripts/custom-init.sh"

[[ -s "$NETBOOT_INITRD" ]] || { echo "✗ $NETBOOT_INITRD missing — run fetch-assets.sh" >&2; exit 1; }
[[ -d "$LIBVIRT_DIR/lib/modules" ]] || { echo "✗ $LIBVIRT_DIR not extracted — run fetch-assets.sh" >&2; exit 1; }
[[ -s "$INIT" ]] || { echo "✗ $INIT missing" >&2; exit 1; }

KVER="$(ls "$LIBVIRT_DIR/lib/modules/" | head -1)"
[[ -n "$KVER" ]] || { echo "✗ no kernel version dir under $LIBVIRT_DIR/lib/modules/" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "→ extracting alpine netboot initramfs (busybox source)"
(cd "$WORK" && gunzip -c "$NETBOOT_INITRD" | cpio -i -d --quiet)

echo "→ replacing /lib/modules with apk modules tree ($KVER)"
rm -rf "$WORK/lib/modules"
mkdir -p "$WORK/lib/modules"
cp -a "$LIBVIRT_DIR/lib/modules/$KVER" "$WORK/lib/modules/"

echo "→ injecting custom /init"
cp "$INIT" "$WORK/init"
chmod 0755 "$WORK/init"

echo "→ repacking → $OUT"
(cd "$WORK" && find . | cpio -o -H newc --quiet) | gzip -9 > "$OUT"

SIZE=$(stat -f%z "$OUT" 2>/dev/null || stat -c%s "$OUT")
echo "✓ $OUT ($SIZE bytes, kernel $KVER)"
