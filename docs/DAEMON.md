# vmetted — long-lived UNIX-socket dispatcher

*Embed it.* This is the wire protocol for driving many sandboxed runs (or
persistent desktop sessions) from a long-lived caller — the daemon behind the
MCP server, and a surface you can build your own agent host on. To run an agent
directly, use the [MCP server](MCP.md); for one-off commands, the [CLI](CLI.md).

`vmetted` listens on a UNIX socket and serves two request families over
the same protocol:

- **Stateless runs** — one guest run per connection, booted **in-process**
  via a capture-aware `vmette::Session` (the schema below). A warm-snapshot
  pool to replace the per-request cold boot is a planned future optimization
  (Apple Silicon); it is not yet implemented.
- **Stateful desktop sessions** — `desktop_*` requests that drive a
  persistent in-process VM held across connections (see
  [Desktop session requests](#desktop-session-requests)).

## Run

```sh
vmetted                                      # default socket
vmetted --socket /tmp/vmette.sock            # override path
vmetted --version                            # print version (also -V)
vmetted --help                               # usage (also -h)
```

Default socket: `$HOME/Library/Caches/vmette/vmette.sock`.

Logs are structured JSON on stderr (tracing-subscriber). Filter with
`RUST_LOG`:

```sh
RUST_LOG=vmetted=debug vmetted
```

`SIGTERM` / `SIGINT` stops accepting new connections, tears down any live
desktop sessions, and removes the socket file before exit. (In-flight stateless
runs are not gracefully drained — they are dropped on exit.)

## Protocol

Line-delimited JSON. One request per connection.

### Request

Only `kernel`, `initramfs`, `rootfs`, and `exec` are required:

```json
{
  "kernel": "/abs/path/vmlinuz-virt",
  "initramfs": "/abs/path/initramfs-vmette",
  "rootfs": "/abs/path/alpine-rootfs",
  "exec": "echo hi; exit 17"
}
```

Every other field is optional and shown below with its default; omit any you
don't need (the daemon owns the one true default — see the prose after):

```json
{
  "rootfs_ro": false,
  "offline": false,
  "shares": [
    { "tag": "host", "path": "/abs/path/host_dir" }
  ],
  "disks": [ "/abs/path/disk.img" ],
  "net": false,
  "switch_root": false,
  "vsock_port": 0,
  "guest_vsock_port": 1025,
  "timeout_seconds": null,
  "vcpus": 1,
  "mem_mib": 512,
  "scratch_mib": null
}
```

`rootfs` is required and follows the same spec format as the CLI's
`--rootfs` flag — a path (`/abs/path` or `./rel`), a bare image ref
(`alpine:3.20`), or a scheme-prefixed URL (`oci://…`, `tar+https://…`,
`tar+file://…`, `squashfs+file://…`). See
[`CLI.md`](CLI.md#rootfs-providers) for the shipped providers.

`vsock_port` is tri-state: `-1` (disable) / `0` (auto) / `>0` (fixed).
`scratch_mib` (MiB) attaches an ephemeral ext4 scratch disk as the writable
overlay upper (the CLI's `--scratch`); omit or `null` for the RAM-backed tmpfs
overlay.

The daemon run schema has no `env` field — the CLI's `--env KEY=VALUE`
(and `Config.env`) is not yet wired through `vmetted`. A daemon client
that needs guest env vars must bake them into the `exec` command itself
(e.g. `exec: "FOO=bar mycmd"`).

### Response stream

Newline-delimited JSON frames. The run lane emits three kinds
(`stdout`/`exit`/`error`); a fourth, `stderr`, exists in the protocol but is
unused by this lane:

```json
{"kind":"stdout","data":"hello world\n"}
{"kind":"exit","code":17}
{"kind":"error","message":"…"}
```

The in-process run lane captures the guest's combined output on one clean
console and emits it as a stream of `stdout` frames, terminated by a single
`exit` (or `error` on a daemon-side failure). Guest stderr is folded into
`stdout`; the `stderr` frame remains in the protocol for compatibility but
is **not** emitted for guest stderr by this lane.

### Client examples

#### Python

```python
import socket, json, os

sock = os.path.expanduser("~/Library/Caches/vmette/vmette.sock")
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock)

req = {
    "kernel": "/abs/path/vmlinuz-virt",
    "initramfs": "/abs/path/initramfs-vmette",
    "rootfs": "/abs/path/alpine-rootfs",
    "exec": "echo from daemon; exit 17",
}
s.sendall((json.dumps(req) + "\n").encode())
s.shutdown(socket.SHUT_WR)

buf = b""
while True:
    chunk = s.recv(4096)
    if not chunk:
        break
    buf += chunk

for line in buf.decode().splitlines():
    frame = json.loads(line)
    if frame["kind"] == "exit":
        raise SystemExit(frame["code"])
    print(frame["kind"], frame["data"], end="")
```

#### shell + jq + socat

```sh
echo '{"kernel":"/k","initramfs":"/i","rootfs":"/p","exec":"true"}' | \
  socat - UNIX-CONNECT:$HOME/Library/Caches/vmette/vmette.sock | \
  jq -r 'select(.kind == "exit") | "exit \(.code)"'
```

## Desktop session requests

Beyond the stateless run protocol, the daemon serves a **stateful**
computer-use path on the same socket: a `desktop_start` boots a persistent
graphical VM that the daemon holds in-process across connections, and
later requests act against it by `session_id`. The registry caps concurrent
sessions at **8**, force-stops any session idle for **30 minutes**, runs the
eviction sweep every **60 seconds**, and tears every session down on shutdown.
See [`DESKTOP.md`](DESKTOP.md) for the session model, the `Action` vocabulary,
and the image build.

Each request is still one JSON object per connection, tagged by `kind`:

| `kind` | Key fields | Reply |
|--------|-----------|-------|
| `desktop_start` | `kernel`, `initramfs`, `image` (resolved client-side; required), `size?` (`"WxH"`; omitted → 1280x800), `net?`, `offline?`, `shares?` (`[{tag,path}]`, mounted at `/mnt/<tag>`), `vcpus?`, `mem_mib?` | `{"kind":"session","session_id":"…"}` |
| `desktop_action` | `session_id`, `action` (a `vmette::Action`, e.g. `{"action":"screenshot"}`, mouse/key/type/scroll, `exec_capture`, `get_clipboard`) | `{"kind":"action_result","ok":true,"error?":"…","x?":…,"y?":…,"png_base64?":"…","text?":"…","exit_code?":…}`. `text?` carries the clipboard (`get_clipboard`) or combined stdout/stderr (`exec_capture`); `exit_code?` carries the `exec_capture` status (absent if it didn't exit cleanly, e.g. a timeout). See [`DESKTOP.md`](DESKTOP.md). |
| `desktop_screenshot_settled` | `session_id`, `timeout_ms?` (default 10000), `stable_hold_ms?` (confirmation hold; daemon default ~500 ms) | `{"kind":"settled","settled":bool,"moving":[…],"png_base64":"…"}` |
| `desktop_what_changed` | `session_id` | `{"kind":"changed","changed?":{"x":…,"y":…,"w":…,"h":…},"png_base64":"…"}` (`changed` absent when nothing moved; `png_base64` is just the cropped changed region — the full frame only when nothing changed) |
| `desktop_view` | `session_id` | `{"kind":"view","addr":"127.0.0.1:PORT"}` — opens (or returns) a live VNC view on a per-session loopback port; idempotent. See [`DESKTOP.md`](DESKTOP.md#live-view-watch--drive-the-desktop). |
| `desktop_stop` | `session_id` | `{"kind":"stopped"}` |

A daemon-side failure on any kind returns `{"kind":"error","message":"…"}`.

## When to use vmetted vs vmette

| Use case | Tool |
|----------|------|
| One-off invocation from a shell | `vmette` |
| Many short-lived invocations from a long-lived process | `vmetted` |
| Persistent desktop / computer-use sessions | `vmetted` (`desktop_*`) |
| Library embedding from Rust/C | link `libvmette` directly |
