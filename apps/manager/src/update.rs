//! Startup update check against the GitHub Releases API.
//!
//! Rules of the house apply: the check runs on its own thread and reports
//! through a channel plus the [`crate::process::Waker`] — the frame loop never
//! blocks on the network. Every failure mode (offline, rate-limited, malformed
//! answer) is logged via `tracing` and swallowed; an update check must never
//! produce an error popup. The check is opt-out-able through
//! `Settings::check_updates_on_startup`.
//!
//! Nothing in this module touches the network in tests: the HTTP fetch is a
//! thin function and everything interesting (semver comparison, response
//! parsing, asset selection) is pure and unit-tested.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc;

use serde::Deserialize;

use crate::process::Waker;

/// Where releases live. The release workflow publishes an Inno Setup installer
/// as `entangled-desktop-<version>-setup.exe` on every push to main.
pub const REPO: &str = "doctorspider42/entangled-destop";

/// `GET /releases/latest` — GitHub serves the newest non-draft, non-prerelease
/// release here, which is exactly the "latest" the banner should offer.
fn latest_release_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// The largest API answer / installer we are willing to read, as a guard
/// against a nonsense response tying the thread up forever.
const MAX_API_BYTES: u64 = 1024 * 1024;
const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

/// A `MAJOR.MINOR.PATCH` version. Ordering is derived field by field, which is
/// exactly semver precedence for the plain numeric form the release pipeline
/// produces (`0.2.<run number>`). A pre-release or build suffix (`-rc1`, `+g1`)
/// is tolerated on parse and ignored for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `1.2.3`, `v1.2.3`, `1.2` (patch 0) and `1.2.3-rc1`; refuses
    /// anything with more dots, empty or non-numeric components.
    pub fn parse(text: &str) -> Option<Self> {
        let core = text
            .trim()
            .strip_prefix('v')
            .unwrap_or_else(|| text.trim())
            .split(['-', '+'])
            .next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A newer release the banner offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: Version,
    /// Direct download of the Windows installer asset, when the release
    /// carries one.
    pub installer_url: Option<String>,
    pub installer_name: Option<String>,
    /// The release page, for hosts that cannot run the installer.
    pub page_url: String,
}

// The subset of the GitHub Releases API answer the check needs.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// Decides whether `json` (a `/releases/latest` answer) advertises something
/// newer than `current`. Pure, so the whole decision is testable offline.
pub fn parse_latest(json: &str, current: Version) -> Result<Option<UpdateInfo>, String> {
    let release: ApiRelease = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if release.draft || release.prerelease {
        // `/releases/latest` should never serve these; trust nothing.
        return Ok(None);
    }
    let Some(version) = Version::parse(&release.tag_name) else {
        return Err(format!("unparsable tag '{}'", release.tag_name));
    };
    if version <= current {
        return Ok(None);
    }
    let installer = release.assets.iter().find(|a| is_installer_asset(&a.name));
    Ok(Some(UpdateInfo {
        version,
        installer_url: installer.map(|a| a.browser_download_url.clone()),
        installer_name: installer.map(|a| a.name.clone()),
        page_url: release.html_url,
    }))
}

/// The release workflow names the asset `entangled-desktop-<v>-setup.exe`;
/// match on the shape rather than the exact version.
fn is_installer_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".exe") && lower.contains("setup")
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .user_agent(concat!("entangled-manager/", env!("ENTANGLED_VERSION")))
        .max_redirects(8)
        .build();
    ureq::Agent::new_with_config(config)
}

fn get_capped(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > limit {
        return Err(format!("response exceeds {limit} bytes"));
    }
    Ok(buf)
}

/// Spawns the startup check. Sends at most one [`UpdateInfo`] (only when a
/// newer version exists) and wakes the UI; any failure is a debug-level log
/// line and silence.
pub fn spawn_check(current: Version, waker: Waker) -> mpsc::Receiver<UpdateInfo> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("update-check".to_string())
        .spawn(move || {
            let json = match get_capped(&agent(), &latest_release_url(), MAX_API_BYTES) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::debug!(error = %e, "update check: cannot reach the releases API");
                    return;
                }
            };
            let json = String::from_utf8_lossy(&json).into_owned();
            match parse_latest(&json, current) {
                Ok(Some(info)) => {
                    tracing::info!(version = %info.version, "update check: newer release found");
                    if tx.send(info).is_ok() {
                        waker();
                    }
                }
                Ok(None) => tracing::debug!("update check: this build is current"),
                Err(e) => tracing::debug!(error = %e, "update check: unusable API answer"),
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "update check: cannot start the thread");
    }
    rx
}

/// Where a downloaded installer lands: `~/Downloads` when it exists, the OS
/// temporary directory otherwise.
pub fn download_dir() -> PathBuf {
    if let Some(home) = crate::settings::home_dir() {
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
    }
    std::env::temp_dir()
}

/// Downloads the installer asset and launches it (Windows). Runs on its own
/// thread; the result comes back over the channel and through the waker.
pub fn spawn_download_and_launch(
    info: UpdateInfo,
    waker: Waker,
) -> mpsc::Receiver<Result<PathBuf, String>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("update-download".to_string())
        .spawn(move || {
            let result = download_and_launch(&info);
            if tx.send(result).is_ok() {
                waker();
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "update download: cannot start the thread");
    }
    rx
}

