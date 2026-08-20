//! RAW file disk backend (backlog MVP-405/408).
//!
//! A plain `std::fs::File` accessed positionally, so the device never has to
//! keep a cursor consistent across interleaved requests. Read-only images
//! (MVP-408, installer ISOs) are opened without write access *and* refuse
//! writes in software, so a negotiation bug cannot corrupt them.

use std::fs::File;
use std::path::Path;

use disk_image::PunchOutcome;

use crate::request::{BlockError, SECTOR_SIZE};

/// Which of the two reclaim commands a log line is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReclaimKind {
    Discard,
    WriteZeroes,
}

impl ReclaimKind {
    fn as_str(self) -> &'static str {
        match self {
            ReclaimKind::Discard => "discard",
            ReclaimKind::WriteZeroes => "write-zeroes",
        }
    }
}

/// An opened RAW disk image.
#[derive(Debug)]
pub struct RawDisk {
    file: File,
    capacity_sectors: u64,
    read_only: bool,
    /// Set once the reclaim mechanism has been reported for each command, so a
    /// guest `fstrim` produces one log line rather than one per range.
    discard_logged: bool,
    write_zeroes_logged: bool,
}

impl RawDisk {
    /// Opens `path` as a RAW image. `writable` false gives a read-only device
    /// (`VIRTIO_BLK_F_RO`).
    pub fn open(path: &Path, writable: bool) -> Result<Self, BlockError> {
        let file = File::options()
            .read(true)
            .write(writable)
            .open(path)
            .map_err(|source| BlockError::Open {
                path: path.display().to_string(),
                source,
            })?;
        let len = file
            .metadata()
            .map_err(|source| BlockError::Open {
                path: path.display().to_string(),
                source,
            })?
            .len();
        if len == 0 {
            return Err(BlockError::EmptyImage);
        }
        if len % SECTOR_SIZE != 0 {
            return Err(BlockError::UnalignedImage {
                path: path.display().to_string(),
                len,
            });
        }
        Ok(Self {
            file,
            capacity_sectors: len / SECTOR_SIZE,
            read_only: !writable,
            discard_logged: false,
            write_zeroes_logged: false,
        })
    }

    /// Wraps an already opened file, for tests and for callers that create the
    /// image themselves.
    pub fn from_file(file: File, writable: bool) -> Result<Self, BlockError> {
        let len = file
            .metadata()
            .map_err(|source| BlockError::Io { offset: 0, source })?
            .len();
        if len == 0 {
            return Err(BlockError::EmptyImage);
        }
        if len % SECTOR_SIZE != 0 {
            return Err(BlockError::UnalignedImage {
                path: "<file>".into(),
                len,
            });
        }
        Ok(Self {
            file,
            capacity_sectors: len / SECTOR_SIZE,
            read_only: !writable,
            discard_logged: false,
            write_zeroes_logged: false,
        })
    }

    /// Disk size in 512-byte sectors — the value the guest sees in the
    /// virtio-blk config space `capacity` field.
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<(), BlockError> {
        positional_read(&mut self.file, buf, offset)
            .map_err(|source| BlockError::Io { offset, source })
    }

