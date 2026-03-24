//! # WST Daemon
//!
//! The WST daemon is a system-level resident process that:
//! - Registers global hotkeys
//! - Manages session persistence
//! - Communicates with the frontend via IPC
//! - Keeps backend processes alive when frontend is hidden

pub mod ipc;
pub mod hotkey;
pub mod lifecycle;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use wst_config::WstConfig;
use wst_session::{SessionManager, SessionManagerConfig};

/// Hotkey event channel capacity
const HOTKEY_CHANNEL_CAPACITY: usize = 32;

/// Daemon state shared across components
pub struct DaemonState {
    /// Session manager
    pub session_manager: Arc<SessionManager>,
    /// Configuration
    pub config: WstConfig,
    /// Whether daemon is shutting down
    pub shutting_down: Arc<RwLock<bool>>,
    /// Current frontend visibility
    pub frontend_visible: Arc<RwLock<bool>>,
}

impl DaemonState {
    /// Create a new daemon state
    pub fn new(config: WstConfig) -> Result<Self> {
        let session_config = SessionManagerConfig {
            max_sessions: config.daemon_max_sessions.unwrap_or(10),
            persist_sessions: config.daemon_persist_backend.unwrap_or(true),
            store_dir: None,
            snapshot_interval: config.daemon_snapshot_interval.unwrap_or(300),
        };

        let session_manager = Arc::new(SessionManager::with_config(session_config)?);

        Ok(Self {
            session_manager,
            config,
            shutting_down: Arc::new(RwLock::new(false)),
            frontend_visible: Arc::new(RwLock::new(false)),
        })
    }

    /// Check if daemon is shutting down
    pub async fn is_shutting_down(&self) -> bool {
        *self.shutting_down.read().await
    }

    /// Initiate shutdown
    pub async fn shutdown(&self) {
        *self.shutting_down.write().await = true;
    }

    /// Check if frontend is visible
    pub async fn is_frontend_visible(&self) -> bool {
        *self.frontend_visible.read().await
    }

    /// Set frontend visibility
    pub async fn set_frontend_visible(&self, visible: bool) {
        *self.frontend_visible.write().await = visible;
    }

    /// Toggle frontend visibility
    pub async fn toggle_frontend(&self) -> bool {
        let mut visible = self.frontend_visible.write().await;
        *visible = !*visible;
        *visible
    }
}

/// Daemon runtime
pub struct WstDaemon {
    state: Arc<DaemonState>,
}

impl WstDaemon {
    /// Create a new daemon
    pub fn new(config: WstConfig) -> Result<Self> {
        let state = Arc::new(DaemonState::new(config)?);
        Ok(Self { state })
    }

    /// Get the daemon state
    pub fn state(&self) -> Arc<DaemonState> {
        self.state.clone()
    }

    /// Run the daemon
    pub async fn run(&self) -> Result<()> {
        tracing::info!("WST Daemon starting...");

        // Create hotkey event channel
        let (hotkey_tx, hotkey_rx) = mpsc::channel(HOTKEY_CHANNEL_CAPACITY);

        // Start global hotkey listener in background thread
        // hotkey_global takes priority; fall back to hotkey, then built-in default.
        let hotkey_str = self.state.config.hotkey_global.as_deref()
            .unwrap_or(&self.state.config.hotkey);
        let hotkey_config = hotkey::HotkeyConfig::parse(hotkey_str)
            .unwrap_or_else(|_| hotkey::HotkeyConfig::default_wst_hotkey());

        let _hotkey_handle = hotkey::start_hotkey_thread(hotkey_config, hotkey_tx)?;

        // Spawn hotkey event handler task
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Err(e) = hotkey::run_hotkey_listener(state, hotkey_rx).await {
                tracing::error!("Hotkey listener error: {}", e);
            }
        });

        // Restore previous sessions
        if let Err(e) = self.restore_sessions().await {
            tracing::warn!("Failed to restore sessions: {}", e);
        }

        // Start IPC server
        let ipc_state = self.state.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::run_ipc_server(ipc_state).await {
                tracing::error!("IPC server error: {}", e);
            }
        });

        tracing::info!("WST Daemon running (press Ctrl+C to stop)");
        tracing::info!("Global hotkey: {} (configurable)", hotkey_str);

        // Wait for shutdown signal
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C, shutting down...");
            }
            _ = self.wait_for_shutdown() => {
                tracing::info!("Shutdown requested");
            }
        }

        // Snapshot sessions before exit
        self.snapshot_sessions().await?;

        tracing::info!("WST Daemon stopped");
        Ok(())
    }

    /// Restore sessions from storage
    async fn restore_sessions(&self) -> Result<()> {
        let ids = self.state.session_manager.restore_sessions().await?;
        tracing::info!("Restored {} sessions", ids.len());
        Ok(())
    }

    /// Snapshot all sessions
    async fn snapshot_sessions(&self) -> Result<()> {
        self.state.session_manager.snapshot_all().await?;
        Ok(())
    }

    /// Wait for shutdown signal
    async fn wait_for_shutdown(&self) -> Result<()> {
        while !self.state.is_shutting_down().await {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_daemon_state() {
        let config = WstConfig::default();
        let state = DaemonState::new(config).unwrap();

        assert!(!state.is_shutting_down().await);
        assert!(!state.is_frontend_visible().await);

        state.set_frontend_visible(true).await;
        assert!(state.is_frontend_visible().await);

        assert_eq!(state.toggle_frontend().await, false);
    }
}
