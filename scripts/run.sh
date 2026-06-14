#!/usr/bin/env bash
# vmette orchestrator: ensure assets exist, build + sign the host binary
# (via cargo), then boot a microVM that runs whatever command was passed
# on the CLI.
#
# Usage:
#   ./scripts/run.sh                              # default probe command
#   ./scripts/run.sh 'uname -a; cat /etc/os-release'
#   SHARE_DIR=/path/to/dir ./scripts/run.sh 'ls /mnt/host'

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ASSETS="$(vmette_guest_assets_dir "$HERE")"
ROOTFS="$ASSETS/alpine-rootfs"
BIN="$HERE/target/release/vmette"
ENT="$HERE/entitlements.plist"

# Prereqs
[[ -s "$ASSETS/vmlinuz-virt"             ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$ASSETS/initramfs-virt"           ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$ASSETS/initramfs-vmette"         ]] || bash "$HERE/scripts/build-initramfs.sh"
[[ -x "$ROOTFS/bin/sh"                   ]] || bash "$HERE/scripts/fetch-alpine-rootfs.sh"
[[ -x "$ROOTFS/usr/local/bin/vsock-send" ]] || bash "$HERE/scripts/build-vsock-send.sh"

# The staged initramfs embeds a copy of custom-init.sh; rebuild when the source
# is newer so an edit can't silently ship the old /init (the guest's
# BOOT_PROTO_VERSION check is the boot-time backstop).
if [[ "$HERE/scripts/custom-init.sh" -nt "$ASSETS/initramfs-vmette" ]]; then
    echo "→ custom-init.sh newer than initramfs; rebuilding"
    bash "$HERE/scripts/build-initramfs.sh"
fi

echo "→ cargo build --release"
( cd "$HERE" && cargo build --release -p vmette-cli )

echo "→ codesigning"
codesign --sign - --force --entitlements "$ENT" --options=runtime "$BIN" >/dev/null

# Default exec if none given on CLI.
if [[ $# -gt 0 ]]; then
    CMD="$*"
else
    CMD='echo "=== uname ==="; uname -a; echo; echo "=== os ==="; cat /etc/os-release 2>/dev/null || cat /etc/alpine-release; echo; echo "=== id ==="; id; echo; echo "=== mounts ==="; mount; echo; echo "=== /mnt ==="; ls /mnt 2>/dev/null'
fi

# Optional extra share via SHARE_DIR env.
SHARE_ARGS=()
if [[ -n "${SHARE_DIR:-}" ]]; then
    if [[ ! -d "$SHARE_DIR" ]]; then
        echo "✗ SHARE_DIR=$SHARE_DIR is not a directory" >&2
        exit 1
    fi
    SHARE_ARGS=(--share "host=$SHARE_DIR")
fi

exec "$BIN" \
    --kernel    "$ASSETS/vmlinuz-virt" \
    --initramfs "$ASSETS/initramfs-vmette" \
    --rootfs    "$ROOTFS" \
    ${SHARE_ARGS[@]+"${SHARE_ARGS[@]}"} \
    --exec      "$CMD" \
    --vcpus     1 \
    --mem-mib   512
