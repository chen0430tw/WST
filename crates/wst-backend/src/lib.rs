pub mod conpty;
pub use conpty::ConPtyBackend;

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use thiserror::Error;
use wst_protocol::{BackendKind, ExecRequest, OutputChunk, SessionEvent, SessionId, TaskId, TaskStatus};

/// Decode a byte slice using the system OEM code page (e.g. Big5/950 on Traditional Chinese Windows).
/// Falls back to UTF-8 lossy if the code page is unknown.
fn decode_oem_line(bytes: &[u8]) -> String {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::GetOEMCP;
        let cp = unsafe { GetOEMCP() };
        let encoding = match cp {
            950 => encoding_rs::BIG5,
            936 | 54936 => encoding_rs::GBK,
            932 => encoding_rs::SHIFT_JIS,
            949 => encoding_rs::EUC_KR,
            _ => {
                return String::from_utf8_lossy(bytes)
                    .trim_end_matches(|c: char| c == '\r' || c == '\n')
                    .to_string();
            }
        };
        let (decoded, _enc, _had_errors) = encoding.decode(bytes);
        decoded.trim_end_matches(|c: char| c == '\r' || c == '\n').to_string()
    }
    #[cfg(not(windows))]
    {
        String::from_utf8_lossy(bytes)
            .trim_end_matches(|c: char| c == '\r' || c == '\n')
            .to_string()
    }
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("backend generic error: {0}")]
    Other(String),
}

pub trait Backend: Send {
    fn kind(&self) -> BackendKind;
    fn spawn_session(&mut self) -> Result<SessionId, BackendError>;
    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError>;
    fn interrupt(&mut self, session: SessionId, task: TaskId) -> Result<(), BackendError>;
    fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError>;
    fn reset(&mut self);
    fn active_session_ids(&self) -> Vec<SessionId>;
}

/// One decoded line coming from a child's stdout/stderr reader thread.
struct LineMsg {
    is_stderr: bool,
    text: String,
}

struct Task {
    child: Option<Child>,
    status: TaskStatus,
    output_buffer: Vec<String>,
    error_buffer: Vec<String>,
    /// Lines streamed from the reader threads; drained non-blocking by `poll_events`.
    rx: Option<Receiver<LineMsg>>,
    /// Exit code once the process has been observed to exit.
    exit_status: Option<i32>,
    /// True once all reader threads have finished (channel disconnected).
    readers_done: bool,
}

/// Spawn a background thread that reads `reader` line by line, decodes each line
/// with the OEM code page, and sends it over `tx`. The thread exits on EOF, which
/// drops its `tx` clone.
fn spawn_reader<R: Read + Send + 'static>(reader: R, is_stderr: bool, tx: Sender<LineMsg>) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
            let line = decode_oem_line(&buf);
            if tx.send(LineMsg { is_stderr, text: line }).is_err() {
                break; // receiver gone
            }
            buf.clear();
        }
        // tx dropped here -> contributes to channel disconnect
    });
}

/// Build a running `Task` from a spawned child, wiring up streaming reader threads.
fn start_task(mut child: Child) -> Task {
    let (tx, rx) = channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, false, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, true, tx.clone());
    }
    // Drop the original sender so the channel disconnects once BOTH reader
    // threads finish (EOF). If there were no readers at all, the channel is
    // already disconnected and the task finalizes on the first poll.
    drop(tx);

    Task {
        child: Some(child),
        status: TaskStatus::Running,
        output_buffer: Vec::new(),
        error_buffer: Vec::new(),
        rx: Some(rx),
        exit_status: None,
        readers_done: false,
    }
}

/// Build a task that immediately reports a single error line and finishes.
fn start_failed_task(msg: String) -> Task {
    let (tx, rx) = channel();
    let _ = tx.send(LineMsg { is_stderr: true, text: msg });
    drop(tx);

    Task {
        child: None,
        status: TaskStatus::Running,
        output_buffer: Vec::new(),
        error_buffer: Vec::new(),
        rx: Some(rx),
        exit_status: Some(1),
        readers_done: false,
    }
}

