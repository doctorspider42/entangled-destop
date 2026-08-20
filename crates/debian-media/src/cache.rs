//! Media cache layout (MVP-609).
//!
//! ```text
//! $XDG_CACHE_HOME/entangled/media/<version>/<arch>-<variant>/<file>
//!                                       /<file>.manifest.toml
//! ```
//!
//! Deviation from the backlog sketch (`media/<version>/<file>`): the text and
//! GTK netboot variants both publish files literally named `linux` and
//! `initrd.gz`, and their initrds are different images. A flat per-version
//! directory would let a cached text-mode initrd be served for a `gtk-netboot`
//! request. The `<arch>-<variant>` component removes that collision; the
//! manifest's recorded URL is still checked on every cache hit as a second
//! line of defence.
//!
//! Downloads land in `<file>.part` and are renamed only once the digest matches
//! a signed checksum entry, so an interrupted run leaves a resumable partial
//! and never a file that looks complete.

use std::path::{Path, PathBuf};

use crate::error::MediaError;
use crate::manifest::Manifest;
use crate::source::{Arch, InstallerVariant};

/// Suffix of the provenance manifest that sits next to each artifact.
pub const MANIFEST_SUFFIX: &str = ".manifest.toml";

/// Suffix of an in-progress download.
pub const PARTIAL_SUFFIX: &str = ".part";

/// Root of the media cache: `<cache>/entangled/media`.
#[derive(Debug, Clone)]
pub struct MediaCache {
    root: PathBuf,
}

impl MediaCache {
    /// Resolves `$XDG_CACHE_HOME/entangled/media`, falling back to
    /// `$HOME/.cache/entangled/media`.
    ///
    /// On Windows (where this crate still has to build and be testable) the
    /// fallback uses `%LOCALAPPDATA%`, then `%USERPROFILE%\.cache`.
    pub fn discover() -> Result<Self, MediaError> {
        Ok(Self::with_root(
            cache_root().ok_or(MediaError::NoCacheDir)?.join("media"),
        ))
    }

    /// An explicit root — used by tests and by a future `--cache-dir` flag.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding one variant's artifacts for one discovered version.
    pub fn variant_dir(&self, version: &str, arch: Arch, variant: InstallerVariant) -> PathBuf {
        self.root
            .join(sanitize(version))
            .join(format!("{}-{}", arch.as_str(), variant.as_str()))
    }

    /// Full path of a cached artifact.
    pub fn artifact_path(
        &self,
        version: &str,
        arch: Arch,
        variant: InstallerVariant,
        file_name: &str,
    ) -> PathBuf {
        self.variant_dir(version, arch, variant)
            .join(sanitize(file_name))
    }
}

/// Path of the manifest belonging to `artifact`.
pub fn manifest_path(artifact: &Path) -> PathBuf {
    let mut name = artifact.file_name().unwrap_or_default().to_os_string();
    name.push(MANIFEST_SUFFIX);
    artifact.with_file_name(name)
}

/// Path of the partial download belonging to `artifact`.
pub fn partial_path(artifact: &Path) -> PathBuf {
    let mut name = artifact.file_name().unwrap_or_default().to_os_string();
    name.push(PARTIAL_SUFFIX);
    artifact.with_file_name(name)
}

/// Reads the manifest next to `artifact`, if any. A malformed manifest is
/// treated as "no manifest": the artifact is unverified and will be re-fetched.
pub fn load_manifest(artifact: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(manifest_path(artifact)).ok()?;
    Manifest::from_toml(&text).ok()
}

/// Writes the manifest next to `artifact`. Called only after both the signature
/// and the digest check have passed (MVP-610).
pub fn store_manifest(artifact: &Path, manifest: &Manifest) -> Result<PathBuf, MediaError> {
    let path = manifest_path(artifact);
    let text = manifest.to_toml()?;
    std::fs::write(&path, text).map_err(|e| MediaError::io(&path, e))?;
    Ok(path)
}

/// Removes an artifact and its manifest, ignoring "already gone".
///
/// Backlog acceptance criterion: a bad digest or signature must remove the
/// partial artifact from the active cache.
pub fn purge(artifact: &Path) {
    for path in [
        artifact.to_path_buf(),
        manifest_path(artifact),
        partial_path(artifact),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "removed unverified media"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "cannot remove media"),
        }
    }
}

/// The whole project's cache root — `<platform cache base>/entangled` — of which
/// the media cache is one subdirectory.
///
/// Public and living here because it is not only this crate's: `entangled
/// install ubuntu` looks for the ISO `scripts/fetch-ubuntu-iso.sh` verified into
/// `<root>/ubuntu/<release>/`, and the two must never disagree about where the
/// cache is. On Windows that means the same `%LOCALAPPDATA%\entangled` for both
/// (see [`cache_base`]), which is the whole reason this is one function and not
/// two.
pub fn cache_root() -> Option<PathBuf> {
    cache_base().map(|base| base.join("entangled"))
}

/// The platform's cache base, in preference order:
/// `$XDG_CACHE_HOME`, `$HOME/.cache`, `%LOCALAPPDATA%`, `%USERPROFILE%\.cache`.
///
/// `HOME` before `LOCALAPPDATA` on purpose: a Windows shell that sets `HOME`
/// (git-bash, MSYS) is a shell whose user also runs the Linux side of this
/// project, and a single cache is better than two half-populated ones.
fn cache_base() -> Option<PathBuf> {
    resolve_cache_base(&non_empty_var)
}

