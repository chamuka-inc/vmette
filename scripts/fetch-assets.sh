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

# e2fsprogs (mke2fs) for the `--scratch` ext4 overlay. The netboot busybox
# ships no mke2fs and overlayfs can't use a vfat upper, so we pull mke2fs and
# its libs from the Alpine main repo; build-initramfs.sh injects them into the
# initramfs (ext4.ko already rides in the apk modules tree). Versions resolve
# from the repo's APKINDEX, so a revision bump needs no edit here. libblkid +
# musl are already in the netboot tree, so we don't refetch them.
#
# Arch bound: fetched from $MAIN_BASE, which is parameterized by $ARCH — the
# SAME arch as the guest kernel + modules above. mke2fs runs in the guest, so
# it must match the guest arch; tracking $ARCH keeps it correct for the
# documented arm64 path (an arm64 build pulls the arm64 mke2fs automatically),
# never hardcoding x86_64.
E2FS_EXTRACT="$DEST/e2fsprogs-extract"
if [[ ! -x "$E2FS_EXTRACT/sbin/mke2fs" ]]; then
    echo "→ resolving e2fsprogs package versions from APKINDEX"
    IDX_TMP="$(mktemp -d)"
    curl -fsSL --retry 3 "$MAIN_BASE/APKINDEX.tar.gz" -o "$IDX_TMP/APKINDEX.tar.gz"
    tar -xzf "$IDX_TMP/APKINDEX.tar.gz" -C "$IDX_TMP"
    pkg_ver() {
        awk -v p="$1" 'BEGIN{RS="";FS="\n"} {n="";v="";for(i=1;i<=NF;i++){if($i ~ /^P:/)n=substr($i,3);if($i ~ /^V:/)v=substr($i,3)} if(n==p)print v}' "$IDX_TMP/APKINDEX"
    }
    rm -rf "$E2FS_EXTRACT"; mkdir -p "$E2FS_EXTRACT"
    for pkg in e2fsprogs e2fsprogs-libs libcom_err libuuid; do
        ver="$(pkg_ver "$pkg")"
        [[ -n "$ver" ]] || { echo "✗ could not resolve $pkg version from APKINDEX" >&2; exit 1; }
        echo "→ fetching ${pkg}-${ver}"
        curl -fsSL --retry 3 "$MAIN_BASE/${pkg}-${ver}.apk" -o "$IDX_TMP/${pkg}.apk"
        # apks are gzipped tarballs; ignore the .PKGINFO/.SIGN dotfile warnings.
        tar -xzf "$IDX_TMP/${pkg}.apk" -C "$E2FS_EXTRACT" 2>/dev/null || true
    done
    rm -rf "$IDX_TMP"
    [[ -x "$E2FS_EXTRACT/sbin/mke2fs" ]] || { echo "✗ mke2fs not found after extraction" >&2; exit 1; }
    echo "✓ e2fsprogs extracted to $E2FS_EXTRACT"
fi

KVER="$(ls "$DEST/linux-virt-extract/lib/modules/" 2>/dev/null | head -1)"
echo
echo "Assets ready (kernel $KVER):"
ls -lh "$DEST"
