//! Sparse-preserving relocation of a RAW disk image (and its `.nvram`
//! sidecar) to another directory or drive.
//!
//! Why not `std::fs::copy` / `rename`: a rename cannot cross filesystems, and
//! a naive copy reads a 24 GiB-apparent / 7 GiB-allocated image back to
//! 24 GiB of real clusters at the destination. This module copies only the
//! **allocated ranges** (SEEK_DATA/SEEK_HOLE on Linux,
//! `FSCTL_QUERY_ALLOCATED_RANGES` on NTFS, a zero-skipping full scan
//! elsewhere) into a destination pre-marked sparse, so holes stay holes.
//!
//! The safety order is fixed and worth stating: **copy → fsync → re-read and
//! verify → rewrite profiles → delete source**. A failure at any step removes
//! the destination copy and restores any profile already rewritten — the one
//! thing this module must never do is leave a profile pointing at a
//! half-copied file.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use control_api::VmConfig;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ops;
use crate::refs;

/// Copy chunk: 1 MiB balances syscall count against progress granularity.
const CHUNK: usize = 1 << 20;

/// Progress callback: `(data_bytes_copied, data_bytes_total)`. Total is the
/// allocated data, not the apparent size — holes are never read or written.
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

#[derive(Debug, Error)]
pub enum MoveError {
    #[error("{0} does not exist or is not a regular file")]
    Missing(String),

    #[error("destination {0} already exists — refusing to overwrite")]
    DestinationExists(String),

    #[error("source and destination are the same file: {0}")]
    SameFile(String),

    #[error(
        "not enough space at {dest}: {free} bytes free, but the image carries \
         {needed} bytes of data"
    )]
    NoSpace {
        dest: String,
        free: u64,
        needed: u64,
    },

    #[error(
        "verification failed for {0}: the destination copy does not match the source. \
         The copy was removed; the source and every profile are untouched"
    )]
    Verify(String),

    #[error(
        "cannot update profile {path}: {message}. The destination copy was removed and \
         already-rewritten profiles were restored; nothing changed"
    )]
    Profile { path: PathBuf, message: String },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What a successful move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    /// `(from, to)` for the image and, when present, the `.nvram` sidecar.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Profiles whose disk/cdrom/nvram entries now point at the new location.
    pub updated_profiles: Vec<PathBuf>,
    /// The image's nominal size.
    pub apparent_bytes: u64,
    /// Bytes actually copied (the allocated data).
    pub data_bytes: u64,
    /// Source files that survived a fully verified move because deleting them
    /// failed (locked, permissions). Safe to delete by hand.
    pub leftover_sources: Vec<PathBuf>,
}

