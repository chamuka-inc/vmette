# Desktop computer use

vmette can run a **persistent graphical Linux desktop** inside a microVM and
drive it the way a computer-use agent expects: take a screenshot, decide,
move/click/type, screenshot again. This is the opposite of the headless
one-shot path — the VM stays alive across many actions until you explicitly
stop it.

The relief is the same as the rest of vmette: a computer-use agent gets its own
real desktop to click around in that is *not* your machine. The boundary is the
hypervisor, the screen the agent sees and the input it injects stay inside the
guest, and it reaches your host filesystem or network only where you explicitly
grant it.

Each session is also isolated **from every other session**: the desktop rootfs
is mounted read-only on the host and overlaid with a per-session tmpfs in the
guest, so anything a session writes — browser profile and cache, cookies,
downloads, `/etc` edits — lives only in that session and is discarded when it
stops. Two sessions never see each other's state, and nothing persists across a
daemon restart. (Explicit `--share`/share mounts are the deliberate exception:
those are writable and shared with the host because you asked for them.)

There is no Apple graphics window involved. The guest runs a headless X server
(`Xvfb :99`) plus a lightweight window manager (`openbox`), and a C agent
(`vmette-desktop-agent`) captures the framebuffer with `XGetImage` and injects
input with `XTEST`. The agent speaks a small framed protocol over **vsock** —
the same bidirectional channel vmette already wires up — so no network and no
display server on the host are required.

