#!/bin/sh
# vmette custom /init (PID 1 inside the alpine initramfs).
#
# Runs as PID 1. Two things make this script unusual:
#
#   1. Alpine's initramfs ships /bin/busybox and a tree of hardlinks
#      (/bin/mkdir → busybox, etc.). bsdcpio on macOS does not preserve those
#      hardlinks across an extract+repack, so the first thing we do is
#      symlink every applet we need back into /bin and /sbin.
#
#   2. /proc isn't mounted yet, so we can't `cat /proc/cmdline` to learn
#      anything. We mount the bare minimum first, then parse the cmdline
#      using only shell builtins (no awk, no grep) so we're not dependent
#      on any applet beyond what the bootstrap creates.

# ---- step 0: bootstrap busybox applet symlinks --------------------------

BB=/bin/busybox
if [ ! -x "$BB" ]; then
    echo "[init] FATAL: /bin/busybox missing" >&2
    while :; do "$BB" sleep 60 2>/dev/null || break; done
    exit 1
fi

$BB mkdir -p /bin /sbin /proc /sys /dev /tmp /newroot

for a in awk base64 basename cat chmod chroot cp dmesg echo find grep head \
         ifconfig ip ln ls mkdir mknod modprobe mount mv poweroff printf rm \
         route sed sleep switch_root sync tr udhcpc umount; do
    [ -e "/bin/$a" ]  || $BB ln -sf busybox "/bin/$a"  2>/dev/null
    [ -e "/sbin/$a" ] || $BB ln -sf busybox "/sbin/$a" 2>/dev/null
done

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

log() { echo "[init] $*" >&2; }

# ---- step 1: mount baseline pseudo-filesystems --------------------------

mount -t proc     proc     /proc     2>/dev/null
mount -t sysfs    sysfs    /sys      2>/dev/null
mount -t devtmpfs devtmpfs /dev      2>/dev/null

# Regenerate modules.dep so modprobe can resolve dependencies. The apk we
# pulled doesn't ship a pre-built modules.dep that matches the layout we
# repacked; busybox depmod is sufficient.
depmod -a 2>/dev/null || true

for m in virtio virtio_ring virtio_pci virtio_console virtio_blk virtio_net \
         fuse virtiofs loop squashfs overlay \
         vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport; do
    modprobe "$m" 2>/dev/null
done

# ---- step 2: cmdline parsing in pure shell ------------------------------

read CMDLINE < /proc/cmdline

cmdline_get() {
    _key="$1"
    for _tok in $CMDLINE; do
        case "$_tok" in
            "$_key="*) echo "${_tok#${_key}=}"; return ;;
        esac
    done
}

cmdline_all() {
    _key="$1"
    for _tok in $CMDLINE; do
        case "$_tok" in
            "$_key="*) echo "${_tok#${_key}=}" ;;
        esac
    done
}

# Pull out the flags we care about up front.
ROOTFS_BLOCK="$(cmdline_get vmette.rootfs_block)"
ROOTFS_RO="$(cmdline_get vmette.rootfs_ro)"
USE_SWITCH_ROOT="$(cmdline_get vmette.switch_root)"
VMETTE_NET="$(cmdline_get vmette.net)"
VMETTE_VSOCK_PORT="$(cmdline_get vmette.vsock_port)"
VMETTE_SNAPSHOT_MODE="$(cmdline_get vmette.snapshot_mode)"
VMETTE_GUEST_VSOCK_PORT="$(cmdline_get vmette.guest_vsock_port)"
VMETTE_DESKTOP="$(cmdline_get vmette.desktop)"
VMETTE_DISPLAY="$(cmdline_get vmette.display)"
export VMETTE_VSOCK_PORT VMETTE_GUEST_VSOCK_PORT

# ---- step 3: mount rootfs share -----------------------------------------

# EXIT_VIA_CTL=1 means the guest root is not host-visible (block or overlaid
# virtio-fs), so the exit code is written to the writable "ctl" share the host
# injected, not onto the root itself. Set by the overlay branches below.
EXIT_VIA_CTL=0

