#!/usr/bin/env bash
# Build the vmette desktop rootfs OCI image (Xvfb + openbox + the
# computer-use agent). The image is consumed by vmette's Agent workload via
# the OCI rootfs provider, e.g. `--rootfs ghcr.io/chamuka-inc/vmette-desktop:latest`.
#
# Usage:
#   scripts/build-desktop-image.sh [--tag REF] [--push] [--with-chromium|--no-chromium]
#
# Notes:
#   * vmette's guest assets are x86_64-only, so we pin --platform linux/amd64.
#     On Apple Silicon this needs Docker/qemu emulation (buildx).
#   * --push requires you to be logged in to the target registry
#     (`docker login ghcr.io`). Pushing is a deliberate, user-initiated step.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CTX="$HERE/images/vmette-desktop"

TAG="ghcr.io/chamuka-inc/vmette-desktop:latest"
PUSH=0
PLATFORM="linux/amd64"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)   TAG="$2"; shift 2 ;;
        --push)  PUSH=1; shift ;;
        --platform) PLATFORM="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,15p' "$0"; exit 0 ;;
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
else
    cat <<EOF

Next:
  • Test locally:  vmette desktop start --image $TAG   (once daemon is running)
  • Publish:       scripts/build-desktop-image.sh --tag $TAG --push
EOF
fi
