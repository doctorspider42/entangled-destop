//! The `entangled fetch` flow (MVP-602/603/604/606/607/608/609/610).
//!
//! # Order of operations is the security property
//!
//! ```text
//! 1. download the signed root (SHA512SUMS + .sign, or Release + Release.gpg)
//! 2. verify the OpenPGP signature against a PINNED keyring        <-- first
//! 3. (archive path only) verify the SHA256SUMS index against the signed Release
//! 4. resolve names/version from the now-trusted digest list
//! 5. stream the artifact to <file>.part, hashing as it goes
//! 6. compare the digest against the trusted entry
//! 7. rename into place and write the provenance manifest          <-- "ready"
//! 8. on any failure: purge the partial and the artifact
//! ```
//!
//! Step 2 before step 6 is not a style preference: a matching digest taken from
//! an unverified checksum file proves nothing. Every early return between 5 and
//! 7 purges, so a failed run can never leave something that looks verified.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::cache::{self, MediaCache};
use crate::digest::{digests_match, DigestAlgo};
use crate::error::MediaError;
use crate::http::{Transport, MAX_CONTROL_FILE_LEN};
use crate::keyring::{self, VerifiedBy};
use crate::manifest::Manifest;
use crate::release::Release;
use crate::rfc3339;
use crate::source::{Arch, InstallerVariant, MediaKind, MediaSource, TrustRoot};
use crate::sums::{self, SumsEntry};

/// Refuse to stream more than this into the cache for a single artifact. The
/// netinst ISO is ~700 MB and a netboot initrd is ~50 MB; the cap only exists so
/// a hostile or broken mirror cannot fill the disk before the digest check runs.
pub const MAX_ARTIFACT_LEN: u64 = 8 * 1024 * 1024 * 1024;

/// Chunk size for the streaming copy-and-hash loop.
const COPY_CHUNK: usize = 256 * 1024;

/// What happened to one artifact during a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStatus {
    /// A verified copy was already in the cache; no bytes were transferred.
    Cached,
    /// Downloaded from scratch.
    Downloaded,
    /// An interrupted download was continued with an HTTP Range request.
    Resumed,
}

impl FetchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FetchStatus::Cached => "cached",
            FetchStatus::Downloaded => "downloaded",
            FetchStatus::Resumed => "resumed",
        }
    }
}

/// One verified artifact in the cache.
#[derive(Debug, Clone)]
pub struct FetchedArtifact {
    pub kind: MediaKind,
    pub path: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub status: FetchStatus,
}

/// How the artifacts in a [`FetchReport`] came to be trusted.
#[derive(Debug, Clone)]
pub enum Provenance {
    /// Checked against a signed trust root downloaded during this run.
    Verified {
        /// URL of the digest list the artifacts were checked against.
        sums_url: String,
        /// Which pinned key signed the trust root.
        signed_by: VerifiedBy,
        /// Name of the keyring that key came from.
        keyring: &'static str,
    },
    /// Revalidated entirely from the provenance manifests of an earlier verified
    /// fetch: every file still hashes to the digest that fetch recorded. No
    /// network access at all (MVP-609).
    Cache,
}

impl Provenance {
    pub fn is_verified_online(&self) -> bool {
        matches!(self, Provenance::Verified { .. })
    }
}

/// Result of `entangled fetch debian --variant …`.
#[derive(Debug, Clone)]
pub struct FetchReport {
    /// Version discovered from signed metadata — never pinned in code.
    pub version: String,
    pub arch: Arch,
    pub variant: InstallerVariant,
    pub provenance: Provenance,
    pub artifacts: Vec<FetchedArtifact>,
}

/// Whether `Fetcher::fetch_with` may answer from the cache alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FetchOptions {
    /// Skip the local cache and always revalidate against a freshly downloaded,
    /// signed trust root. This is how a new point release is picked up.
    pub refresh: bool,
    /// Never touch the network: succeed only if the cache already holds a
    /// complete, manifest-verified set.
    pub offline: bool,
}

