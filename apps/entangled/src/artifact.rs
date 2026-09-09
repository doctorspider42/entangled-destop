//! The machinery behind every **published guest artifact**: the pin, the
//! download, the digest check, the cache and the provenance note.
//!
//! There are two of them now — the bootstrap kernel + initramfs
//! ([`crate::bootstrap`]) and the UEFI firmware ([`crate::firmware`]) — and
//! they want exactly the same thing: a SHA-256 compiled into this binary, an
//! immutable release tag, bytes that are deleted rather than used when they
//! miss the digest, and a `<cache>/<kind>/<tag>/` directory that a second run
//! finds without touching the network. This module is that, once. The two
//! callers differ only in what they name and what they say when it is missing.
//!
//! # What is trusted, and how strongly
//!
//! The pin is a TOML file in *our source tree*, **compiled into this binary**
//! by `include_str!`. It names the release tag, the asset file names and a
//! SHA-256 per asset. A download that does not hash to the pinned digest is
//! deleted, not used.
//!
//! That anchor is worth being precise about, because it is easy to overstate:
//!
//! * the digest is reviewable in `git log` and shipped inside the executable
//!   the user already decided to run. It is **not** a checksum file fetched
//!   from beside the artifact, which would give an attacker who can serve the
//!   artifact the checksum too;
//! * it is **not** a signature. There is no key here and nothing to revoke.
//!   Whoever can land a commit can change the pin, and whoever can publish a
//!   release under the pinned tag *before* the pin is written can choose what
//!   the pin then records. Tags are immutable once assets are attached, which
//!   is what makes an already-pinned release safe to re-fetch forever;
//! * TLS to `github.com` is the transport, so the download is at least not
//!   attacker-modifiable in flight even before the digest check.
//!
//! In short: as strong as the git history of this repository, and no stronger.
//!
//! # The private-repository problem
//!
//! This repository is private, and a plain
//! `https://github.com/<owner>/<repo>/releases/download/<tag>/<file>` URL
//! answers **404** to anyone without credentials — indistinguishable from "not
//! published yet". Private release assets are only reachable through the API:
//! resolve the release by tag, find the asset's id, then `GET` that asset's API
//! URL with `Accept: application/octet-stream`. [`github_token`] finds a token
//! if the host has one (`GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token`) and
//! [`fetch_into`] takes that route when it does. With no token the browser URL
//! is used, and a 404 says so in as many words instead of pretending the
//! release is missing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use debian_media::{DigestAlgo, Download, Manifest, Transport, TransportError};
use serde::Deserialize;

/// One file inside a pinned release.
#[derive(Debug, Clone, Deserialize)]
pub struct PinnedAsset {
    pub name: String,
    /// Lower-case hex SHA-256 of the asset, 64 characters.
    pub sha256: String,
    /// Size in bytes, so the download can be announced and a wildly wrong
    /// response rejected before it is hashed.
    pub bytes: u64,
}

/// Nothing we publish is remotely this big; a server that streams forever is a
/// bug or an attack, and either way it must not fill the disk. The kernel is
/// ~13 MiB and the firmware is exactly 4 MiB.
const MAX_ASSET_LEN: u64 = 256 * 1024 * 1024;

/// The GitHub API answer is a release object with an `assets` array; a megabyte
/// is generous for it.
const MAX_API_LEN: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The release being fetched
// ---------------------------------------------------------------------------

/// Everything [`fetch_into`] needs to know about one pinned release, assembled
/// by the caller from its own pin file.
pub struct Release<'a> {
    /// Where the pin lives in the source tree, for messages that ask a human to
    /// look at it.
    pub pin_path: &'a str,
    /// The immutable release tag holding the assets.
    pub tag: &'a str,
    /// `https://github.com/<owner>/<repo>/releases/download`, already
    /// env-overridden by the caller.
    pub base_url: &'a str,
    /// What the provenance manifest records as the version, e.g.
    /// `"6.12.9 (guest-artifacts-6.12.9-1)"`.
    pub version: &'a str,
    pub assets: &'a [PinnedAsset],
    /// What to say when the release is not there.
    pub hint: Hint<'a>,
}

