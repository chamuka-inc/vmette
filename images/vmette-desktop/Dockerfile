# vmette desktop rootfs — a headless Linux desktop for the Agent workload.
#
# Ships Xvfb + a lightweight WM (openbox) + the in-guest computer-use agent
# (vmette-desktop-agent, XTEST/XGetImage over vsock). vmette boots this as
# the guest rootfs via the OCI provider; the initramfs /init's
# `vmette.desktop=1` branch chroots in and runs the entrypoint, which starts
# Xvfb, the WM, then the agent.
#
# Build with scripts/build-desktop-image.sh (handles the x86_64 platform
# pin — current vmette guest assets are x86_64-only).
#
# Two-stage: a builder compiles the agent against libX11/libXtst dev
# headers; the runtime keeps only the shared libraries, so the shipped image
# stays slim.

# ---- builder: compile vmette-desktop-agent ------------------------------
FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc libc6-dev libx11-dev libxtst-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Vendor stb_image_write.h (single-header PNG encoder), pinned to a commit
# for reproducibility. nothings/stb is public domain / MIT.
ARG STB_COMMIT=5736b15f7ea0ffb08dd38af21067c314d6a3aae9
RUN curl -fsSL \
        "https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h" \
        -o /build/stb_image_write.h --create-dirs

COPY vmette-desktop-agent.c /build/
RUN cc -O2 -s -I/build -o /usr/local/bin/vmette-desktop-agent \
        /build/vmette-desktop-agent.c -lX11 -lXtst

# ---- runtime ------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        xvfb \
        openbox \
        x11-utils \
        x11-xserver-utils \
        xterm \
        libx11-6 \
        libxtst6 \
        fonts-dejavu-core \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Browser for computer-use demos. Comment out to shrink the image; the agent
# works without it (you can `exec` any X app). Note: this pushes the image
# past the tar provider's 512 MiB cap, so use the OCI provider (push to a
# registry) for the browser variant; the slim/no-browser variant fits tar.
RUN apt-get update && apt-get install -y --no-install-recommends \
        chromium \
    && rm -rf /var/lib/apt/lists/* || true

COPY --from=builder /usr/local/bin/vmette-desktop-agent /usr/local/bin/vmette-desktop-agent
COPY entrypoint.sh /usr/local/bin/vmette-desktop-entrypoint.sh
RUN chmod +x /usr/local/bin/vmette-desktop-entrypoint.sh

# The initramfs /init invokes the entrypoint with (HOST_PORT, WxH); this
# CMD documents the shape but is not used on the vmette boot path.
ENTRYPOINT ["/usr/local/bin/vmette-desktop-entrypoint.sh"]