/// Non-blocking poll shared by every pipe-based backend (Cmd / Pwsh / Cygctl).
///
/// Drains whatever lines the reader threads have produced so far, checks whether
/// the process has exited, and only finalizes a task once BOTH the process has
/// exited AND all reader threads have drained (channel disconnected) — so no
/// output is lost even if the process exits a hair before the readers finish.
fn poll_sessions(
    sessions: &mut HashMap<SessionId, HashMap<TaskId, Task>>,
    session: SessionId,
) -> Vec<SessionEvent> {
    let mut result = Vec::new();
    let mut completed_tasks = Vec::new();

    if let Some(tasks) = sessions.get_mut(&session) {
        for (&task_id, task) in tasks.iter_mut() {
            // 1. Drain any lines produced since the last poll (non-blocking).
            if let Some(rx) = &task.rx {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => {
                            if msg.is_stderr {
                                task.error_buffer.push(msg.text.clone());
                            } else {
                                task.output_buffer.push(msg.text.clone());
                            }
                            result.push(SessionEvent::Output(OutputChunk {
                                task_id,
                                is_stderr: msg.is_stderr,
                                text: msg.text,
                            }));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            task.readers_done = true;
                            break;
                        }
                    }
                }
            } else {
                task.readers_done = true;
            }

            // 2. Observe process exit (non-blocking).
            if task.exit_status.is_none() {
                if let Some(child) = task.child.as_mut() {
                    if let Ok(Some(exit_status)) = child.try_wait() {
                        task.exit_status = Some(if exit_status.success() {
                            0
                        } else {
                            exit_status.code().unwrap_or(1)
                        });
                    }
                }
            }

            // 3. Finalize only when the process is gone AND all output is drained.
            if task.readers_done && task.exit_status.is_some() {
                let status = TaskStatus::Exited(task.exit_status.unwrap());
                task.status = status;
                result.push(SessionEvent::TaskUpdated {
                    task_id,
                    status,
                });
                completed_tasks.push(task_id);
            }
        }

        for task_id in completed_tasks {
            tasks.remove(&task_id);
        }
    }

    result
}

pub struct CmdBackend {
    next_session: SessionId,
    next_task: TaskId,
    sessions: HashMap<SessionId, HashMap<TaskId, Task>>,
}

impl CmdBackend {
    pub fn new() -> Self {
        Self {
            next_session: 1,
            next_task: 1,
            sessions: HashMap::new(),
        }
    }
}

impl Default for CmdBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for CmdBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Cmd
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, HashMap::new());
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let task_id = self.next_task;
        self.next_task += 1;

        let child = Command::new("cmd")
            .args(["/C", &req.command_line])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let task = start_task(child);

        self.sessions
            .entry(session)
            .or_insert_with(HashMap::new)
            .insert(task_id, task);

        Ok(task_id)
    }

    fn interrupt(&mut self, _session: SessionId, _task: TaskId) -> Result<(), BackendError> {
        Ok(())
    }

    fn reset(&mut self) {
        self.sessions.clear();
        self.next_session = 1;
        self.next_task = 1;
    }

    fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError> {
        Ok(poll_sessions(&mut self.sessions, session))
    }
}

pub struct PwshBackend {
    next_session: SessionId,
    next_task: TaskId,
    sessions: HashMap<SessionId, HashMap<TaskId, Task>>,
}

impl PwshBackend {
    pub fn new() -> Self {
        Self {
            next_session: 1,
            next_task: 1,
            sessions: HashMap::new(),
        }
    }
}

impl Default for PwshBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for PwshBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Pwsh
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, HashMap::new());
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let task_id = self.next_task;
        self.next_task += 1;

        let child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &req.command_line])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let task = start_task(child);

        self.sessions
            .entry(session)
            .or_insert_with(HashMap::new)
            .insert(task_id, task);

        Ok(task_id)
    }

    fn interrupt(&mut self, _session: SessionId, _task: TaskId) -> Result<(), BackendError> {
        Ok(())
    }

    fn reset(&mut self) {
        self.sessions.clear();
        self.next_session = 1;
        self.next_task = 1;
    }

    fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError> {
        Ok(poll_sessions(&mut self.sessions, session))
    }
}

pub struct CygctlBackend {
    pub cygctl_path: String,
    next_session: SessionId,
    next_task: TaskId,
    sessions: HashMap<SessionId, HashMap<TaskId, Task>>,
}

impl CygctlBackend {
    pub fn new(cygctl_path: impl Into<String>) -> Self {
        Self {
            cygctl_path: cygctl_path.into(),
            next_session: 1,
            next_task: 1,
            sessions: HashMap::new(),
        }
    }
}

impl Backend for CygctlBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Cygctl
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, HashMap::new());
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let task_id = self.next_task;
        self.next_task += 1;

        // Use cygctl to execute the command: cygctl exec <command>
        let child = Command::new(&self.cygctl_path)
            .args(["exec", &req.command_line])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let task = match child {
            Ok(c) => start_task(c),
            Err(e) => start_failed_task(format!("cygctl error: {}", e)),
        };

        self.sessions
            .entry(session)
            .or_insert_with(HashMap::new)
            .insert(task_id, task);

        Ok(task_id)
    }

    fn interrupt(&mut self, _session: SessionId, _task: TaskId) -> Result<(), BackendError> {
        Ok(())
    }

    fn reset(&mut self) {
        self.sessions.clear();
        self.next_session = 1;
        self.next_task = 1;
    }

    fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError> {
        Ok(poll_sessions(&mut self.sessions, session))
    }
}
