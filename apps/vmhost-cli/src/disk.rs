//! `vmhost disk create` — sparse RAW image creation (backlog MVP-409/1001).

use std::io;
use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiskError {
    #[error("invalid size '{0}': use e.g. 32G, 512M or a byte count")]
    InvalidSize(String),

    #[error("size must be a positive multiple of 512 bytes, got {0}")]
    BadAlignment(u64),

    #[error("refusing to overwrite existing file {0}")]
    Exists(String),

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
    // set_len produces a sparse file on ext4/xfs/btrfs and NTFS alike.
    file.set_len(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join("vmhost-disk-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.raw");
        let _ = std::fs::remove_file(&path);

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
    }
}
