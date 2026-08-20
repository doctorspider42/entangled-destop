//! The Edit-VM form: a profile loaded into edit-friendly fields, mutated by
//! the modal, and written back **through `control-api` types** — parse,
//! mutate, serialize, re-validate, write; never a string edit. All logic
//! lives here so it tests without a window; the modal only renders fields.

use std::path::{Path, PathBuf};

use control_api::{
    BootMode, CdromSection, NetworkBackend, NetworkSection, VirtioTransport, VmConfig,
};

/// The network choice as the form shows it (a profile may have no `[network]`
/// at all, which an `Option<NetworkSection>` models but a combo box cannot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkChoice {
    None,
    Tap,
    Usernet,
}

impl NetworkChoice {
    pub const ALL: [NetworkChoice; 3] = [
        NetworkChoice::None,
        NetworkChoice::Tap,
        NetworkChoice::Usernet,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NetworkChoice::None => "none",
            NetworkChoice::Tap => "tap",
            NetworkChoice::Usernet => "usernet",
        }
    }
}

/// Everything the editor can change, plus the profile it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct EditForm {
    pub profile_path: PathBuf,
    /// Shown, never edited: renaming would desync the profile/disk/log file
    /// names the rest of the product derives from it.
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub transport: VirtioTransport,
    pub boot_mode: BootMode,
    // Path-ish fields as text buffers; empty means "absent".
    pub kernel: String,
    pub initramfs: String,
    pub cmdline: String,
    pub firmware: String,
    pub nvram: String,
    pub cdrom: String,
    pub network: NetworkChoice,
    pub interface: String,
    pub mac: String,
    pub display_width: u32,
    pub display_height: u32,
    pub virgl: bool,
    /// The `[[disk]]` list, editable in place (order is guest device order).
    pub disks: Vec<control_api::DiskSection>,
    /// Buffer for the "add disk" field.
    pub add_disk: String,
    /// The untouched parse, for everything the form does not model.
    base: VmConfig,
}

impl EditForm {
    pub fn from_profile(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
        Ok(Self::from_config(path.to_path_buf(), cfg))
    }

    /// A valid, editable profile that never needs to exist on disk. `--mock`
    /// uses it to exercise every editor section without touching user data.
    pub fn mock(name: &str) -> Result<Self, String> {
        let text = format!(
            r#"
name = {name:?}
memory_mib = 4096
vcpus = 4
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"
nvram = "mock-vms/{name}.nvram"

[[disk]]
path = "mock-vms/{name}.raw"
writable = true

[network]
backend = "usernet"

[display]
width = 1920
height = 1080
virgl = true
"#
        );
        let cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
        Ok(Self::from_config(
            PathBuf::from(format!("mock-vms/{name}.toml")),
            cfg,
        ))
    }

    fn from_config(profile_path: PathBuf, cfg: VmConfig) -> Self {
        let text_of = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        Self {
            profile_path,
            name: cfg.name.clone(),
            memory_mib: cfg.memory_mib,
            vcpus: cfg.vcpus,
            transport: cfg.transport,
            boot_mode: cfg.boot.mode,
            kernel: text_of(&cfg.boot.kernel),
            initramfs: text_of(&cfg.boot.initramfs),
            cmdline: cfg.boot.cmdline.clone(),
            firmware: text_of(&cfg.boot.firmware),
            nvram: text_of(&cfg.boot.nvram),
            cdrom: cfg
                .cdrom
                .as_ref()
                .map(|c| c.path.display().to_string())
                .unwrap_or_default(),
            network: match &cfg.network {
                None => NetworkChoice::None,
                Some(n) if n.backend == NetworkBackend::Tap => NetworkChoice::Tap,
                Some(_) => NetworkChoice::Usernet,
            },
            interface: cfg
                .network
                .as_ref()
                .and_then(|n| n.interface.clone())
                .unwrap_or_default(),
            mac: cfg
                .network
                .as_ref()
                .and_then(|n| n.mac.clone())
                .unwrap_or_default(),
            display_width: cfg.display.width,
            display_height: cfg.display.height,
            virgl: cfg.display.virgl,
            disks: cfg.disks.clone(),
            add_disk: String::new(),
            base: cfg,
        }
    }

