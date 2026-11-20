//! ConPTY backend — runs a persistent shell via wst-conpty-broker.exe.
//!
//! wst-ui has an attached conhost console (needed by crossterm). Spawning
//! a ConPTY child directly from a console process causes STATUS_DLL_INIT_FAILED
//! (0xc0000142) because the child's DLL init sees two conflicting console
//! connections. The fix: delegate ConPTY creation to wst-conpty-broker.exe,
//! which is started with DETACHED_PROCESS (no console) so there is no conflict.
//! wst-ui communicates with the broker via stdin/stdout pipes.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver};

use wst_protocol::{BackendKind, ExecRequest, OutputChunk, SessionEvent, SessionId, TaskId};

use crate::{Backend, BackendError};

// ── Line assembly ─────────────────────────────────────────────────────────────

/// How long output must be idle before a trailing line without '\n'
/// (e.g. a shell prompt) is emitted on its own.
const PARTIAL_LINE_FLUSH: std::time::Duration = std::time::Duration::from_millis(100);

/// Removes complete lines from `pending` and returns them decoded.
///
/// Splitting happens on the raw bytes: '\n' never occurs inside a multi-byte
/// UTF-8 sequence, so a character split across two reads stays intact in the
/// leftover bytes. Every UI event is one display line, so a trailing partial
/// line is kept for the next poll unless `flush_partial` is set.
fn take_lines(pending: &mut Vec<u8>, flush_partial: bool) -> Vec<String> {
    let decode = |b: &[u8]| {
        let b = b.strip_suffix(b"\r").unwrap_or(b);
        String::from_utf8_lossy(b).into_owned()
    };
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(off) = pending[start..].iter().position(|&b| b == b'\n') {
        lines.push(decode(&pending[start..start + off]));
        start += off + 1;
    }
    pending.drain(..start);
    if flush_partial && !pending.is_empty() {
        lines.push(decode(pending));
        pending.clear();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::take_lines;

    #[test]
    fn keeps_partial_line_until_flush() {
        let mut p = b"line one\r\nhalf".to_vec();
        assert_eq!(take_lines(&mut p, false), vec!["line one"]);
        assert_eq!(p, b"half");
        p.extend_from_slice(b" done\r\n");
        assert_eq!(take_lines(&mut p, false), vec!["half done"]);
        assert!(p.is_empty());
    }

    #[test]
    fn utf8_char_split_across_reads() {
        let bytes = "中文测试\r\n".as_bytes();
        let mut p = bytes[..4].to_vec(); // cuts "文" in the middle
        assert!(take_lines(&mut p, false).is_empty());
        p.extend_from_slice(&bytes[4..]);
        assert_eq!(take_lines(&mut p, false), vec!["中文测试"]);
    }

    #[test]
    fn flushes_prompt_when_idle() {
        let mut p = b"PS C:\\> ".to_vec();
        assert!(take_lines(&mut p, false).is_empty());
        assert_eq!(take_lines(&mut p, true), vec!["PS C:\\> "]);
        assert!(p.is_empty());
    }
}

// ── Session state ─────────────────────────────────────────────────────────────

struct Session {
    /// Write end of the pipe → broker stdin → PTY input.
    input_write: std::fs::File,
    /// Receives raw VT bytes from the background reader thread.
    output_rx: Receiver<Vec<u8>>,
    /// Leftover bytes from the last poll (partial UTF-8 line, no trailing \n).
    pending: Vec<u8>,
    /// When the last output chunk arrived; a partial line is flushed only
    /// after the output has been idle for PARTIAL_LINE_FLUSH.
    last_rx: std::time::Instant,
    /// Most recently issued task id.
    current_task: TaskId,
}

// ── ConPtyBackend ─────────────────────────────────────────────────────────────

pub struct ConPtyBackend {
    shell: String,
    cols: u16,
    rows: u16,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    next_task: TaskId,
}

impl ConPtyBackend {
    pub fn new(shell: impl Into<String>) -> Self {
        Self {
            shell: shell.into(),
            cols: 220,
            rows: 50,
            sessions: HashMap::new(),
            next_session: 1,
            next_task: 1,
        }
    }

    pub fn cmd() -> Self { Self::new("cmd.exe") }
    pub fn pwsh() -> Self { Self::new("powershell.exe") }
}

// ── Session spawner ───────────────────────────────────────────────────────────

fn spawn_session(shell: &str, cols: u16, rows: u16) -> Result<Session, BackendError> {
    use std::io::Read;
    use std::os::windows::io::IntoRawHandle;
    use std::os::windows::process::CommandExt;

    // Locate wst-conpty-broker.exe next to the current executable.
    let exe = std::env::current_exe()
        .map_err(|e| BackendError::Other(format!("current_exe failed: {}", e)))?;
    let broker_path = exe.parent()
        .map(|d| d.join("wst-conpty-broker.exe"))
        .filter(|p| p.exists())
        .ok_or_else(|| {
            let candidate = exe.parent()
                .map(|d| d.join("wst-conpty-broker.exe").to_string_lossy().to_string())
                .unwrap_or_else(|| "?".to_string());
            BackendError::Other(format!(
                "wst-conpty-broker.exe not found at {:?}", candidate
            ))
        })?;

    // DETACHED_PROCESS: broker starts with no console → no ConPTY conflict.
    const DETACHED_PROCESS: u32 = 0x00000008;

    let mut child = std::process::Command::new(&broker_path)
        .arg(shell)
        .arg(cols.to_string())
        .arg(rows.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map_err(|e| BackendError::Other(format!("spawn broker: {}", e)))?;

    let broker_stdin = child.stdin.take()
        .ok_or_else(|| BackendError::Other("no broker stdin".into()))?;
    let broker_stdout = child.stdout.take()
        .ok_or_else(|| BackendError::Other("no broker stdout".into()))?;

    // Background reader: broker stdout → channel
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut output_rd = broker_stdout;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match output_rd.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() { break; }
                }
            }
        }
    });

    // Convert ChildStdin → File via raw handle
    #[cfg(windows)]
    let input_write = {
        use std::os::windows::io::FromRawHandle;
        let raw = broker_stdin.into_raw_handle();
        unsafe { std::fs::File::from_raw_handle(raw) }
    };
    #[cfg(not(windows))]
    let input_write = {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        unsafe { std::fs::File::from_raw_fd(broker_stdin.into_raw_fd()) }
    };

    // Keep child alive (don't drop it) — store pid so we can track it.
    // Leak the Child; the broker exits when we drop input_write (stdin closes).
    std::mem::forget(child);

    Ok(Session {
        input_write,
        output_rx: rx,
        pending: Vec::new(),
        last_rx: std::time::Instant::now(),
        current_task: 0,
    })
}

