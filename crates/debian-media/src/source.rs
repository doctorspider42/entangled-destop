//! URL resolution for official Debian sources (backlog MVP-601/605).
//!
//! Nothing in here contains a release number. The `stable` suite and the
//! `current` symlink do the version resolution server-side; the concrete
//! version is read back out of the signed metadata at fetch time.

use crate::digest::DigestAlgo;
use crate::keyring::{Keyring, DEBIAN_ARCHIVE, DEBIAN_CD};

/// Guest architecture. Only amd64 in the MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Amd64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
        }
    }
}

impl std::str::FromStr for Arch {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "amd64" | "x86_64" => Ok(Arch::Amd64),
            _ => Err(()),
        }
    }
}

/// Installer flavors supported by `vmhost fetch debian --variant …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerVariant {
    /// Text-mode netboot: rescue mode and the simplest integration tests.
    TextNetboot,
    /// Graphical (GTK) netboot: the recommended MVP path — exercises network,
    /// virtio-gpu and input at once.
    GtkNetboot,
    /// Full netinst ISO from the `current` directory (compatibility path).
    NetinstIso,
}

impl InstallerVariant {
    /// The spelling accepted and printed by the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            InstallerVariant::TextNetboot => "text-netboot",
            InstallerVariant::GtkNetboot => "gtk-netboot",
            InstallerVariant::NetinstIso => "netinst-iso",
        }
    }
}

impl std::str::FromStr for InstallerVariant {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text-netboot" => Ok(InstallerVariant::TextNetboot),
            "gtk-netboot" => Ok(InstallerVariant::GtkNetboot),
            "netinst-iso" => Ok(InstallerVariant::NetinstIso),
            _ => Err(()),
        }
    }
}

/// One downloadable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Kernel,
    Initrd,
    Iso,
    Sha512Sums,
    Sha512SumsSignature,
}

/// How the digest list covering a variant's artifacts is authenticated
/// (MVP-606/607).
///
/// Debian does not use one scheme for everything, so neither can we:
///
/// * CD images carry a detached `SHA512SUMS.sign` in the same directory.
/// * The netboot kernel/initrd live inside the package archive, whose
///   `images/` directory has no signature at all — the archive `Release` file
///   is the signed root and it commits to `SHA256SUMS` by digest.
///
/// The keyring is part of the trust root rather than inferred from its shape, so
/// a source fully describes what must have signed it.
#[derive(Debug, Clone)]
pub enum TrustRoot {
    /// `<dir>/SHA512SUMS` + `<dir>/SHA512SUMS.sign`, verified against
    /// `keyring`. Digest entries are relative to `dir_url`.
    DetachedSums {
        dir_url: String,
        sums_file: &'static str,
        signature_file: &'static str,
        algo: DigestAlgo,
        keyring: &'static Keyring,
    },
    /// `<dists_url>/Release` + `Release.gpg`, verified against `keyring`; the
    /// `Release` file commits to `index_path`'s SHA-256, and that index's digest
    /// entries are relative to `index_base_url`.
    ArchiveRelease {
        dists_url: String,
        /// Path of the digest index relative to `dists_url`.
        index_path: String,
        /// URL the digest index's entries are relative to.
        index_base_url: String,
        algo: DigestAlgo,
        keyring: &'static Keyring,
    },
}

impl TrustRoot {
    /// URL the artifacts' digest entries are relative to.
    pub fn artifact_base_url(&self) -> &str {
        match self {
            TrustRoot::DetachedSums { dir_url, .. } => dir_url,
            TrustRoot::ArchiveRelease { index_base_url, .. } => index_base_url,
        }
    }

    /// Digest algorithm of the artifact-covering digest list.
    pub fn algo(&self) -> DigestAlgo {
        match self {
            TrustRoot::DetachedSums { algo, .. } | TrustRoot::ArchiveRelease { algo, .. } => *algo,
        }
    }

    /// The pinned keyring that must have signed this root.
    pub fn keyring(&self) -> &'static Keyring {
        match self {
            TrustRoot::DetachedSums { keyring, .. } | TrustRoot::ArchiveRelease { keyring, .. } => {
                keyring
            }
        }
    }
}

/// A distribution source that can name its artifact URLs. Debian stable is
/// the only implementation in the MVP; the trait keeps `vmhost fetch`
/// distro-agnostic for later.
pub trait MediaSource {
    /// Base URL of the directory holding `kind` for `variant`.
    fn directory_url(&self, variant: InstallerVariant, kind: MediaKind) -> String;

    /// File name within [`directory_url`](Self::directory_url) when it is
    /// fixed; `None` when it must be discovered from the checksum file (the
    /// ISO name embeds the release number).
    fn file_name(&self, variant: InstallerVariant, kind: MediaKind) -> Option<&'static str>;

