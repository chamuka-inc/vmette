#!/usr/bin/env bash
# vmette installer.
#
# Fetches the matching tarball from the latest GitHub release,
# extracts to ~/.local/share/vmette, ad-hoc codesigns the binaries
# with the virtualization entitlement, clears Gatekeeper quarantine,
# and symlinks into ~/.local/bin.
#
# Usage:
#   curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
#   curl -fsSL https://.../install.sh | VERSION=v0.1.0 bash    # pinned version
#
# Env overrides:
#   VERSION      tag to install (default: latest)
#   PREFIX       install root  (default: $HOME/.local/share/vmette)
#   BIN_DIR      symlink dir   (default: $HOME/.local/bin)
#   REPO         github org/repo (default: chamuka-inc/vmette)

set -euo pipefail

REPO="${REPO:-chamuka-inc/vmette}"
PREFIX="${PREFIX:-$HOME/.local/share/vmette}"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-latest}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "✗ vmette runs on macOS only (Apple Virtualization.framework)" >&2
    exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|arm64) ;;
    *) echo "✗ unsupported arch: $ARCH" >&2; exit 1 ;;
esac

resolve_version() {
    if [[ "$VERSION" == "latest" ]]; then
        curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
            | head -n 1
    else
        echo "$VERSION"
    fi
}

VER="$(resolve_version)"
[[ -n "$VER" ]] || { echo "✗ couldn't resolve release version" >&2; exit 1; }

BASE="https://github.com/$REPO/releases/download/$VER"
TARBALL="vmette-$VER-universal-apple-darwin.tar.gz"
URL="$BASE/$TARBALL"

echo "→ vmette $VER (host arch $ARCH)"
echo "→ fetching $URL"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL --retry 3 "$URL" -o "$TMP/$TARBALL"

mkdir -p "$PREFIX" "$BIN_DIR"
echo "→ extracting to $PREFIX"
tar -xzf "$TMP/$TARBALL" -C "$PREFIX" --strip-components=1

# Clear Gatekeeper quarantine flag so the binaries can run.
xattr -dr com.apple.quarantine "$PREFIX" 2>/dev/null || true

# Ad-hoc codesign; the tarball ships unsigned (or sealed-but-untrusted)
# binaries. All three boot VMs and so need the virtualization entitlement:
# vmette + vmetted directly, and vmette-mcp now boots one-shot VMs in-process
# for execute/fetch_url/workspace (it no longer forks the vmette CLI).
ENT="$PREFIX/entitlements.plist"
if [[ -f "$ENT" ]]; then
    for bin in vmette vmetted vmette-mcp; do
        if [[ -x "$PREFIX/bin/$bin" ]]; then
            codesign --sign - --force --entitlements "$ENT" \
                --options=runtime "$PREFIX/bin/$bin" >/dev/null
        fi
    done
fi

ln -sf "$PREFIX/bin/vmette"     "$BIN_DIR/vmette"
ln -sf "$PREFIX/bin/vmetted"    "$BIN_DIR/vmetted"
ln -sf "$PREFIX/bin/vmette-mcp" "$BIN_DIR/vmette-mcp"

echo
echo "✓ installed:"
echo "    $BIN_DIR/vmette     → $PREFIX/bin/vmette"
echo "    $BIN_DIR/vmetted    → $PREFIX/bin/vmetted"
echo "    $BIN_DIR/vmette-mcp → $PREFIX/bin/vmette-mcp"
echo
echo "    libvmette.dylib  → $PREFIX/lib/libvmette.dylib"
echo "    vmette.h         → $PREFIX/include/vmette.h"
echo "    boot assets      → $PREFIX/assets/{x86_64,aarch64}/{vmlinuz-virt,initramfs-vmette}"
echo "    guest helpers    → $PREFIX/share/vmette/guest/{x86_64,aarch64}/{vsock-send,vsock-runner}"
echo
if ! command -v vmette >/dev/null 2>&1; then
    echo "⚠️  $BIN_DIR isn't on your PATH. Add this to your shell init:"
    echo "      export PATH=\"$BIN_DIR:\$PATH\""
fi
echo
echo "first run: vmette --rootfs alpine:3.20 --exec 'uname -a; exit 0'"
echo "           (kernel + initramfs ship in $PREFIX/assets/$([[ $ARCH == arm64 ]] && echo aarch64 || echo x86_64) and are auto-discovered)"
