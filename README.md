# codex-pty-mcp

[English](README_EN.md) | 中文

给你的 AI Agent 一个**真·伪终端（PTY）**：跑 `htop`、`vim`、REPL、交互式安装器、
`sudo` 密码提示——一切需要 TTY 的程序，并把终端画面渲染成纯文本读回来。

PTY/进程层**逐字节提取自 [openai/codex](https://github.com/openai/codex)**
（`codex-rs/utils/pty`，Apache-2.0）——也就是 Codex CLI 生产环境在用的那套
久经考验的代码。上面套了一层薄的 MCP 服务层（用的是 codex 同款官方
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK），再加一个
`vt100` 屏幕仿真器，把 TUI 画面转成可读文本，效果类似 `tmux capture-pane -p`。

```
┌─────────────────────────────────────────────┐
│ MCP 客户端（ZCode / Claude / 任意 Agent）    │
└──────────────────┬──────────────────────────┘
        stdio JSON-RPC（rmcp 3.2）
┌──────────────────┴──────────────────────────┐
│ 胶水层：8 个 pty_* 工具、会话管理            │
│ vt100 屏幕仿真 + ANSI 剥离的尾部输出         │
├─────────────────────────────────────────────┤
│ codex-utils-pty（提取自 openai/codex）       │
│ portable-pty · 进程组管理 · 异步 I/O         │
└─────────────────────────────────────────────┘
```

## 为什么是提取 codex

PTY 这东西的难点全在细节：进程组硬杀、PTY 关闭时的 EIO/EOF 区分、stdin
关闭的 VEOF 序列、exec 前的 fd 清扫……这些坑 codex 的海量用户都替我们踩平了。
自己写等于重踩一年，用零星小项目等于替作者踩。本仓库把这些生产级代码原样
搬来（文件与上游保持逐字节一致，见
[src/codex_pty/VENDORED.md](src/codex_pty/VENDORED.md)），自己只写了一小层
胶水——需要验证的面积非常小。

## 构建

```sh
cargo build --release
# 产物：target/release/codex-pty-mcp
```

## 注册到 MCP 客户端

ZCode（`~/.zcode/cli/config.json`）：

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

通用客户端（Claude 风格 `mcpServers`）：

```json
{
  "mcpServers": {
    "codex-pty-mcp": {
      "command": "/path/to/codex-pty-mcp/target/release/codex-pty-mcp"
    }
  }
}
```

注册后重启客户端，MCP server 在会话启动时自动连接。

## 工具一览

| 工具 | 用途 |
|---|---|
| `pty_spawn` | 在新 PTY 会话里启动命令（`bash -lc <command>`）或交互式登录 shell；返回渲染后的屏幕 |
| `pty_send` | 输入文本（可选回车），等输出稳定后返回屏幕 |
| `pty_ctrl` | 发送特殊键：`c-c`、`c-d`、`enter`、`esc`、`up/down/left/right`、`pageup/pagedown` 等 |
| `pty_screen` | 读取当前渲染屏幕（纯文本，类似 tmux capture-pane） |
| `pty_tail` | 读取最近 N 字节原始输出并剥离 ANSI 转义（适合非 TUI 命令，UTF-8 完好） |
| `pty_resize` | 按字符格数调整终端尺寸 |
| `pty_list` | 列出所有会话的命令、尺寸、退出状态 |
| `pty_kill` | 杀掉会话的整个进程组并移除会话 |

经验法则：TUI 程序 → 读 `pty_screen`；普通命令 / 长输出 → `pty_tail`。
会话跨工具调用持续存活，可以先起一个 REPL 然后反复往里输入。

## 测试

Rust 测试覆盖会话保留、容量释放、并发上限、PTY EOF 和转义序列。
`scripts/test_pty_mcp.py` 默认验证当前仓库的 release 二进制，通过 stdio
MCP JSON-RPC 检查输出正文、延迟输出、REPL、退出码及 80 次并发启动时的
64 会话上限。安装了 `htop` 时还会验证真实 TUI 的渲染与退出。

```sh
cargo test --locked
cargo build --release --locked
python3 scripts/test_pty_mcp.py
# 也可指定待测二进制：
python3 scripts/test_pty_mcp.py --binary /path/to/codex-pty-mcp
```

GitHub Actions 在每个 PR 和 main 更新时运行这些检查。测试结束或失败时
会清理本次测试创建的会话。

## 许可证

Apache-2.0（见 [LICENSE](LICENSE)）。`src/codex_pty/` 下的 codex 提取文件
保留上游出处（[VENDORED.md](src/codex_pty/VENDORED.md)、
[LICENSE.upstream](src/codex_pty/LICENSE.upstream)），另见
[NOTICE.md](NOTICE.md)。

与 OpenAI 无隶属关系。这是一个独立的个人工具向提取；PTY 层的全部功劳
属于 codex 的作者们。
