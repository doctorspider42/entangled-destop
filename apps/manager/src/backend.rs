//! Which hypervisor actually runs a machine, and what that choice can and
//! cannot do.
//!
//! On Linux there is one answer (KVM) and this module is a formality. On
//! Windows there are two, and they are genuinely different machines:
//!
//! - **Windows (WHP)** — `entangled.exe` runs natively on the Windows
//!   Hypervisor Platform. No TAP, no 3D, but no WSL either.
//! - **WSL (KVM)** — the *Linux* build of `entangled` runs inside WSL, which
//!   has `/dev/kvm` through nested virtualisation. That host can do 3D
//!   (virglrenderer speaks EGL — ADR-0004) and TAP networking, and its window
//!   comes up through WSLg rather than on the Windows desktop directly.
//!
//! The choice is a **launch** decision, not part of the guest's configuration,
//! so it lives in the manager's settings rather than in the VM profile: the
//! same profile is meant to boot on either host (ADR-0002).
//!
//! Everything here is pure logic over strings and paths — no `wsl.exe`, no
//! `#[cfg]`-gated behaviour beyond naming — so the whole matrix tests on both
//! hosts.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The default WSL distribution the manager talks to. `wsl -d <name>` — the
/// name a user sees in `wsl --list`.
pub const DEFAULT_WSL_DISTRO: &str = "Ubuntu";

/// Where a machine is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// The `entangled` binary for *this* operating system: KVM on Linux, WHP
    /// on Windows.
    #[default]
    Native,
    /// The Linux build of `entangled`, started through `wsl.exe`. Only
    /// meaningful on a Windows host.
    Wsl,
}

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Native, Backend::Wsl];

    /// The name shown in the picker. Named after what the user recognises —
    /// the operating system — with the hypervisor in brackets.
    pub const fn label(self) -> &'static str {
        match self {
            Backend::Native => {
                if cfg!(windows) {
                    "Windows (WHP)"
                } else {
                    "Linux (KVM)"
                }
            }
            Backend::Wsl => "WSL (KVM)",
        }
    }

    /// One sentence for the tooltip.
    pub const fn tooltip(self) -> &'static str {
        match self {
            Backend::Native => {
                if cfg!(windows) {
                    "Runs the machine on Windows itself, through the Windows Hypervisor \
                     Platform. Its window is an ordinary Windows window. No 3D acceleration \
                     and no TAP networking yet."
                } else {
                    "Runs the machine on this Linux host through KVM. Everything Entangled \
                     can do is available here."
                }
            }
            Backend::Wsl => {
                "Runs the Linux build of Entangled inside WSL, which has KVM. This is the \
                 host that can do 3D acceleration and TAP networking — but the machine's \
                 window is opened by WSLg, and the machine's files must live on a drive \
                 WSL can reach."
            }
        }
    }

    /// Whether this host can offer the choice at all. WSL only exists as an
    /// alternative on Windows; on Linux the native backend *is* KVM.
    pub const fn available_on_host(self) -> bool {
        match self {
            Backend::Native => true,
            Backend::Wsl => cfg!(windows),
        }
    }

    /// True when this backend runs a Linux kernel with KVM underneath, which is
    /// what every Linux-only capability actually depends on.
    pub const fn is_linux_kvm(self) -> bool {
        match self {
            Backend::Native => cfg!(target_os = "linux"),
            Backend::Wsl => true,
        }
    }

    /// Why 3D is unavailable on this backend, or `None` when it works.
    pub fn virgl_block(self) -> Option<Block> {
        (!self.is_linux_kvm()).then_some(Block {
            short: "Needs the Linux/KVM backend — switch this machine to \"WSL (KVM)\".",
            long: "3D acceleration needs the Linux/KVM backend for now: the host renderer \
                   (virglrenderer) speaks EGL, which Windows has no equivalent of yet \
                   (ADR-0004). Switch this machine to \"WSL (KVM)\", or leave 3D off and \
                   run with 2D — the guest still gets a desktop, drawn by its own \
                   processor.",
        })
    }

    /// Why TAP networking is unavailable, or `None`.
    pub fn tap_block(self) -> Option<Block> {
        (!self.is_linux_kvm()).then_some(Block {
            short: "Linux only — use \"usernet\", which needs no host setup.",
            long: "A host network interface (TAP) exists only on Linux; Windows has no TAP \
                   device and the drivers that would provide one are GPL, which this \
                   product cannot ship. Use \"usernet\" instead — a NAT that runs inside \
                   Entangled, needs no host setup and no administrator.",
        })
    }

    /// Why a Debian installation cannot start here, or `None`.
    ///
    /// The Debian installer boots the project's own bootstrap kernel, which is
    /// a Linux kernel build with no cross build — so on the Windows backend it
    /// is not "missing", it is unbuildable.
    pub fn debian_install_block(self) -> Option<Block> {
        (!self.is_linux_kvm()).then_some(Block {
            short: "Not available here — install Ubuntu, or switch to \"WSL (KVM)\".",
            long: "The Debian installer boots Entangled's own bootstrap kernel, and that \
                   kernel is built on Linux only — there is no Windows build of it. \
                   Install Ubuntu here (it boots verified media through UEFI and needs no \
                   kernel), or switch this machine to \"WSL (KVM)\".",
        })
    }
}

