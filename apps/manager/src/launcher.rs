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

/// Everything the wizard collects (GUI-1602).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMachine {
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub disk_gib: u64,
    pub variant: String,
    pub automated: bool,
    pub headless: bool,
}

impl NewMachine {
    pub fn disk_path(&self, vm_dir: &Path) -> PathBuf {
        vm_dir.join(format!("{}.raw", self.name))
    }

    pub fn profile_path(&self, vm_dir: &Path) -> PathBuf {
        vm_dir.join(format!("{}.toml", self.name))
    }
}

/// `entangled install debian --disk <dir>/<name>.raw …` (GUI-1602).
///
/// The installer VM gets at least 1536 MiB — Debian's installer needs it even
/// when the finished machine is meant to be smaller.
pub fn install_spec(cli: &Path, vm_dir: &Path, machine: &NewMachine) -> TaskSpec {
    let mut args = vec![
        "install".to_string(),
        "debian".to_string(),
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
        cwd: vm_dir.to_path_buf(),
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

/// `entangled install` runs with the VM directory as its working directory, so
/// a profile written by it uses relative paths for the bootstrap kernel. That
/// only resolves if the kernel is reachable from the child's cwd — surfaced as
/// a warning in the UI before an install starts.
pub fn bootstrap_kernel_missing(cwd: &Path) -> bool {
    !cwd.join("artifacts/bootstrap/vmlinuz").is_file()
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
            variant: "text-netboot".into(),
            automated: true,
            headless: false,
        }
    }

    #[test]
    fn install_arguments_match_the_cli_surface() {
        let cli = PathBuf::from("/usr/bin/entangled");
        let vm_dir = Path::new("/vms");
        let spec = install_spec(&cli, vm_dir, &machine());

        assert_eq!(spec.kind, TaskKind::Install);
        assert_eq!(spec.vm, "demo");
        assert_eq!(spec.cwd, vm_dir);
        assert_eq!(spec.log_path, Path::new("/vms/demo-install.log"));
        let line = spec.command_line();
        for needle in [
            "install debian",
            "--disk /vms/demo.raw",
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
        let line = install_spec(&cli, Path::new("/vms"), &m).command_line();
        assert!(line.contains("--memory-mib 1536"), "{line}");
        assert!(line.contains("--headless"), "{line}");
        assert!(!line.contains("--auto"), "{line}");
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
