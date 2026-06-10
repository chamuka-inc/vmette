# vmette desktop rootfs — a headless Linux desktop for the Agent workload.
#
# Ships Xvfb + a lightweight WM (openbox) + the in-guest computer-use agent
# (vmette-desktop-agent, XTEST/XGetImage over vsock). vmette boots this as
# the guest rootfs via the OCI provider; the initramfs /init's
# `vmette.desktop=1` branch chroots in and runs the entrypoint, which starts
# Xvfb, the WM, then the agent.
#
# Build with scripts/build-desktop-image.sh (selects linux/arm64 on Apple
# Silicon and linux/amd64 on Intel; --platform can override it).
#
# Two-stage: a builder compiles the agent against libX11/libXtst dev
# headers; the runtime keeps only the shared libraries, so the shipped image
# stays slim.

# ---- builder: compile vmette-desktop-agent ------------------------------
FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc libc6-dev libx11-dev libxtst-dev libxfixes-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Vendor stb_image_write.h (single-header PNG encoder), pinned to a commit
# for reproducibility. nothings/stb is public domain / MIT.
ARG STB_COMMIT=5736b15f7ea0ffb08dd38af21067c314d6a3aae9
RUN curl -fsSL \
        "https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h" \
        -o /build/stb_image_write.h --create-dirs

COPY vmette-desktop-agent.c /build/
RUN cc -O2 -s -I/build -o /usr/local/bin/vmette-desktop-agent \
        /build/vmette-desktop-agent.c -lX11 -lXtst -lXfixes

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
        libxfixes3 \
        fonts-dejavu-core \
        ca-certificates \
        openssl \
        curl \
        locales \
    && rm -rf /var/lib/apt/lists/*

# Generate a UTF-8 locale. The C locale drops typed non-ASCII (é, €, …) in
# xterm and other apps; en_US.UTF-8 lets them accept the full keymap. The locale
# is *exported* at runtime by entrypoint.sh (the single source of truth) — a
# Dockerfile `ENV LANG=…` would not reach a vmette boot, which chroots into this
# rootfs from the initramfs and never applies Docker's image config.
RUN sed -i 's/# en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen \
    && locale-gen

# Browser for computer-use demos. Comment out to shrink the image; the agent
# works without it (you can `exec` any X app). The image is ~913 MB
# uncompressed, but the tar provider's 512 MiB cap applies to the *gzipped*
# rootfs export, which lands around ~320 MiB — so the browser variant still
# fits the tar provider; the OCI provider (registry push) remains an option.
RUN apt-get update && apt-get install -y --no-install-recommends \
        chromium \
    && rm -rf /var/lib/apt/lists/* || true

# Make plain `chromium <url>` render inside this guest. Debian's chromium is a
# wrapper that sources /etc/chromium.d/* and appends $CHROMIUM_FLAGS, so the
# software-GL / root-sandbox incantation lives here, with the app — not in any
# caller. Without these the window maps but stays black: we are root with user
# namespaces off (--no-sandbox) and there is no GPU (software GL). `--test-type`
# suppresses the "unsupported command-line flag: --no-sandbox" infobar that
# otherwise sits across the top of every frame the agent sees.
RUN mkdir -p /etc/chromium.d \
    && printf '%s\n' \
        'export CHROMIUM_FLAGS="$CHROMIUM_FLAGS --no-sandbox --test-type --disable-dev-shm-usage --disable-gpu --disable-gpu-compositing --use-gl=swiftshader --no-first-run --disable-features=Translate --disable-session-crashed-bubble --hide-crash-restore-bubble"' \
        > /etc/chromium.d/00-vmette-swiftshader

COPY --from=builder /usr/local/bin/vmette-desktop-agent /usr/local/bin/vmette-desktop-agent
COPY entrypoint.sh /usr/local/bin/vmette-desktop-entrypoint.sh
RUN chmod +x /usr/local/bin/vmette-desktop-entrypoint.sh

# Browser launcher used by the agent's `navigate` action (deterministic,
# shell-free URL open). Browser-agnostic so a custom image can swap browsers
# without touching the agent.
COPY vmette-open /usr/local/bin/vmette-open
RUN chmod +x /usr/local/bin/vmette-open

# The initramfs /init invokes the entrypoint with (HOST_PORT, WxH); this
# CMD documents the shape but is not used on the vmette boot path.
ENTRYPOINT ["/usr/local/bin/vmette-desktop-entrypoint.sh"]
