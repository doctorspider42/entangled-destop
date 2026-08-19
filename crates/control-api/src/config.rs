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

/// Smallest guest this machine is willing to build.
pub const MIN_MEMORY_MIB: u64 = 128;

/// Largest guest this machine is willing to build: 64 GiB.
///
/// RAM up to 3072 MiB (`machine_x86::layout::MMIO_HOLE_START`) sits below the
/// 32-bit MMIO hole; anything above that continues at 4 GiB as a second memory
/// region (the high-RAM split — `vmm_core::create_guest_memory` and
/// `machine_x86::e820_map` agree on the shape). The 64 GiB ceiling is a sanity
/// bound, not an architectural one: a typo'd `memory_mib` should be a typed
/// config error before it becomes a 2 TiB `mmap`.
///
/// `apps/entangled` has the test that keeps this consistent with the machine
/// crate; control-api deliberately does not depend on it.
pub const MAX_MEMORY_MIB: u64 = 65536;

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
    /// Optional installer/live medium (an ISO), attached read-only as the last
    /// virtio-blk device — after every `[[disk]]`, so it never shifts the disks'
    /// guest-visible names. `uefi` mode only: the point of a CD-ROM is that the
    /// *firmware* boots it, and a direct-linux guest that merely wants the ISO's
    /// bytes should say what it means with a read-only `[[disk]]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdrom: Option<CdromSection>,
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
    /// `uefi` only: the VM's non-volatile UEFI variable store (UEFI-1804).
    ///
    /// One file per VM, created erased on first use and then owned by the guest
    /// firmware: `BootOrder`, the `Boot####` entries `grub-install` writes, and
    /// (if the firmware is built with secure boot) the key database. Without it
    /// the firmware keeps variables in RAM and an installed system loses its
    /// boot entry every time the VM stops — which is why `entangled install`
    /// always writes this key for a UEFI profile.
    ///
    /// The firmware image itself stays shared and pristine: it is never written
    /// through this path (see `machine_x86::pflash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvram: Option<PathBuf>,
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

impl Default for BootSection {
    /// A direct-Linux section with nothing chosen yet. Exists so that adding a
    /// key to this struct does not have to be threaded through every caller
    /// that builds one by hand (`entangled install` builds four).
    fn default() -> Self {
        Self {
            mode: BootMode::DirectLinux,
            kernel: None,
            initramfs: None,
            firmware: None,
            nvram: None,
            cmdline: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiskSection {
    pub path: PathBuf,
    #[serde(default)]
    pub writable: bool,
}

/// `[cdrom]` — one optional installer/live medium (UEFI-1803's machinery as a
/// first-class config key rather than a hand-written `[[disk]]` pair).
///
/// Always read-only — there is deliberately no `writable` key to get wrong: the
/// medium's value is that its provenance was verified
/// (`scripts/fetch-ubuntu-iso.sh`), and a VMM that can scribble on it destroys
/// exactly that.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CdromSection {
    pub path: PathBuf,
}

/// How the guest's virtio-net device reaches a real network (EPIC 5, WHP-1704).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkBackend {
    /// A host TAP interface (`scripts/setup-tap.sh`). Linux only — Windows has
    /// no TAP, and the drivers that would provide one are GPL (ADR-0002); the
    /// run path reports that rather than this crate, which validates the same
    /// config on every host.
    Tap,
    /// User-mode NAT inside the `entangled` process (smoltcp): DHCP, DNS relay
    /// and outbound TCP with no host interface, no `CAP_NET_ADMIN` and no
    /// administrator. The only backend on Windows, and the rootless option on
    /// Linux.
    Usernet,
}

impl std::fmt::Display for NetworkBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tap => f.write_str("tap"),
            Self::Usernet => f.write_str("usernet"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    pub backend: NetworkBackend,
    /// The host TAP interface. Required by `backend = "tap"`, meaningless (and
    /// therefore refused) for `backend = "usernet"`, whose segment lives inside
    /// the process and touches no host interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Optional fixed MAC ("52:00:…"); derived from the VM name when absent.
    pub mac: Option<String>,
}

impl NetworkSection {
    /// The TAP interface name, for `backend = "tap"` callers. Validation
    /// guarantees it is present for that backend; this accessor keeps the error
    /// typed for callers that build a section by hand.
    pub fn require_interface(&self) -> Result<&str, ConfigError> {
        self.interface.as_deref().ok_or_else(|| {
            ConfigError::Invalid("network.interface is required for backend = \"tap\"".into())
        })
    }
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

