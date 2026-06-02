# vmetted — long-lived UNIX-socket dispatcher

`vmetted` listens on a UNIX socket and serves two request families over
the same protocol:

- **Stateless runs** — one guest run per connection, dispatched by
  spawning the `vmette` CLI as a subprocess (the schema below). A
  warm-snapshot pool to replace the per-request cold boot is a planned
  future optimization (Apple Silicon); it is not yet implemented.
- **Stateful desktop sessions** — `desktop_*` requests that drive a
  persistent in-process VM held across connections (see
  [Desktop session requests](#desktop-session-requests)).

## Run

```sh
vmetted                                      # default socket + vmette path
vmetted --socket /tmp/vmette.sock            # override path
vmetted --vmette /usr/local/bin/vmette       # override CLI binary
```

Default socket: `$HOME/Library/Caches/vmette/vmette.sock`.

Logs are structured JSON on stderr (tracing-subscriber). Filter with
`RUST_LOG`:

```sh
RUST_LOG=vmetted=debug vmetted
```

`SIGTERM` / `SIGINT` drains in-flight connections and removes the
socket file before exit.

## Protocol

Line-delimited JSON. One request per connection.

### Request

```json
{
  "kernel": "/abs/path/vmlinuz-virt",
  "initramfs": "/abs/path/initramfs-vmette",
  "rootfs": "/abs/path/alpine-rootfs",
  "rootfs_ro": false,
  "offline": false,
  "shares": [
    { "tag": "host", "path": "/abs/path/host_dir" }
  ],
  "disks": [ "/abs/path/disk.img" ],
  "exec": "echo hi; exit 17",
  "net": false,
  "switch_root": false,
  "vsock_port": 0,
  "guest_vsock_port": 1025,
  "timeout_seconds": null,
  "vcpus": 1,
  "mem_mib": 512
}
```

`rootfs` is required and follows the same spec format as the CLI's
`--rootfs` flag — a path (`/abs/path` or `./rel`), a bare image ref
(`alpine:3.20`), or a scheme-prefixed URL (`oci://…`, `tar+https://…`,
`tar+file://…`, `squashfs+file://…`). See
[`CLI.md`](CLI.md#rootfs-providers) for the shipped providers.

`rootfs_ro`, `offline`, `shares`, `disks`, `timeout_seconds`, `net`,
`switch_root` are optional. `vsock_port` is `-1` (disable) / `0`
(auto) / `>0` (fixed), defaulting to `0`. `vcpus` defaults to 1,
`mem_mib` to 512.

The daemon run schema has no `env` field — the CLI's `--env KEY=VALUE`
(and `Config.env`) is not yet wired through `vmetted`. A daemon client
that needs guest env vars must bake them into the `exec` command itself
(e.g. `exec: "FOO=bar mycmd"`).

### Response stream

Newline-delimited JSON frames. Three kinds:

```json
{"kind":"stdout","data":"hello world\n"}
{"kind":"stderr","data":"[vmette] guest stopped (exit 17)\r\n"}
{"kind":"exit","code":17}
```

`stdout` carries the guest's process stdout, `stderr` carries vmette's
banner + delegate messages + guest stderr. The final frame is always
`exit` (or `error` on a daemon-side failure).

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
| `desktop_start` | `kernel`, `initramfs`, `image` (resolved client-side; required), `size?` (`"WxH"`), `net?`, `offline?`, `vcpus?`, `mem_mib?` | `{"kind":"session","session_id":"…"}` |
| `desktop_action` | `session_id`, `action` (a `vmette::Action`, e.g. `{"action":"screenshot"}`, mouse/key/type/scroll/exec) | `{"kind":"action_result","ok":true,"error?":"…","x?":…,"y?":…,"png_base64?":"…","text?":"…"}` (`text` carries the clipboard for `get_clipboard`) |
| `desktop_screenshot_settled` | `session_id`, `timeout_ms?` (default 10000), `stable_hold_ms?` (confirmation hold; small default, larger for launches) | `{"kind":"settled","settled":bool,"moving":[…],"png_base64":"…"}` |
| `desktop_what_changed` | `session_id` | `{"kind":"changed","changed?":{"x":…,"y":…,"w":…,"h":…},"png_base64":"…"}` (`changed` absent when nothing moved) |
| `desktop_stop` | `session_id` | `{"kind":"stopped"}` |

A daemon-side failure on any kind returns `{"kind":"error","message":"…"}`.

## Today vs the warm-pool roadmap

The stateless run path today spawns a `vmette` subprocess per request
(full cold boot). A warm-snapshot pool is a planned optimization, not
yet shipped (aarch64 only, since snapshot/restore is Apple-Silicon-only):

| Feature | Today | Warm-pool roadmap (aarch64 only) |
|---------|-------|----------------------------------|
| Per-request cost | ~1 s (full cold boot) | ~50 ms (snapshot resume) |
| Implementation | subprocess spawn per request | in-process warm-snapshot pool |
| Library API | unchanged | adds `OutputSink` trait for non-stdio output |

## When to use vmetted vs vmette

| Use case | Tool |
|----------|------|
| One-off invocation from a shell | `vmette` |
| Many short-lived invocations from a long-lived process | `vmetted` |
| Persistent desktop / computer-use sessions | `vmetted` (`desktop_*`) |
| Library embedding from Rust/C | link `libvmette` directly |
| Future warm-VM pool (aarch64) | `vmetted` (roadmap) |
