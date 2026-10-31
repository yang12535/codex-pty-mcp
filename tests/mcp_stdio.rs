//! Integration tests: launch the `codex-pty-mcp` binary as a subprocess and
//! drive it over stdio with newline-delimited MCP JSON-RPC, exactly like a
//! real MCP client. Covers the initialize handshake, the tool catalog, a full
//! bash-session lifecycle (spawn → send → screen → resize → kill), and the
//! error surfaces for closed/unknown sessions.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous ceiling for a single round-trip; tool calls settle internally
/// (quiet window) so typical latency is well under a second.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// One server process plus a client-side request/response multiplexer.
struct Server {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    pending: HashMap<u64, Value>,
    next_id: u64,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-pty-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the codex-pty-mcp binary");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let mut stderr_pipe = child.stderr.take().expect("server stderr");

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            // Read chunk-wise and hold the mutex only to append, so the
            // timeout diagnostics in wait_response can never block on a
            // server that stays alive without closing its stderr.
            let mut stderr = stderr_pipe;
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut captured = stderr_sink.lock().unwrap();
                        captured.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            }
        });

        Self {
            child,
            stdin,
            rx,
            stderr,
            pending: HashMap::new(),
            next_id: 0,
        }
    }

    fn send_line(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to server stdin");
        self.stdin.flush().expect("flush server stdin");
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str) {
        self.send_line(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// Wait for the response with the given id, stashing out-of-order
    /// responses so concurrent round-trips stay correct.
    fn wait_response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        if let Some(msg) = self.pending.remove(&id) {
            return msg;
        }
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
            match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    let message = serde_json::from_str::<Value>(&line)
                        .unwrap_or_else(|e| panic!("malformed JSON-RPC line {line:?}: {e}"));
                    match message.get("id").and_then(Value::as_u64) {
                        Some(got) if got == id => return message,
                        Some(got) => {
                            self.pending.insert(got, message);
                        }
                        // Server-initiated notification; nothing expected today.
                        None => {}
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let stderr = self.stderr.lock().unwrap().clone();
                    panic!("timed out waiting for response id={id}; server stderr:\n{stderr}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let stderr = self.stderr.lock().unwrap().clone();
                    panic!("server stdout closed before response id={id}; stderr:\n{stderr}");
                }
            }
        }
    }

    /// Complete the MCP initialize handshake and mark it acknowledged.
    fn initialize(&mut self) {
        let id = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "codex-pty-mcp-integration-test", "version": "0"}
            }),
        );
        let response = self.wait_response(id);
        let result = response
            .get("result")
            .expect("initialize must return a result");
        assert_eq!(
            result["protocolVersion"], "2025-06-18",
            "server must negotiate the requested protocol version: {result}"
        );
        assert!(
            result["serverInfo"]["name"].as_str().is_some_and(|n| !n.is_empty()),
            "serverInfo.name must be present: {result}"
        );
        assert!(
            result["capabilities"]["tools"].is_object(),
            "server must advertise tool support: {result}"
        );
        self.notify("notifications/initialized");
    }

    /// Call a tool; Ok carries the joined text content, Err the JSON-RPC error.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, (i64, String)> {
        let id = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let response = self.wait_response(id);
        if let Some(error) = response.get("error") {
            return Err((
                error.get("code").and_then(Value::as_i64).unwrap_or(0),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ));
        }
        let result = &response["result"];
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(!is_error, "{name} returned a tool-level error: {result}");
        let text = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} response missing content array: {response}"))
            .iter()
            .filter_map(|block| {
                if block["type"] == "text" {
                    block["text"].as_str().map(str::to_owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(text)
    }

    fn tool_text(&mut self, name: &str, arguments: Value) -> String {
        self.call_tool(name, arguments)
            .unwrap_or_else(|(code, message)| panic!("{name} failed: {code} {message}"))
    }

    /// Assert a tool call fails at the JSON-RPC level and return (code, message).
    fn tool_err(&mut self, name: &str, arguments: Value) -> (i64, String) {
        self.call_tool(name, arguments)
            .expect_err(&format!("{name} unexpectedly succeeded"))
    }

    /// Spawn a session and return (session_id, full response text).
    fn spawn_session(&mut self, command: Option<&str>) -> (String, String) {
        let args = match command {
            Some(cmd) => json!({"command": cmd}),
            None => json!({}),
        };
        let text = self.tool_text("pty_spawn", args);
        let session_id = text
            .lines()
            .find_map(|line| line.strip_prefix("session_id="))
            .map(str::to_owned)
            .expect("pty_spawn response must contain a session_id line");
        (session_id, text)
    }

    fn list_session_ids(&mut self) -> Vec<String> {
        self.tool_text("pty_list", json!({}))
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                rest.split(']').next().map(str::to_owned)
            })
            .collect()
    }

    /// Kill every live session so a test never leaks processes.
    fn kill_all(&mut self) {
        for session_id in self.list_session_ids() {
            self.tool_text("pty_kill", json!({"session_id": session_id}));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drop the leading header block (everything before the first blank line) so
/// assertions run against actual PTY output — the header embeds the original
/// command string and would otherwise satisfy marker checks by itself.
/// Mirrors scripts/test_pty_mcp.py's payload().
fn payload(text: &str) -> &str {
    text.split_once("\n\n")
        .map(|(_, body)| body)
        .unwrap_or_else(|| panic!("response missing output section after blank line: {text}"))
}

#[test]
fn initialize_handshake_and_tool_catalog() {
    let mut server = Server::start();
    server.initialize();

    let id = server.request("tools/list", json!({}));
    let result = server.wait_response(id)["result"].clone();
    // ZCode-style clients validate the SEP-2549 extended cache hint.
    assert_eq!(result["ttlMs"], 0, "cache hint ttlMs must be 0: {result}");
    assert_eq!(
        result["cacheScope"], "private",
        "cache hint cacheScope must be private: {result}"
    );

    let tools = result["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 8, "expected exactly 8 tools: {tools:?}");
    let by_name: HashMap<&str, &Value> = tools
        .iter()
        .map(|tool| (tool["name"].as_str().expect("tool name"), tool))
        .collect();
    for name in [
        "pty_spawn",
        "pty_send",
        "pty_ctrl",
        "pty_screen",
        "pty_tail",
        "pty_resize",
        "pty_list",
        "pty_kill",
    ] {
        let tool = by_name
            .get(name)
            .unwrap_or_else(|| panic!("tool {name} missing from tools/list: {tools:?}"));
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool {name} must have a description"
        );
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "tool {name} inputSchema must be an object schema"
        );
    }

    let required = |name: &str| by_name[name]["inputSchema"]["required"].clone();
    assert_eq!(required("pty_spawn"), json!([]));
    assert_eq!(required("pty_send"), json!(["session_id", "input"]));
    assert_eq!(required("pty_ctrl"), json!(["session_id", "key"]));
    assert_eq!(required("pty_screen"), json!(["session_id"]));
    assert_eq!(required("pty_tail"), json!(["session_id"]));
    assert_eq!(required("pty_resize"), json!(["session_id", "cols", "rows"]));
    assert_eq!(required("pty_list"), json!([]));
    assert_eq!(required("pty_kill"), json!(["session_id"]));

    for name in ["pty_screen", "pty_tail", "pty_list"] {
        assert_eq!(
            by_name[name]["annotations"]["readOnlyHint"], true,
            "tool {name} must be annotated read-only"
        );
    }
}

#[test]
fn bash_session_send_echo_hello_and_read_screen() {
    let mut server = Server::start();
    server.initialize();

    let (session_id, spawned) = server.spawn_session(Some("bash --norc"));
    let header = spawned.lines().next().expect("pty_spawn header line");
    assert!(header.contains("running"), "fresh session must be running: {header}");
    assert!(
        header.contains("size=120x36"),
        "spawn without size overrides must default to 120x36: {header}"
    );

    let sent = server.tool_text("pty_send", json!({"session_id": session_id, "input": "echo hello"}));
    assert!(sent.contains("echo hello"), "typed line should echo: {sent}");
    assert!(
        sent.lines().any(|line| line.trim() == "hello"),
        "command output 'hello' must appear: {sent}"
    );

    let screen = server.tool_text("pty_screen", json!({"session_id": session_id}));
    assert!(
        screen.lines().any(|line| line.trim() == "hello"),
        "pty_screen must still show the output: {screen}"
    );

    server.kill_all();
}

#[test]
fn resize_updates_pty_size() {
    let mut server = Server::start();
    server.initialize();

    let (session_id, _) = server.spawn_session(Some("bash --norc"));

    let resized = server.tool_text(
        "pty_resize",
        json!({"session_id": session_id, "cols": 100, "rows": 20}),
    );
    assert!(
        resized.contains("size=100x20"),
        "header must report the new geometry: {resized}"
    );

    // `stty size` reads the kernel tty, proving the PTY ioctl really landed.
    let stty = server.tool_text("pty_send", json!({"session_id": session_id, "input": "stty size"}));
    assert!(
        stty.lines().any(|line| line.trim() == "20 100"),
        "stty size must report rows=20 cols=100 after resize: {stty}"
    );

    server.kill_all();
}

#[test]
fn tail_returns_command_output_and_exit_status() {
    let mut server = Server::start();
    server.initialize();

    let (_, spawned) = server.spawn_session(Some("printf 'tail-marker-12345\\n'"));
    let header = spawned.lines().next().expect("pty_spawn header line");
    assert!(
        header.contains("exited (code=0)"),
        "one-shot command must report exit status 0: {header}"
    );
    assert!(
        payload(&spawned).contains("tail-marker-12345"),
        "spawn response body must carry the command output: {spawned}"
    );

    let session_id = spawned
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .expect("session_id")
        .to_string();
    let tail = server.tool_text("pty_tail", json!({"session_id": session_id}));
    assert!(
        payload(&tail).contains("tail-marker-12345"),
        "pty_tail body must carry the output: {tail}"
    );

    server.kill_all();
}

#[test]
fn ctrl_sends_keys_and_unknown_key_is_rejected() {
    let mut server = Server::start();
    server.initialize();

    // ctrl-D at the prompt terminates the interactive shell.
    let (session_id, _) = server.spawn_session(Some("bash --norc"));
    let exited = server.tool_text("pty_ctrl", json!({"session_id": session_id, "key": "c-d"}));
    assert!(
        exited.lines().next().is_some_and(|h| h.contains("exited (code=0)")),
        "ctrl-D must terminate the shell with exit code 0: {exited}"
    );
    server.kill_all();

    let (session_id, _) = server.spawn_session(Some("bash --norc"));
    let (code, message) = server.tool_err("pty_ctrl", json!({"session_id": session_id, "key": "nope"}));
    assert_eq!(code, -32602, "unknown key must be invalid params");
    assert!(message.contains("unknown key"), "unexpected message: {message}");

    server.kill_all();
}

#[test]
fn kill_closes_session_and_later_calls_error() {
    let mut server = Server::start();
    server.initialize();

    let (session_id, _) = server.spawn_session(Some("bash --norc"));

    let killed = server.tool_text("pty_kill", json!({"session_id": session_id}));
    assert!(
        killed.starts_with("killed"),
        "pty_kill must confirm the kill: {killed}"
    );
    assert!(
        !server.list_session_ids().contains(&session_id),
        "killed session must disappear from pty_list"
    );

    // Operating on the closed session is a JSON-RPC invalid-params error.
    for (name, arguments) in [
        ("pty_send", json!({"session_id": session_id, "input": "echo hi"})),
        ("pty_screen", json!({"session_id": session_id})),
        ("pty_tail", json!({"session_id": session_id})),
        ("pty_ctrl", json!({"session_id": session_id, "key": "enter"})),
        ("pty_resize", json!({"session_id": session_id, "cols": 80, "rows": 24})),
    ] {
        let (code, message) = server.tool_err(name, arguments);
        assert_eq!(code, -32602, "{name} on a killed session: ({code}, {message})");
        assert!(
            message.contains("unknown session_id"),
            "{name} on a killed session must report unknown session_id, got: {message}"
        );
    }

    // Killing the same session again is a benign "not found", not an error.
    let again = server.tool_text("pty_kill", json!({"session_id": session_id}));
    assert!(
        again.contains("not found"),
        "second kill must be a benign not-found: {again}"
    );
}