/// The names a 404 message needs, so the hint is written where the caller's
/// vocabulary is rather than guessed here.
pub struct Hint<'a> {
    /// The workflow that publishes this release.
    pub workflow: &'a str,
    /// Environment variable that relocates the download.
    pub base_url_env: &'a str,
    /// Environment variable that points at a directory already holding the
    /// files, which is the way in while nothing is published.
    pub dir_env: &'a str,
}

impl Release<'_> {
    /// The plain browser download URL for one asset. Correct for a public
    /// repository, and a 404 for a private one.
    pub fn url(&self, asset: &PinnedAsset) -> String {
        format!(
            "{}/{}/{}",
            self.base_url.trim_end_matches('/'),
            self.tag,
            asset.name
        )
    }
}

/// Rejects a pin whose digest is missing or malformed. Downloading an artifact
/// and hoping is the alternative, so this is a hard error rather than a
/// warning.
pub fn validate_digests(pin_path: &str, assets: &[PinnedAsset]) -> Result<(), String> {
    for asset in assets {
        if asset.sha256.len() != DigestAlgo::Sha256.hex_len()
            || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "{pin_path}: asset '{}' has no usable sha256 (got {:?}) — a pin without a \
                 digest would download an unverified artifact",
                asset.name, asset.sha256
            ));
        }
    }
    Ok(())
}

/// A tag or file name reduced to something safe to join onto a path. The pin is
/// ours, but it is still text that becomes a path, and `..` in it would escape
/// the cache.
pub fn sanitize(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("..", "__")
}

/// SHA-256 of a file on disk, streamed.
pub fn digest_of(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = DigestAlgo::Sha256.hasher();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finish_hex())
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// What one asset did during a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetStatus {
    /// Already in the cache and hashing to the pinned digest.
    Cached,
    Downloaded,
}

impl AssetStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            AssetStatus::Cached => "cached",
            AssetStatus::Downloaded => "downloaded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchedAsset {
    pub name: String,
    pub path: PathBuf,
    pub url: String,
    pub sha256: String,
    pub status: AssetStatus,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FetchOptions {
    /// Re-download even when the cached copy already matches the pin.
    pub refresh: bool,
    /// Never touch the network: use the cache or fail.
    pub offline: bool,
}

/// Downloads (or re-verifies) every asset of `release` into `dir`.
pub fn fetch_into(
    transport: &dyn Transport,
    release: &Release<'_>,
    dir: &Path,
    options: FetchOptions,
) -> Result<Vec<FetchedAsset>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // Resolved once per fetch, not once per asset: `gh auth token` is a process
    // spawn, and two assets must not cost two of them.
    let mut token = None;
    let mut fetched = Vec::new();
    for asset in release.assets {
        let expected = asset.sha256.to_ascii_lowercase();
        let path = dir.join(sanitize(&asset.name));
        let url = release.url(asset);

        let cached = !options.refresh
            && path.is_file()
            && digest_of(&path).is_ok_and(|found| found == expected);
        let status = if cached {
            AssetStatus::Cached
        } else {
            if options.offline {
                return Err(format!(
                    "--offline, and {} is not in the cache (or does not match the pinned \
                     digest): {}",
                    asset.name,
                    path.display()
                ));
            }
            if token.is_none() {
                token = Some(github_token());
            }
            let token = token.as_ref().and_then(Option::as_deref);
            download_verified(transport, release, asset, &path, &expected, token)?;
            AssetStatus::Downloaded
        };

        // The provenance note the whole project writes beside a verified
        // artifact. `signature_verified` is *false* and that is not a bug:
        // there is no signature over these, only a digest pinned in our source.
        // A manifest that claimed otherwise would be the lie this field exists
        // to prevent.
        //
        // Written on a download, and on a cache hit only when it is missing.
        // `fetched_at` means "when these bytes arrived", so rewriting it on
        // every cache hit would turn the one field that dates the artifact into
        // a record of the last time anything looked at it.
        let manifest_path = debian_media::manifest_path(&path);
        if status == AssetStatus::Downloaded || !manifest_path.is_file() {
            let manifest = Manifest {
                url: url.clone(),
                version: release.version.to_string(),
                fetched_at: debian_media::now_utc(),
                sha512_hex: expected.clone(),
                signature_verified: false,
                signed_by: None,
                keyring: Some(format!("pinned sha256 in {}", release.pin_path)),
            };
            let text = manifest
                .to_toml()
                .map_err(|e| format!("cannot render the provenance manifest: {e}"))?;
            std::fs::write(&manifest_path, text)
                .map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
        }

        fetched.push(FetchedAsset {
            name: asset.name.clone(),
            path,
            url,
            sha256: expected,
            status,
        });
    }

    Ok(fetched)
}

