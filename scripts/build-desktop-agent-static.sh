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

# Pinned sources + their sha256 (verified after download, so a moved or
# corrupted mirror fails loudly instead of silently producing a bad binary).
# Each X tarball is fetched from the first of several independent mirrors that
# serves it with a matching hash. To bump a version: update the version below,
# then `curl -fsSL <url> | sha256sum` for the new checksum.
STB_COMMIT=5736b15f7ea0ffb08dd38af21067c314d6a3aae9
STB_SHA256=cbd5f0ad7a9cf4468affb36354a1d2338034f2c12473cf1a8e32053cb6914a05

echo "→ building static vmette-desktop-agent ($PLATFORM) in a throwaway Alpine container"
docker run --rm --platform "$PLATFORM" \
    -v "$HERE/guest:/guest:ro" -v "$OUT_DIR:/out" \
    alpine:3.20 sh -c "
        set -e
        apk add --no-cache build-base python3 pkgconf util-macros xtrans xorgproto xz \
            linux-headers libxau-dev libxdmcp-dev libxfixes-dev libxtst-dev curl >/dev/null
        cd /tmp
        # fetch FILE WANT_SHA256 URL...: try each mirror until one downloads
        # AND matches the pinned hash, then extract. Fail loudly if none do.
        fetch() {
            f=\"\$1\"; want=\"\$2\"; shift 2
            for u in \"\$@\"; do
                curl -fsSL -o \"\$f\" \"\$u\" 2>/dev/null || continue
                got=\$(sha256sum \"\$f\" | cut -d' ' -f1)
                [ \"\$got\" = \"\$want\" ] && { tar xf \"\$f\"; return 0; }
                echo \"  sha256 mismatch for \$f from \$u (got \$got)\" >&2; rm -f \"\$f\"
            done
            echo \"  FATAL: no mirror served \$f with sha256 \$want\" >&2; exit 1
        }
        L=https://www.x.org/releases/individual/lib
        LA=https://www.x.org/archive/individual/lib
        LF=https://xorg.freedesktop.org/archive/individual/lib
        C=https://www.x.org/releases/individual/xcb
        CA=https://www.x.org/archive/individual/xcb
        CF=https://xcb.freedesktop.org/dist
        fetch xcb-proto-1.15.2.tar.xz 7072beb1f680a2fe3f9e535b797c146d22528990c72f63ddb49d2f350a3653ed \$C/xcb-proto-1.15.2.tar.xz \$CA/xcb-proto-1.15.2.tar.xz \$CF/xcb-proto-1.15.2.tar.xz
        fetch libxcb-1.15.tar.xz cc38744f817cf6814c847e2df37fcb8997357d72fa4bcbc228ae0fe47219a059 \$C/libxcb-1.15.tar.xz \$CA/libxcb-1.15.tar.xz \$CF/libxcb-1.15.tar.xz
        fetch libX11-1.8.7.tar.xz 05f267468e3c851ae2b5c830bcf74251a90f63f04dd7c709ca94dc155b7e99ee \$L/libX11-1.8.7.tar.xz \$LA/libX11-1.8.7.tar.xz \$LF/libX11-1.8.7.tar.xz
        fetch libXext-1.3.5.tar.xz db14c0c895c57ea33a8559de8cb2b93dc76c42ea4a39e294d175938a133d7bca \$L/libXext-1.3.5.tar.xz \$LA/libXext-1.3.5.tar.xz \$LF/libXext-1.3.5.tar.xz
        fetch libXtst-1.2.4.tar.xz 84f5f30b9254b4ffee14b5b0940e2622153b0d3aed8286a3c5b7eeb340ca33c8 \$L/libXtst-1.2.4.tar.xz \$LA/libXtst-1.2.4.tar.xz \$LF/libXtst-1.2.4.tar.xz
        export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
        for d in xcb-proto-1.15.2 libxcb-1.15 libX11-1.8.7 libXext-1.3.5 libXtst-1.2.4; do
            (cd \"\$d\" && ./configure --prefix=/usr/local --enable-static --disable-shared >/dev/null 2>&1 \
                && make -j\$(nproc) >/dev/null 2>&1 && make install >/dev/null 2>&1)
        done
        curl -fsSL \"https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h\" \
            -o /tmp/stb_image_write.h
        echo \"${STB_SHA256}  /tmp/stb_image_write.h\" | sha256sum -c - >/dev/null \
            || { echo '✗ stb_image_write.h sha256 mismatch' >&2; exit 1; }
        cc -static -O2 -s -I/tmp -o /out/vmette-desktop-agent /guest/vmette-desktop-agent.c \
            -lXtst -lXfixes -lX11 -lXext -lxcb -lXau -lXdmcp
        file /out/vmette-desktop-agent | grep -q 'statically linked' || { echo '✗ not static!' >&2; exit 1; }
    "

cp "$HERE/images/vmette-desktop/vmette-desktop-run.sh" "$OUT_DIR/vmette-desktop-run.sh"
chmod +x "$OUT_DIR/vmette-desktop-agent" "$OUT_DIR/vmette-desktop-run.sh"

SIZE=$(stat -f%z "$OUT_DIR/vmette-desktop-agent" 2>/dev/null || stat -c%s "$OUT_DIR/vmette-desktop-agent")
echo "✓ $OUT_DIR/ (agent $SIZE bytes, static + run script)"
echo "  the daemon auto-discovers this and injects it as the 'agent' share for desktop sessions."
