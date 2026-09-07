#!/usr/bin/env python3
"""Drive codex-pty-mcp over stdio like a real MCP client."""
import json, subprocess, sys, threading, time, queue

BIN = "/home/yangtim/.zcode/mcp/codex-pty-mcp/target/release/codex-pty-mcp"

proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, text=True, bufsize=1)
lines = queue.Queue()

def reader():
    for line in proc.stdout:
        lines.put(line.strip())
    lines.put(None)

threading.Thread(target=reader, daemon=True).start()

def send(msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()

def recv(want_id, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            line = lines.get(timeout=0.5)
        except queue.Empty:
            continue
        if line is None:
            print("SERVER DIED:", proc.stderr.read()[:2000]); sys.exit(1)
        msg = json.loads(line)
        if msg.get("id") == want_id:
            return msg
    raise TimeoutError(f"no response for id={want_id}")

def call(method, params, _id=[0]):
    _id[0] += 1
    send({"jsonrpc": "2.0", "id": _id[0], "method": method, "params": params})
    return _id[0]

def tool(name, args, timeout=30):
    i = call("tools/call", {"name": name, "arguments": args})
    resp = recv(i, timeout)
    content = resp["result"]["content"]
    return "\n".join(c.get("text", "") for c in content if c.get("type") == "text")

# handshake
i = call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"}})
init = recv(i)
print("== initialize ok:", init["result"]["serverInfo"]["name"] if "serverInfo" in init["result"] else init["result"])
send({"jsonrpc": "2.0", "method": "notifications/initialized"})

# tools/list
i = call("tools/list", {})
tl = recv(i)
# ZCode's client requires the SEP-2549 cache hint on tools/list results.
assert tl["result"].get("cacheScope") in ("public", "private"), \
    f"missing/invalid cacheScope: {tl['result'].get('cacheScope')!r}"
assert isinstance(tl["result"].get("ttlMs"), int) and tl["result"]["ttlMs"] >= 0, \
    f"missing/invalid ttlMs: {tl['result'].get('ttlMs')!r}"
print("== tools:", [t["name"] for t in tl["result"]["tools"]])

# 1) plain command + tail
print("=== pty_spawn: echo/ls ===")
out = tool("pty_spawn", {"command": "echo 你好-pty && uname -r", "cols": 100, "rows": 20})
print(out)

# 2) htop as real TUI
print("=== pty_spawn: htop ===")
out = tool("pty_spawn", {"command": "htop", "cols": 100, "rows": 25})
print(out[:2200])
sid = out.split("session_id=")[1].split("\n")[0].strip()
print(f"---- captured session_id: {sid}")

# 3) press F1? no — press 'q' via send (htop quits on q)
print("=== pty_send: 'q' to htop ===")
out = tool("pty_send", {"session_id": sid, "input": "q", "enter": False})
print(out[:800])

# 4) interactive shell session + vim-less test: python REPL
print("=== interactive python repl ===")
out = tool("pty_spawn", {"cols": 90, "rows": 20})   # interactive bash
sid2 = out.split("session_id=")[1].split("\n")[0].strip()
out = tool("pty_send", {"session_id": sid2, "input": "python3 -q"})
print(out[:600])
out = tool("pty_send", {"session_id": sid2, "input": "21*2+sum([1,2,3])"})
print(out[:600])
tool("pty_kill", {"session_id": sid2})

# 5) list
print("=== pty_list ===")
print(tool("pty_list", {}))

# 6) settle timing: a quick echo must settle early, not burn the 1500ms cap
print("=== settle timing: quick echo ===")
t0 = time.time()
out = tool("pty_spawn", {"command": "echo timing-test", "cols": 100, "rows": 20})
elapsed = time.time() - t0
assert "timing-test" in out, f"echo output missing: {out!r}"
assert elapsed < 1.2, f"settle too slow: {elapsed:.2f}s (expected < 1.2s)"
print(f"---- spawn returned in {elapsed*1000:.0f}ms")

# 7) strip_ansi: 3-byte ESC sequences (e.g. ESC ( 0 charset switch) must not
#    leave their final byte behind
print("=== strip_ansi: 3-byte charset switch ===")
out = tool("pty_spawn", {"command": "printf '\\033(0ab\\033(Bc\\n'", "cols": 100, "rows": 20})
sid_ansi = out.split("session_id=")[1].split("\n")[0].strip()
out = tool("pty_tail", {"session_id": sid_ansi, "max_bytes": 8000})
assert "abc" in out, f"stripped output missing 'abc': {out!r}"
assert "0abc" not in out, f"3-byte ESC sequence left residue '0abc': {out!r}"
print(out[:400])
tool("pty_kill", {"session_id": sid_ansi})

proc.terminate()
print("ALL OK")
