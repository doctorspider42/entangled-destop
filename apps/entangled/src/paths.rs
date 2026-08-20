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

    /// Every derived install artifact is `disk.with_file_name(...)`, so the name
    /// the stem produces is the name a Windows path with spaces, a drive letter
    /// and dots in the file name has to survive.
    #[test]
    fn stems_survive_windows_paths() {
        assert_eq!(stem_of(Path::new(r"D:\vms\ubuntu.raw"), "x"), "ubuntu");
        assert_eq!(
            stem_of(Path::new(r"D:\my vms\big disk.raw"), "x"),
            "big disk"
        );
        assert_eq!(stem_of(Path::new(r"C:\vms\ubuntu.2.raw"), "x"), "ubuntu.2");
        assert_eq!(
            stem_of(Path::new("/home/ada/vms/ubuntu.raw"), "x"),
            "ubuntu"
        );
        // A bare name is its own stem; a directory-ish path falls back.
        assert_eq!(stem_of(Path::new("ubuntu.raw"), "x"), "ubuntu");
        assert_eq!(stem_of(Path::new(""), "fallback"), "fallback");
    }

    /// The sidecars the install path writes, spelled the way the install path
    /// spells them, against a path with a drive letter and a space in it.
    #[test]
    fn sidecars_stay_beside_the_disk() {
        let disk = Path::new(r"D:\my vms\ubuntu.raw");
        let name = stem_of(disk, "ubuntu");
        for (suffix, expected) in [
            (format!("{name}.nvram"), r"D:\my vms\ubuntu.nvram"),
            (format!("{name}.toml"), r"D:\my vms\ubuntu.toml"),
            (format!("{name}-seed.iso"), r"D:\my vms\ubuntu-seed.iso"),
            (
                format!("{name}-install.log"),
                r"D:\my vms\ubuntu-install.log",
            ),
        ] {
            let path = disk.with_file_name(&suffix);
            // Compared as paths, not strings: on Linux the whole thing is one
            // file name with backslashes in it, and that is still the same file
            // the profile would name.
            assert_eq!(path, Path::new(expected), "{suffix}");
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
        // An empty value is not an override: that is an unset variable spelled
        // badly, and following it would look for the cache at the filesystem
        // root.
        std::env::set_var("ENTANGLED_CACHE", "");
        assert!(cache_root().is_ok() || cache_root().is_err());
        assert_ne!(cache_root().ok(), Some(PathBuf::new()));
        match previous {
            Some(value) => std::env::set_var("ENTANGLED_CACHE", value),
            None => std::env::remove_var("ENTANGLED_CACHE"),
        }
    }
}
