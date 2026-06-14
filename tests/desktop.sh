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
#
# Besides the local-image gates, a final gate exercises the *registry fallback*:
# with no local image discoverable, resolution must fall through to the public
# ghcr image and the daemon must pull + boot it. That gate is skipped (not
# failed) when ghcr is unreachable, so the suite still passes offline.

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

# --- bootstrap prereqs ----------------------------------------------------
[[ -s "$KERNEL"    ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$INITRAMFS" ]] || bash "$HERE/scripts/build-initramfs.sh"
# The desktop rootfs is no longer built by this repo; the gates use a local
# `vmette-desktop-rootfs.tar` if one is present (e.g. a `docker export` of a GUI
# image), otherwise fall through to the published image (DEFAULT_DESKTOP_IMAGE),
# pulled on first use. IMG_ARGS pins the local tar (tier 1) when available.
if [[ -s "$IMAGE_TAR" ]]; then
    IMG_ARGS=(--image "tar+file://$IMAGE_TAR")
    echo "→ using local desktop image: $IMAGE_TAR"
else
    IMG_ARGS=()
    echo "→ no local desktop image; using the published default (pulled on first use)"
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

PASS=0; FAIL=0; SKIP=0; FAILED=()
check() {  # check NAME  (preceding command's $? is the verdict)
    local rc=$? name="$1"
    printf "  %-46s " "$name"
    if [[ "$rc" == 0 ]]; then echo "PASS"; PASS=$((PASS+1));
    else echo "FAIL (rc=$rc)"; FAIL=$((FAIL+1)); FAILED+=("$name"); fi
}
skip() {  # skip NAME WHY  — not a failure; keeps the suite green offline
    printf "  %-46s SKIP (%s)\n" "$1" "$2"; SKIP=$((SKIP+1))
}
ghcr_has_guest_platform() {  # anonymous pull-scope manifest fetch for the public image
    local repo="chamuka-inc/vmette-desktop" tok manifest arch
    arch="$(vmette_guest_arch)"
    tok="$(curl -fsS --max-time 10 \
        "https://ghcr.io/token?scope=repository:${repo}:pull" 2>/dev/null \
        | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
    [[ -n "$tok" ]] || return 1
    manifest="$(mktemp "${TMPDIR:-/tmp}/vmette-ghcr-manifest-XXXXXX.json")"
    curl -fsS --max-time 15 -o "$manifest" \
        -H "Authorization: Bearer $tok" \
        -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json" \
        "https://ghcr.io/v2/${repo}/manifests/latest" || { rm -f "$manifest"; return 1; }
    python3 - "$manifest" "$arch" <<'PY'
import json
import sys

path, arch = sys.argv[1:]
want = {"aarch64": "arm64", "x86_64": "amd64"}.get(arch, arch)
data = json.load(open(path))
manifests = data.get("manifests") or []
if not manifests:
    # Historical GHCR image was a single linux/amd64 manifest. Treat that as
    # usable on Intel only; Apple Silicon needs an explicit arm64 variant.
    sys.exit(0 if want == "amd64" else 1)
for entry in manifests:
    platform = entry.get("platform") or {}
    if platform.get("os") == "linux" and platform.get("architecture") == want:
        sys.exit(0)
sys.exit(1)
PY
    local ok=$?
    rm -f "$manifest"
    return "$ok"
}

echo
echo "=== vmette desktop smoke ($(date +%H:%M:%S)) ==="

# 1. Boot a desktop at a NON-default size — exercises the geometry path end to
#    end (request size → StartParams.display_size → Config → cmdline → Xvfb).
SESSION="$("$VMETTE" desktop --socket "$SOCK" start \
    "${IMG_ARGS[@]}" --size "$SIZE" \
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

# --- registry fallback gate -----------------------------------------------
# The gates above pin an explicit local --image (tier 1). This proves the OTHER
# end of resolution: with NO --image, NO $VMETTE_DESKTOP_IMAGE, and NO
# discoverable local rootfs, the client must fall through to the public ghcr
# image (vmette_assets::DEFAULT_DESKTOP_IMAGE) and the daemon must pull + boot
# it. We run from an isolated empty cwd so `./assets` can't shadow the fallback;
# resolution is client-side, so controlling the CLI's cwd + env fully selects
# the tier. A booted session from this isolated env can ONLY have come from the
# registry — there is no other source — so the session id is the proof.
echo
echo "--- registry fallback (no local image → ghcr) ---"
if ! command -v curl >/dev/null 2>&1; then
    skip "ghcr fallback boots" "curl unavailable"
elif ! ghcr_has_guest_platform; then
    skip "ghcr fallback boots" "ghcr unreachable/offline or no $(vmette_guest_arch) image published"
else
    ISO="$(mktemp -d "${TMPDIR:-/tmp}/vmette-iso-XXXXXX")"
    SESSION="$( (cd "$ISO" && env -u VMETTE_ASSETS_DIR -u VMETTE_DESKTOP_IMAGE \
        "$VMETTE" desktop --socket "$SOCK" start \
        --size "$SIZE" --kernel "$KERNEL" --initramfs "$INITRAMFS") 2>/dev/null)"
    rmdir "$ISO" 2>/dev/null
    [[ -n "$SESSION" ]]; check "ghcr fallback → session id (no local image)"

    if [[ -n "$SESSION" ]]; then
        SHOT2="$(mktemp "${TMPDIR:-/tmp}/vmette-fbshot-XXXXXX.png")"
        "$VMETTE" desktop --socket "$SOCK" screenshot "$SESSION" --out "$SHOT2" >/dev/null 2>&1
        check "fallback desktop screenshot → PNG"
        rm -f "$SHOT2"
        "$VMETTE" desktop --socket "$SOCK" stop "$SESSION" >/dev/null 2>&1
        check "stop fallback session"
        SESSION=""
    else
        echo "  (no fallback session — skipping its sub-gates)"
    fi
fi

echo
echo "=== summary: $PASS passed, $FAIL failed, $SKIP skipped ==="
if [[ "$FAIL" != 0 ]]; then
    printf '  failed: %s\n' "${FAILED[*]}"
    exit 1
fi
