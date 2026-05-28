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

    printf "  %-30s " "$name"
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

echo
echo "=== vmette smoke ($(date +%H:%M:%S)) ==="

run "exit code 0"    0 -- 'exit 0'
run "exit code 42"  42 -- 'exit 42'
run "exit code 1 (false)" 1 -- 'false'

run "basic uname"   0 -- 'uname -r > /dev/null && cat /etc/alpine-release > /dev/null'

run "vsock roundtrip" 0 -- 'echo ping | vsock-send $VMETTE_VSOCK_PORT > /tmp/out && grep -q ping /tmp/out'

run "switch-root pid 1" 0 --switch-root -- 'cat /proc/1/comm | grep -q vmette-runner'

run "ro-rootfs writes fail" 0 --ro-rootfs-share -- 'touch /foo 2>&1 | grep -q "Read-only"'

run "timeout exits 124" 124 --timeout 3 -- 'sleep 30'

run "--vsock-port -1 unsets VMETTE_VSOCK_PORT" 0 --vsock-port -1 -- 'test -z "$VMETTE_VSOCK_PORT"'

run "snapshot --build-snapshot arch guard" 1 --build-snapshot /tmp/foo.snap -- 'true'

echo
echo "=== summary: $PASS passed, $FAIL failed ==="
if [[ "$FAIL" -gt 0 ]]; then
    echo "failed: ${FAILED[*]}"
    exit 1
fi
exit 0
