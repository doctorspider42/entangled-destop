//! Narrow reader for Canonical's signed ARM64 raw-rootfs tar.gz format.
//! Only the first, exactly named regular member is extracted; archive paths
//! never choose host destinations. Callers authenticate the archive first.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MEMBER: &str = "noble-server-cloudimg-arm64.img";
const MAX_ROOTFS_BYTES: u64 = 16 << 30;
const MAX_TRAILER_BYTES: u64 = 512 << 20;
const COPY_BYTES: usize = 64 << 10;
const MAX_BOOT_FILE_BYTES: u64 = 256 << 20;
const MAX_BOOT_ENTRIES: usize = 4096;

fn octal(field: &[u8]) -> Result<u64, String> {
    let text = std::str::from_utf8(field)
        .map_err(|_| "non-ASCII tar number")?
        .trim_matches(|c| c == '\0' || c == ' ');
    u64::from_str_radix(text, 8).map_err(|_| "invalid or overflowing tar number".into())
}

fn rootfs_size(header: &[u8; 512]) -> Result<u64, String> {
    let checksum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, byte)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    if octal(&header[148..156])? != checksum {
        return Err("Ubuntu archive header checksum mismatch".into());
    }
    let name = header[..100]
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default();
    if name != MEMBER.as_bytes()
        || header[345..500].iter().any(|byte| *byte != 0)
        || !matches!(header[156], 0 | b'0')
    {
        return Err("Ubuntu archive must begin with its named regular raw-rootfs member".into());
    }
    let size = octal(&header[124..136])?;
    if size == 0 || size > MAX_ROOTFS_BYTES || size % 512 != 0 {
        return Err("Ubuntu raw-rootfs length is invalid or exceeds 16 GiB".into());
    }
    Ok(size)
}

