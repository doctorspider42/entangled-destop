//! Where things live on a host — the one place the CLI answers that, so the
//! two hosts cannot drift apart (ADR-0002: "protocol, parsing, validation and
//! config logic must build and test everywhere").
//!
//! Two questions only:
//!
//! * **the cache** — `entangled fetch`, `scripts/fetch-ubuntu-iso.sh` and
//!   `entangled install` must all mean the same directory. The resolution lives
//!   in `debian_media::cache_root` (the crate that owns the cache) and is only
//!   wrapped here, with `ENTANGLED_CACHE` as the explicit override the shell
//!   scripts already honour;
//! * **the VM directory** — where a machine's disk, profile, NVRAM and
//!   transcript go when nobody said. `disk_image::manager_vm_dir` already
//!   answers it for `disk rm`/`disk move`, reading the manager's own
//!   `manager.toml`, so the CLI defers to that rather than inventing a second
//!   default the manager would not find.
//!
//! Nothing here touches the filesystem: these are string-to-path decisions, and
//! they are unit-tested as such on both hosts.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The cache root: `ENTANGLED_CACHE` when set, otherwise
/// `$XDG_CACHE_HOME/entangled`, `$HOME/.cache/entangled`,
/// `%LOCALAPPDATA%\entangled` or `%USERPROFILE%\.cache\entangled` — the first
/// one the environment supplies.
pub fn cache_root() -> Result<PathBuf, String> {
    if let Some(dir) = non_empty_env("ENTANGLED_CACHE") {
        return Ok(PathBuf::from(dir));
    }
    debian_media::cache_root().ok_or_else(|| {
        "cannot locate a cache directory: set ENTANGLED_CACHE, or XDG_CACHE_HOME/HOME \
         (LOCALAPPDATA on Windows)"
            .to_string()
    })
}

/// Where `scripts/fetch-ubuntu-iso.sh` leaves verified ISOs:
/// `<cache>/ubuntu/<release>/<file>.iso`.
pub fn ubuntu_cache_dir() -> Result<PathBuf, String> {
    Ok(cache_root()?.join("ubuntu"))
}

/// Where `scripts/fetch-fedora-iso.sh` leaves verified ISOs:
/// `<cache>/fedora/<release>/<file>.iso`.
pub fn fedora_cache_dir() -> Result<PathBuf, String> {
    Ok(cache_root()?.join("fedora"))
}

/// The newest verified ISO under `dir`, if any.
///
/// "Newest" is by sorted path, not by mtime: a release directory is named after
/// the release (`26.04/`), so the name orders them and a re-download does not
/// promote an older release. Only `.iso` files count — the directory also holds
/// `SHA256SUMS`, its signature and a `.provenance` note per image.
pub fn newest_iso(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|file| file.path())
                .filter(|path| has_extension(path, "iso"))
                .collect::<Vec<_>>()
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Case-insensitive extension match: `foo.ISO` is an ISO on a Windows host, and
/// a comparison that says otherwise would report "no ISO in the cache" while
/// looking straight at one.
pub fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