    /// How the digest list covering `variant` is authenticated.
    fn trust_root(&self, variant: InstallerVariant) -> TrustRoot;

    /// Path of an artifact *as listed in the signed digest list*, relative to
    /// [`TrustRoot::artifact_base_url`]. `None` when the name is discovered
    /// (the netinst ISO).
    fn sums_path(&self, variant: InstallerVariant, kind: MediaKind) -> Option<String>;

    /// The artifacts `vmhost fetch --variant <variant>` must produce, in order.
    fn artifacts(&self, variant: InstallerVariant) -> &'static [MediaKind];
}

/// Official Debian stable channel.
#[derive(Debug, Clone)]
pub struct DebianStableSource {
    pub arch: Arch,
}

impl DebianStableSource {
    const ARCHIVE_BASE: &'static str = "https://deb.debian.org/debian";
    const SUITE: &'static str = "stable";
    const CD_BASE: &'static str = "https://cdimage.debian.org/debian-cd/current";

    pub fn new(arch: Arch) -> Self {
        Self { arch }
    }

    fn dists_url(&self) -> String {
        format!("{}/dists/{}", Self::ARCHIVE_BASE, Self::SUITE)
    }

    /// `dists/stable/main/installer-<arch>/current/images` — the root the
    /// archive-side `SHA256SUMS` entries are relative to.
    fn installer_images_rel(&self) -> String {
        format!("main/installer-{}/current/images", self.arch.as_str())
    }

    fn netboot_dir(&self, variant: InstallerVariant) -> String {
        format!(
            "{}/{}",
            self.dists_url(),
            self.netboot_rel(variant)
                .map(|rel| format!("{}/{rel}", self.installer_images_rel()))
                .unwrap_or_else(|| self.installer_images_rel())
        )
    }

    /// Directory of the netboot artifacts relative to the `images/` root.
    fn netboot_rel(&self, variant: InstallerVariant) -> Option<String> {
        let arch = self.arch.as_str();
        match variant {
            InstallerVariant::TextNetboot => Some(format!("netboot/debian-installer/{arch}")),
            InstallerVariant::GtkNetboot => Some(format!("netboot/gtk/debian-installer/{arch}")),
            InstallerVariant::NetinstIso => None,
        }
    }

    fn iso_dir(&self) -> String {
        format!("{}/{}/iso-cd", Self::CD_BASE, self.arch.as_str())
    }
}

impl MediaSource for DebianStableSource {
    fn directory_url(&self, variant: InstallerVariant, _kind: MediaKind) -> String {
        match variant {
            InstallerVariant::NetinstIso => self.iso_dir(),
            v => self.netboot_dir(v),
        }
    }

