//! The Edit-VM form: a profile loaded into edit-friendly fields, mutated by
//! the modal, and written back **through `control-api` types** — parse,
//! mutate, serialize, re-validate, write; never a string edit. All logic
//! lives here so it tests without a window; the modal only renders fields.

use std::path::{Path, PathBuf};

use control_api::{
    BootMode, CdromSection, NetworkBackend, NetworkSection, SoundBackend, SoundSection,
    VirtioTransport, VmConfig,
};

use crate::backend::Backend;

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
    /// `[sound] enabled` — whether the guest gets a virtio-snd card at all.
    pub sound: bool,
    /// `[sound] backend`. Kept across an on/off toggle, the way the profile
    /// keeps it: turning the card off and on again must not silently reset a
    /// deliberate choice.
    pub sound_backend: SoundBackend,
    /// `[gamepad] enabled` — whether the guest gets a virtio-input gamepad
    /// (GAME-2104). `[gamepad] backend` is deliberately **not** modelled: the
    /// form leaves it in [`Self::base`], so a profile that names `evdev` or
    /// `xinput` by hand keeps it across a toggle, and everyone else gets
    /// `auto`, which probes what this host has and never prevents a start.
    pub gamepad: bool,

    /// `[gamepad] players` — how many pads the guest gets, one virtio slot
    /// each. Only meaningful while [`Self::gamepad`] is on; kept across a
    /// toggle so switching the pad off and on again does not silently drop a
    /// second player.
    pub gamepad_players: u8,
    /// The `[[disk]]` list, editable in place (order is guest device order).
    pub disks: Vec<control_api::DiskSection>,
    /// Buffer for the "add disk" field.
    pub add_disk: String,
    /// Where this machine runs. Not part of the profile — see
    /// [`crate::settings::Settings::vm_backends`] — but edited here because it
    /// decides which of the other fields are even available.
    pub backend: Backend,
    /// The backend the machine had when the form opened, so "has anything
    /// changed?" covers the one field that is not part of the profile.
    pub base_backend: Backend,
    /// The working directory a launch would use, so a relative path in the form
    /// can be resolved for the "is this file actually there?" badge. Never
    /// written anywhere; presentation only.
    pub work_dir: PathBuf,
    /// The untouched parse, for everything the form does not model.
    base: VmConfig,
}

impl EditForm {
    /// A path field resolved the way the engine will resolve it: as given when
    /// absolute, against the working directory when relative.
    pub fn resolve(&self, value: &str) -> PathBuf {
        let path = PathBuf::from(value.trim());
        if path.is_absolute() {
            path
        } else {
            self.work_dir.join(path)
        }
    }

    /// The NVRAM file this machine should have if it does not name one: beside
    /// its first disk, named after the machine. That is the convention
    /// `disk-image` already uses for the sidecar, so the file travels with the
    /// disk when it is moved or deleted.
    pub fn suggested_nvram(&self) -> PathBuf {
        match self.disks.first() {
            Some(disk) => disk_image::nvram_sidecar_path(&disk.path),
            None => PathBuf::from(format!("{}.nvram", self.name)),
        }
    }

    /// The firmware to offer when a UEFI machine names none.
    ///
    /// Whatever this computer actually has, by the same lookup the CLI uses —
    /// so on an installed copy the button offers the absolute path of the
    /// firmware that shipped with the program rather than a relative path that
    /// resolves to nothing. Only when nothing is found does it fall back to the
    /// relative path every generated profile has always carried, which at least
    /// tells the reader what the file is called.
    pub fn suggested_firmware(&self) -> PathBuf {
        match crate::launcher::locate_firmware(&self.work_dir) {
            Some((path, _)) => path,
            None => PathBuf::from(crate::launcher::UEFI_FIRMWARE),
        }
    }
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

[sound]
enabled = true
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
            sound: cfg.sound.enabled,
            sound_backend: cfg.sound.backend,
            gamepad: cfg.gamepad.enabled,
            gamepad_players: cfg
                .gamepad
                .players
                .clamp(1, control_api::MAX_GAMEPAD_PLAYERS),
            disks: cfg.disks.clone(),
            add_disk: String::new(),
            backend: Backend::Native,
            base_backend: Backend::Native,
            work_dir: PathBuf::from("."),
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
        cfg.sound = SoundSection {
            enabled: self.sound,
            backend: self.sound_backend,
        };
        // Only the switch: `backend` stays whatever the profile said, because
        // the form does not offer it and silently rewriting a field nobody was
        // shown is how a hand-edited profile loses its choice.
        cfg.gamepad.enabled = self.gamepad;
        cfg.gamepad.players = self
            .gamepad_players
            .clamp(1, control_api::MAX_GAMEPAD_PLAYERS);
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