    /// Attaches (or replaces) the CD-ROM after parsing — the `--cdrom <iso>`
    /// path. Re-runs validation, because the combination rules (uefi mode, the
    /// pci transport) apply to the modified profile, not the one on disk.
    pub fn set_cdrom(&mut self, path: PathBuf) -> Result<(), ConfigError> {
        self.cdrom = Some(CdromSection { path });
        self.validate()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let err = |m: String| Err(ConfigError::Invalid(m));
        if self.name.is_empty() {
            return err("name must not be empty".into());
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return err(format!(
                "memory_mib {} outside supported range {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB}",
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
        // Per-backend network keys, same policy as the boot section: the wrong
        // key is refused rather than ignored, so a profile that names a TAP
        // interface under backend = "usernet" fails loudly instead of quietly
        // not using the interface its author configured.
        if let Some(network) = &self.network {
            match network.backend {
                NetworkBackend::Tap => {
                    if network.interface.is_none() {
                        return err("network.interface is required for backend = \"tap\"".into());
                    }
                }
                NetworkBackend::Usernet => {
                    if network.interface.is_some() {
                        return err("network.interface is only valid for backend = \"tap\"; \
                             the usernet segment lives inside the entangled process and uses \
                             no host interface"
                            .into());
                    }
                }
            }
        }
        // Per-mode boot keys: reject the *wrong* key instead of ignoring it,
        // so a profile that names a kernel under mode = "uefi" fails loudly
        // rather than booting something the author did not ask for.
        match self.boot.mode {
            BootMode::DirectLinux => {
                if self.cdrom.is_some() {
                    return err("cdrom is only valid for mode = \"uefi\": booting a CD-ROM \
                         means the firmware finds its bootloader, which direct-linux skips. \
                         To hand a direct-linux guest the ISO's bytes, use a [[disk]] with \
                         writable = false"
                        .into());
                }
                if self.boot.kernel.is_none() {
                    return err("boot.kernel is required for mode = \"direct-linux\"".into());
                }
                if self.boot.firmware.is_some() {
                    return err("boot.firmware is only valid for mode = \"uefi\"; \
                         direct-linux boots without firmware"
                        .into());
                }
                if self.boot.nvram.is_some() {
                    return err("boot.nvram is only valid for mode = \"uefi\"; \
                         a direct-linux guest has no UEFI variables to store"
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
                // A UEFI firmware cannot see a virtio-mmio device at all: EDK2's
                // CloudHv build ships VirtioPciDeviceDxe/Virtio10Dxe/VirtioBlkDxe
                // and no virtio-MMIO driver (ADR-0003). The combination is not
                // "slower" or "less featured", it is a firmware that boots
                // perfectly and then reports "No bootable option or device was
                // found" — which reads like a bug in the media, the ISO or the
                // block device, and sends the reader looking in four wrong
                // places. Refuse it while we still know why.
                if !self.transport.is_pci() && (!self.disks.is_empty() || self.cdrom.is_some()) {
                    let media = match (self.disks.len(), self.cdrom.is_some()) {
                        (0, _) => "the configured cdrom".to_string(),
                        (n, true) => format!("the {n} configured disk(s) and the cdrom"),
                        (n, false) => format!("the {n} configured disk(s)"),
                    };
                    return err(format!(
                        "mode = \"uefi\" needs transport = \"pci\": a UEFI firmware has no \
                         virtio-mmio driver, so {media} would be invisible to it and the \
                         boot would end at \"No bootable option or device was found\""
                    ));
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
        assert_eq!(
            cfg.network.as_ref().unwrap().interface.as_deref(),
            Some("entangled0")
        );
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
memory_mib = 2560
vcpus = 2
transport = "pci"

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

    /// UEFI-1803: the ISO boot profile — firmware, the pci transport, a writable
    /// target as `/dev/vda` and the installer ISO read-only as `/dev/vdb`.
    const UBUNTU_ISO_EXAMPLE: &str = r#"
name = "ubuntu-uefi"
memory_mib = 2560
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[[disk]]
path = "/home/you/entangled-vms/ubuntu.raw"
writable = true

[[disk]]
path = "/home/you/.cache/entangled/ubuntu/26.04/ubuntu-26.04-live-server-amd64.iso"
writable = false
"#;

    #[test]
    fn parses_the_ubuntu_iso_profile() {
        let cfg = VmConfig::from_toml(UBUNTU_ISO_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert!(cfg.transport.is_pci());
        // Disk order is device order: 00:01.0 is /dev/vda, 00:02.0 is /dev/vdb.
        assert_eq!(cfg.disks.len(), 2);
        assert!(cfg.disks[0].writable, "the install target must be writable");
        assert!(
            !cfg.disks[1].writable,
            "the installer ISO must be attached read-only"
        );
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );
    }

    /// `writable` defaults to false, so an ISO section that simply omits the key
    /// is read-only rather than accidentally writable. This is the direction a
    /// default must fail in.
    #[test]
    fn a_disk_without_writable_is_read_only() {
        let cfg = VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("writable = false", "# no writable key here"),
        )
        .unwrap();
        assert!(!cfg.disks[1].writable);
    }

    /// A UEFI profile with disks on virtio-mmio describes a machine whose
    /// firmware cannot see its own boot media (ADR-0003: CloudHv ships no
    /// virtio-MMIO driver). Refused at parse time, because the symptom — "No
    /// bootable option or device was found" — looks like a media problem.
    #[test]
    fn uefi_with_disks_requires_the_pci_transport() {
        let mmio = UBUNTU_ISO_EXAMPLE.replace("transport = \"pci\"", "");
        let error = VmConfig::from_toml(&mmio).expect_err("mmio + uefi + disks must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("transport = \"pci\""), "{message}");

        // Explicit mmio is refused the same way as the default.
        assert!(matches!(
            VmConfig::from_toml(&UBUNTU_ISO_EXAMPLE.replace("\"pci\"", "\"mmio\"")),
            Err(ConfigError::Invalid(_))
        ));

        // But a firmware-only profile — no disks at all, which is how the
        // firmware bring-up boots to the Boot Manager (examples/uefi-firmware.toml)
        // — stays valid on the default transport: there is no media for the
        // missing driver to miss.
        let cfg = VmConfig::from_toml(
            r#"
name = "uefi-firmware"
memory_mib = 2048
vcpus = 1

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"
"#,
        )
        .expect("a diskless uefi profile is valid on mmio");
        assert!(cfg.disks.is_empty());
        assert_eq!(cfg.transport, VirtioTransport::Mmio);
    }

    /// Guests above 3 GiB are legal since the high-RAM split (the machine
    /// continues RAM at 4 GiB); the ceiling is a sanity bound at 64 GiB, and it
    /// must stay a typed config error, not a panic three crates away.
    #[test]
    fn memory_bounds_allow_the_high_ram_split_and_stop_at_the_sanity_cap() {
        assert_eq!(MAX_MEMORY_MIB, 65536, "64 GiB sanity bound");
        // The GNOME-desktop-sized guest that motivated the split.
        assert!(VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 4096")
        )
        .is_ok());
        // Exactly at the bound is fine; one MiB over is not.
        assert!(VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 65536")
        )
        .is_ok());
        let too_big = UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 65537");
        let error = VmConfig::from_toml(&too_big).expect_err("65 GiB must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("supported range"), "{message}");
    }

    /// The cdrom section: a UEFI profile with `[cdrom]` and no disks at all is
    /// the generic "boot this ISO" machine (`entangled run --cdrom`).
    const CDROM_EXAMPLE: &str = r#"
name = "iso-boot"
memory_mib = 2560
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[cdrom]
path = "/home/you/.cache/entangled/ubuntu/26.04/ubuntu-26.04-desktop-amd64.iso"
"#;

    #[test]
    fn a_cdrom_profile_parses_and_round_trips() {
        let cfg = VmConfig::from_toml(CDROM_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert!(cfg.disks.is_empty());
        let cdrom = cfg.cdrom.as_ref().expect("cdrom section");
        assert!(cdrom.path.to_string_lossy().ends_with(".iso"));
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );
        // And a profile without one serializes without the key.
        let plain = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        assert!(plain.cdrom.is_none());
        let text = toml::to_string_pretty(&plain).unwrap();
        assert!(!text.contains("cdrom"), "stray cdrom key in:\n{text}");
    }

    /// There is no `writable` key to get wrong: a cdrom section that tries to
    /// name one is refused at parse time (`deny_unknown_fields`).
    #[test]
    fn a_cdrom_cannot_be_made_writable() {
        let with_writable = CDROM_EXAMPLE.replace("[cdrom]", "[cdrom]\nwritable = true");
        assert!(matches!(
            VmConfig::from_toml(&with_writable),
            Err(ConfigError::Parse(_))
        ));
    }

    /// The same transport rule the disks obey: firmware cannot see virtio-mmio,
    /// so a cdrom on the default transport would boot to "no bootable option".
    #[test]
    fn a_cdrom_requires_uefi_and_the_pci_transport() {
        let mmio = CDROM_EXAMPLE.replace("transport = \"pci\"", "");
        let error = VmConfig::from_toml(&mmio).expect_err("mmio + cdrom must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("transport = \"pci\""), "{message}");
        assert!(message.contains("cdrom"), "{message}");

        // On direct-linux the section is a category error, and the message says
        // what to use instead.
        let direct =
            BACKLOG_EXAMPLE.replace("[network]", "[cdrom]\npath = \"/isos/x.iso\"\n\n[network]");
        let error = VmConfig::from_toml(&direct).expect_err("cdrom on direct-linux");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("uefi"), "{message}");
        assert!(message.contains("[[disk]]"), "{message}");
    }

    /// `set_cdrom` is the `--cdrom <iso>` path: it must re-validate, so a flag
    /// added to a profile the combination rules refuse fails like the profile
    /// would, not at boot time.
    #[test]
    fn set_cdrom_revalidates_the_modified_profile() {
        let mut cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        cfg.set_cdrom(PathBuf::from("/isos/x.iso")).unwrap();
        assert_eq!(
            cfg.cdrom.as_ref().unwrap().path,
            PathBuf::from("/isos/x.iso")
        );
        // Replacing an existing cdrom is allowed — the flag wins.
        cfg.set_cdrom(PathBuf::from("/isos/y.iso")).unwrap();
        assert_eq!(
            cfg.cdrom.as_ref().unwrap().path,
            PathBuf::from("/isos/y.iso")
        );

        let mut direct = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        let error = direct
            .set_cdrom(PathBuf::from("/isos/x.iso"))
            .expect_err("cdrom on a direct-linux profile");
        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
    }

    /// UEFI-1804: the profile `entangled install ubuntu` writes names an NVRAM
    /// file, and that key must round-trip and stay uefi-only.
    #[test]
    fn the_nvram_key_is_uefi_only_and_round_trips() {
        let with_nvram = UBUNTU_ISO_EXAMPLE.replace(
            r#"firmware = "artifacts/firmware/CLOUDHV.fd""#,
            "firmware = \"artifacts/firmware/CLOUDHV.fd\"\nnvram = \"/vms/ubuntu.nvram\"",
        );
        let cfg = VmConfig::from_toml(&with_nvram).unwrap();
        assert_eq!(cfg.boot.nvram, Some(PathBuf::from("/vms/ubuntu.nvram")));
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );

        // Absent is fine — that is a firmware boot with RAM-only variables.
        assert_eq!(
            VmConfig::from_toml(UBUNTU_ISO_EXAMPLE).unwrap().boot.nvram,
            None
        );

        // On direct-linux it is a mistake worth naming: nothing would ever read
        // the file, so a profile that names one is not describing what it thinks.
        let stray = BACKLOG_EXAMPLE.replace(
            r#"mode = "direct-linux""#,
            "mode = \"direct-linux\"\nnvram = \"/vms/x.nvram\"",
        );
        let error = VmConfig::from_toml(&stray).expect_err("nvram on direct-linux must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("boot.nvram"), "{message}");
    }

    /// WHP-1704: the user-mode NAT backend is a first-class config choice, and
    /// the section round-trips without an interface key.
    #[test]
    fn usernet_is_a_backend_and_needs_no_interface() {
        let usernet = BACKLOG_EXAMPLE.replace(
            "backend = \"tap\"\ninterface = \"entangled0\"",
            "backend = \"usernet\"",
        );
        let cfg = VmConfig::from_toml(&usernet).unwrap();
        let network = cfg.network.as_ref().unwrap();
        assert_eq!(network.backend, NetworkBackend::Usernet);
        assert_eq!(network.interface, None);
        assert!(network.require_interface().is_err());
        assert_eq!(network.backend.to_string(), "usernet");
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(!text.contains("interface"), "no interface key in:\n{text}");
        assert_eq!(VmConfig::from_toml(&text).unwrap(), cfg);
    }

    /// The wrong network key for the backend is refused, in both directions —
    /// same policy as the boot section's per-mode keys.
    #[test]
    fn network_keys_must_match_the_backend() {
        // usernet with a TAP interface: the author configured something the
        // backend would silently ignore.
        let stray = BACKLOG_EXAMPLE.replace("backend = \"tap\"", "backend = \"usernet\"");
        let error = VmConfig::from_toml(&stray).expect_err("usernet + interface must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("usernet"), "{message}");

        // tap without an interface: nothing to open.
        let missing = BACKLOG_EXAMPLE.replace("interface = \"entangled0\"", "");
        let error = VmConfig::from_toml(&missing).expect_err("tap without interface");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("network.interface"), "{message}");

        // The tap example still parses and still names its interface.
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(
            cfg.network.as_ref().unwrap().require_interface().unwrap(),
            "entangled0"
        );
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
