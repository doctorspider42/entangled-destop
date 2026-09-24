//! The installer's configuration volume — a small ISO9660 image handed to the
//! guest as an extra read-only virtio-blk device (backlog UEFI-1804).
//!
//! Two installers use it, and both find it by *filesystem label*:
//!
//! | Volume | Label | File | Read by |
//! |---|---|---|---|
//! | Ubuntu autoinstall | `CIDATA` | `user-data`, `meta-data` | cloud-init's `DataSourceNoCloud` |
//! | Fedora kickstart | `OEMDRV` | `ks.cfg` | Anaconda's dracut module |
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

/// The label Anaconda looks for. Its dracut module resolves
/// `inst.ks=hd:LABEL=OEMDRV:/ks.cfg` through `/dev/disk/by-label/OEMDRV`, and —
/// with no `inst.ks=` at all — auto-detects the same label and the same path
/// (`50-kickstart-genrules.sh` in the installer initramfs). Upper case for the
/// same reason `CIDATA` is: it is matched literally.
pub const OEMDRV_LABEL: &str = "OEMDRV";

/// Minimum image size. Nothing requires it; it keeps the volume a comfortable
/// multiple of the 512-byte sectors virtio-blk reports and leaves room for a
/// larger user-data without changing the shape.
const MIN_IMAGE_BYTES: usize = 64 * 1024;

/// The autoinstall configuration, compiled in so `--auto` works from any
/// working directory (the same reason the Debian preseed is compiled in).
const AUTOINSTALL: &str = include_str!("../../../assets/autoinstall/ubuntu-server.yaml");

/// The Desktop ISO's twin of [`AUTOINSTALL`]: the same document with the
/// desktop's install source. Compiled in for the same reason — an installed
/// copy has no `assets/` directory to pass to `--autoinstall`.
const AUTOINSTALL_DESKTOP: &str = include_str!("../../../assets/autoinstall/ubuntu-desktop.yaml");

/// Which compiled-in autoinstall profile `--auto` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProfile {
    /// `ubuntu-server-minimal`, for the live-server ISO.
    Server,
    /// `ubuntu-desktop-minimal`, for the Desktop ISO: GNOME on GDM.
    Desktop,
}

impl BuiltinProfile {
    /// The profile an ISO needs, from its file name — which is Canonical's
    /// (`ubuntu-26.04.1-desktop-amd64.iso`, `ubuntu-26.04-live-server-amd64.iso`)
    /// for everything `scripts/fetch-ubuntu-iso.sh` verifies into the cache.
    /// Each profile names an install source only its own ISO carries, so
    /// guessing wrong fails the install; a renamed ISO reads as a server one,
    /// which is what `--auto` always meant before the desktop profile existed.
    pub fn for_iso(iso: &Path) -> Self {
        let name = iso
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.contains("desktop") {
            Self::Desktop
        } else {
            Self::Server
        }
    }

    fn template(self) -> &'static str {
        match self {
            Self::Server => AUTOINSTALL,
            Self::Desktop => AUTOINSTALL_DESKTOP,
        }
    }
}

/// `/etc/drirc` on a GPU desktop (ADR-0004, "how a user turns it on"): every
/// OpenGL client on a virtio-gpu device — GNOME Shell first among them — is
/// loaded on Zink, which turns its GL into Vulkan for Mesa's venus driver and
/// so for the host GPU. Without it Mesa picks virgl's GL driver, which our
/// Venus renderer does not serve, and the desktop falls back to llvmpipe.
pub const VENUS_DRIRC: &[&str] = &[
    "<!-- Entangled GPU desktop: every GL client on zink, over Venus (ADR-0004) -->",
    "<driconf>",
    "  <device driver=\"loader\" kernel_driver=\"virtio_gpu\">",
    "    <application name=\"every GL client on zink\">",
    "      <option name=\"dri_driver\" value=\"zink\" />",
    "    </application>",
    "  </device>",
    "</driconf>",
];