/// The resolution itself, over an injected environment so both hosts' layouts
/// are asserted on both hosts (mutating the real environment in a test would
/// race every other test in the process).
fn resolve_cache_base(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir));
    }
    if let Some(home) = var("HOME") {
        return Some(PathBuf::from(home).join(".cache"));
    }
    if let Some(local) = var("LOCALAPPDATA") {
        return Some(PathBuf::from(local));
    }
    var("USERPROFILE").map(|p| PathBuf::from(p).join(".cache"))
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Keeps server-supplied names from escaping the cache directory. Names come
/// from a signed checksum file, but "signed" is not "safe to use as a path".
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' | '+' => c,
            _ => '_',
        })
        .collect();
    // No `..` may survive, in any position: the result is used as a single path
    // component and must not be able to walk out of the cache root.
    let cleaned = cleaned.replace("..", "__");
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return "_".to_string();
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment a Windows shell without `HOME` presents, and the one a
    /// Linux (or git-bash) shell presents. Both are resolved on both hosts, so a
    /// change to the order fails everywhere rather than on one runner.
    #[test]
    fn the_cache_base_follows_the_hosts_conventions() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };

        // Windows: %LOCALAPPDATA% is the cache, not a `.cache` under the profile.
        let windows = env(&[
            ("LOCALAPPDATA", r"C:\Users\ada\AppData\Local"),
            ("USERPROFILE", r"C:\Users\ada"),
        ]);
        assert_eq!(
            resolve_cache_base(&windows),
            Some(PathBuf::from(r"C:\Users\ada\AppData\Local"))
        );

        // Windows without LOCALAPPDATA (a service account): the profile's .cache.
        let profile_only = env(&[("USERPROFILE", r"C:\Users\ada")]);
        assert_eq!(
            resolve_cache_base(&profile_only),
            Some(PathBuf::from(r"C:\Users\ada").join(".cache"))
        );

        // Unix, and any shell that sets HOME: HOME wins over LOCALAPPDATA so a
        // git-bash session shares the cache it already populated.
        let unix = env(&[("HOME", "/home/ada"), ("LOCALAPPDATA", r"C:\ignored")]);
        assert_eq!(
            resolve_cache_base(&unix),
            Some(PathBuf::from("/home/ada").join(".cache"))
        );

        // XDG_CACHE_HOME wins over everything.
        let xdg = env(&[("XDG_CACHE_HOME", "/var/cache/x"), ("HOME", "/home/ada")]);
        assert_eq!(
            resolve_cache_base(&xdg),
            Some(PathBuf::from("/var/cache/x"))
        );

        // Nothing set at all: no cache, and the caller must say so.
        assert_eq!(resolve_cache_base(&env(&[])), None);
    }

    #[test]
    fn layout_separates_variants_within_a_version() {
        let cache = MediaCache::with_root("/cache/entangled/media");
        let text = cache.artifact_path("13.6", Arch::Amd64, InstallerVariant::TextNetboot, "linux");
        let gtk = cache.artifact_path("13.6", Arch::Amd64, InstallerVariant::GtkNetboot, "linux");
        assert_ne!(text, gtk);
        assert!(text.ends_with("13.6/amd64-text-netboot/linux"), "{text:?}");
        assert!(gtk.ends_with("13.6/amd64-gtk-netboot/linux"), "{gtk:?}");
    }

    #[test]
    fn manifest_and_partial_sit_next_to_the_artifact() {
        let artifact = Path::new("/cache/13.6/amd64-gtk-netboot/initrd.gz");
        assert_eq!(
            manifest_path(artifact),
            Path::new("/cache/13.6/amd64-gtk-netboot/initrd.gz.manifest.toml")
        );
        assert_eq!(
            partial_path(artifact),
            Path::new("/cache/13.6/amd64-gtk-netboot/initrd.gz.part")
        );
    }

    #[test]
    fn names_cannot_escape_the_cache_directory() {
        let cache = MediaCache::with_root("/cache");
        let evil = cache.artifact_path(
            "../../etc",
            Arch::Amd64,
            InstallerVariant::NetinstIso,
            "../../../etc/passwd",
        );
        let text = evil.to_string_lossy().replace('\\', "/");
        assert!(text.starts_with("/cache/"), "{text}");
        assert!(!text.contains(".."), "{text}");
    }

    #[test]
    fn sanitize_keeps_real_debian_names_intact() {
        assert_eq!(
            sanitize("debian-13.6.0-amd64-netinst.iso"),
            "debian-13.6.0-amd64-netinst.iso"
        );
        assert_eq!(sanitize("initrd.gz"), "initrd.gz");
        assert_eq!(sanitize(".."), "__");
        assert_eq!(sanitize(""), "_");
        assert!(!sanitize("../../etc/passwd").contains(".."));
    }

    #[test]
    fn purge_is_idempotent_on_missing_files() {
        let dir = std::env::temp_dir().join(format!("entangled-purge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("linux");
        purge(&artifact); // nothing there yet
        std::fs::write(&artifact, b"x").unwrap();
        std::fs::write(manifest_path(&artifact), b"y").unwrap();
        std::fs::write(partial_path(&artifact), b"z").unwrap();
        purge(&artifact);
        assert!(!artifact.exists());
        assert!(!manifest_path(&artifact).exists());
        assert!(!partial_path(&artifact).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
