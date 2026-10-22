//! User configuration stored as JSON in the platform config directory. Has no GPUI dependency.
//!
//! The file carries a `version`. [`Config::load`] upgrades older files step by step (see
//! [`migrate`]) before parsing, so a field can move or be renamed without losing the user's value.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, choose_native_strategy};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod hosts;
mod layout;
mod snippets;

pub use hosts::{Host, HostAuth, Hosts};
pub use layout::{DrawerLayout, Layout, PaneLayout, TabLayout, WindowBounds, WindowLayout};
pub use snippets::{Snippet, Snippets};

const FILE_NAME: &str = "config.json";
/// A file that failed to parse is moved here so the next save does not overwrite it.
const BAD_FILE_NAME: &str = "config.bad.json";

/// Schema version written by this build.
pub const CURRENT_VERSION: u32 = 1;

/// Errors from reading or writing the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine the config directory: {0}")]
    Dirs(#[from] etcetera::HomeDirError),
    #[error("config file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("config file is not valid: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persisted user settings. Unknown fields are ignored and missing ones take their default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub appearance: Appearance,
    pub terminal: TerminalSettings,
    pub shell: ShellSettings,
    pub updates: UpdateSettings,
    pub security: SecuritySettings,
}

/// Skin and mode, by name so this crate stays UI-free; the UI maps unknown names to its defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// "aurora", "onyx" or "mochi".
    pub skin: String,
    /// "dark", "light" or "system" (follow the OS).
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    pub font_family: String,
    /// Points; clamped to 6-72.
    pub font_size: f32,
    /// Multiple of the font size; clamped to 1.0-2.5.
    pub line_height: f32,
    pub smooth_scroll: bool,
    /// Lines kept per pane; clamped to 0-1,000,000.
    pub scrollback: usize,
    /// "block", "bar" or "underline"; programs can still change it while they run.
    pub cursor_style: String,
    pub cursor_blink: bool,
}

/// The shell new tabs start. `program: None` picks the best one found on the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSettings {
    pub program: Option<PathBuf>,
    /// None = the shell family's default arguments.
    pub args: Option<Vec<String>>,
    /// Folder new tabs start in; None = the home folder.
    pub startup_dir: Option<PathBuf>,
    /// Shell-follow default for new file drawers: opening a folder there runs `cd`.
    pub drawer_follow: bool,
    /// Reopen the windows, tabs and splits of the last session at start.
    pub restore_layout: bool,
    /// The new-tab button asks for the folder the tab starts in.
    pub pick_folder_for_new_tab: bool,
}

/// Installed builds only; dev builds never update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Download new releases in the background and install them when the app quits.
    pub auto: bool,
    /// Version of the last start; a different version at start means the app was updated.
    pub last_version: Option<String>,
}

/// How the secrets vault unlocks and when it locks again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// "keystore" (key in the OS keystore), "passphrase" (asked once per launch) or "ask"
    /// (passphrase, asked again after every auto-lock).
    pub unlock: String,
    /// Lock after this many minutes without using a secret; 0 = never. Clamped to 0-1440.
    pub auto_lock_minutes: u32,
    /// Windows Hello / Touch ID before the keystore key is read.
    pub require_presence: bool,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            unlock: "keystore".into(),
            auto_lock_minutes: 10,
            require_presence: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            appearance: Appearance::default(),
            terminal: TerminalSettings::default(),
            shell: ShellSettings::default(),
            updates: UpdateSettings::default(),
            security: SecuritySettings::default(),
        }
    }
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            program: None,
            args: None,
            startup_dir: None,
            drawer_follow: false,
            restore_layout: true,
            pick_folder_for_new_tab: true,
        }
    }
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto: true,
            last_version: None,
        }
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            skin: "aurora".into(),
            mode: "dark".into(),
        }
    }
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_family: "Cascadia Code".into(),
            font_size: 13.0,
            line_height: 1.4,
            smooth_scroll: true,
            scrollback: 10_000,
            cursor_style: "block".into(),
            cursor_blink: true,
        }
    }
}