/// Where the GPU desktop's GSettings default goes in the target, and what it
/// says. A vendor override rather than a dconf database: it needs no dconf
/// profile (Ubuntu ships none for users) and survives package upgrades, which
/// recompile the directory it sits in. `90_` sorts after Ubuntu's own `10_`.
pub const VENUS_GSCHEMA_OVERRIDE: &str =
    "/usr/share/glib-2.0/schemas/90_entangled-venus.gschema.override";

/// The override's lines. `idle-delay 0` because a blanked GNOME on Zink keeps
/// every client upload of the blanked screen and is refused at its Venus
/// share within seconds, which ends the session (ADR-0004, the amendments of
/// the idle blank and of honest heaps). Until client presentation moves to
/// dma-buf, the desktop must not blank.
pub const VENUS_GSCHEMA_LINES: &[&str] = &[
    "# Entangled GPU desktop: no idle blank (ADR-0004)",
    "[org.gnome.desktop.session]",
    "idle-delay=uint32 0",
];

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

    #[error(
        "--venus sets the installed guest up for the GPU desktop from the autoinstall's own \
         late-commands, and this configuration has no block-style 'late-commands:' list to \
         add them to. Add one under 'autoinstall:' (an empty '- true' item will do)"
    )]
    NoLateCommands,

    #[error(
        "--venus adds the desktop user to the 'render' group, and the autoinstall \
         configuration names no usable identity.username ({found}). It must be a plain \
         Unix user name: a lower-case letter or '_', then letters, digits, '_' or '-'"
    )]
    NoUsername { found: String },
}

/// One file per sector run; the seed only ever has two, so the cap is generous
/// and exists to bound the directory record sector.
const MAX_FILE_BYTES: usize = 8 * SECTOR;

/// What went onto the seed volume, for logging and for the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub path: PathBuf,
    pub bytes: usize,
    /// The configuration actually written, after substitution — the cloud-init
    /// user-data for a `CIDATA` volume, the kickstart for an `OEMDRV` one.
    pub contents: String,
}

