//! Persisted settings, stored under `~/Library/Application Support/edifier-menubar/config.json`.
//!
//! The only thing that survives a restart is which CoreBluetooth peripheral the app bound to, so
//! that the next launch can go straight back to the same headset.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Directory under `~/Library/Application Support`.
const APP_DIR: &str = "edifier-menubar";
/// Name of the settings file.
const FILE_NAME: &str = "config.json";

/// Settings remembered between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// CoreBluetooth identifier of the headset; stable across launches on macOS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_identifier: Option<String>,
    /// Advertised name of the headset, used to recognise it after a re-pair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

impl Config {
    /// Path of the settings file inside a given home directory.
    #[must_use]
    pub fn path_in(home: &Path) -> PathBuf {
        home.join("Library")
            .join("Application Support")
            .join(APP_DIR)
            .join(FILE_NAME)
    }

    /// Path of the settings file for the current user.
    ///
    /// # Errors
    ///
    /// Fails when `HOME` is not set, which leaves no place to persist to.
    pub fn path() -> Result<PathBuf> {
        Ok(Self::path_in(&crate::home()?))
    }

    /// Load the settings file for the current user.
    ///
    /// # Errors
    ///
    /// See [`Config::path`] and [`Config::load_from`].
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    /// Load the settings from `path`, treating a missing file as [`Config::default`].
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read or parsed.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let config: Self = serde_json::from_str(&text)
            .with_context(|| format!("cannot parse {}", path.display()))?;
        Ok(config)
    }

    /// Write the settings file for the current user.
    ///
    /// # Errors
    ///
    /// See [`Config::path`] and [`Config::save_to`].
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Write the settings to `path`, creating the parent directory when needed.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be written.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut text = serde_json::to_string_pretty(self).context("cannot encode settings")?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }

    /// Remember a discovered endpoint.
    ///
    /// Returns `true` when something changed and the settings need saving.
    pub fn remember(&mut self, identifier: &str, name: Option<&str>) -> bool {
        let mut changed = false;
        if self.device_identifier.as_deref() != Some(identifier) {
            self.device_identifier = Some(identifier.to_string());
            changed = true;
        }
        let name = name.map(str::to_string);
        if self.device_name != name {
            self.device_name = name;
            changed = true;
        }
        changed
    }

    /// Forget the remembered device, so the next scan discovers it from scratch.
    pub fn forget(&mut self) -> bool {
        let changed = self.device_identifier.is_some() || self.device_name.is_some();
        self.device_identifier = None;
        self.device_name = None;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn path_in_uses_the_application_support_directory() {
        let path = Config::path_in(Path::new("/Users/example"));
        assert_eq!(
            path,
            PathBuf::from("/Users/example/Library/Application Support/edifier-menubar/config.json")
        );
    }

    #[test]
    fn missing_file_loads_the_defaults() {
        let dir = TempDir::new("config-missing");
        let path = dir.path().join("nested").join("config.json");
        assert_eq!(Config::load_from(&path).expect("loads"), Config::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new("config-round-trip");
        let path = dir.path().join("deep").join("config.json");
        let config = Config {
            device_identifier: Some("B1E4E6C2-1365-C7BE-6B22-4B0F0FDD7C4A".to_string()),
            device_name: Some("EDIFIER W820NB".to_string()),
        };
        config.save_to(&path).expect("saves");

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            text.ends_with('\n'),
            "file should end with a newline: {text}"
        );
        assert_eq!(Config::load_from(&path).expect("loads"), config);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let dir = TempDir::new("config-unknown-field");
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"device_identifier": "x", "surprise": 1}"#).expect("writes");
        let error = Config::load_from(&path).expect_err("rejects unknown field");
        assert!(error.to_string().contains("cannot parse"), "{error:#}");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_default() {
        let dir = TempDir::new("config-malformed");
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json").expect("writes");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn partial_files_fall_back_to_default_fields() {
        let dir = TempDir::new("config-partial");
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"device_name": "EDIFIER W820NB"}"#).expect("writes");
        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.device_identifier, None);
        assert_eq!(config.device_name.as_deref(), Some("EDIFIER W820NB"));
    }

    #[test]
    fn remember_only_reports_real_changes() {
        let mut config = Config::default();
        assert!(config.remember("uuid-1", Some("EDIFIER W820NB")));
        assert_eq!(config.device_identifier.as_deref(), Some("uuid-1"));
        assert_eq!(config.device_name.as_deref(), Some("EDIFIER W820NB"));

        assert!(!config.remember("uuid-1", Some("EDIFIER W820NB")));
        assert!(config.remember("uuid-1", None));
        assert_eq!(config.device_name, None);

        assert!(config.forget());
        assert_eq!(config, Config::default());
        assert!(!config.forget());
    }
}
