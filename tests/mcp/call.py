#!/usr/bin/env python3
"""One-shot MCP tool caller against the live vmette-mcp / vmetted desktop.

Usage:
  call.py <tool> '<json-args>' [--save /tmp/shot.png]

Prints the text content of the result. If the result carries an image
content block and --save is given, writes the PNG there. The desktop
session lives in vmetted, so successive invocations can target the same
session_id and drive a persistent Chromium.
"""
import base64
import json
import sys

from driver import MCP, text_of


def main():
    if len(sys.argv) < 3:
        print("usage: call.py <tool> <json-args> [--save path]", file=sys.stderr)
        sys.exit(2)
    tool = sys.argv[1]
    args = json.loads(sys.argv[2])
    save = None
    if "--save" in sys.argv:
        save = sys.argv[sys.argv.index("--save") + 1]

    m = MCP(allow_network=True)
    m.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                             "clientInfo": {"name": "mcp_call", "version": "0"}})
    m.notify("notifications/initialized")

    r = m.call_tool(tool, args, timeout=480)
    if "error" in r:
        print("ERROR:", json.dumps(r["error"]))
        m.close()
        sys.exit(1)

    res = r.get("result", {})
    img = next((c for c in res.get("content", []) if c.get("type") == "image"), None)
    if img and save:
        data = base64.b64decode(img["data"])
        with open(save, "wb") as f:
            f.write(data)
        print(f"[image {len(data)} bytes -> {save}]")
    # also print any text content
    txt = text_of(r)
    if txt:
        print(txt)
    m.close()


if __name__ == "__main__":
    main()