fn download_and_launch(info: &UpdateInfo) -> Result<PathBuf, String> {
    let (Some(url), Some(name)) = (&info.installer_url, &info.installer_name) else {
        return Err("this release carries no Windows installer asset".to_string());
    };
    let bytes = get_capped(&agent(), url, MAX_INSTALLER_BYTES)
        .map_err(|e| format!("download failed: {e}"))?;
    let path = download_dir().join(name);
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    launch_installer(&path)?;
    Ok(path)
}

#[cfg(windows)]
fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    // The installer takes over from here: its CloseApplications step asks this
    // manager to exit before files are replaced.
    std::process::Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot launch {}: {e}", path.display()))
}

#[cfg(not(windows))]
fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    // The asset is a Windows installer; on other hosts the banner offers the
    // release page instead, so this is only reachable programmatically.
    Err(format!(
        "{} is a Windows installer; download it from the release page instead",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("test version")
    }

    #[test]
    fn versions_parse_the_shapes_github_serves() {
        assert_eq!(
            v("v0.2.17"),
            Version {
                major: 0,
                minor: 2,
                patch: 17
            }
        );
        assert_eq!(v("1.2.3"), v("v1.2.3"));
        assert_eq!(v("1.2").patch, 0);
        assert_eq!(v("1.2.3-rc1"), v("1.2.3"));
        assert_eq!(v("1.2.3+build9"), v("1.2.3"));
        for bad in ["", "v", "one.two", "1.2.3.4", "1..3", "-1.0.0"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn version_ordering_is_numeric_not_lexicographic() {
        assert!(v("0.2.10") > v("0.2.9"));
        assert!(v("0.10.0") > v("0.9.99"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.2.0"), v("0.2.0"));
    }

    fn release_json(tag: &str, extra: &str) -> String {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/{REPO}/releases/tag/{tag}",
                {extra}
                "assets": [
                    {{"name": "SHA256SUMS.txt",
                      "browser_download_url": "https://example.invalid/sums"}},
                    {{"name": "entangled-desktop-0.2.9-setup.exe",
                      "browser_download_url": "https://example.invalid/setup.exe"}}
                ]
            }}"#
        )
    }

    #[test]
    fn a_newer_release_yields_the_installer_asset() {
        let info = parse_latest(&release_json("v0.2.9", ""), v("0.2.3"))
            .expect("parse")
            .expect("newer");
        assert_eq!(info.version, v("0.2.9"));
        assert_eq!(
            info.installer_url.as_deref(),
            Some("https://example.invalid/setup.exe")
        );
        assert_eq!(
            info.installer_name.as_deref(),
            Some("entangled-desktop-0.2.9-setup.exe")
        );
        assert!(info.page_url.contains("/releases/tag/v0.2.9"));
    }

    #[test]
    fn an_equal_or_older_release_is_silence() {
        assert_eq!(
            parse_latest(&release_json("v0.2.9", ""), v("0.2.9")).expect("parse"),
            None
        );
        assert_eq!(
            parse_latest(&release_json("v0.2.9", ""), v("0.3.0")).expect("parse"),
            None
        );
    }

    #[test]
    fn drafts_and_prereleases_are_never_offered() {
        for extra in ["\"draft\": true,", "\"prerelease\": true,"] {
            assert_eq!(
                parse_latest(&release_json("v9.9.9", extra), v("0.2.0")).expect("parse"),
                None,
                "{extra} must be ignored"
            );
        }
    }

    #[test]
    fn a_release_without_an_installer_still_reports_the_page() {
        let json = format!(
            r#"{{"tag_name": "v0.3.0",
                 "html_url": "https://github.com/{REPO}/releases/tag/v0.3.0",
                 "assets": []}}"#
        );
        let info = parse_latest(&json, v("0.2.0"))
            .expect("parse")
            .expect("newer");
        assert_eq!(info.installer_url, None);
        assert!(info.page_url.ends_with("v0.3.0"));
    }

    #[test]
    fn garbage_answers_are_errors_not_panics() {
        assert!(parse_latest("not json", v("0.2.0")).is_err());
        assert!(parse_latest(
            r#"{"tag_name": "not-a-version", "html_url": "x"}"#,
            v("0.2.0")
        )
        .is_err());
    }

    #[test]
    fn installer_assets_are_recognised_by_shape() {
        assert!(is_installer_asset("entangled-desktop-0.2.9-setup.exe"));
        assert!(is_installer_asset("Entangled-SETUP.EXE"));
        assert!(!is_installer_asset("entangled-desktop-0.2.9.msi"));
        assert!(!is_installer_asset("setup.exe.sha256"));
        assert!(!is_installer_asset("entangled.exe"));
    }

    #[test]
    fn download_dir_is_always_writable_shaped() {
        // Either ~/Downloads or the temp dir — never an empty path.
        assert!(!download_dir().as_os_str().is_empty());
    }
}
