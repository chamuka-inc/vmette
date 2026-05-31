# vmette-mcp — Model Context Protocol server

`vmette-mcp` is the security boundary between an untrusted agent and the
machine it runs on. Any MCP-aware agent host (Claude Desktop, Cursor, Cline,
Zed, Goose, custom clients) gets a set of tools whose every effect lands
inside a Linux microVM — never on your host. The agent gets a real shell,
filesystem, and (optionally) network to work with; that environment is *not*
your machine, and it cannot reach your real filesystem unless you explicitly
share a directory into it, nor the network unless you start the server with
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

`Cline > Settings > MCP Servers`, add:

```jsonc
{ "vmette": {
    "command": "vmette-mcp",
    "args": ["--allow-network"]
}}
```

### Any other MCP host

Anything that supports stdio-launched MCP servers will work. Pass the
binary path as `command` and the flags as `args`.

## CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--default-image REF` | `alpine:3.20` | Rootfs used when `execute` or `workspace_create` doesn't specify one. |
| `--allow-network` | off | Permits tool calls with `network=true`. Without it, `fetch_url` always fails and `execute`/`workspace_run` ignore the network field. |
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

Returns (structured): `{"workspace_id": "...", "image": "...", "host_path": "..."}`

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
| `desktop_screenshot` | `session_id` | PNG image block |
| `desktop_screenshot_when_settled` | `session_id`, `timeout_ms?` | note + PNG, once the screen stops changing |
| `desktop_what_changed` | `session_id` | note + PNG of the region changed since the last capture |
| `desktop_cursor_position` | `session_id` | `"x y"` |
| `desktop_move` / `desktop_click` / `desktop_double_click` / `desktop_right_click` | `session_id`, `x`, `y` | status |
| `desktop_type` | `session_id`, `text` | status |
| `desktop_key` | `session_id`, `keys` (e.g. `ctrl+c`) | status |
| `desktop_scroll` | `session_id`, `x`, `y`, `direction`, `amount` | status |
| `desktop_exec` | `session_id`, `command` (e.g. `xterm &`) | status |
| `desktop_stop` | `session_id` | status |

## Security model

The boundary is the microVM: the agent is meant to be fully in control *inside*
it and unable to reach the host *outside* it. Everything below is default-deny —
the agent is granted host filesystem access and network only where you say so.

What the server **does** isolate:

- The agent cannot read or write outside a workspace it created. The
  workspace dir is under `$TMPDIR/vmette-mcp-<pid>/` and is removed
  when the MCP session ends gracefully. Ungraceful exits (SIGKILL,
  panic-abort) leave the dir on disk; the next `vmette-mcp` startup
  reaps orphans whose owning PID is dead or whose mtime is older than
  24 hours, so disk pressure doesn't accumulate indefinitely.
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