/// Builds the autoinstall user-data for `hostname`, either from the compiled-in
/// `builtin` profile or from a caller-supplied file (which then wins).
pub fn user_data(
    hostname: &str,
    custom: Option<&Path>,
    builtin: BuiltinProfile,
) -> Result<String, SeedError> {
    let template = match custom {
        Some(path) => std::fs::read_to_string(path).map_err(|source| SeedError::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?,
        None => builtin.template().to_string(),
    };
    // A file that is not cloud-config would be *ignored*, and the installer
    // would sit at its first interactive screen with no explanation. Refuse it
    // here instead.
    if !template.trim_start().starts_with("#cloud-config") {
        return Err(SeedError::NotCloudConfig);
    }
    Ok(template.replace(HOSTNAME_PLACEHOLDER, hostname))
}

/// The late-commands that make an installed Ubuntu a GPU desktop on the
/// Venus renderer (`install ubuntu --venus`; ADR-0004, "how a user turns it
/// on"), for the desktop user `user`, one YAML list item per entry.
///
/// All of it happens inside the installer, against `/target`, so the first
/// boot is already the finished machine — nothing is typed into a serial
/// console afterwards:
///
/// 1. [`VENUS_DRIRC`] as `/etc/drirc`: GNOME Shell and every GL client on Zink.
/// 2. [`VENUS_GSCHEMA_LINES`] as [`VENUS_GSCHEMA_OVERRIDE`], compiled in the
///    target: no idle blank.
/// 3. `user` in `render`. A graphical session reaches `/dev/dri/renderD128`
///    by logind's ACL without it; a serial or SSH login — which is how every
///    Vulkan probe of this project reaches the guest — does not, and gets an
///    `EACCES` that reads like a renderer bug.
pub fn venus_late_commands(user: &str) -> Vec<String> {
    let printf = |lines: &[&str], path: &str| {
        let mut command = String::from(">-\n  printf '%s\\n'\n");
        for line in lines {
            command.push_str(&format!("  '{line}'\n"));
        }
        command.push_str(&format!("  > /target{path}"));
        command
    };
    vec![
        printf(VENUS_DRIRC, "/etc/drirc"),
        printf(VENUS_GSCHEMA_LINES, VENUS_GSCHEMA_OVERRIDE),
        "curtin in-target -- glib-compile-schemas /usr/share/glib-2.0/schemas".to_string(),
        format!("curtin in-target -- usermod -aG render {user}"),
    ]
}

/// `user_data` with [`venus_late_commands`] put at the head of its
/// `late-commands:` list, for the user its `identity` creates.
///
/// A text edit rather than a YAML round trip, on purpose: the document is
/// the user's (or ours, with its comments), and a parser would reflow it and
/// drop every comment that explains it. The edit is small enough to be exact
/// — find the key, find its items' indentation, insert — and anything it
/// cannot place is refused by name rather than guessed at.
pub fn with_venus_guest(user_data: &str) -> Result<String, SeedError> {
    let user = identity_username(user_data)?;
    let lines: Vec<&str> = user_data.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let key = lines
        .iter()
        .position(|line| {
            let body = line.trim();
            body == "late-commands:"
                || body
                    .strip_prefix("late-commands:")
                    .is_some_and(|rest| rest.trim_start().starts_with('#'))
        })
        .ok_or(SeedError::NoLateCommands)?;
    let key_indent = indent(lines[key]);
    // The items' indentation is the first item's; YAML also allows them at
    // the key's own column. With no item yet, two past the key.
    let item_indent = lines[key + 1..]
        .iter()
        .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .filter(|line| line.trim_start().starts_with('-') && indent(line) >= key_indent)
        .map_or(key_indent + 2, |line| indent(line));
    let pad = " ".repeat(item_indent);
    let mut out = String::with_capacity(user_data.len() + 1024);
    for line in &lines[..=key] {
        out.push_str(line);
        out.push('\n');
    }
    for command in venus_late_commands(&user) {
        // Line 0 is the item; the rest are a block scalar's content, which
        // `venus_late_commands` already indents two past the dash.
        for (i, line) in command.lines().enumerate() {
            out.push_str(&pad);
            if i == 0 {
                out.push_str("- ");
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    for line in &lines[key + 1..] {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// `identity.username`: the first `username:` key of the document, which in
/// an autoinstall is the identity's. Refused unless it is a plain Unix name,
/// because it is spliced into a shell command.
fn identity_username(user_data: &str) -> Result<String, SeedError> {
    let found = user_data
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .find_map(|line| line.trim().strip_prefix("username:"))
        .map(|value| {
            let value = value.split('#').next().unwrap_or_default().trim();
            value.trim_matches(|c| c == '"' || c == '\'').to_string()
        });
    let Some(name) = found else {
        return Err(SeedError::NoUsername {
            found: "none".into(),
        });
    };
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && name.len() <= 32;
    if plain {
        Ok(name)
    } else {
        Err(SeedError::NoUsername {
            found: format!("'{name}'"),
        })
    }
}

/// Writes a NoCloud seed volume at `path` carrying `user_data`.
///
/// `instance_id` becomes the seed's `meta-data`. cloud-init treats a change of
/// instance id as a new instance, so a fresh install gets a fresh one.
pub fn write(path: &Path, user_data: &str, instance_id: &str) -> Result<Seed, SeedError> {
    let meta_data = format!("instance-id: {instance_id}\nlocal-hostname: {instance_id}\n");
    write_volume(
        path,
        SEED_LABEL,
        &[
            ("USER-DATA.;1", user_data.as_bytes()),
            ("META-DATA.;1", meta_data.as_bytes()),
        ],
        user_data,
    )
}

/// Writes an [`OEMDRV_LABEL`] volume carrying a single `ks.cfg`, which is what
/// Anaconda fetches and runs.
///
/// The recorded identifier is `KS.CFG;1`; `isofs` presents that as `ks.cfg`,
/// which is the name `fetch-kickstart-disk` copies out — the same translation
/// the NoCloud names rely on, modelled in
/// [`tests::the_names_translate_the_way_isofs_does`].
pub fn write_kickstart(path: &Path, kickstart: &str) -> Result<Seed, SeedError> {
    write_volume(
        path,
        OEMDRV_LABEL,
        &[("KS.CFG;1", kickstart.as_bytes())],
        kickstart,
    )
}

/// Writes a labelled ISO9660 volume holding `files`.
fn write_volume(
    path: &Path,
    label: &str,
    files: &[(&str, &[u8])],
    contents: &str,
) -> Result<Seed, SeedError> {
    let image = build_iso9660(files, label)?;
    std::fs::write(path, &image).map_err(|source| SeedError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    tracing::info!(
        path = %path.display(),
        bytes = image.len(),
        label,
        files = files.len(),
        "wrote the installer configuration volume"
    );
    Ok(Seed {
        path: path.to_path_buf(),
        bytes: image.len(),
        contents: contents.to_string(),
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
fn build_iso9660(files: &[(&str, &[u8])], label: &str) -> Result<Vec<u8>, SeedError> {
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
    fill_a_chars(&mut pvd[40..72], label); // volume identifier: the LABEL
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
        build_iso9660(
            &[
                (
                    "USER-DATA.;1",
                    b"#cloud-config\nautoinstall:\n  version: 1\n" as &[u8],
                ),
                ("META-DATA.;1", b"instance-id: test\n"),
            ],
            SEED_LABEL,
        )
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
        let text = user_data("ubuntu-demo", None, BuiltinProfile::Server).unwrap();
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

    /// The Desktop ISO gets the desktop profile, by Canonical's file name;
    /// anything else keeps the server profile `--auto` always meant.
    #[test]
    fn the_builtin_profile_follows_the_iso() {
        for (iso, profile) in [
            ("ubuntu-26.04.1-desktop-amd64.iso", BuiltinProfile::Desktop),
            ("UBUNTU-26.04-DESKTOP-AMD64.ISO", BuiltinProfile::Desktop),
            ("ubuntu-26.04-live-server-amd64.iso", BuiltinProfile::Server),
            ("installer.iso", BuiltinProfile::Server),
        ] {
            let path = Path::new("cache").join("ubuntu").join(iso);
            assert_eq!(BuiltinProfile::for_iso(&path), profile, "{iso}");
        }
        let desktop = user_data("gpu", None, BuiltinProfile::Desktop).expect("desktop");
        assert!(desktop.contains("id: ubuntu-desktop-minimal"), "{desktop}");
        assert!(desktop.contains("hostname: \"gpu\""));
        let server = user_data("srv", None, BuiltinProfile::Server).expect("server");
        assert!(server.contains("id: ubuntu-server-minimal"));
    }

    /// What `--venus` adds to the built-in profiles, byte for byte: four
    /// items at the head of `late-commands`, at the list's own indentation,
    /// before the serial-console items that were already there.
    #[test]
    fn venus_late_commands_are_exact_and_first() {
        const EXPECTED: &str = r#"  late-commands:
    - >-
      printf '%s\n'
      '<!-- Entangled GPU desktop: every GL client on zink, over Venus (ADR-0004) -->'
      '<driconf>'
      '  <device driver="loader" kernel_driver="virtio_gpu">'
      '    <application name="every GL client on zink">'
      '      <option name="dri_driver" value="zink" />'
      '    </application>'
      '  </device>'
      '</driconf>'
      > /target/etc/drirc
    - >-
      printf '%s\n'
      '# Entangled GPU desktop: no idle blank (ADR-0004)'
      '[org.gnome.desktop.session]'
      'idle-delay=uint32 0'
      > /target/usr/share/glib-2.0/schemas/90_entangled-venus.gschema.override
    - curtin in-target -- glib-compile-schemas /usr/share/glib-2.0/schemas
    - curtin in-target -- usermod -aG render entangled
    - sed -i '/^GRUB_CMDLINE_LINUX_DEFAULT=/d"#;
        for profile in [BuiltinProfile::Server, BuiltinProfile::Desktop] {
            let plain = user_data("gpu", None, profile).expect("user-data");
            let text = with_venus_guest(&plain).expect("venus added");
            assert!(text.contains(EXPECTED), "{profile:?}:\n{text}");
            assert_eq!(text.matches("late-commands:").count(), 1);
            // Nothing else moved: taking the four items out gives the
            // original back.
            let items: String = venus_late_commands("entangled")
                .iter()
                .flat_map(|command| {
                    command
                        .lines()
                        .enumerate()
                        .map(|(i, line)| format!("    {}{line}\n", if i == 0 { "- " } else { "" }))
                        .collect::<Vec<_>>()
                })
                .collect();
            assert_eq!(text.replacen(&items, "", 1), plain, "{profile:?}");
        }
    }

    /// What the shell in the installer runs, after YAML folds each `>-` item
    /// onto one line: the drirc and the override land with exactly their
    /// lines. Single quotes delimit every line, so none may contain one.
    #[test]
    fn the_folded_commands_write_exactly_the_files() {
        for line in VENUS_DRIRC.iter().chain(VENUS_GSCHEMA_LINES) {
            assert!(!line.contains('\''), "{line}");
        }
        let fold = |command: &str| {
            let mut lines = command.lines();
            assert_eq!(lines.next(), Some(">-"));
            lines.map(str::trim).collect::<Vec<_>>().join(" ")
        };
        let commands = venus_late_commands("desk_user-1");
        let quoted = |lines: &[&str]| {
            lines
                .iter()
                .map(|l| format!("'{l}'"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            fold(&commands[0]),
            format!("printf '%s\\n' {} > /target/etc/drirc", quoted(VENUS_DRIRC))
        );
        assert_eq!(
            fold(&commands[1]),
            format!(
                "printf '%s\\n' {} > /target{VENUS_GSCHEMA_OVERRIDE}",
                quoted(VENUS_GSCHEMA_LINES)
            )
        );
        assert_eq!(
            commands[3],
            "curtin in-target -- usermod -aG render desk_user-1"
        );
        // Leading spaces of an XML line sit inside its quotes, so folding
        // (which trims the YAML indentation) keeps them.
        assert!(fold(&commands[0]).contains("'  <device driver="));
    }

    /// A user's own autoinstall: the list's indentation is followed, an
    /// indentless list too, and what cannot be placed is refused by name.
    #[test]
    fn venus_follows_a_custom_document_or_refuses_it() {
        let indentless = "#cloud-config\nautoinstall:\n  version: 1\n  identity:\n    \
                          username: \"alice\"\n  late-commands:\n  - echo one\n";
        let text = with_venus_guest(indentless).expect("indentless list");
        assert!(
            text.contains("  late-commands:\n  - >-\n    printf '%s\\n'\n"),
            "{text}"
        );
        assert!(text.contains("  - curtin in-target -- usermod -aG render alice\n  - echo one\n"));

        let empty = "#cloud-config\nautoinstall:\n  identity:\n    username: bob # me\n  \
                     late-commands:   # ours\n  shutdown: poweroff\n";
        let text = with_venus_guest(empty).expect("an empty list is given items");
        assert!(
            text.contains("  late-commands:   # ours\n    - >-\n"),
            "{text}"
        );
        assert!(
            text.contains("render bob\n  shutdown: poweroff\n"),
            "{text}"
        );

        let none = "#cloud-config\nautoinstall:\n  identity:\n    username: bob\n";
        assert!(matches!(
            with_venus_guest(none),
            Err(SeedError::NoLateCommands)
        ));
        let flow = "#cloud-config\nautoinstall:\n  identity:\n    username: bob\n  \
                    late-commands: [true]\n";
        assert!(matches!(
            with_venus_guest(flow),
            Err(SeedError::NoLateCommands)
        ));

        for bad in ["", "Bob", "bob; reboot", "$(id)", "9lives"] {
            let text = format!(
                "#cloud-config\nautoinstall:\n  identity:\n    username: '{bad}'\n  \
                 late-commands:\n    - true\n"
            );
            assert!(
                matches!(with_venus_guest(&text), Err(SeedError::NoUsername { .. })),
                "{bad:?} must be refused"
            );
        }
        let nobody = "#cloud-config\nautoinstall:\n  late-commands:\n    - true\n";
        assert!(matches!(
            with_venus_guest(nobody),
            Err(SeedError::NoUsername { .. })
        ));
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
            user_data("h", Some(&path), BuiltinProfile::Server),
            Err(SeedError::NotCloudConfig)
        ));

        std::fs::write(&path, "#cloud-config\nautoinstall:\n  version: 1\n").unwrap();
        assert!(user_data("h", Some(&path), BuiltinProfile::Server)
            .unwrap()
            .contains("autoinstall"));
        // A missing file is a typed error, not a panic.
        assert!(matches!(
            user_data("h", Some(&dir.join("nope.yaml")), BuiltinProfile::Server),
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
        let text = user_data("seedtest", None, BuiltinProfile::Server).unwrap();
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

    /// The Fedora volume is the same image with a different label and a
    /// different file name — and both halves matter: Anaconda resolves
    /// `/dev/disk/by-label/OEMDRV` (so the PVD volume identifier is what finds
    /// it at all) and then copies `/ks.cfg` off the mount.
    #[test]
    fn the_kickstart_volume_is_labelled_oemdrv_and_holds_ks_cfg() {
        let dir = std::env::temp_dir().join("entangled-seed-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("oemdrv-{}.iso", std::process::id()));
        let ks = "text\npoweroff\n%packages\n@^workstation-product-environment\n%end\n";
        let seed = write_kickstart(&path, ks).unwrap();
        assert_eq!(seed.contents, ks);

        let image = std::fs::read(&path).unwrap();
        assert_eq!(&image[0x8001..0x8006], b"CD001");
        let label = &image[0x8000 + 40..0x8000 + 72];
        assert_eq!(&label[..6], b"OEMDRV");
        assert!(label[6..].iter().all(|&b| b == b' '));
        // ...and nothing of the Ubuntu volume leaked into it.
        assert!(!String::from_utf8_lossy(&image).contains("CIDATA"));

        // One file, recorded as KS.CFG;1, which isofs shows as ks.cfg.
        let root = &image[20 * SECTOR..21 * SECTOR];
        let mut at = 0usize;
        let mut names = Vec::new();
        while at < root.len() && root[at] != 0 {
            let len = root[at] as usize;
            let record = &root[at..at + len];
            let id_len = record[32] as usize;
            names.push(String::from_utf8_lossy(&record[33..33 + id_len]).into_owned());
            at += len;
        }
        assert_eq!(names.len(), 3, "'.', '..' and one file: {names:?}");
        assert_eq!(names[2], "KS.CFG;1");
        assert!(String::from_utf8_lossy(&image).contains("workstation-product-environment"));
        std::fs::remove_file(&path).unwrap();
    }

    /// A user-data larger than the volume's file cap is a typed error rather
    /// than a corrupt image.
    #[test]
    fn an_oversized_file_is_refused() {
        let huge = vec![b'x'; MAX_FILE_BYTES + 1];
        assert!(matches!(
            build_iso9660(&[("USER-DATA.;1", &huge)], SEED_LABEL),
            Err(SeedError::TooLarge { .. })
        ));
        // Exactly at the cap is fine, and grows the image beyond the minimum.
        let big = vec![b'y'; MAX_FILE_BYTES];
        let image = build_iso9660(&[("USER-DATA.;1", &big)], SEED_LABEL).unwrap();
        assert!(image.len() >= MIN_IMAGE_BYTES);
    }
}
