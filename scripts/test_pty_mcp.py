#!/usr/bin/env python3
"""Drive the checkout's release binary over stdio like a real MCP client."""

import argparse
import json
from pathlib import Path
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import time


def payload(text):
    """Exclude headers, which contain the original command and its markers."""
    header, separator, body = text.partition("\n\n")
    assert separator, f"missing output section: {header!r}"
    return body


def session_id(text):
    match = re.search(r"^session_id=(pty-\d+)$", text, re.MULTILINE)
    assert match, f"missing session id: {text!r}"
    return match[1]


def result_text(response):
    assert "error" not in response, response
    result = response["result"]
    assert not result.get("isError"), result
    return "\n".join(c.get("text", "") for c in result["content"]
                     if c.get("type") == "text")


def wait_for_completed_output(client, out, expected, timeout=10):
    """Capacity tests allow startup past the quiet window under CPU load."""
    sid = session_id(out)
    deadline = time.monotonic() + timeout
    while "exited (code=0)" not in out.splitlines()[0] or expected not in payload(out):
        assert time.monotonic() < deadline, out
        time.sleep(0.05)
        out = client.tool("pty_screen", {"session_id": sid})
    return out


class Client:
    def __init__(self, binary):
        self.stderr = tempfile.TemporaryFile(mode="w+t")
        self.proc = subprocess.Popen(
            [str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.stderr, text=True, bufsize=1,
        )
        self.lines = queue.Queue()
        self.pending = {}
        self.next_id = 0
        threading.Thread(target=self.reader, daemon=True).start()

    def reader(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def call(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id,
                   "method": method, "params": params})
        return self.next_id

    def recv(self, want_id, timeout=30):
        if want_id in self.pending:
            return self.pending.pop(want_id)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"no response for id={want_id}")
            try:
                line = self.lines.get(timeout=remaining)
            except queue.Empty:
                raise TimeoutError(f"no response for id={want_id}") from None
            if line is None:
                self.stderr.seek(0)
                raise RuntimeError(f"server died: {self.stderr.read()[:2000]}")
            message = json.loads(line)
            if message.get("id") == want_id:
                return message
            if "id" in message:
                # Concurrent requests can complete out of order.
                self.pending[message["id"]] = message

    def tool(self, name, arguments):
        request = self.call("tools/call", {"name": name, "arguments": arguments})
        return result_text(self.recv(request))

    def clear_sessions(self):
        listing = self.tool("pty_list", {})
        for sid in re.findall(r"^\[(pty-\d+)\]", listing, re.MULTILINE):
            self.tool("pty_kill", {"session_id": sid})

    def close(self):
        try:
            if self.proc.poll() is None:
                self.clear_sessions()
        finally:
            self.proc.stdin.close()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
            self.proc.stdout.close()
            self.stderr.close()


