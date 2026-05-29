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

# run NAME EXPECTED_EXIT [vmette extra args...] -- COMMAND
# Default rootfs is the local alpine dir at $ROOTFS (DirProvider).
run() {
    local name="$1" want="$2"; shift 2
    local -a extra=()
    while [[ "$1" != "--" ]]; do extra+=("$1"); shift; done
    shift  # drop --
    local cmd="$*"

    printf "  %-40s " "$name"
    local log; log=$(mktemp)
    local got
    "$BIN" \
        --kernel    "$ASSETS/vmlinuz-virt" \
        --initramfs "$ASSETS/initramfs-vmette" \
        --rootfs    "$ROOTFS" \
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

# Variant that asserts on captured guest output instead of exit code.
# Use this when exit-code propagation is disabled (e.g. --rootfs-ro)
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
        --kernel    "$ASSETS/vmlinuz-virt" \
        --initramfs "$ASSETS/initramfs-vmette" \
        --rootfs    "$ROOTFS" \
        "${extra[@]+"${extra[@]}"}" \
        --exec      "$cmd" </dev/null >"$log" 2>&1
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

# Variant that uses an arbitrary --rootfs SPEC instead of the local dir.
# Used by the OCI image gates.
run_rootfs() {
    local name="$1" want="$2" spec="$3"; shift 3
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
        --rootfs    "$spec" \
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

run_output "ro-rootfs writes fail" "Read-only" --rootfs-ro -- 'touch /foo 2>&1; true'

run "timeout exits 124" 124 --timeout 3 -- 'sleep 30'

run "--vsock-port -1 unsets VMETTE_VSOCK_PORT" 0 --vsock-port -1 -- 'test -z "$VMETTE_VSOCK_PORT"'

run "snapshot --build-snapshot arch guard" 1 --build-snapshot /tmp/foo.snap -- 'true'

# Parse-time rejection of impossible combo (switch-root + ro + exec → guest panic).
# Expected: vmette exits 2 from the arg parser; never reaches the VM.
run "--switch-root+--rootfs-ro rejected" 2 --switch-root --rootfs-ro -- 'true'

# --rootfs SPEC dispatch: DirProvider claims path-like specs; the bare
# alpine dir is already covered by `run` above, so test the relative
# form here to assert path normalisation works.
run_rootfs "DirProvider on relative path" 0 "./assets/alpine-rootfs" -- 'true'

# Discovery sub-command: should list dir/tar/oci providers.
printf "  %-40s " "providers subcommand lists all three"
PROV=$("$BIN" providers 2>&1)
if echo "$PROV" | grep -q 'dir' && echo "$PROV" | grep -q 'tar' && echo "$PROV" | grep -q 'oci'; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"; FAIL=$((FAIL + 1)); FAILED+=("providers subcommand")
    echo "    --- output ---"; echo "$PROV" | sed 's/^/    /'
fi

# OciProvider end-to-end: pulls from Docker Hub on first run (~30s),
# cached after (~3s). Exercises the catch-all bare-ref path.
run_rootfs "--rootfs alpine:3.20 (network)" 0 alpine:3.20 -- 'grep -q "^3.20" /etc/alpine-release'

# --offline path against the warm cache from the previous gate. Force
# the offline-fallback branch by wiping the refs/ entry and pruning
# stale extracted dirs so scan_offline_fallback has exactly one candidate.
CACHE="$HOME/Library/Caches/vmette/oci"
REF_GLOB=("$CACHE"/refs/*alpine*3.20*.digest)
DIR_GLOB=("$CACHE"/*alpine*3.20*__*)
if (( ${#REF_GLOB[@]} == 0 )) || [[ ! -e "${REF_GLOB[0]}" ]]; then
    printf "  %-40s FAIL (no warm cache from prior alpine:3.20 gate)\n" "--offline alpine:3.20 (fallback scan)"
    FAIL=$((FAIL + 1)); FAILED+=("--offline alpine:3.20 missing prereq")
else
    rm -f "${REF_GLOB[@]}"
    # shellcheck disable=SC2012
    ls -dt "${DIR_GLOB[@]}" 2>/dev/null | tail -n +2 | xargs -r rm -rf
    run_rootfs "--offline alpine:3.20 (fallback scan)" 0 alpine:3.20 --offline -- 'true'
fi

# Offline-cache-miss with an unknown image should fail.
run_rootfs "--offline unknown ref → fail" 1 nosuchimage_vmette_test:v0 --offline -- 'true'

# OCI rootfs + --rootfs-ro: verifies the registry-resolved path is
# still mounted RO. Exit-code is disabled with --rootfs-ro so assert
# on captured guest output (mount table should show 'ro' for /).
printf "  %-40s " "OCI rootfs + --rootfs-ro"
log=$(mktemp)
"$BIN" \
    --kernel    "$ASSETS/vmlinuz-virt" \
    --initramfs "$ASSETS/initramfs-vmette" \
    --rootfs    alpine:3.20 \
    --rootfs-ro \
    --offline \
    --exec      'touch /foo 2>&1; true' </dev/null >"$log" 2>&1
if grep -qE 'Read-only' "$log"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL  (expected Read-only error from touch)"
    FAIL=$((FAIL + 1)); FAILED+=("OCI rootfs + --rootfs-ro")
    echo "    --- log tail ---"
    tail -8 "$log" | sed 's/^/    /'
fi
rm -f "$log"

# --rootfs assets/alpine-rootfs (no leading ./) used to confusingly
# fall through to OciProvider's catch-all. New OciProvider now emits
# a friendly hint suggesting the ./ prefix.
printf "  %-40s " "bare relative path → path-hint error"
log=$(mktemp)
"$BIN" \
    --kernel    "$ASSETS/vmlinuz-virt" \
    --initramfs "$ASSETS/initramfs-vmette" \
    --rootfs    assets/alpine-rootfs \
    --exec      'true' </dev/null >"$log" 2>&1
got=$?
if [[ "$got" == "1" ]] && grep -qE 'looks like a relative path|did you mean|./' "$log"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL  (expected exit 1 with path-hint, got exit $got)"
    FAIL=$((FAIL + 1)); FAILED+=("path-hint error")
    echo "    --- log tail ---"
    tail -5 "$log" | sed 's/^/    /'
fi
rm -f "$log"

# --rootfs missing value (next token is another --flag) should be
# rejected as a usage error, not silently consumed.
printf "  %-40s " "missing --rootfs value rejected"
log=$(mktemp)
"$BIN" --rootfs --kernel "$ASSETS/vmlinuz-virt" --initramfs "$ASSETS/initramfs-vmette" --exec true </dev/null >"$log" 2>&1
got=$?
if [[ "$got" == "2" ]] && grep -qE 'expects a value|needs a value' "$log"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL  (expected exit 2 with helpful message, got exit $got)"
    FAIL=$((FAIL + 1)); FAILED+=("missing --rootfs value")
    echo "    --- log tail ---"
    tail -5 "$log" | sed 's/^/    /'
fi
rm -f "$log"

# Numeric flag with non-numeric value should be a usage error, not a
# silent fallback to the default.
printf "  %-40s " "--vsock-port non-numeric rejected"
log=$(mktemp)
"$BIN" \
    --kernel    "$ASSETS/vmlinuz-virt" \
    --initramfs "$ASSETS/initramfs-vmette" \
    --rootfs    "$ROOTFS" \
    --vsock-port 1234x \
    --exec      'true' </dev/null >"$log" 2>&1
got=$?
if [[ "$got" == "2" ]] && grep -qE 'expects a number' "$log"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL  (expected exit 2 with numeric error, got exit $got)"
    FAIL=$((FAIL + 1)); FAILED+=("--vsock-port non-numeric")
    echo "    --- log tail ---"
    tail -5 "$log" | sed 's/^/    /'
fi
rm -f "$log"

echo
echo "=== summary: $PASS passed, $FAIL failed ==="
if [[ "$FAIL" -gt 0 ]]; then
    echo "failed: ${FAILED[*]}"
    exit 1
fi
exit 0
