//! codex-pty-mcp: interactive PTY sessions exposed over the Model Context
//! Protocol. The PTY/process layer is vendored from openai/codex
//! (codex-rs/utils/pty, Apache-2.0). The modules are mounted at the crate
//! root via #[path] so the vendored files' `crate::`-relative paths keep
//! resolving unchanged, byte-identical to upstream.

#[path = "codex_pty/process.rs"]
mod process;
#[path = "codex_pty/process_group.rs"]
mod process_group;
#[path = "codex_pty/pty.rs"]
mod pty;
#[path = "codex_pty/unix_io.rs"]
mod unix_io;

mod handler;
mod session;

use rmcp::ServiceExt;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let server = handler::PtyMcpServer::new();
    let running = server.serve(rmcp::transport::io::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
