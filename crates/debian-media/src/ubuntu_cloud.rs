//! Authenticated Ubuntu ARM64 rootfs for native desktop provisioning.
//! The immutable build matches hardware acceptance; updates come from signed
//! Ubuntu APT repositories inside the installed guest.
use crate::{DigestAlgo, FetchOptions, FetchStatus, Manifest, MediaError, Transport};
use std::fs::File;
use std::path::{Path, PathBuf};

pub const UBUNTU_CLOUD_VERSION: &str = "noble-20260926-arm64";
pub const UBUNTU_CLOUD_DIRECTORY: &str = "https://cloud-images.ubuntu.com/noble/20260926";
pub const UBUNTU_CLOUD_ARCHIVE: &str = "noble-server-cloudimg-arm64.tar.gz";

#[derive(Debug)]
pub struct UbuntuCloudArtifact {
    pub path: PathBuf,
    pub manifest: Manifest,
    pub status: FetchStatus,
}

/// Verify cached metadata and data again, or fetch signature-first over HTTPS.
/// Does not require GPG or mutate the caller's keychain/keyring.
pub fn fetch_ubuntu_cloud_with(
    transport: &dyn Transport,
    directory: &Path,
    options: FetchOptions,
) -> Result<UbuntuCloudArtifact, MediaError> {
    std::fs::create_dir_all(directory).map_err(|e| MediaError::io(directory, e))?;
    let path = directory.join(UBUNTU_CLOUD_ARCHIVE);
    let result = fetch_inner(transport, directory, &path, options);
    if result.as_ref().is_err_and(|e| e.invalidates_cache()) {
        for failed in [
            path.clone(),
            crate::partial_path(&path),
            crate::manifest_path(&path),
            directory.join("SHA256SUMS"),
            directory.join("SHA256SUMS.gpg"),
        ] {
            let _ = std::fs::remove_file(failed);
        }
    }
    result
}

fn fetch_inner(
    transport: &dyn Transport,
    directory: &Path,
    path: &Path,
    options: FetchOptions,
) -> Result<UbuntuCloudArtifact, MediaError> {
    let sums_url = format!("{UBUNTU_CLOUD_DIRECTORY}/SHA256SUMS");
    let sums_path = directory.join("SHA256SUMS");
    let signature_path = directory.join("SHA256SUMS.gpg");
    let read = |path: &Path| {
        let file = File::open(path).map_err(|e| MediaError::io(path, e))?;
        let length = file.metadata().map_err(|e| MediaError::io(path, e))?.len();
        if length > crate::MAX_CONTROL_FILE_LEN {
            return Err(MediaError::TooLarge {
                url: sums_url.clone(),
                limit: crate::MAX_CONTROL_FILE_LEN,
            });
        }
        std::fs::read(path).map_err(|e| MediaError::io(path, e))
    };
    let (sums, signature) = if !options.refresh && sums_path.is_file() && signature_path.is_file() {
        (read(&sums_path)?, read(&signature_path)?)
    } else {
        if options.offline {
            return Err(MediaError::NothingCached {
                variant: "cloud-rootfs",
                arch: "arm64",
            });
        }
        let get = |url: &str| {
            transport
                .get_all(url, crate::MAX_CONTROL_FILE_LEN)
                .map_err(|source| MediaError::Transport {
                    url: url.into(),
                    source,
                })
        };
        (get(&sums_url)?, get(&format!("{sums_url}.gpg"))?)
    };
    let signer = crate::verify_detached(&crate::UBUNTU_CLOUD, &sums, &signature, &sums_url)?;
    let text = std::str::from_utf8(&sums).map_err(|_| MediaError::NotUtf8 {
        url: sums_url.clone(),
    })?;
    let entries =
        crate::parse_sums_with(text, DigestAlgo::Sha256).map_err(|source| MediaError::Sums {
            url: sums_url.clone(),
            source,
        })?;
    let expected = entries
        .iter()
        .find(|entry| entry.file_name == UBUNTU_CLOUD_ARCHIVE)
        .ok_or_else(|| MediaError::NotListed {
            file: UBUNTU_CLOUD_ARCHIVE.into(),
            sums_url,
        })?
        .sha512_hex
        .clone();
    for (destination, bytes) in [(&sums_path, &sums), (&signature_path, &signature)] {
        std::fs::write(destination, bytes).map_err(|e| MediaError::io(destination, e))?;
    }
    let url = format!("{UBUNTU_CLOUD_DIRECTORY}/{UBUNTU_CLOUD_ARCHIVE}");
    let cached_digest = if path.is_file() {
        let mut file = File::open(path).map_err(|e| MediaError::io(path, e))?;
        Some(crate::fetch::hash_reader(
            &mut file,
            DigestAlgo::Sha256,
            path,
        )?)
    } else {
        None
    };
    let cached = cached_digest
        .as_ref()
        .is_some_and(|digest| digest == &expected);
    if let Some(actual) = cached_digest.filter(|digest| digest != &expected) {
        crate::purge(path);
        if options.offline {
            return Err(MediaError::DigestMismatch {
                url,
                algo: "sha256",
                expected,
                actual,
            });
        }
    }
    let status = if cached && !options.refresh {
        FetchStatus::Cached
    } else {
        if options.offline {
            return Err(MediaError::NothingCached {
                variant: "cloud-rootfs",
                arch: "arm64",
            });
        }
        crate::fetch::download_verified(transport, &url, path, &expected, DigestAlgo::Sha256)?
    };
    let manifest_path = crate::manifest_path(path);
    let fetched_at = if status == FetchStatus::Cached {
        std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|text| Manifest::from_toml(&text).ok())
            .filter(|old| old.url == url && old.sha512_hex == expected)
            .map(|old| old.fetched_at)
            .unwrap_or_else(crate::now_utc)
    } else {
        crate::now_utc()
    };
    let manifest = Manifest {
        url,
        version: UBUNTU_CLOUD_VERSION.into(),
        fetched_at,
        sha512_hex: expected,
        signature_verified: true,
        signed_by: Some(signer.signing_fingerprint),
        keyring: Some(crate::UBUNTU_CLOUD.name.into()),
    };
    let text = manifest.to_toml().map_err(MediaError::ManifestWrite)?;
    std::fs::write(&manifest_path, text).map_err(|e| MediaError::io(&manifest_path, e))?;
    Ok(UbuntuCloudArtifact {
        path: path.into(),
        manifest,
        status,
    })
}
