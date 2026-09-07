# Vendored from openai/codex

Files in this directory (pty.rs, process.rs, process_group.rs, unix_io.rs) are
vendored verbatim from https://github.com/openai/codex (codex-rs/utils/pty),
commit 16ff14c, licensed under the Apache License 2.0
(see LICENSE.upstream; upstream NOTICE: "OpenAI Codex, Copyright 2025 OpenAI").

Local modifications for embedding as crate-root modules are tracked by diffing
against the upstream commit. The MCP/service layer of this repository is not
derived from codex.
