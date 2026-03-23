use crate::{Result, SessionError, SessionState};
use crate::session::{Session, SessionConfig};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use wst_protocol::{SessionId, TaskId};
use crate::store::SessionStore;

/// Configuration for SessionManager
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Maximum number of sessions
    pub max_sessions: usize,
    /// Whether to persist sessions
    pub persist_sessions: bool,
    /// Directory for session storage
    pub store_dir: Option<String>,
    /// Session snapshot interval in seconds
    pub snapshot_interval: u64,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            max_sessions: 10,
            persist_sessions: true,
            store_dir: None,
            snapshot_interval: 300,
        }
    }
}

/// Manages multiple WST sessions
pub struct SessionManager {
    /// Configuration
    config: SessionManagerConfig,
    /// Active sessions by ID
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
    /// Current active session ID
    current_session: Arc<RwLock<Option<SessionId>>>,
    /// Session store for persistence
    store: Option<SessionStore>,
}

impl SessionManager {
    /// Create a new session manager with default config
    pub fn new() -> Result<Self> {
        Self::with_config(SessionManagerConfig::default())
    }

    /// Create a new session manager with custom config
    pub fn with_config(config: SessionManagerConfig) -> Result<Self> {
        let store = if config.persist_sessions {
            let store_dir = config.store_dir.clone().unwrap_or_else(|| {
                SessionStore::default_dir().to_string_lossy().to_string()
            });
            Some(SessionStore::new(store_dir)?)
        } else {
            None
        };

        Ok(Self {
            config,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            current_session: Arc::new(RwLock::new(None)),
            store,
        })
    }

    /// Create a new session
    pub async fn create_session(&self, config: SessionConfig) -> Result<SessionId> {
        // Check max sessions limit
        let sessions = self.sessions.read().await;
        if sessions.len() >= self.config.max_sessions {
            drop(sessions);
            // Try to close a non-persistent idle session first
            self.cleanup_idle_sessions().await?;
        }

        let session = Session::new(config);
        let id = session.id();

        // Persist if enabled
        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        // Add to active sessions
        self.sessions.write().await.insert(id, session);

        // Set as current if this is the first session
        if self.current_session.read().await.is_none() {
            *self.current_session.write().await = Some(id);
        }

        tracing::info!("Created session {}", id);
        Ok(id)
    }

    /// Get a session by ID
    pub async fn get_session(&self, id: SessionId) -> Result<Session> {
        let sessions = self.sessions.read().await;
        sessions.get(&id)
            .cloned()
            .ok_or(SessionError::SessionNotFound(id))
    }

    /// Get the current active session
    pub async fn current_session(&self) -> Result<Session> {
        let current_id = *self.current_session.read().await;
        match current_id {
            Some(id) => self.get_session(id).await,
            None => Err(SessionError::SessionNotFound(0)),
        }
    }

    /// Switch to a different session
    pub async fn switch_session(&self, id: SessionId) -> Result<()> {
        // Verify session exists
        {
            let sessions = self.sessions.read().await;
            sessions.get(&id)
                .ok_or(SessionError::SessionNotFound(id))?;
        }

        *self.current_session.write().await = Some(id);
        tracing::info!("Switched to session {}", id);
        Ok(())
    }

    /// Close a session
    pub async fn close_session(&self, id: SessionId) -> Result<()> {
        let mut sessions = self.sessions.write().await;

        let session = sessions.get(&id)
            .ok_or(SessionError::SessionNotFound(id))?;

        // Don't close if it has running tasks
        if session.task_count() > 0 {
            return Err(SessionError::SessionLocked(id));
        }

        // Set state to closing
        let mut session = session.clone();
        session.set_state(SessionState::Closing);

        // Remove from store if persisted
        if let Some(store) = &self.store {
            store.delete(id).await?;
        }

        sessions.remove(&id);

        // Update current session if needed
        if *self.current_session.read().await == Some(id) {
            *self.current_session.write().await = sessions.keys().next().copied();
        }

        tracing::info!("Closed session {}", id);
        Ok(())
    }

    /// List all sessions
    pub async fn list_sessions(&self) -> Vec<Session> {
        let sessions = self.sessions.read().await;
        sessions.values().cloned().collect()
    }

