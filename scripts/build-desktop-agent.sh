#!/usr/bin/env bash
# Compile guest/vmette-desktop-agent.c for the guest (x86_64 Linux) against
# libX11/libXtst, producing assets/vmette-desktop-agent for inspection and
# quick iteration. The agent links those libs dynamically, so it must run
# inside the desktop rootfs (it is NOT a static initramfs helper) — this
# script just proves it compiles and lets you eyeball the binary.
#
# The full image build (scripts/build-desktop-image.sh) compiles the agent
# the same way as a Dockerfile stage; this is the standalone fast path.
#
# Usage: scripts/build-desktop-agent.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$HERE/guest/vmette-desktop-agent.c"
OUT_DIR="$HERE/assets"
PLATFORM="linux/amd64"
STB_COMMIT="5736b15f7ea0ffb08dd38af21067c314d6a3aae9"

[[ -f "$SRC" ]] || { echo "✗ $SRC missing" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "✗ docker not found" >&2; exit 1; }
mkdir -p "$OUT_DIR"

echo "→ compiling vmette-desktop-agent ($PLATFORM) in a throwaway container"
docker run --rm --platform "$PLATFORM" \
    -v "$HERE/guest:/src:ro" \
    -v "$OUT_DIR:/out" \
    debian:bookworm-slim \
    bash -c "
        set -e
        apt-get update >/dev/null
        apt-get install -y --no-install-recommends \
            gcc libc6-dev libx11-dev libxtst-dev ca-certificates curl >/dev/null
        curl -fsSL \
            'https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h' \
            -o /tmp/stb_image_write.h
        cc -O2 -s -I/tmp -o /out/vmette-desktop-agent /src/vmette-desktop-agent.c -lX11 -lXtst
    "

SIZE=$(stat -f%z "$OUT_DIR/vmette-desktop-agent" 2>/dev/null || stat -c%s "$OUT_DIR/vmette-desktop-agent")
echo "✓ $OUT_DIR/vmette-desktop-agent ($SIZE bytes)"
