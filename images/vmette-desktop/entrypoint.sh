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
