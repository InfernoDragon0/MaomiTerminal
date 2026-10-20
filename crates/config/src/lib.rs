//! User configuration stored as JSON in the platform config directory. Has no GPUI dependency.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, choose_native_strategy};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "config.json";

/// Errors from reading or writing the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine the config directory: {0}")]
    Dirs(#[from] etcetera::HomeDirError),
    #[error("config file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("config file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persisted user settings. Unknown fields are ignored and missing ones take their default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Skin name, e.g. "aurora" or "onyx".
    pub skin: String,
    /// "dark" or "light".
    pub mode: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            skin: "aurora".into(),
            mode: "dark".into(),
        }
    }
}

impl Config {
    /// Path of the config file under the platform config directory.
    pub fn path() -> Result<PathBuf, ConfigError> {
        let strategy = choose_native_strategy(AppStrategyArgs {
            top_level_domain: String::new(),
            author: "maomi".into(),
            app_name: "MaomiTerminal".into(),
        })?;
        Ok(strategy.config_dir().join(FILE_NAME))
    }

    /// Loads the config; a missing file gives the defaults.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// Writes the config, creating the directory when needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let path = std::env::temp_dir().join("maomi-config-test-missing/config.json");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("maomi-config-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let config = Config {
            skin: "onyx".into(),
            mode: "light".into(),
        };
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
        _ = fs::remove_dir_all(dir);
    }
}
