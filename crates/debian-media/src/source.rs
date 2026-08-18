//! URL resolution for official Debian sources (backlog MVP-601/605).

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

/// One downloadable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Kernel,
    Initrd,
    Iso,
    Sha512Sums,
    Sha512SumsSignature,
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
}

/// Official Debian stable channel.
#[derive(Debug, Clone)]
pub struct DebianStableSource {
    pub arch: Arch,
}

impl DebianStableSource {
    const NETBOOT_BASE: &'static str =
        "https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot";
    const CD_BASE: &'static str = "https://cdimage.debian.org/debian-cd/current";

    fn netboot_dir(&self, variant: InstallerVariant) -> String {
        let arch = self.arch.as_str();
        match variant {
            InstallerVariant::TextNetboot => {
                format!("{}/debian-installer/{arch}", Self::NETBOOT_BASE)
            }
            InstallerVariant::GtkNetboot => {
                format!("{}/gtk/debian-installer/{arch}", Self::NETBOOT_BASE)
            }
            InstallerVariant::NetinstIso => unreachable!("ISO artifacts use CD_BASE"),
        }
    }

    fn iso_dir(&self) -> String {
        format!("{}/{}/iso-cd", Self::CD_BASE, self.arch.as_str())
    }
}

impl MediaSource for DebianStableSource {
    fn directory_url(&self, variant: InstallerVariant, kind: MediaKind) -> String {
        match (variant, kind) {
            (InstallerVariant::NetinstIso, _) => self.iso_dir(),
            (v, _) => self.netboot_dir(v),
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
}
