use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use wst_backend::{Backend, CmdBackend, CygctlBackend, PwshBackend};
use wst_config::WstConfig;
use wst_protocol::{BackendKind, ExecRequest, SessionEvent, SessionId, TaskId};

const MAX_HISTORY: usize = 1000;

pub struct HistoryEntry {
    pub command: String,
    pub timestamp: u64,
}

pub struct History {
    entries: VecDeque<HistoryEntry>,
    index: usize,
}

impl History {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_HISTORY),
            index: 0,
        }
    }

    pub fn add(&mut self, command: String) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.entries.push_back(HistoryEntry { command, timestamp });
        if self.entries.len() > MAX_HISTORY {
            self.entries.pop_front();
        }
        self.index = self.entries.len();
    }

    pub fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.index > 0 {
            self.index = self.index.saturating_sub(1);
        }
        self.entries.get(self.index).map(|e| e.command.as_str())
    }

    pub fn next(&mut self) -> Option<&str> {
        if self.index < self.entries.len() {
            self.index += 1;
        }
        self.entries.get(self.index).map(|e| e.command.as_str())
    }

    pub fn reset(&mut self) {
        self.index = self.entries.len();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    pub fn commands(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.command.clone()).collect()
    }

    pub fn search(&self, prefix: &str) -> Vec<&str> {
        self.entries
            .iter()
            .rev()
            .filter_map(|e| {
                if e.command.starts_with(prefix) {
                    Some(e.command.as_str())
                } else {
                    None
                }
            })
            .take(10)
            .collect()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WstCore {
    config: WstConfig,
    backend_kind: BackendKind,
    backend: Box<dyn Backend>,
    session: Option<SessionId>,
    history: History,
}

impl WstCore {
    pub fn new(config: WstConfig) -> Self {
        let backend_kind = config.default_backend;

        let backend: Box<dyn Backend> = match backend_kind {
            BackendKind::Cmd => Box::new(CmdBackend::new()),
            BackendKind::Pwsh => Box::new(PwshBackend::new()),
            BackendKind::Cygctl => Box::new(CygctlBackend::new(&config.cygctl_path)),
        };

        Self {
            config,
            backend_kind,
            backend,
            session: None,
            history: History::new(),
        }
    }

    pub fn default_backend(&self) -> BackendKind {
        self.backend_kind
    }

    pub fn ensure_session(&mut self) -> Result<SessionId> {
        if let Some(session) = self.session {
            Ok(session)
        } else {
            let session = self.backend.spawn_session().map_err(|e| anyhow!("{}", e))?;
            self.session = Some(session);
            Ok(session)
        }
    }

    pub fn create_session(&mut self) -> Result<SessionId> {
        let session = self.backend.spawn_session().map_err(|e| anyhow!("{}", e))?;
        self.session = Some(session);
        Ok(session)
    }

    pub fn exec(&mut self, command: String) -> Result<TaskId> {
        if command.trim().is_empty() {
            return Err(anyhow!("empty command"));
        }

        self.history.add(command.clone());

        let session = self.ensure_session()?;
        let req = ExecRequest {
            command_line: command,
            cwd: None,
            env: vec![],
        };

        self.backend.exec(session, req).map_err(|e| anyhow!("{}", e))
    }

    pub fn exec_with_session(&mut self, session: SessionId, command: String) -> Result<TaskId> {
        if command.trim().is_empty() {
            return Err(anyhow!("empty command"));
        }

        self.history.add(command.clone());

        let req = ExecRequest {
            command_line: command,
            cwd: None,
            env: vec![],
        };

        self.backend.exec(session, req).map_err(|e| anyhow!("{}", e))
    }

    pub fn tick(&mut self) -> Result<Vec<SessionEvent>> {
        if let Some(session) = self.session {
            let events = self.backend.poll_events(session).map_err(|e| anyhow!("{}", e))?;
            Ok(events)
        } else {
            Ok(vec![])
        }
    }

    pub fn tick_session(&mut self, session: SessionId) -> Result<Vec<SessionEvent>> {
        Ok(self.backend.poll_events(session).map_err(|e| anyhow!("{}", e))?)
    }

    pub fn config(&self) -> &WstConfig {
        &self.config
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn history_commands(&self) -> Vec<String> {
        self.history.commands()
    }

    pub fn history_prev(&mut self) -> Option<String> {
        self.history.prev().map(|s| s.to_string())
    }

    pub fn history_next(&mut self) -> Option<String> {
        self.history.next().map(|s| s.to_string())
    }

    pub fn history_reset(&mut self) {
        self.history.reset();
    }

    pub fn switch_backend(&mut self, kind: BackendKind) -> Result<()> {
        if kind == self.backend_kind {
            return Ok(());
        }

        let new_backend: Box<dyn Backend> = match kind {
            BackendKind::Cmd => Box::new(CmdBackend::new()),
            BackendKind::Pwsh => Box::new(PwshBackend::new()),
            BackendKind::Cygctl => Box::new(CygctlBackend::new(&self.config.cygctl_path)),
        };

        self.backend = new_backend;
        self.backend_kind = kind;
        self.session = None;
        // Clear all old backend state
        self.backend.reset();
        Ok(())
    }
}
