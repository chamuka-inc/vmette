#!/bin/sh
# vmette desktop entrypoint. Started by the initramfs /init's
# `vmette.desktop=1` branch (chroot/switch_root into this rootfs).
#
#   vmette-desktop-entrypoint.sh HOST_PORT [WIDTHxHEIGHT]
#
# Brings up Xvfb on :99, a lightweight WM, then execs the computer-use agent
# which connects out to the host on HOST_PORT and serves the framed
# screenshot/input protocol. We exec the agent as the final step so it is
# the process the guest's PID-1 init waits on; when it exits the boot path
# proceeds to power off.

set -u

HOST_PORT="${1:-}"
SIZE="${2:-1280x800}"

if [ -z "$HOST_PORT" ]; then
    echo "[desktop] FATAL: no HOST_PORT argument" >&2
    exit 2
fi

export DISPLAY=:99
export HOME="${HOME:-/root}"

# Use the UTF-8 locale generated in the image. vmette boots this rootfs via its
# own init (chroot from the initramfs), so the Dockerfile's `ENV LANG=…` — which
# only applies when Docker runs the image — never reaches us. Exporting it here
# is what actually puts the agent (and the xterm/chromium it launches, which
# inherit this environment) into UTF-8 mode; without it they run in the C locale
# and silently drop typed non-ASCII (é, €, …).
export LANG="${LANG:-en_US.UTF-8}"
export LC_ALL="${LC_ALL:-en_US.UTF-8}"

install_ca_certs() {
    [ -d /mnt/certs ] || return 0

    mkdir -p /usr/local/share/ca-certificates/vmette \
        /etc/chromium/policies/managed \
        /etc/opt/chrome/policies/managed

    json=/etc/chromium/policies/managed/vmette-certs.json
    tmp="${json}.tmp"
    split_dir=$(mktemp -d)
    count=0

    # `openssl x509 -in FILE` only reads the FIRST certificate in FILE, so a
    # combined bundle — or a full chain shipped as one .crt/.pem — would import
    # just its leading cert. Split every input file into one-cert PEMs first,
    # then import each individually.
    src_idx=0
    for src in /mnt/certs/*.crt /mnt/certs/*.pem; do
        [ -f "$src" ] || continue
        src_idx=$((src_idx + 1))
        awk -v dir="$split_dir" -v base="$src_idx" '
            /-----BEGIN CERTIFICATE-----/ { n++; out = sprintf("%s/%04d-%04d.pem", dir, base, n) }
            out { print > out }
            /-----END CERTIFICATE-----/ { if (out) close(out); out = "" }
        ' "$src"
    done

    printf '{\n  "CAPlatformIntegrationEnabled": true,\n  "CACertificates": [\n' >"$tmp"
    for cert in "$split_dir"/*.pem; do
        [ -f "$cert" ] || continue
        out="/usr/local/share/ca-certificates/vmette/vmette-$((count + 1)).crt"
        if ! openssl x509 -in "$cert" -out "$out" 2>/var/log/vmette-ca.err; then
            echo "[desktop] warning: skipping unreadable CA certificate" >&2
            continue
        fi
        der=$(openssl x509 -in "$out" -outform DER | base64 -w0)
        [ "$count" -gt 0 ] && printf ',\n' >>"$tmp"
        printf '    "%s"' "$der" >>"$tmp"
        count=$((count + 1))
    done
    printf '\n  ]\n}\n' >>"$tmp"

    rm -rf "$split_dir"

    if [ "$count" -gt 0 ]; then
        mv "$tmp" "$json"
        cp "$json" /etc/opt/chrome/policies/managed/vmette-certs.json
        update-ca-certificates >/var/log/vmette-ca.log 2>&1 || true
        echo "[desktop] installed $count CA certificate(s) for system trust and Chromium" >&2
    else
        rm -f "$tmp"
        echo "[desktop] warning: /mnt/certs contained no readable .crt/.pem CA certificates" >&2
    fi
}

install_ca_certs

# The initramfs carries /dev in as devtmpfs but does not mount devpts; without
# it terminal emulators (xterm, the agent's `exec` targets) fail to allocate a
# pty ("get_pty: not enough ptys"). Mount it here as part of desktop bring-up.
if ! mountpoint -q /dev/pts 2>/dev/null; then
    mkdir -p /dev/pts
    mount -t devpts devpts /dev/pts 2>/dev/null || true
fi

echo "[desktop] starting Xvfb on :99 (${SIZE}x24)" >&2
Xvfb :99 -screen 0 "${SIZE}x24" -nolisten tcp >/var/log/Xvfb.log 2>&1 &

# Wait for the X server to accept connections (xdpyinfo from x11-utils).
i=0
while [ "$i" -lt 100 ]; do
    if xdpyinfo -display :99 >/dev/null 2>&1; then
        break
    fi
    i=$((i + 1))
    sleep 0.1
done
if ! xdpyinfo -display :99 >/dev/null 2>&1; then
    echo "[desktop] FATAL: Xvfb did not come up" >&2
    cat /var/log/Xvfb.log >&2 2>/dev/null
    exit 1
fi

echo "[desktop] starting openbox" >&2
openbox >/var/log/openbox.log 2>&1 &

# Paint the root window a neutral colour so an idle desktop is a neutral slate
# rather than pure black — which an agent's first screenshot (taken right after
# desktop_start returns) would read as a broken/blank capture. openbox clears
# the root to black when it initialises, so a colour set BEFORE it is clobbered;
# xsetroot must run AFTER openbox is up to stick. Do it in the background (so the
# agent still execs promptly) once openbox has registered as the EWMH window
# manager. The daemon's desktop_start settle barrier blocks until this paint
# lands, so the first screenshot is never the pre-paint black framebuffer.
(
    n=0
    while [ "$n" -lt 30 ]; do
        if xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1; then
            break
        fi
        n=$((n + 1))
        sleep 0.1
    done
    xsetroot -solid '#2e3440' 2>/dev/null || true
) &

echo "[desktop] exec agent → host:${HOST_PORT}" >&2
exec vmette-desktop-agent "$HOST_PORT" :99
