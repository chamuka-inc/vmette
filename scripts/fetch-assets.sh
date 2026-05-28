#!/usr/bin/env bash
# Fetch the assets vmette needs:
#   * Alpine netboot initramfs (busybox + base tree source)
#   * Alpine linux-virt apk     (kernel + complete modules tree, including
#                                vsock + virtiofs which netboot lacks)
#
# Final layout under assets/ :
#   vmlinuz-virt              ← from the apk (matches its modules)
#   initramfs-virt            ← from netboot (busybox source for repack)
#   linux-virt.apk            ← raw apk, kept so we can re-extract on demand
#   linux-virt-extract/       ← extracted apk tree

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$HERE/assets"
SERIES="${ALPINE_SERIES:-3.20}"
ARCH="${ARCH:-x86_64}"
APK_NAME="${APK_NAME:-linux-virt-6.6.141-r0.apk}"

NETBOOT_BASE="https://dl-cdn.alpinelinux.org/alpine/v${SERIES}/releases/${ARCH}/netboot"
MAIN_BASE="https://dl-cdn.alpinelinux.org/alpine/v${SERIES}/main/${ARCH}"

mkdir -p "$DEST"

fetch() {
    local url="$1" out="$2"
    if [[ -s "$out" ]]; then
        echo "✓ $(basename "$out") already present"
        return
    fi
    echo "→ downloading $(basename "$out")"
    curl -fsSL --retry 3 -o "$out" "$url"
}

fetch "$NETBOOT_BASE/initramfs-virt"  "$DEST/initramfs-virt"
fetch "$MAIN_BASE/$APK_NAME"          "$DEST/linux-virt.apk"

if [[ ! -d "$DEST/linux-virt-extract/boot" ]]; then
    echo "→ extracting linux-virt apk"
    rm -rf "$DEST/linux-virt-extract"
    mkdir -p "$DEST/linux-virt-extract"
    tar -xzf "$DEST/linux-virt.apk" -C "$DEST/linux-virt-extract"
fi

cp -f "$DEST/linux-virt-extract/boot/vmlinuz-virt" "$DEST/vmlinuz-virt"

KVER="$(ls "$DEST/linux-virt-extract/lib/modules/" 2>/dev/null | head -1)"
echo
echo "Assets ready (kernel $KVER):"
ls -lh "$DEST"
