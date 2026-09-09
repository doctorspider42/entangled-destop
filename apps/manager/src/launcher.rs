//! Finding the Entangled engine, and turning UI intent into a [`TaskSpec`].
//!
//! "Engine" is what the product calls the `entangled` binary in front of a
//! user: the manager is the application, the engine is the part that actually
//! runs a machine. A GUI user should never have to know where it lives, so
//! resolution happens here and the result is reported as *status*, not asked
//! for as a setting (the override still exists, under Advanced).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use thiserror::Error;

use crate::backend::{self, Backend};
use crate::discovery::VmEntry;
use crate::process::{TaskKind, TaskSpec, Waker};
use crate::settings::Settings;

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("the engine you chose in Settings is not there any more: {0}")]
    OverrideMissing(PathBuf),

    #[error(
        "the Entangled engine is missing — it normally sits next to this manager. \
         Reinstall Entangled Desktop, or locate the file yourself under \
         Settings ▸ Advanced."
    )]
    NotFound,
}

const BINARY: &str = if cfg!(windows) {
    "entangled.exe"
} else {
    "entangled"
};

/// How the engine was found — the sentence the Settings panel shows instead of
/// a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineOrigin {
    /// An explicit path from Settings ▸ Advanced.
    Chosen,
    /// The normal case: shipped alongside the manager.
    BesideManager,
    /// Found on `PATH` — a developer checkout, usually.
    OnPath,
}

impl EngineOrigin {
    pub const fn label(self) -> &'static str {
        match self {
            EngineOrigin::Chosen => "chosen in Settings",
            EngineOrigin::BesideManager => "next to the manager",
            EngineOrigin::OnPath => "found on PATH",
        }
    }
}

/// A resolved engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    pub path: PathBuf,
    pub origin: EngineOrigin,
    /// Filled in by the background version probe; `None` until it answers.
    pub version: Option<String>,
}

impl Engine {
    /// The one-line status: `"entangled 0.2.0 — next to the manager"`. The tick
    /// is added by the view, which owns the colour.
    pub fn summary(&self) -> String {
        match &self.version {
            Some(version) => format!("entangled {version} — {}", self.origin.label()),
            None => format!("entangled — {}", self.origin.label()),
        }
    }
}

/// Resolution order: explicit setting, the manager's own directory (that is how
/// `cargo build` and a release install lay them out), then `PATH`.
pub fn locate_engine(settings: &Settings) -> Result<Engine, LaunchError> {
    if let Some(explicit) = &settings.entangled_binary {
        return if explicit.is_file() {
            Ok(Engine {
                path: explicit.clone(),
                origin: EngineOrigin::Chosen,
                version: None,
            })
        } else {
            Err(LaunchError::OverrideMissing(explicit.clone()))
        };
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| {
        exe.parent()
            .map(Path::to_path_buf)
            .filter(|dir| dir.join(BINARY).is_file())
    }) {
        return Ok(Engine {
            path: dir.join(BINARY),
            origin: EngineOrigin::BesideManager,
            version: None,
        });
    }
    if let Some(found) = search_path(BINARY) {
        return Ok(Engine {
            path: found,
            origin: EngineOrigin::OnPath,
            version: None,
        });
    }
    Err(LaunchError::NotFound)
}

fn search_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// Asks the engine its version, on a worker thread.
///
/// One `--version` costs a process spawn; on the frame loop that is a visible
/// hitch on a cold file cache, so it goes where every other slow thing in this
/// crate goes. Failure is silence: an engine that cannot say its version still
/// runs machines, and the status line simply omits the number.
pub fn spawn_version_probe(engine: PathBuf, waker: Waker) -> mpsc::Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let builder = std::thread::Builder::new().name("engine-version".to_string());
    let spawned = builder.spawn(move || {
        let mut command = std::process::Command::new(&engine);
        command.arg("--version");
        crate::process::quiet_command(&mut command);
        let version = command
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| parse_version(&String::from_utf8_lossy(&out.stdout)));
        let _ = tx.send(version);
        waker();
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot probe the engine version");
    }
    rx
}

