//! Filesystem-level operations on RAW disk images: size parsing, sparse
//! creation, grow-only resize, apparent-vs-allocated size, free space and the
//! `.nvram` sidecar convention.
//!
//! The platform-specific pieces are all *optional* information or best-effort
//! optimisations: `allocated_bytes`/`disk_space` return `None` where the host
//! cannot answer, and `mark_sparse` failing (a FAT volume, say) merely costs
//! disk space, never correctness.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiskError {
    #[error("invalid size '{0}': use e.g. 32G, 512M or a byte count")]
    InvalidSize(String),

    #[error("size must be a positive multiple of 512 bytes, got {0}")]
    BadAlignment(u64),

    #[error("refusing to overwrite existing file {0}")]
    Exists(String),

    #[error("{0} does not exist or is not a regular file")]
    NotAFile(String),

    #[error(
        "refusing to shrink {path} from {current} to {requested} bytes: the {lost} bytes at \
         the end — and any partition or filesystem data inside them — would be destroyed. \
         Shrink filesystems and the partition table inside the guest first, or copy to a \
         new smaller image"
    )]
    Shrink {
        path: String,
        current: u64,
        requested: u64,
        lost: u64,
    },

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

/// Parses "32G" / "512M" / "1T" / plain byte counts into bytes.
pub fn parse_size(s: &str) -> Result<u64, DiskError> {
    let s = s.trim();
    let err = || DiskError::InvalidSize(s.to_string());
    let (digits, multiplier) = match s.char_indices().last().ok_or_else(err)? {
        (i, 'K' | 'k') => (&s[..i], 1u64 << 10),
        (i, 'M' | 'm') => (&s[..i], 1 << 20),
        (i, 'G' | 'g') => (&s[..i], 1 << 30),
        (i, 'T' | 't') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    let value: u64 = digits.parse().map_err(|_| err())?;
    value.checked_mul(multiplier).ok_or_else(err)
}

/// Creates a sparse RAW disk image of exactly `bytes` bytes. Never
/// overwrites an existing file.
pub fn create_raw(path: &Path, bytes: u64) -> Result<(), DiskError> {
    if bytes == 0 || bytes % 512 != 0 {
        return Err(DiskError::BadAlignment(bytes));
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                DiskError::Exists(path.display().to_string())
            } else {
                DiskError::Io(e)
            }
        })?;
    // set_len produces a sparse file on ext4/xfs/btrfs; NTFS additionally wants
    // the sparse attribute set first, or it reserves clusters up to EOF. Failure
    // (FAT, exotic filters) costs space, not correctness — ignore it.
    let _ = mark_sparse(&file);
    file.set_len(bytes)?;
    Ok(())
}

/// What [`resize_raw`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeOutcome {
    pub previous_bytes: u64,
    pub new_bytes: u64,
}

impl ResizeOutcome {
    /// False when the image already had the requested size (a no-op).
    pub fn grew(&self) -> bool {
        self.new_bytes > self.previous_bytes
    }
}

/// Grows a RAW image to `bytes`, sparsely (the new tail occupies no disk
/// space until the guest writes it). **Grow-only**: shrinking is refused with
/// [`DiskError::Shrink`] naming exactly what would be lost, because the bytes
/// past the new end are guest data — a filesystem's blocks, or the GPT backup
/// header. Asking for the current size is an explicit no-op, not an error.
///
/// The partition table inside the image still describes the old size
/// afterwards; the guest has to grow it (parted/growpart + resize2fs). The
/// caller owns telling the user that.
pub fn resize_raw(path: &Path, bytes: u64) -> Result<ResizeOutcome, DiskError> {
    if bytes == 0 || bytes % 512 != 0 {
        return Err(DiskError::BadAlignment(bytes));
    }
    let meta =
        std::fs::metadata(path).map_err(|_| DiskError::NotAFile(path.display().to_string()))?;
    if !meta.is_file() {
        return Err(DiskError::NotAFile(path.display().to_string()));
    }
    let current = meta.len();
    if bytes < current {
        return Err(DiskError::Shrink {
            path: path.display().to_string(),
            current,
            requested: bytes,
            lost: current - bytes,
        });
    }
    let outcome = ResizeOutcome {
        previous_bytes: current,
        new_bytes: bytes,
    };
    if bytes == current {
        return Ok(outcome);
    }
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    // Best effort, same rationale as in `create_raw`. On an image created
    // before this crate marked files sparse, this also stops the *extension*
    // from being allocated on NTFS (existing clusters keep their allocation).
    let _ = mark_sparse(&file);
    file.set_len(bytes)?;
    Ok(outcome)
}