if [ -n "$ROOTFS_BLOCK" ]; then
    # Block-image rootfs (e.g. squashfs): the host attached the image
    # read-only as /dev/vda (storage slot 0). Mount it read-only as the
    # lower layer and overlay a tmpfs so the guest gets a writable / that
    # is discarded on shutdown — exactly the sandbox semantic. fstype is
    # taken verbatim from the cmdline so this branch is fs-agnostic.
    DEV=/dev/vda
    modprobe "$ROOTFS_BLOCK" 2>/dev/null
    modprobe overlay 2>/dev/null
    mkdir -p /lower /ovl /newroot
    if mount -t "$ROOTFS_BLOCK" -o ro "$DEV" /lower 2>/dev/null; then
        log "mounted $ROOTFS_BLOCK $DEV at /lower (ro)"
    else
        log "FATAL: could not mount $ROOTFS_BLOCK on $DEV; dropping to shell"
        exec /bin/sh
    fi
    mount -t tmpfs tmpfs /ovl 2>/dev/null
    mkdir -p /ovl/upper /ovl/work
    if mount -t overlay overlay \
        -o lowerdir=/lower,upperdir=/ovl/upper,workdir=/ovl/work /newroot 2>/dev/null; then
        log "overlay root at /newroot (upper=tmpfs, lower=$ROOTFS_BLOCK ro)"
        EXIT_VIA_CTL=1
    else
        log "FATAL: overlay mount failed; dropping to shell"
        exec /bin/sh
    fi
elif [ "$(cmdline_get vmette.rootfs)" = "1" ]; then
    # Directory rootfs over virtio-fs. The host always shares it READ-ONLY.
    if [ "$ROOTFS_RO" = "1" ]; then
        # `--rootfs-ro`: mount it read-only directly, no writable layer.
        if mount -t virtiofs -o ro rootfs /newroot 2>/dev/null; then
            log "mounted virtio-fs 'rootfs' at /newroot (ro)"
        else
            log "FATAL: could not mount virtio-fs tag 'rootfs'; dropping to shell"
            exec /bin/sh
        fi
    else
        # Default: overlay a per-session tmpfs upper over the read-only
        # virtio-fs lower, so the guest gets a writable / whose writes are
        # discarded on shutdown and NEVER reach the shared host directory —
        # the same isolation the block path gets. Without this, every session
        # sharing the extracted rootfs dir would see each other's writes.
        modprobe overlay 2>/dev/null
        mkdir -p /lower /ovl /newroot
        if mount -t virtiofs -o ro rootfs /lower 2>/dev/null; then
            log "mounted virtio-fs 'rootfs' at /lower (ro, overlay lower)"
        else
            log "FATAL: could not mount virtio-fs tag 'rootfs'; dropping to shell"
            exec /bin/sh
        fi
        mount -t tmpfs tmpfs /ovl 2>/dev/null
        mkdir -p /ovl/upper /ovl/work
        if mount -t overlay overlay \
            -o lowerdir=/lower,upperdir=/ovl/upper,workdir=/ovl/work /newroot 2>/dev/null; then
            log "overlay root at /newroot (upper=tmpfs, lower=virtio-fs ro)"
            EXIT_VIA_CTL=1
        else
            log "FATAL: overlay mount over virtio-fs failed; dropping to shell"
            exec /bin/sh
        fi
    fi
else
    log "no rootfs share; running in initramfs (limited environment)"
    cp -a /bin /sbin /usr /etc /lib /newroot/ 2>/dev/null
fi

# ---- step 3b: networking (when --net is set) ----------------------------
# Bring up the first non-lo interface via DHCP. NAT'd by VZ so we get a
# 192.168.x.x address. udhcpc takes 1-2 s — only run when asked.
if [ "$VMETTE_NET" = "1" ]; then
    # Write our own udhcpc script; busybox udhcpc requires one and we
    # can't rely on alpine's being present in the initramfs.
    cat > /tmp/udhcpc.sh <<'UDHCPC_EOF'
#!/bin/sh
[ -z "$1" ] && exit 1
case "$1" in
    deconfig)
        ip addr flush dev "$interface" 2>/dev/null
        ;;
    bound|renew)
        ip addr add "$ip/${mask:-24}" dev "$interface" 2>/dev/null
        ip link set "$interface" up 2>/dev/null
        [ -n "$router" ] && ip route add default via "$router" 2>/dev/null
        : > /etc/resolv.conf
        [ -n "$domain" ] && echo "search $domain" >> /etc/resolv.conf
        for n in $dns; do echo "nameserver $n" >> /etc/resolv.conf; done
        ;;
