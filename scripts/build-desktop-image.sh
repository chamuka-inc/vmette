#!/usr/bin/env bash
# Build the vmette desktop rootfs OCI image (Xvfb + openbox + the
# computer-use agent). The image is consumed by vmette's Agent workload via
# the OCI rootfs provider, e.g. `--rootfs ghcr.io/chamuka-inc/vmette-desktop:latest`.
#
# Usage:
#   scripts/build-desktop-image.sh [--tag REF] [--push] [--platform PLAT]
#                                  [--export [PATH]]
#
# Notes:
#   * The default platform follows the vmette guest architecture. Override with
#     --platform to build/push a different variant.
#   * --push requires you to be logged in to the target registry
#     (`docker login ghcr.io`). Pushing is a deliberate, user-initiated step.
#   * --export writes the built rootfs to a tarball (default:
#     assets/<arch>/vmette-desktop-rootfs.tar). That is the canonical local source of
#     truth: the CLI and vmette-mcp auto-discover it (as `tar+file://…`) ahead
#     of the registry fallback, so `make desktop-image` is all a dev needs to
#     run computer-use against the current source. `make desktop-image` wraps
#     `--export`.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$HERE/scripts/guest-arch.sh"
ARCH="$(vmette_guest_arch)"
CTX="$HERE/images/vmette-desktop"

TAG="ghcr.io/chamuka-inc/vmette-desktop:latest"
PUSH=0
EXPORT=""        # set to a path by --export; "" means no export
# Filename MUST match vmette_assets::DESKTOP_ROOTFS_ASSET (crates/vmette-assets/src/lib.rs):
# that is how the CLI / vmette-mcp auto-discover this export. Renaming one without
# the other silently breaks discovery (desktop_start falls back to the registry).
DEFAULT_EXPORT="$HERE/assets/$ARCH/vmette-desktop-rootfs.tar"
case "$ARCH" in
    x86_64) PLATFORM="linux/amd64" ;;
    aarch64) PLATFORM="linux/arm64" ;;
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
            sed -n '2,21p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if ! command -v docker >/dev/null 2>&1; then
    echo "✗ docker not found — install Docker Desktop or set up buildx" >&2
    exit 1
fi

# The agent source lives in guest/; the Dockerfile expects it in the build
# context. Copy it in for the build, then clean up.
cp "$HERE/guest/vmette-desktop-agent.c" "$CTX/vmette-desktop-agent.c"
trap 'rm -f "$CTX/vmette-desktop-agent.c"' EXIT

echo "→ building $TAG ($PLATFORM)"
BUILD_ARGS=(build --platform "$PLATFORM" -t "$TAG" "$CTX")
if [[ "$PUSH" == 1 ]]; then
    # buildx can build + push in one shot for non-native platforms.
    if docker buildx version >/dev/null 2>&1; then
        docker buildx build --platform "$PLATFORM" -t "$TAG" --push "$CTX"
        echo "✓ built and pushed $TAG"
        exit 0
    fi
fi

docker "${BUILD_ARGS[@]}"
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
  • Publish:                scripts/build-desktop-image.sh --tag $TAG --push
EOF
fi
