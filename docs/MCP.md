# vmette-mcp — Model Context Protocol server

`vmette-mcp` lets you run your agent on your Mac without the anxiety. Any
MCP-aware agent host (Claude Code, Claude Desktop, Cursor, Cline, Zed, Goose,
custom clients) gets a set of tools whose every effect lands inside a Linux
microVM — never on your host. The agent gets its own real shell, filesystem,
and (optionally) network to work in; that environment is *not* your machine, so
it cannot touch your real filesystem unless you explicitly share a directory
into it, nor reach the network unless you start the server with
`--allow-network`. Most tools boot a fresh VM per call; the `desktop_*` family
drives a persistent graphical desktop session.

Each tool call boots a fresh kernel via Apple's `Virtualization.framework`
(~1 second), runs the agent's request, and tears down the VM on return — so the
isolation boundary is the hypervisor, not a container or a `chroot`. The MCP
server itself is long-lived — it dies when the client closes its stdio
connection.

## Install

`vmette-mcp` ships in the same release as `vmette` and `vmetted`. After
running the install script you'll have `vmette-mcp` on your `PATH`:

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
vmette-mcp --help
```

Building from source:

```sh
cargo build --release -p vmette-mcp
ls target/release/vmette-mcp
```

## Client configuration

Every client launches the same `vmette-mcp` binary over stdio. First install it
so it's on your `PATH`:

```sh
curl -fsSL https://github.com/chamuka-inc/vmette/releases/latest/download/install.sh | bash
vmette-mcp --help          # confirm it's on PATH
```

The kernel + initramfs ship with the install and are auto-discovered — no asset
flags needed. Common flags (all optional):

- `--allow-network` — permit guest egress; omit for default-deny (then
  `network: true` calls are refused, not silently run offline).
- `--default-image <ref>` — image used by `execute` / `workspace_*` when a call
  doesn't name one (e.g. `python:3.12-alpine`).
- `--workspace-cap <n>` — max concurrent live workspaces (default 8).

### Claude Code (CLI)

One command — no JSON to edit:

```sh
claude mcp add vmette --scope user -- vmette-mcp --allow-network   # all projects
claude mcp add vmette -- vmette-mcp --allow-network                # this project only
```

Verify with `claude mcp list` (look for `vmette … ✓ Connected`) or `/mcp` inside
a session. Remove with `claude mcp remove vmette`. Everything after `--` is the
launch command, so add flags there: `… -- vmette-mcp --default-image python:3.12-alpine --allow-network`.

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`:

```jsonc
{
  "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": [
        "--default-image", "python:3.12-alpine",
        "--allow-network",
        "--workspace-cap", "8"
      ]
    }
  }
}
```

Restart Claude Desktop. The vmette tools appear under "vmette" in the
tool picker. Drop `--allow-network` if you don't want the agent to make
any outbound HTTP calls.

### Cursor

`.cursor/mcp.json` in any project, or `~/.cursor/mcp.json` globally:

```jsonc
{ "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": ["--default-image", "alpine:3.20", "--allow-network"]
}}}
```

### Cline (VS Code)

`Cline > Settings > MCP Servers > Configure`, add under `mcpServers`:

```jsonc
{ "mcpServers": {
    "vmette": {
      "command": "vmette-mcp",
      "args": ["--allow-network"]
}}}
```

### Zed

`~/.config/zed/settings.json` → `context_servers`:

```jsonc
{ "context_servers": {
    "vmette": {
      "source": "custom",
      "command": "vmette-mcp",
      "args": ["--allow-network"]
}}}
```

### Goose

Add an stdio extension to `~/.config/goose/config.yaml` (or run
`goose configure` → *Add Extension* → *Command-line (stdio)*):

```yaml
extensions:
  vmette:
    type: stdio
    cmd: vmette-mcp
    args: ["--allow-network"]
    enabled: true
```

### Any other MCP host

Anything that supports stdio-launched MCP servers will work. Pass the
binary path as `command` and the flags as `args`.

## CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--default-image REF` | `alpine:3.20` | Rootfs used when `execute` or `workspace_create` doesn't specify one. |
| `--allow-network` | off | Permits tool calls with `network=true`. Without it, `fetch_url` always fails and any `execute`/`workspace_create` call requesting `network=true` is rejected with an error (the field is not silently ignored). |
| `--workspace-cap N` | `8` | Maximum concurrent workspaces per MCP session. Prevents an agent from spamming `workspace_create` and exhausting disk. |
| `--kernel PATH` | autodiscovered | Override vmlinuz path. Default: `vmlinuz-virt` discovered from `$VMETTE_ASSETS_DIR`, `./assets`, or `<install-prefix>/assets` (the same search the `vmette` CLI uses). |
| `--initramfs PATH` | autodiscovered | Override initramfs path. Default: `initramfs-vmette` discovered from the same locations as `--kernel`. |
| `--vmette PATH` | autodiscovered | Override `vmette` binary path. Default: `$VMETTE_BIN`, sibling-of-this-binary, then `$PATH` lookup. |
| `--socket PATH` | `~/Library/Caches/vmette/vmette.sock` | vmetted socket for the `desktop_*` tools. The daemon is started automatically on first desktop use if it isn't already running. |