    /// True when the form currently differs from what was loaded — profile
    /// *or* backend, since Save writes both.
    pub fn dirty(&self) -> bool {
        if self.backend != self.base_backend {
            return true;
        }
        match self.to_config() {
            Ok(cfg) => cfg != self.base,
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_api::GamepadBackend;

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

    /// A machine with no `[sound]` section must keep having none until the
    /// editor is actually asked for a card — and once asked, the chosen output
    /// must survive an off/on toggle rather than snapping back to `auto`.
    #[test]
    fn the_sound_card_is_opt_in_and_remembers_its_output() {
        let path = temp_profile("sound");
        let mut form = EditForm::from_profile(&path).expect("load");
        assert!(!form.sound, "a profile with no [sound] has no card");
        assert_eq!(form.sound_backend, SoundBackend::Auto);
        assert!(!form.dirty());

        form.sound = true;
        form.sound_backend = SoundBackend::Null;
        assert!(form.dirty());
        form.save().expect("save");

        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert!(cfg.sound.enabled);
        assert_eq!(cfg.sound.backend, SoundBackend::Null);

        // Off again: the card goes, the choice stays, and the profile still
        // parses (`backend` is meaningless while disabled, never refused).
        let mut form = EditForm::from_profile(&path).expect("reload");
        form.sound = false;
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert!(!cfg.sound.enabled);
        assert_eq!(cfg.sound.backend, SoundBackend::Null);
    }

    /// The gamepad is the same opt-in switch as the sound card, with one
    /// difference the form has to honour: it does **not** offer `backend`, so a
    /// profile that names one by hand must come back out with it intact. A
    /// checkbox that quietly rewrote a neighbouring field would be the worst
    /// kind of editor.
    #[test]
    fn the_gamepad_is_opt_in_and_leaves_a_hand_picked_backend_alone() {
        let path = temp_profile("gamepad");
        let mut form = EditForm::from_profile(&path).expect("load");
        assert!(!form.gamepad, "a profile with no [gamepad] has none");
        assert!(!form.dirty());

        form.gamepad = true;
        assert!(form.dirty());
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert!(cfg.gamepad.enabled);
        assert_eq!(cfg.gamepad.backend, GamepadBackend::Auto);

        // A backend the form never shows survives both directions of the
        // toggle, because `to_config` only writes the switch.
        std::fs::write(
            &path,
            format!("{PROFILE}\n[gamepad]\nenabled = true\nbackend = \"null\"\n"),
        )
        .expect("hand edit");
        let mut form = EditForm::from_profile(&path).expect("reload");
        assert!(form.gamepad);
        form.gamepad = false;
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert!(!cfg.gamepad.enabled);
        assert_eq!(cfg.gamepad.backend, GamepadBackend::Null);
    }

    /// The player count is the one gamepad field the form *does* own, so it
    /// has to round-trip both ways and stay inside the range the config
    /// validator will accept.
    #[test]
    fn the_gamepad_player_count_round_trips_and_stays_in_range() {
        let path = temp_profile("players");
        let mut form = EditForm::from_profile(&path).expect("load");
        assert_eq!(form.gamepad_players, 1, "one player unless asked");

        form.gamepad = true;
        form.gamepad_players = 2;
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert_eq!(cfg.gamepad.players, 2);
        assert_eq!(
            EditForm::from_profile(&path)
                .expect("reload")
                .gamepad_players,
            2
        );

        // A count out of range can only come from a hand-edited form (the
        // picker offers 1..=MAX), and it must be clamped rather than written
        // out to produce a profile the CLI would then refuse to start.
        let mut form = EditForm::from_profile(&path).expect("reload");
        form.gamepad_players = 99;
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert_eq!(cfg.gamepad.players, control_api::MAX_GAMEPAD_PLAYERS);

        // Turning the pad off keeps the count, so switching back on does not
        // silently lose player two.
        let mut form = EditForm::from_profile(&path).expect("reload");
        form.gamepad_players = 2;
        form.gamepad = false;
        form.save().expect("save");
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("reload");
        assert!(!cfg.gamepad.enabled);
        assert_eq!(cfg.gamepad.players, 2);
    }

    /// The picker never offers the other host's word, because naming it is a
    /// refusal at start rather than a fallback — but a profile that already
    /// names it has to explain itself instead of being silently rewritten.
    #[test]
    fn only_this_engines_own_sound_backend_is_offered() {
        for backend in [Backend::Native, Backend::Wsl] {
            let offered = backend.sound_backends();
            assert!(offered.contains(&SoundBackend::Auto));
            assert!(offered.contains(&SoundBackend::Null));
            let native = backend.native_sound_backend();
            assert_eq!(
                native,
                if backend.is_linux_kvm() {
                    SoundBackend::Alsa
                } else {
                    SoundBackend::Wasapi
                }
            );
            assert!(offered.contains(&native));
            let foreign = if native == SoundBackend::Alsa {
                SoundBackend::Wasapi
            } else {
                SoundBackend::Alsa
            };
            assert!(!offered.contains(&foreign), "{backend:?} offered {foreign}");

            // Neither of the always-safe words is ever flagged...
            assert!(backend.sound_backend_block(SoundBackend::Auto).is_none());
            assert!(backend.sound_backend_block(SoundBackend::Null).is_none());
            assert!(backend.sound_backend_block(native).is_none());
            // ...and the foreign one always is, with a fix in the short line.
            let reason = backend
                .sound_backend_block(foreign)
                .expect("the other host's backend is blocked");
            assert!(reason.short.contains("auto"), "{}", reason.short);
            assert!(reason.short.len() < 90, "{}", reason.short);
        }
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