/// `"entangled 0.2.0\n"` → `"0.2.0"`. Anything unexpected yields `None` rather
/// than a half-parsed string in the status line.
fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// How a child is actually started: directly, or through `wsl.exe`.
///
/// Built from the settings once per action, so the two spellings of "run this
/// engine command" live in one place and every path argument goes through the
/// same translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub backend: Backend,
    /// The native engine. Used directly by [`Backend::Native`]; still carried
    /// for the WSL case so the UI can report both halves.
    pub engine: PathBuf,
    pub distro: String,
    /// Path to the Linux build of `entangled` *inside* WSL. Empty means "let
    /// the WSL PATH decide".
    pub linux_engine: String,
}

impl Runner {
    pub fn new(backend: Backend, engine: PathBuf, settings: &Settings) -> Self {
        Self {
            backend,
            engine,
            distro: settings.wsl_distro.clone(),
            linux_engine: settings.wsl_entangled.clone().unwrap_or_default(),
        }
    }

    /// A path as the engine will see it. Identity natively; `/mnt/...` under
    /// WSL, and a typed refusal when there is no such translation.
    pub fn path_arg(&self, path: &Path) -> Result<String, String> {
        match self.backend {
            Backend::Native => Ok(path.display().to_string()),
            Backend::Wsl => backend::to_wsl_path(path).map_err(|e| e.0),
        }
    }

    /// `(program, argv)` for `entangled doctor` — the Diagnostics panel's whole
    /// implementation. Public because the check runs outside the supervisor:
    /// it is not a machine, it produces output and exits.
    pub fn doctor_command(&self, cwd: &Path) -> Result<(PathBuf, Vec<String>), String> {
        self.command(cwd, vec!["doctor".to_string()])
    }

    /// `(program, argv)` for an engine command line.
    fn command(&self, cwd: &Path, args: Vec<String>) -> Result<(PathBuf, Vec<String>), String> {
        match self.backend {
            Backend::Native => Ok((self.engine.clone(), args)),
            Backend::Wsl => {
                let args = backend::wsl_args(&self.distro, &self.linux_engine, Some(cwd), &args);
                Ok((PathBuf::from(backend::WSL_PROGRAM), args))
            }
        }
    }
}

