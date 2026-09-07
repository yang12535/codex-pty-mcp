//! rmcp ServerHandler: eight tools for driving interactive PTY sessions.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;use rmcp::model::ContentBlock;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use serde::Deserialize;
use serde_json::json;

use crate::session::SessionManager;
use crate::session::settle;

#[derive(Clone)]
pub struct PtyMcpServer {
    manager: Arc<SessionManager>,
}

impl PtyMcpServer {
    pub fn new() -> Self {
        Self {
            manager: Arc::new(SessionManager::default()),
        }
    }

    fn tool(
        name: &str,
        description: &str,
        schema: serde_json::Value,
        required: &[&str],
    ) -> Tool {
        let mut schema: JsonObject = serde_json::from_value(schema)
            .expect("static tool schema must be valid JSON object");
        let required_list: Vec<serde_json::Value> =
            required.iter().map(|r| json!(r)).collect();
        schema.insert("required".into(), json!(required_list));
        Tool::new(
            Cow::Owned(name.to_string()),
            Cow::Owned(description.to_string()),
            Arc::new(schema),
        )
    }
}

impl Default for PtyMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct SpawnParams {
    command: Option<String>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

#[derive(Deserialize)]
struct SendParams {
    session_id: String,
    input: String,
    enter: Option<bool>,
    wait_ms: Option<u64>,
}

#[derive(Deserialize)]
struct SessionIdParams {
    session_id: String,
}

#[derive(Deserialize)]
struct TailParams {
    session_id: String,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct CtrlParams {
    session_id: String,
    key: String,
    wait_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ResizeParams {
    session_id: String,
    cols: u16,
    rows: u16,
}

/// Map a key name to the byte sequence written to the PTY.
fn ctrl_bytes(key: &str) -> Option<Vec<u8>> {
    let seq: Vec<u8> = match key {
        "c-c" | "ctrl-c" | "^c" => vec![0x03],
        "c-d" | "ctrl-d" | "^d" => vec![0x04],
        "c-z" | "ctrl-z" | "^z" => vec![0x1a],
        "c-l" | "ctrl-l" | "^l" => vec![0x0c],
        "enter" | "return" => vec![b'\r'],
        "esc" | "escape" => vec![0x1b],
        "tab" => vec![b'\t'],
        "space" => vec![b' '],
        "bspace" | "backspace" => vec![0x7f],
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        _ => return None,
    };
    Some(seq)
}

fn header(s: &crate::session::Session) -> String {
    let exited = if s.handle.has_exited() {
        format!("exited (code={:?})", s.handle.exit_code())
    } else {
        "running".to_string()
    };
    let size = s
        .size
        .lock()
        .map(|size| format!("{}x{}", size.cols, size.rows))
        .unwrap_or_else(|_| "?x?".into());
    format!(
        "[{}] cmd={:?} size={} {}",
        s.id, s.command, size, exited
    )
}

fn internal<E: std::fmt::Display>(error: E) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn text_result(text: String) -> Result<CallToolResponse, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
}

impl ServerHandler for PtyMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let obj = json!({
            "type": "object",
            "properties": {},
        });
        let tools = vec![
            Self::tool(
                "pty_spawn",
                "Spawn a command in a new PTY session and return its screen. \
                 Omit `command` to get an interactive login shell. TUI programs \
                 (htop, vim, fzf, REPLs...) run for real.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell line to run via `bash -lc`. Omit for an interactive shell."},
                        "cwd": {"type": "string", "description": "Working directory."},
                        "cols": {"type": "integer", "description": "Terminal width, default 120."},
                        "rows": {"type": "integer", "description": "Terminal height, default 36."}
                    },
                }),
                &[],
            ),
            Self::tool(
                "pty_send",
                "Type text into a running PTY session, optionally pressing Enter, \
                 then wait for output to settle and return the rendered screen.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "input": {"type": "string", "description": "Text to write to the PTY."},
                        "enter": {"type": "boolean", "description": "Append Enter after the text, default true."},
                        "wait_ms": {"type": "integer", "description": "Max time to wait for output to settle, default 1500."}
                    },
                }),
                &["session_id", "input"],
            ),
            Self::tool(
                "pty_ctrl",
                "Send a control/special key to a session: c-c, c-d, c-z, enter, esc, \
                 tab, bspace, up/down/left/right, pageup/pagedown, home/end.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "key": {"type": "string", "description": "Key name, e.g. c-c, enter, up."},
                        "wait_ms": {"type": "integer", "description": "Max settle time, default 1500."}
                    },
                }),
                &["session_id", "key"],
            ),
            Self::tool(
                "pty_screen",
                "Read the current rendered screen (tmux capture-pane style plain text) of a session.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"}
                    },
                }),
                &["session_id"],
            ),
            Self::tool(
                "pty_tail",
                "Read the last N bytes of raw output with ANSI escapes stripped. \
                 Best for normal (non-TUI) command output, scrollback, and anything \
                 longer than one screen.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "max_bytes": {"type": "integer", "description": "Default 8000."}
                    },
                }),
                &["session_id"],
            ),
            Self::tool(
                "pty_resize",
                "Resize a session's terminal in character cells.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "cols": {"type": "integer"},
                        "rows": {"type": "integer"}
                    },
                }),
                &["session_id", "cols", "rows"],
            ),
            Self::tool(
                "pty_list",
                "List all PTY sessions with their command, size and exit status.",
                obj.clone(),
                &[],
            ),
            Self::tool(
                "pty_kill",
                "Kill a session's process group and drop the session.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"}
                    },
                }),
                &["session_id"],
            ),
        ];
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        let manager = &*self.manager;
        match request.name.as_ref() {
            "pty_spawn" => {
                let params: SpawnParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let cols = params.cols.unwrap_or(120);
                let rows = params.rows.unwrap_or(36);
                let session = manager
                    .spawn(params.command, params.cwd, cols, rows)
                    .await
                    .map_err(internal)?;
                settle(&session, 1500).await;
                text_result(format!(
                    "{}\nsession_id={}\n\n{}",
                    header(&session),
                    session.id,
                    session.screen_text()
                ))
            }
            "pty_send" => {
                let params: SendParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let session = manager
                    .get(&params.session_id)
                    .ok_or_else(|| McpError::invalid_params("unknown session_id", None))?;
                let mut input = params.input.into_bytes();
                if params.enter.unwrap_or(true) {
                    input.push(b'\r');
                }
                let writer = session.handle.writer_sender();
                writer
                    .send(input)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                settle(&session, params.wait_ms.unwrap_or(1500)).await;
                text_result(format!("{}\n\n{}", header(&session), session.screen_text()))
            }
            "pty_ctrl" => {
                let params: CtrlParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let session = manager
                    .get(&params.session_id)
                    .ok_or_else(|| McpError::invalid_params("unknown session_id", None))?;
                let bytes = ctrl_bytes(&params.key).ok_or_else(|| {
                    McpError::invalid_params(format!("unknown key {:?}", params.key), None)
                })?;
                let writer = session.handle.writer_sender();
                writer
                    .send(bytes)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                settle(&session, params.wait_ms.unwrap_or(1500)).await;
                text_result(format!("{}\n\n{}", header(&session), session.screen_text()))
            }
            "pty_screen" => {
                let params: SessionIdParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let session = manager
                    .get(&params.session_id)
                    .ok_or_else(|| McpError::invalid_params("unknown session_id", None))?;
                text_result(format!("{}\n\n{}", header(&session), session.screen_text()))
            }
            "pty_tail" => {
                let params: TailParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let session = manager
                    .get(&params.session_id)
                    .ok_or_else(|| McpError::invalid_params("unknown session_id", None))?;
                let max = params.max_bytes.unwrap_or(8000).min(64 * 1024);
                text_result(format!("{}\n\n{}", header(&session), session.tail_text(max)))
            }
            "pty_resize" => {
                let params: ResizeParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let session = manager
                    .get(&params.session_id)
                    .ok_or_else(|| McpError::invalid_params("unknown session_id", None))?;
                session.resize(params.cols, params.rows).map_err(internal)?;
                text_result(format!("{}", header(&session)))
            }
            "pty_list" => {
                let mut lines = vec!["id | cmd | size | status".to_string()];
                for session in manager.list() {
                    lines.push(header(&session));
                }
                text_result(lines.join("\n"))
            }
            "pty_kill" => {
                let params: SessionIdParams = serde_json::from_value(json!(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                match manager.kill(&params.session_id) {
                    Some(cmd) => text_result(format!("killed {} ({:?})", params.session_id, cmd)),
                    None => text_result(format!("{} not found", params.session_id)),
                }
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}
