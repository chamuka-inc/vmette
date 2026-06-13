#!/usr/bin/env bash
# Build the HOST-INJECTED desktop agent: a *fully static* (musl)
# vmette-desktop-agent plus the vmette-desktop-run.sh startup, assembled into
#   assets/<guest-arch>/desktop-agent/
# which the daemon discovers (vmette_assets::resolve_agent_share) and mounts
# into Agent-workload guests as the `agent` virtio-fs share.
#
# Static linking is what makes ONE agent per arch run inside ANY rootfs
# regardless of its libc (glibc or musl) — it carries its own musl libc + X
# client stack and depends on nothing from the rootfs but the X server socket.
# Alpine ships no static archives for the X client libs, so we build
# libxcb/libX11/libXext/libXtst from source with --enable-static (pinned
# versions); libXau/libXdmcp/libXfixes static archives come from the -dev pkgs.
#
# Usage: scripts/build-desktop-agent-static.sh [--platform PLAT]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ARCH="$(vmette_guest_arch)"
OUT_DIR="$HERE/assets/$ARCH/desktop-agent"

case "$ARCH" in
    x86_64) PLATFORM="linux/amd64" ;;
    aarch64) PLATFORM="linux/arm64" ;;
    *) echo "✗ unsupported guest arch: $ARCH" >&2; exit 1 ;;
esac
while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform) PLATFORM="$2"; shift 2 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

command -v docker >/dev/null 2>&1 || { echo "✗ docker not found" >&2; exit 1; }
[[ -f "$HERE/guest/vmette-desktop-agent.c" ]] || { echo "✗ agent source missing" >&2; exit 1; }
[[ -f "$HERE/images/vmette-desktop/vmette-desktop-run.sh" ]] || { echo "✗ run script missing" >&2; exit 1; }

mkdir -p "$OUT_DIR"

# Pinned X source versions (from the mirrors that still serve them).
STB_COMMIT=5736b15f7ea0ffb08dd38af21067c314d6a3aae9

echo "→ building static vmette-desktop-agent ($PLATFORM) in a throwaway Alpine container"
docker run --rm --platform "$PLATFORM" \
    -v "$HERE/guest:/guest:ro" -v "$OUT_DIR:/out" \
    alpine:3.20 sh -c "
        set -e
        apk add --no-cache build-base python3 pkgconf util-macros xtrans xorgproto xz \
            linux-headers libxau-dev libxdmcp-dev libxfixes-dev libxtst-dev curl >/dev/null
        cd /tmp
        fetch() { curl -fsSL -O \"\$1\"; tar xf \"\$(basename \"\$1\")\"; }
        fetch https://xcb.freedesktop.org/dist/xcb-proto-1.15.2.tar.xz
        fetch https://xcb.freedesktop.org/dist/libxcb-1.15.tar.xz
        fetch https://www.x.org/releases/individual/lib/libX11-1.8.7.tar.xz
        fetch https://www.x.org/releases/individual/lib/libXext-1.3.5.tar.xz
        fetch https://www.x.org/releases/individual/lib/libXtst-1.2.4.tar.xz
        export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
        for d in xcb-proto-1.15.2 libxcb-1.15 libX11-1.8.7 libXext-1.3.5 libXtst-1.2.4; do
            (cd \"\$d\" && ./configure --prefix=/usr/local --enable-static --disable-shared >/dev/null 2>&1 \
                && make -j\$(nproc) >/dev/null 2>&1 && make install >/dev/null 2>&1)
        done
        curl -fsSL \"https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h\" \
            -o /tmp/stb_image_write.h
        cc -static -O2 -s -I/tmp -o /out/vmette-desktop-agent /guest/vmette-desktop-agent.c \
            -lXtst -lXfixes -lX11 -lXext -lxcb -lXau -lXdmcp
        file /out/vmette-desktop-agent | grep -q 'statically linked' || { echo '✗ not static!' >&2; exit 1; }
    "

cp "$HERE/images/vmette-desktop/vmette-desktop-run.sh" "$OUT_DIR/vmette-desktop-run.sh"
chmod +x "$OUT_DIR/vmette-desktop-agent" "$OUT_DIR/vmette-desktop-run.sh"

SIZE=$(stat -f%z "$OUT_DIR/vmette-desktop-agent" 2>/dev/null || stat -c%s "$OUT_DIR/vmette-desktop-agent")
echo "✓ $OUT_DIR/ (agent $SIZE bytes, static + run script)"
echo "  the daemon auto-discovers this and injects it as the 'agent' share for desktop sessions."
