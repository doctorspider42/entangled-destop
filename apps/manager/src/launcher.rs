//! Locating the `entangled` CLI and turning UI intent into a [`TaskSpec`].

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::discovery::VmEntry;
use crate::process::{TaskKind, TaskSpec};
use crate::settings::Settings;

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("the configured entangled binary {0} does not exist")]
    OverrideMissing(PathBuf),

    #[error(
        "cannot find the 'entangled' binary — put it next to entangled-manager, \
         on PATH, or set it in Settings"
    )]
    NotFound,
}

const BINARY: &str = if cfg!(windows) {
    "entangled.exe"
} else {
    "entangled"
};

/// Resolution order: explicit setting, the manager's own directory (that is how
/// `cargo build` and a release tarball lay them out), then `PATH`.
pub fn locate_cli(settings: &Settings) -> Result<PathBuf, LaunchError> {
    if let Some(explicit) = &settings.entangled_binary {
        return if explicit.is_file() {
            Ok(explicit.clone())
        } else {
            Err(LaunchError::OverrideMissing(explicit.clone()))
        };
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| {
        exe.parent()
            .map(Path::to_path_buf)
            .filter(|dir| dir.join(BINARY).is_file())
    }) {
        return Ok(dir.join(BINARY));
    }
    if let Some(found) = search_path(BINARY) {
        return Ok(found);
    }
    Err(LaunchError::NotFound)
}

fn search_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// The installer variants `entangled install --variant` accepts.
pub const VARIANTS: [&str; 3] = ["text-netboot", "gtk-netboot", "netinst-iso"];

/// Installation family exposed by the human-facing wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestFamily {
    Debian,
    Ubuntu,
}

impl GuestFamily {
    pub const fn cli_name(self) -> &'static str {
        match self {
            Self::Debian => "debian",
            Self::Ubuntu => "ubuntu",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Debian => "Debian",
            Self::Ubuntu => "Ubuntu",
        }
    }
}

/// Whether the installer receives a fresh sparse disk or an existing image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskMode {
    CreateNew,
    UseExisting,
}

/// Everything the wizard collects (GUI-1602).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMachine {
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub disk_gib: u64,
    /// Empty means `<name>.raw` inside the manager's VM directory.
    pub disk_path: String,
    pub disk_mode: DiskMode,
    pub family: GuestFamily,
    /// Optional local Ubuntu installer ISO. Empty uses the verified cache.
    pub iso_path: String,
    pub variant: String,
    pub automated: bool,
    pub headless: bool,
}

impl NewMachine {
    pub fn disk_path(&self, vm_dir: &Path) -> PathBuf {
        let configured = self.disk_path.trim();
        if configured.is_empty() {
            return vm_dir.join(format!("{}.raw", self.name));
        }
        let path = PathBuf::from(configured);
        if path.is_absolute() {
            path
        } else {
            vm_dir.join(path)
        }
    }
}

/// `entangled install debian --disk <dir>/<name>.raw …` (GUI-1602).
///
/// The installer VM gets at least 1536 MiB — Debian's installer needs it even
/// when the finished machine is meant to be smaller.
///
/// `cwd` is the working directory the child runs in, and it matters: the CLI
/// resolves the bootstrap kernel as the relative `artifacts/bootstrap/vmlinuz`,
/// and writes that same relative path into the profile it generates. Disk and
/// profile paths passed here are absolute, so they are unaffected.
pub fn install_spec(cli: &Path, vm_dir: &Path, cwd: PathBuf, machine: &NewMachine) -> TaskSpec {
    let mut args = vec![
        "install".to_string(),
        machine.family.cli_name().to_string(),
        "--disk".to_string(),
        machine.disk_path(vm_dir).display().to_string(),
        "--size".to_string(),
        format!("{}G", machine.disk_gib),
        "--variant".to_string(),
        machine.variant.clone(),
        "--memory-mib".to_string(),
        machine.memory_mib.max(1536).to_string(),
        "--name".to_string(),
        machine.name.clone(),
    ];
    if machine.family == GuestFamily::Ubuntu && !machine.iso_path.trim().is_empty() {
        args.push("--iso".to_string());
        args.push(machine.iso_path.trim().to_string());
    }
    if machine.automated {
        args.push("--auto".to_string());
    }
    if machine.headless {
        args.push("--headless".to_string());
    }

    TaskSpec {
        kind: TaskKind::Install,
        vm: machine.name.clone(),
        program: cli.to_path_buf(),
        args,
        cwd,
        log_path: vm_dir.join(format!("{}-install.log", machine.name)),
    }
}

/// `entangled run <profile>` (GUI-1603). The VM opens its own window; the
/// manager only tracks the child.
pub fn run_spec(cli: &Path, vm: &VmEntry, cwd: PathBuf, vm_dir: &Path) -> TaskSpec {
    TaskSpec {
        kind: TaskKind::Run,
        vm: vm.name.clone(),
        program: cli.to_path_buf(),
        args: vec!["run".to_string(), vm.profile_path.display().to_string()],
        cwd,
        log_path: vm_dir.join(format!("{}-run.log", vm.name)),
    }
}

