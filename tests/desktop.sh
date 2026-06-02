#!/usr/bin/env bash
# End-to-end smoke for the *stateful desktop* subsystem — the part tests/run.sh
# (one-shot CLI) does not touch: vmetted's session registry, the desktop wire
# protocol, and the agent round-trips (screenshot / cursor / clipboard / input).
#
# Usage:  bash tests/desktop.sh
#
# It boots a real ~1-2 GB Xvfb desktop VM, so it is a separate, opt-in harness
# rather than part of the default `cargo test`. It is NOT slow to *write off* —
# the VM boots in a couple of seconds; the only one-time cost is building the
# desktop rootfs image (Docker) if it is not already present.
#
# Everything under test is rebuilt FROM SOURCE every run (vmette + vmetted, then
# re-signed), and vmetted runs on a private socket pointed at the freshly built
# vmette — so a stale installed daemon can never satisfy these gates. The
# desktop rootfs image is treated as a bootstrappable asset (like the kernel):
# built once if missing, reused after.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSETS="$HERE/assets"
KERNEL="$ASSETS/vmlinuz-virt"
INITRAMFS="$ASSETS/initramfs-vmette"
IMAGE_TAR="$ASSETS/vmette-desktop-rootfs.tar"
VMETTE="$HERE/target/release/vmette"
VMETTED="$HERE/target/release/vmetted"
SIZE="1024x768"

# --- bootstrap prereqs ----------------------------------------------------
[[ -s "$KERNEL"    ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$INITRAMFS" ]] || bash "$HERE/scripts/build-initramfs.sh"
if [[ ! -s "$IMAGE_TAR" ]]; then
    echo "→ desktop rootfs image missing; building from source (one-time, Docker)…"
    bash "$HERE/scripts/build-desktop-image.sh" --export "$IMAGE_TAR" || {
        echo "FATAL: could not build the desktop rootfs image (need Docker)." >&2
        exit 1
    }
fi

# --- build + sign the code under test (always, from source) ---------------
echo "→ cargo build --release -q (vmette + vmetted)"
(cd "$HERE" && cargo build --release -q) || { echo "FATAL: build failed" >&2; exit 1; }
# vmetted boots the desktop VM in-process, so IT needs the virtualization
# entitlement (not vmette, which the desktop path never spawns). Sign both.
codesign --sign - --force --entitlements "$HERE/entitlements.plist" \
    --options=runtime "$VMETTE"  >/dev/null
codesign --sign - --force --entitlements "$HERE/entitlements.plist" \
    --options=runtime "$VMETTED" >/dev/null

# --- start a private vmetted (never the user's default socket) -------------
SOCK="$(mktemp -u "${TMPDIR:-/tmp}/vmette-e2e-XXXXXX.sock")"
SESSION=""
cleanup() {
    [[ -n "$SESSION" ]] && "$VMETTE" desktop --socket "$SOCK" stop "$SESSION" >/dev/null 2>&1
    [[ -n "${VMETTED_PID:-}" ]] && kill "$VMETTED_PID" 2>/dev/null
    rm -f "$SOCK"
}
trap cleanup EXIT

"$VMETTED" --socket "$SOCK" --vmette "$VMETTE" >/dev/null 2>&1 &
VMETTED_PID=$!
for _ in $(seq 1 50); do
    [[ -S "$SOCK" ]] && break
    sleep 0.1
done
[[ -S "$SOCK" ]] || { echo "FATAL: vmetted did not bind $SOCK" >&2; exit 1; }

PASS=0; FAIL=0; FAILED=()
check() {  # check NAME  (preceding command's $? is the verdict)
    local rc=$? name="$1"
    printf "  %-46s " "$name"
    if [[ "$rc" == 0 ]]; then echo "PASS"; PASS=$((PASS+1));
    else echo "FAIL (rc=$rc)"; FAIL=$((FAIL+1)); FAILED+=("$name"); fi
}

echo
echo "=== vmette desktop smoke ($(date +%H:%M:%S)) ==="

# 1. Boot a desktop at a NON-default size — exercises the geometry path end to
#    end (request size → StartParams.display_size → Config → cmdline → Xvfb).
SESSION="$("$VMETTE" desktop --socket "$SOCK" start \
    --image "tar+file://$IMAGE_TAR" --size "$SIZE" \
    --kernel "$KERNEL" --initramfs "$INITRAMFS" 2>/dev/null)"
[[ -n "$SESSION" ]]; check "start desktop → session id"

if [[ -z "$SESSION" ]]; then
    echo "  (no session — skipping the rest)"; FAIL=$((FAIL+1))
else
    # 2. Screenshot → PNG written, and its pixel dimensions equal the requested
    #    framebuffer. Validates the action_reply screenshot (base64-PNG) branch
    #    AND that the requested geometry actually took effect in the guest.
    SHOT="$(mktemp "${TMPDIR:-/tmp}/vmette-shot-XXXXXX.png")"
    "$VMETTE" desktop --socket "$SOCK" screenshot "$SESSION" --out "$SHOT" >/dev/null 2>&1
    check "screenshot → PNG file"
    dims="$(sips -g pixelWidth -g pixelHeight "$SHOT" 2>/dev/null \
        | awk '/pixelWidth/{w=$2} /pixelHeight/{h=$2} END{print w"x"h}')"
    [[ "$dims" == "$SIZE" ]]; check "framebuffer is $SIZE (got ${dims:-none})"
    rm -f "$SHOT"

    # 3. Cursor position → two integers.
    "$VMETTE" desktop --socket "$SOCK" cursor "$SESSION" 2>/dev/null \
        | grep -Eq '^-?[0-9]+ -?[0-9]+$'; check "cursor → 'X Y'"

    # 4. Pointer move + left click — input round-trips without error.
    "$VMETTE" desktop --socket "$SOCK" click "$SESSION" 200 200 >/dev/null 2>&1
    check "move + left-click"

    # 5. Clipboard set→get round-trip. THE gate for the want_text→text reply
    #    branch: the bytes return as decoded text, not a base64 PNG.
    CLIP="vmette-e2e-$RANDOM"
    "$VMETTE" desktop --socket "$SOCK" set-clipboard "$SESSION" "$CLIP" >/dev/null 2>&1
    got="$("$VMETTE" desktop --socket "$SOCK" get-clipboard "$SESSION" 2>/dev/null)"
    [[ "$got" == "$CLIP" ]]; check "clipboard round-trip (got '${got}')"

    # 6. Tear the session down cleanly.
    "$VMETTE" desktop --socket "$SOCK" stop "$SESSION" >/dev/null 2>&1
    check "stop session"
    SESSION=""  # stopped; don't double-stop in cleanup
fi

echo
echo "=== summary: $PASS passed, $FAIL failed ==="
if [[ "$FAIL" != 0 ]]; then
    printf '  failed: %s\n' "${FAILED[*]}"
    exit 1
fi
