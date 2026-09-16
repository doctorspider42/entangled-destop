//! The host 3D renderer — where a Venus-capable `libvirglrenderer.so.1` comes
//! from on a machine that cannot build one ([ADR-0004](../../../docs/adr/0004-virtio-gpu-3d.md)).
//!
//! The third artifact this project publishes and pins, after the UEFI firmware
//! and the bootstrap kernel, and for the same reason: it is content a *user*
//! must not be asked to build. Venus needs virglrenderer >= 1.0 with
//! `-Dvenus=true`; jammy packages 0.9.1, which has no Venus and no blob
//! resources at all. Building one needs meson, ninja and six `-dev` packages —
//! a reasonable ask of a developer and an unreasonable one of somebody who
//! installed a desktop VMM.
//!
//! # Two files, not one
//!
//! Venus in virglrenderer 1.1 exists **only** behind the render server:
//! `VIRGL_RENDERER_VENUS` does nothing without `VIRGL_RENDERER_RENDER_SERVER`,
//! and the library `fork`/`exec`s a separate `virgl_render_server` binary. So
//! the artifact is a pair, and the pair has to stay together — which is why a
//! directory holding only the library is not a renderer as far as this module
//! is concerned.
//!
//! The server's path is compiled into the library as an absolute path under
//! its build prefix, which for a downloaded artifact is a directory on a CI
//! runner. `virtio_gpu::virgl` works around that by deriving the path from the
//! library it opened and exporting `RENDER_SERVER_EXEC_PATH`; this module's job
//! is only to put the two files in one directory so that derivation has
//! something to find.
//!
//! # Why the library found here is a *preference*
//!
//! [`crate::run_vm`] passes it through `ENTANGLED_VIRGL_LIB_DEFAULT` rather
//! than `ENTANGLED_VIRGL_LIB`. The difference is what happens when the file
//! will not load: an operator who named a library gets an error, and a library
//! this program chose falls through to the system one. The host that makes
//! that matter has no `libvulkan.so.1` — a hard `DT_NEEDED` of any
//! Venus-capable build, and absent from the distribution's own 0.9.x — where
//! preferring our download would take 3D away from somebody who had it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::artifact::{self, FetchOptions, FetchedAsset, Hint, PinnedAsset};

/// The shared library the VMM `dlopen`s.
pub const LIB_FILE: &str = "libvirglrenderer.so.1";
/// The Venus decoder the library spawns.
pub const SERVER_FILE: &str = "virgl_render_server";

/// Point at a directory that already holds both files. Used verbatim and
/// **not** digest-checked: it is the way in for a local build, and for the
/// period before anything is published.
const DIR_ENV: &str = "ENTANGLED_VIRGL_DIR";
/// Relocates the download, for a mirror or a test.
const BASE_URL_ENV: &str = "ENTANGLED_VIRGL_BASE_URL";

const PINNED_TOML: &str = include_str!("../../../guest/virglrenderer/pinned.toml");
const PIN_PATH: &str = "guest/virglrenderer/pinned.toml";
const WORKFLOW: &str = ".github/workflows/virglrenderer.yml";
const BUILD_SCRIPT: &str = "guest/virglrenderer/build-virglrenderer.sh";

// ---------------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------------

/// One published virglrenderer build, named by an immutable release tag.
#[derive(Debug, Clone, Deserialize)]
pub struct Pinned {
    pub tag: String,
    /// Upstream virglrenderer version, for the message that says what is being
    /// downloaded.
    pub virglrenderer_version: String,
    pub base_url: String,
    /// Where the corresponding source is published. virglrenderer is MIT, so
    /// this is a courtesy rather than an obligation — but the firmware and the
    /// kernel both carry one, and a user should not have to learn which
    /// artifacts bothered.
    pub source: String,
    pub assets: Vec<PinnedAsset>,
}

