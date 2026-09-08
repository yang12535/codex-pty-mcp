//! Session state: one spawned PTY per session id, a vt100 screen emulator for
//! rendered-screen reads, and a small raw tail ring for non-TUI output.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::process::{ProcessHandle, TerminalSize};
use crate::pty;

/// Bytes of raw output kept per session for tail reads.
pub const TAIL_CAP: usize = 64 * 1024;
/// Maximum live sessions; a full table evicts exited sessions before failing.
const MAX_SESSIONS: usize = 64;
/// Initial quiet window before returning output after a spawn/send.
const QUIET_MS: u64 = 300;
const POLL_MS: u64 = 100;

/// Expand a lone `~` or a `~/`-prefixed path to $HOME; leave the path
/// untouched when HOME is unset.
fn expand_home(path: &str) -> String {
    if path != "~" && !path.starts_with("~/") {
        return path.to_string();
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{home}{}", &path[1..]),
        _ => path.to_string(),
    }
}

pub struct Session {
    pub id: String,
    pub command: String,
    pub size: StdMutex<TerminalSize>,
    pub handle: ProcessHandle,
    screen: StdMutex<vt100::Parser>,
    tail: StdMutex<VecDeque<u8>>,
    total: AtomicU64,
    /// Set after the pump drains the PTY output channel. Descendants may
    /// still exist, but none can produce further output through this reader.
    eof: AtomicBool,
}

impl Session {
    /// True once the PTY reader hit EOF (pump drained and stopped).
    pub fn is_eof(&self) -> bool {
        self.eof.load(Ordering::SeqCst)
    }

    fn feed(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut parser) = self.screen.lock() {
            parser.process(bytes);
        }
        if let Ok(mut tail) = self.tail.lock() {
            tail.extend(bytes.iter().copied());
            let overflow = tail.len().saturating_sub(TAIL_CAP);
            drop(tail.drain(..overflow));
        }
        self.total.fetch_add(bytes.len() as u64, Ordering::SeqCst);
    }

    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::SeqCst)
    }

    /// Render the current emulated screen as plain text (tmux capture-pane
    /// style, colors dropped).
    pub fn screen_text(&self) -> String {
        if let Ok(parser) = self.screen.lock() {
            return parser.screen().contents();
        }
        String::new()
    }

    /// Last `max` bytes of raw output with ANSI escape sequences stripped.
    pub fn tail_text(&self, max: usize) -> String {
        let bytes: Vec<u8> = match self.tail.lock() {
            Ok(tail) => {
                let start = tail.len().saturating_sub(max);
                tail.iter().skip(start).copied().collect()
            }
            Err(_) => Vec::new(),
        };
        strip_ansi(&bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.handle.resize(TerminalSize { rows, cols })?;
        if let Ok(mut parser) = self.screen.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        if let Ok(mut size) = self.size.lock() {
            *size = TerminalSize { rows, cols };
        }
        Ok(())
    }
}

/// Wait until output goes quiet (no new bytes for QUIET_MS) or the overall
/// deadline expires, returning earlier once the child exits and the pump
/// drains. Process exit alone is not a drain signal: descendants can still
/// own the PTY and write after the direct child exits.
pub async fn settle(session: &Session, wait_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(QUIET_MS * 2));
    let mut last_total = session.total_bytes();
    let mut last_change = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        let now = session.total_bytes();
        if now != last_total {
            last_total = now;
            last_change = Instant::now();
        } else if (session.handle.has_exited() && session.is_eof())
            || last_change.elapsed() >= Duration::from_millis(QUIET_MS)
        {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
    }
}

/// Capacity follows membership in the manager, not outstanding screen reads
/// or the output pump's Arc. Removing an entry terminates its process before
/// releasing the permit, including when the manager itself is dropped.
struct ManagedSession {
    session: std::sync::Arc<Session>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ManagedSession {
    fn drop(&mut self) {
        self.session.handle.terminate();
    }
}

pub struct SessionManager {
    sessions: StdMutex<HashMap<String, ManagedSession>>,
    counter: AtomicU64,
    permits: std::sync::Arc<Semaphore>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self {
            sessions: StdMutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            permits: std::sync::Arc::new(Semaphore::new(MAX_SESSIONS)),
        }
    }
}

