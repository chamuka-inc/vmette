#!/usr/bin/env bash
# End-to-end smoke for the *live desktop view* (VNC) subsystem — the RFB server
# in vmetted (crates/vmette-daemon/src/{rfb,view}.rs) and the desktop_view wire
# path. It boots a real Xvfb desktop VM via vmetted, opens a live view, and
# drives it with a minimal stdlib RFB client (tests/rfb_probe.py):
#
#   * desktop_view returns a loopback vnc://HOST:PORT (and is idempotent),
#   * the RFB handshake + ServerInit advertise the requested framebuffer size,
#   * a FramebufferUpdate streams the screen as Raw rectangles,
#   * a viewer pointer move round-trips through the view as a guest action
#     (verified independently via `vmette desktop cursor`).
#
# Like tests/desktop.sh this rebuilds + re-signs the code under test FROM SOURCE
# and runs vmetted on a private socket, so a stale installed daemon can never
# satisfy the gates. Usage:  bash tests/view.sh
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ASSETS="$(vmette_guest_assets_dir "$HERE")"
KERNEL="$ASSETS/vmlinuz-virt"
INITRAMFS="$ASSETS/initramfs-vmette"
IMAGE_TAR="$ASSETS/vmette-desktop-rootfs.tar"
VMETTE="$HERE/target/release/vmette"
VMETTED="$HERE/target/release/vmetted"
SIZE="1024x768"
SIZE_W="${SIZE%x*}"
SIZE_H="${SIZE#*x}"
MOVE_X=300
MOVE_Y=250

# --- bootstrap prereqs ----------------------------------------------------
[[ -s "$KERNEL"    ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$INITRAMFS" ]] || bash "$HERE/scripts/build-initramfs.sh"
# The desktop rootfs isn't built by this repo; use a local tar if present, else
# the published default image (pulled on first use).
if [[ -s "$IMAGE_TAR" ]]; then
    IMG_ARGS=(--image "tar+file://$IMAGE_TAR")
else
    IMG_ARGS=()
fi

# --- build + sign the code under test (always, from source) ---------------
echo "→ cargo build --release -q (vmette + vmetted)"
(cd "$HERE" && cargo build --release -q) || { echo "FATAL: build failed" >&2; exit 1; }
codesign --sign - --force --entitlements "$HERE/entitlements.plist" \
    --options=runtime "$VMETTE"  >/dev/null
codesign --sign - --force --entitlements "$HERE/entitlements.plist" \
    --options=runtime "$VMETTED" >/dev/null

# --- start a private vmetted (never the user's default socket) -------------
SOCK="$(mktemp -u "${TMPDIR:-/tmp}/vmette-view-XXXXXX.sock")"
SESSION=""
cleanup() {
    [[ -n "$SESSION" ]] && "$VMETTE" desktop --socket "$SOCK" stop "$SESSION" >/dev/null 2>&1
    [[ -n "${VMETTED_PID:-}" ]] && kill "$VMETTED_PID" 2>/dev/null
    rm -f "$SOCK"
}
trap cleanup EXIT

"$VMETTED" --socket "$SOCK" --vmette "$VMETTE" >/dev/null 2>&1 &
VMETTED_PID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done
[[ -S "$SOCK" ]] || { echo "FATAL: vmetted did not bind $SOCK" >&2; exit 1; }

PASS=0; FAIL=0; FAILED=()
check() {
    local rc=$? name="$1"
    printf "  %-46s " "$name"
    if [[ "$rc" == 0 ]]; then echo "PASS"; PASS=$((PASS+1));
    else echo "FAIL (rc=$rc)"; FAIL=$((FAIL+1)); FAILED+=("$name"); fi
}

echo
echo "=== vmette live view smoke ($(date +%H:%M:%S)) ==="

SESSION="$("$VMETTE" desktop --socket "$SOCK" start \
    ${IMG_ARGS[@]+"${IMG_ARGS[@]}"} --size "$SIZE" \
    --kernel "$KERNEL" --initramfs "$INITRAMFS" 2>/dev/null)"
[[ -n "$SESSION" ]]; check "start desktop → session id"

if [[ -z "$SESSION" ]]; then
    echo "  (no session — skipping the rest)"; exit 1
fi

# 1. desktop_view → vnc://HOST:PORT on loopback.
URL="$("$VMETTE" desktop --socket "$SOCK" view "$SESSION" 2>/dev/null)"
[[ "$URL" =~ ^vnc://127\.0\.0\.1:[0-9]+$ ]]; check "view → loopback vnc:// url (${URL:-none})"
ADDR="${URL#vnc://}"
HOST="${ADDR%:*}"; PORT="${ADDR#*:}"

# 2. Idempotent: a second view returns the SAME address (no new port).
URL2="$("$VMETTE" desktop --socket "$SOCK" view "$SESSION" 2>/dev/null)"
[[ "$URL2" == "$URL" ]]; check "view is idempotent (same addr)"

# 3. Drive the view with a real RFB client: handshake + ServerInit size +
#    a streamed FramebufferUpdate, then inject a pointer move.
python3 "$HERE/tests/rfb_probe.py" "$HOST" "$PORT" "$SIZE_W" "$SIZE_H" "$MOVE_X" "$MOVE_Y"
check "RFB handshake + framebuffer update + input"

# 4. The injected move round-tripped: the guest pointer is where the view put
#    it. THE gate that the client→server input path actually drives the session.
got="$("$VMETTE" desktop --socket "$SOCK" cursor "$SESSION" 2>/dev/null)"
[[ "$got" == "$MOVE_X $MOVE_Y" ]]; check "viewer move drove the cursor (got '${got}')"

# 5. Streaming/interactive regression: with the screen continuously changing, a
#    client that requests-an-update-then-waits in a loop must keep receiving
#    frames. This is the pattern that stalled when the frame writer dropped a
#    request that arrived mid-send (a single-shot probe never triggers it).
"$VMETTE" desktop --socket "$SOCK" exec "$SESSION" \
    "xterm -geometry 60x6+10+10 -e sh -c 'while true; do date +%H:%M:%S.%N; sleep 0.2; done' &" \
    >/dev/null 2>&1
sleep 4  # let the clock start ticking so the screen is actually changing
python3 "$HERE/tests/rfb_probe.py" "$HOST" "$PORT" "$SIZE_W" "$SIZE_H" 1 1 --stream 4
check "sustained streaming under change (no writer stall)"

# 6. Tear the session down — the view server must come down with it (no leaked
#    accept thread / listener); a clean stop is the observable signal.
"$VMETTE" desktop --socket "$SOCK" stop "$SESSION" >/dev/null 2>&1; check "stop session (tears view down)"
SESSION=""

echo
echo "=== summary: $PASS passed, $FAIL failed ==="
if [[ "$FAIL" != 0 ]]; then
    printf '  failed: %s\n' "${FAILED[*]}"
    exit 1
fi