/// The installer variants `entangled install --variant` accepts. Debian only —
/// the Ubuntu path installs from a verified ISO and has no variants.
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

    /// What a new machine installs by default on *this* host.
    ///
    /// Ubuntu on Windows, because it is the only one that works there out of
    /// the box: it boots verified media through UEFI and installs offline,
    /// while the Debian path needs the project's own bootstrap kernel, which is
    /// a Linux kernel build that does not cross-build. Debian stays the Linux
    /// default — that is the MVP's target and what every existing profile there
    /// was installed with.
    pub const fn default_for_host() -> Self {
        if cfg!(windows) {
            Self::Ubuntu
        } else {
            Self::Debian
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
    /// Where this machine will run once it exists — and where its installer
    /// runs now.
    pub backend: Backend,
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

/// `entangled install <distro> --disk <dir>/<name>.raw …` (GUI-1602).
///
/// The installer VM gets at least 1536 MiB — Debian's installer needs it even
/// when the finished machine is meant to be smaller. (The Ubuntu path raises its
/// own floor to 2560 for subiquity and writes the requested size into the
/// installed profile, so passing the user's number through is right for both.)
///
/// `cwd` is the working directory the child runs in, and it matters: the CLI
/// resolves the bootstrap kernel and the UEFI firmware as relative paths
/// (`artifacts/bootstrap/vmlinuz`, `artifacts/firmware/CLOUDHV.fd`), and writes
/// the firmware path into the profile it generates.
///
/// `--network` is deliberately not passed: the CLI's per-host default is the
/// right answer (TAP on Linux, the in-process user-mode NAT on Windows), and a
/// GUI that pinned it would be wrong on one of the two hosts.
pub fn install_spec(
    runner: &Runner,
    vm_dir: &Path,
    cwd: PathBuf,
    machine: &NewMachine,
) -> Result<TaskSpec, String> {
    let mut args = vec![
        "install".to_string(),
        machine.family.cli_name().to_string(),
        "--disk".to_string(),
        runner.path_arg(&machine.disk_path(vm_dir))?,
        "--size".to_string(),
        format!("{}G", machine.disk_gib),
        "--memory-mib".to_string(),
        machine.memory_mib.max(1536).to_string(),
        "--name".to_string(),
        machine.name.clone(),
    ];
    // `--variant` names a Debian netboot flavour. The Ubuntu path takes an ISO
    // instead and ignores the flag entirely, so passing it would put a setting
    // on the reviewed command line that does nothing — worse than absent.
    match machine.family {
        GuestFamily::Debian => {
            args.push("--variant".to_string());
            args.push(machine.variant.clone());
        }
        GuestFamily::Ubuntu => {
            if !machine.iso_path.trim().is_empty() {
                args.push("--iso".to_string());
                args.push(runner.path_arg(Path::new(machine.iso_path.trim()))?);
            }
        }
    }
    if machine.automated {
        args.push("--auto".to_string());
    }
    if machine.headless {
        args.push("--headless".to_string());
    }

    let (program, args) = runner.command(&cwd, args)?;
    Ok(TaskSpec {
        kind: TaskKind::Install,
        vm: machine.name.clone(),
        program,
        args,
        cwd,
        log_path: vm_dir.join(format!("{}-install.log", machine.name)),
        control: false,
    })
}

/// `entangled run --control-stdin <profile>` (GUI-1603). The VM opens its own
/// window; the manager tracks the child and keeps its stdin as the lifecycle
/// control channel, which is what the Pause and Restart buttons write to
/// (ADR-0005).
///
/// Under the WSL backend the same pipe still works: `wsl.exe` forwards its
/// stdin to the Linux process, so Pause and Restart reach the guest exactly as
/// they do natively. What differs is the window — WSLg opens it, on its own
/// Wayland compositor.
pub fn run_spec(
    runner: &Runner,
    vm: &VmEntry,
    cwd: PathBuf,
    vm_dir: &Path,
) -> Result<TaskSpec, String> {
    let args = vec![
        "run".to_string(),
        "--control-stdin".to_string(),
        runner.path_arg(&vm.profile_path)?,
    ];
    let (program, args) = runner.command(&cwd, args)?;
    Ok(TaskSpec {
        kind: TaskKind::Run,
        vm: vm.name.clone(),
        program,
        args,
        cwd,
        log_path: vm_dir.join(format!("{}-run.log", vm.name)),
        control: true,
    })
}

/// `entangled resume --control-stdin <snapshot>` (ADR-0006): start a machine
/// **from its saved session** instead of booting it.
///
/// The snapshot carries the profile it was taken from, verbatim, so this needs
/// nothing else — not the `.toml`, which may since have been edited or deleted.
/// That is also why `vm` is passed separately: the supervisor keys a task by
/// machine name, and the name has to be the one the cards use, whether or not a
/// profile of that name still exists.
///
/// The control channel is the same pipe a `run` gets, so a resumed machine can
/// be paused, restarted and suspended again like any other. A bare `save` on it
/// writes back to the file it came from, which is what makes closing and
/// re-opening a machine a loop rather than a one-way trip.
pub fn resume_spec(
    runner: &Runner,
    vm: &str,
    snapshot: &Path,
    cwd: PathBuf,
    vm_dir: &Path,
) -> Result<TaskSpec, String> {
    let args = vec![
        "resume".to_string(),
        "--control-stdin".to_string(),
        runner.path_arg(snapshot)?,
    ];
    let (program, args) = runner.command(&cwd, args)?;
    Ok(TaskSpec {
        kind: TaskKind::Run,
        vm: vm.to_string(),
        program,
        args,
        cwd,
        // The same log a cold start writes to: for a person following a
        // machine, "resumed" and "started" are the same event in its history.
        log_path: vm_dir.join(format!("{vm}-run.log")),
        control: true,
    })
}

/// The bootstrap kernel a direct-Linux VM boots from, relative to the child's
/// working directory. A checkout that ran `guest/bootstrap-kernel/build.sh` has
/// it there, and the profiles `install debian` writes on such a host reference
/// it by exactly this relative path — which is why it is also the placeholder
/// in the machine editor's KERNEL field.
pub const BOOTSTRAP_KERNEL: &str = "artifacts/bootstrap/vmlinuz";

/// Its initramfs. Half a pair is not a pair: a kernel with no initramfs boots to
/// a panic, which is a worse failure than "not found".
pub const BOOTSTRAP_INITRD: &str = "artifacts/bootstrap/initrd.img";

/// The UEFI firmware an Ubuntu install boots through, same story: relative to the
/// child's working directory, named in the profile that comes out.
pub const UEFI_FIRMWARE: &str = "artifacts/firmware/CLOUDHV.fd";

/// Its file name on its own, for the three-place lookup below.
const FIRMWARE_FILE: &str = "CLOUDHV.fd";

/// How the UEFI firmware is obtained, in words a user can act on.
///
/// It used to say "copy artifacts/firmware/CLOUDHV.fd in from a Linux checkout
/// or a release", which was accurate and useless: somebody who installed
/// Entangled Desktop from the Windows installer has neither. The firmware now
/// ships *inside* that installer and is downloadable besides, so the fix is a
/// command and a reinstall rather than an errand.
pub const FIRMWARE_FIX: &str = if cfg!(windows) {
    "The firmware normally ships with Entangled Desktop, in artifacts\\firmware next to the \
     program. If it is not there, `entangled fetch firmware` downloads it (4 MiB, checked \
     against a digest built into this program), or reinstalling puts it back."
} else {
    "The firmware normally ships with Entangled Desktop, in artifacts/firmware next to the \
     program. If it is not there, `entangled fetch firmware` downloads it (4 MiB, digest \
     checked), or build it with `bash guest/firmware/build-cloudhv.sh` (about 2.5 minutes)."
};

/// Where a firmware image was found, in the words the panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareOrigin {
    /// A directory named by `ENTANGLED_FIRMWARE_DIR`.
    Directory,
    /// `artifacts/firmware/` beside the program — what the installer ships.
    Install,
    /// The verified cache, filled by `entangled fetch firmware`.
    Cache,
    /// `artifacts/firmware/` under the child's working directory.
    Checkout,
}

impl FirmwareOrigin {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Directory => "ENTANGLED_FIRMWARE_DIR",
            Self::Install => "this installation",
            Self::Cache => "the verified cache",
            Self::Checkout => "the working directory",
        }
    }
}