/// Moves `disk` (and its `.nvram` sidecar) into `dest_dir`, sparse-preserving
/// and verified, then rewrites every profile in `profiles` that references
/// either file. See the module docs for the ordering and rollback rules.
pub fn move_disk(
    disk: &Path,
    dest_dir: &Path,
    profiles: &[PathBuf],
    progress: Progress,
) -> Result<MoveOutcome, MoveError> {
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source: io::Error| MoveError::Io { path, source }
    };

    if !disk.is_file() {
        return Err(MoveError::Missing(disk.display().to_string()));
    }
    let src = std::fs::canonicalize(disk).map_err(io_err(disk))?;
    std::fs::create_dir_all(dest_dir).map_err(io_err(dest_dir))?;

    let file_name = src
        .file_name()
        .ok_or_else(|| MoveError::Missing(disk.display().to_string()))?;
    let dst = dest_dir.join(file_name);
    if dst.exists() {
        if std::fs::canonicalize(&dst).is_ok_and(|c| c == src) {
            return Err(MoveError::SameFile(src.display().to_string()));
        }
        return Err(MoveError::DestinationExists(dst.display().to_string()));
    }

    // Keep reporting in the user's own spelling of the path; `src` (canonical,
    // `\\?\`-prefixed on Windows) exists for identity checks and mappings.
    let nvram_src = ops::existing_nvram_sidecar(disk);
    let nvram_dst = nvram_src
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|name| dest_dir.join(name));
    if let Some(nvram_dst) = &nvram_dst {
        if nvram_dst.exists() {
            return Err(MoveError::DestinationExists(
                nvram_dst.display().to_string(),
            ));
        }
    }

    // Space: what the copy will actually write is the allocated data (plus the
    // tiny sidecar). The *apparent* size is the worst case the guest can grow
    // into later — the caller warns about that, this only refuses what cannot
    // physically fit now.
    let needed = ops::allocated_bytes(&src)
        .unwrap_or_else(|| src.metadata().map(|m| m.len()).unwrap_or(0))
        .saturating_add(
            nvram_src
                .as_ref()
                .and_then(|p| p.metadata().ok())
                .map(|m| m.len())
                .unwrap_or(0),
        );
    if let Some((free, _total)) = ops::disk_space(dest_dir) {
        if free < needed {
            return Err(MoveError::NoSpace {
                dest: dest_dir.display().to_string(),
                free,
                needed,
            });
        }
    }

    // A closure that undoes the destination files; used by every failure path
    // from here on.
    let cleanup_dst = |nvram_dst: &Option<PathBuf>| {
        let _ = std::fs::remove_file(&dst);
        if let Some(nvram_dst) = nvram_dst {
            let _ = std::fs::remove_file(nvram_dst);
        }
    };

    // 1. Copy the image, holes preserved.
    let (stats, ranges, src_digest) = match sparse_copy(disk, &dst, progress) {
        Ok(result) => result,
        Err(source) => {
            cleanup_dst(&None);
            return Err(MoveError::Io {
                path: dst.clone(),
                source,
            });
        }
    };

    // 2. Verify: re-read the destination's data ranges and compare digests
    //    (holes read back as zeros on both sides, so ranges + digest pin the
    //    whole logical content given equal lengths).
    let verified = (|| -> io::Result<bool> {
        let dst_len = std::fs::metadata(&dst)?.len();
        if dst_len != stats.apparent_bytes {
            return Ok(false);
        }
        Ok(hash_ranges(&dst, &ranges)? == src_digest)
    })();
    match verified {
        Ok(true) => {}
        Ok(false) => {
            cleanup_dst(&None);
            return Err(MoveError::Verify(dst.display().to_string()));
        }
        Err(source) => {
            cleanup_dst(&None);
            return Err(MoveError::Io {
                path: dst.clone(),
                source,
            });
        }
    }

    // 3. The sidecar (small, plain copy + byte-for-byte verify).
    if let (Some(nvram_from), Some(nvram_to)) = (&nvram_src, &nvram_dst) {
        let copied = std::fs::copy(nvram_from, nvram_to)
            .and_then(|_| Ok(std::fs::read(nvram_from)? == std::fs::read(nvram_to)?));
        match copied {
            Ok(true) => {}
            Ok(false) => {
                cleanup_dst(&nvram_dst);
                return Err(MoveError::Verify(nvram_to.display().to_string()));
            }
            Err(source) => {
                cleanup_dst(&nvram_dst);
                return Err(MoveError::Io {
                    path: nvram_to.clone(),
                    source,
                });
            }
        }
    }

    // 4. Rewrite profiles — while the source still exists, so relative and
    //    absolute references can still be resolved against it.
    let mut mappings: Vec<(PathBuf, PathBuf)> = vec![(src.clone(), dst.clone())];
    if let (Some(from), Some(to)) = (&nvram_src, &nvram_dst) {
        if let Ok(canonical) = std::fs::canonicalize(from) {
            mappings.push((canonical, to.clone()));
        }
    }
    let mut rewritten: Vec<(PathBuf, String)> = Vec::new();
    let mut updated_profiles = Vec::new();
    for profile in profiles {
        match rewrite_profile(profile, &mappings) {
            Ok(Some(original)) => {
                rewritten.push((profile.clone(), original));
                updated_profiles.push(profile.clone());
            }
            Ok(None) => {}
            Err(message) => {
                // Restore everything already rewritten, then drop the copies.
                for (path, original) in &rewritten {
                    let _ = std::fs::write(path, original);
                }
                cleanup_dst(&nvram_dst);
                return Err(MoveError::Profile {
                    path: profile.clone(),
                    message,
                });
            }
        }
    }

    // 5. Delete the sources. The move is already correct and durable; a
    //    failure here (a locked file, say) is reported, not rolled back.
    let mut leftover_sources = Vec::new();
    if let Err(e) = std::fs::remove_file(&src) {
        tracing::warn!(path = %src.display(), error = %e, "moved, but cannot delete the source image");
        leftover_sources.push(disk.to_path_buf());
    }
    let mut moved = vec![(disk.to_path_buf(), dst)];
    if let (Some(from), Some(to)) = (nvram_src, nvram_dst) {
        if let Err(e) = std::fs::remove_file(&from) {
            tracing::warn!(path = %from.display(), error = %e, "moved, but cannot delete the source nvram");
            leftover_sources.push(from.clone());
        }
        moved.push((from, to));
    }

    Ok(MoveOutcome {
        moved,
        updated_profiles,
        apparent_bytes: stats.apparent_bytes,
        data_bytes: stats.data_bytes,
        leftover_sources,
    })
}

