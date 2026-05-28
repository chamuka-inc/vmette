#!/usr/bin/env bash
# End-to-end smoke runner for vmette. Boots a real microVM for each
# gate; expect ~1 s per case plus a one-time release build. Prints a
# PASS/FAIL summary at the end and exits non-zero on any failure.
#
# Usage:  bash tests/run.sh
#
# Prereqs (auto-bootstrapped if missing):
#   * assets/vmlinuz-virt + assets/initramfs-vmette + assets/alpine-rootfs
#   * /usr/local/bin/vsock-{send,runner} in the rootfs

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HERE/target/release/vmette"
ASSETS="$HERE/assets"
ROOTFS="$ASSETS/alpine-rootfs"

# Bootstrap prereqs
[[ -s "$ASSETS/vmlinuz-virt"             ]] || bash "$HERE/scripts/fetch-assets.sh"
[[ -s "$ASSETS/initramfs-vmette"         ]] || bash "$HERE/scripts/build-initramfs.sh"
[[ -x "$ROOTFS/bin/sh"                   ]] || bash "$HERE/scripts/fetch-alpine-rootfs.sh"
[[ -x "$ROOTFS/usr/local/bin/vsock-send" ]] || bash "$HERE/scripts/build-vsock-send.sh"

# Always rebuild + re-sign — cargo test or any other build can break
# the previous signature, and a missing entitlement looks identical to
# a config-invalid error.
echo "→ cargo build --release -q"
(cd "$HERE" && cargo build --release -q)
codesign --sign - --force --entitlements "$HERE/entitlements.plist" \
    --options=runtime "$BIN" >/dev/null

PASS=0
FAIL=0
FAILED=()

run() {
    # run NAME EXPECTED_EXIT [vmette extra args...] -- COMMAND
    local name="$1" want="$2"; shift 2
    local -a extra=()
    while [[ "$1" != "--" ]]; do extra+=("$1"); shift; done
    shift  # drop --
    local cmd="$*"

    printf "  %-40s " "$name"
    local log; log=$(mktemp)
    local got
    "$BIN" \
        --kernel       "$ASSETS/vmlinuz-virt" \
        --initramfs    "$ASSETS/initramfs-vmette" \
        --rootfs-share "$ROOTFS" \
        "${extra[@]+"${extra[@]}"}" \
        --exec         "$cmd" </dev/null >"$log" 2>&1
    got=$?
    if [[ "$got" == "$want" ]]; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL  (expected exit $want, got $got)"
        FAIL=$((FAIL + 1))
        FAILED+=("$name")
        echo "    --- log tail ---"
        tail -5 "$log" | sed 's/^/    /'
    fi
    rm -f "$log"
}

# Variant that asserts on captured guest output instead of exit code.
# Use this when exit-code propagation is disabled (e.g. --ro-rootfs-share)
# so the test can still detect a regression.
run_output() {
    # run_output NAME EXPECT_REGEX [vmette extra args...] -- COMMAND
    local name="$1" expect="$2"; shift 2
    local -a extra=()
    while [[ "$1" != "--" ]]; do extra+=("$1"); shift; done
    shift
    local cmd="$*"

    printf "  %-40s " "$name"
    local log; log=$(mktemp)
    "$BIN" \
        --kernel       "$ASSETS/vmlinuz-virt" \
        --initramfs    "$ASSETS/initramfs-vmette" \
        --rootfs-share "$ROOTFS" \
        "${extra[@]+"${extra[@]}"}" \
        --exec         "$cmd" </dev/null >"$log" 2>&1
    if grep -qE "$expect" "$log"; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL  (expected output matching /$expect/)"
        FAIL=$((FAIL + 1))
        FAILED+=("$name")
        echo "    --- log tail ---"
        tail -8 "$log" | sed 's/^/    /'
    fi
    rm -f "$log"
}

