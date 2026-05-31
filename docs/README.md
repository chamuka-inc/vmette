# vmette documentation

Reference docs for each surface of vmette. Start with the
[project README](../README.md) for the overview and install instructions.

| Doc | Covers |
|-----|--------|
| [CLI.md](CLI.md) | The `vmette` command-line interface — flags, `--rootfs` specs, examples. |
| [API.md](API.md) | The Rust library (`vmette` crate) and the C ABI (`vmette.h`). |
| [DAEMON.md](DAEMON.md) | `vmetted` — the long-lived UNIX-socket dispatcher and its wire protocol. |
| [MCP.md](MCP.md) | `vmette-mcp` — the Model Context Protocol server for AI agents. |
| [DESKTOP.md](DESKTOP.md) | Software-rendered GUI desktop sessions for computer-use agents. |
| [HACKING.md](HACKING.md) | Building from source, the workspace layout, and debugging. |

## Workspace at a glance

vmette is a Cargo workspace of ten crates (full layout in [HACKING.md](HACKING.md)):
the wire contracts live in `vmette-proto`, the VZ wrapper and public API in
`vmette`, and the rootfs providers in the `vmette-provider-*` crates, aggregated
by `vmette-providers`. The `vmette`, `vmetted`, and `vmette-mcp` binaries come
from `vmette-cli`, `vmette-daemon`, and `vmette-mcp` respectively.
