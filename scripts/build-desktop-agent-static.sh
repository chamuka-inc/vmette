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
#
# NO DOCKER: like the vsock guest helpers (build-vsock-send.sh), this
# cross-compiles on the host with a macOS-hosted <arch>-linux-musl toolchain.
# Alpine ships no static archives for the X client libs, so the stack
# (xorgproto/xtrans/xcb-proto + libXau/libXdmcp/libxcb/libX11/libXext/libXfixes/
# libXi/libXtst) is built from source into a throwaway staging prefix with
# --enable-static, then the agent is statically linked against it. aarch64 is a
# plain cross target (native speed — no emulation).
#
# Host tools required: curl, tar (xz), python3, pkg-config, make, and the
# <arch>-linux-musl-gcc cross toolchain (see build-vsock-send.sh for install).
#
# Usage: scripts/build-desktop-agent-static.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ARCH="$(vmette_guest_arch)"
OUT_DIR="$HERE/assets/$ARCH/desktop-agent"

# Cross toolchain — same selection as build-vsock-send.sh (FiloSottile default
# name, messense prebuilt as the alt). The --host triple is the toolchain triple.
case "$ARCH" in
    x86_64)  DEF_CC="x86_64-linux-musl-gcc";  ALT_CC="x86_64-unknown-linux-musl-gcc" ;;
    aarch64) DEF_CC="aarch64-linux-musl-gcc"; ALT_CC="aarch64-unknown-linux-musl-gcc" ;;
    *) echo "✗ unsupported guest arch: $ARCH" >&2; exit 1 ;;
esac
CC="${GUEST_CC:-}"
if [[ -z "$CC" ]]; then
    if command -v "$DEF_CC" >/dev/null 2>&1; then CC="$DEF_CC"
    elif command -v "$ALT_CC" >/dev/null 2>&1; then CC="$ALT_CC"
    else CC="$DEF_CC"; fi
fi
if ! command -v "$CC" >/dev/null 2>&1; then
    cat >&2 <<EOF
✗ $CC not found. Install a macOS-hosted $ARCH-linux-musl cross toolchain:
  brew install FiloSottile/musl-cross/musl-cross                              # $DEF_CC (from source, slow)
  brew install messense/macos-cross-toolchains/$ARCH-unknown-linux-musl       # prebuilt; then GUEST_CC=$ALT_CC
EOF
    exit 1
fi
TARGET="${CC%-gcc}"
for t in curl tar python3 pkg-config make shasum; do
    command -v "$t" >/dev/null 2>&1 || { echo "✗ need '$t' on PATH" >&2; exit 1; }
done

JOBS="$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)"
WORK="$(mktemp -d)"; STAGE="$WORK/stage"; mkdir -p "$STAGE"
trap 'rm -rf "$WORK"' EXIT
export PKG_CONFIG_PATH="$STAGE/lib/pkgconfig:$STAGE/share/pkgconfig"
export PKG_CONFIG_LIBDIR="$PKG_CONFIG_PATH"

# Pinned sources + sha256 (verified after download; cross-checked across two
# mirrors). To bump a version: update it + `curl -fsSL <url> | shasum -a 256`.
STB_COMMIT=5736b15f7ea0ffb08dd38af21067c314d6a3aae9
STB_SHA256=cbd5f0ad7a9cf4468affb36354a1d2338034f2c12473cf1a8e32053cb6914a05

# fetch FILE SUBDIR SHA256: try several independent x.org mirrors until one
# downloads with a matching hash, then extract.
fetch() {
    local f="$1" sub="$2" want="$3" mirrors u got
    case "$sub" in
        proto) mirrors="https://www.x.org/releases/individual/proto https://www.x.org/archive/individual/proto https://xorg.freedesktop.org/archive/individual/proto" ;;
        lib)   mirrors="https://www.x.org/releases/individual/lib https://www.x.org/archive/individual/lib https://xorg.freedesktop.org/archive/individual/lib" ;;
        xcb)   mirrors="https://www.x.org/releases/individual/xcb https://www.x.org/archive/individual/xcb https://xcb.freedesktop.org/dist" ;;
    esac
    for u in $mirrors; do
        curl -fsSL -o "$WORK/$f" "$u/$f" 2>/dev/null || continue
        got="$(shasum -a 256 "$WORK/$f" | cut -d' ' -f1)"
        if [[ "$got" == "$want" ]]; then tar -C "$WORK" -xf "$WORK/$f"; return 0; fi
        echo "  sha256 mismatch for $f from $u" >&2; rm -f "$WORK/$f"
    done
    echo "✗ FATAL: no mirror served $f with sha256 $want" >&2; exit 1
}