    /// Rename a session
    pub async fn rename_session(&self, id: SessionId, name: String) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&id)
            .ok_or(SessionError::SessionNotFound(id))?;

        session.set_name(name);

        // Update store
        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        Ok(())
    }

    /// Add a task to a session
    pub async fn add_task(&self, session_id: SessionId, task_id: TaskId) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound(session_id))?;

        session.add_task(task_id);

        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        Ok(())
    }

    /// Remove a task from a session
    pub async fn remove_task(&self, session_id: SessionId, task_id: TaskId) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound(session_id))?;

        session.remove_task(task_id);

        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        Ok(())
    }

    /// Add command to session history
    pub async fn add_history(&self, session_id: SessionId, command: String) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id)
            .ok_or(SessionError::SessionNotFound(session_id))?;

        session.add_history(command);

        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        Ok(())
    }

    /// Restore sessions from store
    pub async fn restore_sessions(&self) -> Result<Vec<SessionId>> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };

        let snapshots = store.restore_all().await?;
        let mut restored_ids = Vec::new();

        for snapshot in snapshots {
            let session: Session = Session::restore(snapshot.clone());
            let id = session.id();

            self.sessions.write().await.insert(id, session);
            restored_ids.push(id);
        }

        // Set current session to first restored
        if let Some(&first_id) = restored_ids.first() {
            *self.current_session.write().await = Some(first_id);
        }

        tracing::info!("Restored {} sessions", restored_ids.len());
        Ok(restored_ids)
    }

    /// Snapshot all sessions
    pub async fn snapshot_all(&self) -> Result<()> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(()),
        };

        let sessions = self.sessions.read().await;
        for session in sessions.values() {
            store.save(session.snapshot()).await?;
        }

        tracing::debug!("Snapshot all sessions");
        Ok(())
    }

    /// Clean up idle, non-persistent sessions
    async fn cleanup_idle_sessions(&self) -> Result<()> {
        let sessions = self.sessions.read().await;
        let mut to_remove = Vec::new();

        for (id, session) in sessions.iter() {
            if !session.persistent && session.is_idle() && session.task_count() == 0 {
                to_remove.push(*id);
            }
        }

        drop(sessions);

        for id in to_remove {
            let _ = self.close_session(id).await;
        }

        Ok(())
    }

    /// Get session count
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Check if a session exists
    pub async fn has_session(&self, id: SessionId) -> bool {
        self.sessions.read().await.contains_key(&id)
    }

    /// Update session state
    pub async fn update_session_state(&self, id: SessionId, state: SessionState) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&id)
            .ok_or(SessionError::SessionNotFound(id))?;

        session.set_state(state);

        if let Some(store) = &self.store {
            store.save(session.snapshot()).await?;
        }

        Ok(())
    }

    /// Get session by name
    pub async fn get_session_by_name(&self, name: &str) -> Result<Session> {
        let sessions = self.sessions.read().await;
        for session in sessions.values() {
            if session.name() == name {
                return Ok(session.clone());
            }
        }
        Err(SessionError::SessionNotFound(0))
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_session() {
        let manager = SessionManager::new().unwrap();
        let config = SessionConfig {
            name: "test".to_string(),
            ..Default::default()
        };

        let id = manager.create_session(config).await.unwrap();
        assert!(manager.has_session(id).await);

        let session = manager.get_session(id).await.unwrap();
        assert_eq!(session.name(), "test");
    }

    #[tokio::test]
    async fn test_switch_session() {
        let manager = SessionManager::new().unwrap();

        let id1 = manager.create_session(SessionConfig::default()).await.unwrap();
        let id2 = manager.create_session(SessionConfig::default()).await.unwrap();

        manager.switch_session(id1).await.unwrap();
        assert_eq!(manager.current_session().await.unwrap().id(), id1);

        manager.switch_session(id2).await.unwrap();
        assert_eq!(manager.current_session().await.unwrap().id(), id2);
    }

    #[tokio::test]
    async fn test_close_session() {
        let manager = SessionManager::new().unwrap();

        let id = manager.create_session(SessionConfig::default()).await.unwrap();
        manager.close_session(id).await.unwrap();
        assert!(!manager.has_session(id).await);
    }

    #[tokio::test]
    async fn test_task_management() {
        let manager = SessionManager::new().unwrap();

        let id = manager.create_session(SessionConfig::default()).await.unwrap();
        manager.add_task(id, 1).await.unwrap();
        manager.add_task(id, 2).await.unwrap();

        let session = manager.get_session(id).await.unwrap();
        assert_eq!(session.task_count(), 2);

        // Can't close session with running tasks
        assert!(manager.close_session(id).await.is_err());

        manager.remove_task(id, 1).await.unwrap();
        manager.remove_task(id, 2).await.unwrap();

        // Now can close
        assert!(manager.close_session(id).await.is_ok());
    }
}