/// Bytes the copy will actually move for `disk` — the allocated data. What the
/// manager's move dialog shows next to the destination's free space.
pub fn copy_bill(disk: &Path) -> Option<(u64 /* data */, u64 /* apparent */)> {
    let apparent = disk.metadata().ok()?.len();
    Some((ops::allocated_bytes(disk).unwrap_or(apparent), apparent))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyStats {
    apparent_bytes: u64,
    data_bytes: u64,
}

/// What [`sparse_copy`] hands back: stats, the copied ranges and the
/// verification digest over them.
type CopyResult = (CopyStats, Vec<(u64, u64)>, [u8; 32]);

/// Copies `src` to `dst` (which must not exist), writing only data ranges into
/// a sparse, pre-sized destination. Returns the stats, the ranges and a digest
/// of `(offset, len, bytes)` over every range, for verification.
fn sparse_copy(src: &Path, dst: &Path, progress: Progress) -> io::Result<CopyResult> {
    let mut src_file = File::open(src)?;
    let len = src_file.metadata()?.len();
    let ranges = data_ranges(&src_file, len);
    let total: u64 = ranges.iter().map(|(_, l)| l).sum();

    let mut dst_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    // Best effort — on a filesystem without sparse support the copy is still
    // correct, only larger.
    let _ = ops::mark_sparse(&dst_file);
    dst_file.set_len(len)?;

    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    progress(0, total);
    let mut buf = vec![0u8; CHUNK];
    for &(offset, length) in &ranges {
        hasher.update(offset.to_le_bytes());
        hasher.update(length.to_le_bytes());
        src_file.seek(SeekFrom::Start(offset))?;
        let mut pos = offset;
        let mut remaining = length;
        while remaining > 0 {
            let n = remaining.min(CHUNK as u64) as usize;
            src_file.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            // A zero chunk inside a "data" range (the fallback path reports the
            // whole file as one range) stays a hole at the destination.
            if buf[..n].iter().any(|&b| b != 0) {
                dst_file.seek(SeekFrom::Start(pos))?;
                dst_file.write_all(&buf[..n])?;
            }
            pos += n as u64;
            remaining -= n as u64;
            copied += n as u64;
            progress(copied, total);
        }
    }
    // Durable before it is verified, verified before anything points at it.
    dst_file.sync_all()?;

    Ok((
        CopyStats {
            apparent_bytes: len,
            data_bytes: total,
        },
        ranges,
        hasher.finalize().into(),
    ))
}

/// The verification digest: the same `(offset, len, bytes)` walk sparse_copy
/// hashed, re-read from `path`.
fn hash_ranges(path: &Path, ranges: &[(u64, u64)]) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    for &(offset, length) in ranges {
        hasher.update(offset.to_le_bytes());
        hasher.update(length.to_le_bytes());
        file.seek(SeekFrom::Start(offset))?;
        let mut remaining = length;
        while remaining > 0 {
            let n = remaining.min(CHUNK as u64) as usize;
            file.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            remaining -= n as u64;
        }
    }
    Ok(hasher.finalize().into())
}

