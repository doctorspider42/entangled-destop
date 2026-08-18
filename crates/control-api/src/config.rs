//! The `vmhost run <file>.toml` configuration format (backlog MVP-1201).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to parse VM config: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub boot: BootSection,
    #[serde(default, rename = "disk")]
    pub disks: Vec<DiskSection>,
    pub network: Option<NetworkSection>,
    #[serde(default)]
    pub display: DisplaySection,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BootMode {
    /// Direct bzImage + initramfs load — the only MVP mode.
    DirectLinux,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BootSection {
    pub mode: BootMode,
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    #[serde(default)]
    pub cmdline: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiskSection {
    pub path: PathBuf,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkBackend {
    Tap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    pub backend: NetworkBackend,
    pub interface: String,
    /// Optional fixed MAC ("52:00:…"); derived from the VM name when absent.
    pub mac: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct DisplaySection {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

impl Default for DisplaySection {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }
}

impl VmConfig {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let cfg: VmConfig = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let err = |m: String| Err(ConfigError::Invalid(m));
        if self.name.is_empty() {
            return err("name must not be empty".into());
        }
        if !(128..=65536).contains(&self.memory_mib) {
            return err(format!(
                "memory_mib {} outside supported range 128..=65536",
                self.memory_mib
            ));
        }
        if !(1..=64).contains(&self.vcpus) {
            return err(format!(
                "vcpus {} outside supported range 1..=64",
                self.vcpus
            ));
        }
        if self.display.width == 0 || self.display.height == 0 {
            return err("display dimensions must be non-zero".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the backlog (EPIC 12) must parse as-is.
    const BACKLOG_EXAMPLE: &str = r#"
name = "debian-demo"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"
initramfs = "artifacts/bootstrap/initrd.img"
cmdline = "console=ttyS0 root=/dev/vda1 rw"

[[disk]]
path = "images/debian.raw"
writable = true

[network]
backend = "tap"
interface = "vmhost0"

[display]
width = 1920
height = 1080
scale = 1.0
"#;

    #[test]
    fn parses_backlog_example() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(cfg.name, "debian-demo");
        assert_eq!(cfg.boot.mode, BootMode::DirectLinux);
        assert_eq!(cfg.disks.len(), 1);
        assert!(cfg.disks[0].writable);
        assert_eq!(cfg.network.as_ref().unwrap().interface, "vmhost0");
        assert_eq!(cfg.display.width, 1920);
    }

    #[test]
    fn display_and_disks_are_optional() {
        let cfg = VmConfig::from_toml(
            r#"
name = "tiny"
memory_mib = 512
vcpus = 1
[boot]
mode = "direct-linux"
kernel = "vmlinuz"
"#,
        )
        .unwrap();
        assert_eq!(cfg.display, DisplaySection::default());
        assert!(cfg.disks.is_empty());
        assert!(cfg.network.is_none());
    }

    #[test]
    fn rejects_nonsense() {
        assert!(VmConfig::from_toml("name = 3").is_err());
        let zero_mem = BACKLOG_EXAMPLE.replace("memory_mib = 2048", "memory_mib = 1");
        assert!(matches!(
            VmConfig::from_toml(&zero_mem),
            Err(ConfigError::Invalid(_))
        ));
        let typo = BACKLOG_EXAMPLE.replace("[display]", "[dispaly]");
        assert!(matches!(
            VmConfig::from_toml(&typo),
            Err(ConfigError::Parse(_))
        ));
    }
}
