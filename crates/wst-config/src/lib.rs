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
    pub hotkey: String,
}

impl Default for WstConfig {
    fn default() -> Self {
        Self {
            default_backend: BackendKind::Cmd,
            cygctl_path: "./cygctl.exe".to_string(),
            fullscreen: true,
            hotkey: "Ctrl+Alt+Space".to_string(),
        }
    }
}

impl WstConfig {
    pub fn load_default() -> Result<Self> {
        let path = Path::new("wst.toml");
        if !path.exists() {
            return Ok(Self::default());
        }

        let text = fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text)?;
        Ok(cfg)
    }
}
