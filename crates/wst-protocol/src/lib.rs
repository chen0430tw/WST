use serde::{Deserialize, Serialize};

pub type SessionId = u64;
pub type TaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendKind {
    Cygctl,
    Pwsh,
    Cmd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command_line: String,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputChunk {
    pub task_id: TaskId,
    pub is_stderr: bool,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Exited(i32),
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    SessionStarted(SessionId),
    Output(OutputChunk),
    TaskUpdated {
        task_id: TaskId,
        status: TaskStatus,
    },
    Debug {
        message: String,
    },
}
