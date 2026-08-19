//! The cloud-init NoCloud seed volume (backlog UEFI-1804).
//!
//! `entangled install ubuntu` hands subiquity its autoinstall configuration on a
//! small extra virtio-blk volume. cloud-init finds it by *filesystem label*:
//! `DataSourceNoCloud` intersects the set of `TYPE=vfat`/`TYPE=iso9660` devices
//! with the set whose label is `cidata` or `CIDATA`, and requires `user-data`
//! and `meta-data` at the volume root.
//!
//! # Why ISO9660, written by hand
//!
//! * The verified ISO stays read-only and untouched. The seed is a separate
//!   volume, so nothing has to be repacked and the media's provenance survives.
//! * ISO9660 is the smaller of the two acceptable filesystems to *write*: a
//!   read-only descriptor, a one-record path table, one directory sector and the
//!   files. A FAT writer needs a BPB, two allocation tables, cluster arithmetic
//!   and the FAT12/16 boundary rules to be right, all so a 44-byte YAML file can
//!   be read once.
//! * No new dependency, and therefore no `cargo deny` question. The whole format
//!   used here is 200 lines and every field is asserted by the tests below.
//!
//! # File names
//!
//! ISO9660 records names in upper case with a version suffix — `USER-DATA.;1`.
//! Linux's `isofs` translates that back on mount (`isofs_name_translate`:
//! lower-case, drop a trailing `.;1`), so the seed presents exactly `user-data`
//! and `meta-data` to cloud-init without needing Rock Ridge or Joliet extensions.
//! That translation is the one thing here that depends on the *reader* rather
//! than the spec, so [`tests::the_names_translate_the_way_isofs_does`] models it.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// ISO9660 logical sector size.
const SECTOR: usize = 2048;

/// The 16 sectors before the first volume descriptor: on a bootable image this
/// is where an MBR would live. Here it is zeroes, but it must be present — the
/// volume descriptors are addressed absolutely.
const SYSTEM_AREA_SECTORS: usize = 16;

/// The label cloud-init looks for. Upper case: `DataSourceNoCloud` tries
/// `label.upper()` and `label.lower()`, and nothing in between, so a mixed-case
/// label would silently not be a seed.
pub const SEED_LABEL: &str = "CIDATA";

/// Minimum image size. Nothing requires it; it keeps the volume a comfortable
/// multiple of the 512-byte sectors virtio-blk reports and leaves room for a
/// larger user-data without changing the shape.
const MIN_IMAGE_BYTES: usize = 64 * 1024;

/// The autoinstall configuration, compiled in so `--auto` works from any
/// working directory (the same reason the Debian preseed is compiled in).
const AUTOINSTALL: &str = include_str!("../../../assets/autoinstall/ubuntu-server.yaml");

/// Placeholder substituted with the VM name.
const HOSTNAME_PLACEHOLDER: &str = "@HOSTNAME@";

#[derive(Debug, Error)]
pub enum SeedError {
    #[error("cannot write the seed volume {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot read the autoinstall file {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the autoinstall configuration is {len} bytes; the seed volume holds files of at \
         most {max} bytes"
    )]
    TooLarge { len: usize, max: usize },

    #[error(
        "the autoinstall configuration does not start with the '#cloud-config' line. \
         Delivered as cloud-init user-data, subiquity's directives must sit under a \
         top-level 'autoinstall:' key of a cloud-config document — a bare 'version: 1' \
         document is only valid as autoinstall.yaml on the installation medium, which \
         is read-only verified media here"
    )]
    NotCloudConfig,
}

/// One file per sector run; the seed only ever has two, so the cap is generous
/// and exists to bound the directory record sector.
const MAX_FILE_BYTES: usize = 8 * SECTOR;

/// What went onto the seed volume, for logging and for the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub path: PathBuf,
    pub bytes: usize,
    /// The user-data actually written (after hostname substitution).
    pub user_data: String,
}

