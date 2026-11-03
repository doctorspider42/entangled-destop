//! Which system an installer ISO holds, read from the image itself — what the
//! manager's wizard asks the moment a file is dropped on it, so the person
//! never has to say "this is Ubuntu" about a file that already says so.
//!
//! Three facts decide it, in this order:
//!
//! * a `sources/install.wim` (or `.esd`, or split `.swm`) on the image:
//!   Windows Setup media, whatever the label;
//! * `.disk/info`, the one-line description Ubuntu and Debian discs carry
//!   (`Ubuntu 26.04.1 LTS "…" - Release amd64 (…)`);
//! * the ISO 9660 volume label (`Ubuntu-Server 24.04 LTS amd64`,
//!   `Debian 13.1.0 amd64 n`, `Fedora-E-dvd-x86_64-44`).
//!
//! Everything read is untrusted: the image is a file from the internet. The
//! description is bounded, stripped of control characters and cut short before
//! it is shown to anyone, and a malformed image is simply [`InstallerKind::Unknown`]
//! — never an error that stops a person from booting it by hand.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::{open, MediaError, SECTOR};

/// The ISO 9660 primary volume descriptor's sector, and its volume identifier
/// (ECMA-119 §8.4.6: offset 40, 32 bytes, space padded) — what `blkid` calls
/// `LABEL=`.
const PVD_SECTOR: u64 = 16;
const VOLUME_ID_OFFSET: usize = 40;
const VOLUME_ID_LEN: usize = 32;

/// Largest `.disk/info` read. Real ones are one line of under 100 bytes.
const MAX_DISK_INFO: u64 = 4096;

/// Longest title shown for an image.
const MAX_TITLE: usize = 80;

/// The installer families this product knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerKind {
    /// Windows Setup media (Microsoft's ISO).
    Windows,
    /// An Ubuntu installer; `desktop` is false for the Server image.
    Ubuntu {
        desktop: bool,
    },
    Debian,
    /// A Fedora image; `live` images cannot be installed unattended.
    Fedora {
        live: bool,
    },
    /// Anything else: boot it and install by hand.
    Unknown,
}

/// What [`identify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identified {
    pub kind: InstallerKind,
    /// A short human description: the disc's own `.disk/info` line when it
    /// has one, else its volume label. Empty when the image has neither.
    pub title: String,
}