/// Streams one asset to `<path>.part`, hashing as it goes, and renames it into
/// place only once the digest matches the pin.
///
/// No resume: these are small enough that restarting is cheaper than the
/// bookkeeping, and unlike a 2.9 GiB ISO an interrupted 13 MiB download is not
/// worth keeping. A failed verification deletes the partial — the same rule
/// `debian-media` follows, for the same reason.
fn download_verified(
    transport: &dyn Transport,
    release: &Release<'_>,
    asset: &PinnedAsset,
    path: &Path,
    expected: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let partial = debian_media::partial_path(path);
    let _ = std::fs::remove_file(&partial);

    let url = release.url(asset);
    tracing::info!(
        url,
        bytes = asset.bytes,
        authenticated = token.is_some(),
        "downloading guest artifact"
    );
    let download = start_download(transport, release, asset, token).map_err(|e| {
        format!(
            "cannot download {url}: {e}{}",
            missing_hint(release, &e, token)
        )
    })?;

    let mut hasher = DigestAlgo::Sha256.hasher();
    let mut file = std::fs::File::create(&partial)
        .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;
    let mut reader = download.body.take(MAX_ASSET_LEN + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(format!("download of {url} failed: {e}"));
            }
        };
        written += read as u64;
        if written > MAX_ASSET_LEN {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "{url} is larger than the {MAX_ASSET_LEN} byte limit for a guest artifact"
            ));
        }
        hasher.update(&buf[..read]);
        if let Err(e) = file.write_all(&buf[..read]) {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("cannot write {}: {e}", partial.display()));
        }
    }
    if let Err(e) = file.flush() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("cannot write {}: {e}", partial.display()));
    }
    drop(file);

    let found = hasher.finish_hex();
    if found != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "{url} does not match the digest pinned in {}: expected sha256 {expected}, got \
             {found} ({written} bytes). The file has been deleted; nothing unverified is kept",
            release.pin_path
        ));
    }
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!(
            "cannot move the verified download into {}: {e}",
            path.display()
        )
    })
}