/// Builds the autoinstall user-data for `hostname`, either from the compiled-in
/// profile or from a caller-supplied file.
pub fn user_data(hostname: &str, custom: Option<&Path>) -> Result<String, SeedError> {
    let template = match custom {
        Some(path) => std::fs::read_to_string(path).map_err(|source| SeedError::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?,
        None => AUTOINSTALL.to_string(),
    };
    // A file that is not cloud-config would be *ignored*, and the installer
    // would sit at its first interactive screen with no explanation. Refuse it
    // here instead.
    if !template.trim_start().starts_with("#cloud-config") {
        return Err(SeedError::NotCloudConfig);
    }
    Ok(template.replace(HOSTNAME_PLACEHOLDER, hostname))
}

/// Writes a NoCloud seed volume at `path` carrying `user_data`.
///
/// `instance_id` becomes the seed's `meta-data`. cloud-init treats a change of
/// instance id as a new instance, so a fresh install gets a fresh one.
pub fn write(path: &Path, user_data: &str, instance_id: &str) -> Result<Seed, SeedError> {
    let meta_data = format!("instance-id: {instance_id}\nlocal-hostname: {instance_id}\n");
    let image = build_iso9660(&[
        ("USER-DATA.;1", user_data.as_bytes()),
        ("META-DATA.;1", meta_data.as_bytes()),
    ])?;
    std::fs::write(path, &image).map_err(|source| SeedError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    tracing::info!(
        path = %path.display(),
        bytes = image.len(),
        label = SEED_LABEL,
        "wrote the cloud-init NoCloud seed volume"
    );
    Ok(Seed {
        path: path.to_path_buf(),
        bytes: image.len(),
        user_data: user_data.to_string(),
    })
}

/// Assembles a minimal single-directory ISO9660 image.
///
/// Layout, in logical sectors:
///
/// | Sector | Content |
/// |---|---|
/// | 0..16 | system area (zeroes) |
/// | 16 | primary volume descriptor |
/// | 17 | volume descriptor set terminator |
/// | 18 | type-L path table (one record: the root) |
/// | 19 | type-M path table (the same, big-endian) |
/// | 20 | root directory: `.`, `..`, then one record per file |
/// | 21.. | file contents, one sector run each |
fn build_iso9660(files: &[(&str, &[u8])]) -> Result<Vec<u8>, SeedError> {
    for (_, data) in files {
        if data.len() > MAX_FILE_BYTES {
            return Err(SeedError::TooLarge {
                len: data.len(),
                max: MAX_FILE_BYTES,
            });
        }
    }

    const PVD_LBA: u32 = SYSTEM_AREA_SECTORS as u32;
    const TERMINATOR_LBA: u32 = PVD_LBA + 1;
    const PATH_TABLE_L_LBA: u32 = TERMINATOR_LBA + 1;
    const PATH_TABLE_M_LBA: u32 = PATH_TABLE_L_LBA + 1;
    const ROOT_DIR_LBA: u32 = PATH_TABLE_M_LBA + 1;
    const FIRST_FILE_LBA: u32 = ROOT_DIR_LBA + 1;

    // Where each file's extent starts, and how long the whole volume is.
    let mut extents = Vec::with_capacity(files.len());
    let mut next_lba = FIRST_FILE_LBA;
    for (_, data) in files {
        extents.push(next_lba);
        next_lba += sectors_for(data.len());
    }
    let mut total_sectors = next_lba;
    let min_sectors = MIN_IMAGE_BYTES.div_ceil(SECTOR) as u32;
    total_sectors = total_sectors.max(min_sectors);

    // ---- root directory ----
    let mut root_dir = Vec::with_capacity(SECTOR);
    // "." and "..", both pointing at the root itself (the root is its own parent).
    root_dir.extend_from_slice(&directory_record("\0", ROOT_DIR_LBA, SECTOR as u32, true));
    root_dir.extend_from_slice(&directory_record(
        "\u{1}",
        ROOT_DIR_LBA,
        SECTOR as u32,
        true,
    ));
    for ((name, data), &lba) in files.iter().zip(&extents) {
        root_dir.extend_from_slice(&directory_record(name, lba, data.len() as u32, false));
    }
    // The directory must fit one sector; with two files it uses ~130 bytes.
    debug_assert!(
        root_dir.len() <= SECTOR,
        "root directory overflows a sector"
    );
    root_dir.resize(SECTOR, 0);

    // ---- path table (one record: the root directory) ----
    // 8 bytes of header plus a 1-byte identifier padded to even = 10.
    let path_table_size = 10u32;
    let mut path_l = Vec::with_capacity(SECTOR);
    path_l.push(1); // length of directory identifier
    path_l.push(0); // extended attribute record length
    path_l.extend_from_slice(&ROOT_DIR_LBA.to_le_bytes());
    path_l.extend_from_slice(&1u16.to_le_bytes()); // parent directory number
    path_l.push(0); // identifier: a single zero byte means "root"
    path_l.push(0); // pad to even
    path_l.resize(SECTOR, 0);

    let mut path_m = Vec::with_capacity(SECTOR);
    path_m.push(1);
    path_m.push(0);
    path_m.extend_from_slice(&ROOT_DIR_LBA.to_be_bytes());
    path_m.extend_from_slice(&1u16.to_be_bytes());
    path_m.push(0);
    path_m.push(0);
    path_m.resize(SECTOR, 0);

    // ---- primary volume descriptor ----
    let mut pvd = vec![0u8; SECTOR];
    pvd[0] = 1; // volume descriptor type: primary
    pvd[1..6].copy_from_slice(b"CD001"); // standard identifier — what blkid matches
    pvd[6] = 1; // version
    fill_a_chars(&mut pvd[8..40], ""); // system identifier
    fill_a_chars(&mut pvd[40..72], SEED_LABEL); // volume identifier: the LABEL
    both_endian32(&mut pvd[80..88], total_sectors);
    both_endian16(&mut pvd[120..124], 1); // volume set size
    both_endian16(&mut pvd[124..128], 1); // volume sequence number
    both_endian16(&mut pvd[128..132], SECTOR as u16);
    both_endian32(&mut pvd[132..140], path_table_size);
    pvd[140..144].copy_from_slice(&PATH_TABLE_L_LBA.to_le_bytes());
    // 144..148 optional type-L path table: none.
    pvd[148..152].copy_from_slice(&PATH_TABLE_M_LBA.to_be_bytes());
    // 152..156 optional type-M path table: none.
    let root_record = directory_record("\0", ROOT_DIR_LBA, SECTOR as u32, true);
    pvd[156..156 + root_record.len()].copy_from_slice(&root_record);
    for range in [190..318, 318..446, 446..574, 574..702] {
        fill_a_chars(&mut pvd[range], "");
    }
    for range in [702..739, 739..776, 776..813] {
        fill_a_chars(&mut pvd[range], "");
    }
    // Dates: "0000000000000000" + 0 means "not specified", which is legal and
    // keeps the image byte-identical from one run to the next. A timestamp here
    // would make the seed unreproducible for no reader's benefit.
    for range in [813..830, 830..847, 847..864, 864..881] {
        pvd[range.clone()].fill(b'0');
        pvd[range.end - 1] = 0;
    }
    pvd[881] = 1; // file structure version

    // ---- terminator ----
    let mut terminator = vec![0u8; SECTOR];
    terminator[0] = 0xff;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;

    // ---- assemble ----
    let mut image = vec![0u8; total_sectors as usize * SECTOR];
    let put = |image: &mut Vec<u8>, lba: u32, bytes: &[u8]| {
        let at = lba as usize * SECTOR;
        image[at..at + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut image, PVD_LBA, &pvd);
    put(&mut image, TERMINATOR_LBA, &terminator);
    put(&mut image, PATH_TABLE_L_LBA, &path_l);
    put(&mut image, PATH_TABLE_M_LBA, &path_m);
    put(&mut image, ROOT_DIR_LBA, &root_dir);
    for ((_, data), &lba) in files.iter().zip(&extents) {
        put(&mut image, lba, data);
    }
    Ok(image)
}

/// One ISO9660 directory record. `name` is the recorded identifier: a single
/// `\0` for ".", `\u{1}` for "..", and `NAME.EXT;1` for a file.
fn directory_record(name: &str, extent_lba: u32, length: u32, directory: bool) -> Vec<u8> {
    let id = name.as_bytes();
    let mut record = Vec::with_capacity(33 + id.len() + 1);
    record.push(0); // length, filled in below
    record.push(0); // extended attribute record length
    let mut both = [0u8; 8];
    both_endian32(&mut both, extent_lba);
    record.extend_from_slice(&both);
    both_endian32(&mut both, length);
    record.extend_from_slice(&both);
    // Recording date and time: 7 bytes, all zero except a plausible year. Zero
    // is accepted by isofs and keeps the image reproducible.
    record.extend_from_slice(&[0, 1, 1, 0, 0, 0, 0]);
    record.push(if directory { 0x02 } else { 0x00 }); // file flags
    record.push(0); // file unit size (not interleaved)
    record.push(0); // interleave gap size
    let mut vol_seq = [0u8; 4];
    both_endian16(&mut vol_seq, 1);
    record.extend_from_slice(&vol_seq);
    record.push(id.len() as u8);
    record.extend_from_slice(id);
    if record.len() % 2 != 0 {
        record.push(0); // records are even-length
    }
    // Length fits in a byte: identifiers here are at most 12 characters.
    record[0] = record.len() as u8;
    record
}

/// Writes `value` as a little-endian *and* big-endian pair, as ISO9660 stores
/// every numeric field.
fn both_endian32(out: &mut [u8], value: u32) {
    out[..4].copy_from_slice(&value.to_le_bytes());
    out[4..8].copy_from_slice(&value.to_be_bytes());
}

fn both_endian16(out: &mut [u8], value: u16) {
    out[..2].copy_from_slice(&value.to_le_bytes());
    out[2..4].copy_from_slice(&value.to_be_bytes());
}

/// Fills a fixed-width text field with `text`, space padded, upper case: the
/// "a-characters" ISO9660 allows in volume identifiers.
fn fill_a_chars(field: &mut [u8], text: &str) {
    field.fill(b' ');
    for (slot, byte) in field.iter_mut().zip(text.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
}

fn sectors_for(len: usize) -> u32 {
    len.div_ceil(SECTOR).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_image() -> Vec<u8> {
        build_iso9660(&[
            (
                "USER-DATA.;1",
                b"#cloud-config\nautoinstall:\n  version: 1\n",
            ),
            ("META-DATA.;1", b"instance-id: test\n"),
        ])
        .unwrap()
    }

    /// What `blkid` reads to decide this is a seed at all: the `CD001` standard
    /// identifier at byte 0x8001 (TYPE=iso9660) and the volume identifier
    /// (LABEL=CIDATA). Get either wrong and cloud-init never looks inside.
    #[test]
    fn blkid_sees_an_iso9660_volume_labelled_cidata() {
        let image = seed_image();
        assert_eq!(&image[0x8000..0x8006], b"\x01CD001");
        assert_eq!(&image[0x8001..0x8006], b"CD001");
        let label = &image[0x8000 + 40..0x8000 + 72];
        assert_eq!(&label[..6], b"CIDATA");
        assert!(
            label[6..].iter().all(|&b| b == b' '),
            "label is space padded"
        );
        // The first 32 KiB are the system area, and they are empty: nothing here
        // is bootable and nothing pretends to be.
        assert!(image[..0x8000].iter().all(|&b| b == 0));
        // Descriptor set terminator immediately after the PVD.
        assert_eq!(&image[0x8800..0x8806], b"\xffCD001");
    }

    /// The volume must describe its own size, or a reader that trusts the
    /// descriptor (rather than the device capacity) walks off the end.
    #[test]
    fn the_volume_size_matches_the_image() {
        let image = seed_image();
        let pvd = &image[0x8000..0x8800];
        let sectors_le = u32::from_le_bytes([pvd[80], pvd[81], pvd[82], pvd[83]]);
        let sectors_be = u32::from_be_bytes([pvd[84], pvd[85], pvd[86], pvd[87]]);
        assert_eq!(sectors_le, sectors_be, "both-endian fields must agree");
        assert_eq!(sectors_le as usize * SECTOR, image.len());
        assert_eq!(
            u16::from_le_bytes([pvd[128], pvd[129]]),
            SECTOR as u16,
            "logical block size"
        );
        assert_eq!(pvd[881], 1, "file structure version");
        // At least 64 KiB, and a whole number of 512-byte virtio-blk sectors.
        assert!(image.len() >= MIN_IMAGE_BYTES);
        assert_eq!(image.len() % 512, 0);
    }

    /// The root directory holds `.`, `..` and one record per file, and each
    /// record points at an extent that really contains the file.
    #[test]
    fn the_directory_records_point_at_the_files() {
        let image = seed_image();
        let root = &image[20 * SECTOR..21 * SECTOR];

        let mut at = 0usize;
        let mut found = Vec::new();
        while at < root.len() && root[at] != 0 {
            let len = root[at] as usize;
            let record = &root[at..at + len];
            let extent = u32::from_le_bytes([record[2], record[3], record[4], record[5]]) as usize;
            let size =
                u32::from_le_bytes([record[10], record[11], record[12], record[13]]) as usize;
            let id_len = record[32] as usize;
            let id = String::from_utf8_lossy(&record[33..33 + id_len]).into_owned();
            let is_dir = record[25] & 0x02 != 0;
            assert_eq!(len % 2, 0, "records are even-length");
            found.push((id, extent, size, is_dir));
            at += len;
        }

        assert_eq!(found.len(), 4, "'.', '..' and two files: {found:?}");
        assert!(
            found[0].3 && found[1].3,
            "the first two records are directories"
        );
        let (id, extent, size, is_dir) = &found[2];
        assert_eq!(id, "USER-DATA.;1");
        assert!(!is_dir);
        let data = &image[extent * SECTOR..extent * SECTOR + size];
        assert!(std::str::from_utf8(data)
            .unwrap()
            .starts_with("#cloud-config"));
        let (id, extent, size, _) = &found[3];
        assert_eq!(id, "META-DATA.;1");
        assert_eq!(
            std::str::from_utf8(&image[extent * SECTOR..extent * SECTOR + size]).unwrap(),
            "instance-id: test\n"
        );
        // The two files are in different sectors: an extent is a sector run.
        assert_ne!(found[2].1, found[3].1);
    }

    /// cloud-init requires the names `user-data` and `meta-data`. What it
    /// actually sees is whatever `isofs` makes of the recorded identifiers, so
    /// this models `isofs_name_translate`: lower-case, and drop a trailing
    /// `.;1`. If this ever fails, the seed needs Rock Ridge names instead.
    #[test]
    fn the_names_translate_the_way_isofs_does() {
        fn isofs_name_translate(recorded: &str) -> String {
            let bytes = recorded.as_bytes();
            let mut out = String::new();
            for (i, &c) in bytes.iter().enumerate() {
                if c == 0 {
                    break;
                }
                // Drop a trailing ".;1", then a trailing ";1", then map ';' to '.'.
                if c == b'.' && i + 3 == bytes.len() && &bytes[i + 1..] == b";1" {
                    break;
                }
                if c == b';' && i + 2 == bytes.len() && bytes[i + 1] == b'1' {
                    break;
                }
                out.push(if c == b';' {
                    '.'
                } else {
                    c.to_ascii_lowercase() as char
                });
            }
            out
        }
        assert_eq!(isofs_name_translate("USER-DATA.;1"), "user-data");
        assert_eq!(isofs_name_translate("META-DATA.;1"), "meta-data");
    }

    /// The compiled-in profile must be the shape a NoCloud seed needs, and the
    /// hostname must actually be substituted — an unsubstituted placeholder
    /// would become the installed system's hostname.
    #[test]
    fn the_builtin_autoinstall_is_cloud_config_with_our_hostname() {
        let text = user_data("ubuntu-demo", None).unwrap();
        assert!(text.starts_with("#cloud-config"));
        assert!(!text.contains(HOSTNAME_PLACEHOLDER));
        assert!(text.contains("hostname: \"ubuntu-demo\""));
        // The keys the install depends on, each for a stated reason.
        for needle in [
            "autoinstall:",       // the cloud-init delivery form
            "version: 1",         // the only schema version there is
            "name: direct",       // storage layout
            "path: /dev/vda",     // and on the first virtio disk, not the ISO
            "shutdown: poweroff", // how the host learns the install finished
            "console=ttyS0",      // how the installed system proves it booted
            "fallback: offline-install",
            "username: entangled",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
        // A password *hash*, never a plaintext password.
        assert!(text.contains("password: \"$6$"));
    }

    /// A configuration that is not cloud-config would be silently ignored by
    /// cloud-init, leaving the installer at its first interactive screen.
    #[test]
    fn a_non_cloud_config_autoinstall_is_refused() {
        let dir = std::env::temp_dir().join("entangled-seed-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("bare-{}.yaml", std::process::id()));
        std::fs::write(&path, "version: 1\nidentity:\n  username: x\n").unwrap();
        assert!(matches!(
            user_data("h", Some(&path)),
            Err(SeedError::NotCloudConfig)
        ));

        std::fs::write(&path, "#cloud-config\nautoinstall:\n  version: 1\n").unwrap();
        assert!(user_data("h", Some(&path)).unwrap().contains("autoinstall"));
        // A missing file is a typed error, not a panic.
        assert!(matches!(
            user_data("h", Some(&dir.join("nope.yaml"))),
            Err(SeedError::ReadConfig { .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    /// Writing the volume and reading its metadata back, end to end.
    #[test]
    fn write_produces_a_seed_with_both_files() {
        let dir = std::env::temp_dir().join("entangled-seed-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("seed-{}.iso", std::process::id()));
        let text = user_data("seedtest", None).unwrap();
        let seed = write(&path, &text, "entangled-seedtest").unwrap();

        assert_eq!(seed.bytes, std::fs::metadata(&path).unwrap().len() as usize);
        let image = std::fs::read(&path).unwrap();
        let text_in_image = String::from_utf8_lossy(&image);
        assert!(text_in_image.contains("instance-id: entangled-seedtest"));
        assert!(text_in_image.contains("hostname: \"seedtest\""));
        // Reproducible: the same inputs give byte-identical output (no
        // timestamps), so a re-run does not churn the volume.
        let again = write(&path, &text, "entangled-seedtest").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), image);
        assert_eq!(again.bytes, seed.bytes);
        std::fs::remove_file(&path).unwrap();
    }

    /// A user-data larger than the volume's file cap is a typed error rather
    /// than a corrupt image.
    #[test]
    fn an_oversized_file_is_refused() {
        let huge = vec![b'x'; MAX_FILE_BYTES + 1];
        assert!(matches!(
            build_iso9660(&[("USER-DATA.;1", &huge)]),
            Err(SeedError::TooLarge { .. })
        ));
        // Exactly at the cap is fine, and grows the image beyond the minimum.
        let big = vec![b'y'; MAX_FILE_BYTES];
        let image = build_iso9660(&[("USER-DATA.;1", &big)]).unwrap();
        assert!(image.len() >= MIN_IMAGE_BYTES);
    }
}