/// A capability this backend cannot offer, in two lengths.
///
/// The short line goes inline, next to the control that is greyed out — long
/// enough to say what to do, short enough not to become a paragraph in the
/// middle of a form. The long one is the tooltip, where the *why* belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub short: &'static str,
    pub long: &'static str,
}

/// A machine's files as seen from the chosen backend: either fine, or a reason
/// the launch is refused with a concrete fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    Fine,
    /// Reachable, but with a caveat worth saying out loud (drvfs and sparse
    /// files, mostly).
    Caveat(String),
    Refused(String),
}

#[cfg(test)]
impl Reachability {
    fn refusal(&self) -> Option<&str> {
        match self {
            Reachability::Refused(message) => Some(message),
            _ => None,
        }
    }

    fn caveat(&self) -> Option<&str> {
        match self {
            Reachability::Caveat(message) => Some(message),
            _ => None,
        }
    }
}

/// Errors [`to_wsl_path`] reports, each with the fix in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslPathError(pub String);

impl std::fmt::Display for WslPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Translates a Windows path into the path WSL sees.
///
/// `D:\vms\ubuntu.toml` → `/mnt/d/vms/ubuntu.toml`. A path that is already
/// POSIX passes through (the settings may legitimately hold one — the Linux
/// binary's own path, for instance). Everything else is refused *here*, in the
/// UI, rather than deep inside `wsl.exe` where the message would be "The system
/// cannot find the path specified".
///
/// Deliberately pure string work: `Path` semantics differ per host, and this
/// function must give the same answers when the tests run on Linux.
pub fn to_wsl_path(path: &Path) -> Result<String, WslPathError> {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        return Err(WslPathError("the path is empty".to_string()));
    }
    // Already a Linux path.
    if text.starts_with('/') && !text.starts_with("//") {
        return Ok(text);
    }
    // UNC / network share: WSL can mount those, but not by any rule we can
    // derive, so say so instead of guessing.
    if text.starts_with("//") {
        return Err(WslPathError(format!(
            "{} is a network path (UNC). WSL cannot see it under /mnt automatically — \
             move the machine's files onto a local drive, or mount the share inside WSL \
             yourself and point the VM directory at the mount.",
            path.display()
        )));
    }
    let bytes = text.as_bytes();
    let drive_letter = (bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/')
        .then(|| bytes[0] as char)
        .filter(char::is_ascii_alphabetic);
    match drive_letter {
        Some(letter) => Ok(format!(
            "/mnt/{}{}",
            letter.to_ascii_lowercase(),
            &text[2..]
        )),
        None => Err(WslPathError(format!(
            "{} is not an absolute path with a drive letter, so there is no place for it \
             under /mnt in WSL. Pick the file again with the browse button — the manager \
             stores the full path.",
            path.display()
        ))),
    }
}

