# codex-pty-mcp

Give your AI agent a **real pseudo-terminal** over MCP: run `htop`, `vim`,
REPLs, interactive installers, `sudo` prompts — anything that needs a TTY —
and read the screen as plain text.

The PTY/process layer is **vendored verbatim from
[openai/codex](https://github.com/openai/codex)** (`codex-rs/utils/pty`,
Apache-2.0) — the same battle-tested code Codex CLI uses in production.
On top of it sits a thin MCP service layer (the official
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK, the same one
codex uses) plus a `vt100` screen emulator that renders TUI output into
readable text, like `tmux capture-pane -p`.

```
┌─────────────────────────────────────────────┐
│ MCP client (ZCode / Claude / any agent)     │
└──────────────────┬──────────────────────────┘
        stdio JSON-RPC (rmcp 3.2)
┌──────────────────┴──────────────────────────┐
│ glue layer: 8 pty_* tools, session manager  │
│ vt100 screen emulation + ANSI-stripped tail │
├─────────────────────────────────────────────┤
│ codex-utils-pty (vendored from openai/codex)│
│ portable-pty · process groups · async IO    │
└─────────────────────────────────────────────┘
```

## Build

```sh
cargo build --release
# binary: target/release/codex-pty-mcp
```

## Register with your MCP client

ZCode (`~/.zcode/cli/config.json`):

```json
{
  "mcp": {
    "servers": {
      "codex-pty-mcp": {
        "command": "/path/to/codex-pty-mcp/target/release/codex-pty-mcp",
        "args": []
      }
    }
  }
}
```

Generic clients (Claude-style `mcpServers`):

```json
{
  "mcpServers": {
    "codex-pty-mcp": {
      "command": "/path/to/codex-pty-mcp/target/release/codex-pty-mcp"
    }
  }
}
```

Restart your client so the server connects at session start.

## Tools

| Tool | Purpose |
|---|---|
| `pty_spawn` | Spawn a command (`bash -lc <command>`) or an interactive login shell in a new PTY; returns the rendered screen |
| `pty_send` | Type text (optional Enter), wait for output to settle, return the screen |
| `pty_ctrl` | Send a special key: `c-c`, `c-d`, `enter`, `esc`, `up/down/left/right`, `pageup/pagedown`, … |
| `pty_screen` | Read the current rendered screen (plain text, tmux capture-pane style) |
| `pty_tail` | Read the last N bytes of raw output with ANSI escapes stripped (best for non-TUI commands, UTF-8 preserved) |
| `pty_resize` | Resize the PTY in character cells |
| `pty_list` | List sessions with command, size, exit status |
| `pty_kill` | Kill the session's process group and drop it |

Rules of thumb: TUI apps → read `pty_screen`; plain commands / long output →
`pty_tail`. Sessions stay alive across tool calls, so you can spawn a REPL
once and keep typing into it.

## Testing

`scripts/test_pty_mcp.py` speaks raw MCP JSON-RPC over stdio — spawns `htop`,
screenshots the rendered display, quits it, drives a Python REPL, and asserts
exit codes. Good smoke test after any change:

```sh
python3 scripts/test_pty_mcp.py
```

## License

Apache-2.0 (see [LICENSE](LICENSE)). The vendored codex files under
`src/codex_pty/` keep their upstream provenance in
[src/codex_pty/VENDORED.md](src/codex_pty/VENDORED.md); see also
[NOTICE.md](NOTICE.md).

Not affiliated with OpenAI. This is an independent extraction for personal
tooling; all credit for the hard PTY parts belongs to the codex authors.
