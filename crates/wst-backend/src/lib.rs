use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use thiserror::Error;
use wst_protocol::{BackendKind, ExecRequest, OutputChunk, SessionEvent, SessionId, TaskId, TaskStatus};

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

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, HashMap::new());
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let task_id = self.next_task;
        self.next_task += 1;


        // Use PowerShell with UTF-8 encoding to execute cmd commands
        // This properly captures cmd.exe output including built-in commands like dir
        let ps_command = format!(
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; cmd /C {}",
            req.command_line.replace("\"", "`\"")
        );

        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_command])
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
                    let mut buf = String::new();
                    while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                        // Trim both trailing whitespace and any carriage returns
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
                if let Some(ref mut reader) = task.stderr_reader {
                    use std::io::BufRead;
                    let mut buf = String::new();
                    while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                        // Trim both trailing whitespace and any carriage returns
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
                        Err(e) => {
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

    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, HashMap::new());
        Ok(id)
    }

    fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
        let task_id = self.next_task;
        self.next_task += 1;

        // Set output encoding to UTF-8 before executing command
        let ps_command = format!(
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; {}",
            req.command_line
        );

        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_command])
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
                if let Some(ref mut reader) = task.stderr_reader {
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
            Ok(c) => Task {
                child: Some(c),
                status: TaskStatus::Running,
                output_buffer: Vec::new(),
                error_buffer: Vec::new(),
                stdout_reader: None,
                stderr_reader: None,
            },
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
                if let Some(mut child) = task.child.take() {
                    if let Some(exit_status) = child.try_wait()? {
                        // Process completed
                        if let Some(stdout) = child.stdout {
                            let reader = BufReader::new(stdout);
                            for line in reader.lines().flatten() {
                                task.output_buffer.push(line);
                            }
                        }
                        if let Some(stderr) = child.stderr {
                            let reader = BufReader::new(stderr);
                            for line in reader.lines().flatten() {
                                task.error_buffer.push(line);
                            }
                        }

                        let status = if exit_status.success() {
                            TaskStatus::Exited(0)
                        } else {
                            TaskStatus::Exited(exit_status.code().unwrap_or(1))
                        };

                        task.status = status;

                        for line in &task.output_buffer {
                            result.push(SessionEvent::Output(OutputChunk {
                                task_id: task_id,
                                is_stderr: false,
                                text: line.clone(),
                            }));
                        }
                        for line in &task.error_buffer {
                            result.push(SessionEvent::Output(OutputChunk {
                                task_id: task_id,
                                is_stderr: true,
                                text: line.clone(),
                            }));
                        }

                        result.push(SessionEvent::TaskUpdated {
                            task_id: task_id,
                            status,
                        });

                        completed_tasks.push(task_id);
                    } else {
                        task.child = Some(child);
                    }
                } else if task.status == TaskStatus::Failed {
                    // Send pending error events
                    for line in &task.error_buffer {
                        result.push(SessionEvent::Output(OutputChunk {
                            task_id,
                            is_stderr: true,
                            text: line.clone(),
                        }));
                    }
                    result.push(SessionEvent::TaskUpdated {
                        task_id,
                        status: TaskStatus::Exited(1),
                    });
                    task.status = TaskStatus::Exited(1);
                    completed_tasks.push(task_id);
                }
            }

            for task_id in completed_tasks {
                tasks.remove(&task_id);
            }
        }

        Ok(result)
    }
}
