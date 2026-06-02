#!/usr/bin/env bash
# Pull Alpine's mini rootfs tarball and extract it as a plain rootfs
# directory suitable for vmette's --rootfs (the DirProvider shares it into
# the guest over virtio-fs).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$HERE/assets/alpine-rootfs"
VER="${ALPINE_VER:-3.20.3}"
SERIES="${ALPINE_VER%.*}"
SERIES="${SERIES:-3.20}"
ARCH="${ARCH:-x86_64}"
URL="https://dl-cdn.alpinelinux.org/alpine/v${SERIES}/releases/${ARCH}/alpine-minirootfs-${VER}-${ARCH}.tar.gz"

if [[ -x "$DEST/bin/sh" ]]; then
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