/// Whether a UEFI firmware is reachable from a child started in `cwd`, and from
/// where.
///
/// This mirrors `entangled`'s own resolver (`apps/entangled/src/firmware.rs`,
/// which is the authority): an explicit directory, the install directory beside
/// the program, the verified cache, then the working directory. It is
/// deliberately a *little* more generous about the cache — the CLI knows which
/// release tag its build pins and the manager does not, so any tag directory
/// holding the file counts here. Being generous is the right way round: the
/// worst case is a pre-flight that lets an install start and a CLI that then
/// says precisely which artifact it wanted, which is a better message than the
/// one this check could write.
pub fn locate_firmware(cwd: &Path) -> Option<(PathBuf, FirmwareOrigin)> {
    let image = |dir: PathBuf| -> Option<PathBuf> {
        let path = dir.join(FIRMWARE_FILE);
        path.is_file().then_some(path)
    };
    if let Some(dir) = std::env::var_os("ENTANGLED_FIRMWARE_DIR") {
        if let Some(path) = image(PathBuf::from(dir)) {
            return Some((path, FirmwareOrigin::Directory));
        }
    }
    if let Some(path) = install_firmware_dir().and_then(image) {
        return Some((path, FirmwareOrigin::Install));
    }
    if let Some(cache) = cache_root() {
        let found = std::fs::read_dir(cache.join("firmware"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .find_map(|entry| image(entry.path()));
        if let Some(path) = found {
            return Some((path, FirmwareOrigin::Cache));
        }
    }
    image(cwd.join("artifacts").join("firmware")).map(|path| (path, FirmwareOrigin::Checkout))
}

/// `artifacts\firmware` beside the manager executable — where
/// `installer/entangled.iss` puts the shipped copy, next to `entangled.exe`
/// itself. The CLI resolves it relative to *its own* executable and the two sit
/// in the same directory in every layout this project produces (`cargo build`
/// and the installer alike).
fn install_firmware_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("artifacts").join("firmware"))
}