esac
exit 0
UDHCPC_EOF
    chmod +x /tmp/udhcpc.sh

    IFACE=""
    for i in /sys/class/net/*; do
        name="${i##*/}"
        [ "$name" = "lo" ] && continue
        IFACE="$name"; break
    done

    if [ -n "$IFACE" ]; then
        ip link set "$IFACE" up 2>/dev/null
        if udhcpc -i "$IFACE" -q -t 3 -n -s /tmp/udhcpc.sh 2>/tmp/udhcpc.err; then
            IPADDR="$(ip -4 addr show "$IFACE" 2>/dev/null | awk '/inet /{print $2; exit}')"
            log "network up on $IFACE ($IPADDR)"
            if [ -f /etc/resolv.conf ] && [ -d /newroot/etc ]; then
                cp /etc/resolv.conf /newroot/etc/resolv.conf 2>/dev/null
            fi
        else
            log "WARNING: udhcpc on $IFACE failed: $(cat /tmp/udhcpc.err 2>/dev/null)"
        fi
    else
        log "WARNING: --net set but no non-lo interface found"
    fi
fi

# ---- step 4: mount additional virtio-fs shares --------------------------

mkdir -p /newroot/mnt
for tag in $(cmdline_all vmette.share); do
    mkdir -p "/newroot/mnt/$tag"
    if mount -t virtiofs "$tag" "/newroot/mnt/$tag" 2>/dev/null; then
        log "mounted virtio-fs '$tag' at /mnt/$tag"
    else
        log "warning: failed to mount virtio-fs '$tag'"
    fi
done

# ---- step 5: prepare /proc /sys /dev for chroot or switch_root ---------

mkdir -p /newroot/tmp
mount -t tmpfs tmpfs /newroot/tmp 2>/dev/null

if [ "$USE_SWITCH_ROOT" = "1" ]; then
    for d in proc sys dev; do
        mkdir -p "/newroot/$d"
        mount --move "/$d" "/newroot/$d" 2>/dev/null
    done
else
    for d in proc sys dev; do
        mkdir -p "/newroot/$d"
        mount --bind "/$d" "/newroot/$d" 2>/dev/null
    done
fi

# ---- step 6: run the workload -------------------------------------------

# Snapshot build mode: chroot in, exec vsock-runner which signals READY to
# the host then blocks on accept() for a command. The host pauses + saves
# the VM at the accept() blocker; on resume, vsock-runner reads the new
# command, runs it, streams output back, reboots.
if [ "$VMETTE_SNAPSHOT_MODE" = "server" ]; then
    if [ -z "$VMETTE_VSOCK_PORT" ] || [ -z "$VMETTE_GUEST_VSOCK_PORT" ]; then
        log "FATAL: snapshot_mode=server but vsock ports not set"
        sync; poweroff -f; sleep 60
    fi
    log "snapshot mode: exec vsock-runner $VMETTE_VSOCK_PORT $VMETTE_GUEST_VSOCK_PORT"
    if [ "$USE_SWITCH_ROOT" = "1" ]; then
        exec switch_root /newroot /usr/local/bin/vsock-runner "$VMETTE_VSOCK_PORT" "$VMETTE_GUEST_VSOCK_PORT"
    fi
    exec chroot /newroot /usr/local/bin/vsock-runner "$VMETTE_VSOCK_PORT" "$VMETTE_GUEST_VSOCK_PORT"
fi

