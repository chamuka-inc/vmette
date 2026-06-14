#!/usr/bin/env python3
"""Drive a Chromium web task through vmette-mcp (live, via vmetted).

Boots the *browser* desktop rootfs and opens a real web page with a single
generic `desktop_launch` call. The tool knows nothing about browsers: it
backgrounds whatever command you give it, waits for the screen to paint and
settle, and returns the frame. The Chromium incantation (software GL,
--no-sandbox) lives in the desktop *image* (/etc/chromium.d/*), so a bare
`chromium <url>` renders — no caller has to know the flags. Then it exercises
the mouse input path. All over the MCP desktop_* tools, the same surface a
computer-use agent would use.

Image: leave DESKTOP_IMAGE unset (the default) to let vmette resolve the desktop
image the same way the CLI/MCP do — auto-discover a local
assets/<arch>/vmette-desktop-rootfs.tar (build one with `make desktop-image`),
else pull the published DEFAULT_DESKTOP_IMAGE. Set DESKTOP_IMAGE to pin a
specific rootfs, e.g. `tar+file:///path/to/rootfs.tar` or an OCI ref.
"""
import base64
import os
import sys
import time

from driver import MCP, text_of, check, PASS, FAIL

# Unset → let vmette resolve the default desktop image: it auto-discovers a
# local assets/<arch>/vmette-desktop-rootfs.tar, else pulls the published
# DEFAULT_DESKTOP_IMAGE. Set DESKTOP_IMAGE to pin a specific rootfs ref.
IMAGE = os.environ.get("DESKTOP_IMAGE")
QUERY = os.environ.get("DESKTOP_QUERY", "Apple Virtualization framework")
URL = "https://duckduckgo.com/?q=" + QUERY.replace(" ", "+")

# A pure-black 1280x800 PNG compresses to ~30 KB; a rendered page lands well
# north of this. Used to tell "Chromium has painted" from "still a black
# window mid-load" without decoding pixels.
RENDER_MIN = 50_000


def png_of(resp):
    """Return decoded PNG bytes from a tools/call image content block, or None."""
    for c in resp.get("result", {}).get("content", []):
        if c.get("type") == "image":
            return base64.b64decode(c["data"])
    return None


def shot(m, sid, path):
    """Take a screenshot, save it, return (png_bytes or None)."""
    r = m.call_tool("desktop_screenshot", {"session_id": sid}, timeout=60)
    png = png_of(r)
    if png:
        with open(path, "wb") as f:
            f.write(png)
    return png


def preflight():
    if IMAGE and IMAGE.startswith("tar+file://"):
        p = IMAGE[len("tar+file://"):]
        if not os.path.exists(p):
            print(f"✗ browser rootfs tar not found: {p}", file=sys.stderr)
            print("  produce it (see this file's docstring) or set DESKTOP_IMAGE.",
                  file=sys.stderr)
            sys.exit(2)


def main():
    preflight()
    m = MCP(allow_network=True)
    m.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                             "clientInfo": {"name": "desk", "version": "0"}})
    m.notify("notifications/initialized")

    print(f"== desktop_start (browser rootfs + Xvfb, network on; up to 8 min)\n   image={IMAGE or '(server default)'}")
    t0 = time.time()
    start_args = {"size": "1280x800", "network": True}
    if IMAGE:
        start_args["image"] = IMAGE
    r = m.call_tool("desktop_start", start_args, timeout=480)
    if "error" in r:
        check("desktop_start", False, str(r["error"])[:160]); m.close(); sys.exit(1)
    sid = text_of(r).strip()
    print(f"  session_id={sid}  ({time.time()-t0:.0f}s)")
    check("desktop_start returns session id", bool(sid))

    try:
        print("\n== first screenshot (bare desktop)")
        png = shot(m, sid, "/tmp/desk_0_boot.png")
        ok = png is not None and png[:8] == b"\x89PNG\r\n\x1a\n"
        check("screenshot returns valid PNG", ok, f"{len(png) if png else 0} bytes")

        # The whole browser-task flow is now one generic call: desktop_launch
        # backgrounds the command, waits for first paint, settles, and hands
        # back the rendered page as an image content block. Note the command is
        # a bare `chromium <url>` — the software-GL flags come from the image.
        print(f"\n== desktop_launch: chromium {URL}")
        t1 = time.time()
        r = m.call_tool("desktop_launch",
                        {"session_id": sid, "command": f"chromium {URL}", "wait_ms": 90000},
                        timeout=180)
        if "error" in r:
            check("desktop_launch", False, str(r["error"])[:160])
            raise SystemExit("desktop_launch failed")
        note = text_of(r)
        png = png_of(r)
        if png:
            with open("/tmp/desk_1_page.png", "wb") as f:
                f.write(png)
        res_sz = len(png) if png else 0
        print(f"  note: {note!r}  ({time.time()-t1:.0f}s)")
        print(f"  page frame {res_sz} bytes -> /tmp/desk_1_page.png")
        check("desktop_launch reports it launched", note.startswith("launched"))
        check("page rendered (not a black window)", res_sz >= RENDER_MIN,
              f"{res_sz} bytes (need >= {RENDER_MIN})")

        print("\n== what_changed after the open")
        r = m.call_tool("desktop_what_changed", {"session_id": sid}, timeout=60)
        wnote = next((c["text"] for c in r.get("result", {}).get("content", [])
                      if c.get("type") == "text"), "")
        print("  note:", wnote)
        check("what_changed reports a note", bool(wnote))

        print("\n== mouse input path (move + click + cursor_position)")
        m.call_tool("desktop_move", {"session_id": sid, "x": 200, "y": 200}, timeout=60)
        r = m.call_tool("desktop_click", {"session_id": sid, "x": 200, "y": 200}, timeout=60)
        check("click ok", "click at 200 200" in text_of(r))
        r = m.call_tool("desktop_cursor_position", {"session_id": sid}, timeout=60)
        check("cursor reports 200 200", text_of(r).strip() == "200 200", text_of(r).strip())
    finally:
        print("\n== desktop_stop")
        r = m.call_tool("desktop_stop", {"session_id": sid}, timeout=60)
        print("  stop:", text_of(r))
        check("desktop_stop ok", "stopped" in text_of(r))

    m.close()
    print(f"\n==== DESKTOP RESULT: {len(PASS)} passed, {len(FAIL)} failed ====")
    print("  screenshots: /tmp/desk_0_boot.png /tmp/desk_1_page.png")
    if FAIL:
        print("FAILED:", FAIL); sys.exit(1)


if __name__ == "__main__":
    main()