echo "→ cross-building static vmette-desktop-agent ($ARCH, $CC) — no Docker"
fetch xorgproto-2023.2.tar.xz proto b61fbc7db82b14ce2dc705ab590efc32b9ad800037113d1973811781d5118c2c
fetch xtrans-1.5.0.tar.xz     lib   1ba4b703696bfddbf40bacf25bce4e3efb2a0088878f017a50e9884b0c8fb1bd
fetch xcb-proto-1.15.2.tar.xz xcb   7072beb1f680a2fe3f9e535b797c146d22528990c72f63ddb49d2f350a3653ed
fetch libXau-1.0.11.tar.xz    lib   f3fa3282f5570c3f6bd620244438dbfbdd580fc80f02f549587a0f8ab329bbeb
fetch libXdmcp-1.1.4.tar.xz   lib   2dce5cc317f8f0b484ec347d87d81d552cdbebb178bd13c5d8193b6b7cd6ad00
fetch libxcb-1.15.tar.xz      xcb   cc38744f817cf6814c847e2df37fcb8997357d72fa4bcbc228ae0fe47219a059
fetch libX11-1.8.7.tar.xz     lib   05f267468e3c851ae2b5c830bcf74251a90f63f04dd7c709ca94dc155b7e99ee
fetch libXext-1.3.5.tar.xz    lib   db14c0c895c57ea33a8559de8cb2b93dc76c42ea4a39e294d175938a133d7bca
fetch libXfixes-6.0.1.tar.xz  lib   b695f93cd2499421ab02d22744458e650ccc88c1d4c8130d60200213abc02d58
fetch libXi-1.8.1.tar.xz      lib   89bfc0e814f288f784202e6e5f9b362b788ccecdeb078670145eacd8749656a7
fetch libXtst-1.2.4.tar.xz    lib   84f5f30b9254b4ffee14b5b0940e2622153b0d3aed8286a3c5b7eeb340ca33c8

# Build-time infra (headers / .pc / the xcb python codegen package) — host
# native, no --host: these install no target code, only data the cross builds
# consume.
for d in xorgproto-2023.2 xtrans-1.5.0 xcb-proto-1.15.2; do
    ( cd "$WORK/$d" && ./configure --prefix="$STAGE" >"$WORK/cfg-$d.log" 2>&1 && make install >>"$WORK/cfg-$d.log" 2>&1 ) \
        || { echo "✗ infra $d failed:" >&2; tail -8 "$WORK/cfg-$d.log" >&2; exit 1; }
done
export PYTHONPATH="$(echo "$STAGE"/lib/python*/site-packages 2>/dev/null | tr ' ' ':')"

# Cross-build the X client stack static (dependency order). `--host` puts
# autotools in cross mode; the two overrides are the standard X cross fixups:
# `xorg_cv_malloc0_returns_null=no` (malloc(0)≠NULL on Linux — a run-test that
# can't execute when cross), and `PKG_CONFIG=pkg-config` (don't look for a
# nonexistent <triple>-pkg-config).
for d in libXau-1.0.11 libXdmcp-1.1.4 libxcb-1.15 libX11-1.8.7 \
         libXext-1.3.5 libXfixes-6.0.1 libXi-1.8.1 libXtst-1.2.4; do
    ( cd "$WORK/$d" \
        && ./configure --host="$TARGET" --prefix="$STAGE" --enable-static --disable-shared \
             CC="$CC" PKG_CONFIG=pkg-config xorg_cv_malloc0_returns_null=no >"$WORK/cfg-$d.log" 2>&1 \
        && make -j"$JOBS" >>"$WORK/cfg-$d.log" 2>&1 \
        && make install >>"$WORK/cfg-$d.log" 2>&1 ) \
        || { echo "✗ cross-build $d failed:" >&2; tail -10 "$WORK/cfg-$d.log" >&2; exit 1; }
done

curl -fsSL "https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h" -o "$WORK/stb_image_write.h"
echo "${STB_SHA256}  $WORK/stb_image_write.h" | shasum -a 256 -c - >/dev/null \
    || { echo "✗ stb_image_write.h sha256 mismatch" >&2; exit 1; }

mkdir -p "$OUT_DIR"
"$CC" -static -O2 -s -I"$WORK" -I"$STAGE/include" -o "$OUT_DIR/vmette-desktop-agent" \
    "$HERE/guest/vmette-desktop-agent.c" -L"$STAGE/lib" \
    -lXtst -lXfixes -lX11 -lXext -lXi -lxcb -lXau -lXdmcp
file "$OUT_DIR/vmette-desktop-agent" | grep -q 'statically linked' \
    || { echo "✗ agent is not statically linked!" >&2; exit 1; }

cp "$HERE/images/vmette-desktop/vmette-desktop-run.sh" "$OUT_DIR/vmette-desktop-run.sh"
chmod +x "$OUT_DIR/vmette-desktop-agent" "$OUT_DIR/vmette-desktop-run.sh"

SIZE=$(stat -f%z "$OUT_DIR/vmette-desktop-agent" 2>/dev/null || stat -c%s "$OUT_DIR/vmette-desktop-agent")
echo "✓ $OUT_DIR/ (agent $SIZE bytes, static + run script)"
echo "  the daemon auto-discovers this and injects it as the 'agent' share for desktop sessions."
