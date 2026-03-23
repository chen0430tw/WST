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

/// WST Core with multi-backend support
pub struct WstCore {
    config: WstConfig,
    backend_manager: BackendManager,
    history: History,
}

impl WstCore {
    pub fn new(config: WstConfig) -> Self {
        let backend_manager = BackendManager::new(&config);
        Self {
            config,
            backend_manager,
            history: History::new(),
        }
    }

    pub fn default_backend(&self) -> BackendKind {
        self.backend_manager.default_backend()
    }

    pub fn ensure_session(&mut self) -> Result<SessionId> {
        self.backend_manager.ensure_session()
    }

    pub fn create_session(&mut self) -> Result<SessionId> {
        self.backend_manager.create_session()
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

        self.backend_manager.exec(session, req).map_err(|e| anyhow!("{}", e))
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

        self.backend_manager.exec(session, req).map_err(|e| anyhow!("{}", e))
    }

    pub fn tick(&mut self) -> Result<Vec<SessionEvent>> {
        self.backend_manager.tick().map_err(|e| anyhow!("{}", e))
    }

    pub fn tick_session(&mut self, session: SessionId) -> Result<Vec<SessionEvent>> {
        self.backend_manager.tick_session(session).map_err(|e| anyhow!("{}", e))
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
        self.backend_manager.switch_backend(kind)
    }
}

/// Backend manager for handling multiple backend instances
pub struct BackendManager {
    backends: std::collections::HashMap<BackendKind, Box<dyn Backend>>,
    default_backend: BackendKind,
    /// Tracks which backend owns each session ID
    session_owners: std::collections::HashMap<SessionId, BackendKind>,
}

impl BackendManager {
    pub fn new(config: &WstConfig) -> Self {
        let mut backends: std::collections::HashMap<BackendKind, Box<dyn Backend>> = std::collections::HashMap::new();

        backends.insert(BackendKind::Cmd, Box::new(CmdBackend::new()));
        backends.insert(BackendKind::Pwsh, Box::new(PwshBackend::new()));
        backends.insert(BackendKind::Cygctl, Box::new(CygctlBackend::new(&config.cygctl_path)));

        Self {
            backends,
            default_backend: config.default_backend,
            session_owners: std::collections::HashMap::new(),
        }
    }

    pub fn default_backend(&self) -> BackendKind {
        self.default_backend
    }

    pub fn get_backend(&mut self, kind: BackendKind) -> &mut dyn Backend {
        if !self.backends.contains_key(&kind) {
            // Create on-demand
            let backend: Box<dyn Backend> = match kind {
                BackendKind::Cmd => Box::new(CmdBackend::new()),
                BackendKind::Pwsh => Box::new(PwshBackend::new()),
                BackendKind::Cygctl => Box::new(CygctlBackend::new("cygctl")),
            };
            self.backends.insert(kind, backend);
        }
        // Safe unwrap: we just inserted if not present
        self.backends.get_mut(&kind).map(|b| b.as_mut()).unwrap()
    }

    pub fn ensure_session(&mut self) -> Result<SessionId> {
        let kind = self.default_backend;
        let backend = self.get_backend(kind);
        let sid = backend.spawn_session().map_err(|e| anyhow!("{}", e))?;
        self.session_owners.insert(sid, kind);
        Ok(sid)
    }

    pub fn create_session(&mut self) -> Result<SessionId> {
        let kind = self.default_backend;
        let backend = self.get_backend(kind);
        let sid = backend.spawn_session().map_err(|e| anyhow!("{}", e))?;
        self.session_owners.insert(sid, kind);
        Ok(sid)
    }

    pub fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId> {
        // Only run on the backend that owns this session
        let kind = self.session_owners.get(&session).copied()
            .unwrap_or(self.default_backend);
        let backend = self.get_backend(kind);
        backend.exec(session, req).map_err(|e| anyhow!("{}", e))
    }

    pub fn tick(&mut self) -> Result<Vec<SessionEvent>> {
        let mut all_events = Vec::new();
        // Collect (kind, session_ids) to avoid borrow issues
        let mut by_kind: std::collections::HashMap<BackendKind, Vec<SessionId>> =
            std::collections::HashMap::new();
        for backend in self.backends.values() {
            let kind = backend.kind();
            let sids = backend.active_session_ids();
            if !sids.is_empty() {
                by_kind.entry(kind).or_default().extend(sids);
            }
        }
        for (kind, sids) in by_kind {
            let backend = self.get_backend(kind);
            for sid in sids {
                if let Ok(events) = backend.poll_events(sid) {
                    all_events.extend(events);
                }
            }
        }
        Ok(all_events)
    }

    pub fn tick_session(&mut self, session: SessionId) -> Result<Vec<SessionEvent>> {
        let kind = self.session_owners.get(&session).copied()
            .unwrap_or(self.default_backend);
        let backend = self.get_backend(kind);
        backend.poll_events(session).map_err(|e| anyhow!("{}", e))
    }

    pub fn switch_backend(&mut self, kind: BackendKind) -> Result<()> {
        self.default_backend = kind;
        Ok(())
    }
}
