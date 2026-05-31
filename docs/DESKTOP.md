# Desktop computer use

vmette can run a **persistent graphical Linux desktop** inside a microVM and
drive it the way a computer-use agent expects: take a screenshot, decide,
move/click/type, screenshot again. This is the opposite of the headless
one-shot path — the VM stays alive across many actions until you explicitly
stop it.

There is no Apple graphics window involved. The guest runs a headless X server
(`Xvfb :99`) plus a lightweight window manager (`openbox`), and an in-guest C
agent (`vmette-desktop-agent`) captures the framebuffer with `XGetImage` and
injects input with `XTEST`. The agent speaks a small framed protocol over
**vsock** — the same bidirectional channel vmette already wires up — so no
network and no display server on the host are required.

## Architecture

```
vmette-mcp  desktop_* tools ─┐
vmette CLI  `desktop` subcmd ─┼─ UNIX socket ─▶ vmetted (session registry)
                              │                    │ holds a live vmette::Session per id
                              └────────────────────┘
                                                   ▼ framed vsock round-trip
              guest: Xvfb :99 + openbox + vmette-desktop-agent (XTEST / XGetImage)
```

The host-side primitive is `vmette::Session` with the **Agent** workload
strategy. The one-shot `run()` path is the same primitive with the **OneShot**
strategy; desktop is purely additive and never touches the headless fast path.

Sessions are owned by **vmetted**, not by the client connection that created
them (each connection is one request). A session therefore outlives its creating
connection and is freed only by `desktop_stop`, idle eviction, or daemon
shutdown. The daemon caps concurrent sessions (each is a ~2 GB VM) and evicts
sessions left untouched for longer than the idle TTL (30 min).

## Prerequisites

1. **The daemon must be running.** All desktop access (CLI and MCP) routes
   through `vmetted`:

   ```sh
   vmetted &
   ```

2. **The desktop rootfs image.** Build and (optionally) push it:

   ```sh
   bash scripts/build-desktop-image.sh                 # local build
   bash scripts/build-desktop-image.sh --push          # push to ghcr.io
   bash scripts/build-desktop-image.sh --tag my/desktop:dev
   ```

   The default ref is `ghcr.io/chamuka-inc/vmette-desktop:latest`, baked into
   the daemon and MCP defaults the same way `python:3.12-alpine` is. The image
   is x86_64-only (`--platform linux/amd64`), matching vmette's guest assets;
   on Apple Silicon the build needs Docker buildx + qemu emulation.

   The image bundles `xvfb`, `openbox`, `x11-utils`, base fonts, the compiled
   `vmette-desktop-agent`, and an entrypoint that starts `Xvfb`, the WM, then
   the agent. It also ships `chromium` plus an `/etc/chromium.d/` flags file so
   a bare `chromium <url>` renders under the headless software-GL guest
   (`--no-sandbox`, `--use-gl=swiftshader`, …) — the browser incantation lives
   with the browser, in the image, so `desktop_launch` and the CLI stay
   application-agnostic. Drop the `chromium` install line to shrink the image;
   the agent works without it (you can launch any X app).

## Use it (CLI)

The `vmette desktop` subcommand group is a thin client for manual end-to-end
testing without an MCP host:

```sh
SID=$(vmette desktop start)                  # boots a desktop, prints SESSION_ID
vmette desktop screenshot "$SID" --out shot.png
open shot.png                                # confirm a rendered desktop

vmette desktop exec "$SID" 'xterm &'         # launch an app
vmette desktop screenshot "$SID" --out shot2.png

vmette desktop move  "$SID" 640 400
vmette desktop click "$SID" 640 400
vmette desktop type  "$SID" 'echo hello'
vmette desktop key   "$SID" 'Return'
vmette desktop scroll "$SID" 640 400 down 3
vmette desktop cursor "$SID"                 # prints "X Y"

vmette desktop stop "$SID"                   # tear it down
```

`start` options: `--image REF`, `--size WxH`, `--net`, `--offline`,
`--kernel PATH`, `--initramfs PATH` (kernel/initramfs default to
`assets/vmlinuz-virt` and `assets/initramfs-vmette` when run from the repo).

Global: `--socket PATH` overrides the daemon socket (default
`~/Library/Caches/vmette/vmette.sock`).

## Use it (AI agents via MCP)

`vmette-mcp` exposes the desktop tools to any MCP host. They require `vmetted`
to be running; the MCP server connects to its socket. Override the socket with
`--socket PATH`.