/// The bootstrap kernel every VM boots from, relative to the child's working
/// directory. `entangled install` refuses to start without it and profiles
/// reference it by this relative path, so the UI checks for it up front.
pub const BOOTSTRAP_KERNEL: &str = "artifacts/bootstrap/vmlinuz";

pub fn bootstrap_kernel_missing(cwd: &Path) -> bool {
    !cwd.join(BOOTSTRAP_KERNEL).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> NewMachine {
        NewMachine {
            name: "demo".into(),
            memory_mib: 4096,
            vcpus: 4,
            disk_gib: 20,
            disk_path: String::new(),
            disk_mode: DiskMode::CreateNew,
            family: GuestFamily::Debian,
            iso_path: String::new(),
            variant: "text-netboot".into(),
            automated: true,
            headless: false,
        }
    }

    #[test]
    fn install_arguments_match_the_cli_surface() {
        let cli = PathBuf::from("/usr/bin/entangled");
        let vm_dir = Path::new("/vms");
        let spec = install_spec(&cli, vm_dir, PathBuf::from("/srv/entangled"), &machine());

        assert_eq!(spec.kind, TaskKind::Install);
        assert_eq!(spec.vm, "demo");
        // The child runs where the bootstrap kernel is, not in the VM directory.
        assert_eq!(spec.cwd, Path::new("/srv/entangled"));
        assert_eq!(spec.log_path, Path::new("/vms/demo-install.log"));
        let line = spec.command_line();
        // The disk path goes through Path::join, so the separator is the
        // host's — build the expected fragment the same way.
        let disk_arg = format!("--disk {}", vm_dir.join("demo.raw").display());
        for needle in [
            "install debian",
            disk_arg.as_str(),
            "--size 20G",
            "--variant text-netboot",
            "--memory-mib 4096",
            "--name demo",
            "--auto",
        ] {
            assert!(line.contains(needle), "missing {needle} in {line}");
        }
        assert!(!line.contains("--headless"));
    }

    #[test]
    fn installer_memory_has_a_floor_and_headless_is_optional() {
        let cli = PathBuf::from("entangled");
        let mut m = machine();
        m.memory_mib = 512;
        m.automated = false;
        m.headless = true;
        let line = install_spec(&cli, Path::new("/vms"), PathBuf::from("/srv"), &m).command_line();
        assert!(line.contains("--memory-mib 1536"), "{line}");
        assert!(line.contains("--headless"), "{line}");
        assert!(!line.contains("--auto"), "{line}");
    }

    #[test]
    fn ubuntu_local_iso_and_existing_disk_reach_the_cli() {
        let mut m = machine();
        m.family = GuestFamily::Ubuntu;
        m.iso_path = "/isos/ubuntu.iso".into();
        m.disk_mode = DiskMode::UseExisting;
        m.disk_path = "kept.raw".into();
        let line = install_spec(
            Path::new("entangled"),
            Path::new("/vms"),
            PathBuf::from("/srv"),
            &m,
        )
        .command_line();
        assert!(line.contains("install ubuntu"), "{line}");
        assert!(line.contains("--iso /isos/ubuntu.iso"), "{line}");
        assert!(
            line.contains(&Path::new("/vms").join("kept.raw").display().to_string()),
            "{line}"
        );
    }

    #[test]
    fn run_spec_points_at_the_profile() {
        let vm = VmEntry {
            name: "debian-demo".into(),
            profile_path: PathBuf::from("/vms/debian-demo.toml"),
            memory_mib: 2048,
            vcpus: 2,
            display: (1920, 1080),
            network_interface: Some("entangled0".into()),
            disks: vec![],
        };
        let spec = run_spec(
            &PathBuf::from("/usr/bin/entangled"),
            &vm,
            PathBuf::from("/srv/entangled"),
            Path::new("/vms"),
        );
        assert_eq!(spec.kind, TaskKind::Run);
        assert_eq!(
            spec.command_line(),
            "/usr/bin/entangled run /vms/debian-demo.toml"
        );
        assert_eq!(spec.cwd, Path::new("/srv/entangled"));
        assert_eq!(spec.log_path, Path::new("/vms/debian-demo-run.log"));
    }

    #[test]
    fn explicit_binary_must_exist() {
        let mut settings = Settings {
            entangled_binary: Some(PathBuf::from("/definitely/not/here/entangled")),
            ..Settings::default()
        };
        assert!(matches!(
            locate_cli(&settings),
            Err(LaunchError::OverrideMissing(_))
        ));

        // A real file is accepted as-is.
        let here = std::env::current_exe().expect("test binary path");
        settings.entangled_binary = Some(here.clone());
        assert_eq!(locate_cli(&settings).expect("located"), here);
    }
}