/// The `.nvram` sidecar path of a disk image (`desktop.raw` → `desktop.nvram`):
/// the UEFI variable store `entangled install ubuntu` writes next to the disk.
pub fn nvram_sidecar_path(disk: &Path) -> PathBuf {
    disk.with_extension("nvram")
}

/// The `.nvram` sidecar, if one actually exists next to the disk.
pub fn existing_nvram_sidecar(disk: &Path) -> Option<PathBuf> {
    let sidecar = nvram_sidecar_path(disk);
    sidecar.is_file().then_some(sidecar)
}

/// Binary-prefix formatting ("16.0 GiB") for every disk-size label in the
/// product — the CLI and the manager share this so no two views round
/// differently.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Platform pieces
// ---------------------------------------------------------------------------

/// Bytes the file actually occupies on disk (its data ranges, compression and
/// sparseness accounted for). `None` when the platform cannot say — the caller
/// shows only the apparent size then.
#[cfg(unix)]
pub fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    // st_blocks is always in 512-byte units, regardless of the filesystem's
    // block size.
    Some(std::fs::metadata(path).ok()?.blocks().saturating_mul(512))
}

/// Bytes the file actually occupies on disk. On NTFS the "compressed" size is
/// the allocated size for sparse and compressed files alike.
#[cfg(windows)]
pub fn allocated_bytes(path: &Path) -> Option<u64> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let wide = wide_path(path);
    let mut high: u32 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call,
    // and `high` is a valid out-pointer for the duration of the call.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR::from_raw(wide.as_ptr()), Some(&mut high)) };
    if low == u32::MAX {
        // 0xFFFFFFFF is both "error" and a legal low half; the last-error
        // code disambiguates.
        // SAFETY: reads this thread's last-error slot; no pointers involved.
        if unsafe { GetLastError() } != NO_ERROR {
            return None;
        }
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

#[cfg(not(any(unix, windows)))]
pub fn allocated_bytes(_path: &Path) -> Option<u64> {
    None
}

/// `(free, total)` bytes of the filesystem holding `dir`, or `None` when the
/// host cannot say.
#[cfg(unix)]
pub fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: plain-old-data out-struct; every field is meaningful as zero
    // until statvfs fills it.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `stats` a valid
    // out-pointer for the duration of the call.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stats) };
    if rc != 0 {
        return None;
    }
    // f_frsize is the unit of f_blocks/f_bavail; fall back to f_bsize where a
    // filesystem reports 0.
    let unit = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    } as u64;
    Some((
        (stats.f_bavail as u64).saturating_mul(unit),
        (stats.f_blocks as u64).saturating_mul(unit),
    ))
}

/// `(free, total)` bytes of the volume holding `dir`.
#[cfg(windows)]
pub fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide = wide_path(dir);
    let mut free: u64 = 0;
    let mut total: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer and both out-pointers
    // are valid for the duration of the call.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut free),
            Some(&mut total),
            None,
        )
    }
    .ok()?;
    Some((free, total))
}

#[cfg(not(any(unix, windows)))]
pub fn disk_space(_dir: &Path) -> Option<(u64, u64)> {
    None
}

/// Marks an open file sparse. A no-op outside Windows: unix filesystems keep
/// holes wherever nothing was written, with no attribute involved.
#[cfg(windows)]
pub(crate) fn mark_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows::Win32::System::IO::DeviceIoControl;

    let mut returned = 0u32;
    // SAFETY: the handle belongs to the live `File` borrowed for this call;
    // FSCTL_SET_SPARSE takes no input buffer (absence means "set the
    // attribute") and no output buffer; `returned` is a valid out-pointer.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    // windows::core::Error does not implement std::error::Error without the
    // crate's `std` feature; the message string is all the caller needs.
    .map_err(|e| io::Error::other(e.message()))
}