# Desktop (Agent) mode: chroot/switch_root into the desktop rootfs and exec
# its entrypoint, which starts Xvfb + a WM + vmette-desktop-agent. The agent
# connects out to the host on vmette.vsock_port and serves the framed
# screenshot/input protocol; it is long-lived (the host ends the session via
# an explicit stop), so we do NOT poweroff here — we exec the entrypoint and
# let it own PID 1.
if [ "$VMETTE_DESKTOP" = "1" ]; then
    if [ -z "$VMETTE_VSOCK_PORT" ]; then
        log "FATAL: desktop mode but vmette.vsock_port not set"
        sync; poweroff -f; sleep 60
    fi
    SIZE="${VMETTE_DISPLAY:-1280x800}"
    ENTRY=/usr/local/bin/vmette-desktop-entrypoint.sh
    if [ ! -x "/newroot$ENTRY" ]; then
        log "FATAL: $ENTRY missing in rootfs (is this the vmette-desktop image?)"
        sync; poweroff -f; sleep 60
    fi
    log "desktop mode: exec $ENTRY (port $VMETTE_VSOCK_PORT, display $SIZE)"
    if [ "$USE_SWITCH_ROOT" = "1" ]; then
        exec switch_root /newroot "$ENTRY" "$VMETTE_VSOCK_PORT" "$SIZE"
    fi
    exec chroot /newroot "$ENTRY" "$VMETTE_VSOCK_PORT" "$SIZE"
fi

B64="$(cmdline_get vmette.exec)"

if [ -n "$B64" ]; then
    USER_CMD="$(printf '%s' "$B64" | base64 -d 2>/dev/null)"
    if [ -z "$USER_CMD" ]; then
        log "FATAL: vmette.exec base64 decode failed"
        sync; poweroff -f; sleep 60
    fi
fi

EXIT_FILE=""
if [ "$EXIT_VIA_CTL" = "1" ]; then
    # The overlay root's writable upper is a tmpfs the host can't see, so
    # write the exit code into the dedicated writable "ctl" virtio-fs share
    # the host reads back. It is mounted at /newroot/mnt/ctl (step 4) and,
    # being under /newroot, survives switch_root automatically. The path is
    # relative to the post-pivot/chroot root, so it works for both the
    # chroot (/newroot/mnt/ctl/...) and switch_root (/mnt/ctl/...) paths.
    EXIT_FILE="/mnt/ctl/.vmette-exit"
elif [ "$ROOTFS_RO" != "1" ]; then
    # No overlay and not read-only: a host-visible writable root (the
    # initramfs-only fallback). Drop the exit code onto the root directly.
    EXIT_FILE="/.vmette-exit"
fi

if [ "$USE_SWITCH_ROOT" = "1" ]; then
    if [ "$ROOTFS_RO" = "1" ]; then
        log "WARNING: --switch-root with read-only rootfs — exit code won't propagate"
    fi
    RUNNER="/newroot/.vmette-runner.sh"
    if [ -n "$USER_CMD" ]; then
        cat > "$RUNNER" 2>/dev/null <<RUNNER_EOF
#!/bin/sh
export VMETTE_VSOCK_PORT='$VMETTE_VSOCK_PORT'
[ -r /.vmette-image-env ] && . /.vmette-image-env 2>/dev/null
/bin/sh -c '$(printf '%s' "$USER_CMD" | sed "s/'/'\\\\''/g")'
RC=\$?
sync
${EXIT_FILE:+echo "\$RC" > "$EXIT_FILE" 2>/dev/null}
poweroff -f
sleep 60
RUNNER_EOF
        chmod +x "$RUNNER" 2>/dev/null
        log "switch_root → /.vmette-runner.sh"
        exec switch_root /newroot /.vmette-runner.sh
    fi
    log "switch_root → interactive shell"
    exec switch_root /newroot /bin/sh
fi

# ---- chroot path (default) ----------------------------------------------

if [ -z "$B64" ]; then
    log "no vmette.exec; dropping to interactive shell in chroot"
    chroot /newroot /bin/sh
    RC=$?
else
    log "exec: $USER_CMD"
    # Source the image's env (PATH etc., written by the OCI provider) if present,
    # then run the user command. $USER_CMD is passed as a positional arg so it
    # needs no re-escaping here.
    chroot /newroot /bin/sh -c '[ -r /.vmette-image-env ] && . /.vmette-image-env 2>/dev/null; exec /bin/sh -c "$1"' vmette "$USER_CMD"
    RC=$?
    log "exit=$RC"
fi

sync
if [ -n "$EXIT_FILE" ]; then
    echo "$RC" > "/newroot$EXIT_FILE" 2>/dev/null
fi
poweroff -f

while :; do sleep 60; done
