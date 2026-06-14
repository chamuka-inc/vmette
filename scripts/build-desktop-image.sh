#!/usr/bin/env bash
# Build the vmette desktop rootfs OCI image (Xvfb + openbox + a browser). This
# is OPTIONAL: `vmette desktop start` defaults to the published image
# `ghcr.io/chamuka-inc/vmette-desktop:latest` (vmette_assets::DEFAULT_DESKTOP_IMAGE),
# pulled on first use, and the computer-use agent is host-injected — so any GUI
# rootfs works without building anything. Use this only to customize the desktop
# rootfs (different WM/browser/fonts) or to republish the default image.
#
# Usage:
#   scripts/build-desktop-image.sh [--tag REF] [--push] [--platform PLAT]
#                                  [--export [PATH]]
#
# Notes:
#   * Build/export default to the host guest architecture (single platform).
#   * --push WITHOUT --platform republishes the full multi-arch manifest
#     (linux/amd64 + linux/arm64) in one shot, so a push can never leave one
#     architecture stale. Multi-platform builds use a `docker-container` buildx
#     builder (auto-created, named `vmette-desktop-build`; the non-native arch
#     builds under qemu emulation, which Docker Desktop bundles).
#   * --push requires you to be logged in to the target registry
#     (`docker login ghcr.io`). Pushing is a deliberate, user-initiated step.
#   * --export writes the built rootfs to a tarball (default:
#     assets/<arch>/vmette-desktop-rootfs.tar) — single-arch only. That tarball
#     is the canonical local source of truth: the CLI and vmette-mcp
#     auto-discover it (as `tar+file://…`) ahead of the registry fallback, so
#     `make desktop-image` is all a dev needs to run computer-use against a
#     locally built rootfs. `make desktop-image` wraps `--export`.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ARCH="$(vmette_guest_arch)"
CTX="$HERE/images/vmette-desktop"

TAG="ghcr.io/chamuka-inc/vmette-desktop:latest"
PUSH=0
PLATFORM=""      # set by --platform; default resolved per-mode below
EXPORT=""        # set to a path by --export; "" means no export
# Filename MUST match vmette_assets::DESKTOP_ROOTFS_ASSET (crates/vmette-assets/src/lib.rs):
# that is how the CLI / vmette-mcp auto-discover this export. Renaming one without
# the other silently breaks discovery (desktop_start falls back to the registry).
DEFAULT_EXPORT="$HERE/assets/$ARCH/vmette-desktop-rootfs.tar"
case "$ARCH" in
    x86_64) HOST_PLATFORM="linux/amd64" ;;
    aarch64) HOST_PLATFORM="linux/arm64" ;;
    *) echo "✗ unsupported guest arch: $ARCH" >&2; exit 1 ;;
esac

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)   TAG="$2"; shift 2 ;;
        --push)  PUSH=1; shift ;;
        --platform) PLATFORM="$2"; shift 2 ;;
        --export)
            # Optional path argument; bare --export uses the canonical asset.
            if [[ $# -ge 2 && "$2" != --* ]]; then EXPORT="$2"; shift 2;
            else EXPORT="$DEFAULT_EXPORT"; shift; fi ;;
        -h|--help)
            sed -n '2,28p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if ! command -v docker >/dev/null 2>&1; then
    echo "✗ docker not found — install Docker Desktop or set up buildx" >&2
    exit 1
fi

# Default platform per mode: a bare --push republishes BOTH arches (the default
# image must stay a consistent manifest); everything else builds the host arch.
if [[ -z "$PLATFORM" ]]; then
    if [[ "$PUSH" == 1 ]]; then PLATFORM="linux/amd64,linux/arm64"; else PLATFORM="$HOST_PLATFORM"; fi
fi
MULTI=0
[[ "$PLATFORM" == *,* ]] && MULTI=1

if [[ -n "$EXPORT" && "$MULTI" == 1 ]]; then
    echo "✗ --export writes one rootfs tarball; it cannot combine with a multi-platform --platform" >&2
    exit 2
fi

# The agent source lives in guest/; the Dockerfile's builder stage expects it in
# the build context. Copy it in for the build, then clean up on exit. (The agent
# is host-injected at runtime, so the baked copy is only a fallback, but the
# reference Dockerfile still compiles it.)
cp "$HERE/guest/vmette-desktop-agent.c" "$CTX/vmette-desktop-agent.c"
trap 'rm -f "$CTX/vmette-desktop-agent.c"' EXIT

# A multi-platform build needs a buildx container builder (the default `docker`
# driver builds one platform at a time). Create one once, reuse it after.
ensure_multi_builder() {
    local name="vmette-desktop-build"
    docker buildx inspect "$name" >/dev/null 2>&1 || \
        docker buildx create --name "$name" --driver docker-container >/dev/null
    echo "$name"
}

echo "→ building $TAG ($PLATFORM)"
if [[ "$MULTI" == 1 ]]; then
    # buildx builds + pushes the whole manifest in one shot. Multi-arch images
    # can't be loaded into the local docker store, so this path is push-only.
    [[ "$PUSH" == 1 ]] || { echo "✗ a multi-platform build must --push (cannot load a manifest list locally)" >&2; exit 2; }
    BUILDER="$(ensure_multi_builder)"
    docker buildx build --builder "$BUILDER" --platform "$PLATFORM" -t "$TAG" --push "$CTX"
    echo "✓ built and pushed $TAG ($PLATFORM)"
    exit 0
fi

if [[ "$PUSH" == 1 ]] && docker buildx version >/dev/null 2>&1; then
    docker buildx build --platform "$PLATFORM" -t "$TAG" --push "$CTX"
    echo "✓ built and pushed $TAG"
    exit 0
fi

docker build --platform "$PLATFORM" -t "$TAG" "$CTX"
echo "✓ built $TAG"

if [[ "$PUSH" == 1 ]]; then
    echo "→ pushing $TAG"
    docker push "$TAG"
    echo "✓ pushed $TAG"
fi

# Export the built rootfs to a tarball the tar+file:// provider can boot. A
# throwaway container's filesystem is exported flat (no image layers), which is
# exactly what the rootfs provider wants.
if [[ -n "$EXPORT" ]]; then
    echo "→ exporting rootfs → $EXPORT"
    mkdir -p "$(dirname "$EXPORT")"
    CID="$(docker create --platform "$PLATFORM" "$TAG")"
    trap 'docker rm -f "$CID" >/dev/null 2>&1; rm -f "$CTX/vmette-desktop-agent.c"' EXIT
    docker export "$CID" > "$EXPORT"
    echo "✓ exported $(du -h "$EXPORT" | cut -f1) → $EXPORT"
    echo "  the CLI / vmette-mcp auto-discover this ahead of the registry fallback."
elif [[ "$PUSH" != 1 ]]; then
    cat <<EOF

Next:
  • Local source of truth:  scripts/build-desktop-image.sh --export   (or: make desktop-image)
  • Test locally:           vmette desktop start --image $TAG   (once daemon is running)
  • Publish (multi-arch):   scripts/build-desktop-image.sh --push
EOF
fi