impl Pinned {
    fn asset(&self, name: &str) -> Result<&PinnedAsset, String> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("{PIN_PATH} names no asset '{name}'"))
    }

    fn release<'a>(&'a self, base_url: &'a str, version: &'a str) -> artifact::Release<'a> {
        artifact::Release {
            pin_path: PIN_PATH,
            tag: &self.tag,
            base_url,
            version,
            assets: &self.assets,
            hint: Hint {
                workflow: WORKFLOW,
                base_url_env: BASE_URL_ENV,
                dir_env: DIR_ENV,
            },
        }
    }
}

/// Reads the compiled-in pin.
///
/// A parse failure is a build-time mistake rather than a user's, so it says
/// which file to fix; `the_compiled_in_pin_parses_and_names_both_assets` keeps
/// it from ever reaching a release.
pub fn pinned() -> Result<Pinned, String> {
    let pin: Pinned = toml::from_str(PINNED_TOML)
        .map_err(|e| format!("{PIN_PATH} is not a valid virglrenderer pin: {e}"))?;
    artifact::validate_digests(PIN_PATH, &pin.assets)?;
    Ok(pin)
}

fn base_url(pin: &Pinned) -> String {
    std::env::var(BASE_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| pin.base_url.clone())
}

// ---------------------------------------------------------------------------
// Finding it
// ---------------------------------------------------------------------------

/// How a located renderer was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A directory named by `ENTANGLED_VIRGL_DIR`.
    Directory,
    /// The verified cache, digest-checked against the pin on the way out.
    Cache,
}

impl Origin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Directory => DIR_ENV,
            Origin::Cache => "the verified cache",
        }
    }
}

/// A usable renderer: the library, and the Venus decoder beside it.
#[derive(Debug, Clone)]
pub struct Renderer {
    pub lib: PathBuf,
    pub server: PathBuf,
    pub origin: Origin,
}

/// The cache directory for one pinned release: `<cache>/virglrenderer/<tag>`.
pub fn cache_dir(pin: &Pinned) -> Result<PathBuf, String> {
    Ok(crate::paths::cache_root()?
        .join("virglrenderer")
        .join(artifact::sanitize(&pin.tag)))
}

/// Where a Venus-capable renderer is on this host, if anywhere.
///
/// In order: an explicit directory, then the verified cache. There is no
/// checkout arm — `build-virglrenderer.sh` installs into a cache of its own and
/// prints the `ENTANGLED_VIRGL_LIB` line that names it, which is the operator
/// override and outranks everything here.
pub fn locate() -> Option<Renderer> {
    if let Some(dir) = std::env::var_os(DIR_ENV).map(PathBuf::from) {
        if let Some(found) = pair_in(&dir, Origin::Directory) {
            return Some(found);
        }
    }
    let pin = pinned().ok()?;
    let dir = cache_dir(&pin).ok()?;
    let found = pair_in(&dir, Origin::Cache)?;
    // The cache is the one origin nobody hand-placed, so it is the one that
    // gets re-checked: a truncated file from a killed download, or a directory
    // left over from a pin that has since moved on, must not become the
    // renderer a guest's Vulkan runs through.
    for (path, name) in [(&found.lib, LIB_FILE), (&found.server, SERVER_FILE)] {
        let asset = pin.asset(name).ok()?;
        if artifact::digest_of(path).ok()? != asset.sha256.to_ascii_lowercase() {
            return None;
        }
    }
    Some(found)
}

fn pair_in(dir: &Path, origin: Origin) -> Option<Renderer> {
    let lib = dir.join(LIB_FILE);
    let server = dir.join(SERVER_FILE);
    (lib.is_file() && server.is_file()).then_some(Renderer {
        lib,
        server,
        origin,
    })
}

