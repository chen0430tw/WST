use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wst_protocol::{BackendKind, SessionId, TaskId};
use uuid::Uuid;

/// Configuration for creating a new session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Display name for the session
    pub name: String,
    /// Backend type for this session
    pub backend: BackendKind,
    /// Initial working directory
    pub cwd: Option<String>,
    /// Initial environment variables
    pub env: HashMap<String, String>,
    /// Whether this session persists when frontend closes
    pub persistent: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            name: format!("Session-{}", Uuid::new_v4().as_simple()),
            backend: BackendKind::Cygctl,
            cwd: None,
            env: HashMap::new(),
            persistent: true,
        }
    }
}

/// State of a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// Session is being initialized
    Initializing,
    /// Session is ready for commands
    Ready,
    /// Session has running tasks
    Busy,
    /// Session is idle (no running tasks)
    Idle,
    /// Session encountered an error
    Error,
    /// Session is being closed
    Closing,
    /// Session is closed
    Closed,
}

/// A WST session - an independent execution context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier
    pub id: SessionId,
    /// Display name
    pub name: String,
    /// Backend type
    pub backend: BackendKind,
    /// Current state
    pub state: SessionState,
    /// Creation time
    pub created_at: DateTime<Utc>,
    /// Last activity time
    pub last_activity: DateTime<Utc>,
    /// Current working directory
    pub cwd: String,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Whether session persists
    pub persistent: bool,
    /// Running tasks in this session
    pub tasks: Vec<TaskId>,
    /// Command history for this session
    pub history: Vec<String>,
}

impl Session {
    /// Create a new session with the given configuration
    pub fn new(config: SessionConfig) -> Self {
        Self {
            id: generate_session_id(),
            name: config.name,
            backend: config.backend,
            state: SessionState::Initializing,
            created_at: Utc::now(),
            last_activity: Utc::now(),
            cwd: config.cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "C:\\".to_string())
            }),
            env: config.env,
            persistent: config.persistent,
            tasks: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Get the session ID
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Get the session name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Set the session name
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// Get the session state
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Set the session state
    pub fn set_state(&mut self, state: SessionState) {
        self.state = state;
        self.last_activity = Utc::now();
    }

    /// Get the current working directory
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Set the current working directory
    pub fn set_cwd(&mut self, cwd: String) {
        self.cwd = cwd;
    }

    /// Get an environment variable
    pub fn get_env(&self, key: &str) -> Option<&String> {
        self.env.get(key)
    }

    /// Set an environment variable
    pub fn set_env(&mut self, key: String, value: String) {
        self.env.insert(key, value);
    }

    /// Add a task to this session
    pub fn add_task(&mut self, task_id: TaskId) {
        if !self.tasks.contains(&task_id) {
            self.tasks.push(task_id);
            self.state = SessionState::Busy;
        }
    }

    /// Remove a task from this session
    pub fn remove_task(&mut self, task_id: TaskId) {
        self.tasks.retain(|&t| t != task_id);
        if self.tasks.is_empty() {
            self.state = SessionState::Idle;
        }
    }

    /// Get the number of running tasks
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Add a command to history
    pub fn add_history(&mut self, command: String) {
        if !command.is_empty() && !self.history.contains(&command) {
            self.history.push(command);
            self.last_activity = Utc::now();
        }
    }

    /// Get command history
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Check if session is idle
    pub fn is_idle(&self) -> bool {
        self.state == SessionState::Idle || self.state == SessionState::Ready
    }

    /// Check if session is active (not closed)
    pub fn is_active(&self) -> bool {
        !matches!(self.state, SessionState::Closed | SessionState::Closing)
    }

    /// Update last activity time
    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
    }

    /// Get the duration since last activity
    pub fn idle_duration(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.last_activity)
    }

    /// Create a snapshot for persistence
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id,
            name: self.name.clone(),
            backend: self.backend,
            state: self.state,
            created_at: self.created_at,
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            history: self.history.clone(),
        }
    }

    /// Restore from a snapshot
    pub fn restore(snapshot: SessionSnapshot) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name,
            backend: snapshot.backend,
            state: snapshot.state,
            created_at: snapshot.created_at,
            last_activity: Utc::now(),
            cwd: snapshot.cwd,
            env: snapshot.env,
            persistent: true,
            tasks: Vec::new(),
            history: snapshot.history,
        }
    }
}

/// Snapshot of a session for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub name: String,
    pub backend: BackendKind,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub history: Vec<String>,
}

/// Generate a unique session ID
fn generate_session_id() -> SessionId {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Combine timestamp with random component for uniqueness
    let random = (uuid::Uuid::new_v4().as_u128() & 0xFFFF) as u64;
    (timestamp << 16) | random
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let config = SessionConfig {
            name: "test-session".to_string(),
            ..Default::default()
        };
        let session = Session::new(config);

        assert_eq!(session.name(), "test-session");
        assert_eq!(session.state(), SessionState::Initializing);
        assert!(session.is_active());
        // Initializing is not idle: cleanup_idle_sessions must not reap it.
        assert!(!session.is_idle());
    }

    #[test]
    fn test_session_tasks() {
        let mut session = Session::new(SessionConfig::default());

        session.add_task(1);
        session.add_task(2);
        assert_eq!(session.task_count(), 2);
        assert_eq!(session.state(), SessionState::Busy);

        session.remove_task(1);
        assert_eq!(session.task_count(), 1);

        session.remove_task(2);
        assert_eq!(session.task_count(), 0);
        assert_eq!(session.state(), SessionState::Idle);
    }

    #[test]
    fn test_session_history() {
        let mut session = Session::new(SessionConfig::default());

        session.add_history("ls -la".to_string());
        session.add_history("pwd".to_string());

        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[0], "ls -la");
        assert_eq!(session.history()[1], "pwd");

        // Duplicate should not be added
        session.add_history("ls -la".to_string());
        assert_eq!(session.history().len(), 2);
    }

    #[test]
    fn test_session_env() {
        let mut session = Session::new(SessionConfig::default());

        session.set_env("PATH".to_string(), "/usr/bin".to_string());
        session.set_env("HOME".to_string(), "/root".to_string());

        assert_eq!(session.get_env("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(session.get_env("HOME"), Some(&"/root".to_string()));
        assert_eq!(session.get_env("NONEXISTENT"), None);
    }
}
