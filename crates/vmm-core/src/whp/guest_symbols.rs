//! Naming where a guest is, for the exit trace (`$ENTANGLED_WHP_TRACE_EXITS`).
//!
//! A RIP in a guest kernel is a number until something names it. Windows does
//! not come with symbols, but every Windows module is a PE image whose export
//! table names its public functions, and the kernel's are most of the ones a
//! boot gets stuck in. So: walk down from the RIP to the image's `MZ` header,
//! read the export directory, and report the nearest export at or below it.
//! Diagnostics only. Nothing here runs unless the trace is on, and every read
//! goes through the caller's checked guest-virtual reader.

/// Reads guest virtual memory; `None` when a byte does not translate or is
/// not RAM.
pub(super) type ReadGva<'a> = &'a dyn Fn(u64, &mut [u8]) -> Option<()>;

/// How far below the RIP to look for the image header: ntoskrnl is ~10 MiB.
const MAX_IMAGE_SCAN: u64 = 32 << 20;
const PAGE: u64 = 4096;
/// Upper bound on export names read, so a corrupt table cannot spin.
const MAX_EXPORTS: u32 = 8192;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The base of the PE image containing `rip`, by its `MZ`/`PE` headers.
pub(super) fn image_base(read: ReadGva<'_>, rip: u64) -> Option<u64> {
    let mut page = rip & !(PAGE - 1);
    let floor = rip.saturating_sub(MAX_IMAGE_SCAN);
    let mut header = [0u8; 0x200];
    while page >= floor {
        if read(page, &mut header[..2]).is_some() && &header[..2] == b"MZ" {
            read(page, &mut header)?;
            let nt = u32_at(&header, 0x3c)? as usize;
            // `SizeOfImage`, 56 bytes into the optional header: a header
            // whose image does not reach `rip` is a PE embedded in the one
            // we are looking for, not its start.
            let size = u32_at(&header, nt + 4 + 20 + 56).unwrap_or(0);
            if header.get(nt..nt + 4) == Some(b"PE\0\0") && page + u64::from(size) > rip {
                return Some(page);
            }
        }
        page = page.checked_sub(PAGE)?;
    }
    None
}

/// The export directory of the PE32+ image at `base`: (count, functions,
/// names, ordinals), as guest virtual addresses.
fn export_tables(read: ReadGva<'_>, base: u64) -> Option<(u32, u64, u64, u64)> {
    let mut header = [0u8; 0x400];
    read(base, &mut header)?;
    let nt = u32_at(&header, 0x3c)? as usize;
    // PE32+ optional header follows the 20-byte file header; the export data
    // directory is its first entry, 112 bytes in.
    let optional = nt + 4 + 20;
    if u16_at(&header, optional)? != 0x20b {
        return None;
    }
    let export_rva = u32_at(&header, optional + 112)?;
    if export_rva == 0 {
        return None;
    }
    let mut dir = [0u8; 40];
    read(base + u64::from(export_rva), &mut dir)?;
    Some((
        u32_at(&dir, 0x18)?.min(MAX_EXPORTS),
        base + u64::from(u32_at(&dir, 0x1c)?),
        base + u64::from(u32_at(&dir, 0x20)?),
        base + u64::from(u32_at(&dir, 0x24)?),
    ))
}

/// The address of export `name` of the image at `base`.
pub(super) fn export_address(read: ReadGva<'_>, base: u64, name: &str) -> Option<u64> {
    let (count, functions, names, ordinals) = export_tables(read, base)?;
    let want = name.as_bytes();
    for i in 0..u64::from(count) {
        let mut name_rva = [0u8; 4];
        read(names + i * 4, &mut name_rva)?;
        let mut candidate = [0u8; 64];
        read(
            base + u64::from(u32::from_le_bytes(name_rva)),
            &mut candidate,
        )?;
        let len = candidate.iter().position(|&b| b == 0).unwrap_or(0);
        if &candidate[..len] == want {
            let mut ordinal = [0u8; 2];
            read(ordinals + i * 2, &mut ordinal)?;
            let mut function = [0u8; 4];
            read(
                functions + u64::from(u16::from_le_bytes(ordinal)) * 4,
                &mut function,
            )?;
            return Some(base + u64::from(u32::from_le_bytes(function)));
        }
    }
    None
}

/// `"name+0xoff"` for the nearest export of the image at `base` at or below
/// `rip`, or `"+0xrva"` when the image exports nothing below it.
pub(super) fn nearest_export(read: ReadGva<'_>, base: u64, rip: u64) -> Option<String> {
    let mut header = [0u8; 0x400];
    read(base, &mut header)?;
    let nt = u32_at(&header, 0x3c)? as usize;
    // PE32+ optional header follows the 20-byte file header; the export data
    // directory is its first entry, 112 bytes in.
    let optional = nt + 4 + 20;
    if u16_at(&header, optional)? != 0x20b {
        return None;
    }
    let export_rva = u32_at(&header, optional + 112)?;
    let target = rip.checked_sub(base)?;
    if export_rva == 0 {
        return Some(format!("+{target:#x}"));
    }
    let mut dir = [0u8; 40];
    read(base + u64::from(export_rva), &mut dir)?;
    let count = u32_at(&dir, 0x18)?.min(MAX_EXPORTS);
    let functions = base + u64::from(u32_at(&dir, 0x1c)?);
    let names = base + u64::from(u32_at(&dir, 0x20)?);
    let ordinals = base + u64::from(u32_at(&dir, 0x24)?);
    let mut best: Option<(u64, u32)> = None;
    for i in 0..u64::from(count) {
        let mut ordinal = [0u8; 2];
        read(ordinals + i * 2, &mut ordinal)?;
        let mut function = [0u8; 4];
        read(
            functions + u64::from(u16::from_le_bytes(ordinal)) * 4,
            &mut function,
        )?;
        let rva = u64::from(u32::from_le_bytes(function));
        if rva <= target && best.is_none_or(|(at, _)| rva > at) {
            best = Some((rva, i as u32));
        }
    }
    let Some((rva, index)) = best else {
        return Some(format!("+{target:#x}"));
    };
    let mut name_rva = [0u8; 4];
    read(names + u64::from(index) * 4, &mut name_rva)?;
    let mut name = [0u8; 64];
    read(base + u64::from(u32::from_le_bytes(name_rva)), &mut name)?;
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Some(format!(
        "{}+{:#x}",
        String::from_utf8_lossy(&name[..len]),
        target - rva
    ))
}