    /// The form applied to the original config. Every rule `control-api`
    /// enforces is re-checked by serialize + reparse, so the error the modal
    /// shows is exactly the one `entangled run` would raise.
    pub fn to_config(&self) -> Result<VmConfig, String> {
        let path_of = |s: &str| -> Option<PathBuf> {
            let s = s.trim();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        let mut cfg = self.base.clone();
        cfg.memory_mib = self.memory_mib;
        cfg.vcpus = self.vcpus;
        cfg.transport = self.transport;
        cfg.boot.mode = self.boot_mode;
        cfg.boot.kernel = path_of(&self.kernel);
        cfg.boot.initramfs = path_of(&self.initramfs);
        cfg.boot.cmdline = self.cmdline.clone();
        cfg.boot.firmware = path_of(&self.firmware);
        cfg.boot.nvram = path_of(&self.nvram);
        cfg.cdrom = path_of(&self.cdrom).map(|path| CdromSection { path });
        cfg.network = match self.network {
            NetworkChoice::None => None,
            NetworkChoice::Tap => Some(NetworkSection {
                backend: NetworkBackend::Tap,
                interface: (!self.interface.trim().is_empty())
                    .then(|| self.interface.trim().to_string()),
                mac: (!self.mac.trim().is_empty()).then(|| self.mac.trim().to_string()),
            }),
            NetworkChoice::Usernet => Some(NetworkSection {
                backend: NetworkBackend::Usernet,
                interface: None,
                mac: (!self.mac.trim().is_empty()).then(|| self.mac.trim().to_string()),
            }),
        };
        cfg.display.width = self.display_width;
        cfg.display.height = self.display_height;
        cfg.display.virgl = self.virgl;
        cfg.disks = self.disks.clone();

        let out = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
        VmConfig::from_toml(&out).map_err(|e| e.to_string())
    }

    /// Validates and writes the edited profile.
    pub fn save(&self) -> Result<(), String> {
        let cfg = self.to_config()?;
        let out = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
        std::fs::write(&self.profile_path, out).map_err(|e| e.to_string())
    }

    /// True when the form currently differs from the profile on disk.
    pub fn dirty(&self) -> bool {
        match self.to_config() {
            Ok(cfg) => cfg != self.base,
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
name = "edit-me"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"
initramfs = "artifacts/bootstrap/initrd.img"
cmdline = "console=ttyS0 root=UUID=deadbeef rw"

[[disk]]
path = "edit-me.raw"
writable = true

[network]
backend = "tap"
interface = "entangled0"
"#;

    fn temp_profile(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "entangled-manager-tests/editor-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("edit-me.toml");
        std::fs::write(&path, PROFILE).expect("profile");
        path
    }

    #[test]
    fn loads_the_profile_into_fields() {
        let path = temp_profile("load");
        let form = EditForm::from_profile(&path).expect("load");
        assert_eq!(form.name, "edit-me");
        assert_eq!(form.memory_mib, 2048);
        assert_eq!(form.vcpus, 2);
        assert_eq!(form.transport, VirtioTransport::Mmio);
        assert_eq!(form.boot_mode, BootMode::DirectLinux);
        assert_eq!(form.kernel, "artifacts/bootstrap/vmlinuz");
        assert_eq!(form.network, NetworkChoice::Tap);
        assert_eq!(form.interface, "entangled0");
        assert!(!form.virgl);
        assert_eq!(form.disks.len(), 1);
        assert!(!form.dirty(), "an untouched form is clean");
    }

    #[test]
    fn mock_profile_is_valid_without_a_file() {
        let form = EditForm::mock("preview").expect("mock");
        assert_eq!(form.name, "preview");
        assert_eq!(form.boot_mode, BootMode::Uefi);
        assert!(form.to_config().is_ok());
        assert!(!form.profile_path.exists());
    }

    #[test]
    fn saves_resource_and_network_edits_through_control_api() {
        let path = temp_profile("save");
        let mut form = EditForm::from_profile(&path).expect("load");
        form.memory_mib = 4096;
        form.vcpus = 6;
        form.network = NetworkChoice::Usernet;
        form.interface = String::new();
        form.virgl = true;
        assert!(form.dirty());
        form.save().expect("save");

        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert_eq!(cfg.memory_mib, 4096);
        assert_eq!(cfg.vcpus, 6);
        assert_eq!(
            cfg.network.as_ref().unwrap().backend,
            NetworkBackend::Usernet
        );
        assert_eq!(cfg.network.as_ref().unwrap().interface, None);
        assert!(cfg.display.virgl);
        // Untouched parts survived.
        assert_eq!(cfg.boot.cmdline, "console=ttyS0 root=UUID=deadbeef rw");
        assert_eq!(cfg.disks.len(), 1);
    }

    #[test]
    fn a_boot_mode_switch_needs_the_matching_fields() {
        let path = temp_profile("uefi");
        let mut form = EditForm::from_profile(&path).expect("load");
        form.boot_mode = BootMode::Uefi;
        // Kernel still set, no firmware: control-api's own message surfaces.
        let error = form.to_config().expect_err("invalid combination");
        assert!(
            error.contains("boot.firmware") || error.contains("uefi"),
            "{error}"
        );

        // The valid shape: firmware set, kernel/initramfs cleared, pci
        // transport (the disk list is non-empty).
        form.kernel = String::new();
        form.initramfs = String::new();
        form.firmware = "artifacts/firmware/CLOUDHV.fd".into();
        form.transport = VirtioTransport::Pci;
        let cfg = form.to_config().expect("valid uefi profile");
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        form.save().expect("save");
    }

    #[test]
    fn tap_without_interface_surfaces_the_validation_message() {
        let path = temp_profile("tap");
        let mut form = EditForm::from_profile(&path).expect("load");
        form.interface = "  ".into();
        let error = form.to_config().expect_err("tap needs an interface");
        assert!(error.contains("network.interface"), "{error}");
        // And the profile on disk was never touched by the failed edit.
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("entangled0"));
    }

    #[test]
    fn disk_edits_ride_the_same_form() {
        let path = temp_profile("disks");
        let mut form = EditForm::from_profile(&path).expect("load");
        form.disks.push(control_api::DiskSection {
            path: PathBuf::from("scratch.raw"),
            writable: false,
        });
        form.disks[0].writable = false;
        form.save().expect("save");

        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert_eq!(cfg.disks.len(), 2);
        assert!(!cfg.disks[0].writable);
        assert_eq!(cfg.disks[1].path, PathBuf::from("scratch.raw"));
    }
}
