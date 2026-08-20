//! Persisted manager settings (`~/.config/entangled/manager.toml`).
//!
//! Portable by design (ADR-0002): the only OS-specific part is where the
//! configuration directory lives, and that is resolved from environment
//! variables rather than a platform crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::backend::{Backend, DEFAULT_WSL_DISTRO};

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
    /// Ask the GitHub Releases API for a newer version once, on startup
    /// (background thread; failures are silent). Default on, and the check is
    /// skipped entirely when this is off.
    pub check_updates_on_startup: bool,
    /// Ambient motion, hover easing and live status pulses. Turning it off also
    /// drops the manager from a 25 FPS idle repaint loop to event-driven draws.
    pub animations_enabled: bool,
    /// Wizard defaults.
    pub default_memory_mib: u64,
    pub default_vcpus: u32,
    pub default_disk_gib: u64,
    pub default_variant: String,
    /// Which hypervisor a new machine uses. On Linux there is only one answer;
    /// on Windows this is the WHP-or-WSL choice.
    pub default_backend: Backend,
    /// Per-machine overrides of [`Self::default_backend`], keyed by VM name.
    ///
    /// Manager state rather than profile state on purpose: where a machine runs
    /// is a property of *this host*, and the same profile is meant to boot on
    /// either backend (ADR-0002). Writing it into the profile would make the
    /// file host-specific, which is exactly what that ADR forbids.
    pub vm_backends: BTreeMap<String, Backend>,
    /// The WSL distribution the WSL backend talks to (`wsl -d <name>`).
    pub wsl_distro: String,
    /// Path to the Linux build of `entangled`, as seen *inside* WSL. `None`
    /// means "whatever `entangled` resolves to on the WSL PATH" — which is the
    /// honest default, because a Windows install ships no Linux binary and a
    /// developer's build lives wherever their target directory pointed.
    pub wsl_entangled: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            vm_dir: default_vm_dir(),
            entangled_binary: None,
            work_dir: None,
            headless_install: false,
            check_updates_on_startup: true,
            animations_enabled: true,
            default_memory_mib: 2048,
            default_vcpus: 2,
            default_disk_gib: 16,
            default_variant: "text-netboot".to_string(),
            default_backend: Backend::Native,
            vm_backends: BTreeMap::new(),
            wsl_distro: DEFAULT_WSL_DISTRO.to_string(),
            wsl_entangled: None,
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
        let mut settings: Self = toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        // A settings file written before the memory ceiling existed (or by hand)
        // could name a guest size `control_api` now refuses. Clamping the wizard
        // *default* is the friendly half of that rule: the user gets the largest
        // VM this machine can build instead of a create button that fails.
        if settings.default_memory_mib > control_api::MAX_MEMORY_MIB {
            tracing::warn!(
                configured = settings.default_memory_mib,
                max = control_api::MAX_MEMORY_MIB,
                "default_memory_mib exceeds what the machine can build; clamping"
            );
            settings.default_memory_mib = control_api::MAX_MEMORY_MIB;
        }
        Ok(settings)
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

    /// Where a named machine runs: its own choice, else the default — and
    /// always the native backend when the chosen one does not exist on this
    /// host, so a settings file carried from Windows to Linux still works.
    pub fn backend_for(&self, vm: &str) -> Backend {
        let chosen = self
            .vm_backends
            .get(vm)
            .copied()
            .unwrap_or(self.default_backend);
        if chosen.available_on_host() {
            chosen
        } else {
            Backend::Native
        }
    }

    /// Records a machine's backend, dropping the entry when it merely repeats
    /// the default — the settings file should not fill up with redundant rows.
    pub fn set_backend_for(&mut self, vm: &str, backend: Backend) {
        if backend == self.default_backend {
            self.vm_backends.remove(vm);
        } else {
            self.vm_backends.insert(vm.to_string(), backend);
        }
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
            check_updates_on_startup: false,
            animations_enabled: false,
            default_memory_mib: 3072,
            default_vcpus: 4,
            default_disk_gib: 40,
            default_variant: "gtk-netboot".into(),
            default_backend: Backend::Wsl,
            vm_backends: [("ubuntu-lab".to_string(), Backend::Native)]
                .into_iter()
                .collect(),
            wsl_distro: "Debian".into(),
            wsl_entangled: Some("/home/spider/bin/entangled".into()),
        };

        settings.save_to(&path).expect("save");
        let back = Settings::load_from(&path).expect("load");
        assert_eq!(settings, back);
    }

    /// The backend a machine runs on: its own entry, the default, and — on a
    /// host where the chosen one cannot exist — the native fallback. A settings
    /// file copied from Windows must not leave a Linux user with a "WSL"
    /// machine that can never start.
    #[test]
    fn backend_resolution_falls_back_to_what_this_host_has() {
        let mut settings = Settings {
            default_backend: Backend::Native,
            ..Settings::default()
        };
        assert_eq!(settings.backend_for("anything"), Backend::Native);

        settings.set_backend_for("lab", Backend::Wsl);
        assert_eq!(
            settings.backend_for("lab"),
            if cfg!(windows) {
                Backend::Wsl
            } else {
                Backend::Native
            }
        );

        // Setting a machine back to the default drops the row rather than
        // storing a duplicate of it.
        settings.set_backend_for("lab", Backend::Native);
        assert!(settings.vm_backends.is_empty());
    }

    /// A settings file naming more RAM than the machine can build must not turn
    /// the wizard into a create button that fails: the default is clamped to what
    /// `control_api` will accept.
    #[test]
    fn an_oversized_memory_default_is_clamped_on_load() {
        let dir = temp_dir("settings-clamp");
        let path = dir.join("manager.toml");
        let settings = Settings {
            default_memory_mib: control_api::MAX_MEMORY_MIB * 4,
            ..Settings::default()
        };
        settings.save_to(&path).expect("save");
        let back = Settings::load_from(&path).expect("load");
        assert_eq!(back.default_memory_mib, control_api::MAX_MEMORY_MIB);
        // Everything else survives untouched.
        assert_eq!(back.default_vcpus, settings.default_vcpus);
        assert_eq!(back.vm_dir, settings.vm_dir);
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
        // A settings file written before the update check existed keeps the
        // check ON — the toggle is opt-out, not opt-in.
        assert!(loaded.check_updates_on_startup);
    }

    /// The "check for updates on startup" switch persists in both positions.
    #[test]
    fn the_update_check_toggle_round_trips() {
        let dir = temp_dir("settings-updates-toggle");
        let path = dir.join("manager.toml");
        for enabled in [false, true] {
            let settings = Settings {
                check_updates_on_startup: enabled,
                ..Settings::default()
            };
            settings.save_to(&path).expect("save");
            let back = Settings::load_from(&path).expect("load");
            assert_eq!(back.check_updates_on_startup, enabled);
        }
    }

    #[test]
    fn the_animation_toggle_round_trips() {
        let dir = temp_dir("settings-motion-toggle");
        let path = dir.join("manager.toml");
        for enabled in [false, true] {
            let settings = Settings {
                animations_enabled: enabled,
                ..Settings::default()
            };
            settings.save_to(&path).expect("save");
            assert_eq!(
                Settings::load_from(&path).unwrap().animations_enabled,
                enabled
            );
        }
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
