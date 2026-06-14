#!/usr/bin/env python3
"""Drive vmette-mcp over stdio (newline-delimited JSON-RPC) and run scenarios live.

This is a manual end-to-end harness, like `tests/run.sh`: it boots real VMs, so
it needs a codesigned macOS build. Build the release binaries first:

    cargo build --release -p vmette-mcp -p vmette-cli

then run:

    python3 tests/mcp/driver.py          # the subprocess (execute/workspace/fetch) suite
    python3 tests/mcp/desktop_e2e.py     # the desktop (Xvfb + Chromium) suite
    python3 tests/mcp/call.py <tool> '<json-args>' [--save shot.png]

The repo root is auto-detected from this file's location; override with
VMETTE_REPO if you run the scripts from a copy elsewhere.
"""
import json
import os
import pathlib
import subprocess
import sys
import threading
import time
import queue

REPO = os.environ.get("VMETTE_REPO") or str(pathlib.Path(__file__).resolve().parents[2])
MCP_BIN = os.environ.get("VMETTE_MCP_BIN", f"{REPO}/target/release/vmette-mcp")
VMETTE_BIN = os.environ.get("VMETTE_BIN", f"{REPO}/target/release/vmette")

_bootstrapped = False


def ensure_built_and_signed():
    """Build + codesign the binaries under test, from source, once per run.

    vmette-mcp boots one-shot VMs *in-process* (execute/workspace/fetch_url) and
    auto-spawns vmetted for the desktop tools; both boot VMs in-process, so each
    needs the com.apple.security.virtualization entitlement. Any cargo build
    invalidates a prior signature, so — like tests/desktop.sh — we rebuild and
    re-sign unconditionally rather than trust a stale binary. Skipped when the
    binaries are overridden to point outside the repo (VMETTE_MCP_BIN).
    """
    global _bootstrapped
    if _bootstrapped or os.environ.get("VMETTE_SKIP_BUILD"):
        return
    _bootstrapped = True
    if not MCP_BIN.startswith(REPO):
        return  # custom binary path — caller owns building/signing it
    ents = f"{REPO}/entitlements.plist"
    subprocess.run(
        ["cargo", "build", "--release", "-p", "vmette-mcp", "-p", "vmette-cli",
         "-p", "vmette-daemon"],
        cwd=REPO, check=True,
    )
    for binary in ("vmette-mcp", "vmetted", "vmette"):
        path = f"{REPO}/target/release/{binary}"
        if os.path.exists(path):
            subprocess.run(
                ["codesign", "--sign", "-", "--force", "--entitlements", ents,
                 "--options=runtime", path],
                check=True, capture_output=True,
            )


class MCP:
    def __init__(self, allow_network=True):
        ensure_built_and_signed()
        args = [MCP_BIN]
        if allow_network:
            args.append("--allow-network")
        env = dict(os.environ, VMETTE_BIN=VMETTE_BIN, RUST_LOG="vmette_mcp=warn")
        self.p = subprocess.Popen(
            args, cwd=REPO, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, bufsize=1, env=env,
        )
        self._id = 0
        self.q = queue.Queue()
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._stderr, daemon=True).start()

    def _reader(self):
        for line in self.p.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.q.put(json.loads(line))
            except json.JSONDecodeError:
                sys.stderr.write(f"[non-json stdout] {line}\n")

    def _stderr(self):
        for line in self.p.stderr:
            sys.stderr.write(f"[mcp-stderr] {line.rstrip()}\n")

    def _send(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self._id += 1
            msg["id"] = self._id
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        return msg.get("id")

    def request(self, method, params=None, timeout=180):
        want = self._send(method, params)
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = self.q.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg.get("id") == want:
                return msg
        raise TimeoutError(f"no response to {method} in {timeout}s")

    def notify(self, method, params=None):
        self._send(method, params, notify=True)

    def call_tool(self, name, args, timeout=180):
        return self.request("tools/call", {"name": name, "arguments": args}, timeout)

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def text_of(resp):
    """Pull text content out of a tools/call result."""
    r = resp.get("result", {})
    if "content" in r:
        parts = []
        for c in r["content"]:
            if c.get("type") == "text":
                parts.append(c["text"])
            elif c.get("type") == "image":
                parts.append(f"<image {c.get('mimeType')} {len(c.get('data',''))} b64 bytes>")
        return "\n".join(parts)
    return json.dumps(r)


PASS, FAIL = [], []


def check(name, cond, detail=""):
    (PASS if cond else FAIL).append(name)
    mark = "PASS" if cond else "FAIL"
    print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""))