/// The trusted digest list for one variant, after signature verification.
struct SignedIndex {
    /// URL of the file whose entries `entries` came from.
    sums_url: String,
    /// URL the entries' paths are relative to.
    base_url: String,
    algo: DigestAlgo,
    entries: Vec<SumsEntry>,
    /// Version taken from the signed root, when it states one.
    version: Option<String>,
    signed_by: VerifiedBy,
    keyring: &'static str,
}

/// Downloads and verifies Debian installer media into a cache.
pub struct Fetcher<S, T> {
    source: S,
    transport: T,
    cache: MediaCache,
    arch: Arch,
}

impl<S: MediaSource, T: Transport> Fetcher<S, T> {
    pub fn new(source: S, transport: T, cache: MediaCache, arch: Arch) -> Self {
        Self {
            source,
            transport,
            cache,
            arch,
        }
    }

    pub fn cache(&self) -> &MediaCache {
        &self.cache
    }

    /// The transport this fetcher uses. Tests assert on the fixture's request
    /// log through this — "which URLs were fetched, in which order" is part of
    /// the trust-chain contract, not an implementation detail.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// Fetches every artifact belonging to `variant`, verifying each one.
    ///
    /// A second run over an intact cache performs no network access at all: see
    /// [`Self::fetch_with`] for the exact policy.
    pub fn fetch(&self, variant: InstallerVariant) -> Result<FetchReport, MediaError> {
        self.fetch_with(variant, FetchOptions::default())
    }

    /// [`Self::fetch`] with explicit cache policy.
    ///
    /// * default — answer from the cache when it holds a complete
    ///   manifest-verified set (zero network), otherwise go online;
    /// * `refresh` — always revalidate against a freshly signed trust root;
    /// * `offline` — never go online, fail when the cache cannot answer.
    pub fn fetch_with(
        &self,
        variant: InstallerVariant,
        options: FetchOptions,
    ) -> Result<FetchReport, MediaError> {
        if !options.refresh {
            if let Some(report) = self.fetch_cached(variant)? {
                tracing::info!(
                    version = %report.version,
                    "complete verified set already in the cache; no network access"
                );
                return Ok(report);
            }
        }
        if options.offline {
            return Err(MediaError::NothingCached {
                variant: variant.as_str(),
                arch: self.arch.as_str(),
            });
        }
        self.fetch_online(variant)
    }

    /// The online path: download the signed trust root, then the artifacts.
    fn fetch_online(&self, variant: InstallerVariant) -> Result<FetchReport, MediaError> {
        let index = self.resolve_index(variant)?;

        // The netinst ISO's name carries the version; netboot takes it from the
        // signed `Release` file. Either way it is discovered, never pinned.
        let discovered_iso = if self.source.sums_path(variant, MediaKind::Iso).is_none()
            && self.source.artifacts(variant).contains(&MediaKind::Iso)
        {
            Some(
                sums::find_plain_netinst_iso(&index.entries, self.arch.as_str())
                    .ok_or_else(|| MediaError::NoNetinstIso {
                        arch: self.arch.as_str().to_string(),
                        sums_url: index.sums_url.clone(),
                    })?
                    .file_name
                    .clone(),
            )
        } else {
            None
        };

        let version = match (&index.version, &discovered_iso) {
            (_, Some(iso)) => sums::version_from_iso_name(iso)
                .map(str::to_owned)
                .ok_or_else(|| MediaError::UnknownVersion {
                    source_desc: iso.clone(),
                })?,
            (Some(v), None) => v.clone(),
            (None, None) => {
                return Err(MediaError::UnknownVersion {
                    source_desc: index.sums_url.clone(),
                })
            }
        };

        let span = tracing::info_span!(
            "fetch",
            distro = "debian",
            variant = variant.as_str(),
            arch = self.arch.as_str(),
            version = %version
        );
        let _guard = span.enter();

        let mut artifacts = Vec::new();
        for &kind in self.source.artifacts(variant) {
            let sums_path = match self.source.sums_path(variant, kind) {
                Some(p) => p,
                None => discovered_iso
                    .clone()
                    .ok_or_else(|| MediaError::NoNetinstIso {
                        arch: self.arch.as_str().to_string(),
                        sums_url: index.sums_url.clone(),
                    })?,
            };
            artifacts.push(self.fetch_one(variant, kind, &sums_path, &version, &index)?);
        }

        Ok(FetchReport {
            version,
            arch: self.arch,
            variant,
            provenance: Provenance::Verified {
                sums_url: index.sums_url,
                signed_by: index.signed_by,
                keyring: index.keyring,
            },
            artifacts,
        })
    }