    pub fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        positional_write(&mut self.file, buf, offset)
            .map_err(|source| BlockError::Io { offset, source })
    }

    /// `VIRTIO_BLK_T_DISCARD`: the guest has stopped needing these bytes, so
    /// give them back to the host filesystem.
    ///
    /// A discard is a hint. Where the filesystem cannot punch holes this
    /// changes nothing and still reports success, which the spec allows and the
    /// guest cannot tell apart from a device that reclaimed nothing useful. The
    /// one thing it must never do is *pretend* — hence the log line naming the
    /// mechanism, once per disk.
    pub fn discard(&mut self, offset: u64, len: u64) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        let outcome = disk_image::punch_hole(&self.file, offset, len).map_err(|source| {
            BlockError::Discard {
                offset,
                len,
                source,
            }
        })?;
        self.report(ReclaimKind::Discard, outcome);
        Ok(())
    }

    /// `VIRTIO_BLK_T_WRITE_ZEROES`: these bytes must read back as zeros
    /// afterwards, whatever the host filesystem can or cannot deallocate.
    ///
    /// `unmap` is the guest's permission to reclaim the space as well; without
    /// it the blocks stay provisioned and real zeros are written.
    pub fn write_zeroes(&mut self, offset: u64, len: u64, unmap: bool) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        let outcome =
            disk_image::write_zeroes(&self.file, offset, len, unmap).map_err(|source| {
                BlockError::Discard {
                    offset,
                    len,
                    source,
                }
            })?;
        self.report(ReclaimKind::WriteZeroes, outcome);
        Ok(())
    }

    /// Logs the mechanism the host actually used — once per disk per command,
    /// not once per request: on a busy `fstrim` this is thousands of calls.
    fn report(&mut self, kind: ReclaimKind, outcome: PunchOutcome) {
        let logged = match kind {
            ReclaimKind::Discard => &mut self.discard_logged,
            ReclaimKind::WriteZeroes => &mut self.write_zeroes_logged,
        };
        if *logged {
            return;
        }
        *logged = true;
        let command = kind.as_str();
        match outcome {
            PunchOutcome::Unsupported => tracing::warn!(
                command,
                "host filesystem cannot punch holes; discards will not reclaim space \
                 (move the image to ext4/xfs/btrfs or NTFS to get reclaim)"
            ),
            other => tracing::info!(
                command,
                mechanism = other.as_str(),
                "virtio-blk reclaim path chosen"
            ),
        }
    }

    /// `VIRTIO_BLK_T_FLUSH`: guarantee everything written so far is on stable
    /// storage. A no-op for read-only images.
    pub fn flush(&mut self) -> Result<(), BlockError> {
        if self.read_only {
            return Ok(());
        }
        self.file
            .sync_all()
            .map_err(|source| BlockError::Flush { source })
    }
}

// Positional I/O. On unix `pread`/`pwrite` need no cursor at all; elsewhere we
// seek explicitly, which is why the helpers take `&mut File`.

#[cfg(unix)]
fn positional_read(file: &mut File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn positional_write(file: &mut File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(not(unix))]
fn positional_read(file: &mut File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

#[cfg(not(unix))]
fn positional_write(file: &mut File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_image(name: &str, sectors: u64) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("entangled-blk-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let file = File::options()
            .create_new(true)
            .write(true)
            .open(&path)
            .expect("create image");
        file.set_len(sectors * SECTOR_SIZE).expect("size image");
        path
    }

    #[test]
    fn capacity_is_reported_in_sectors() {
        let path = temp_image("capacity.raw", 8);
        let disk = RawDisk::open(&path, true).expect("open");
        assert_eq!(disk.capacity_sectors(), 8);
        assert!(!disk.is_read_only());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn positional_round_trip() {
        let path = temp_image("roundtrip.raw", 4);
        let mut disk = RawDisk::open(&path, true).expect("open");
        let payload = [0x5au8; 512];
        assert!(disk.write_at(&payload, 512).is_ok());
        assert!(disk.flush().is_ok());

        let mut back = [0u8; 512];
        assert!(disk.read_at(&mut back, 512).is_ok());
        assert_eq!(back, payload);

        // Sector 0 is untouched.
        assert!(disk.read_at(&mut back, 0).is_ok());
        assert_eq!(back, [0u8; 512]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_only_disks_refuse_writes() {
        let path = temp_image("readonly.raw", 2);
        let mut disk = RawDisk::open(&path, false).expect("open");
        assert!(disk.is_read_only());
        assert!(matches!(
            disk.write_at(&[0u8; 512], 0),
            Err(BlockError::ReadOnly)
        ));
        // Flush is a harmless no-op.
        assert!(disk.flush().is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reads_past_the_end_are_io_errors_not_panics() {
        let path = temp_image("short.raw", 1);
        let mut disk = RawDisk::open(&path, true).expect("open");
        assert!(matches!(
            disk.read_at(&mut [0u8; 512], 512),
            Err(BlockError::Io { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn misaligned_and_empty_images_are_rejected() {
        let dir = std::env::temp_dir().join("entangled-blk-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");

        let odd = dir.join("odd.raw");
        let _ = std::fs::remove_file(&odd);
        std::fs::write(&odd, [0u8; 100]).expect("write odd image");
        assert!(matches!(
            RawDisk::open(&odd, true),
            Err(BlockError::UnalignedImage { .. })
        ));

        let empty = dir.join("empty.raw");
        let _ = std::fs::remove_file(&empty);
        std::fs::write(&empty, []).expect("write empty image");
        assert!(matches!(
            RawDisk::open(&empty, true),
            Err(BlockError::EmptyImage)
        ));

        let missing = dir.join("does-not-exist.raw");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            RawDisk::open(&missing, true),
            Err(BlockError::Open { .. })
        ));

        let _ = std::fs::remove_file(&odd);
        let _ = std::fs::remove_file(&empty);
    }
}
