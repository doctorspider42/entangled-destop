//! What a Windows guest needs besides Microsoft's own ISO (ADR-0008,
//! ADR-0009): the virtio-win drivers, and WinFsp for shared folders —
//! downloaded from their publishers and verified against SHA-256 digests
//! compiled into this binary (`guest/windows/pinned.toml`), through the same
//! machinery as the firmware ([`crate::artifact`]). With them `install
//! windows` needs nothing a user has to go and find.

use std::path::PathBuf;

use serde::Deserialize;

use crate::artifact::{self, FetchOptions, Hint, PinnedAsset};

/// The compiled-in pins.
const PINNED_TOML: &str = include_str!("../../../guest/windows/pinned.toml");

/// Where they live, for messages that ask a human to look.
const PIN_PATH: &str = "guest/windows/pinned.toml";

/// Relocates every download here (a mirror holding the same files under the
/// same `<tag>/<name>` paths).
const BASE_URL_ENV: &str = "ENTANGLED_WINDOWS_MEDIA_BASE_URL";

/// One downloadable piece of a Windows guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    /// virtio-win's driver ISO: Setup sees neither disk nor network without it.
    VirtioWin,
    /// WinFsp's installer: a shared folder as a drive (virtio-fs).
    WinFsp,
}

impl Media {
    /// The spelling `entangled fetch` accepts, and the cache directory name.
    pub const fn target(self) -> &'static str {
        match self {
            Self::VirtioWin => "virtio-win",
            Self::WinFsp => "winfsp",
        }
    }

    /// What it is, for messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::VirtioWin => "the virtio-win drivers",
            Self::WinFsp => "WinFsp",
        }
    }

    /// The `entangled fetch` target named `name`, if it is one of these.
    pub fn from_target(name: &str) -> Option<Self> {
        [Self::VirtioWin, Self::WinFsp]
            .into_iter()
            .find(|m| m.target().eq_ignore_ascii_case(name))
    }
}

/// One pinned upstream release.
#[derive(Debug, Clone, Deserialize)]
pub struct Pin {
    pub version: String,
    pub base_url: String,
    pub tag: String,
    pub source: String,
    pub assets: Vec<PinnedAsset>,
}

#[derive(Debug, Deserialize)]
struct Pins {
    virtio_win: Pin,
    winfsp: Pin,
}

/// The compiled-in pin of `media`. A parse failure is a build mistake, so it
/// names the file.
pub fn pin(media: Media) -> Result<Pin, String> {
    let pins: Pins = toml::from_str(PINNED_TOML)
        .map_err(|e| format!("{PIN_PATH} is not a valid pin file: {e}"))?;
    let pin = match media {
        Media::VirtioWin => pins.virtio_win,
        Media::WinFsp => pins.winfsp,
    };
    artifact::validate_digests(PIN_PATH, &pin.assets)?;
    if pin.assets.len() != 1 {
        return Err(format!(
            "{PIN_PATH}: {} must name exactly one file",
            media.target()
        ));
    }
    Ok(pin)
}

/// `<cache>/<target>/<tag>`.
pub fn cache_dir(media: Media, pin: &Pin) -> Result<PathBuf, String> {
    Ok(crate::paths::cache_root()?
        .join(media.target())
        .join(artifact::sanitize(&pin.tag)))
}

/// Where the pinned file lands in the cache — whether or not it is there yet.
pub fn cached_path(media: Media) -> Result<PathBuf, String> {
    let pin = pin(media)?;
    Ok(cache_dir(media, &pin)?.join(artifact::sanitize(&pin.assets[0].name)))
}

/// The pinned file, from the verified cache when it is there and matches,
/// downloaded and verified otherwise (unless `options.offline`).
pub fn fetch(media: Media, options: FetchOptions) -> Result<PathBuf, String> {
    fetch_with(&debian_media::UreqTransport::new(), media, options)
}

/// [`fetch`] against any transport, for the tests.
pub fn fetch_with(
    transport: &dyn debian_media::Transport,
    media: Media,
    options: FetchOptions,
) -> Result<PathBuf, String> {
    let pin = pin(media)?;
    let dir = cache_dir(media, &pin)?;
    let base_url = std::env::var(BASE_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| pin.base_url.clone());
    let version = format!("{} {}", media.target(), pin.version);
    let release = artifact::Release {
        pin_path: PIN_PATH,
        tag: &pin.tag,
        base_url: &base_url,
        version: &version,
        assets: &pin.assets,
        hint: Hint {
            workflow: "its publisher",
            base_url_env: BASE_URL_ENV,
            dir_env: "--drivers/--winfsp",
        },
    };
    let mut fetched = artifact::fetch_into(transport, &release, &dir, options)?;
    fetched
        .pop()
        .map(|asset| asset.path)
        .ok_or_else(|| format!("{PIN_PATH} names no file for {}", media.target()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_pins_parse_and_name_one_file_each() {
        for media in [Media::VirtioWin, Media::WinFsp] {
            let pin = pin(media).expect("a valid pin");
            assert_eq!(pin.assets.len(), 1);
            assert_eq!(Media::from_target(media.target()), Some(media));
        }
        assert!(pin(Media::VirtioWin).unwrap().assets[0]
            .name
            .ends_with(".iso"));
        assert!(pin(Media::WinFsp).unwrap().assets[0].name.ends_with(".msi"));
        assert_eq!(Media::from_target("virgl"), None);
    }
}
