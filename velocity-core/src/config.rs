use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{VelocityError, VelocityResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub hotkey: HotkeyConfig,
    pub excludes: ExcludeConfig,
    pub drives: DriveConfig,
    pub indexing: IndexingConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub key: String,           // e.g., "Alt+Space"
    pub modifiers: Vec<String>, // e.g., ["Alt", "Space"]
}

impl HotkeyConfig {
    pub fn hotkey_hint(&self) -> &str {
        &self.key
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludeConfig {
    pub enabled: bool,
    pub patterns: Vec<String>,  // Glob patterns
    pub file_path: Option<PathBuf>, // Path to excludes.txt
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    pub enabled: bool,
    pub drives: Vec<String>,    // e.g., ["C:", "D:"]
    pub file_path: Option<PathBuf>, // Path to drives.txt
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    pub enabled: bool,
    pub interval_secs: u64,     // Re-index interval
    pub show_progress: bool,
    pub partial_search_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,          // "system", "light", "dark"
    pub max_results: usize,
    pub preview_enabled: bool,
    pub preview_max_size_mb: u64,
    pub hover_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig {
                key: "Alt+Space".to_string(),
                modifiers: vec!["Alt".to_string(), "Space".to_string()],
            },
            excludes: ExcludeConfig {
                enabled: true,
                patterns: vec![
                    "**/node_modules/**".to_string(),
                    "**/.git/**".to_string(),
                    "**/AppData/**".to_string(),
                    "**/Windows/**".to_string(),
                    "**/Program Files/**".to_string(),
                    "**/Program Files (x86)/**".to_string(),
                    "**/$Recycle.Bin/**".to_string(),
                    "**/System Volume Information/**".to_string(),
                ],
                file_path: None,
            },
            drives: DriveConfig {
                enabled: true,
                drives: vec![],
                file_path: None,
            },
            indexing: IndexingConfig {
                enabled: true,
                interval_secs: 300,
                show_progress: true,
                partial_search_allowed: true,
            },
            ui: UiConfig {
                theme: "system".to_string(),
                max_results: 100,
                preview_enabled: true,
                preview_max_size_mb: 50,
                hover_delay_ms: 300,
            },
        }
    }
}

impl Config {
    pub fn load() -> VelocityResult<Self> {
        let exe_dir = std::env::current_exe()?
            .parent()
            .ok_or_else(|| VelocityError::Config("No exe parent".to_string()))?
            .to_path_buf();

        // Try to load excludes.txt
        let excludes_path = exe_dir.join("excludes.txt");
        let mut excludes = Self::default().excludes;
        if excludes_path.exists() {
            let content = std::fs::read_to_string(&excludes_path)?;
            excludes.patterns = content
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .collect();
            excludes.file_path = Some(excludes_path);
        }

        // Try to load drives.txt
        let drives_path = exe_dir.join("drives.txt");
        let mut drives = Self::default().drives;
        if drives_path.exists() {
            let content = std::fs::read_to_string(&drives_path)?;
            drives.drives = content
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .collect();
            drives.file_path = Some(drives_path);
        }

        Ok(Self {
            excludes,
            drives,
            ..Default::default()
        })
    }

    pub fn save_excludes(&self) -> VelocityResult<()> {
        if let Some(path) = &self.excludes.file_path {
            let content = self.excludes.patterns.join("\n");
            std::fs::write(path, content)?;
        }
        Ok(())
    }

    pub fn save_drives(&self) -> VelocityResult<()> {
        if let Some(path) = &self.drives.file_path {
            let content = self.drives.drives.join("\n");
            std::fs::write(path, content)?;
        }
        Ok(())
    }
}