def run(client):
    init = client.recv(client.call("initialize", {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "regression-test", "version": "1"},
    }))
    assert "result" in init, init
    client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    tools_result = client.recv(client.call("tools/list", {}))["result"]
    assert tools_result.get("cacheScope") in ("public", "private"), tools_result
    assert type(tools_result.get("ttlMs")) is int and tools_result["ttlMs"] >= 0
    tools = {tool["name"]: tool for tool in tools_result["tools"]}
    assert len(tools) == 8, tools
    for name in ("pty_screen", "pty_tail", "pty_list"):
        assert tools[name]["annotations"]["readOnlyHint"] is True, tools[name]
    print("PASS MCP handshake, tool list, cache hints, read-only annotations")

    out = client.tool("pty_spawn", {"command": "printf '你好-pty\\n'"})
    first = session_id(out)
    assert "你好-pty" in payload(out), out
    assert "exited (code=0)" in out.splitlines()[0], out
    assert "Some(" not in out.splitlines()[0], out
    client.tool("pty_spawn", {"command": "echo second-command"})
    for name in ("pty_screen", "pty_tail"):
        assert "你好-pty" in payload(client.tool(name, {"session_id": first}))
    print("PASS completed sessions stay readable after another spawn")

    start = time.monotonic()
    out = client.tool("pty_spawn", {"command": "echo timing-test"})
    elapsed = time.monotonic() - start
    assert "timing-test" in payload(out), out
    assert elapsed < 1.2, f"settle took {elapsed:.2f}s (expected < 1.2s)"
    print(f"PASS quick echo settles in {elapsed * 1000:.0f} ms")

    # Ignore terminal hangup so the descendant survives its session leader.
    # Immediate output resets the quiet window before the delayed write.
    out = client.tool("pty_spawn", {
        "command": "trap '' HUP; echo ready; (sleep 0.15; echo late-marker) &",
    })
    assert "late-marker" in payload(out), out
    print("PASS delayed output after wrapper exit is captured")

    for command, expected in (
        ("printf '\\033(0ab\\033(Bc\\n'", "abc\n"),
        ("printf 'a\\033$(Cb\\n'", "ab\n"),
        ("printf 'a\\033(\\nb\\n'", "a\nb\n"),
        ("printf 'a\\033(中文\\n'", "a中文\n"),
    ):
        sid = session_id(client.tool("pty_spawn", {"command": command}))
        tail = payload(client.tool("pty_tail", {"session_id": sid}))
        assert tail == expected, (command, tail, expected)
        client.tool("pty_kill", {"session_id": sid})
    print("PASS ESC intermediates, malformed sequences, UTF-8 payloads")

    out = client.tool("pty_spawn", {"command": "python3 -q"})
    sid = session_id(out)
    out = client.tool("pty_send", {"session_id": sid, "input": "21*2+sum([1,2,3])"})
    assert "48" in payload(out), out
    out = client.tool("pty_resize", {"session_id": sid, "cols": 90, "rows": 20})
    assert "size=90x20" in out, out
    out = client.tool("pty_ctrl", {"session_id": sid, "key": "c-d"})
    assert "exited (code=0)" in out.splitlines()[0], out
    client.tool("pty_kill", {"session_id": sid})
    print("PASS interactive REPL, send, resize, control key")

    if shutil.which("htop"):
        out = client.tool("pty_spawn", {"command": "htop", "cols": 100, "rows": 25})
        sid = session_id(out)
        # htop can initialize more slowly than the initial quiet window.
        deadline = time.monotonic() + 5
        while not payload(out).strip() and time.monotonic() < deadline:
            time.sleep(0.1)
            out = client.tool("pty_screen", {"session_id": sid})
        assert payload(out).strip() and "running" in out.splitlines()[0], out
        out = client.tool("pty_send", {"session_id": sid, "input": "q", "enter": False})
        assert "exited (code=0)" in out.splitlines()[0], out
        client.tool("pty_kill", {"session_id": sid})
        print("PASS htop TUI rendering and quit")
    else:
        print("SKIP optional htop smoke test (htop not installed)")

    client.clear_sessions()
    requests = [client.call("tools/call", {
        "name": "pty_spawn", "arguments": {"command": "exec sleep 60"},
    }) for _ in range(80)]
    accepted = []
    rejected = 0
    for request in requests:
        response = client.recv(request)
        if "error" in response:
            assert "too many sessions (max 64)" in response["error"]["message"], response
            rejected += 1
        else:
            accepted.append(session_id(result_text(response)))
    assert len(accepted) == 64 and rejected == 16, (len(accepted), rejected)
    listing = client.tool("pty_list", {})
    assert len(re.findall(r"^\[pty-\d+\]", listing, re.MULTILINE)) == 64, listing
    client.tool("pty_kill", {"session_id": accepted[0]})
    # Allocation must succeed immediately after kill; output can arrive
    # later when the runner is still scheduling the concurrent startup burst.
    replacement = client.tool("pty_spawn", {"command": "echo replacement"})
    wait_for_completed_output(client, replacement, "replacement")
    # At capacity, only the completed replacement is eligible for eviction.
    # The public header reports exit, but pump EOF may follow it, so wait
    # boundedly for eviction eligibility. Allocation after kill above never
    # retries, and the Rust tests check the exact EOF/capacity transition.
    deadline = time.monotonic() + 10
    while True:
        response = client.recv(client.call("tools/call", {
            "name": "pty_spawn", "arguments": {"command": "echo pressure-replacement"},
        }))
        if "error" not in response:
            break
        assert "too many sessions (max 64)" in response["error"]["message"], response
        assert time.monotonic() < deadline, response
        time.sleep(0.05)
    wait_for_completed_output(client, result_text(response), "pressure-replacement")
    print("PASS 80 concurrent spawns: 64 accepted, 16 rejected; kill and eviction free capacity")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=(
        Path(__file__).resolve().parents[1] / "target/release/codex-pty-mcp"
    ))
    args = parser.parse_args()
    binary = args.binary.resolve()
    print(f"Testing {binary}")
    client = Client(binary)
    try:
        run(client)
    finally:
        client.close()
    print("ALL OK")


if __name__ == "__main__":
    main()