/// Whether a machine whose files live under `paths` can be started on
/// `backend`, and what the user should know if it can.
///
/// The native backend never has anything to say: the files are already where
/// the binary runs. The WSL backend does, twice over — a path it cannot
/// translate is a hard refusal, and a path under `/mnt` is a warning, because
/// drvfs has no sparse files: a 16 GiB image on `D:` really occupies 16 GiB the
/// moment WSL writes to the end of it.
pub fn reachability<'a>(
    backend: Backend,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Reachability {
    if backend != Backend::Wsl {
        return Reachability::Fine;
    }
    let mut on_windows_drive = false;
    for path in paths {
        match to_wsl_path(path) {
            Err(e) => return Reachability::Refused(e.0),
            Ok(translated) => on_windows_drive |= translated.starts_with("/mnt/"),
        }
    }
    if on_windows_drive {
        return Reachability::Caveat(
            "This machine's files are on a Windows drive. WSL reaches them, but that \
             filesystem cannot store sparse images — a disk that shows as 16 GiB will \
             really occupy 16 GiB once the guest has written across it, and it is slower \
             than a disk inside WSL's own filesystem."
                .to_string(),
        );
    }
    Reachability::Fine
}

/// The `wsl.exe` command line that starts `entangled` inside a distribution.
///
/// `linux_binary` is the path to the *Linux* build. There is no way to derive
/// it from Windows — a release install has no Linux half and a developer's
/// build lives wherever `CARGO_TARGET_DIR` pointed — so it is a setting, and an
/// empty one means "whatever `entangled` resolves to on the WSL PATH".
///
/// Infallible: the paths that *can* fail translation are the engine arguments,
/// and those go through [`to_wsl_path`] before they reach here. `--cd` takes a
/// Windows path and `wsl.exe` translates it itself.
pub fn wsl_args(
    distro: &str,
    linux_binary: &str,
    cwd: Option<&Path>,
    entangled_args: &[String],
) -> Vec<String> {
    let mut args = vec!["-d".to_string(), distro.to_string()];
    if let Some(cwd) = cwd {
        // `wsl --cd` takes the *Windows* path and does the translation itself,
        // which keeps one less rule of ours in the loop.
        args.push("--cd".to_string());
        args.push(cwd.display().to_string());
    }
    args.push("-e".to_string());
    let binary = linux_binary.trim();
    args.push(if binary.is_empty() {
        "entangled".to_string()
    } else {
        binary.to_string()
    });
    args.extend(entangled_args.iter().cloned());
    args
}