/// What to tell someone who has no Venus-capable renderer. One clause per way
/// out, in the order they should try them.
pub fn missing_hint() -> String {
    let mut hint = String::from("run `entangled fetch virglrenderer`");
    if let Ok(pin) = pinned() {
        hint.push_str(&format!(
            " (virglrenderer {}, SHA-256 pinned)",
            pin.virglrenderer_version
        ));
    }
    hint.push_str(&format!(
        ", or build one with `bash {BUILD_SCRIPT}` and point ENTANGLED_VIRGL_LIB at the \
         result, or point {DIR_ENV} at a directory holding {LIB_FILE} and {SERVER_FILE}"
    ));
    hint
}

// ---------------------------------------------------------------------------
// Fetching it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub tag: String,
    pub virglrenderer_version: String,
    pub source: String,
    pub dir: PathBuf,
    pub assets: Vec<FetchedAsset>,
}

/// Downloads (or re-verifies) the pinned renderer into the cache.
pub fn fetch(options: FetchOptions) -> Result<FetchReport, String> {
    let transport = debian_media::UreqTransport::new();
    fetch_with(&transport, options)
}

/// The body, against any `Transport` — which is how the accept and reject paths
/// are tested without a network.
pub fn fetch_with(
    transport: &dyn debian_media::Transport,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    let pin = pinned()?;
    let dir = cache_dir(&pin)?;
    fetch_into(transport, &pin, &dir, options)
}

/// The same, against an explicit pin and directory, so tests can pin digests of
/// bytes they made up.
pub fn fetch_into(
    transport: &dyn debian_media::Transport,
    pin: &Pinned,
    dir: &Path,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    let url = base_url(pin);
    let version = format!("{} ({})", pin.virglrenderer_version, pin.tag);
    let assets = artifact::fetch_into(transport, &pin.release(&url, &version), dir, options)?;
    make_executable(&dir.join(SERVER_FILE))?;
    Ok(FetchReport {
        tag: pin.tag.clone(),
        virglrenderer_version: pin.virglrenderer_version.clone(),
        source: pin.source.clone(),
        dir: dir.to_path_buf(),
        assets,
    })
}

/// A downloaded file arrives without an execute bit, and virglrenderer `exec`s
/// this one. Silent on a host where the concept does not apply.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if !path.is_file() {
        return Ok(());
    }
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the permissions of {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| format!("cannot make {} executable: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin has to parse and name both halves of the pair, or a release
    /// ships a `fetch` that cannot work.
    #[test]
    fn the_compiled_in_pin_parses_and_names_both_assets() {
        let pin = pinned().expect("the compiled-in pin parses");
        assert!(!pin.tag.is_empty(), "a pin with no tag fetches nothing");
        pin.asset(LIB_FILE).expect("the library is pinned");
        pin.asset(SERVER_FILE).expect("the render server is pinned");
        assert!(
            pin.base_url.starts_with("https://"),
            "the base url must be https, got {:?}",
            pin.base_url
        );
    }

    /// Both files or nothing: a library with no render server beside it cannot
    /// serve Venus, and reporting it as a usable renderer would turn a missing
    /// download into a mystery at VM start.
    #[test]
    fn a_directory_with_only_the_library_is_not_a_renderer() {
        let dir = std::env::temp_dir().join(format!(
            "entangled-virgl-pair-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join(LIB_FILE), b"not really a library").expect("write");
        assert!(
            pair_in(&dir, Origin::Cache).is_none(),
            "the library alone must not count"
        );
        std::fs::write(dir.join(SERVER_FILE), b"not really a server").expect("write");
        assert!(
            pair_in(&dir, Origin::Cache).is_some(),
            "both files together are a renderer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hint is what a user reads when 3D will not start, so it has to name
    /// every way in.
    #[test]
    fn the_missing_hint_names_every_way_in() {
        let hint = missing_hint();
        assert!(hint.contains("entangled fetch virglrenderer"), "{hint}");
        assert!(hint.contains(BUILD_SCRIPT), "{hint}");
        assert!(hint.contains(DIR_ENV), "{hint}");
    }
}