# Variant that uses --image instead of --rootfs-share.
# Supports optional extra args between image and -- (e.g. --image-offline).
run_image() {
    local name="$1" want="$2" image="$3"; shift 3
    local -a extra=()
    while [[ "$1" != "--" ]]; do extra+=("$1"); shift; done
    shift
    local cmd="$*"

    printf "  %-40s " "$name"
    local log; log=$(mktemp)
    local got
    "$BIN" \
        --kernel    "$ASSETS/vmlinuz-virt" \
        --initramfs "$ASSETS/initramfs-vmette" \
        --image     "$image" \
        "${extra[@]+"${extra[@]}"}" \
        --exec      "$cmd" </dev/null >"$log" 2>&1
    got=$?
    if [[ "$got" == "$want" ]]; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL  (expected exit $want, got $got)"
        FAIL=$((FAIL + 1))
        FAILED+=("$name")
        echo "    --- log tail ---"
        tail -5 "$log" | sed 's/^/    /'
    fi
    rm -f "$log"
}

echo
echo "=== vmette smoke ($(date +%H:%M:%S)) ==="

run "exit code 0"    0 -- 'exit 0'
run "exit code 42"  42 -- 'exit 42'
run "exit code 1 (false)" 1 -- 'false'

run "basic uname"   0 -- 'uname -r > /dev/null && cat /etc/alpine-release > /dev/null'

run "vsock roundtrip" 0 -- 'echo ping | vsock-send $VMETTE_VSOCK_PORT > /tmp/out && grep -q ping /tmp/out'

run "switch-root pid 1" 0 --switch-root -- 'cat /proc/1/comm | grep -q vmette-runner'

run_output "ro-rootfs writes fail" "Read-only" --ro-rootfs-share -- 'touch /foo 2>&1; true'

run "timeout exits 124" 124 --timeout 3 -- 'sleep 30'

run "--vsock-port -1 unsets VMETTE_VSOCK_PORT" 0 --vsock-port -1 -- 'test -z "$VMETTE_VSOCK_PORT"'

run "snapshot --build-snapshot arch guard" 1 --build-snapshot /tmp/foo.snap -- 'true'

# --image: pulls from Docker Hub on first run (~30s), cached after (~3s).
run_image "--image alpine:3.20 (network required)" 0 alpine:3.20 -- 'grep -q "^3.20" /etc/alpine-release'

# --image-offline path: assumes the alpine:3.20 cache is warm from the
# previous gate. To force the offline-fallback branch (rather than the
# in-TTL fast-path which would also serve), wipe the refs/ entry AND
# any stale alpine_3.20__<old-digest> extraction dirs, then run
# offline. scan_offline_fallback picks newest-by-marker-mtime so we
# must ensure exactly one candidate exists.
CACHE="$HOME/Library/Caches/vmette/images"
# Glob avoids hardcoding sanitize_ref's exact output; assert non-empty.
REF_GLOB=("$CACHE"/refs/*alpine*3.20*.digest)
DIR_GLOB=("$CACHE"/*alpine*3.20*__*)
if (( ${#REF_GLOB[@]} == 0 )) || [[ ! -e "${REF_GLOB[0]}" ]]; then
    printf "  %-40s FAIL (no warm cache from prior alpine:3.20 gate)\n" "--image-offline (fallback scan)"
    FAIL=$((FAIL + 1)); FAILED+=("--image-offline missing prereq")
else
    rm -f "${REF_GLOB[@]}"
    # Keep newest alpine_3.20__* dir, prune the rest. With ls -dt we'd
    # sort newest-first; tail -n +2 drops the first (newest).
    # shellcheck disable=SC2012
    ls -dt "${DIR_GLOB[@]}" 2>/dev/null | tail -n +2 | xargs -r rm -rf
    run_image "--image-offline (fallback scan)" 0 alpine:3.20 --image-offline -- 'true'
fi

# Also exercise the offline-cache-miss branch with an unknown image.
run_image "--image-offline (unknown ref → fail)" 1 nosuchimage_vmette_test:v0 --image-offline -- 'true'

# Parse-time rejection of impossible combo (switch-root + ro + exec → guest panic).
# Expected: vmette exits 2 from the arg parser; never reaches the VM.
run "--switch-root+--ro-rootfs-share rejected" 2 --switch-root --ro-rootfs-share -- 'true'

echo
echo "=== summary: $PASS passed, $FAIL failed ==="
if [[ "$FAIL" -gt 0 ]]; then
    echo "failed: ${FAILED[*]}"
    exit 1
fi
exit 0
