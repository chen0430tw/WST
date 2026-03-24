pub mod conpty;
pub use conpty::ConPtyBackend;

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, Command, Stdio};

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

struct Task {
    child: Option<Child>,
    status: TaskStatus,
    output_buffer: Vec<String>,
    error_buffer: Vec<String>,
    stdout_reader: Option<BufReader<std::process::ChildStdout>>,
    stderr_reader: Option<BufReader<std::process::ChildStderr>>,
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


        let mut child = Command::new("cmd")
            .args(["/C", &req.command_line])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Take stdout and stderr to prevent buffer from filling up
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take().map(BufReader::new);

        let task = Task {
            child: Some(child),
            status: TaskStatus::Running,
            output_buffer: Vec::new(),
            error_buffer: Vec::new(),
            stdout_reader: stdout,
            stderr_reader: stderr,
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
        let mut result = Vec::new();
        let mut completed_tasks = Vec::new();

        if let Some(tasks) = self.sessions.get_mut(&session) {
                for (&task_id, task) in tasks.iter_mut() {
                // Read all available output from stdout/stderr
                // This prevents buffer deadlock and shows output immediately
                if let Some(ref mut reader) = task.stdout_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.output_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: false,
                            text: line,
                        }));
                        buf.clear();
                    }
                }
                if let Some(ref mut reader) = task.stderr_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.error_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: true,
                            text: line,
                        }));
                        buf.clear();
                    }
                }

                if let Some(mut child) = task.child.take() {
                    match child.try_wait() {
                        Ok(Some(exit_status)) => {

                            // Read any remaining output after process exits
                            if let Some(mut reader) = task.stdout_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    if !line.is_empty() {
                                        task.output_buffer.push(line.clone());
                                        result.push(SessionEvent::Output(OutputChunk {
                                            task_id: task_id,
                                            is_stderr: false,
                                            text: line,
                                        }));
                                    }
                                    buf.clear();
                                }
                            }
                            if let Some(mut reader) = task.stderr_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    if !line.is_empty() {
                                        task.error_buffer.push(line.clone());
                                        result.push(SessionEvent::Output(OutputChunk {
                                            task_id: task_id,
                                            is_stderr: true,
                                            text: line,
                                        }));
                                    }
                                    buf.clear();
                                }
                            }

                            let status = if exit_status.success() {
                                TaskStatus::Exited(0)
                            } else {
                                TaskStatus::Exited(exit_status.code().unwrap_or(1))
                            };

                            task.status = status;

                            result.push(SessionEvent::TaskUpdated {
                                task_id: task_id,
                                status,
                            });

                            // Mark task for removal
                            completed_tasks.push(task_id);
                        }
                        Ok(None) => {
                            // Still running, put child back
                            task.child = Some(child);
                        }
                        Err(_) => {
                            task.child = Some(child);
                        }
                    }
                } else if task.status == TaskStatus::Running {
                    // Process was removed but still marked as running - should not happen
                }
            }

            // Remove completed tasks
            for task_id in completed_tasks {
                tasks.remove(&task_id);
            }
        }

        Ok(result)
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

        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &req.command_line])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Take stdout and stderr to prevent buffer from filling up
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take().map(BufReader::new);

        let task = Task {
            child: Some(child),
            status: TaskStatus::Running,
            output_buffer: Vec::new(),
            error_buffer: Vec::new(),
            stdout_reader: stdout,
            stderr_reader: stderr,
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
        let mut result = Vec::new();
        let mut completed_tasks = Vec::new();

        if let Some(tasks) = self.sessions.get_mut(&session) {
            for (&task_id, task) in tasks.iter_mut() {
                // First, read all available output from stdout/stderr
                if let Some(ref mut reader) = task.stdout_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.output_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: false,
                            text: line,
                        }));
                        buf.clear();
                    }
                }
                if let Some(ref mut reader) = task.stderr_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.error_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: true,
                            text: line,
                        }));
                        buf.clear();
                    }
                }

                // Then check if process has exited
                if let Some(mut child) = task.child.take() {
                    match child.try_wait() {
                        Ok(Some(exit_status)) => {
                            // Read any remaining output after process exits
                            if let Some(mut reader) = task.stdout_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    task.output_buffer.push(line.clone());
                                    result.push(SessionEvent::Output(OutputChunk {
                                        task_id: task_id,
                                        is_stderr: false,
                                        text: line,
                                    }));
                                    buf.clear();
                                }
                            }
                            if let Some(mut reader) = task.stderr_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    task.error_buffer.push(line.clone());
                                    result.push(SessionEvent::Output(OutputChunk {
                                        task_id: task_id,
                                        is_stderr: true,
                                        text: line,
                                    }));
                                    buf.clear();
                                }
                            }

                            let status = if exit_status.success() {
                                TaskStatus::Exited(0)
                            } else {
                                TaskStatus::Exited(exit_status.code().unwrap_or(1))
                            };

                            task.status = status;

                            result.push(SessionEvent::TaskUpdated {
                                task_id: task_id,
                                status,
                            });

                            completed_tasks.push(task_id);
                        }
                        Ok(None) => {
                            task.child = Some(child);
                        }
                        Err(_) => {
                            task.child = Some(child);
                        }
                    }
                }
            }

            for task_id in completed_tasks {
                tasks.remove(&task_id);
            }
        }

        Ok(result)
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
            Ok(mut c) => {
                // Take stdout and stderr to prevent buffer from filling up
                let stdout = c.stdout.take().map(BufReader::new);
                let stderr = c.stderr.take().map(BufReader::new);
                Task {
                    child: Some(c),
                    status: TaskStatus::Running,
                    output_buffer: Vec::new(),
                    error_buffer: Vec::new(),
                    stdout_reader: stdout,
                    stderr_reader: stderr,
                }
            }
            Err(e) => {
                // If cygctl is not found, create a task that will immediately fail
                let task = Task {
                    child: None,
                    status: TaskStatus::Failed,
                    output_buffer: Vec::new(),
                    error_buffer: vec![format!("cygctl error: {}", e)],
                    stdout_reader: None,
                    stderr_reader: None,
                };
                self.sessions
                    .entry(session)
                    .or_insert_with(HashMap::new)
                    .insert(task_id, task);
                return Ok(task_id);
            }
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
        let mut result = Vec::new();
        let mut completed_tasks = Vec::new();

        if let Some(tasks) = self.sessions.get_mut(&session) {
            for (&task_id, task) in tasks.iter_mut() {
                // First, read all available output from stdout/stderr (real-time)
                if let Some(ref mut reader) = task.stdout_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.output_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: false,
                            text: line,
                        }));
                        buf.clear();
                    }
                }
                if let Some(ref mut reader) = task.stderr_reader {
                    use std::io::BufRead;
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                        let line = decode_oem_line(&buf);
                        task.error_buffer.push(line.clone());
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id: task_id,
                            is_stderr: true,
                            text: line,
                        }));
                        buf.clear();
                    }
                }

                // Then check if process has exited
                if let Some(mut child) = task.child.take() {
                    match child.try_wait() {
                        Ok(Some(exit_status)) => {
                            // Read any remaining output after process exits
                            if let Some(mut reader) = task.stdout_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    if !line.is_empty() {
                                        task.output_buffer.push(line.clone());
                                        result.push(SessionEvent::Output(OutputChunk {
                                            task_id: task_id,
                                            is_stderr: false,
                                            text: line,
                                        }));
                                    }
                                    buf.clear();
                                }
                            }
                            if let Some(mut reader) = task.stderr_reader.take() {
                                use std::io::BufRead;
                                let mut buf = String::new();
                                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                                    let line = buf.trim_end().trim_end_matches('\r').to_string();
                                    if !line.is_empty() {
                                        task.error_buffer.push(line.clone());
                                        result.push(SessionEvent::Output(OutputChunk {
                                            task_id: task_id,
                                            is_stderr: true,
                                            text: line,
                                        }));
                                    }
                                    buf.clear();
                                }
                            }

                            let status = if exit_status.success() {
                                TaskStatus::Exited(0)
                            } else {
                                TaskStatus::Exited(exit_status.code().unwrap_or(1))
                            };

                            task.status = status;

                            result.push(SessionEvent::TaskUpdated {
                                task_id: task_id,
                                status,
                            });

                            completed_tasks.push(task_id);
                        }
                        Ok(None) => {
                            task.child = Some(child);
                        }
                        Err(_) => {
                            task.child = Some(child);
                        }
                    }
                }
            }

            for task_id in completed_tasks {
                tasks.remove(&task_id);
            }
        }

        Ok(result)
    }
}