/// The artifact `entangled install <distro>` would fail on, if any — one message
/// ready to show, or `None` when this host can install that distribution now.
///
/// Per distro rather than one check for both, because the two paths need
/// different files: an Ubuntu install has no use for the bootstrap kernel, and
/// blocking it on a missing one (which is what the wizard used to do) makes the
/// only installer that works on Windows unreachable there.
pub fn missing_install_artifact(cwd: &Path, family: GuestFamily) -> Option<String> {
    match family {
        GuestFamily::Ubuntu => {
            if locate_firmware(cwd).is_some() {
                return None;
            }
            Some(format!(
                "no UEFI firmware ({FIRMWARE_FILE}) on this computer — not next to the \
                 program, not in the verified cache, and not under {}. {FIRMWARE_FIX}",
                cwd.display()
            ))
        }
        GuestFamily::Debian => {
            if bootstrap_artifacts_present(cwd) {
                return None;
            }
            Some(format!(
                "no bootstrap kernel for the Debian installer — not at {} and none in the \
                 verified cache. Run `entangled fetch bootstrap-kernel` (about 13 MiB, checked \
                 against a SHA-256 pinned in this build){}",
                cwd.join(BOOTSTRAP_KERNEL).display(),
                if cfg!(windows) {
                    ", or copy an artifacts/bootstrap/ directory in from a Linux checkout and \
                     point Settings ▸ Advanced ▸ working directory at it."
                } else {
                    ", or build it with `bash guest/bootstrap-kernel/build.sh`."
                }
            ))
        }
    }
}