/// The program `wsl_args` belongs to.
pub const WSL_PROGRAM: &str = "wsl.exe";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn windows_paths_land_under_mnt() {
        assert_eq!(
            to_wsl_path(&PathBuf::from(r"D:\entangled-vms\ubuntu.toml")).unwrap(),
            "/mnt/d/entangled-vms/ubuntu.toml"
        );
        assert_eq!(
            to_wsl_path(&PathBuf::from("C:/Users/spider/vm.raw")).unwrap(),
            "/mnt/c/Users/spider/vm.raw"
        );
    }

    #[test]
    fn posix_paths_pass_through_and_junk_is_refused_with_a_fix() {
        assert_eq!(
            to_wsl_path(&PathBuf::from("/home/spider/entangled")).unwrap(),
            "/home/spider/entangled"
        );
        let unc = to_wsl_path(&PathBuf::from(r"\\nas\share\vm.raw")).unwrap_err();
        assert!(unc.0.contains("network path"), "{unc}");
        let relative = to_wsl_path(&PathBuf::from(r"vms\x.toml")).unwrap_err();
        assert!(relative.0.contains("absolute path"), "{relative}");
        assert!(to_wsl_path(&PathBuf::from("")).is_err());
    }

    /// The whole point of the refusal: it happens in the UI, before a child is
    /// spawned, and it names the file that cannot be reached.
    #[test]
    fn an_unreachable_machine_is_refused_before_launch() {
        let paths = [
            PathBuf::from(r"\\nas\vms\a.toml"),
            PathBuf::from("D:/a.raw"),
        ];
        let result = reachability(Backend::Wsl, paths.iter().map(PathBuf::as_path));
        let message = result.refusal().expect("refused");
        assert!(message.contains("nas"), "{message}");

        // The native backend never refuses on reachability grounds.
        assert_eq!(
            reachability(Backend::Native, paths.iter().map(PathBuf::as_path)),
            Reachability::Fine
        );
    }

    /// drvfs cannot do sparse files, and a user who never hears that fills a
    /// drive by accident. It is a caveat, not a refusal.
    #[test]
    fn windows_drives_are_reachable_but_not_sparse() {
        let paths = [PathBuf::from(r"D:\vms\a.raw")];
        let result = reachability(Backend::Wsl, paths.iter().map(PathBuf::as_path));
        let caveat = result.caveat().expect("caveat");
        assert!(caveat.contains("sparse"), "{caveat}");

        // A machine that lives inside WSL's own filesystem has nothing to warn
        // about.
        let native = [PathBuf::from("/home/spider/entangled-vms/a.raw")];
        assert_eq!(
            reachability(Backend::Wsl, native.iter().map(PathBuf::as_path)),
            Reachability::Fine
        );
    }

    #[test]
    fn the_wsl_command_line_names_the_distro_the_cwd_and_the_linux_binary() {
        let args = wsl_args(
            "Ubuntu",
            "/home/spider/target/debug/entangled",
            Some(Path::new(r"D:\entangled-desktop")),
            &[
                "run".into(),
                "--control-stdin".into(),
                "/mnt/d/a.toml".into(),
            ],
        );
        assert_eq!(
            args,
            vec![
                "-d",
                "Ubuntu",
                "--cd",
                r"D:\entangled-desktop",
                "-e",
                "/home/spider/target/debug/entangled",
                "run",
                "--control-stdin",
                "/mnt/d/a.toml",
            ]
        );

        // No configured binary: fall back to the WSL PATH rather than inventing
        // a path that does not exist.
        let fallback = wsl_args("Ubuntu", "  ", None, &["doctor".into()]);
        assert_eq!(fallback, vec!["-d", "Ubuntu", "-e", "entangled", "doctor"]);
    }

    /// The capability matrix is the whole reason this module exists: every
    /// "this fails at boot" message must be answerable before launch.
    #[test]
    fn capabilities_follow_the_kernel_not_the_host() {
        assert!(Backend::Wsl.is_linux_kvm());
        assert!(Backend::Wsl.virgl_block().is_none());
        assert!(Backend::Wsl.tap_block().is_none());
        assert!(Backend::Wsl.debian_install_block().is_none());

        assert_eq!(Backend::Native.is_linux_kvm(), cfg!(target_os = "linux"));
        if cfg!(windows) {
            let reason = Backend::Native.virgl_block().expect("blocked on WHP");
            assert!(reason.short.contains("Linux/KVM"), "{}", reason.short);
            assert!(reason.long.contains("ADR-0004"), "{}", reason.long);
            // The inline line must stay short enough to sit under a field.
            assert!(reason.short.len() < 90, "{}", reason.short);
            assert!(Backend::Native.tap_block().is_some());
            assert!(Backend::Native.debian_install_block().is_some());
            assert!(Backend::Wsl.available_on_host());
        } else {
            assert!(Backend::Native.virgl_block().is_none());
            assert!(!Backend::Wsl.available_on_host());
        }
    }
}