/// Creates a new sparse image, retaining the raw ext4 layout and growing only
/// the enclosing disk. resize2fs must grow the filesystem inside the guest.
/// A failed extraction removes only the file created by this invocation.
pub fn extract_rootfs(archive: &Path, destination: &Path, disk_bytes: u64) -> Result<u64, String> {
    let mut gzip = flate2::read::GzDecoder::new(File::open(archive).map_err(|e| e.to_string())?);
    let mut header = [0; 512];
    gzip.read_exact(&mut header)
        .map_err(|e| format!("Ubuntu archive header: {e}"))?;
    let size = rootfs_size(&header)?;
    if disk_bytes < size || disk_bytes % 512 != 0 {
        return Err(format!(
            "disk must be at least {size} bytes and sector aligned"
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| format!("cannot create {}: {e}", destination.display()))?;
    let _ = disk_image::mark_sparse(&output);
    let result = (|| {
        let mut remaining = size;
        let mut bytes = vec![0; COPY_BYTES];
        while remaining != 0 {
            let count = remaining.min(COPY_BYTES as u64) as usize;
            gzip.read_exact(&mut bytes[..count])
                .map_err(|e| format!("Ubuntu rootfs: {e}"))?;
            if bytes[..count].iter().all(|byte| *byte == 0) {
                output
                    .seek(SeekFrom::Current(count as i64))
                    .map_err(|e| e.to_string())?;
            } else {
                output
                    .write_all(&bytes[..count])
                    .map_err(|e| e.to_string())?;
            }
            remaining -= count as u64;
        }
        // Drain the bounded remainder to check gzip CRC/truncation. Nothing
        // from additional archive members is ever written to the host.
        let mut trailer = 0u64;
        loop {
            let count = gzip
                .read(&mut bytes)
                .map_err(|e| format!("Ubuntu archive trailer: {e}"))?;
            if count == 0 {
                break;
            }
            trailer += count as u64;
            if trailer > MAX_TRAILER_BYTES {
                return Err("Ubuntu archive trailer exceeds 512 MiB".into());
            }
        }
        output.set_len(disk_bytes).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        Ok(size)
    })();
    drop(output);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

/// Read a matching kernel/initrd pair without mounting or writing guest ext4.
/// The output directory must be new, so a previous VM's boot files are safe.
pub fn extract_boot(image: &Path, destination: &Path) -> Result<(PathBuf, PathBuf), String> {
    let fs = ext4_view::Ext4::load_from_path(image).map_err(|e| e.to_string())?;
    let mut kernels = Vec::new();
    for (index, entry) in fs.read_dir("/boot").map_err(|e| e.to_string())?.enumerate() {
        if index >= MAX_BOOT_ENTRIES {
            return Err("guest /boot has too many entries".into());
        }
        let name = entry
            .map_err(|e| e.to_string())?
            .path()
            .display()
            .to_string();
        if name.starts_with("/boot/vmlinuz-") {
            kernels.push(name);
        }
    }
    kernels.sort();
    let kernel = kernels
        .last()
        .ok_or("Ubuntu rootfs contains no boot kernel")?;
    let version = kernel
        .strip_prefix("/boot/vmlinuz-")
        .ok_or("invalid kernel filename")?;
    let sources = [kernel.clone(), format!("/boot/initrd.img-{version}")];
    let mut files = Vec::new();
    for source in sources {
        let size = fs
            .metadata(source.as_str())
            .map_err(|e| e.to_string())?
            .len();
        if size == 0 || size > MAX_BOOT_FILE_BYTES {
            return Err("Ubuntu boot file exceeds 256 MiB or is empty".into());
        }
        files.push(fs.read(source.as_str()).map_err(|e| e.to_string())?);
    }
    std::fs::create_dir(destination).map_err(|e| format!("cannot create boot directory: {e}"))?;
    let paths = (destination.join("vmlinuz"), destination.join("initrd.img"));
    let result = (|| {
        for (path, bytes) in [(&paths.0, &files[0]), (&paths.1, &files[1])] {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        Ok(paths.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "entangled-ubuntu-image-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn header(name: &str, kind: u8, size: u64) -> [u8; 512] {
        let mut h = [0; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[124..136].fill(b'0');
        let digits = format!("{size:o}");
        h[136 - digits.len()..136].copy_from_slice(digits.as_bytes());
        h[156] = kind;
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }
    fn archive(path: &Path, payload: &[u8]) {
        let mut gzip =
            flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        gzip.write_all(&header(MEMBER, b'0', payload.len() as u64))
            .unwrap();
        gzip.write_all(payload).unwrap();
        gzip.write_all(&[0; 1024]).unwrap();
        gzip.finish().unwrap();
    }
    #[test]
    fn archive_paths_links_bad_checksums_and_oversized_members_are_refused() {
        assert!(rootfs_size(&header("../../outside", b'0', 512)).is_err());
        assert!(rootfs_size(&header(MEMBER, b'2', 512)).is_err());
        assert!(rootfs_size(&header(MEMBER, b'0', MAX_ROOTFS_BYTES + 512)).is_err());
        let mut bad = header(MEMBER, b'0', 512);
        bad[0] ^= 1;
        assert!(rootfs_size(&bad).is_err());
    }
    #[test]
    fn sparse_extraction_retains_bytes_grows_only_the_disk_and_refuses_overwrite() {
        let temp = Temp::new();
        let archive_path = temp.0.join("rootfs.tar.gz");
        let disk = temp.0.join("disk.raw");
        let mut payload = vec![0; 1 << 20];
        payload[..5].copy_from_slice(b"hello");
        archive(&archive_path, &payload);
        assert_eq!(
            extract_rootfs(&archive_path, &disk, 2 << 20).unwrap(),
            1 << 20
        );
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 2 << 20);
        let result = std::fs::read(&disk).unwrap();
        assert_eq!(&result[..payload.len()], payload);
        assert!(result[payload.len()..].iter().all(|b| *b == 0));
        assert!(extract_rootfs(&archive_path, &disk, 2 << 20).is_err());
        assert_eq!(std::fs::read(&disk).unwrap(), result);
    }
    #[test]
    fn bad_gzip_crc_removes_only_the_new_image() {
        let temp = Temp::new();
        let source = temp.0.join("archive.gz");
        let target = temp.0.join("disk.raw");
        archive(&source, &[0; 512]);
        let mut bytes = std::fs::read(&source).unwrap();
        let checksum = bytes.len() - 8;
        bytes[checksum] ^= 1;
        std::fs::write(&source, bytes).unwrap();
        assert!(extract_rootfs(&source, &target, 1024).is_err());
        assert!(!target.exists());
        assert!(source.exists());
    }
    #[test]
    #[ignore = "real verified Ubuntu archive; set ENTANGLED_UBUNTU_CLOUD_ARCHIVE"]
    fn actual_verified_cloud_archive_yields_matching_boot_files() {
        let source = PathBuf::from(
            std::env::var_os("ENTANGLED_UBUNTU_CLOUD_ARCHIVE").expect("archive path"),
        );
        assert_eq!(
            crate::artifact::digest_of(&source).unwrap(),
            "1800eb56b6b839a08c020551e16fbd8c95b2772452ecce3737a9ad25fe594126"
        );
        let temp = Temp::new();
        let image = temp.0.join("ubuntu.raw");
        assert_eq!(
            extract_rootfs(&source, &image, 8 << 30).unwrap(),
            2084569088
        );
        let (kernel, initrd) = extract_boot(&image, &temp.0.join("boot")).unwrap();
        assert_eq!(std::fs::metadata(kernel).unwrap().len(), 18585914);
        assert_eq!(std::fs::metadata(initrd).unwrap().len(), 29286178);
    }
}
