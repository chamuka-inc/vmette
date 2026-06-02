#!/usr/bin/env python3
"""Minimal RFB (VNC) client that exercises vmetted's live desktop view.

Usage: rfb_probe.py HOST PORT EXP_W EXP_H MOVE_X MOVE_Y [--stream SECONDS]

Performs the RFB 3.8 handshake, asserts the ServerInit framebuffer size, reads
one (non-incremental) FramebufferUpdate and consumes its Raw rectangles, then
either:

  * (default) injects a bare pointer move to (MOVE_X, MOVE_Y) — which the daemon
    translates into a MouseMove action; the harness then independently asks the
    daemon for the cursor position to confirm the input round-tripped; or
  * (--stream SECONDS) runs the real interactive client loop — request an
    incremental update, wait for it, repeat — for SECONDS against a screen that
    is changing, asserting frames keep arriving. This is the regression for the
    writer's lost-wakeup stall, where a request arriving mid-send was dropped
    and the client waited forever; a single-shot probe never triggers it.

Pure stdlib; speaks only what the vmette RFB server implements (3.8, VNC
Authentication unverified, Raw). Prints OK lines on success; exits non-zero with
a message on any mismatch.
"""
import socket
import struct
import sys
import time


def recvn(s, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"short read: wanted {n}, got {len(buf)}")
        buf += chunk
    return bytes(buf)


def read_framebuffer_update(s, bytes_per_px):
    """Read one FramebufferUpdate message; return the total pixels it carried."""
    msg = recvn(s, 1)[0]
    if msg != 0:
        sys.exit(f"FAIL: expected FramebufferUpdate (0), got message type {msg}")
    recvn(s, 1)  # padding
    nrects = struct.unpack(">H", recvn(s, 2))[0]
    total = 0
    for _ in range(nrects):
        _x, _y, rw, rh, enc = struct.unpack(">HHHHi", recvn(s, 12))
        if enc != 0:
            sys.exit(f"FAIL: non-Raw encoding {enc} in rectangle")
        recvn(s, rw * rh * bytes_per_px)
        total += rw * rh
    return nrects, total


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    exp_w, exp_h = int(sys.argv[3]), int(sys.argv[4])
    move_x, move_y = int(sys.argv[5]), int(sys.argv[6])
    stream_secs = 0.0
    if "--stream" in sys.argv:
        stream_secs = float(sys.argv[sys.argv.index("--stream") + 1])

    s = socket.create_connection((host, port), timeout=20)
    s.settimeout(20)

    # ProtocolVersion.
    ver = recvn(s, 12)
    if ver != b"RFB 003.008\n":
        sys.exit(f"FAIL: unexpected version banner {ver!r}")
    s.sendall(b"RFB 003.008\n")

    # Security: server offers a type list (3.7+); we expect VNC Auth (2).
    nsec = recvn(s, 1)[0]
    if nsec == 0:
        # An error string follows; surface it.
        elen = struct.unpack(">I", recvn(s, 4))[0]
        sys.exit(f"FAIL: server rejected connection: {recvn(s, elen)!r}")
    types = recvn(s, nsec)
    if 2 not in types:
        sys.exit(f"FAIL: server did not offer VNC Auth, got {list(types)}")
    s.sendall(bytes([2]))
    # VNC Authentication: read the 16-byte challenge, send a response (the view
    # is loopback-only and does not verify it), then read SecurityResult.
    recvn(s, 16)
    s.sendall(b"\x00" * 16)
    res = struct.unpack(">I", recvn(s, 4))[0]
    if res != 0:
        sys.exit(f"FAIL: SecurityResult not OK ({res})")

    # ClientInit (shared).
    s.sendall(bytes([1]))

    # ServerInit.
    hdr = recvn(s, 24)
    w, h = struct.unpack(">HH", hdr[0:4])
    bpp = hdr[4]
    namelen = struct.unpack(">I", hdr[20:24])[0]
    name = recvn(s, namelen)
    if (w, h) != (exp_w, exp_h):
        sys.exit(f"FAIL: ServerInit size {w}x{h} != requested {exp_w}x{exp_h}")
    if bpp % 8 != 0 or bpp == 0:
        sys.exit(f"FAIL: implausible bits-per-pixel {bpp}")
    print(f"OK ServerInit {w}x{h} bpp={bpp} name={name.decode(errors='replace')!r}")

    # SetEncodings: Raw only.
    s.sendall(struct.pack(">BBH", 2, 0, 1) + struct.pack(">i", 0))

    bytes_per_px = bpp // 8

    # Full, non-incremental FramebufferUpdateRequest → expect the whole frame.
    s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))
    nrects, total = read_framebuffer_update(s, bytes_per_px)
    if nrects < 1:
        sys.exit("FAIL: initial FramebufferUpdate carried no rectangles")
    print(f"OK FramebufferUpdate {nrects} rect(s), {total} px")

    if stream_secs > 0:
        # Interactive client loop: request an incremental update, wait for it,
        # repeat. With a changing screen this must keep delivering frames; the
        # lost-wakeup stall would freeze it after the first frame or two.
        deadline = time.monotonic() + stream_secs
        frames = 0
        while time.monotonic() < deadline:
            s.sendall(struct.pack(">BBHHHH", 3, 1, 0, 0, w, h))  # incremental
            read_framebuffer_update(s, bytes_per_px)
            frames += 1
        if frames < 5:
            sys.exit(f"FAIL: only {frames} streamed updates — writer stalled")
        print(f"OK sustained streaming: {frames} updates in {stream_secs:g}s")
    else:
        # Inject a bare pointer move (no buttons) → MouseMove action guest-side.
        s.sendall(struct.pack(">BBHH", 5, 0, move_x, move_y))
        # Let the reader thread dispatch it to the agent before we disconnect.
        time.sleep(0.7)
        print(f"OK injected pointer move to {move_x},{move_y}")
    s.close()


if __name__ == "__main__":
    main()
