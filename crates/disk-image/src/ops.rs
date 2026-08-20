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
// Thin-provisioning reclaim: hole punching and zeroing
// ---------------------------------------------------------------------------
//
// A sparse RAW image only ever grows: `create_raw` allocates nothing and the
// guest's writes fill it in. Getting space *back* needs the host to deallocate
// the ranges the guest has stopped using, which is what the virtio-blk
// `DISCARD` / `WRITE_ZEROES` commands ask for (the guest side of that is
// `fstrim`, or a mount with `discard`).
//
// Two host mechanisms, one meaning:
//
// * Linux `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)` — drops the
//   range's blocks, keeps the file length, and guarantees the range reads back
//   as zeros.
// * Windows `FSCTL_SET_ZERO_DATA` on a **sparse** file — deallocates the whole
//   clusters inside the range and zeroes the partial ones at either end. On a
//   file that is not sparse the same call writes zeros without deallocating,
//   which is exactly the "keep the blocks provisioned" variant.
//
// Both can fail on a filesystem that has no holes to give (FAT, drvfs, an
// exotic filter). Correctness must not depend on them, so the split is:
// [`write_zeroes`] falls back to writing real zeros and therefore *always*
// leaves zeros behind, while [`punch_hole`] is allowed to report
// [`PunchOutcome::Unsupported`] and change nothing — a discard is a hint, and
// the spec lets a device ignore it.

/// Which mechanism actually served a [`punch_hole`] or [`write_zeroes`] call.
///
/// Worth logging once per backing file: it is the difference between a guest
/// `fstrim` that reclaims host space and one that only looks like it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchOutcome {
    /// The filesystem deallocated the range. The host got the space back and
    /// the range reads back as zeros.
    Deallocated,
    /// Real zeros were written. No space is reclaimed, but the range reads back
    /// as zeros — all that `WRITE_ZEROES` promises.
    Zeroed,
    /// The filesystem cannot punch holes here and nothing was changed. Only a
    /// discard may legally end this way.
    Unsupported,
}

impl PunchOutcome {
    /// A short label for log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            PunchOutcome::Deallocated => "deallocated",
            PunchOutcome::Zeroed => "zero-filled",
            PunchOutcome::Unsupported => "unsupported",
        }
    }
}

/// Chunk used by the zero-writing fallback. Fixed and small on purpose: the
/// range a guest may ask to zero is bounded by the device, but the *host*
/// memory it costs must not depend on that bound at all.
const ZERO_CHUNK: usize = 64 << 10;

/// Deallocates `len` bytes at `offset` in `file`, keeping the file length.
///
/// Never writes: on a filesystem that cannot punch holes this reports
/// [`PunchOutcome::Unsupported`] and leaves the range exactly as it was. The
/// caller decides whether that is acceptable — it is for `DISCARD`, it is not
/// for `WRITE_ZEROES` (use [`write_zeroes`] there).
///
/// A zero-length range is a no-op and reports [`PunchOutcome::Zeroed`], since
/// an empty range trivially already reads as zeros.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<PunchOutcome> {
    if len == 0 {
        return Ok(PunchOutcome::Zeroed);
    }
    // `offset + len` must be expressible for every backend below. The device
    // has already checked the range against the image, but this function is
    // public and must not depend on its caller for that.
    offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "punch range overflows u64"))?;
    punch_hole_impl(file, offset, len)
}

/// Makes `len` bytes at `offset` read back as zeros, deallocating them when
/// `may_unmap` is set and the filesystem can.
///
/// **Always leaves zeros** — that is the `VIRTIO_BLK_T_WRITE_ZEROES` contract,
/// so a filesystem without holes gets the zero-writing fallback rather than an
/// error. `may_unmap` false skips punching entirely: the guest asked for the
/// blocks to stay provisioned.
pub fn write_zeroes(
    file: &File,
    offset: u64,
    len: u64,
    may_unmap: bool,
) -> io::Result<PunchOutcome> {
    if len == 0 {
        return Ok(PunchOutcome::Zeroed);
    }
    offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero range overflows u64"))?;
    if may_unmap {
        // Both backends guarantee a punched range reads back as zeros, so a
        // successful punch satisfies WRITE_ZEROES outright.
        match punch_hole_impl(file, offset, len) {
            Ok(PunchOutcome::Unsupported) => {}
            Ok(other) => return Ok(other),
            // A punch that fails for any other reason must not cost us the
            // zeroing guarantee: fall through and write them.
            Err(_) => {}
        }
    }
    zero_fill(file, offset, len)?;
    Ok(PunchOutcome::Zeroed)
}