/// Whether a bootstrap kernel + initramfs pair is reachable from a child started
/// in `cwd`.
///
/// This mirrors `entangled`'s own resolver (`apps/entangled/src/bootstrap.rs`,
/// which is the authority): an explicit directory, then the child's working
/// directory, then the verified cache. It is deliberately a *little* more
/// generous about the cache — the CLI knows which release tag this build pins
/// and the manager does not, so any tag directory holding both files counts
/// here. Being generous is the right way round: the worst case is a pre-flight
/// that lets an install start and a CLI that then says precisely which artifact
/// it wanted, which is a better message than the one this check could write.
pub fn bootstrap_artifacts_present(cwd: &Path) -> bool {
    let pair = |dir: &Path| dir.join("vmlinuz").is_file() && dir.join("initrd.img").is_file();
    if let Some(dir) = std::env::var_os("ENTANGLED_BOOTSTRAP_DIR") {
        if pair(Path::new(&dir)) {
            return true;
        }
    }
    if cwd.join(BOOTSTRAP_KERNEL).is_file() && cwd.join(BOOTSTRAP_INITRD).is_file() {
        return true;
    }
    let Some(cache) = cache_root() else {
        return false;
    };
    std::fs::read_dir(cache.join("bootstrap"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| pair(&entry.path()))
}

/// The one cache resolution the whole project shares, honouring the same
/// `ENTANGLED_CACHE` override the CLI and the fetch scripts do.
fn cache_root() -> Option<PathBuf> {
    match std::env::var("ENTANGLED_CACHE") {
        Ok(dir) if !dir.trim().is_empty() => Some(PathBuf::from(dir)),
        _ => debian_media::cache_root(),
    }
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
            backend: Backend::Native,
        }
    }

    fn native(engine: &str) -> Runner {
        Runner {
            backend: Backend::Native,
            engine: PathBuf::from(engine),
            distro: "Ubuntu".into(),
            linux_engine: String::new(),
        }
    }

    #[test]
    fn install_arguments_match_the_cli_surface() {
        let vm_dir = Path::new("/vms");
        let spec = install_spec(
            &native("/usr/bin/entangled"),
            vm_dir,
            PathBuf::from("/srv/entangled"),
            &machine(),
        )
        .expect("spec");

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

    /// The Ubuntu wiring: the distro reaches the command line, `--variant` does
    /// not (the ISO path has no variants, and `install ubuntu` silently ignores
    /// the flag — a no-op on the reviewed command line is a lie), and everything
    /// else is spelled the same way.
    #[test]
    fn the_ubuntu_install_names_the_distro_and_drops_the_variant() {
        let mut m = machine();
        m.family = GuestFamily::Ubuntu;
        let line = install_spec(
            &native("entangled"),
            Path::new("/vms"),
            PathBuf::from("/srv"),
            &m,
        )
        .expect("spec")
        .command_line();
        assert!(line.contains("install ubuntu"), "{line}");
        assert!(!line.contains("--variant"), "{line}");
        assert!(
            line.contains("--size 20G") && line.contains("--name demo"),
            "{line}"
        );
        // The network is the CLI's per-host decision, never the GUI's.
        assert!(!line.contains("--network"), "{line}");
    }

    /// Which artifact blocks which installer. The Ubuntu path must not be gated
    /// on the bootstrap kernel — that is what made it unreachable on Windows,
    /// the only host where it is the *default*.
    #[test]
    fn each_distro_is_gated_on_its_own_artifact() {
        let dir = std::env::temp_dir().join(format!("entangled-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("artifacts/firmware")).unwrap();
        std::fs::write(dir.join(UEFI_FIRMWARE), b"fake firmware").unwrap();
        // An empty cache of our own: the real one on a developer's machine may
        // well hold a fetched kernel, and this test is about the *checkout*.
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let _cache_env = EnvGuard::set("ENTANGLED_CACHE", cache.as_os_str());
        let _dir_env = EnvGuard::clear("ENTANGLED_BOOTSTRAP_DIR");
        let _fw_env = EnvGuard::clear("ENTANGLED_FIRMWARE_DIR");

        // Firmware present, kernel absent: Ubuntu can go, Debian cannot — and
        // the refusal is a command, not a dead end. That sentence is the whole
        // difference this feature makes on Windows.
        assert_eq!(missing_install_artifact(&dir, GuestFamily::Ubuntu), None);
        let debian =
            missing_install_artifact(&dir, GuestFamily::Debian).expect("no bootstrap kernel");
        assert!(debian.contains(BOOTSTRAP_KERNEL), "{debian}");
        assert!(
            debian.contains("entangled fetch bootstrap-kernel"),
            "the Debian pre-flight must name the way out: {debian}"
        );

        // Half a pair is still missing: a kernel with no initramfs boots to a
        // panic, which is a worse failure than this message.
        std::fs::create_dir_all(dir.join("artifacts/bootstrap")).unwrap();
        std::fs::write(dir.join(BOOTSTRAP_KERNEL), b"fake kernel").unwrap();
        assert!(missing_install_artifact(&dir, GuestFamily::Debian).is_some());
        std::fs::write(dir.join(BOOTSTRAP_INITRD), b"fake initrd").unwrap();

        // And the other way round. The refusal must name a way out a person in
        // front of *this* computer has: the firmware ships with the program and
        // is downloadable, so "go and get a Linux checkout" is not it.
        std::fs::remove_file(dir.join(UEFI_FIRMWARE)).unwrap();
        assert_eq!(missing_install_artifact(&dir, GuestFamily::Debian), None);
        let ubuntu = missing_install_artifact(&dir, GuestFamily::Ubuntu).expect("no firmware");
        assert!(ubuntu.contains(FIRMWARE_FILE), "{ubuntu}");
        assert!(
            ubuntu.contains("entangled fetch firmware"),
            "the firmware pre-flight must name the way out: {ubuntu}"
        );
        assert!(
            !ubuntu.contains("Copy artifacts/firmware/CLOUDHV.fd in from a Linux checkout"),
            "the old dead end came back: {ubuntu}"
        );

        // ...and a cached firmware, under any tag, satisfies it — the same
        // generosity the bootstrap arm above shows, for the same reason.
        let tag = cache.join("firmware/firmware-edk2-stable202602-1");
        std::fs::create_dir_all(&tag).unwrap();
        std::fs::write(tag.join(FIRMWARE_FILE), b"fetched firmware").unwrap();
        assert_eq!(missing_install_artifact(&dir, GuestFamily::Ubuntu), None);
        assert_eq!(
            locate_firmware(&dir).map(|(_, origin)| origin),
            Some(FirmwareOrigin::Cache)
        );
        std::fs::remove_file(tag.join(FIRMWARE_FILE)).unwrap();

        // …and the arm that exists so a Windows user who ran the fetch is not
        // told to go build a kernel: a pair in the cache, nothing in the working
        // directory. Folded into this test rather than written as its own,
        // because both touch `ENTANGLED_CACHE` and cargo runs tests in threads
        // of one process — two of them racing produced exactly the flake you
        // would expect.
        let elsewhere = dir.join("empty-checkout");
        let tag = cache.join("bootstrap/guest-artifacts-6.12.9-1");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(&tag).unwrap();
        std::fs::write(tag.join("vmlinuz"), b"fetched kernel").unwrap();
        assert!(
            missing_install_artifact(&elsewhere, GuestFamily::Debian).is_some(),
            "half a cached pair is not a pair"
        );
        std::fs::write(tag.join("initrd.img"), b"fetched initrd").unwrap();
        assert_eq!(
            missing_install_artifact(&elsewhere, GuestFamily::Debian),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One environment variable, set or cleared for the length of a test and put
    /// back afterwards. These tests share a process with every other test in this
    /// binary, so without it they steal each other's cache root.
    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self(key, previous)
        }

        fn clear(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self(key, previous)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.1.take() {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    /// The default a fresh wizard opens with. Both installers work on both hosts
    /// now, so this is a preference rather than a capability: Windows opens on
    /// Ubuntu because that one installs entirely offline from a verified ISO,
    /// while Debian's d-i downloads the system from a mirror.
    #[test]
    fn the_default_family_is_the_one_this_host_can_install() {
        let default = GuestFamily::default_for_host();
        assert_eq!(
            default.cli_name(),
            if cfg!(windows) { "ubuntu" } else { "debian" }
        );
        let mut m = machine();
        m.family = default;
        let line = install_spec(
            &native("entangled"),
            Path::new("/vms"),
            PathBuf::from("/srv"),
            &m,
        )
        .expect("spec")
        .command_line();
        assert!(
            line.contains(&format!("install {}", default.cli_name())),
            "{line}"
        );
    }

    #[test]
    fn installer_memory_has_a_floor_and_headless_is_optional() {
        let mut m = machine();
        m.memory_mib = 512;
        m.automated = false;
        m.headless = true;
        let line = install_spec(
            &native("entangled"),
            Path::new("/vms"),
            PathBuf::from("/srv"),
            &m,
        )
        .expect("spec")
        .command_line();
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
            &native("entangled"),
            Path::new("/vms"),
            PathBuf::from("/srv"),
            &m,
        )
        .expect("spec")
        .command_line();
        assert!(line.contains("install ubuntu"), "{line}");
        assert!(line.contains("--iso /isos/ubuntu.iso"), "{line}");
        assert!(
            line.contains(&Path::new("/vms").join("kept.raw").display().to_string()),
            "{line}"
        );
    }

    fn entry() -> VmEntry {
        VmEntry {
            name: "debian-demo".into(),
            profile_path: PathBuf::from("/vms/debian-demo.toml"),
            memory_mib: 2048,
            vcpus: 2,
            transport: control_api::VirtioTransport::Pci,
            display: (1920, 1080),
            network_interface: Some("entangled0".into()),
            disks: vec![],
            uefi: false,
        }
    }

    #[test]
    fn run_spec_points_at_the_profile() {
        let spec = run_spec(
            &native("/usr/bin/entangled"),
            &entry(),
            PathBuf::from("/srv/entangled"),
            Path::new("/vms"),
        )
        .expect("spec");
        assert_eq!(spec.kind, TaskKind::Run);
        assert_eq!(
            spec.command_line(),
            "/usr/bin/entangled run --control-stdin /vms/debian-demo.toml"
        );
        // The flag and the pipe are one decision: asking the CLI to read
        // lifecycle commands is useless unless the supervisor keeps a stdin to
        // write them to, and vice versa (ADR-0005).
        assert!(
            spec.control,
            "a run task must keep its child's stdin, or Pause and Restart have nowhere to go"
        );
        assert_eq!(spec.cwd, Path::new("/srv/entangled"));
        assert_eq!(spec.log_path, Path::new("/vms/debian-demo-run.log"));
    }

    /// A resume is a start: same task kind, same log, same control pipe — and
    /// the file it names is translated for the engine that will open it.
    #[test]
    fn resume_spec_starts_the_snapshot_not_the_profile() {
        let spec = resume_spec(
            &native("/usr/bin/entangled"),
            "debian-demo",
            Path::new("/vms/debian-demo.esnap"),
            PathBuf::from("/srv/entangled"),
            Path::new("/vms"),
        )
        .expect("spec");
        assert_eq!(spec.kind, TaskKind::Run);
        assert_eq!(
            spec.command_line(),
            "/usr/bin/entangled resume --control-stdin /vms/debian-demo.esnap"
        );
        assert!(
            spec.control,
            "a resumed machine must be pausable and suspendable like any other"
        );
        assert_eq!(spec.log_path, Path::new("/vms/debian-demo-run.log"));

        let through_wsl = resume_spec(
            &Runner {
                backend: Backend::Wsl,
                engine: PathBuf::from("C:/Program Files/Entangled/entangled.exe"),
                distro: "Ubuntu".into(),
                linux_engine: "/home/spider/entangled".into(),
            },
            "debian-demo",
            Path::new("D:/vms/debian-demo.esnap"),
            PathBuf::from("D:/entangled-desktop"),
            Path::new("D:/vms"),
        )
        .expect("spec");
        assert!(
            through_wsl
                .command_line()
                .contains("/mnt/d/vms/debian-demo.esnap"),
            "{}",
            through_wsl.command_line()
        );
    }

    /// The WSL backend: `wsl.exe` is the program, the profile path is
    /// translated, and stdin stays a pipe so Pause and Restart still reach the
    /// guest through `wsl.exe`.
    #[test]
    fn the_wsl_backend_rewrites_the_program_and_the_paths() {
        let runner = Runner {
            backend: Backend::Wsl,
            engine: PathBuf::from(r"C:\Program Files\Entangled\entangled.exe"),
            distro: "Ubuntu".into(),
            linux_engine: "/home/spider/target/debug/entangled".into(),
        };
        let mut vm = entry();
        vm.profile_path = PathBuf::from(r"D:\entangled-vms\debian-demo.toml");
        let spec = run_spec(
            &runner,
            &vm,
            PathBuf::from(r"D:\entangled-desktop"),
            Path::new(r"D:\entangled-vms"),
        )
        .expect("spec");

        assert_eq!(spec.program, Path::new("wsl.exe"));
        assert!(spec.control, "the control pipe survives wsl.exe");
        let line = spec.command_line();
        assert!(line.contains("-d Ubuntu"), "{line}");
        assert!(
            line.contains("/home/spider/target/debug/entangled"),
            "{line}"
        );
        assert!(
            line.contains("/mnt/d/entangled-vms/debian-demo.toml"),
            "{line}"
        );
        // The log still lands on the Windows side, where the manager reads it.
        assert_eq!(
            spec.log_path,
            Path::new(r"D:\entangled-vms").join("debian-demo-run.log")
        );
    }

    /// A machine WSL cannot see is refused with the reason, at spec-building
    /// time — not deep inside the engine, and not as a silent failure.
    #[test]
    fn an_untranslatable_path_fails_the_spec_not_the_boot() {
        let runner = Runner {
            backend: Backend::Wsl,
            engine: PathBuf::from("entangled.exe"),
            distro: "Ubuntu".into(),
            linux_engine: String::new(),
        };
        let mut vm = entry();
        vm.profile_path = PathBuf::from(r"\\nas\vms\demo.toml");
        let error = run_spec(&runner, &vm, PathBuf::from(r"D:\x"), Path::new(r"D:\x"))
            .expect_err("unreachable");
        assert!(error.contains("network path"), "{error}");
    }

    #[test]
    fn explicit_binary_must_exist_and_is_reported_as_chosen() {
        let mut settings = Settings {
            entangled_binary: Some(PathBuf::from("/definitely/not/here/entangled")),
            ..Settings::default()
        };
        assert!(matches!(
            locate_engine(&settings),
            Err(LaunchError::OverrideMissing(_))
        ));

        // A real file is accepted as-is, and the status says where it came from.
        let here = std::env::current_exe().expect("test binary path");
        settings.entangled_binary = Some(here.clone());
        let engine = locate_engine(&settings).expect("located");
        assert_eq!(engine.path, here);
        assert_eq!(engine.origin, EngineOrigin::Chosen);
        assert!(engine.summary().contains("chosen in Settings"));
    }

    #[test]
    fn the_version_line_is_parsed_or_dropped() {
        assert_eq!(parse_version("entangled 0.2.17\n"), Some("0.2.17".into()));
        assert_eq!(parse_version("entangled 0.2.0"), Some("0.2.0".into()));
        // Anything that is not "<name> <digits…>" is not a version.
        assert_eq!(parse_version("entangled"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("bash: entangled: not found"), None);
    }
}