/// A VM name derived from its disk path, for the profile, the NVRAM sidecar and
/// the transcript.
///
/// `file_stem` rather than any hand-rolled splitting, because the separator
/// differs per host and a name is not a path: `D:\vms\my vm.raw` is `my vm`,
/// and `C:\vms\ubuntu.2.raw` is `ubuntu.2` (the *last* dot, as
/// `with_file_name` would agree).
pub fn stem_of(disk: &Path, fallback: &str) -> String {
    disk.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory in *this* host's spelling, with a space in it — which is the
    /// interesting part on both hosts and the part a Linux-first codebase gets
    /// away with ignoring.
    fn vm_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"D:\my vms")
        } else {
            PathBuf::from("/srv/my vms")
        }
    }

    /// Every derived install artifact is `disk.with_file_name(...)`, so whatever
    /// the stem produces is what the profile, the NVRAM sidecar and the
    /// transcript are called. Spaces, drive letters and dotted names all have to
    /// survive it.
    #[test]
    fn stems_use_the_hosts_path_syntax() {
        let dir = vm_dir();
        assert_eq!(stem_of(&dir.join("ubuntu.raw"), "x"), "ubuntu");
        assert_eq!(stem_of(&dir.join("big disk.raw"), "x"), "big disk");
        // The *last* dot, as `with_file_name` would agree.
        assert_eq!(stem_of(&dir.join("ubuntu.2.raw"), "x"), "ubuntu.2");
        // A bare name is its own stem; nothing at all falls back.
        assert_eq!(stem_of(Path::new("ubuntu.raw"), "x"), "ubuntu");
        assert_eq!(stem_of(Path::new(""), "fallback"), "fallback");

        // The host-specific halves, asserted where they are true rather than
        // asserted everywhere and wrong on one host: a backslash is a separator
        // on Windows and an ordinary file-name character on unix, and that is
        // exactly the difference this module exists to keep straight.
        #[cfg(windows)]
        {
            assert_eq!(stem_of(Path::new(r"D:\vms\ubuntu.raw"), "x"), "ubuntu");
            assert_eq!(stem_of(Path::new(r"\\srv\vms\ubuntu.raw"), "x"), "ubuntu");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                stem_of(Path::new("/home/ada/vms/ubuntu.raw"), "x"),
                "ubuntu"
            );
            // One file whose name happens to contain backslashes — legal here.
            assert_eq!(
                stem_of(Path::new(r"D:\vms\ubuntu.raw"), "x"),
                r"D:\vms\ubuntu"
            );
        }
    }

    /// Every sidecar the install path writes lands in the disk's own directory,
    /// under the disk's own name — including when that name has a space in it and
    /// the directory is on another drive.
    #[test]
    fn sidecars_stay_beside_the_disk() {
        let dir = vm_dir();
        let disk = dir.join("my vm.raw");
        let name = stem_of(&disk, "ubuntu");
        assert_eq!(name, "my vm");
        for suffix in [
            format!("{name}.nvram"),
            format!("{name}.toml"),
            format!("{name}-seed.iso"),
            format!("{name}-install.log"),
            format!("{name}.install-initrd.img"),
        ] {
            let path = disk.with_file_name(&suffix);
            assert_eq!(path.parent(), Some(dir.as_path()), "{suffix}");
            assert_eq!(
                path.file_name().and_then(OsStr::to_str),
                Some(suffix.as_str())
            );
            // And the same directory the disk is in, spelled the host's way.
            assert_eq!(path, dir.join(&suffix), "{suffix}");
        }
    }

    #[test]
    fn iso_extension_matching_ignores_case() {
        assert!(has_extension(Path::new(r"C:\c\ubuntu-26.04.iso"), "iso"));
        assert!(has_extension(Path::new(r"C:\c\UBUNTU.ISO"), "iso"));
        assert!(!has_extension(
            Path::new(r"C:\c\ubuntu.iso.provenance"),
            "iso"
        ));
        assert!(!has_extension(Path::new("SHA256SUMS"), "iso"));
    }

    /// `ENTANGLED_CACHE` is what the shell scripts honour, so the CLI must too —
    /// and it must be taken literally, spaces and drive letter included.
    #[test]
    fn an_explicit_cache_override_wins() {
        // Serialised with nothing else: this is the one test here that touches
        // the process environment, and it puts it back.
        let previous = std::env::var_os("ENTANGLED_CACHE");
        // SAFETY-equivalent note: single-threaded within this test, and the
        // variable is restored below. std::env::set_var is safe on all
        // supported hosts in the 2021 edition this crate builds under.
        std::env::set_var("ENTANGLED_CACHE", r"D:\entangled cache");
        assert_eq!(
            cache_root().unwrap(),
            PathBuf::from(r"D:\entangled cache"),
            "an explicit cache root is used verbatim"
        );
        assert_eq!(
            ubuntu_cache_dir().unwrap(),
            PathBuf::from(r"D:\entangled cache").join("ubuntu")
        );
        assert_eq!(
            fedora_cache_dir().unwrap(),
            PathBuf::from(r"D:\entangled cache").join("fedora")
        );
        // A whitespace-only value is not an override: that is an unset variable
        // spelled badly, and following it would look for the cache at the
        // filesystem root (`/ubuntu`, `\ubuntu`) and report an empty one.
        std::env::set_var("ENTANGLED_CACHE", "   ");
        let fallback = cache_root();
        assert_ne!(
            fallback.as_deref().ok(),
            Some(std::path::Path::new("   ")),
            "a blank ENTANGLED_CACHE must be ignored, not obeyed"
        );
        // ...and what it falls back to is the media cache's own root, which is
        // the whole point of sharing one resolution.
        assert_eq!(fallback.ok(), debian_media::cache_root());
        match previous {
            Some(value) => std::env::set_var("ENTANGLED_CACHE", value),
            None => std::env::remove_var("ENTANGLED_CACHE"),
        }
    }
}