`vmette-mcp` writes structured logs (tracing) to **stderr**. `stdout`
is reserved for MCP frames; anything written there desyncs the client.
Filter with `RUST_LOG`:

```sh
RUST_LOG=vmette_mcp=debug vmette-mcp --allow-network
```

## Tools

### `execute`

One-shot code execution. Each call boots a fresh microVM; no state
persists between calls.

| Input | Type | Notes |
|-------|------|-------|
| `language` | string | `python`, `node`, or `shell`. Maps to `python:3.12-alpine`, `node:20-alpine`, `alpine:3.20`. |
| `code` | string | Source — quoting is handled, embedded `'`, `$`, backticks all safe. |
| `network` | bool, default false | Requires `--allow-network` server-side. |
| `timeout` | int, default 30 | Seconds. Exceeded → guest force-stopped, exit 124. |

Returns: `exit: N\n\nstdout:\n...\n\nstderr:\n...`

### `fetch_url`

HTTP(S) GET via a Python urllib script inside a microVM. Requires
`--allow-network`.

| Input | Type | Notes |
|-------|------|-------|
| `url` | string | Only http/https. Redirects followed by urllib. |
| `max_bytes` | int, default 20000 | Body cap for context-window control. |

Returns: `exit: 0\n\nstdout:\n{"status": 200, "body": "..."}\n`

### `workspace_create`

Allocate a per-task scratch directory on the host. The server creates
it under `$TMPDIR/vmette-mcp-<pid>/<uuid>/` and tracks it for the
session lifetime.

| Input | Type | Notes |
|-------|------|-------|
| `image` | string, default `--default-image` | OCI ref used by subsequent `workspace_run` calls. |
| `network` | bool, default false | Network policy for `workspace_run` calls (requires `--allow-network`). |

Returns (structured): `{"workspace_id": "...", "image": "..."}`. The host
path is deliberately **not** returned — the agent operates on the workspace
only through `workspace_id`, never a host filesystem path.

### `workspace_write`

Write a file into a workspace from the host. The agent never sees the
file path — only the relative path inside the workspace.

| Input | Type | Notes |
|-------|------|-------|
| `workspace_id` | string | From `workspace_create`. |
| `path` | string | Relative path. `..` and absolute paths rejected. Symlinks refused (defense against agent-created symlinks via `workspace_run`). |
| `content` | string | Overwrites if existing. Opened with `O_NOFOLLOW`. |

### `workspace_read`

Read a file from a workspace. Same path safety as `workspace_write`.

### `workspace_run`

Run a shell command inside the workspace's microVM. The workspace
directory is mounted **read-write** at `/mnt/work` and is the initial
`cwd`. Image and network policy were fixed at `workspace_create` time.

| Input | Type | Notes |
|-------|------|-------|
| `workspace_id` | string | From `workspace_create`. |
| `command` | string | Shell command. Runs as `sh -c "cd /mnt/work && $command"`. |
| `timeout` | int, default 60 | Seconds. |

### `workspace_destroy`

Remove the workspace's on-disk directory and forget the ID. Idempotent.

### desktop computer-use tools

A separate family that drives a **persistent** graphical desktop session
(Xvfb + window manager) instead of a one-shot VM. These route through
`vmetted` (the session must outlive a single tool call); the MCP server
launches the daemon automatically on first desktop use if it isn't already
running. `desktop_start` returns a `session_id` to pass to the rest;
`desktop_screenshot` returns a PNG **image content block** for the agent to
look at. Full reference, protocol, and image build in
[`DESKTOP.md`](DESKTOP.md).