/// The browser URL when there is no token, the API asset endpoint when there
/// is. Both end up streaming the same bytes; only the private repository needs
/// the second.
fn start_download(
    transport: &dyn Transport,
    release: &Release<'_>,
    asset: &PinnedAsset,
    token: Option<&str>,
) -> Result<Download, TransportError> {
    let Some(token) = token else {
        return transport.get_range(&release.url(asset), 0);
    };
    let Some(api) = api_asset_url(transport, release, &asset.name, token)? else {
        // A token that cannot name the asset is no better than no token: fall
        // back rather than fail, so a stale `gh` login does not break a public
        // download that would have worked.
        return transport.get_range(&release.url(asset), 0);
    };
    transport.get_with_headers(
        &api,
        &[
            ("Authorization", &format!("Bearer {token}")),
            // Without this GitHub answers with the asset's JSON metadata, which
            // would hash to nothing the pin recognises.
            ("Accept", "application/octet-stream"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
    )
}

/// `GET /repos/<owner>/<repo>/releases/tags/<tag>`, then the id of the asset
/// with this name. `None` when the base URL is not a GitHub release URL (a
/// mirror set through the environment) or the release has no such asset.
fn api_asset_url(
    transport: &dyn Transport,
    release: &Release<'_>,
    name: &str,
    token: &str,
) -> Result<Option<String>, TransportError> {
    let Some(repo) = github_repo(release.base_url) else {
        return Ok(None);
    };
    let url = format!(
        "https://api.github.com/repos/{repo}/releases/tags/{}",
        release.tag
    );
    let body = transport.get_all_with_headers(
        &url,
        MAX_API_LEN,
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
    )?;
    let text = String::from_utf8_lossy(&body);
    Ok(asset_url_in(&text, name))
}

/// The `url` of the named asset in a GitHub release JSON document.
///
/// Split out and pure so the parse is unit-tested without a network: the field
/// wanted is `assets[].url` (the API endpoint), *not* `browser_download_url`,
/// which is the one that 404s on a private repository.
fn asset_url_in(json: &str, name: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ReleaseDoc {
        #[serde(default)]
        assets: Vec<AssetDoc>,
    }
    #[derive(Deserialize)]
    struct AssetDoc {
        name: String,
        url: String,
    }
    let doc: ReleaseDoc = serde_json::from_str(json).ok()?;
    doc.assets
        .into_iter()
        .find(|a| a.name == name)
        .map(|a| a.url)
}

/// `owner/repo` out of `https://github.com/owner/repo/releases/download`.
fn github_repo(base_url: &str) -> Option<String> {
    let rest = base_url
        .strip_prefix("https://github.com/")
        .or_else(|| base_url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// A GitHub token, if this host has one: `GITHUB_TOKEN`, `GH_TOKEN`, then
/// whatever `gh auth token` prints.
///
/// Only ever used as an `Authorization` header against `api.github.com`. It is
/// never logged, never written to a manifest and never passed to a mirror: the
/// authenticated path is taken only when the pinned base URL really is
/// `github.com` ([`github_repo`]).
pub fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(value) = std::env::var(key).ok().filter(|v| !v.trim().is_empty()) {
            tracing::debug!(
                source = key,
                "using a GitHub token for the artifact download"
            );
            return Some(value.trim().to_string());
        }
    }
    let mut command = std::process::Command::new("gh");
    command.args(["auth", "token"]);
    command.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // No console window when the manager (a GUI process) drives this.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    tracing::debug!(
        source = "gh auth token",
        "using a GitHub token for the artifact download"
    );
    Some(token)
}

/// A 404 here has a small number of likely causes and an exact fix for each, so
/// say them rather than leaving "HTTP status 404" to be interpreted.
///
/// The private-repository case is first because it is the one that is true
/// today: this repository is private, so an unauthenticated release download
/// 404s whether or not the release exists.
fn missing_hint(release: &Release<'_>, error: &TransportError, token: Option<&str>) -> String {
    let Hint {
        workflow,
        base_url_env,
        dir_env,
    } = release.hint;
    match error {
        TransportError::Status(404) if token.is_none() => format!(
            "\n  this repository is PRIVATE, so an unauthenticated release download always \
             answers 404 — whether or not the asset exists. Set GITHUB_TOKEN (or GH_TOKEN, or \
             sign in with `gh auth login`) and run this again; with a token the download goes \
             through the API asset endpoint, which private repositories do serve.\
             \n  Failing that: the release pinned in {} may simply not be published yet (see \
             {workflow}), {base_url_env} can point at a mirror, and {dir_env} can name a \
             directory that already holds the files",
            release.pin_path
        ),
        TransportError::Status(404) => format!(
            "\n  the release tag pinned in {} has no such asset, even with a token. Either \
             {workflow} has not published it yet, or the token cannot see this repository, or \
             {base_url_env} points somewhere that does not serve it. {dir_env} names a \
             directory that already holds the files",
            release.pin_path
        ),
        TransportError::Status(401 | 403) => format!(
            "\n  the GitHub token was rejected. Check it can read {} — `gh auth status`, or a \
             fine-grained token with Contents: read",
            github_repo(release.base_url).unwrap_or_else(|| release.base_url.to_string())
        ),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_without_a_usable_digest_is_refused() {
        let bad = |sha: &str| PinnedAsset {
            name: "x".into(),
            sha256: sha.into(),
            bytes: 1,
        };
        for sha in ["", &"z".repeat(64), &"ab".repeat(10)] {
            assert!(
                validate_digests("pin.toml", &[bad(sha)]).is_err(),
                "{sha:?}"
            );
        }
        assert!(validate_digests("pin.toml", &[bad(&"d4".repeat(32))]).is_ok());
    }

    /// `..` in a tag must not walk out of the cache directory. The pin is ours,
    /// but it is still text that becomes a path.
    #[test]
    fn path_components_from_a_pin_are_sanitised() {
        assert_eq!(
            sanitize("firmware-edk2-stable202602-1"),
            "firmware-edk2-stable202602-1"
        );
        for hostile in ["../../etc", "..\\..\\windows", "a/b", "c:evil"] {
            let safe = sanitize(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('\\') && !safe.contains(".."),
                "{hostile:?} sanitised to {safe:?}"
            );
        }
    }

    /// The private-repository route: the repo is parsed out of the pinned base
    /// URL, and only when that URL really is GitHub — a mirror named through
    /// the environment must never receive an `Authorization` header.
    #[test]
    fn only_a_github_base_url_yields_a_repository() {
        assert_eq!(
            github_repo("https://github.com/doctorspider42/entangled-destop/releases/download"),
            Some("doctorspider42/entangled-destop".to_string())
        );
        assert_eq!(
            github_repo("https://mirror.invalid/releases/download"),
            None
        );
        assert_eq!(github_repo("http://127.0.0.1:8080/download"), None);
        assert_eq!(github_repo("https://github.com/"), None);
        assert_eq!(github_repo("https://github.com/onlyowner"), None);
    }

    /// The asset endpoint, not `browser_download_url`: the latter is precisely
    /// the URL that 404s on a private repository, so picking the wrong field
    /// would silently reintroduce the bug this path exists to fix.
    #[test]
    fn the_api_asset_url_is_the_one_read_out_of_the_release_json() {
        let json = r#"{
            "tag_name": "firmware-1",
            "assets": [
              {"name": "OTHER.fd", "url": "https://api.github.com/repos/o/r/releases/assets/1",
               "browser_download_url": "https://github.com/o/r/releases/download/firmware-1/OTHER.fd"},
              {"name": "CLOUDHV.fd", "url": "https://api.github.com/repos/o/r/releases/assets/2",
               "browser_download_url": "https://github.com/o/r/releases/download/firmware-1/CLOUDHV.fd"}
            ]
        }"#;
        assert_eq!(
            asset_url_in(json, "CLOUDHV.fd").as_deref(),
            Some("https://api.github.com/repos/o/r/releases/assets/2")
        );
        assert_eq!(asset_url_in(json, "nothing.fd"), None);
        assert_eq!(asset_url_in("{}", "CLOUDHV.fd"), None);
        assert_eq!(asset_url_in("not json at all", "CLOUDHV.fd"), None);
    }

    /// The 404 a user actually hits today, on a private repository with no
    /// token: it must say *private* and name the token, because "404" alone
    /// reads as "this project never published it".
    #[test]
    fn an_unauthenticated_404_blames_the_private_repository() {
        let assets = [PinnedAsset {
            name: "CLOUDHV.fd".into(),
            sha256: "d4".repeat(32),
            bytes: 4,
        }];
        let release = Release {
            pin_path: "guest/firmware/pinned.toml",
            tag: "firmware-1",
            base_url: "https://github.com/o/r/releases/download",
            version: "1",
            assets: &assets,
            hint: Hint {
                workflow: ".github/workflows/firmware.yml",
                base_url_env: "ENTANGLED_FIRMWARE_BASE_URL",
                dir_env: "ENTANGLED_FIRMWARE_DIR",
            },
        };
        let hint = missing_hint(&release, &TransportError::Status(404), None);
        assert!(hint.contains("PRIVATE"), "{hint}");
        assert!(hint.contains("GITHUB_TOKEN"), "{hint}");
        assert!(hint.contains("guest/firmware/pinned.toml"), "{hint}");

        // With a token a 404 means something else, and says so.
        let hint = missing_hint(&release, &TransportError::Status(404), Some("t"));
        assert!(hint.contains("even with a token"), "{hint}");
        // A rejected token is neither of those.
        let hint = missing_hint(&release, &TransportError::Status(403), Some("t"));
        assert!(hint.contains("o/r"), "{hint}");
        // Everything else adds nothing rather than guessing.
        assert!(missing_hint(&release, &TransportError::Status(500), None).is_empty());
    }
}
