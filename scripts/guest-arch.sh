#!/usr/bin/env bash
# Shared guest architecture mapping for asset scripts.
# ARCH may be set explicitly to x86_64 or aarch64; otherwise derive it from the host.

vmette_guest_arch() {
    local arch="${ARCH:-$(uname -m)}"
    case "$arch" in
        arm64|aarch64) echo "aarch64" ;;
        x86_64|amd64) echo "x86_64" ;;
        *) echo "unsupported guest arch: $arch" >&2; return 1 ;;
    esac
}

vmette_guest_assets_dir() {
    local repo_root="$1"
    local arch
    arch="$(vmette_guest_arch)" || return 1
    echo "$repo_root/assets/$arch"
}