| Tool | Input | Returns |
|------|-------|---------|
| `desktop_start` | `image?`, `size?`, `network?` | session id (text) |
| `desktop_screenshot` | `session_id` | **PNG image content block** |
| `desktop_screenshot_when_settled` | `session_id`, `timeout_ms?` | **PNG image content block** (once the screen stops changing) |
| `desktop_what_changed` | `session_id` | a note describing the changed region since the last capture |
| `desktop_cursor_position` | `session_id` | `"x y"` |
| `desktop_move` | `session_id`, `x`, `y` | status text |
| `desktop_click` | `session_id`, `x`, `y` | status text |
| `desktop_double_click` | `session_id`, `x`, `y` | status text |
| `desktop_right_click` | `session_id`, `x`, `y` | status text |
| `desktop_type` | `session_id`, `text` | status text |
| `desktop_key` | `session_id`, `keys` | status text |
| `desktop_scroll` | `session_id`, `x`, `y`, `direction`, `amount` | status text |
| `desktop_exec` | `session_id`, `command` | status text (fire-and-forget) |
| `desktop_launch` | `session_id`, `command`, `wait_ms?` | **PNG image content block** (the app's first painted frame) |
| `desktop_stop` | `session_id` | status text |

`desktop_screenshot` returns an MCP image content block
(`image/png`), which is what makes the loop consumable by a computer-use agent.
`desktop_click` / `desktop_double_click` / `desktop_right_click` move the
pointer to `(x, y)` first, then click (agent click actions fire at the current
pointer position). `network=true` on `desktop_start` is subject to the server's
`--allow-network` gate.

**Starting an app and seeing it: `desktop_launch`.** `desktop_exec` is
fire-and-forget — it launches a command and returns immediately, leaving you to
poll for the window. `desktop_launch` is the one-call alternative: it
backgrounds the command (redirecting its stdio to a guest log so a chatty app
can't block before painting), waits for the screen to actually change and then
settle, and returns that frame. It is **application-agnostic** — it knows
nothing about browsers. You pass a complete command and supply whatever flags
the app needs; e.g. `command: "chromium https://example.com"`,
`"gimp /mnt/a.png"`, or `"xterm"`. The app-specific incantation a headless
software-rendered guest requires (for the browser: `--no-sandbox`, software GL)
lives in the **desktop image**, not in this tool — see below — so a bare
`chromium <url>` renders. Network-dependent apps only reach the network when the
session was started with `network=true`.

## Protocol

### Daemon (UNIX socket, line-delimited JSON)

One request object per connection; one reply object back.

```jsonc
// → boot a session
{ "kind": "desktop_start",
  "kernel": "/abs/vmlinuz-virt", "initramfs": "/abs/initramfs-vmette",
  "image": "ghcr.io/chamuka-inc/vmette-desktop:latest",   // optional
  "size": "1280x800",                                       // optional
  "net": false, "offline": false }
// ← { "kind": "session", "session_id": "a1b2c3..." }

// → one action
{ "kind": "desktop_action", "session_id": "a1b2c3...",
  "action": { "action": "left_click" } }
// ← { "kind": "action_result", "ok": true }
//   screenshots add "png_base64"; cursor_position adds "x"/"y";
//   failures set "ok": false and "error".

// → stop
{ "kind": "desktop_stop", "session_id": "a1b2c3..." }
// ← { "kind": "stopped" }
```

Errors come back as `{ "kind": "error", "message": "..." }`.

### Guest (framed vsock)

Between the host `Session` and the in-guest agent the wire format is binary:

```text
[u32 LE header_len][header JSON][optional binary payload]
```

The request header is an `Action`; the response header is a `ResponseHeader`
(`ok`, `error?`, `x?`, `y?`, `payload_len`). Screenshots travel as a raw PNG
payload after the header. See `crates/vmette/src/desktop.rs`.

## Action reference

Actions mirror the Anthropic computer-use tool so the MCP layer maps 1:1.
JSON shape is `{"action": "<name>", ...fields}`.

| Action | Fields | Effect |
|--------|--------|--------|
| `screenshot` | — | Capture framebuffer → PNG payload. |
| `cursor_position` | — | Report pointer `(x, y)` in the header. |
| `mouse_move` | `x`, `y` | Absolute pointer move. |
| `left_click` | — | Left click at current position. |
| `right_click` | — | Right click at current position. |
| `middle_click` | — | Middle click at current position. |
| `double_click` | — | Double left click at current position. |
| `left_click_drag` | `x`, `y` | Press, move to `(x, y)`, release. |
| `type` | `text` | Type a UTF-8 string via synthetic key events. |
| `key` | `keys` | Press a chord, e.g. `"ctrl+c"`, `"Return"`, `"alt+Tab"`. |
| `scroll` | `x`, `y`, `direction`, `amount` | Scroll `amount` clicks (`up`/`down`/`left`/`right`). |
| `wait` | `ms` | Sleep guest-side to let the UI settle. |
| `exec` | `command` | Launch a shell command (e.g. `"chromium &"`). |

## Constraints

- **Software-rendered Xvfb, no GPU.** Fine for agentic GUI control and UI
  testing; not for video / WebGL / 3D.
- **Slower boot than headless** — several seconds for the desktop image + Xvfb
  + WM + first app, versus ~1 s for a headless one-shot.
- **Memory:** each session is a live VM holding a browser; budget 1–2 GB RAM
  and ≥2 vCPUs per session. The daemon caps concurrent sessions.
- **Idle eviction:** sessions untouched for 30 minutes are force-stopped.
- **Arch:** the desktop image and agent are x86_64-only, matching vmette's
  guest assets.
- **No human-viewable display.** A VNC bridge (x11vnc over virtio-net/vsock) is
  a deliberate follow-on; the agent path needs no display.