    /// The zero-network path (MVP-609): does the cache already hold a complete,
    /// manifest-verified set for `variant`?
    ///
    /// Every candidate version directory is checked; the highest version that
    /// fully validates wins. "Validates" means each artifact has a manifest that
    /// claims signature verification, names the same version, and records a
    /// digest the file on disk still hashes to.
    pub fn fetch_cached(
        &self,
        variant: InstallerVariant,
    ) -> Result<Option<FetchReport>, MediaError> {
        let entries = match std::fs::read_dir(self.cache.root()) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MediaError::io(self.cache.root(), e)),
        };

        let mut best: Option<(String, Vec<FetchedArtifact>)> = None;
        for entry in entries.flatten() {
            let version = entry.file_name().to_string_lossy().into_owned();
            let dir = self.cache.variant_dir(&version, self.arch, variant);
            if !dir.is_dir() {
                continue;
            }
            let Some(artifacts) = self.cached_set(variant, &version, &dir)? else {
                continue;
            };
            let better = match &best {
                None => true,
                Some((current, _)) => {
                    sums::compare_versions(&version, current) == std::cmp::Ordering::Greater
                }
            };
            if better {
                best = Some((version, artifacts));
            }
        }

        Ok(best.map(|(version, artifacts)| FetchReport {
            version,
            arch: self.arch,
            variant,
            provenance: Provenance::Cache,
            artifacts,
        }))
    }

    /// Validates one cached version directory. `Ok(None)` means "incomplete or
    /// not verifiable", which is never an error — it just means go online.
    fn cached_set(
        &self,
        variant: InstallerVariant,
        version: &str,
        dir: &Path,
    ) -> Result<Option<Vec<FetchedArtifact>>, MediaError> {
        let kinds = self.source.artifacts(variant);
        let mut artifacts = Vec::with_capacity(kinds.len());
        for &kind in kinds {
            // The ISO's file name is not known without the signed sums, so it is
            // recovered from the cache directory instead.
            let path = match self.source.sums_path(variant, kind) {
                Some(sums_path) => {
                    let name = sums_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&sums_path)
                        .to_string();
                    dir.join(name)
                }
                None => match self.cached_iso(dir)? {
                    Some(p) => p,
                    None => return Ok(None),
                },
            };
            let Some(manifest) = cache::load_manifest(&path) else {
                return Ok(None);
            };
            if !manifest.signature_verified || manifest.version != version {
                return Ok(None);
            }
            let Some(algo) = manifest.algo() else {
                return Ok(None);
            };
            let mut file = match File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(MediaError::io(&path, e)),
            };
            let actual = hash_reader(&mut file, algo, &path)?;
            if !digests_match(&actual, &manifest.sha512_hex) {
                tracing::warn!(path = %path.display(), "cached artifact no longer matches its manifest");
                return Ok(None);
            }
            artifacts.push(FetchedArtifact {
                kind,
                manifest_path: cache::manifest_path(&path),
                manifest,
                path,
                status: FetchStatus::Cached,
            });
        }
        Ok(Some(artifacts))
    }

    /// Finds the single cached netinst ISO in `dir`, if there is exactly one.
    fn cached_iso(&self, dir: &Path) -> Result<Option<PathBuf>, MediaError> {
        let mut found = None;
        for entry in std::fs::read_dir(dir)
            .map_err(|e| MediaError::io(dir, e))?
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let candidate = SumsEntry {
                sha512_hex: String::new(),
                file_name: name,
            };
            if candidate.is_plain_netinst_iso(self.arch.as_str()) {
                if found.is_some() {
                    return Ok(None); // ambiguous; let the online path decide
                }
                found = Some(entry.path());
            }
        }
        Ok(found)
    }

    /// Steps 1–4: fetch the signed root and turn it into a trusted digest list.
    fn resolve_index(&self, variant: InstallerVariant) -> Result<SignedIndex, MediaError> {
        let root = self.source.trust_root(variant);
        let keyring = root.keyring();
        match &root {
            TrustRoot::DetachedSums {
                dir_url,
                sums_file,
                signature_file,
                algo,
                keyring: _,
            } => {
                let sums_url = format!("{dir_url}/{sums_file}");
                let sig_url = format!("{dir_url}/{signature_file}");
                let sums = self.get_control(&sums_url)?;
                let signature = self.get_control(&sig_url)?;

                // FIRST: the signature. Nothing below this line is trusted
                // before it returns Ok.
                let signed_by = keyring::verify_detached(keyring, &sums, &signature, &sums_url)?;
                tracing::info!(url = %sums_url, key = %signed_by.signing_fingerprint, "checksum file signature verified");

                let entries = self.parse_entries(&sums, *algo, &sums_url)?;
                Ok(SignedIndex {
                    sums_url,
                    base_url: dir_url.clone(),
                    algo: *algo,
                    entries,
                    version: None,
                    signed_by,
                    keyring: keyring.name,
                })
            }
            TrustRoot::ArchiveRelease {
                dists_url,
                index_path,
                index_base_url,
                algo,
                keyring: _,
            } => {
                let release_url = format!("{dists_url}/Release");
                let sig_url = format!("{dists_url}/Release.gpg");
                let release_bytes = self.get_control(&release_url)?;
                let signature = self.get_control(&sig_url)?;

                // FIRST: the signature over the archive root.
                let signed_by =
                    keyring::verify_detached(keyring, &release_bytes, &signature, &release_url)?;
                tracing::info!(url = %release_url, key = %signed_by.signing_fingerprint, "archive Release signature verified");

                let release_text =
                    std::str::from_utf8(&release_bytes).map_err(|_| MediaError::NotUtf8 {
                        url: release_url.clone(),
                    })?;
                let release =
                    Release::parse(release_text).map_err(|source| MediaError::Release {
                        url: release_url.clone(),
                        source,
                    })?;
                let (want_digest, want_size) =
                    release
                        .index_digest(index_path)
                        .map_err(|source| MediaError::Release {
                            url: release_url.clone(),
                            source,
                        })?;
                let want_digest = want_digest.to_string();

                // Second hop: the digest index itself is covered by the signed
                // Release, so it is verified before any of its entries are used.
                let sums_url = format!("{dists_url}/{index_path}");
                let sums = self.get_control(&sums_url)?;
                if sums.len() as u64 != want_size {
                    return Err(MediaError::DigestMismatch {
                        url: sums_url,
                        algo: "size",
                        expected: want_size.to_string(),
                        actual: sums.len().to_string(),
                    });
                }
                let actual = crate::release::RELEASE_INDEX_ALGO.hex_of(&sums);
                if !digests_match(&actual, &want_digest) {
                    return Err(MediaError::DigestMismatch {
                        url: sums_url,
                        algo: crate::release::RELEASE_INDEX_ALGO.as_str(),
                        expected: want_digest,
                        actual,
                    });
                }
                tracing::info!(url = %sums_url, "checksum index matches the signed Release");

                let entries = self.parse_entries(&sums, *algo, &sums_url)?;
                Ok(SignedIndex {
                    sums_url,
                    base_url: index_base_url.clone(),
                    algo: *algo,
                    entries,
                    version: release.version.clone(),
                    signed_by,
                    keyring: keyring.name,
                })
            }
        }
    }

    fn get_control(&self, url: &str) -> Result<Vec<u8>, MediaError> {
        self.transport
            .get_all(url, MAX_CONTROL_FILE_LEN)
            .map_err(|source| MediaError::Transport {
                url: url.to_string(),
                source,
            })
    }

    fn parse_entries(
        &self,
        bytes: &[u8],
        algo: DigestAlgo,
        url: &str,
    ) -> Result<Vec<SumsEntry>, MediaError> {
        let text = std::str::from_utf8(bytes).map_err(|_| MediaError::NotUtf8 {
            url: url.to_string(),
        })?;
        sums::parse_sums_with(text, algo).map_err(|source| MediaError::Sums {
            url: url.to_string(),
            source,
        })
    }

    /// Steps 5–8 for a single artifact.
    fn fetch_one(
        &self,
        variant: InstallerVariant,
        kind: MediaKind,
        sums_path: &str,
        version: &str,
        index: &SignedIndex,
    ) -> Result<FetchedArtifact, MediaError> {
        let entry =
            sums::find_entry(&index.entries, sums_path).ok_or_else(|| MediaError::NotListed {
                file: sums_path.to_string(),
                sums_url: index.sums_url.clone(),
            })?;
        let expected = entry.sha512_hex.clone();

        let file_name = sums_path.rsplit('/').next().unwrap_or(sums_path);
        let url = format!("{}/{}", index.base_url, sums_path);
        let path = self
            .cache
            .artifact_path(version, self.arch, variant, file_name);
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| MediaError::io(dir, e))?;

        // MVP-609: a verified cache hit costs zero network access.
        if let Some(manifest) = self.cached_and_verified(&path, &url, &expected, index.algo)? {
            tracing::info!(path = %path.display(), "using verified cache entry");
            return Ok(FetchedArtifact {
                kind,
                manifest_path: cache::manifest_path(&path),
                manifest,
                path,
                status: FetchStatus::Cached,
            });
        }

        // Any manifest still lying around describes data we are about to
        // replace; drop it now so an interrupted run cannot look verified.
        let _ = std::fs::remove_file(cache::manifest_path(&path));

        let outcome = self.download_verified(&url, &path, &expected, index.algo);
        let status = match outcome {
            Ok(status) => status,
            Err(e) => {
                // Acceptance criterion: a bad digest or signature removes the
                // partial artifact from the active cache. An *interrupted*
                // transfer is different — its partial is exactly what makes the
                // next run resumable, so it is kept.
                if e.invalidates_cache() {
                    cache::purge(&path);
                }
                return Err(e);
            }
        };

        let manifest = Manifest {
            url,
            version: version.to_string(),
            fetched_at: rfc3339::now_utc(),
            sha512_hex: expected,
            signature_verified: true,
            signed_by: Some(index.signed_by.signing_fingerprint.clone()),
            keyring: Some(index.keyring.to_string()),
        };
        let manifest_path = match cache::store_manifest(&path, &manifest) {
            Ok(p) => p,
            Err(e) => {
                cache::purge(&path);
                return Err(e);
            }
        };
        tracing::info!(path = %path.display(), status = status.as_str(), "artifact verified");

        Ok(FetchedArtifact {
            kind,
            path,
            manifest_path,
            manifest,
            status,
        })
    }

    /// Returns the manifest when `path` holds a byte-for-byte verified copy that
    /// the manifest actually describes. Purely local work — the whole point of
    /// MVP-609 is that this path performs no network access.
    ///
    /// Three things must line up: the manifest says the signature was verified,
    /// it records the same URL we are about to fetch (so a text-netboot initrd
    /// can never satisfy a gtk-netboot request), and the file on disk still
    /// hashes to the digest from the freshly signed checksum list.
    fn cached_and_verified(
        &self,
        path: &Path,
        url: &str,
        expected: &str,
        algo: DigestAlgo,
    ) -> Result<Option<Manifest>, MediaError> {
        let Some(manifest) = cache::load_manifest(path) else {
            return Ok(None);
        };
        if !manifest.signature_verified
            || manifest.url != url
            || !digests_match(&manifest.sha512_hex, expected)
        {
            tracing::debug!(path = %path.display(), "cached manifest does not describe the wanted artifact");
            return Ok(None);
        }
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MediaError::io(path, e)),
        };
        let actual = hash_reader(&mut file, algo, path)?;
        if digests_match(&actual, expected) {
            Ok(Some(manifest))
        } else {
            tracing::warn!(path = %path.display(), "cached artifact no longer matches its manifest");
            Ok(None)
        }
    }

    /// Streams `url` into `<path>.part`, resuming when a partial exists, then
    /// renames it to `path` once the digest matches `expected`.
    fn download_verified(
        &self,
        url: &str,
        path: &Path,
        expected: &str,
        algo: DigestAlgo,
    ) -> Result<FetchStatus, MediaError> {
        let partial = cache::partial_path(path);
        let existing = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);

        let mut download = match self.begin(url, existing) {
            Ok(d) => d,
            // 416 means the partial is at least as long as the resource; start over.
            Err(MediaError::Transport {
                source: crate::http::TransportError::Status(416),
                ..
            }) if existing > 0 => {
                tracing::debug!(url, "server rejected the range; restarting from scratch");
                self.begin(url, 0)?
            }
            Err(e) => return Err(e),
        };

        // Guard against a partial longer than the whole resource.
        if download.resumed {
            if let Some(total) = download.total_len {
                if existing > total {
                    tracing::debug!(
                        url,
                        existing,
                        total,
                        "partial is longer than the resource; restarting"
                    );
                    download = self.begin(url, 0)?;
                }
            }
        }

        let resumed = download.resumed && existing > 0;
        let mut hasher = algo.hasher();
        let mut file = if resumed {
            // SHA-2 state cannot be persisted between runs, so the bytes we
            // already have on disk are re-read and re-hashed here.
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&partial)
                .map_err(|e| MediaError::io(&partial, e))?;
            hash_into(&mut f, &mut hasher, &partial)?;
            f.seek(SeekFrom::Start(existing))
                .map_err(|e| MediaError::io(&partial, e))?;
            tracing::info!(url, offset = existing, "resuming download");
            f
        } else {
            File::create(&partial).map_err(|e| MediaError::io(&partial, e))?
        };

        let mut written = if resumed { existing } else { 0 };
        {
            let mut writer = BufWriter::with_capacity(COPY_CHUNK, &mut file);
            let mut buf = vec![0u8; COPY_CHUNK];
            loop {
                let n = download
                    .body
                    .read(&mut buf)
                    .map_err(|e| MediaError::Transport {
                        url: url.to_string(),
                        source: e.into(),
                    })?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > MAX_ARTIFACT_LEN {
                    return Err(MediaError::TooLarge {
                        url: url.to_string(),
                        limit: MAX_ARTIFACT_LEN,
                    });
                }
                hasher.update(&buf[..n]);
                writer
                    .write_all(&buf[..n])
                    .map_err(|e| MediaError::io(&partial, e))?;
            }
            writer.flush().map_err(|e| MediaError::io(&partial, e))?;
        }
        file.sync_all().map_err(|e| MediaError::io(&partial, e))?;
        drop(file);

        // A transfer that stopped short is not a corrupt artifact: keep the
        // partial so the next run resumes it instead of purging and restarting.
        if let Some(total) = download.total_len {
            if written < total {
                return Err(MediaError::Interrupted {
                    url: url.to_string(),
                    received: written,
                    expected: total,
                });
            }
        }

        let actual = hasher.finish_hex();
        if !digests_match(&actual, expected) {
            return Err(MediaError::DigestMismatch {
                url: url.to_string(),
                algo: algo.as_str(),
                expected: expected.to_string(),
                actual,
            });
        }

        std::fs::rename(&partial, path).map_err(|e| MediaError::io(path, e))?;
        Ok(if resumed {
            FetchStatus::Resumed
        } else {
            FetchStatus::Downloaded
        })
    }

    fn begin(&self, url: &str, offset: u64) -> Result<crate::http::Download, MediaError> {
        self.transport
            .get_range(url, offset)
            .map_err(|source| MediaError::Transport {
                url: url.to_string(),
                source,
            })
    }
}

fn hash_reader(
    reader: &mut impl Read,
    algo: DigestAlgo,
    path: &Path,
) -> Result<String, MediaError> {
    let mut hasher = algo.hasher();
    hash_into(reader, &mut hasher, path)?;
    Ok(hasher.finish_hex())
}

fn hash_into(
    reader: &mut impl Read,
    hasher: &mut crate::digest::Hasher,
    path: &Path,
) -> Result<(), MediaError> {
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        let n = reader.read(&mut buf).map_err(|e| MediaError::io(path, e))?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
    }
}
