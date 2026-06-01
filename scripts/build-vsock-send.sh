#!/usr/bin/env bash
# Cross-compile the guest-side vsock helpers (vsock-send + vsock-runner)
# statically with musl, drop them into the guest rootfs at /usr/local/bin.
# busybox `nc` doesn't speak AF_VSOCK, hence custom binaries.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOTFS="$HERE/assets/alpine-rootfs"
BIN_DIR="$ROOTFS/usr/local/bin"

# The guest helpers cross-compile to x86_64-linux-musl. GUEST_CC names that
# cross toolchain — kept distinct from the host $CC so it never leaks into a
# host/cargo build (which must keep using clang for the Apple targets).
GUEST_CC="${GUEST_CC:-x86_64-linux-musl-gcc}"
if ! command -v "$GUEST_CC" >/dev/null 2>&1; then
    cat >&2 <<EOF
✗ $GUEST_CC not found. Install a macOS-hosted x86_64-linux-musl cross toolchain:
  brew install FiloSottile/musl-cross/musl-cross                          # x86_64-linux-musl-gcc (builds from source, slow)
  brew install messense/macos-cross-toolchains/x86_64-unknown-linux-musl  # prebuilt; then GUEST_CC=x86_64-unknown-linux-musl-gcc
EOF
    exit 1
fi

[[ -d "$ROOTFS" ]] || { echo "✗ $ROOTFS missing — run fetch-alpine-rootfs.sh first" >&2; exit 1; }
mkdir -p "$BIN_DIR"

for name in vsock-send vsock-runner; do
    SRC="$HERE/guest/${name}.c"
    DEST="$BIN_DIR/${name}"
    echo "→ compiling $SRC → $DEST"
    "$GUEST_CC" -static -O2 -s -o "$DEST" "$SRC"
    SIZE=$(stat -f%z "$DEST" 2>/dev/null || stat -c%s "$DEST")
    echo "  ✓ $DEST ($SIZE bytes)"
done