/// Writes `len` real zero bytes at `offset`, in [`ZERO_CHUNK`] pieces.
///
/// Positional throughout, so it never disturbs a cursor another handle shares
/// and can never extend the file past `offset + len`.
fn zero_fill(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let zeros = [0u8; ZERO_CHUNK];
    let mut done = 0u64;
    while done < len {
        let chunk = usize::try_from((len - done).min(ZERO_CHUNK as u64)).unwrap_or(ZERO_CHUNK);
        positional_write_all(file, &zeros[..chunk], offset.saturating_add(done))?;
        done += chunk as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn positional_write_all(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn positional_write_all(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    let mut done = 0usize;
    while done < buf.len() {
        let written = file.seek_write(&buf[done..], offset.saturating_add(done as u64))?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zero-fill wrote nothing",
            ));
        }
        done += written;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn positional_write_all(_file: &File, _buf: &[u8], _offset: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no positional write on this platform",
    ))
}

/// `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`.
///
/// `EOPNOTSUPP`/`ENOSYS` is the kernel saying this filesystem has no holes to
/// give. `EINVAL` is folded in with them: a few filesystems answer an
/// unsupported mode combination that way, and being wrong about it only costs
/// `WRITE_ZEROES` the zero-writing path — correctness is unaffected either way.
#[cfg(target_os = "linux")]
fn punch_hole_impl(file: &File, offset: u64, len: u64) -> io::Result<PunchOutcome> {
    use std::os::unix::io::AsRawFd as _;

    let (Ok(off), Ok(count)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "punch range does not fit an off_t",
        ));
    };
    // SAFETY: `fd` belongs to the live `File` borrowed for this call and no
    // memory crosses the boundary — fallocate takes integers only. The mode is
    // the documented "deallocate, keep the length" combination, so the file's
    // size cannot change under any other holder of the same file.
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            off,
            count,
        )
    };
    if rc == 0 {
        return Ok(PunchOutcome::Deallocated);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) => Ok(PunchOutcome::Unsupported),
        _ => Err(error),
    }
}

/// No `fallocate` outside Linux; the caller's zero-fill fallback covers it.
#[cfg(all(unix, not(target_os = "linux")))]
fn punch_hole_impl(_file: &File, _offset: u64, _len: u64) -> io::Result<PunchOutcome> {
    Ok(PunchOutcome::Unsupported)
}

