//! The `entangled run <file>.toml` configuration format (backlog MVP-1201).

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
    /// Which virtio transport the VM's devices sit on. Defaults to `mmio`, so
    /// every profile written before the pci transport existed still describes
    /// exactly the machine it used to.
    #[serde(default)]
    pub transport: VirtioTransport,
    pub boot: BootSection,
    #[serde(default, rename = "disk")]
    pub disks: Vec<DiskSection>,
    pub network: Option<NetworkSection>,
    #[serde(default)]
    pub display: DisplaySection,
}

/// The virtio transport a VM's devices are attached to (EPIC 3 / EPIC 19).
///
/// Devices themselves are transport-agnostic, so this changes only how the guest
/// *finds* them — and how much of the guest has to cooperate:
///
/// * `mmio` needs `virtio_mmio.device=` clauses on the kernel command line and a
///   kernel built with `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`. Nothing enumerates:
///   the host tells the guest where to look. This is the MVP default and what
///   every existing profile means.
/// * `pci` needs nothing on the command line — the guest walks the bus — but does
///   need `CONFIG_VIRTIO_PCI`. It is the only transport a UEFI firmware can use:
///   EDK2's CloudHv build ships `VirtioPciDeviceDxe` and no virtio-MMIO driver at
///   all (ADR-0003), so booting an installer ISO requires it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VirtioTransport {
    /// virtio-mmio slots announced on the kernel command line.
    #[default]
    Mmio,
    /// virtio-pci functions on the PCI root bus.
    Pci,
}

impl VirtioTransport {
    pub fn is_pci(self) -> bool {
        matches!(self, Self::Pci)
    }
}