The agent itself is supplied by **vmette** (a per-arch static binary it injects
into the guest), not baked into the rootfs — so the desktop runs on any image
that provides an X server + a window manager. See
[Bring your own desktop rootfs](#bring-your-own-desktop-rootfs).

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

2. **A desktop rootfs.** You don't have to build anything: with no `--image`,
   vmette pulls the published default `ghcr.io/chamuka-inc/vmette-desktop:latest`
   automatically on first use (the image is public). The first `desktop start`
   extracts it and caches it under `~/Library/Caches/vmette/oci/`; later starts
   are cache hits. That image is a convenience default (Xvfb + openbox + chromium
   + fonts) — **this repo no longer builds it**; the agent is host-injected (see
   [Bring your own desktop rootfs](#bring-your-own-desktop-rootfs)), so any GUI
   image works and you customize by bringing your own, not by rebuilding ours.

   **Resolution order** (client-side, in `vmette` and `vmette-mcp`, mirroring how
   kernel/initramfs are resolved):

   1. explicit `--image REF` (CLI) / `image` arg (MCP) — wins
   2. `$VMETTE_DESKTOP_IMAGE` (any rootfs spec, e.g. a `tar+file://` or OCI ref)
   3. a locally-provided `assets/<arch>/vmette-desktop-rootfs.tar` (e.g. a
      `docker export` of your own GUI image) → `tar+file://…`
   4. `ghcr.io/chamuka-inc/vmette-desktop:latest` — the published default when no
      local asset is present

   Because resolution is client-side, `$VMETTE_DESKTOP_IMAGE` is read from the
   **client** process (your shell for `vmette desktop start`, the `vmette-mcp`
   server for `desktop_start`) — not the daemon.

   **No Docker needed to run.** vmette never shells out to Docker — its OCI
   provider is a self-contained registry client, so the published default works
   out of the box on a machine without Docker. The reference recipe for that
   image (and a starting point for your own) lives in `images/vmette-desktop/`
   (`Dockerfile` + `entrypoint.sh` + `vmette-open`): `xvfb`, `openbox`,
   `x11-utils`, fonts, `chromium` with an `/etc/chromium.d/` flags file (so a
   bare `chromium <url>` renders under the headless software-GL guest:
   `--no-sandbox`, `--use-gl=swiftshader`, `--start-maximized`, …).

   **Building the image is optional** (Docker required) — only to customize the
   rootfs or republish the default. `scripts/build-desktop-image.sh` wraps it:

   ```sh
   make desktop-image                          # → assets/<arch>/vmette-desktop-rootfs.tar (local, host arch)
   scripts/build-desktop-image.sh --export     # same, explicit
   scripts/build-desktop-image.sh --tag my-registry/my-desktop:latest --push   # publish your own
   scripts/build-desktop-image.sh --push       # republish the default — full amd64+arm64 manifest
   ```

   A bare `--push` always rebuilds **both** architectures into one manifest
   (arm64 builds under qemu, bundled with Docker Desktop), so a publish can never
   leave one arch stale. `--export` (the `make` target) writes the local
   `tar+file://` rootfs the CLI/MCP auto-discover ahead of the registry default.

## Bring your own desktop rootfs

The computer-use agent is **not** part of the rootfs image — vmette ships it and
injects it at boot. So a desktop session runs on **any** rootfs that provides an
**X server (`Xvfb`)** and a **window manager**; that is the entire contract. The
bundled `vmette-desktop` image is just one convenient such rootfs (it also adds
Chromium + fonts); you can equally point `--image` at a stock `debian + xvfb +
openbox` image, your own GUI image, or an OCI ref:

```sh
vmette desktop start --image tar+file:///path/to/my-gui-rootfs.tar
vmette desktop start --image my-registry/my-desktop:latest
```

How it works:

- vmette builds a **per-arch, fully-static** `vmette-desktop-agent` (musl + a
  from-source X client stack — `scripts/build-desktop-agent-static.sh`) into
  `assets/<arch>/desktop-agent/`, beside a self-contained
  `vmette-desktop-run.sh` startup. Being fully static, **one** agent binary runs
  in any rootfs regardless of its libc (glibc or musl) — it carries its own libc
  and X clients and needs nothing from the rootfs but the X server socket.

  ```sh
  bash scripts/build-desktop-agent-static.sh   # → assets/<arch>/desktop-agent/
  ```

- `vmetted` discovers that directory (`vmette_assets::resolve_agent_share`) and
  mounts it into Agent-workload guests as the **read-only** `agent` virtio-fs
  share. The guest's PID-1 init runs the injected `/mnt/agent/vmette-desktop-run.sh`,
  which brings up `Xvfb` + a window manager (auto-detected: openbox / fluxbox /
  icewm / …) and execs the static agent. The share is read-only so a guest can't
  write back into vmette's host-side agent directory.

- **Fallback / compatibility.** When the agent asset isn't present, the init
  falls back to an agent baked into the rootfs at
  `/usr/local/bin/vmette-desktop-entrypoint.sh` — which is what the bundled
  `vmette-desktop` image ships, so it keeps working unchanged. When the asset
  *is* present, the injected agent is used for every session (so a single,
  host-version-matched agent runs everywhere).

A minimal bring-your-own image needs only:

```dockerfile
FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends xvfb openbox \
 && rm -rf /var/lib/apt/lists/*
# add your apps (a browser, an editor, …); no vmette agent required.
```

The agent injects a host CA-cert policy for Chromium when `--ca-certs` is in
play (best-effort), but otherwise makes no assumptions about the rootfs's apps.

## Use it (CLI)

The `vmette desktop` subcommand group is a thin client for manual end-to-end
testing without an MCP host:

```sh
SID=$(vmette desktop start)                  # boots a desktop, prints SESSION_ID
vmette desktop screenshot "$SID" --out shot.png
open shot.png                                # confirm a rendered desktop

vmette desktop exec "$SID" 'xterm &'         # launch an app
vmette desktop screenshot "$SID" --out shot2.png

vmette desktop navigate "$SID" https://example.com   # open a URL (no shell)
vmette desktop exec-capture "$SID" 'cat /etc/os-release'   # run a command, print its output

vmette desktop move  "$SID" 640 400
vmette desktop click "$SID" 640 400
vmette desktop drag  "$SID" 200 200 600 400  # press at (200,200), drag to (600,400)
vmette desktop type  "$SID" 'echo hello'
vmette desktop key   "$SID" 'Return'
vmette desktop scroll "$SID" 640 400 down 3
vmette desktop cursor "$SID"                 # prints "X Y"

vmette desktop stop "$SID"                   # tear it down
```

`start` options: `--image REF`, `--size WxH`, `--net`, `--offline`,
`--ca-certs DIR`, `--kernel PATH`, `--initramfs PATH` (kernel/initramfs default to
`assets/<arch>/vmlinuz-virt` and `assets/<arch>/initramfs-vmette` when run from the repo).

`--ca-certs DIR` mounts a host directory of `.crt` / `.pem` enterprise CA
certificates at `/mnt/certs`. At desktop boot the guest installs them into the
system trust store (generically, in the initramfs init) and the desktop image
additionally writes Chromium's managed `CACertificates` policy, so browser
automation works behind TLS-inspecting proxies without
`--ignore-certificate-errors`. When `--ca-certs` is omitted it falls back to the
machine-wide source (`$VMETTE_CA_CERTS`, else `~/.config/vmette/certs`) — the
same certificates every other vmette root trusts (see
[HACKING.md](HACKING.md#trusting-a-host-ca-in-every-guest)). On macOS,
`scripts/export-macos-ca-certs.sh` stages the keychain roots there in one step.

Global: `--socket PATH` overrides the daemon socket (default
`~/Library/Caches/vmette/vmette.sock`).

## Use it (AI agents via MCP)

`vmette-mcp` exposes the desktop tools to any MCP host. They require `vmetted`
to be running; the MCP server connects to its socket. Override the socket with
`--socket PATH`.

| Tool | Input | Returns |
|------|-------|---------|
| `desktop_start` | `image?`, `size?`, `network?`, `ca_certs?` | session id (text) |
| `desktop_screenshot` | `session_id` | a **framebuffer note** (`framebuffer WxH; …`) **plus a PNG image content block** |
| `desktop_screenshot_when_settled` | `session_id`, `timeout_ms?` | note + framebuffer note + **PNG image content block** (once the screen stops changing) |
| `desktop_what_changed` | `session_id` | a note describing the changed region since the last capture + framebuffer note **plus a PNG image content block** of the fresh frame |
| `desktop_cursor_position` | `session_id` | `"x y"` |
| `desktop_move` | `session_id`, `x`, `y` | status text (echoes where the pointer landed; flags `(constrained)`) |
| `desktop_click` | `session_id`, `x`, `y` | status text (echoes the click position) |
| `desktop_double_click` | `session_id`, `x`, `y` | status text (echoes the click position) |
| `desktop_right_click` | `session_id`, `x`, `y` | status text (echoes the click position) |
| `desktop_middle_click` | `session_id`, `x`, `y` | status text (echoes the click position) |
| `desktop_drag` | `session_id`, `x`, `y` | status text (presses the left button, moves to `(x, y)`, releases — the drag starts at the current pointer position) |
| `desktop_type` | `session_id`, `text` | status text |
| `desktop_key` | `session_id`, `keys` | status text |
| `desktop_get_clipboard` | `session_id` | the clipboard text, exact (empty if unset — click the content to focus it before `ctrl+a`/`ctrl+c`, or the copy grabs nothing) |
| `desktop_set_clipboard` | `session_id`, `text` | status text — owns the `CLIPBOARD` + `PRIMARY` selections |
| `desktop_paste` | `session_id`, `text` | status text — set the clipboard, then Ctrl+V |
| `desktop_scroll` | `session_id`, `x`, `y`, `direction`, `amount` | status text |
| `desktop_exec` | `session_id`, `command` | status text (fire-and-forget) |
| `desktop_exec_capture` | `session_id`, `command`, `timeout_ms?` | the command's combined stdout/stderr + exit code (runs to completion) |
| `desktop_navigate` | `session_id`, `url` | status text — opens `url` in the browser with no shell and no synthetic keystrokes |
| `desktop_launch` | `session_id`, `command`, `wait_ms?` | **PNG image content block** (the app's first painted frame) |
| `desktop_stop` | `session_id` | status text |

`desktop_screenshot` returns an MCP image content block
(`image/png`), which is what makes the loop consumable by a computer-use agent.
Alongside the image it returns a **framebuffer note** (`framebuffer WxH; …
origin top-left`): pointer/click coordinates are in that exact pixel space, so an
agent reasoning over a downscaled rendering can map its target back to true
coordinates instead of guessing the scale. `desktop_click` /
`desktop_double_click` / `desktop_right_click` move the pointer to `(x, y)`
first, then click (agent click actions fire at the current pointer position).
Pointer actions (`desktop_move`/click/scroll/drag) **echo where the pointer
actually landed** in their status text — if a window manager constrained the
move, the reply reads `… landed at X Y (constrained)`, so a missed target is
observable in one round-trip instead of requiring a follow-up screenshot.
`network=true` on `desktop_start` is subject to the server's `--allow-network`
gate.

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

**Navigating a browser: `desktop_navigate`.** Rather than focusing the address
bar and typing (which races omnibox autocomplete and focus), `desktop_navigate`
hands the URL straight to the browser's launcher with **no shell and no
synthetic keystrokes**, so the URL is never word-split or interpreted. It is
fire-and-forget — it returns once navigation starts, so follow it with
`desktop_screenshot_when_settled` to wait for the page to paint. The desktop
image ships a browser-agnostic `vmette-open` launcher, so a custom image can
swap browsers without touching the agent.

**Reading a command's output: `desktop_exec_capture`.** Unlike the
fire-and-forget `desktop_exec`, this runs a short command to completion and
returns its combined stdout/stderr plus exit code — for reading a file or
probing state inside a desktop session without OCR'ing a screenshot. The
in-guest agent is single-threaded, so it blocks other desktop actions until the
command returns or hits its (bounded) timeout; keep it to short, terminating
commands and use `desktop_exec` / `desktop_launch` to start GUI apps.

## Protocol

### Daemon (UNIX socket, line-delimited JSON)

One request object per connection; one reply object back.

```jsonc
// → boot a session
{ "kind": "desktop_start",
  "kernel": "/abs/vmlinuz-virt", "initramfs": "/abs/initramfs-vmette",
  "image": "tar+file:///abs/assets/aarch64/vmette-desktop-rootfs.tar", // required; client-resolved
  "size": "1280x800",                                          // optional
  "net": false, "offline": false,
  "shares": [{"tag":"certs", "path":"/abs/company-cas"}] } // optional
// ← { "kind": "session", "session_id": "a1b2c3..." }

// → one action
{ "kind": "desktop_action", "session_id": "a1b2c3...",
  "action": { "action": "left_click" } }
// ← { "kind": "action_result", "ok": true, "x": 200, "y": 200 }
//   screenshots add "png_base64"; cursor_position AND the pointer actions
//   (move/click/scroll/drag) add "x"/"y" — the resulting pointer position;
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
(`ok`, `error?`, `x?`, `y?`, `payload_len`). `x`/`y` carry the pointer position:
`cursor_position` reports it, and every pointer action echoes the *resulting*
position. Screenshots travel as a raw PNG payload after the header (its pixel
dimensions are the coordinate space for all pointer actions). See
`crates/vmette/src/desktop.rs`.

## Action reference

Actions mirror the Anthropic computer-use tool so the MCP layer maps 1:1.
JSON shape is `{"action": "<name>", ...fields}`.

| Action | Fields | Effect |
|--------|--------|--------|
| `screenshot` | — | Capture framebuffer → PNG payload. The PNG's pixel size is the coordinate space for all pointer actions. The mouse pointer is composited in (via XFixes), so the cursor shows in screenshots and the live view. |
| `cursor_position` | — | Report pointer `(x, y)` in the header. |
| `mouse_move` | `x`, `y` | Absolute pointer move. Header echoes the resulting `(x, y)`. |
| `left_click` | — | Left click at current position. Header echoes the click `(x, y)`. |
| `right_click` | — | Right click at current position. Header echoes the click `(x, y)`. |
| `middle_click` | — | Middle click at current position. Header echoes the click `(x, y)`. |
| `double_click` | — | Double left click at current position. Header echoes the click `(x, y)`. |
| `left_click_drag` | `x`, `y` | Press, drag to `(x, y)` with **interpolated motion** (so drag-and-drop targets recognize the gesture), release. Header echoes the resulting `(x, y)`. |
| `type` | `text` | Type a UTF-8 string via synthetic key events. |
| `key` | `keys` | Press a chord, e.g. `"ctrl+c"`, `"Return"`, `"alt+Tab"`. |
| `scroll` | `x`, `y`, `direction`, `amount` | Scroll `amount` clicks (`up`/`down`/`left`/`right`). |
| `set_clipboard` | `text` | Own the `CLIPBOARD` + `PRIMARY` selections with `text`. |
| `get_clipboard` | — | Read clipboard text; returned as the response payload (UTF-8). |
| `wait` | `ms` | Sleep guest-side to let the UI settle. |
| `exec` | `command` | Launch a shell command (e.g. `"chromium &"`). |

## Live view (watch / drive the desktop)

A running session can be watched — and optionally driven — by a human over
**VNC**, without changing the guest. `desktop_view` asks the daemon to start a
live view and returns a loopback address:

```text
desktop_view { "session_id": "…" }  →  vnc://127.0.0.1:5901
```

Open it with any VNC client — on macOS, `open vnc://127.0.0.1:5901` launches
Screen Sharing; [TigerVNC](https://tigervnc.org/) and other standard viewers
work too.

How it works: the daemon runs a small RFB (VNC) server that reuses the
session's existing capabilities — it captures the screen with the `screenshot`
action and translates the viewer's mouse/keyboard into the same computer-use
actions the agent uses (`mouse_move`, `left_click`, `left_click_drag`,
`scroll`, `type`, `key`, …). So a human and the agent drive the *same* display,
taking turns through the session's request lock (a screenshot never interleaves
with synthetic input). No x11vnc in the guest, no second vsock port — it is a
translation layer in the daemon (`crates/vmette-daemon/src/{rfb,view}.rs`).

Properties:

- **Per-session, per-port.** Each session's view binds its own ephemeral
  loopback port, so several desktops can be watched at once with no collision.
  `desktop_view` is idempotent — repeated calls return the same address.
- **Loopback only.** The listener binds `127.0.0.1`; the view is reachable only
  from the host. It offers **VNC Authentication** (macOS Screen Sharing refuses
  plain `None`), but the challenge response is **not verified** — type any
  password to connect. The access boundary is the loopback bind + the ephemeral
  per-session port, not the password.
- **Pull-based, ~5 fps.** The server sends changed tiles in response to the
  client's update requests (reusing the same tile-diff idea as the settle
  detector). Plenty for watching an agent act in discrete steps; not a video
  feed.
- **Lifecycle.** The view is torn down with the session (`desktop_stop`, idle
  eviction, or daemon shutdown).

## Constraints

- **Software-rendered Xvfb, no GPU.** Fine for agentic GUI control and UI
  testing; not for video / WebGL / 3D.
- **Slower boot than headless** — several seconds for the desktop image + Xvfb
  + WM + first app, versus ~1 s for a headless one-shot.
- **Memory:** each session is a live VM holding a browser; budget 1–2 GB RAM
  and ≥2 vCPUs per session. The daemon caps concurrent sessions.
- **Idle eviction:** sessions untouched for 30 minutes are force-stopped.
- **Arch:** the desktop image and agent must match vmette's guest assets
  (`aarch64` on Apple Silicon, `x86_64` on Intel).
- **Live view is loopback-only and ~5 fps** (see [Live view](#live-view-watch--drive-the-desktop)):
  enough to watch and drive the agent, not a video feed.
