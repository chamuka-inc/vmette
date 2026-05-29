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

echo "[desktop] exec agent → host:${HOST_PORT}" >&2
exec vmette-desktop-agent "$HOST_PORT" :99