impl std::fmt::Display for VirtioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mmio => f.write_str("mmio"),
            Self::Pci => f.write_str("pci"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BootMode {
    /// Direct bzImage + initramfs load — the MVP mode (ADR-0001 §3).
    DirectLinux,
    /// Boot a UEFI firmware image, which then finds its own bootloader
    /// (EPIC 18, [ADR-0003](../../docs/adr/0003-uefi-firmware.md)).
    Uefi,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BootSection {
    pub mode: BootMode,
    /// `direct-linux` only: the kernel `bzImage` the host loads itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
    /// `direct-linux` only: initramfs loaded after the kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<PathBuf>,
    /// `uefi` only: the firmware image (a PVH ELF such as `CLOUDHV.fd`, or a
    /// flash image entered through the reset vector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<PathBuf>,
    /// Kernel command line. Meaningless in `uefi` mode — the firmware and the
    /// guest bootloader own the command line there.
    #[serde(default)]
    pub cmdline: String,
}

impl BootSection {
    /// The kernel image, for `direct-linux` profiles. Validation guarantees it
    /// is present in that mode; this accessor keeps the error typed for
    /// callers that build a `BootSection` by hand.
    pub fn require_kernel(&self) -> Result<&PathBuf, ConfigError> {
        self.kernel.as_ref().ok_or_else(|| {
            ConfigError::Invalid("boot.kernel is required for mode = \"direct-linux\"".into())
        })
    }

    /// The firmware image, for `uefi` profiles.
    pub fn require_firmware(&self) -> Result<&PathBuf, ConfigError> {
        self.firmware.as_ref().ok_or_else(|| {
            ConfigError::Invalid("boot.firmware is required for mode = \"uefi\"".into())
        })
    }
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
        // Per-mode boot keys: reject the *wrong* key instead of ignoring it,
        // so a profile that names a kernel under mode = "uefi" fails loudly
        // rather than booting something the author did not ask for.
        match self.boot.mode {
            BootMode::DirectLinux => {
                if self.boot.kernel.is_none() {
                    return err("boot.kernel is required for mode = \"direct-linux\"".into());
                }
                if self.boot.firmware.is_some() {
                    return err("boot.firmware is only valid for mode = \"uefi\"; \
                         direct-linux boots without firmware"
                        .into());
                }
            }
            BootMode::Uefi => {
                if self.boot.firmware.is_none() {
                    return err("boot.firmware is required for mode = \"uefi\"".into());
                }
                if self.boot.kernel.is_some() || self.boot.initramfs.is_some() {
                    return err("boot.kernel/boot.initramfs are only valid for mode = \
                         \"direct-linux\"; in uefi mode the firmware loads the guest"
                        .into());
                }
            }
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
interface = "entangled0"

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
        assert_eq!(
            cfg.boot.require_kernel().unwrap(),
            &PathBuf::from("artifacts/bootstrap/vmlinuz")
        );
        assert_eq!(cfg.disks.len(), 1);
        assert!(cfg.disks[0].writable);
        assert_eq!(cfg.network.as_ref().unwrap().interface, "entangled0");
        assert_eq!(cfg.display.width, 1920);
    }

    /// A profile written before the pci transport existed must keep meaning
    /// exactly what it meant: virtio-mmio.
    #[test]
    fn the_transport_defaults_to_mmio() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(cfg.transport, VirtioTransport::Mmio);
        assert!(!cfg.transport.is_pci());
    }

    #[test]
    fn the_transport_can_be_selected_and_round_trips() {
        let pci = BACKLOG_EXAMPLE.replace("vcpus = 2", "vcpus = 2\ntransport = \"pci\"");
        let cfg = VmConfig::from_toml(&pci).unwrap();
        assert_eq!(cfg.transport, VirtioTransport::Pci);
        assert!(cfg.transport.is_pci());
        assert_eq!(cfg.transport.to_string(), "pci");
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );

        // A typo is a hard error rather than a silent fall back to mmio: a VM
        // whose devices the guest cannot find looks like a device bug.
        let typo = BACKLOG_EXAMPLE.replace("vcpus = 2", "vcpus = 2\ntransport = \"pcie\"");
        assert!(matches!(
            VmConfig::from_toml(&typo),
            Err(ConfigError::Parse(_))
        ));
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

    /// EPIC 18 / ADR-0003: a UEFI profile names a firmware image and no kernel.
    const UEFI_EXAMPLE: &str = r#"
name = "ubuntu-uefi"
memory_mib = 4096
vcpus = 2

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[[disk]]
path = "images/ubuntu.raw"
writable = true
"#;

    #[test]
    fn parses_uefi_profile() {
        let cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert_eq!(
            cfg.boot.require_firmware().unwrap(),
            &PathBuf::from("artifacts/firmware/CLOUDHV.fd")
        );
        assert!(cfg.boot.kernel.is_none());
        assert!(cfg.boot.require_kernel().is_err());
    }

    #[test]
    fn boot_keys_must_match_the_mode() {
        // uefi without firmware
        let no_fw = UEFI_EXAMPLE.replace(r#"firmware = "artifacts/firmware/CLOUDHV.fd""#, "");
        assert!(matches!(
            VmConfig::from_toml(&no_fw),
            Err(ConfigError::Invalid(_))
        ));
        // uefi *and* a kernel: ambiguous, refuse it
        let both = UEFI_EXAMPLE.replace(
            r#"mode = "uefi""#,
            "mode = \"uefi\"\nkernel = \"artifacts/bootstrap/vmlinuz\"",
        );
        assert!(matches!(
            VmConfig::from_toml(&both),
            Err(ConfigError::Invalid(_))
        ));
        // direct-linux with a firmware key
        let stray = BACKLOG_EXAMPLE.replace(
            r#"mode = "direct-linux""#,
            "mode = \"direct-linux\"\nfirmware = \"CLOUDHV.fd\"",
        );
        assert!(matches!(
            VmConfig::from_toml(&stray),
            Err(ConfigError::Invalid(_))
        ));
        // direct-linux without a kernel
        let no_kernel = BACKLOG_EXAMPLE.replace(r#"kernel = "artifacts/bootstrap/vmlinuz""#, "");
        assert!(matches!(
            VmConfig::from_toml(&no_kernel),
            Err(ConfigError::Invalid(_))
        ));
    }

    /// A UEFI profile has no `kernel`; the serializer must not choke on the
    /// `None` (bare `Option` in a TOML table is an error without `skip`).
    #[test]
    fn uefi_profile_round_trips_through_toml() {
        let cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !text.contains("kernel"),
            "unexpected kernel key in:\n{text}"
        );
        assert_eq!(VmConfig::from_toml(&text).unwrap(), cfg);
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