| Tool | Input | Returns |
|------|-------|---------|
| `desktop_start` | `image?`, `size?`, `network?` | session id |
| `desktop_view` | `session_id` | `vnc://host:port` — open a live VNC view a human can watch and drive (see [DESKTOP.md](DESKTOP.md#live-view-watch--drive-the-desktop)) |
| `desktop_screenshot` | `session_id` | PNG image block |
| `desktop_screenshot_when_settled` | `session_id`, `timeout_ms?` | note + PNG, once the screen has stopped changing and stayed still |
| `desktop_what_changed` | `session_id` | note + PNG of the region changed since the last capture |
| `desktop_cursor_position` | `session_id` | `"x y"` |
| `desktop_move` / `desktop_click` / `desktop_double_click` / `desktop_right_click` / `desktop_middle_click` | `session_id`, `x`, `y` | status |
| `desktop_drag` | `session_id`, `x`, `y` | status — press-move-release from the current pointer to `(x, y)`: text selection, sliders, drag-and-drop, drawing |
| `desktop_type` | `session_id`, `text` | status |
| `desktop_key` | `session_id`, `keys` (e.g. `ctrl+c`) | status |
| `desktop_get_clipboard` | `session_id` | the clipboard text (exact; empty if unset) — read text out of a GUI app without OCR |
| `desktop_set_clipboard` | `session_id`, `text` | status — put `text` on the clipboard (CLIPBOARD + PRIMARY) |
| `desktop_paste` | `session_id`, `text` | status — set the clipboard then Ctrl+V; fast, lossless input vs `desktop_type` |
| `desktop_scroll` | `session_id`, `x`, `y`, `direction`, `amount` | status |
| `desktop_exec` | `session_id`, `command` (e.g. `xterm &`) | status |
| `desktop_launch` | `session_id`, `command`, `wait_ms?` | note + PNG of the app's first settled frame |
| `desktop_stop` | `session_id` | status |

`desktop_launch` is the one-call "start an app and see it" tool: it backgrounds
the command, waits for the window to paint, then for the screen to **settle and
stay settled**, and returns that frame. Prefer it over `desktop_exec` + manual
`desktop_screenshot` polling. The settle is held briefly so a network-bound app
(a browser painting its chrome, then fetching its page) returns the *loaded*
frame rather than a blank mid-load one — the same hold backs
`desktop_screenshot_when_settled`.

`desktop_drag` presses at the **current** pointer position and releases at
`(x, y)`, so call `desktop_move` first to set the start of the drag — the
target you pass is only where the drag ends.

### Computer-use tips / limitations

The desktop tools drive a real X session, so a few things behave the way a
physical mouse and keyboard would. Knowing these up front avoids the usual
surprises:

- **Coordinates are absolute desktop pixels.** `(0, 0)` is the top-left of the
  whole display, not of any window. Clicking inside a maximized browser means
  the page viewport starts *below* the browser chrome (roughly the toolbar
  height), so a coordinate that looks right in the page is off by that offset.
  Take a `desktop_screenshot` and calibrate against what's actually on screen.
- **Typing goes to the focused widget, and success is not delivery.**
  `desktop_type` / `desktop_key` deliver keystrokes to whatever currently has
  focus — click *inside* the target window first (an absolute coordinate that
  lands outside the window will focus the root and silently drop the input).
  Their `ok` status only means the X server accepted the synthetic event, **not**
  that a focused widget received it — X/XTEST exposes no delivery signal. Always
  confirm the effect with a follow-up `desktop_screenshot` (e.g. that your text
  actually appeared at the prompt) rather than trusting the `ok`.
- **Typing is one synthetic keystroke at a time.** Fine for form fields and
  shell commands; slow for very large blobs. To put a big file into the guest,
  write it with `desktop_exec` (e.g. a here-doc) rather than typing it.
- **`desktop_exec` is fire-and-forget.** It backgrounds a command and returns
  immediately — it does **not** capture stdout/stderr or report an exit code, so
  you cannot use it to verify a result. To check a command's output inside a
  desktop session, redirect it to a guest file (`mycmd > /tmp/out 2>&1`) and
  surface that file visually (e.g. `desktop_launch "xterm -hold -e cat /tmp/out"`),
  or run the command on the one-shot path (`execute` / `workspace_run`), which
  *does* return exit code + stdout + stderr.
- **Settle ignores sub-tile pixel noise.** `desktop_what_changed` and the
  settle logic compare in tiles, so a tiny visual change — a single counter
  digit ticking, a small checkmark appearing — can read as "nothing changed."
  Confirm such fine-grained results another way (a screenshot you inspect, or
  reading state via `desktop_exec`).

## Security model

The boundary is the microVM: the agent is meant to be fully in control *inside*
it and unable to reach the host *outside* it. Everything below is default-deny —
the agent is granted host filesystem access and network only where you say so.

What the server **does** isolate:

- The agent cannot read or write outside a workspace it created. The
  workspace dir is under `$TMPDIR/vmette-mcp-<pid>/` and is removed
  when the MCP session ends gracefully. Ungraceful exits (SIGKILL,
  panic-abort) leave the dir on disk; the next `vmette-mcp` startup
  reaps orphans whose owning PID is dead (and the dir is at least 60s
  old, a grace window against racing a just-started peer and against PID
  reuse) or whose mtime is older than 24 hours, so disk pressure doesn't
  accumulate indefinitely.
- `workspace_write` and `workspace_read` walk the path with `openat(...,
  O_DIRECTORY | O_NOFOLLOW)` at every component, and `mkdirat` for
  any missing intermediate (mkdirat fails atomically if the name
  already exists as a symlink). This closes the nested-path TOCTOU
  where an agent creates `ws/a → /etc` via `workspace_run` and then
  asks `workspace_write` for `a/b/c`.
- The microVM has no network unless `--allow-network` is set AND the
  caller passed `network=true`.
- Path traversal (`..`, absolute paths, rooted prefixes) is rejected
  at the API boundary, not just at the filesystem.
- `fetch_url` parses the URL up front and rejects anything other than
  `http` and `https` schemes. `file://`, `ftp://`, `data://`,
  `gopher://`, etc. are refused. Caps the returned body to
  `max_bytes` (default 20 000).
- All tool output (stdout + stderr from the guest, plus vmette's
  banner on stderr) is bounded at 1 MiB per stream. A runaway guest
  is truncated with a clear marker; the long-lived MCP server cannot
  be OOMed by adversarial guest output.

What the server **does not** isolate:

- `workspace_run` is real Linux. The agent has a full shell inside the
  microVM and can do whatever the rootfs's binaries support — install
  packages (if `--allow-network`), fork bombs (capped by 1 vCPU + 512
  MiB), `rm -rf /` (only deletes the OCI cache tree, refetched next
  call). This is the intended threat model: the agent is fully in
  control inside the VM; the VM cannot reach your host.
- The server process itself runs with your user's permissions. If you
  don't trust the `vmette-mcp` binary, don't install it. (You're
  trusting `vmette` too — same story.)

## Limitations

- **macOS only.** vmette wraps Apple's VZ framework; there's no
  cross-platform port.
- **One workspace = one rootfs.** You can't change the image of a
  workspace after `workspace_create`. Workaround: destroy + recreate.
- **Workspace state lives on the host.** A workspace survives across
  `workspace_run` calls because the host directory persists — but the
  microVM itself is fresh each time. Anything installed via
  `apk add` / `pip install` in one `workspace_run` is **not** present
  in the next call.
- **OCI image-pull TTL is 1 hour.** A tag like `:latest` may refetch
  the manifest on first call past the TTL; use `--offline` (via the
  `vmette` CLI directly) to pin.
- **No streaming output.** Each tool call returns once the guest exits.
  Long-running tasks should respect `timeout` and write progress to
  the workspace dir for `workspace_read` polling.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| Server fails to start: `vmette binary not found` | `vmette` not on `PATH`. Pass `--vmette /path/to/vmette` or symlink it. |
| Server fails: `kernel not found` | Assets not installed. Run `install.sh` or build them: `bash scripts/fetch-assets.sh && bash scripts/build-initramfs.sh`. |
| Every tool call returns exit 1 with `start failed` | Codesigning lost. Re-run `codesign --sign - --force --entitlements entitlements.plist --options=runtime $(which vmette)`. |
| `fetch_url` returns "this MCP server was started without --allow-network" | Add `--allow-network` to your client config and restart the host. |
| `workspace_create` returns "workspace cap reached" | Destroy idle workspaces or raise `--workspace-cap`. |
| `desktop_*` tools fail with "connect … failed (is vmetted running?)" | Start the daemon (`vmetted &`); the desktop tools route through it. |
| `desktop_start` returns "session cap reached" | Stop an idle desktop session, or wait for idle eviction (30 min). |
| `cargo`/`node`/etc. "not found" in a toolchain image | The image's configured `Env` (incl. `PATH`) is applied automatically. If the image was extracted by an older vmette it lacks the env file — clear its cache (`rm -rf ~/Library/Caches/vmette/oci/<image>`) so it re-extracts. |
| `No space left on device` mid-build | The guest's writable `/` is a RAM-backed tmpfs overlay (~half the guest RAM). Route large writes to the workspace mount (`/mnt/work`) — e.g. `CARGO_HOME` and the build target dir — rather than the rootfs. |
| Linker error: `cannot find …rcgu.o` during a native compile | Heavy parallel codegen writing many object files to the virtio-fs share can race. Build with a single codegen unit: `RUSTFLAGS="-C codegen-units=1"` (or the equivalent for your toolchain). |
