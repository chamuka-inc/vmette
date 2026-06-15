# vmette documentation

Reference docs for each surface of vmette. See the
[project README](../README.md) for the overview and install instructions.

| Doc | Covers |
|-----|--------|
| [MCP.md](MCP.md) | `vmette-mcp` — the Model Context Protocol server. Give Claude Code/Cursor a sandboxed machine. **Start here.** |
| [DESKTOP.md](DESKTOP.md) | Persistent GUI desktop sessions for computer-use agents. |
| [CLI.md](CLI.md) | The `vmette` command — run a one-off command in a sandbox; flags, `--rootfs` specs, examples. |
| [API.md](API.md) | *Embed it:* the Rust library (`vmette` crate) and the C ABI (`vmette.h`). |
| [DAEMON.md](DAEMON.md) | *Embed it:* `vmetted` — the long-lived UNIX-socket dispatcher (stateless runs plus persistent desktop sessions with a live VNC view) and its wire protocol. |
| [HACKING.md](HACKING.md) | Building from source, the workspace layout, and debugging. |

## Workspace at a glance

vmette is a Cargo workspace of crates (full layout in [HACKING.md](HACKING.md)):
the wire contracts live in `vmette-proto`, the VZ wrapper and public API in
`vmette`, and the rootfs providers in the `vmette-provider-*` crates, aggregated
by `vmette-providers`. The `vmette`, `vmetted`, and `vmette-mcp` binaries come
from `vmette-cli`, `vmette-daemon`, and `vmette-mcp` respectively, with
`vmette-daemon-client` shared between the CLI and MCP server for talking to
`vmetted`.