def main():
    m = MCP(allow_network=True)
    init = m.request("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "mcp_driver", "version": "0"},
    })
    sv = init.get("result", {}).get("serverInfo", {})
    print(f"== initialize: {sv}")
    m.notify("notifications/initialized")

    tools = m.request("tools/list")
    names = sorted(t["name"] for t in tools["result"]["tools"])
    print(f"== {len(names)} tools: {names}")
    check("tools/list non-empty", len(names) >= 10)

    print("\n== Scenario: execute python")
    r = m.call_tool("execute", {"language": "python", "code": "print(sum(range(101)))"})
    out = text_of(r); print(out)
    check("python sum(0..100)=5050", "5050" in out and "exit: 0" in out)

    print("\n== Scenario: execute node")
    r = m.call_tool("execute", {"language": "node", "code": "console.log(2**10)"})
    out = text_of(r); print(out)
    check("node 2**10=1024", "1024" in out and "exit: 0" in out)

    print("\n== Scenario: execute shell (quoting torture)")
    r = m.call_tool("execute", {"language": "shell", "code": "echo \"it's $((6*7)) `echo nested`\""})
    out = text_of(r); print(out)
    check("shell quoting => it's 42 nested", "it's 42 nested" in out)

    print("\n== Scenario: execute timeout => exit 124")
    r = m.call_tool("execute", {"language": "shell", "code": "sleep 30", "timeout": 3})
    out = text_of(r); print(out)
    check("timeout yields exit 124", "124" in out)

    print("\n== Scenario: workspace lifecycle")
    r = m.call_tool("workspace_create", {})
    res = r.get("result", {})
    # workspace_create returns structured JSON (Json wrapper) -> structuredContent or content text
    ws_id = None
    if "structuredContent" in res:
        ws_id = res["structuredContent"].get("workspace_id")
    if not ws_id:
        # fall back: text content is JSON
        try:
            ws_id = json.loads(text_of(r)).get("workspace_id")
        except Exception:
            pass
    print(f"  workspace_id={ws_id}")
    check("workspace_create returns id", bool(ws_id))

    if ws_id:
        r = m.call_tool("workspace_write", {"workspace_id": ws_id, "path": "hello.txt", "content": "from-host-42\n"})
        print("  write:", text_of(r))
        check("workspace_write ok", "wrote" in text_of(r))

        r = m.call_tool("workspace_run", {"workspace_id": ws_id, "command": "cat hello.txt; echo computed=$((40+2)) > out.txt"})
        out = text_of(r); print("  run:", out)
        check("workspace_run sees host file", "from-host-42" in out)

        r = m.call_tool("workspace_read", {"workspace_id": ws_id, "path": "out.txt"})
        out = text_of(r); print("  read:", out)
        check("workspace_read sees guest-written file", "computed=42" in out)

        # path-safety: absolute / .. must be rejected
        r = m.call_tool("workspace_read", {"workspace_id": ws_id, "path": "../../../etc/passwd"})
        err = r.get("error") or text_of(r)
        print("  traversal-read:", json.dumps(err)[:160])
        check("path traversal rejected", "error" in r or "not" in str(err).lower())

        r = m.call_tool("workspace_destroy", {"workspace_id": ws_id})
        print("  destroy:", text_of(r))
        check("workspace_destroy ok", "destroyed" in text_of(r))

    print("\n== Scenario: fetch_url (network)")
    r = m.call_tool("fetch_url", {"url": "https://example.com", "max_bytes": 400})
    out = text_of(r); print(out[:300])
    check("fetch_url example.com 200 + body", '"status": 200' in out and "Example Domain" in out)

    print("\n== Scenario: fetch_url scheme rejection")
    r = m.call_tool("fetch_url", {"url": "file:///etc/passwd"})
    err = r.get("error") or text_of(r)
    print("  ", json.dumps(err)[:160])
    check("file:// scheme rejected", "error" in r and "http" in json.dumps(err).lower())

    print("\n== Scenario: execute unknown language rejection")
    r = m.call_tool("execute", {"language": "ruby", "code": "puts 1"})
    err = r.get("error") or text_of(r)
    print("  ", json.dumps(err)[:160])
    check("unknown language rejected", "error" in r and "ruby" in json.dumps(err).lower())

    m.close()
    print(f"\n==== RESULT: {len(PASS)} passed, {len(FAIL)} failed ====")
    if FAIL:
        print("FAILED:", FAIL)
        sys.exit(1)


if __name__ == "__main__":
    main()