/// Reads `path` and says which installer it is. Only an image that cannot be
/// opened at all is an error; one that is merely unfamiliar is
/// [`InstallerKind::Unknown`].
pub fn identify(path: &Path) -> Result<Identified, MediaError> {
    let mut file = std::fs::File::open(path).map_err(|source| MediaError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let label = volume_label(&mut file).unwrap_or_default();
    let (windows, disk_info) = match open(path) {
        Ok(mut image) => {
            let windows = [
                "sources/install.wim",
                "sources/install.esd",
                "sources/install.swm",
            ]
            .iter()
            .any(|p| matches!(image.find(p), Ok(Some(entry)) if !entry.is_dir));
            let disk_info = match image.find(".disk/info") {
                Ok(Some(entry)) if !entry.is_dir => image
                    .read_file(&entry.file, MAX_DISK_INFO)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            };
            (windows, disk_info)
        }
        Err(_) => (false, None),
    };
    Ok(classify(&label, disk_info.as_deref(), windows))
}

/// The decision, from what the image says — pure, so every rule is tested
/// without an ISO on disk.
pub fn classify(label: &str, disk_info: Option<&str>, windows_setup: bool) -> Identified {
    let label = clean(label);
    let info = disk_info
        .map(|text| clean(text.lines().next().unwrap_or("")))
        .unwrap_or_default();
    let title = if info.is_empty() {
        label.clone()
    } else {
        info.clone()
    };
    let kind = if windows_setup {
        InstallerKind::Windows
    } else {
        // `.disk/info` first: it is the distribution's own words; the label
        // is the fallback for discs without one (Fedora, a respin).
        let says = |text: &str| -> Option<InstallerKind> {
            let lower = text.to_ascii_lowercase();
            if lower.starts_with("ubuntu") {
                Some(InstallerKind::Ubuntu {
                    desktop: !lower.starts_with("ubuntu-server") && !lower.contains("server"),
                })
            } else if lower.starts_with("debian") {
                Some(InstallerKind::Debian)
            } else if lower.starts_with("fedora") {
                Some(InstallerKind::Fedora {
                    live: text.contains("Live"),
                })
            } else {
                None
            }
        };
        says(&info)
            .or_else(|| says(&label))
            .unwrap_or(InstallerKind::Unknown)
    };
    let title = match kind {
        InstallerKind::Windows => "Windows 10 / 11".to_string(),
        _ => title,
    };
    Identified { kind, title }
}

/// The ISO 9660 volume label, trimmed; `None` for an image without a primary
/// volume descriptor.
fn volume_label<R: Read + Seek>(r: &mut R) -> Option<String> {
    let mut sector = vec![0u8; SECTOR as usize];
    r.seek(SeekFrom::Start(PVD_SECTOR * SECTOR)).ok()?;
    r.read_exact(&mut sector).ok()?;
    if sector[0] != 1 || &sector[1..6] != b"CD001" {
        return None;
    }
    let raw = &sector[VOLUME_ID_OFFSET..VOLUME_ID_OFFSET + VOLUME_ID_LEN];
    Some(String::from_utf8_lossy(raw).trim().to_string())
}

/// Printable characters only, whitespace collapsed, at most [`MAX_TITLE`].
fn clean(text: &str) -> String {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = words.join(" ");
    if out.chars().count() > MAX_TITLE {
        out = out.chars().take(MAX_TITLE - 1).collect::<String>() + "…";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optical::iso9660::IsoBuilder;

    #[test]
    fn windows_setup_wins_over_any_label() {
        let found = classify("CCCOMA_X64FRE_EN-US_DV9", None, true);
        assert_eq!(found.kind, InstallerKind::Windows);
        assert_eq!(found.title, "Windows 10 / 11");
    }

    #[test]
    fn ubuntu_desktop_and_server_are_told_apart() {
        let desktop = classify(
            "Ubuntu 26.04.1 LTS amd64",
            Some("Ubuntu 26.04.1 LTS \"Resolute Raccoon\" - Release amd64 (20260806)\n"),
            false,
        );
        assert_eq!(desktop.kind, InstallerKind::Ubuntu { desktop: true });
        assert!(desktop.title.starts_with("Ubuntu 26.04.1 LTS"));
        let server = classify("Ubuntu-Server 24.04 LTS amd64", None, false);
        assert_eq!(server.kind, InstallerKind::Ubuntu { desktop: false });
    }

    #[test]
    fn debian_fedora_and_strangers() {
        assert_eq!(
            classify("Debian 13.1.0 amd64 n", None, false).kind,
            InstallerKind::Debian
        );
        assert_eq!(
            classify("Fedora-E-dvd-x86_64-44", None, false).kind,
            InstallerKind::Fedora { live: false }
        );
        assert_eq!(
            classify("Fedora-WS-Live-44-1-1", None, false).kind,
            InstallerKind::Fedora { live: true }
        );
        let other = classify("ARCH_202611", None, false);
        assert_eq!(other.kind, InstallerKind::Unknown);
        assert_eq!(other.title, "ARCH_202611");
    }

    #[test]
    fn a_hostile_description_is_tamed() {
        let found = classify(
            "",
            Some(&format!("Ubuntu\u{1b}[31m{}\n2nd", "x".repeat(500))),
            false,
        );
        assert_eq!(found.kind, InstallerKind::Ubuntu { desktop: true });
        assert!(!found.title.contains('\u{1b}'));
        assert!(found.title.chars().count() <= MAX_TITLE);
        assert!(!found.title.contains("2nd"));
    }

    #[test]
    fn reads_a_real_image() {
        let mut builder = IsoBuilder::new("DEBIAN 13 AMD64");
        builder
            .add_file(
                ".disk/info",
                b"Debian GNU/Linux 13.1.0 \"Trixie\" - Official amd64 NETINST".to_vec(),
            )
            .unwrap();
        let path = std::env::temp_dir().join(format!("identify-{}.iso", std::process::id()));
        std::fs::write(&path, builder.build()).unwrap();
        let found = identify(&path);
        let _ = std::fs::remove_file(&path);
        let found = found.unwrap();
        assert_eq!(found.kind, InstallerKind::Debian);
        assert!(
            found.title.starts_with("Debian GNU/Linux 13.1.0"),
            "{}",
            found.title
        );
    }

    /// Real installer images, when `ENTANGLED_IDENTIFY_ISOS` lists them
    /// (`;`-separated `path=expected` pairs, expected one of `windows`,
    /// `ubuntu-desktop`, `ubuntu-server`, `debian`, `fedora`, `fedora-live`,
    /// `unknown`).
    #[test]
    #[ignore = "needs installer ISOs named by ENTANGLED_IDENTIFY_ISOS"]
    fn reads_real_installer_images() {
        let list = std::env::var("ENTANGLED_IDENTIFY_ISOS").unwrap_or_default();
        for pair in list.split(';').filter(|p| !p.is_empty()) {
            let (path, expected) = pair.rsplit_once('=').expect("path=expected");
            let found = identify(Path::new(path)).unwrap();
            let name = match found.kind {
                InstallerKind::Windows => "windows",
                InstallerKind::Ubuntu { desktop: true } => "ubuntu-desktop",
                InstallerKind::Ubuntu { desktop: false } => "ubuntu-server",
                InstallerKind::Debian => "debian",
                InstallerKind::Fedora { live: false } => "fedora",
                InstallerKind::Fedora { live: true } => "fedora-live",
                InstallerKind::Unknown => "unknown",
            };
            println!("{path}: {name} — {}", found.title);
            assert_eq!(name, expected, "{path}");
        }
    }

    #[test]
    fn junk_is_unknown_not_an_error() {
        let path = std::env::temp_dir().join(format!("identify-junk-{}.iso", std::process::id()));
        std::fs::write(&path, vec![0x5au8; 64 * 1024]).unwrap();
        let found = identify(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(found.unwrap().kind, InstallerKind::Unknown);
    }
}