    fn file_name(&self, variant: InstallerVariant, kind: MediaKind) -> Option<&'static str> {
        match (variant, kind) {
            (_, MediaKind::Sha512Sums) => Some("SHA512SUMS"),
            (_, MediaKind::Sha512SumsSignature) => Some("SHA512SUMS.sign"),
            (InstallerVariant::NetinstIso, MediaKind::Iso) => None, // discovered from SHA512SUMS
            (_, MediaKind::Kernel) => Some("linux"),
            (_, MediaKind::Initrd) => Some("initrd.gz"),
            _ => None,
        }
    }

    fn trust_root(&self, variant: InstallerVariant) -> TrustRoot {
        match variant {
            InstallerVariant::NetinstIso => TrustRoot::DetachedSums {
                dir_url: self.iso_dir(),
                sums_file: "SHA512SUMS",
                signature_file: "SHA512SUMS.sign",
                algo: DigestAlgo::Sha512,
                keyring: &DEBIAN_CD,
            },
            InstallerVariant::TextNetboot | InstallerVariant::GtkNetboot => {
                let images = self.installer_images_rel();
                TrustRoot::ArchiveRelease {
                    dists_url: self.dists_url(),
                    index_path: format!("{images}/SHA256SUMS"),
                    index_base_url: format!("{}/{images}", self.dists_url()),
                    algo: DigestAlgo::Sha256,
                    keyring: &DEBIAN_ARCHIVE,
                }
            }
        }
    }

    fn sums_path(&self, variant: InstallerVariant, kind: MediaKind) -> Option<String> {
        let rel = self.netboot_rel(variant)?;
        let name = self.file_name(variant, kind)?;
        Some(format!("{rel}/{name}"))
    }

    fn artifacts(&self, variant: InstallerVariant) -> &'static [MediaKind] {
        match variant {
            InstallerVariant::NetinstIso => &[MediaKind::Iso],
            _ => &[MediaKind::Kernel, MediaKind::Initrd],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> DebianStableSource {
        DebianStableSource { arch: Arch::Amd64 }
    }

    #[test]
    fn gtk_netboot_urls_match_backlog() {
        let s = src();
        assert_eq!(
            s.directory_url(InstallerVariant::GtkNetboot, MediaKind::Kernel),
            "https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/gtk/debian-installer/amd64"
        );
        assert_eq!(
            s.file_name(InstallerVariant::GtkNetboot, MediaKind::Initrd),
            Some("initrd.gz")
        );
    }

    #[test]
    fn text_netboot_urls_match_backlog() {
        let s = src();
        assert_eq!(
            s.directory_url(InstallerVariant::TextNetboot, MediaKind::Kernel),
            "https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/debian-installer/amd64"
        );
    }

    #[test]
    fn iso_comes_from_current_directory_without_pinned_name() {
        let s = src();
        assert_eq!(
            s.directory_url(InstallerVariant::NetinstIso, MediaKind::Iso),
            "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd"
        );
        assert_eq!(
            s.file_name(InstallerVariant::NetinstIso, MediaKind::Iso),
            None
        );
    }

    #[test]
    fn no_url_contains_a_release_number() {
        let s = src();
        for variant in [
            InstallerVariant::TextNetboot,
            InstallerVariant::GtkNetboot,
            InstallerVariant::NetinstIso,
        ] {
            let root = s.trust_root(variant);
            let mut urls = vec![root.artifact_base_url().to_string()];
            if let TrustRoot::ArchiveRelease {
                dists_url,
                index_path,
                ..
            } = &root
            {
                urls.push(dists_url.clone());
                urls.push(index_path.clone());
            }
            urls.push(s.directory_url(variant, MediaKind::Kernel));
            for url in urls {
                // The architecture name and the digest algorithm names are the
                // only places a digit may legitimately appear.
                let stripped = url
                    .replace(s.arch.as_str(), "")
                    .replace("SHA256SUMS", "")
                    .replace("SHA512SUMS", "");
                assert!(
                    !stripped.chars().any(|c| c.is_ascii_digit()),
                    "{url} looks like it pins a release number"
                );
                assert!(
                    url.starts_with("https://") || !url.contains("://"),
                    "{url} is not HTTPS"
                );
            }
        }
    }

    #[test]
    fn netboot_trust_root_is_the_signed_archive_release() {
        let s = src();
        let root = s.trust_root(InstallerVariant::GtkNetboot);
        assert_eq!(root.algo(), DigestAlgo::Sha256);
        assert_eq!(root.keyring().name, "Debian archive");
        match root {
            TrustRoot::ArchiveRelease {
                dists_url,
                index_path,
                index_base_url,
                ..
            } => {
                assert_eq!(dists_url, "https://deb.debian.org/debian/dists/stable");
                assert_eq!(index_path, "main/installer-amd64/current/images/SHA256SUMS");
                assert_eq!(
                    index_base_url,
                    "https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images"
                );
            }
            other => panic!("unexpected trust root {other:?}"),
        }
    }

    #[test]
    fn iso_trust_root_is_the_detached_cd_signature() {
        let s = src();
        let root = s.trust_root(InstallerVariant::NetinstIso);
        assert_eq!(root.algo(), DigestAlgo::Sha512);
        assert_eq!(root.keyring().name, "Debian CD");
        assert_eq!(
            root.artifact_base_url(),
            "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd"
        );
    }

    /// The netboot kernel is the same file for both variants but the initrds
    /// differ, so the sums paths must not collide.
    #[test]
    fn netboot_sums_paths_are_variant_specific() {
        let s = src();
        assert_eq!(
            s.sums_path(InstallerVariant::TextNetboot, MediaKind::Initrd)
                .as_deref(),
            Some("netboot/debian-installer/amd64/initrd.gz")
        );
        assert_eq!(
            s.sums_path(InstallerVariant::GtkNetboot, MediaKind::Initrd)
                .as_deref(),
            Some("netboot/gtk/debian-installer/amd64/initrd.gz")
        );
        assert_eq!(
            s.sums_path(InstallerVariant::NetinstIso, MediaKind::Iso),
            None
        );
    }

    #[test]
    fn variant_names_round_trip() {
        for v in [
            InstallerVariant::TextNetboot,
            InstallerVariant::GtkNetboot,
            InstallerVariant::NetinstIso,
        ] {
            assert_eq!(v.as_str().parse::<InstallerVariant>(), Ok(v));
        }
        assert!("cd-rom".parse::<InstallerVariant>().is_err());
        assert_eq!("amd64".parse::<Arch>(), Ok(Arch::Amd64));
        assert!("riscv64".parse::<Arch>().is_err());
    }

    #[test]
    fn artifact_lists_match_the_variant() {
        let s = src();
        assert_eq!(
            s.artifacts(InstallerVariant::GtkNetboot),
            &[MediaKind::Kernel, MediaKind::Initrd]
        );
        assert_eq!(s.artifacts(InstallerVariant::NetinstIso), &[MediaKind::Iso]);
    }
}
