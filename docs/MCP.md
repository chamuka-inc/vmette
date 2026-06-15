# vmette-mcp — Model Context Protocol server

`vmette-mcp` gives an agent a hardware-isolated sandbox to run untrusted work in.
Any MCP-aware agent host (Claude Code, Claude Desktop, Cursor, Cline, Zed, Goose,
custom clients) gets a set of tools — `execute`, `fetch_url`, `workspace_*`,
`desktop_*` — whose every effect lands inside a Linux microVM on Apple's
`Virtualization.framework`, never on your host, with all host access default-deny
(see [Security model](#security-model)). Most tools boot a fresh VM per call; the
`desktop_*` family drives a persistent graphical desktop session.

Adding the sandbox doesn't replace the host's own tools — in Claude Code, native
Bash / Read / Write still run on your Mac, and the model chooses which tool to
call. To make the VM the agent's **only** way to execute code, restrict the host
tools too — e.g. disable Claude Code's Bash tool via permissions, or use a host
that exposes only vmette. The MCP server is long-lived — it dies when the client
closes its stdio connection.

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

Every client launches the same `vmette-mcp` binary over stdio. Install it first
(see [Install](#install) above) so it's on your `PATH`.

The kernel + initramfs ship with the install and are auto-discovered — no asset
flags needed. Common flags (all optional):

- `--allow-network` — permit guest egress (see the [CLI flags](#cli-flags)
  table for the full rule).
- `--default-image <ref>` — image used by `workspace_create` (and thus
  `workspace_run`) when a call doesn't name one (default `alpine:3.20`).
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
| `--default-image REF` | `alpine:3.20` | Rootfs used when `workspace_create` doesn't name one. Does not affect `execute` (see its `language` row). |
| `--allow-network` | off | Permits tool calls with `network=true` (the project's only default-deny network/filesystem gate — see also [Security model](#security-model)). Without it, `fetch_url` always fails and any `execute`/`workspace_create` call requesting `network=true` is rejected with an error (the field is not silently ignored). |
| `--workspace-cap N` | `8` | Maximum concurrent open workspaces (per server process). Prevents an agent from spamming `workspace_create` and exhausting disk. |
| `--kernel PATH` | autodiscovered | Override vmlinuz path. Default: `vmlinuz-virt` discovered from `$VMETTE_ASSETS_DIR`, `./assets`, or `<install-prefix>/assets` (the same search the `vmette` CLI uses). |
| `--initramfs PATH` | autodiscovered | Override initramfs path. Default: `initramfs-vmette` discovered from the same locations as `--kernel`. |
| `--socket PATH` | `~/Library/Caches/vmette/vmette.sock` | vmetted socket for the `desktop_*` tools. |
| `--ca-certs DIR` | explicit flag > `$VMETTE_CA_CERTS` > `~/.config/vmette/certs` | Host directory of `.crt`/`.pem`/`.cer` CA certificates trusted inside **every** guest (`execute`, `fetch_url`, `workspace_run`, and the `desktop_*` default), so HTTPS works behind a TLS-inspecting proxy / enterprise CA. Opt-in: nothing is mounted when unset and the default dir is absent. On macOS, `scripts/export-macos-ca-certs.sh` stages the keychain roots there. See [HACKING.md](HACKING.md#trusting-a-host-ca-in-every-guest). |

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
| `language` | string | `python`, `node`, or `shell`. Maps to `python:3.12-alpine`, `node:20-alpine`, `alpine:3.20`. This mapping is fixed; `--default-image` does **not** apply (see the [CLI flags](#cli-flags) table). |
| `code` | string | Source — quoting is handled, embedded `'`, `$`, backticks all safe. |
| `network` | bool, default false | Requires `--allow-network` (see [flags table](#cli-flags)). |
| `timeout` | int, default 30 | Seconds. Exceeded → guest force-stopped, exit 124. |
| `scratch_mib` | int, optional | Ephemeral ext4 scratch disk size in MiB backing the writable root + `/tmp`. Set this when a build/extract would exceed the RAM-backed overlay (`No space left on device`); created sparse per call, discarded when the call returns. Omit for light work. |

Returns: `exit: N\n\nstdout:\n...` — the guest's stdout and stderr arrive folded into the one captured stream, so a separate `stderr:` block does not appear (the in-process lane always leaves stderr empty).

### `fetch_url`

HTTP(S) GET via a Python urllib script inside a microVM. Always runs in
`python:3.12-alpine`. Requires `--allow-network` (see [flags table](#cli-flags)).

| Input | Type | Notes |
|-------|------|-------|
| `url` | string | Only http/https. Redirects followed by urllib. |
| `max_bytes` | int, default 20000 | Body cap for context-window control. |

Returns: `exit: 0\n\nstdout:\n{"status": 200, "body": "..."}\n`

### `workspace_create`

Allocate a per-task scratch directory on the host. The server creates
it under `$TMPDIR/vmette-mcp-<pid>-<nonce>/<uuid>/` and tracks it for the
session lifetime.

| Input | Type | Notes |
|-------|------|-------|
| `image` | string, default `--default-image` | OCI ref used by subsequent `workspace_run` calls. |
| `network` | bool, default false | Network policy for `workspace_run` calls (requires `--allow-network`; see [flags table](#cli-flags)). |

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
| `scratch_mib` | int, optional | Ephemeral ext4 scratch disk size in MiB backing the writable root + `/tmp` for this run (not the workspace share, which always persists). Set it when a build would exceed the RAM-backed overlay. |

### `workspace_destroy`

Remove the workspace's on-disk directory and forget the ID. Idempotent.

### desktop computer-use tools

A separate family that drives a **persistent** graphical desktop session
(Xvfb + window manager) instead of a one-shot VM. These route through
`vmetted` (the session must outlive a single tool call); the MCP server
launches the daemon automatically on first desktop use if it isn't already
running. `desktop_start` returns a `session_id` to pass to the rest;
`desktop_screenshot` returns a PNG **image content block** for the agent to
look at, plus a **framebuffer note** stating the pixel dimensions — so the agent
targets clicks in the screenshot's true coordinate space instead of guessing the
scale of a downscaled rendering. Full reference, protocol, and image build in
[`DESKTOP.md`](DESKTOP.md).

| Tool | Input | Returns |
|------|-------|---------|
| `desktop_start` | `image?`, `size?` (default `1280x800`), `network?`, `ca_certs?` (host CA dir mounted at `/mnt/certs`, mirrors `--ca-certs` per-session) | session id |
| `desktop_view` | `session_id` | `vnc://127.0.0.1:port` (loopback) — open a live VNC view a human can watch and drive (see [DESKTOP.md](DESKTOP.md#live-view-watch--drive-the-desktop)) |
| `desktop_screenshot` | `session_id` | framebuffer note (`framebuffer WxH; …`) + PNG image block |
| `desktop_screenshot_when_settled` | `session_id`, `timeout_ms?` (default 10000 ms) | note + framebuffer note + PNG, once the screen has stopped changing and stayed still |
| `desktop_what_changed` | `session_id` | note + framebuffer note + PNG of the region changed since the last capture |
| `desktop_cursor_position` | `session_id` | `"x y"` |
| `desktop_move` / `desktop_click` / `desktop_double_click` / `desktop_right_click` / `desktop_middle_click` | `session_id`, `x`, `y` | status (echoes where the pointer landed; flags `(constrained)` if the WM clamped it) |
| `desktop_drag` | `session_id`, `x`, `y` | status — drag to `(x, y)`; see the prose below for the "move first" requirement |
| `desktop_type` | `session_id`, `text` | status |
| `desktop_key` | `session_id`, `keys` (e.g. `ctrl+c`) | status |
| `desktop_get_clipboard` | `session_id` | the clipboard text (exact; empty if unset) — read text out of a GUI app without OCR |
| `desktop_set_clipboard` | `session_id`, `text` | status — put `text` on the clipboard (CLIPBOARD + PRIMARY) |
| `desktop_paste` | `session_id`, `text` | status — set the clipboard then Ctrl+V; fast, lossless input vs `desktop_type` |
| `desktop_scroll` | `session_id`, `x`, `y`, `direction`, `amount` | status |
| `desktop_exec` | `session_id`, `command` (e.g. `xterm &`) | status — fire-and-forget; no output captured (see [tips](#computer-use-tips--limitations) for `exec` vs `exec_capture`) |
| `desktop_exec_capture` | `session_id`, `command`, `timeout_ms?` (forwarded verbatim; the guest applies a ~15s default, clamped to ~25s) | the command's combined stdout/stderr + exit code — run a short command to completion and read its output |
| `desktop_navigate` | `session_id`, `url` | status — open `url` (an http(s) URL or a local file path) in the browser with no shell and no synthetic keystrokes (deterministic; pair with `desktop_screenshot_when_settled`) |
| `desktop_launch` | `session_id`, `command`, `wait_ms?` (first-paint budget, default 60000 ms) | note + PNG of the app's first settled frame |
| `desktop_stop` | `session_id` | status — errors if the id is unknown (a stopped session is already gone), unlike the idempotent `workspace_destroy` |

`desktop_launch` is the one-call "start an app and see it" tool: it backgrounds
the command, waits for the window to paint, then for the screen to **settle and
stay settled**, and returns that frame. Prefer it over `desktop_exec` + manual
`desktop_screenshot` polling. The settle is held briefly so a network-bound app
(a browser painting its chrome, then fetching its page) returns the *loaded*
frame rather than a blank mid-load one — the same hold backs
`desktop_screenshot_when_settled`.

`desktop_drag` presses at the **current** pointer position and releases at
`(x, y)`, so call `desktop_move` first to set the start of the drag — the
target you pass is only where the drag ends. The drag uses **interpolated
motion** (a stream of intermediate steps plus a dwell over the drop zone), so it
works on drag-and-drop targets that gate on the gesture — reordering lists,
sliders, a pivot-table field layout — not just text selection.

### Computer-use tips / limitations

The desktop tools drive a real X session, so a few things behave the way a
physical mouse and keyboard would. Knowing these up front avoids the usual
surprises:

- **Coordinates are absolute desktop pixels.** `(0, 0)` is the top-left of the
  whole display, not of any window. Clicking inside a maximized browser means
  the page viewport starts *below* the browser chrome (roughly the toolbar
  height), so a coordinate that looks right in the page is off by that offset.
  Take a `desktop_screenshot` and calibrate against what's actually on screen.
- **Focus matters: typing, key chords, and clipboard copy all go to the focused
  widget.** `desktop_type`, `desktop_key`, and `ctrl+a`/`ctrl+c` deliver to
  whatever currently has keyboard focus — `desktop_click` *inside* the target
  window (or document) first. An absolute coordinate that lands outside it
  focuses the root and silently drops the input; right after a page loads, focus
  is usually the toolbar/address bar, not the document, so a copy grabs nothing
  and you read back empty.
- **Success is not delivery.** The `ok` status from `desktop_type` / `desktop_key`
  only means the X server accepted the synthetic event, **not** that a focused
  widget received it — X/XTEST exposes no delivery signal. Always confirm the
  effect with a follow-up `desktop_screenshot` (e.g. that your text actually
  appeared at the prompt) rather than trusting the `ok`.
- **Typing is one synthetic keystroke at a time.** Fine for form fields and
  shell commands; slow for very large blobs. To put a big file into the guest,
  write it with `desktop_exec` (e.g. a here-doc) rather than typing it.
- **`desktop_exec` is fire-and-forget.** It backgrounds a command and returns
  immediately — it does **not** capture stdout/stderr or report an exit code, so
  you cannot use it to verify a result. To read a command's output inside a
  desktop session, use **`desktop_exec_capture`**, which runs a short command to
  completion and returns its combined stdout/stderr plus exit code (the in-guest
  agent is single-threaded, so keep it short — it blocks other desktop actions
  until it returns or times out). For launching a GUI app, stay with
  `desktop_exec` / `desktop_launch`. Commands on the one-shot path (`execute` /
  `workspace_run`) also return exit code + stdout + stderr.
- **Navigate a browser with `desktop_navigate`, not keystrokes.** It hands the
  URL straight to the browser with no shell and no synthetic typing — no omnibox
  focus races, no autocomplete surprises. It returns once navigation starts, so
  follow it with `desktop_screenshot_when_settled` to wait for the page to paint.
  The session must have been started with `network=true`.
- **Copying text out reads the clipboard exactly.** `desktop_get_clipboard`
  reads exactly what `ctrl+c` placed on the clipboard. Focus the document first
  (see "Focus matters" above), *then* `desktop_key 'ctrl+a'`,
  `desktop_key 'ctrl+c'`, and `desktop_get_clipboard`.
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
  workspace dir is under `$TMPDIR/vmette-mcp-<pid>-<nonce>/` and is removed
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
- Captured guest output (stdout and stderr arrive folded into one clean
  stream) is bounded at 1 MiB. A runaway guest is truncated with a clear
  marker (`[output truncated at 1048576 bytes]`); the long-lived MCP server
  cannot be OOMed by adversarial guest output.

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
  the manifest on first call past the TTL. The TTL is owned by the OCI
  provider, not `vmette-mcp` (the server has no `--offline` flag); pin a
  tag by digest, or use the `vmette` CLI's `--offline` directly.
- **No streaming output.** Each tool call returns once the guest exits.
  Long-running tasks should respect `timeout` and write progress to
  the workspace dir for `workspace_read` polling.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| Server fails: `kernel not found` | Assets not installed. Run `install.sh` or build them: `bash scripts/fetch-assets.sh && bash scripts/build-initramfs.sh`. |
| Tool calls fail with an MCP error mentioning `vmette session start` failed | Codesigning lost. The MCP server boots VMs in-process, so **it** must carry the virtualization entitlement: re-run `codesign --sign - --force --entitlements entitlements.plist --options=runtime $(which vmette-mcp)`. |
| `fetch_url` returns "this MCP server was started without --allow-network" | Add `--allow-network` to your client config and restart the host. |
| `workspace_create` returns "workspace cap reached" | Destroy idle workspaces or raise `--workspace-cap`. |
| `desktop_*` tools fail with "connect … failed (is vmetted running?)" | The daemon failed to auto-spawn (it's normally started on first desktop use). Check that `vmetted` is on your `PATH` and codesigned; inspect its stderr by starting it manually (`vmetted &`). |
| `desktop_start` returns "session cap reached" | Stop an idle desktop session, or wait for idle eviction (30 min). |
| `cargo`/`node`/etc. "not found" in a toolchain image | The image's configured `Env` (incl. `PATH`) is applied automatically. If the image was extracted by an older vmette it lacks the env file — clear the provider cache (`rm -rf ~/Library/Caches/vmette/oci`) so it re-extracts (the on-disk dir is `oci/<sanitized-ref>__<digest>/`, not the bare ref). |
| `No space left on device` mid-build | The guest's writable `/` is a RAM-backed tmpfs overlay (~half the guest RAM). Route large writes to the workspace mount (`/mnt/work`) — e.g. `CARGO_HOME` and the build target dir — rather than the rootfs. |
| Linker error: `cannot find …rcgu.o` during a native compile | Heavy parallel codegen writing many object files to the virtio-fs share can race. Build with a single codegen unit: `RUSTFLAGS="-C codegen-units=1"` (or the equivalent for your toolchain). |
