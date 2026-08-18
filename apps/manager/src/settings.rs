//! Persisted manager settings (`~/.config/entangled/manager.toml`).
//!
//! Portable by design (ADR-0002): the only OS-specific part is where the
//! configuration directory lives, and that is resolved from environment
//! variables rather than a platform crate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("cannot determine a configuration directory (set XDG_CONFIG_HOME or HOME)")]
    NoConfigDir,

    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot serialize settings: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Everything the manager remembers between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Directory scanned for `*.toml` VM profiles and used for new disks.
    pub vm_dir: PathBuf,
    /// Explicit path to the `entangled` binary; `None` means "discover it"
    /// (see [`crate::launcher::locate_cli`]).
    pub entangled_binary: Option<PathBuf>,
    /// Working directory for spawned CLI children. VM profiles may contain
    /// relative paths (the installer writes `artifacts/bootstrap/vmlinuz`),
    /// and those resolve against this directory.
    pub work_dir: Option<PathBuf>,
    /// Pass `--headless` to `entangled install` by default (no installer
    /// window; the log pane still shows the serial console).
    pub headless_install: bool,
    /// Wizard defaults.
    pub default_memory_mib: u64,
    pub default_vcpus: u32,
    pub default_disk_gib: u64,
    pub default_variant: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            vm_dir: default_vm_dir(),
            entangled_binary: None,
            work_dir: None,
            headless_install: false,
            default_memory_mib: 2048,
            default_vcpus: 2,
            default_disk_gib: 16,
            default_variant: "text-netboot".to_string(),
        }
    }
}

impl Settings {
    /// Reads the settings file, falling back to defaults when it does not
    /// exist yet. A malformed file is an error — silently resetting somebody's
    /// VM directory would be worse.
    pub fn load_from(path: &Path) -> Result<Self, SettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(SettingsError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn save_to(&self, path: &Path) -> Result<(), SettingsError> {
        let text = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SettingsError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, text).map_err(|source| SettingsError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Directory a spawned child should run in: the configured one, else the
    /// manager's own working directory.
    pub fn child_cwd(&self) -> PathBuf {
        self.work_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// `$XDG_CONFIG_HOME/entangled/manager.toml`, `$HOME/.config/…` on Unix,
/// `%APPDATA%\entangled\manager.toml` on Windows.
pub fn config_path() -> Result<PathBuf, SettingsError> {
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("entangled").join("manager.toml"));
    }
    #[cfg(windows)]
    if let Some(dir) = non_empty_env("APPDATA") {
        return Ok(PathBuf::from(dir).join("entangled").join("manager.toml"));
    }
    home_dir()
        .map(|home| home.join(".config").join("entangled").join("manager.toml"))
        .ok_or(SettingsError::NoConfigDir)
}

pub fn home_dir() -> Option<PathBuf> {
    non_empty_env("HOME")
        .or_else(|| non_empty_env("USERPROFILE"))
        .map(PathBuf::from)
}

fn default_vm_dir() -> PathBuf {
    match home_dir() {
        Some(home) => home.join("entangled-vms"),
        None => PathBuf::from("entangled-vms"),
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "entangled-manager-tests/{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = temp_dir("settings-roundtrip");
        let path = dir.join("nested").join("manager.toml");

        let settings = Settings {
            vm_dir: PathBuf::from("/srv/vms"),
            entangled_binary: Some(PathBuf::from("/opt/entangled/bin/entangled")),
            work_dir: Some(PathBuf::from("/srv")),
            headless_install: true,
            default_memory_mib: 4096,
            default_vcpus: 4,
            default_disk_gib: 40,
            default_variant: "gtk-netboot".into(),
        };

        settings.save_to(&path).expect("save");
        let back = Settings::load_from(&path).expect("load");
        assert_eq!(settings, back);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = temp_dir("settings-missing");
        let loaded = Settings::load_from(&dir.join("absent.toml")).expect("defaults");
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn partial_file_keeps_defaults_for_absent_keys() {
        let dir = temp_dir("settings-partial");
        let path = dir.join("manager.toml");
        std::fs::write(&path, "vm_dir = \"/tmp/only-this\"\n").expect("write");

        let loaded = Settings::load_from(&path).expect("load");
        assert_eq!(loaded.vm_dir, PathBuf::from("/tmp/only-this"));
        assert_eq!(loaded.default_vcpus, Settings::default().default_vcpus);
        assert_eq!(loaded.default_variant, Settings::default().default_variant);
    }

    #[test]
    fn malformed_file_is_an_error_not_a_silent_reset() {
        let dir = temp_dir("settings-broken");
        let path = dir.join("manager.toml");
        std::fs::write(&path, "vm_dir = 12\n").expect("write");
        assert!(matches!(
            Settings::load_from(&path),
            Err(SettingsError::Parse { .. })
        ));

        std::fs::write(&path, "unknown_key = true\n").expect("write");
        assert!(matches!(
            Settings::load_from(&path),
            Err(SettingsError::Parse { .. })
        ));
    }
}