impl SessionManager {
    /// Spawn `command` in a new PTY session (or an interactive login shell
    /// when no command is given).
    pub async fn spawn(
        &self,
        command: Option<String>,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<std::sync::Arc<Session>> {
        let id = format!("pty-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        // Reserve before creating the process, including concurrent spawns.
        // Keep completed sessions readable until capacity is exhausted; only
        // then evict one entry whose direct child exited AND pump drained.
        let permit = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match std::sync::Arc::clone(&self.permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    let dead = sessions
                        .iter()
                        .find(|(_, entry)| {
                            entry.session.handle.has_exited() && entry.session.is_eof()
                        })
                        .map(|(id, _)| id.clone());
                    if let Some(id) = dead {
                        drop(sessions.remove(&id));
                    }
                    std::sync::Arc::clone(&self.permits)
                        .try_acquire_owned()
                        .map_err(|_| anyhow::anyhow!("too many sessions (max {MAX_SESSIONS})"))?
                }
            }
        };
        // On spawn failure the local permit is dropped, restoring capacity.
        let dir = cwd
            .map(|cwd| PathBuf::from(expand_home(&cwd)))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let (program, args): (&str, Vec<String>) = match command {
            Some(ref cmd) if !cmd.trim().is_empty() => ("bash", vec!["-lc".into(), cmd.clone()]),
            _ => ("bash", vec!["-l".into()]),
        };
        let displayed = match command {
            Some(cmd) => cmd,
            None => "bash -l".into(),
        };

        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("TERM".into(), "xterm-256color".into());

        let size = TerminalSize { rows, cols };
        let spawned = pty::spawn_process(program, &args, &dir, &env, &None, size, &[]).await?;

        let session = std::sync::Arc::new(Session {
            id: id.clone(),
            command: displayed,
            size: StdMutex::new(size),
            handle: spawned.session,
            screen: StdMutex::new(vt100::Parser::new(rows, cols, 1000)),
            tail: StdMutex::new(VecDeque::with_capacity(4096)),
            total: AtomicU64::new(0),
            eof: AtomicBool::new(false),
        });

        // Pump PTY output into the emulator + tail for as long as it flows.
        let pump_session = std::sync::Arc::clone(&session);
        let mut stdout_rx: mpsc::Receiver<Vec<u8>> = spawned.stdout_rx;
        tokio::spawn(async move {
            while let Some(chunk) = stdout_rx.recv().await {
                pump_session.feed(&chunk);
            }
            pump_session.eof.store(true, Ordering::SeqCst);
        });

        // A permit is held, so a slot is guaranteed; insertion never fails.
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.insert(
            id,
            ManagedSession {
                session: std::sync::Arc::clone(&session),
                _permit: permit,
            },
        );
        drop(sessions);
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<std::sync::Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .map(|entry| std::sync::Arc::clone(&entry.session))
    }

    pub fn list(&self) -> Vec<std::sync::Arc<Session>> {
        let mut out: Vec<std::sync::Arc<Session>> = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|entry| std::sync::Arc::clone(&entry.session))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill the process and forget the session. Returns its command line.
    pub fn kill(&self, id: &str) -> Option<String> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = sessions.remove(id)?;
        let command = entry.session.command.clone();
        // Drop under the same lock used by reservation: a replacement cannot
        // observe a removed entry whose permit has not yet been released.
        drop(entry);
        Some(command)
    }
}

