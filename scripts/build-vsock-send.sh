#!/usr/bin/env bash
# Cross-compile the guest-side vsock helpers (vsock-send + vsock-runner)
# statically with musl, drop them into the guest rootfs at /usr/local/bin.
# busybox `nc` doesn't speak AF_VSOCK, hence custom binaries.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOTFS="$HERE/assets/alpine-rootfs"
BIN_DIR="$ROOTFS/usr/local/bin"

CC="${CC:-x86_64-linux-musl-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    cat >&2 <<EOF
✗ $CC not found.
  Install with:  brew install FiloSottile/musl-cross/musl-cross
EOF
    exit 1
fi

[[ -d "$ROOTFS" ]] || { echo "✗ $ROOTFS missing — run fetch-alpine-rootfs.sh first" >&2; exit 1; }
mkdir -p "$BIN_DIR"

for name in vsock-send vsock-runner; do
    SRC="$HERE/vmette/${name}.c"
    DEST="$BIN_DIR/${name}"
    echo "→ compiling $SRC → $DEST"
    "$CC" -static -O2 -s -o "$DEST" "$SRC"
    SIZE=$(stat -f%z "$DEST" 2>/dev/null || stat -c%s "$DEST")
    echo "  ✓ $DEST ($SIZE bytes)"
done
