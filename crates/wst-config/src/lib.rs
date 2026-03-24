use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wst_protocol::BackendKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WstConfig {
    pub default_backend: BackendKind,
    pub cygctl_path: String,
    pub fullscreen: bool,
    pub alternate_screen: bool,
    pub hotkey: String,

    // Second stage: Daemon settings
    #[serde(rename = "daemon")]
    pub daemon_autostart: Option<bool>,
    #[serde(rename = "daemon_persist_backend")]
    pub daemon_persist_backend: Option<bool>,
    #[serde(rename = "daemon_max_sessions")]
    pub daemon_max_sessions: Option<usize>,
    #[serde(rename = "daemon_snapshot_interval")]
    pub daemon_snapshot_interval: Option<u64>,
    #[serde(rename = "hotkey_global")]
    pub hotkey_global: Option<String>,

    // Second stage: Session settings
    #[serde(rename = "session_restore")]
    pub session_restore_on_startup: Option<bool>,
}

impl Default for WstConfig {
    fn default() -> Self {
        Self {
            default_backend: BackendKind::Cmd,
            cygctl_path: "./cygctl.exe".to_string(),
            fullscreen: true,
            alternate_screen: true,
            hotkey: "Ctrl+Alt+F3".to_string(),

            // Second stage defaults
            daemon_autostart: Some(true),
            daemon_persist_backend: Some(true),
            daemon_max_sessions: Some(10),
            daemon_snapshot_interval: Some(300),
            hotkey_global: Some("Ctrl+Alt+F3".to_string()),
            session_restore_on_startup: Some(true),
        }
    }
}

impl WstConfig {
    pub fn load_default() -> Result<Self> {
        // Prefer wst.toml next to the executable so the binary can be launched
        // from any working directory and still find its config.
        let exe_sibling = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("wst.toml")));

        let path = exe_sibling
            .filter(|p| p.exists())
            .unwrap_or_else(|| Path::new("wst.toml").to_path_buf());

        if !path.exists() {
            return Ok(Self::default());
        }

        let text = fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&text)?;
        Ok(cfg)
    }
}
