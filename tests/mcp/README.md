# MCP end-to-end harness

Manual, live end-to-end tests for the `vmette-mcp` server — the AI-agent surface
of vmette. Like [`tests/run.sh`](../run.sh), these boot **real VMs**, so they
need a codesigned macOS build and (for the network/desktop suites) outbound
network. They are not part of `cargo test` and run on demand.

## Build + sign

The harness rebuilds and re-signs the binaries under test **from source on
startup** (like `tests/desktop.sh`), so a stale or unsigned binary can never
satisfy the gates — no manual build step is needed. It compiles `vmette-mcp`,
`vmette-cli`, and `vmette-daemon`, then codesigns `vmette-mcp`, `vmetted`, and
`vmette` with the virtualization entitlement: `vmette-mcp` boots one-shot VMs
**in-process** and auto-spawns `vmetted` (also in-process) for the desktop
tools, so each binary needs `com.apple.security.virtualization`.

The harness launches `target/release/vmette-mcp --allow-network` and points it
at `target/release/vmette` (override with `VMETTE_MCP_BIN` / `VMETTE_BIN`). The
repo root is auto-detected; set `VMETTE_REPO` to run from a copy elsewhere. When
`VMETTE_MCP_BIN` points outside the repo, the auto build+sign is skipped (you
own building/signing it); `VMETTE_SKIP_BUILD=1` also skips it.

## Suites

| Script | What it exercises |
|--------|-------------------|
| `driver.py` | The subprocess path: `tools/list`, `execute` (python/node/shell, quoting, timeout→124, unknown-language rejection), the `workspace_*` lifecycle (create/write/run/read/destroy + path-traversal rejection), and `fetch_url` (200 body + `file://` scheme rejection). Also the shared MCP client (`MCP`, `text_of`, `check`) imported by the other scripts. |
| `desktop_e2e.py` | The stateful desktop path through `vmetted`: `desktop_start` (browser rootfs + Xvfb), `desktop_screenshot`, `desktop_launch` (background a command, wait for paint + settle, return the frame), `desktop_what_changed`, and the mouse path (`desktop_move`/`desktop_click`/`desktop_cursor_position`). Saves frames to `/tmp/desk_*.png`. |
| `call.py` | One-shot invoker for ad-hoc driving: `call.py <tool> '<json-args>' [--save shot.png]`. Successive calls can target the same `session_id` to drive a persistent desktop. |

## Run

```bash
python3 tests/mcp/driver.py

# desktop suite uses the published default image
# (ghcr.io/chamuka-inc/vmette-desktop:latest) when DESKTOP_IMAGE is unset; or
# point DESKTOP_IMAGE at your own GUI rootfs (Xvfb + a window manager):
DESKTOP_IMAGE=tar+file:///tmp/my-desktop-rootfs.tar \
  python3 tests/mcp/desktop_e2e.py

# ad-hoc:
python3 tests/mcp/call.py desktop_screenshot '{"session_id":"<id>"}' --save /tmp/shot.png
```

Each suite prints `PASS`/`FAIL` per check and exits non-zero if any check fails.