// ── Backend impl ──────────────────────────────────────────────────────────────

impl Backend for ConPtyBackend {
    fn kind(&self) -> BackendKind { BackendKind::ConPty }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let session = spawn_session(&self.shell, self.cols, self.rows)?;
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, session);
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let sess = self.sessions.get_mut(&session)
            .ok_or_else(|| BackendError::Other("session not found".into()))?;

        let task_id = self.next_task;
        self.next_task += 1;
        sess.current_task = task_id;

        let mut line = req.command_line;
        // Enter key sends CR only; an extra LF makes PSReadLine open a
        // ">>" continuation line after every command.
        line.push('\r');
        sess.input_write.write_all(line.as_bytes()).map_err(BackendError::Io)?;
        sess.input_write.flush().map_err(BackendError::Io)?;

        Ok(task_id)
    }

    fn interrupt(&mut self, session: SessionId, _task: TaskId) -> Result<(), BackendError> {
        if let Some(sess) = self.sessions.get_mut(&session) {
            let _ = sess.input_write.write_all(&[0x03]);
            let _ = sess.input_write.flush();
        }
        Ok(())
    }

    fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError> {
        let mut events = Vec::new();
        let sess = match self.sessions.get_mut(&session) {
            Some(s) => s,
            None => return Ok(events),
        };

        let now = std::time::Instant::now();
        while let Ok(chunk) = sess.output_rx.try_recv() {
            sess.pending.extend_from_slice(&chunk);
            sess.last_rx = now;
        }

        let flush_partial = now.duration_since(sess.last_rx) >= PARTIAL_LINE_FLUSH;
        let task_id = sess.current_task;
        for line in take_lines(&mut sess.pending, flush_partial) {
            events.push(SessionEvent::Output(OutputChunk {
                task_id,
                is_stderr: false,
                text: line,
            }));
        }

        Ok(events)
    }

    fn reset(&mut self) {
        self.sessions.clear();
        self.next_session = 1;
        self.next_task = 1;
    }
}

// ── Non-Windows stub ──────────────────────────────────────────────────────────

#[cfg(not(windows))]
fn spawn_session(_shell: &str, _cols: u16, _rows: u16) -> Result<Session, BackendError> {
    Err(BackendError::Other("ConPTY not supported on this platform".into()))
}