/// Rewrites every disk/cdrom/nvram entry of `profile` that resolves to a
/// mapping's old path, through `control-api` types (never string edits).
/// `Ok(Some(original_text))` when the file changed, `Ok(None)` when it holds
/// no reference.
fn rewrite_profile(
    profile: &Path,
    mappings: &[(PathBuf, PathBuf)],
) -> Result<Option<String>, String> {
    let text = std::fs::read_to_string(profile).map_err(|e| e.to_string())?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let profile_dir = profile.parent();

    let map = |declared: &Path| -> Option<PathBuf> {
        mappings
            .iter()
            .find(|(old, _)| refs::resolves_to(declared, profile_dir, old))
            .map(|(_, new)| new.clone())
    };

    let mut changed = false;
    for disk in &mut cfg.disks {
        if let Some(new) = map(&disk.path) {
            disk.path = new;
            changed = true;
        }
    }
    if let Some(cdrom) = &mut cfg.cdrom {
        if let Some(new) = map(&cdrom.path) {
            cdrom.path = new;
            changed = true;
        }
    }
    if let Some(nvram) = &mut cfg.boot.nvram {
        if let Some(new) = map(nvram) {
            *nvram = new;
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }

    let out = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    // Re-validate the serialized form before it replaces a working profile.
    VmConfig::from_toml(&out).map_err(|e| e.to_string())?;
    std::fs::write(profile, out).map_err(|e| e.to_string())?;
    Ok(Some(text))
}

// ---------------------------------------------------------------------------
// Allocated-range enumeration
// ---------------------------------------------------------------------------

/// The file's allocated (data) ranges as `(offset, len)`, sorted. Falls back
/// to "the whole file is one range" wherever the filesystem cannot answer —
/// the zero-skip in [`sparse_copy`] then keeps the destination sparse anyway.
#[cfg(target_os = "linux")]
fn data_ranges(file: &File, len: u64) -> Vec<(u64, u64)> {
    use std::os::unix::io::AsRawFd as _;
    if len == 0 {
        return Vec::new();
    }
    let fd = file.as_raw_fd();
    let whole = || vec![(0, len)];
    let mut ranges = Vec::new();
    let mut offset: libc::off_t = 0;
    loop {
        // SAFETY: `fd` belongs to the live `File` borrowed for this call; no
        // memory crosses the boundary.
        let data = unsafe { libc::lseek(fd, offset, libc::SEEK_DATA) };
        if data < 0 {
            let errno = io::Error::last_os_error().raw_os_error();
            // ENXIO: nothing but holes from `offset` on — done.
            return if errno == Some(libc::ENXIO) {
                ranges
            } else {
                whole()
            };
        }
        // SAFETY: same fd; SEEK_HOLE always succeeds past a data offset (the
        // implicit hole at EOF backstops it).
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < data {
            return whole();
        }
        ranges.push((data as u64, (hole - data) as u64));
        offset = hole;
        if offset as u64 >= len {
            return ranges;
        }
    }
}

/// FSCTL_QUERY_ALLOCATED_RANGES on NTFS; one output buffer per round, looping
/// on ERROR_MORE_DATA.
#[cfg(windows)]
fn data_ranges(file: &File, len: u64) -> Vec<(u64, u64)> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
    use windows::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    if len == 0 {
        return Vec::new();
    }
    let Ok(len_i) = i64::try_from(len) else {
        return vec![(0, len)];
    };
    let handle = HANDLE(file.as_raw_handle());
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut next: i64 = 0;
    loop {
        let query = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: next,
            Length: len_i - next,
        };
        let mut out = [FILE_ALLOCATED_RANGE_BUFFER::default(); 64];
        let mut returned = 0u32;
        // SAFETY: the handle belongs to the live `File`; the input buffer is a
        // valid FILE_ALLOCATED_RANGE_BUFFER for its stated size, the output
        // buffer is writable for its stated size, and `returned` is a valid
        // out-pointer — all for the duration of the call.
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_ALLOCATED_RANGES,
                Some(&query as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                Some(out.as_mut_ptr() as *mut std::ffi::c_void),
                std::mem::size_of_val(&out) as u32,
                Some(&mut returned),
                None,
            )
        };
        let more = match result {
            Ok(()) => false,
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
            // Not NTFS (or a filter refused): copy everything, zero-skip keeps
            // the destination sparse.
            Err(_) => return vec![(0, len)],
        };
        let count = returned as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        for range in &out[..count] {
            ranges.push((range.FileOffset as u64, range.Length as u64));
            next = range.FileOffset + range.Length;
        }
        if !more || count == 0 {
            return ranges;
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn data_ranges(_file: &File, len: u64) -> Vec<(u64, u64)> {
    if len == 0 {
        Vec::new()
    } else {
        vec![(0, len)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::temp_dir;

    /// A sparse fixture: `apparent` bytes with data planted at `chunks` of
    /// `(offset, byte, len)`.
    fn sparse_fixture(path: &Path, apparent: u64, chunks: &[(u64, u8, usize)]) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        let _ = ops::mark_sparse(&file);
        file.set_len(apparent).unwrap();
        let mut file = file;
        for &(offset, byte, len) in chunks {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&vec![byte; len]).unwrap();
        }
    }

    fn read_all(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn moves_a_sparse_image_preserving_content_and_holes() {
        let dir = temp_dir("move-sparse");
        let src_dir = dir.join("src");
        let dst_dir = dir.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        let disk = src_dir.join("vm.raw");
        // 32 MiB apparent, ~2 MiB of data in three islands.
        sparse_fixture(
            &disk,
            32 << 20,
            &[
                (0, 0xaa, 4096),
                (5 << 20, 0xbb, 1 << 20),
                (30 << 20, 0xcc, 512),
            ],
        );
        std::fs::write(src_dir.join("vm.nvram"), b"boot-entries").unwrap();
        let original = read_all(&disk);
        let src_allocated = ops::allocated_bytes(&disk);

        let mut updates = Vec::new();
        let outcome = move_disk(&disk, &dst_dir, &[], &mut |done, total| {
            updates.push((done, total))
        })
        .expect("move");

        let new_disk = dst_dir.join("vm.raw");
        assert!(!disk.exists(), "source deleted after verification");
        assert!(!src_dir.join("vm.nvram").exists());
        assert!(new_disk.is_file());
        assert_eq!(read_all(&dst_dir.join("vm.nvram")), b"boot-entries");
        assert_eq!(outcome.apparent_bytes, 32 << 20);
        assert_eq!(outcome.moved.len(), 2);
        assert!(outcome.leftover_sources.is_empty());

        // Content identical, byte for byte (holes read as zeros).
        assert_eq!(read_all(&new_disk), original);

        // Progress: monotonic, ends at total.
        assert!(updates.windows(2).all(|w| w[0].0 <= w[1].0));
        let &(done, total) = updates.last().unwrap();
        assert_eq!(done, total);

        // Sparseness preserved: wherever the platform can measure allocation
        // and the source was sparse, the destination must be far below
        // apparent too (this is the 24G-apparent/7G-actual case in miniature).
        if let (Some(src_alloc), Some(dst_alloc)) = (src_allocated, ops::allocated_bytes(&new_disk))
        {
            if src_alloc < (32 << 20) / 2 {
                assert!(
                    dst_alloc < (32 << 20) / 2,
                    "copy ballooned: {dst_alloc} allocated of {} apparent",
                    32 << 20
                );
                // And the copy moved ~the data, not ~the apparent size.
                assert!(outcome.data_bytes < 32 << 20);
            }
        }
    }

    #[test]
    fn move_rewrites_referencing_profiles_and_only_them() {
        let dir = temp_dir("move-profiles");
        let src_dir = dir.join("vms");
        let dst_dir = dir.join("bigdrive");
        std::fs::create_dir_all(&src_dir).unwrap();
        let disk = src_dir.join("desktop.raw");
        sparse_fixture(&disk, 4 << 20, &[(0, 0x11, 8192)]);
        std::fs::write(src_dir.join("desktop.nvram"), b"vars").unwrap();

        let profile = src_dir.join("desktop.toml");
        std::fs::write(
            &profile,
            format!(
                r#"
name = "desktop"
memory_mib = 2048
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"
nvram = "{nvram}"

[[disk]]
path = "{disk}"
writable = true
"#,
                nvram = src_dir
                    .join("desktop.nvram")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\"),
                disk = disk.display().to_string().replace('\\', "\\\\"),
            ),
        )
        .unwrap();
        let bystander = src_dir.join("other.toml");
        std::fs::write(
            &bystander,
            r#"
name = "other"
memory_mib = 1024
vcpus = 1

[boot]
mode = "direct-linux"
kernel = "vmlinuz"

[[disk]]
path = "other.raw"
"#,
        )
        .unwrap();
        let bystander_before = std::fs::read_to_string(&bystander).unwrap();

        let outcome = move_disk(
            &disk,
            &dst_dir,
            &[profile.clone(), bystander.clone()],
            &mut |_, _| {},
        )
        .expect("move");
        assert_eq!(outcome.updated_profiles, vec![profile.clone()]);

        // The rewritten profile parses, validates and points at the new files.
        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&profile).unwrap()).unwrap();
        assert_eq!(cfg.disks[0].path, dst_dir.join("desktop.raw"));
        assert_eq!(cfg.boot.nvram, Some(dst_dir.join("desktop.nvram")));
        // The bystander is untouched, byte for byte.
        assert_eq!(
            std::fs::read_to_string(&bystander).unwrap(),
            bystander_before
        );
    }

    #[test]
    fn a_failed_profile_rewrite_rolls_everything_back() {
        let dir = temp_dir("move-rollback");
        let src_dir = dir.join("vms");
        let dst_dir = dir.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        let disk = src_dir.join("vm.raw");
        sparse_fixture(&disk, 1 << 20, &[(0, 0x22, 4096)]);

        let good = src_dir.join("good.toml");
        std::fs::write(
            &good,
            format!(
                r#"
name = "good"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "vmlinuz"

[[disk]]
path = "{}"
"#,
                disk.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let good_before = std::fs::read_to_string(&good).unwrap();
        // A "profile" that cannot be parsed: rewriting it fails after `good`
        // was already rewritten, which must restore `good` and drop the copy.
        let broken = src_dir.join("broken.toml");
        std::fs::write(&broken, "not a profile").unwrap();

        let error = move_disk(
            &disk,
            &dst_dir,
            &[good.clone(), broken.clone()],
            &mut |_, _| {},
        )
        .expect_err("broken profile must fail the move");
        assert!(matches!(error, MoveError::Profile { .. }), "{error}");

        assert!(disk.exists(), "source image untouched");
        assert!(!dst_dir.join("vm.raw").exists(), "destination copy removed");
        assert_eq!(
            std::fs::read_to_string(&good).unwrap(),
            good_before,
            "already-rewritten profile restored"
        );
    }

    #[test]
    fn move_refuses_bad_destinations() {
        let dir = temp_dir("move-refuse");
        let src_dir = dir.join("vms");
        std::fs::create_dir_all(&src_dir).unwrap();
        let disk = src_dir.join("vm.raw");
        sparse_fixture(&disk, 1 << 20, &[(0, 0x33, 512)]);

        // Same directory: the destination is the source.
        let error = move_disk(&disk, &src_dir, &[], &mut |_, _| {}).expect_err("same file");
        assert!(matches!(error, MoveError::SameFile(_)), "{error}");
        assert!(disk.exists());

        // Occupied destination.
        let dst_dir = dir.join("occupied");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::write(dst_dir.join("vm.raw"), b"tenant").unwrap();
        let error = move_disk(&disk, &dst_dir, &[], &mut |_, _| {}).expect_err("occupied");
        assert!(matches!(error, MoveError::DestinationExists(_)), "{error}");
        assert_eq!(read_all(&dst_dir.join("vm.raw")), b"tenant");

        // Missing source.
        let error = move_disk(
            &src_dir.join("ghost.raw"),
            &dir.join("x"),
            &[],
            &mut |_, _| {},
        )
        .expect_err("missing");
        assert!(matches!(error, MoveError::Missing(_)), "{error}");
    }

    #[test]
    fn data_ranges_cover_exactly_the_data_where_supported() {
        let dir = temp_dir("move-ranges");
        let path = dir.join("ranged.raw");
        sparse_fixture(&path, 16 << 20, &[(1 << 20, 0x44, 4096)]);
        let file = File::open(&path).unwrap();
        let ranges = data_ranges(&file, 16 << 20);
        // Every byte of data must be inside some range, whatever the platform
        // reports (a single whole-file range is legal).
        let covers = |offset: u64| ranges.iter().any(|&(o, l)| offset >= o && offset < o + l);
        assert!(covers(1 << 20));
        assert!(covers((1 << 20) + 4095));
        let total: u64 = ranges.iter().map(|(_, l)| l).sum();
        assert!(total <= 16 << 20);
        assert!(total >= 4096);
    }

    #[test]
    fn hash_ranges_detects_corruption() {
        let dir = temp_dir("move-hash");
        let a = dir.join("a.raw");
        let b = dir.join("b.raw");
        sparse_fixture(&a, 1 << 20, &[(4096, 0x55, 512)]);
        std::fs::copy(&a, &b).unwrap();
        let ranges = vec![(0u64, 1u64 << 20)];
        assert_eq!(
            hash_ranges(&a, &ranges).unwrap(),
            hash_ranges(&b, &ranges).unwrap()
        );
        // Flip one byte.
        let mut f = std::fs::OpenOptions::new().write(true).open(&b).unwrap();
        f.seek(SeekFrom::Start(4100)).unwrap();
        f.write_all(&[0x56]).unwrap();
        drop(f);
        assert_ne!(
            hash_ranges(&a, &ranges).unwrap(),
            hash_ranges(&b, &ranges).unwrap()
        );
    }
}