#[cfg(not(windows))]
pub(crate) fn mark_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::temp_dir;

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_size("32G").unwrap(), 32 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("999999999T").is_err()); // overflow
    }

    #[test]
    fn creates_sparse_and_refuses_overwrite() {
        let dir = temp_dir("ops-create");
        let path = dir.join("t.raw");

        create_raw(&path, 1 << 20).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1 << 20);
        assert!(matches!(
            create_raw(&path, 1 << 20),
            Err(DiskError::Exists(_))
        ));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejects_unaligned_sizes() {
        let p = std::env::temp_dir().join("never-created.raw");
        assert!(matches!(
            create_raw(&p, 100),
            Err(DiskError::BadAlignment(100))
        ));
        assert!(!p.exists());
        assert!(matches!(
            resize_raw(&p, 100),
            Err(DiskError::BadAlignment(100))
        ));
    }

    #[test]
    fn a_fresh_image_occupies_almost_nothing() {
        let dir = temp_dir("ops-sparse");
        let path = dir.join("sparse.raw");
        create_raw(&path, 64 << 20).unwrap();
        // Sparse on every filesystem this project supports (ext4, NTFS): the
        // 64 MiB image must not allocate 64 MiB. The bound is generous —
        // metadata, not data.
        if let Some(allocated) = allocated_bytes(&path) {
            assert!(
                allocated < 1 << 20,
                "expected a sparse image, got {allocated} bytes allocated"
            );
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn resize_grows_sparsely_and_reports_the_sizes() {
        let dir = temp_dir("ops-grow");
        let path = dir.join("grow.raw");
        create_raw(&path, 1 << 20).unwrap();
        // Real data at the front so the grown file still carries it.
        {
            use std::io::{Seek as _, SeekFrom, Write as _};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"BOOTSECTORDATA").unwrap();
        }

        let outcome = resize_raw(&path, 8 << 20).unwrap();
        assert_eq!(outcome.previous_bytes, 1 << 20);
        assert_eq!(outcome.new_bytes, 8 << 20);
        assert!(outcome.grew());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8 << 20);
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..14], b"BOOTSECTORDATA");

        // The grown tail is a hole, not 7 MiB of allocated zeros.
        if let Some(allocated) = allocated_bytes(&path) {
            assert!(
                allocated < 2 << 20,
                "expected a sparse extension, got {allocated} bytes allocated"
            );
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn resize_refuses_to_shrink_and_names_the_loss() {
        let dir = temp_dir("ops-shrink");
        let path = dir.join("shrink.raw");
        create_raw(&path, 4 << 20).unwrap();

        let error = resize_raw(&path, 1 << 20).expect_err("shrink must be refused");
        let DiskError::Shrink {
            current,
            requested,
            lost,
            ..
        } = &error
        else {
            panic!("expected Shrink, got {error}");
        };
        assert_eq!(*current, 4 << 20);
        assert_eq!(*requested, 1 << 20);
        assert_eq!(*lost, 3 << 20);
        let message = error.to_string();
        assert!(message.contains("would be destroyed"), "{message}");
        // Nothing happened.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4 << 20);

        // Same size: explicit no-op.
        let outcome = resize_raw(&path, 4 << 20).unwrap();
        assert!(!outcome.grew());

        // A missing file is a typed error, not io noise.
        assert!(matches!(
            resize_raw(&dir.join("ghost.raw"), 1 << 20),
            Err(DiskError::NotAFile(_))
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn nvram_sidecar_follows_the_stem() {
        let dir = temp_dir("ops-nvram");
        let disk = dir.join("desktop.raw");
        assert_eq!(nvram_sidecar_path(&disk), dir.join("desktop.nvram"));
        assert_eq!(existing_nvram_sidecar(&disk), None);
        std::fs::write(dir.join("desktop.nvram"), b"vars").unwrap();
        assert_eq!(
            existing_nvram_sidecar(&disk),
            Some(dir.join("desktop.nvram"))
        );
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
        assert_eq!(format_bytes(700 * 1024 * 1024), "700 MiB");
    }

    #[test]
    fn disk_space_answers_for_the_temp_dir() {
        // The exact numbers are the host's business; the shape must hold.
        if let Some((free, total)) = disk_space(&std::env::temp_dir()) {
            assert!(total > 0);
            assert!(free <= total);
        }
    }
}