/// Strip ANSI/OSC escape sequences at the byte level, keeping everything else
/// (including multi-byte UTF-8) so non-ASCII output survives.
fn strip_ansi(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1b && i + 1 < input.len() {
            match input[i + 1] {
                b'[' => {
                    let mut j = i + 2;
                    while j < input.len() && !(0x40..=0x7e).contains(&input[j]) {
                        j += 1;
                    }
                    i = (j + 1).min(input.len());
                    continue;
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    let mut j = i + 2;
                    while j < input.len() {
                        if input[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if input[j] == 0x1b && j + 1 < input.len() && input[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                0x20..=0x2f => {
                    // ESC + intermediate bytes (0x20..=0x2f) + one final
                    // byte (0x30..=0x7e), e.g. `ESC ( 0` selects the DEC
                    // line-drawing charset. Only consume bytes that match
                    // the grammar: a truncated/malformed sequence must not
                    // eat a payload byte (newline, UTF-8 lead, ...).
                    let mut j = i + 1;
                    while j < input.len() && (0x20..=0x2f).contains(&input[j]) {
                        j += 1;
                    }
                    if j < input.len() && (0x30..=0x7e).contains(&input[j]) {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        match b {
            b'\n' | b'\t' | 0x20..=0x7e => out.push(b),
            0x80..=0xff => out.push(b),
            _ => {}
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn manager(capacity: usize) -> SessionManager {
        SessionManager {
            permits: Arc::new(Semaphore::new(capacity)),
            ..SessionManager::default()
        }
    }

    async fn spawn(manager: &SessionManager, command: &str) -> Arc<Session> {
        manager
            .spawn(Some(command.into()), None, 80, 24)
            .await
            .unwrap()
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process state did not arrive");
    }

    #[tokio::test]
    async fn completed_sessions_remain_readable_below_capacity() {
        let manager = manager(2);
        let first = spawn(&manager, "printf retained-output").await;
        wait_until(|| first.handle.has_exited() && first.is_eof()).await;
        let second = spawn(&manager, "printf second-output").await;
        let retained = manager
            .get(&first.id)
            .expect("completed session was evicted");
        assert_eq!(retained.tail_text(100), "retained-output");
        assert!(retained.screen_text().contains("retained-output"));
        manager.kill(&first.id);
        manager.kill(&second.id);
    }

    #[tokio::test]
    async fn capacity_pressure_evicts_completed_entry_even_with_external_reference() {
        let manager = manager(1);
        let first = spawn(&manager, "printf finished").await;
        wait_until(|| first.handle.has_exited() && first.is_eof()).await;
        let second = spawn(&manager, "exec sleep 30").await;
        assert!(manager.get(&first.id).is_none());
        assert_eq!(first.tail_text(100), "finished");
        assert_eq!(manager.list().len(), 1);
        manager.kill(&second.id);
    }

    #[tokio::test]
    async fn kill_releases_capacity_before_returning_even_with_external_reference() {
        let manager = manager(1);
        let first = spawn(&manager, "exec sleep 30").await;
        assert!(manager.kill(&first.id).is_some());
        let replacement = spawn(&manager, "exec sleep 30").await;
        assert!(manager.get(&first.id).is_none());
        assert_eq!(manager.list().len(), 1);
        manager.kill(&replacement.id);
    }

    #[tokio::test]
    async fn failed_spawn_returns_reserved_capacity() {
        let manager = manager(1);
        // An embedded NUL is rejected by process creation, after reservation.
        let result = manager.spawn(Some("echo \0".into()), None, 80, 24).await;
        assert!(result.is_err());
        let session = spawn(&manager, "exec sleep 30").await;
        manager.kill(&session.id);
    }

    #[tokio::test]
    async fn dropping_manager_terminates_sessions_with_outstanding_references() {
        let manager = manager(1);
        let session = spawn(&manager, "exec sleep 30").await;
        drop(manager);
        wait_until(|| session.handle.has_exited()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_spawns_respect_capacity() {
        let manager = Arc::new(manager(3));
        let barrier = Arc::new(tokio::sync::Barrier::new(12));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..12 {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            tasks.spawn(async move {
                barrier.wait().await;
                manager
                    .spawn(Some("exec sleep 30".into()), None, 80, 24)
                    .await
            });
        }
        let mut accepted = Vec::new();
        let mut rejected = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(session) => accepted.push(session),
                Err(error) => {
                    assert!(error.to_string().contains("too many sessions"));
                    rejected += 1;
                }
            }
            assert!(manager.list().len() <= 3);
        }
        assert_eq!(accepted.len(), 3);
        assert_eq!(rejected, 9);
        for session in accepted {
            manager.kill(&session.id);
        }
    }

    #[tokio::test]
    async fn exited_wrapper_with_open_pty_is_not_evicted() {
        let manager = manager(1);
        let session = spawn(&manager, "trap '' HUP; (sleep 30; echo late) &").await;
        wait_until(|| session.handle.has_exited()).await;
        assert!(!session.is_eof());
        let result = manager.spawn(Some("true".into()), None, 80, 24).await;
        assert!(result.is_err());
        assert!(manager.get(&session.id).is_some());
        manager.kill(&session.id);
    }

    #[tokio::test]
    async fn exited_and_drained_session_skips_full_quiet_window() {
        let manager = manager(1);
        let session = spawn(&manager, "printf fast-output").await;
        wait_until(|| session.handle.has_exited() && session.is_eof()).await;
        let result = tokio::time::timeout(Duration::from_millis(250), settle(&session, 1500)).await;
        manager.kill(&session.id);
        result.expect("EOF should settle after one 100 ms poll, before the 300 ms quiet window");
        assert_eq!(session.screen_text(), "fast-output");
    }

    #[test]
    fn escape_intermediates_and_malformed_payloads() {
        assert_eq!(strip_ansi(b"\x1b(0ab\x1b(Bc\n"), "abc\n");
        assert_eq!(strip_ansi(b"a\x1b$(Cb"), "ab");
        assert_eq!(strip_ansi(b"a\x1b(\nb\n"), "a\nb\n");
        assert_eq!(strip_ansi("a\x1b(中文".as_bytes()), "a中文");
        assert_eq!(strip_ansi(b"a\x1b$("), "a");
    }
}
