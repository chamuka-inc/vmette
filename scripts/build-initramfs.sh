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
E2FS_DIR="$HERE/assets/e2fsprogs-extract"
OUT="$HERE/assets/initramfs-vmette"
INIT="$HERE/scripts/custom-init.sh"

[[ -s "$NETBOOT_INITRD" ]] || { echo "✗ $NETBOOT_INITRD missing — run fetch-assets.sh" >&2; exit 1; }
[[ -d "$LIBVIRT_DIR/lib/modules" ]] || { echo "✗ $LIBVIRT_DIR not extracted — run fetch-assets.sh" >&2; exit 1; }
[[ -x "$E2FS_DIR/sbin/mke2fs" ]] || { echo "✗ $E2FS_DIR/sbin/mke2fs missing — run fetch-assets.sh" >&2; exit 1; }
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

echo "→ injecting mke2fs + libs (for the --scratch ext4 overlay)"
# busybox has no mke2fs and overlayfs can't use a vfat upper, so the guest
# needs a real ext4 formatter to back the scratch disk. Inject the binary and
# the libs its DT_NEEDED references (libblkid + musl are already in the tree).
# `cp -a` preserves the SONAME symlinks (libext2fs.so.2 → libext2fs.so.2.x).
cp "$E2FS_DIR/sbin/mke2fs" "$WORK/sbin/mke2fs"
chmod 0755 "$WORK/sbin/mke2fs"
for lib in libcom_err libe2p libext2fs libuuid libss; do
    cp -a "$E2FS_DIR/lib/$lib".so* "$WORK/lib/" 2>/dev/null || true
done
# Verify mke2fs's required libs actually landed — a partial/corrupt e2fsprogs
# extract would otherwise ship an initramfs whose mke2fs fails only at guest
# runtime (cryptic loader error → silent tmpfs fallback). libblkid + musl come
# from the netboot tree; libss is optional (not in mke2fs's DT_NEEDED).
for lib in libcom_err libe2p libext2fs libuuid; do
    ls "$WORK/lib/$lib".so* >/dev/null 2>&1 || {
        echo "✗ initramfs missing $lib — bad e2fsprogs extract; re-run fetch-assets.sh" >&2
        exit 1
    }
done

echo "→ injecting custom /init"
cp "$INIT" "$WORK/init"
chmod 0755 "$WORK/init"

echo "→ repacking → $OUT"
(cd "$WORK" && find . | cpio -o -H newc --quiet) | gzip -9 > "$OUT"

SIZE=$(stat -f%z "$OUT" 2>/dev/null || stat -c%s "$OUT")
echo "✓ $OUT ($SIZE bytes, kernel $KVER)"