/// `FSCTL_SET_ZERO_DATA`, after making sure the file carries the sparse
/// attribute — without it NTFS zeroes the range but keeps every cluster
/// allocated, which is a correct `WRITE_ZEROES` and a useless `DISCARD`.
#[cfg(windows)]
fn punch_hole_impl(file: &File, offset: u64, len: u64) -> io::Result<PunchOutcome> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, HANDLE, WIN32_ERROR,
    };
    use windows::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};
    use windows::Win32::System::IO::DeviceIoControl;

    let (Ok(start), Ok(end)) = (
        i64::try_from(offset),
        i64::try_from(offset.saturating_add(len)),
    ) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero range does not fit a LARGE_INTEGER",
        ));
    };
    // Idempotent, and cheap next to the ioctl. An image from `create_raw` is
    // already sparse; one restored from a backup, copied by Explorer or made by
    // an older build is not — and a guest fstrim is exactly the moment we would
    // like it to be.
    let _ = mark_sparse(file);

    let request = FILE_ZERO_DATA_INFORMATION {
        FileOffset: start,
        BeyondFinalZero: end,
    };
    let mut returned = 0u32;
    // SAFETY: the handle belongs to the live `File` borrowed for this call; the
    // input buffer is one initialised FILE_ZERO_DATA_INFORMATION described by
    // its own `size_of`, this FSCTL takes no output buffer, and `returned` is a
    // valid out-pointer — all of them live for the whole call.
    let result = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_ZERO_DATA,
            Some(&request as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    match result {
        Ok(()) => Ok(PunchOutcome::Deallocated),
        Err(e) => {
            let unsupported = [ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED]
                .iter()
                .any(|code: &WIN32_ERROR| e.code() == code.to_hresult());
            if unsupported {
                Ok(PunchOutcome::Unsupported)
            } else {
                Err(io::Error::other(e.message()))
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn punch_hole_impl(_file: &File, _offset: u64, _len: u64) -> io::Result<PunchOutcome> {
    Ok(PunchOutcome::Unsupported)
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

    // -------------------------------------------------- hole punch / zeroing

    /// Creates a sparse image and fills `data_bytes` at the front with a
    /// recognisable pattern, so a punch can be seen in both the content and
    /// the allocated size.
    fn filled_image(dir: &Path, name: &str, apparent: u64, data_bytes: u64) -> PathBuf {
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        create_raw(&path, apparent).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let chunk = vec![0xa5u8; 64 << 10];
        let mut written = 0u64;
        while written < data_bytes {
            let n = ((data_bytes - written) as usize).min(chunk.len());
            positional_write_all(&file, &chunk[..n], written).unwrap();
            written += n as u64;
        }
        file.sync_all().unwrap();
        path
    }

    fn read_range(path: &Path, offset: u64, len: usize) -> Vec<u8> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut f = File::open(path).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).unwrap();
        buf
    }

    /// The acceptance for the host half: a punched range reads back as zeros,
    /// the file keeps its length, and where the filesystem supports holes the
    /// allocated size actually drops. This is the same mechanism a guest
    /// `fstrim` drives through virtio-blk DISCARD.
    #[test]
    fn punching_a_hole_reclaims_space_and_reads_back_as_zeros() {
        let dir = temp_dir("ops-punch");
        // 8 MiB of real data at the front of a 16 MiB image.
        let path = filled_image(&dir, "punch.raw", 16 << 20, 8 << 20);
        let before = allocated_bytes(&path);
        assert_eq!(read_range(&path, 4 << 20, 16), [0xa5u8; 16]);

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        // Punch the middle 4 MiB, leaving 2 MiB of data on either side.
        let outcome = punch_hole(&file, 2 << 20, 4 << 20).unwrap();
        drop(file);

        // Length never changes: FALLOC_FL_KEEP_SIZE / FSCTL_SET_ZERO_DATA.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16 << 20);
        // A filesystem that kept the untouched 8 MiB tail as a hole can punch
        // holes, so on *this* host `Unsupported` would be a regression rather
        // than a limitation. That is what makes this a real assertion on ext4
        // and on NTFS, and still a skip on drvfs.
        if before.is_some_and(|allocated| allocated < 10 << 20) {
            assert_eq!(
                outcome,
                PunchOutcome::Deallocated,
                "this filesystem keeps holes, so it must be able to punch one"
            );
        }
        match outcome {
            PunchOutcome::Deallocated => {
                // The hole reads as zeros...
                assert_eq!(read_range(&path, 2 << 20, 4096), vec![0u8; 4096]);
                assert_eq!(read_range(&path, (6 << 20) - 4096, 4096), vec![0u8; 4096]);
                // ...the data around it survived...
                assert_eq!(read_range(&path, 0, 16), [0xa5u8; 16]);
                assert_eq!(read_range(&path, 6 << 20, 16), [0xa5u8; 16]);
                // ...and the host got the space back. Printed as well as
                // asserted: which mechanism a host actually has is the one
                // thing this test knows and a reader of the log does not.
                if let (Some(before), Some(after)) = (before, allocated_bytes(&path)) {
                    println!(
                        "punch_hole: 16 MiB image, 8 MiB written, 4 MiB punched: \
                         allocated {} -> {}",
                        format_bytes(before),
                        format_bytes(after)
                    );
                    assert!(
                        after + (3 << 20) <= before,
                        "punching 4 MiB should reclaim it: {before} -> {after}"
                    );
                }
            }
            PunchOutcome::Unsupported => {
                // Legal: a discard is a hint. Nothing may have changed.
                assert_eq!(read_range(&path, 2 << 20, 16), [0xa5u8; 16]);
            }
            PunchOutcome::Zeroed => panic!("punch_hole must never write zeros itself"),
        }
        std::fs::remove_file(&path).unwrap();
    }

    /// `WRITE_ZEROES` has no escape hatch: whatever the filesystem can do, the
    /// range must read back as zeros afterwards.
    #[test]
    fn write_zeroes_always_leaves_zeros_whichever_path_it_takes() {
        let dir = temp_dir("ops-zeroes");
        let path = filled_image(&dir, "zeroes.raw", 4 << 20, 4 << 20);

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        // With unmap: may deallocate, must still read as zeros.
        let unmapped = write_zeroes(&file, 1 << 20, 1 << 20, true).unwrap();
        assert_ne!(
            unmapped,
            PunchOutcome::Unsupported,
            "write_zeroes must never give up: it can always write zeros"
        );
        // Without unmap: the guest wants the blocks kept, so zeros are written.
        let kept = write_zeroes(&file, 3 << 20, 512 << 10, false).unwrap();
        assert_eq!(kept, PunchOutcome::Zeroed);
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4 << 20);
        assert_eq!(read_range(&path, 1 << 20, 8192), vec![0u8; 8192]);
        assert_eq!(read_range(&path, (2 << 20) - 8192, 8192), vec![0u8; 8192]);
        assert_eq!(read_range(&path, 3 << 20, 8192), vec![0u8; 8192]);
        // Untouched data on both sides of both ranges.
        assert_eq!(read_range(&path, 0, 16), [0xa5u8; 16]);
        assert_eq!(read_range(&path, 2 << 20, 16), [0xa5u8; 16]);
        assert_eq!(read_range(&path, (3 << 20) + (512 << 10), 16), [0xa5u8; 16]);
        std::fs::remove_file(&path).unwrap();
    }

    /// Sub-sector, unaligned and boundary ranges: the device lets the guest ask
    /// for any 512-byte multiple, and the filesystem's own granularity is
    /// coarser. Whatever gets deallocated, the *content* must be exact — a
    /// punch that zeroed one byte too many would silently corrupt a guest
    /// filesystem.
    #[test]
    fn unaligned_ranges_zero_exactly_what_was_asked_for() {
        let dir = temp_dir("ops-unaligned");
        let path = filled_image(&dir, "unaligned.raw", 1 << 20, 1 << 20);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();

        // One sector in the middle of a 4 KiB cluster, and a range straddling
        // two clusters.
        write_zeroes(&file, 4096 + 512, 512, true).unwrap();
        write_zeroes(&file, (64 << 10) - 512, 1024, true).unwrap();
        // A zero-length range is a no-op, not an error.
        assert_eq!(
            write_zeroes(&file, 8192, 0, true).unwrap(),
            PunchOutcome::Zeroed
        );
        assert_eq!(punch_hole(&file, 8192, 0).unwrap(), PunchOutcome::Zeroed);
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(read_range(&path, 4096, 512), [0xa5u8; 512]);
        assert_eq!(read_range(&path, 4096 + 512, 512), vec![0u8; 512]);
        assert_eq!(read_range(&path, 4096 + 1024, 512), [0xa5u8; 512]);
        assert_eq!(read_range(&path, (64 << 10) - 1024, 512), [0xa5u8; 512]);
        assert_eq!(read_range(&path, (64 << 10) - 512, 1024), vec![0u8; 1024]);
        assert_eq!(read_range(&path, (64 << 10) + 512, 512), [0xa5u8; 512]);
        // Nothing was written where nothing was asked for.
        assert_eq!(read_range(&path, 8192, 512), [0xa5u8; 512]);
        std::fs::remove_file(&path).unwrap();
    }

    /// A range whose end overflows `u64` is rejected before any syscall — the
    /// public helpers must not lean on the device having checked first.
    #[test]
    fn overflowing_ranges_are_refused_not_wrapped() {
        let dir = temp_dir("ops-overflow");
        let path = filled_image(&dir, "overflow.raw", 512 << 10, 4096);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(punch_hole(&file, u64::MAX, 512).is_err());
        assert!(write_zeroes(&file, u64::MAX - 1, 4, true).is_err());
        assert!(write_zeroes(&file, u64::MAX, 1, false).is_err());
        // The file is untouched.
        drop(file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 512 << 10);
        assert_eq!(read_range(&path, 0, 16), [0xa5u8; 16]);
        std::fs::remove_file(&path).unwrap();
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
