#!/bin/sh
# vmette desktop startup — the HOST-INJECTED counterpart to the in-image
# entrypoint. Shipped *with vmette* (a per-arch static `vmette-desktop-agent`
# beside this script) and mounted into the guest as the read-only `agent`
# virtio-fs share; the initramfs /init's desktop branch execs it when that
# share is present (see scripts/custom-init.sh). This is what decouples the
# desktop workload from a vmette-specific rootfs image: the rootfs only has to
# provide an X server (Xvfb) + a window manager — any GUI image works — and
# vmette supplies the agent.
#
#   vmette-desktop-run.sh HOST_PORT [WIDTHxHEIGHT]
#
# Brings up Xvfb on :99 + a WM (auto-detected from the rootfs), then execs the
# bundled static agent, which connects out to the host on HOST_PORT and serves
# the framed screenshot/input protocol. Exec'ing the agent last makes it the
# process the guest's PID-1 waits on; its exit ends the boot.

set -u

HOST_PORT="${1:-}"
SIZE="${2:-1280x800}"
if [ -z "$HOST_PORT" ]; then
    echo "[desktop] FATAL: no HOST_PORT argument" >&2
    exit 2
fi

# The agent ships beside this script in the injected `agent` share.
SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
AGENT="$SELF_DIR/vmette-desktop-agent"
if [ ! -x "$AGENT" ]; then
    echo "[desktop] FATAL: bundled agent $AGENT missing/not executable" >&2
    exit 2
fi

export DISPLAY=:99
export HOME="${HOME:-/root}"
# UTF-8 so the agent (and apps it launches) accept the full keymap; the rootfs
# may not have generated en_US.UTF-8, so fall back to C.UTF-8 which musl/glibc
# both provide.
export LANG="${LANG:-C.UTF-8}"
export LC_ALL="${LC_ALL:-C.UTF-8}"

# Best-effort: terminal emulators need devpts to allocate a pty; the initramfs
# brings /dev in as devtmpfs but does not mount devpts.
if ! mountpoint -q /dev/pts 2>/dev/null; then
    mkdir -p /dev/pts 2>/dev/null
    mount -t devpts devpts /dev/pts 2>/dev/null || true
fi

# Best-effort: install host CA certs into Chromium's managed policy if both a
# certs share and chromium are present (system trust is already installed by the
# init). No-op on a rootfs without certs or without chromium.
if [ -d /mnt/certs ] && command -v openssl >/dev/null 2>&1; then
    mkdir -p /etc/chromium/policies/managed /etc/opt/chrome/policies/managed 2>/dev/null
    json=/etc/chromium/policies/managed/vmette-certs.json
    split_dir=$(mktemp -d 2>/dev/null) || split_dir=/tmp/vmette-certs
    mkdir -p "$split_dir"
    si=0
    for src in /mnt/certs/*.crt /mnt/certs/*.pem; do
        [ -f "$src" ] || continue
        si=$((si + 1))
        awk -v dir="$split_dir" -v base="$si" '
            /-----BEGIN CERTIFICATE-----/ { n++; out = sprintf("%s/%04d-%04d.pem", dir, base, n) }
            out { print > out }
            /-----END CERTIFICATE-----/ { if (out) close(out); out = "" }
        ' "$src" 2>/dev/null
    done
    count=0
    printf '{\n  "CAPlatformIntegrationEnabled": true,\n  "CACertificates": [\n' >"$json.tmp" 2>/dev/null
    for cert in "$split_dir"/*.pem; do
        [ -f "$cert" ] || continue
        der=$(openssl x509 -in "$cert" -outform DER 2>/dev/null | base64 -w0 2>/dev/null)
        [ -z "$der" ] && continue
        [ "$count" -gt 0 ] && printf ',\n' >>"$json.tmp"
        printf '    "%s"' "$der" >>"$json.tmp"
        count=$((count + 1))
    done
    printf '\n  ]\n}\n' >>"$json.tmp"
    if [ "$count" -gt 0 ]; then
        mv "$json.tmp" "$json" 2>/dev/null
        cp "$json" /etc/opt/chrome/policies/managed/vmette-certs.json 2>/dev/null
        echo "[desktop] installed $count CA cert(s) into Chromium policy" >&2
    else
        rm -f "$json.tmp" 2>/dev/null
    fi
    rm -rf "$split_dir" 2>/dev/null
fi

echo "[desktop] starting Xvfb on :99 (${SIZE}x24)" >&2
mkdir -p /tmp/.X11-unix 2>/dev/null
Xvfb :99 -screen 0 "${SIZE}x24" -nolisten tcp >/tmp/Xvfb.log 2>&1 &

# Wait for the X server's socket (no dependency on x11-utils' xdpyinfo, which a
# stock rootfs may lack).
i=0
while [ "$i" -lt 100 ]; do
    [ -S /tmp/.X11-unix/X99 ] && break
    i=$((i + 1))
    sleep 0.1
done
if [ ! -S /tmp/.X11-unix/X99 ]; then
    echo "[desktop] FATAL: Xvfb did not come up" >&2
    cat /tmp/Xvfb.log >&2 2>/dev/null
    exit 1
fi

# Start a window manager — whatever the rootfs ships. Without one, override-
# redirect/maximize behavior is broken (e.g. browsers open tiny windows).
for wm in openbox fluxbox icewm matchbox-window-manager twm; do
    if command -v "$wm" >/dev/null 2>&1; then
        echo "[desktop] starting WM: $wm" >&2
        "$wm" >/tmp/wm.log 2>&1 &
        break
    fi
done

# Paint the root a neutral slate once the WM is up (an idle pure-black root reads
# as a broken first screenshot). Best-effort; needs xsetroot. Background so the
# agent still execs promptly.
if command -v xsetroot >/dev/null 2>&1; then
    (
        n=0
        while [ "$n" -lt 30 ]; do
            command -v xprop >/dev/null 2>&1 && xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1 && break
            n=$((n + 1)); sleep 0.1
        done
        xsetroot -solid '#2e3440' 2>/dev/null || true
    ) &
fi

echo "[desktop] exec bundled agent → host:${HOST_PORT}" >&2
exec "$AGENT" "$HOST_PORT" :99
