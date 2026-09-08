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
    /// Set by the pump task when the PTY reader reaches EOF: the whole
    /// process tree is gone and no more output can arrive.
    eof: AtomicBool,
    /// Held for the session's lifetime to bound concurrent process creation.
    _permit: OwnedSemaphorePermit,
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
/// deadline expires. Process exit alone is not a drain signal: descendants
/// can still own the PTY and write after the direct child exits.
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
        } else if last_change.elapsed() >= Duration::from_millis(QUIET_MS) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
    }
}

pub struct SessionManager {
    sessions: StdMutex<HashMap<String, std::sync::Arc<Session>>>,
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
        // Reap sessions that are fully gone (exited AND PTY at EOF) to free
        // their slots. has_exited alone is not enough: background
        // descendants can still own the PTY and write after bash exits.
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dead: Vec<String> = sessions
                .iter()
                .filter(|(_, session)| session.handle.has_exited() && session.is_eof())
                .map(|(id, _)| id.clone())
                .collect();
            for id in dead {
                if let Some(session) = sessions.remove(&id) {
                    session.handle.terminate();
                }
            }
        }
        // Bound concurrent process creation, not just the table size: a
        // spawn burst must not launch processes only to kill them after.
        // The permit is released when the Session (and its pump) is dropped.
        let permit = std::sync::Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("too many sessions (max {MAX_SESSIONS})"))?;
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
        let spawned = pty::spawn_process(
            program,
            &args,
            &dir,
            &env,
            &None,
            size,
            &[],
        )
        .await?;

        let session = std::sync::Arc::new(Session {
            id: id.clone(),
            command: displayed,
            size: StdMutex::new(size),
            handle: spawned.session,
            screen: StdMutex::new(vt100::Parser::new(rows, cols, 1000)),
            tail: StdMutex::new(VecDeque::with_capacity(4096)),
            total: AtomicU64::new(0),
            eof: AtomicBool::new(false),
            _permit: permit,
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
        sessions.insert(id, std::sync::Arc::clone(&session));
        drop(sessions);
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<std::sync::Arc<Session>> {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(id).cloned())
    }

    pub fn list(&self) -> Vec<std::sync::Arc<Session>> {
        let mut out: Vec<std::sync::Arc<Session>> = self
            .sessions
            .lock()
            .map(|sessions| sessions.values().cloned().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill the process and forget the session. Returns its command line.
    pub fn kill(&self, id: &str) -> Option<String> {
        let session = {
            let mut sessions = self.sessions.lock().ok()?;
            sessions.remove(id)
        }?;
        session.handle.terminate();
        Some(session.command.clone())
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
