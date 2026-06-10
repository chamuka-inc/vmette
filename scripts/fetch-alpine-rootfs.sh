#!/usr/bin/env bash
# Pull Alpine's mini rootfs tarball and extract it as a plain rootfs
# directory suitable for vmette's --rootfs (the DirProvider shares it into
# the guest over virtio-fs).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
VER="${ALPINE_VER:-3.20.3}"
SERIES="${VER%.*}"
SERIES="${SERIES:-3.20}"
ARCH="$(vmette_guest_arch)"
DEST="${ROOTFS_DIR:-$HERE/assets/$ARCH/alpine-rootfs}"
URL="https://dl-cdn.alpinelinux.org/alpine/v${SERIES}/releases/${ARCH}/alpine-minirootfs-${VER}-${ARCH}.tar.gz"

# Idempotency guard. /bin/sh in the minirootfs is an *absolute* symlink to
# /bin/busybox, so `-x "$DEST/bin/sh"` always fails on the macOS host (it
# dereferences against the host root, which has no /bin/busybox) — that would
# make this script re-download + re-extract on every run. Test `-L` (symlink
# exists, no deref) and fall back to `-x` for a non-busybox real /bin/sh.
if [[ -L "$DEST/bin/sh" || -x "$DEST/bin/sh" ]]; then
  echo "✓ $DEST already populated"
  exit 0
fi

rm -rf "$DEST"
mkdir -p "$DEST"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "→ downloading $URL"
curl -fsSL --retry 3 -o "$TMP/rootfs.tar.gz" "$URL"

echo "→ extracting to $DEST"
# --no-same-owner so non-root extraction works; sticky/special-file warnings
# from BSD tar are harmless here (vmette only needs the file tree).
tar -xzf "$TMP/rootfs.tar.gz" -C "$DEST" --no-same-owner 2>&1 | \
  grep -vE 'Ignoring|Can.t set|Unknown' || true

echo "✓ rootfs ready: $DEST"
ls "$DEST/bin/sh" >/dev/null && echo "  /bin/sh present"