impl Config {
    /// Path of the config file under the platform config directory.
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(config_dir()?.join(FILE_NAME))
    }

    /// Loads the config; a missing file gives the defaults.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// Loads the config, falling back to the defaults on any error.
    ///
    /// A file that is not valid is moved to `config.bad.json` first so the user's edits survive
    /// the next save. The error is returned for logging.
    pub fn load_or_default() -> (Self, Option<ConfigError>) {
        match Self::path() {
            Ok(path) => Self::load_or_default_from(&path),
            Err(err) => (Self::default(), Some(err)),
        }
    }

    /// Writes the config, creating the directory when needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    /// Values pulled into their allowed ranges; a blank font family becomes the default.
    pub fn sanitized(mut self) -> Self {
        let defaults = TerminalSettings::default();
        let t = &mut self.terminal;
        t.font_size = clamp_or(t.font_size, 6.0, 72.0, defaults.font_size);
        t.line_height = clamp_or(t.line_height, 1.0, 2.5, defaults.line_height);
        t.scrollback = t.scrollback.min(1_000_000);
        if t.font_family.trim().is_empty() {
            t.font_family = defaults.font_family;
        }
        self.security.auto_lock_minutes = self.security.auto_lock_minutes.min(1440);
        if self
            .shell
            .startup_dir
            .as_ref()
            .is_some_and(|d| d.as_os_str().is_empty())
        {
            self.shell.startup_dir = None;
        }
        self
    }

    fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn load_or_default_from(path: &Path) -> (Self, Option<ConfigError>) {
        match Self::load_from(path) {
            Ok(config) => (config, None),
            Err(err) => {
                if matches!(err, ConfigError::Json(_)) {
                    _ = fs::rename(path, path.with_file_name(BAD_FILE_NAME));
                }
                (Self::default(), Some(err))
            }
        }
    }

    fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut value: Value = serde_json::from_str(text)?;
        migrate(&mut value);
        let config: Self = serde_json::from_value(value)?;
        Ok(Self {
            version: CURRENT_VERSION,
            ..config.sanitized()
        })
    }

    fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        write_atomic(path, &serde_json::to_string_pretty(self)?)
    }
}

/// The platform config directory for MaomiTerminal.
fn config_dir() -> Result<PathBuf, ConfigError> {
    let strategy = choose_native_strategy(AppStrategyArgs {
        top_level_domain: String::new(),
        author: "maomi".into(),
        app_name: "MaomiTerminal".into(),
    })?;
    Ok(strategy.config_dir())
}

/// Writes a sibling file then renames it over `path`, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Upgrades a parsed file to [`CURRENT_VERSION`] in place, one version at a time.
///
/// Files from a newer build are left as they are and parsed best-effort.
fn migrate(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let mut version = obj.get("version").and_then(Value::as_u64).unwrap_or(0);
    while version < u64::from(CURRENT_VERSION) {
        // One step per version; add `else if version == 1 { v1_to_v2(obj) }` and so on.
        if version == 0 {
            v0_to_v1(obj);
        }
        version += 1;
    }
}

/// v0 (no `version`) kept `skin` and `mode` at the top level; v1 groups them under `appearance`.
fn v0_to_v1(obj: &mut Map<String, Value>) {
    let mut appearance = Map::new();
    for key in ["skin", "mode"] {
        if let Some(v) = obj.remove(key) {
            appearance.insert(key.into(), v);
        }
    }
    if !appearance.is_empty() {
        obj.insert("appearance".into(), Value::Object(appearance));
    }
}

/// `value` clamped to `min..=max`; NaN becomes `fallback`.
fn clamp_or(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_nan() {
        fallback
    } else {
        value.clamp(min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("maomi-config-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = temp_dir("missing").join("config.json");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.json");
        let mut config = Config::default();
        config.appearance.skin = "onyx".into();
        config.appearance.mode = "light".into();
        config.terminal.font_size = 15.0;
        config.shell.program = Some("bash".into());
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
        assert!(!path.with_extension("json.tmp").exists());
        _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v0_file_is_migrated() {
        let config = Config::parse(r#"{"skin": "mochi", "mode": "light"}"#).unwrap();
        assert_eq!(config.version, CURRENT_VERSION);
        assert_eq!(config.appearance.skin, "mochi");
        assert_eq!(config.appearance.mode, "light");
        assert_eq!(config.terminal, TerminalSettings::default());
    }

    #[test]
    fn restore_layout_defaults_on_for_older_files() {
        let config = Config::parse(r#"{"version": 1, "shell": {"drawer_follow": true}}"#).unwrap();
        assert!(config.shell.restore_layout);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let config = Config::parse(r#"{"version": 1, "terminal": {"font_size": 16}}"#).unwrap();
        assert_eq!(config.terminal.font_size, 16.0);
        assert!(config.terminal.smooth_scroll);
        assert_eq!(config.appearance, Appearance::default());
        assert!(config.updates.auto);
        assert_eq!(config.security, SecuritySettings::default());
    }

    #[test]
    fn newer_version_parses_best_effort() {
        let text = r#"{"version": 99, "appearance": {"skin": "onyx"}, "future": true}"#;
        let config = Config::parse(text).unwrap();
        assert_eq!(config.appearance.skin, "onyx");
        assert_eq!(config.version, CURRENT_VERSION);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let text = r#"{"version": 1, "terminal": {"font_size": 400, "line_height": 0.2, "font_family": " "}}"#;
        let config = Config::parse(text).unwrap();
        assert_eq!(config.terminal.font_size, 72.0);
        assert_eq!(config.terminal.line_height, 1.0);
        assert_eq!(config.terminal.font_family, "Cascadia Code");
    }

    #[test]
    fn invalid_file_is_moved_aside() {
        let dir = temp_dir("bad");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{ not json").unwrap();
        let (config, err) = Config::load_or_default_from(&path);
        assert_eq!(config, Config::default());
        assert!(matches!(err, Some(ConfigError::Json(_))));
        assert!(!path.exists());
        assert!(dir.join(BAD_FILE_NAME).exists());
        _ = fs::remove_dir_all(dir);
    }
}
