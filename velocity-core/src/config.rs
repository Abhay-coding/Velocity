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

    /// Validate the hotkey configuration
    pub fn validate(&self) -> VelocityResult<()> {
        if self.key.is_empty() {
            return Err(VelocityError::Config("Hotkey cannot be empty".to_string()));
        }

        // The stored list contains every component, including the final key.
        let key_parts: Vec<&str> = self.key.split('+').map(str::trim).collect();
        if key_parts.len() < 2 || key_parts.iter().any(|part| part.is_empty()) {
            return Err(VelocityError::Config("Hotkey must include a modifier and a key".to_string()));
        }
        if key_parts.len() != self.modifiers.len()
            || !key_parts.iter().zip(&self.modifiers).all(|(part, stored)| part.eq_ignore_ascii_case(stored.trim())) {
            return Err(VelocityError::Config("Hotkey components don't match key string".to_string()));
        }
        let valid_modifiers = ["Alt", "Control", "Ctrl", "Shift", "Win", "Meta", "Super"];
        for modifier in &key_parts[..key_parts.len() - 1] {
            if !valid_modifiers.iter().any(|v| v.eq_ignore_ascii_case(modifier)) {
                return Err(VelocityError::Config(format!("Invalid modifier key: {}", modifier)));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludeConfig {
    pub enabled: bool,
    pub patterns: Vec<String>,  // Glob patterns
    pub file_path: Option<PathBuf>, // Path to excludes.txt
}

impl ExcludeConfig {
    /// Validate the exclude configuration
    pub fn validate(&self) -> VelocityResult<()> {
        if !self.enabled {
            return Ok(());
        }

        // Validate each glob pattern
        for pattern in &self.patterns {
            if pattern.is_empty() {
                return Err(VelocityError::Config("Exclude pattern cannot be empty".to_string()));
            }
            // Try to create a Glob to validate the pattern
            if glob::Pattern::new(pattern).is_err() {
                return Err(VelocityError::Config(format!("Invalid glob pattern: {}", pattern)));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    pub enabled: bool,
    pub drives: Vec<String>,    // e.g., ["C:", "D:"]
    pub file_path: Option<PathBuf>, // Path to drives.txt
}

impl DriveConfig {
    /// Validate the drive configuration
    pub fn validate(&self) -> VelocityResult<()> {
        if !self.enabled {
            return Ok(());
        }

        // Validate each drive path
        for drive in &self.drives {
            if drive.is_empty() {
                return Err(VelocityError::Config("Drive path cannot be empty".to_string()));
            }
            // Basic validation for Windows drive format (e.g., "C:", "D:")
            if drive.len() < 2 || !drive.ends_with(':') {
                return Err(VelocityError::Config(format!("Invalid drive format: {}", drive)));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    pub enabled: bool,
    pub interval_secs: u64,     // Re-index interval
    pub show_progress: bool,
    pub partial_search_allowed: bool,
}

impl IndexingConfig {
    /// Validate the indexing configuration
    pub fn validate(&self) -> VelocityResult<()> {
        if !self.enabled {
            return Ok(());
        }

        // Validate interval (minimum 60 seconds, maximum 1 day)
        if self.interval_secs < 60 {
            return Err(VelocityError::Config("Indexing interval must be at least 60 seconds".to_string()));
        }
        if self.interval_secs > 86400 {
            return Err(VelocityError::Config("Indexing interval must be at most 86400 seconds (1 day)".to_string()));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,          // "system", "light", "dark"
    pub max_results: usize,
    pub preview_enabled: bool,
    pub preview_max_size_mb: u64,
    pub hover_delay_ms: u64,
}

impl UiConfig {
    /// Validate the UI configuration
    pub fn validate(&self) -> VelocityResult<()> {
        // Validate theme
        let valid_themes = ["system", "light", "dark"];
        if !valid_themes.contains(&self.theme.as_str()) {
            return Err(VelocityError::Config(format!("Invalid theme: {} (must be system, light, or dark)", self.theme)));
        }

        // Validate max_results
        if self.max_results == 0 {
            return Err(VelocityError::Config("max_results must be greater than 0".to_string()));
        }
        if self.max_results > 10000 {
            return Err(VelocityError::Config("max_results must be at most 10000".to_string()));
        }

        // Validate preview_max_size_mb
        if self.preview_max_size_mb == 0 {
            return Err(VelocityError::Config("preview_max_size_mb must be greater than 0".to_string()));
        }
        if self.preview_max_size_mb > 1000 {
            return Err(VelocityError::Config("preview_max_size_mb must be at most 1000".to_string()));
        }

        // Validate hover_delay_ms
        if self.hover_delay_ms > 5000 {
            return Err(VelocityError::Config("hover_delay_ms must be at most 5000".to_string()));
        }

        Ok(())
    }
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
    /// Validate all configuration
    pub fn validate(&self) -> VelocityResult<()> {
        self.hotkey.validate()?;
        self.excludes.validate()?;
        self.drives.validate()?;
        self.indexing.validate()?;
        self.ui.validate()?;
        Ok(())
    }

    pub fn load() -> VelocityResult<Self> {
        let exe_dir = std::env::current_exe()?
            .parent()
            .ok_or_else(|| VelocityError::Config("No exe parent".to_string()))?
            .to_path_buf();

        // Try to load config.toml for all settings
        let config_path = exe_dir.join("config.toml");
        let mut config = Self::default();

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let toml_config: Config = toml::from_str(&content)
                .map_err(|e| VelocityError::Config(format!("Failed to parse config.toml: {}", e)))?;
            config = toml_config;
        } else {
            // Fallback: load individual files for backwards compatibility
            // Try to load excludes.txt
            let excludes_path = exe_dir.join("excludes.txt");
            if excludes_path.exists() {
                let content = std::fs::read_to_string(&excludes_path)?;
                config.excludes.patterns = content
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && !s.starts_with('#'))
                    .collect();
                config.excludes.file_path = Some(excludes_path);
            }

            // Try to load drives.txt
            let drives_path = exe_dir.join("drives.txt");
            if drives_path.exists() {
                let content = std::fs::read_to_string(&drives_path)?;
                config.drives.drives = content
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && !s.starts_with('#'))
                    .collect();
                config.drives.file_path = Some(drives_path);
            }

            // Try to load hotkey.txt
            let hotkey_path = exe_dir.join("hotkey.txt");
            if hotkey_path.exists() {
                let content = std::fs::read_to_string(&hotkey_path)?;
                let key = content.trim().to_string();
                if !key.is_empty() {
                    config.hotkey.key = key.clone();
                    // Parse modifiers from key string (e.g., "Alt+Space" -> ["Alt", "Space"])
                    config.hotkey.modifiers = key.split('+').map(|s| s.trim().to_string()).collect();
                }
            }
        }

        config.validate()?;
        Ok(config)
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

    /// Save all configuration to config.toml
    pub fn save_config(&self, exe_dir: Option<PathBuf>) -> VelocityResult<()> {
        let dir = exe_dir.unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
        let config_path = dir.join("config.toml");
        let content = toml::to_string_pretty(self)
            .map_err(|e| VelocityError::Config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().expect("default config must be usable");
    }

    #[test]
    fn rejects_inconsistent_hotkey_components() {
        let hotkey = HotkeyConfig { key: "Alt+Space".into(), modifiers: vec!["Ctrl".into(), "Space".into()] };
        assert!(hotkey.validate().is_err());
    }
}
