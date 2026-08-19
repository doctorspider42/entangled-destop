//! Just enough AML to emit a real DSDT (ACPI 6.5 §20 "ACPI Machine Language
//! Specification").
//!
//! Hand-rolled rather than pulled from a crate: the DSDT this machine needs has
//! five kinds of object in it (`Name`, `Package`, `Buffer`, `Scope`, `Device`),
//! and encoding those is ~150 lines against a stable, 25-year-old grammar. The
//! encoders below are byte-for-byte what `iasl` produces for the equivalent ASL,
//! which is what [`super::tests`] and `iasl -d` on the generated table check.
//!
//! Everything here is host-OS independent and allocation-only — no guest input
//! ever reaches it, so there is no untrusted path to bound.

// ---- opcodes (ACPI 6.5 table 20.1) ---------------------------------------

const ZERO_OP: u8 = 0x00;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0a;
const WORD_PREFIX: u8 = 0x0b;
const DWORD_PREFIX: u8 = 0x0c;
const STRING_PREFIX: u8 = 0x0d;
const BUFFER_OP: u8 = 0x11;
const PACKAGE_OP: u8 = 0x12;
const SCOPE_OP: u8 = 0x10;
const DUAL_NAME_PREFIX: u8 = 0x2e;
const ROOT_CHAR: u8 = b'\\';
const EXT_OP_PREFIX: u8 = 0x5b;
const DEVICE_OP: u8 = 0x82; // after EXT_OP_PREFIX

/// A `NameSeg`: exactly four `LeadNameChar`/`NameChar` bytes, `_`-padded.
///
/// Names are compile-time constants in this crate, so an over-long or
/// out-of-alphabet name is a bug in *our* source, not guest input: it is
/// truncated/normalised rather than reported, and [`super::tests`] asserts the
/// names we actually emit.
pub fn name_seg(name: &str) -> [u8; 4] {
    let mut seg = [b'_'; 4];
    for (slot, byte) in seg.iter_mut().zip(name.bytes().take(4)) {
        *slot = byte.to_ascii_uppercase();
    }
    seg
}

/// `\NAME` — a name relative to the root scope.
pub fn root_name(name: &str) -> Vec<u8> {
    let mut out = vec![ROOT_CHAR];
    out.extend_from_slice(&name_seg(name));
    out
}

/// `\PARENT.CHILD` — a two-segment path from the root.
pub fn root_path2(parent: &str, child: &str) -> Vec<u8> {
    let mut out = vec![ROOT_CHAR, DUAL_NAME_PREFIX];
    out.extend_from_slice(&name_seg(parent));
    out.extend_from_slice(&name_seg(child));
    out
}

/// `PkgLength`, whose encoded size is part of the length it encodes
/// (ACPI 6.5 §20.2.4).
pub fn pkg_length(payload: usize) -> Vec<u8> {
    // Try each width until the total (payload + the length bytes themselves)
    // fits. 1 byte holds 6 bits; every wider form holds 4 bits in the lead byte
    // plus 8 per follower.
    for bytes in 1..=4usize {
        let total = payload + bytes;
        let capacity = if bytes == 1 {
            0x40
        } else {
            1 << (4 + 8 * (bytes - 1))
        };
        if total < capacity {
            let mut out = Vec::with_capacity(bytes);
            if bytes == 1 {
                out.push(total as u8);
            } else {
                out.push((((bytes - 1) as u8) << 6) | (total & 0x0f) as u8);
                for i in 0..bytes - 1 {
                    out.push(((total >> (4 + 8 * i)) & 0xff) as u8);
                }
            }
            return out;
        }
    }
    // 2^28 bytes of AML is not a table we could ever place; clamp rather than
    // panic, and let the caller's size check reject the blob.
    vec![0xc0 | 0x0f, 0xff, 0xff, 0xff]
}

/// Wraps `body` in `opcode + PkgLength`.
fn packaged(opcode: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = opcode.to_vec();
    out.extend_from_slice(&pkg_length(body.len()));
    out.extend_from_slice(body);
    out
}

// ---- data objects --------------------------------------------------------

/// The smallest integer encoding that holds `value`.
pub fn integer(value: u64) -> Vec<u8> {
    match value {
        0 => vec![ZERO_OP],
        v if v <= 0xff => vec![BYTE_PREFIX, v as u8],
        v if v <= 0xffff => {
            let mut out = vec![WORD_PREFIX];
            out.extend_from_slice(&(v as u16).to_le_bytes());
            out
        }
        v if v <= 0xffff_ffff => {
            let mut out = vec![DWORD_PREFIX];
            out.extend_from_slice(&(v as u32).to_le_bytes());
            out
        }
        v => {
            let mut out = vec![0x0e]; // QWordPrefix
            out.extend_from_slice(&v.to_le_bytes());
            out
        }
    }
}

/// A `DWordConst` even when the value would fit in fewer bytes. `_HID` values
/// are compared as integers, but keeping the width fixed makes the emitted
/// bytes match `iasl`'s `EisaId()` output exactly.
pub fn dword(value: u32) -> Vec<u8> {
    let mut out = vec![DWORD_PREFIX];
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// A NUL-terminated AML string.
pub fn string(text: &str) -> Vec<u8> {
    let mut out = vec![STRING_PREFIX];
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    out
}

/// `EisaId("PNP0A03")` — the compressed 7-character PNP/EISA id, stored as the
/// little-endian DWORD `iasl` emits (`0x030ad041` for `PNP0A03`).
pub fn eisa_id(id: &str) -> Vec<u8> {
    let b = id.as_bytes();
    if b.len() != 7 {
        return dword(0);
    }
    let letter =
        |c: u8| u32::from(c.to_ascii_uppercase().wrapping_sub(b'A').wrapping_add(1)) & 0x1f;
    let hex = |c: u8| char::from(c).to_digit(16).unwrap_or(0);
    let compressed: u32 = (letter(b[0]) << 10) | (letter(b[1]) << 5) | letter(b[2]);
    // The manufacturer word is stored big-endian, the product/revision nibbles
    // in source order — i.e. the whole thing byte-swapped relative to a plain
    // integer, which is why this is assembled a byte at a time.
    let bytes = [
        (compressed >> 8) as u8,
        (compressed & 0xff) as u8,
        ((hex(b[3]) << 4) | hex(b[4])) as u8,
        ((hex(b[5]) << 4) | hex(b[6])) as u8,
    ];
    dword(u32::from_le_bytes(bytes))
}

/// `Package (n) { … }`.
pub fn package(elements: &[Vec<u8>]) -> Vec<u8> {
    let mut body = vec![elements.len() as u8];
    for element in elements {
        body.extend_from_slice(element);
    }
    packaged(&[PACKAGE_OP], &body)
}

/// `Buffer (n) { … }` with a constant size.
pub fn buffer(bytes: &[u8]) -> Vec<u8> {
    let mut body = integer(bytes.len() as u64);
    body.extend_from_slice(bytes);
    packaged(&[BUFFER_OP], &body)
}

// ---- named objects -------------------------------------------------------

/// `Name (name, value)` where `name` is a single `NameSeg`.
pub fn name(name: &str, value: &[u8]) -> Vec<u8> {
    let mut out = vec![NAME_OP];
    out.extend_from_slice(&name_seg(name));
    out.extend_from_slice(value);
    out
}

/// `Name (path, value)` for an already-encoded `NameString`.
pub fn name_path(path: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = vec![NAME_OP];
    out.extend_from_slice(path);
    out.extend_from_slice(value);
    out
}

/// `Scope (path) { body }`.
pub fn scope(path: &[u8], body: &[u8]) -> Vec<u8> {
    let mut inner = path.to_vec();
    inner.extend_from_slice(body);
    packaged(&[SCOPE_OP], &inner)
}

/// `Device (name) { body }`.
pub fn device(name: &str, body: &[u8]) -> Vec<u8> {
    let mut inner = name_seg(name).to_vec();
    inner.extend_from_slice(body);
    packaged(&[EXT_OP_PREFIX, DEVICE_OP], &inner)
}

// ---- resource descriptors (ACPI 6.5 §6.4) --------------------------------

/// Address-space descriptor general flags: resource producer, positive decode,
/// both ends of the range fixed.
const RES_GENERAL_FLAGS: u8 = 0x0c;

/// `WordBusNumber (…, min, max, 0, len)` — 16 bytes.
pub fn word_bus_number(min: u16, max: u16) -> Vec<u8> {
    let mut out = vec![0x88];
    out.extend_from_slice(&0x000du16.to_le_bytes()); // payload length
    out.push(0x02); // resource type: bus number range
    out.push(RES_GENERAL_FLAGS);
    out.push(0x00); // type-specific flags
    out.extend_from_slice(&0u16.to_le_bytes()); // granularity
    out.extend_from_slice(&min.to_le_bytes());
    out.extend_from_slice(&max.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // translation offset
    out.extend_from_slice(&(max - min + 1).to_le_bytes());
    out
}

/// `IO (Decode16, base, base, align, len)` — 8 bytes.
pub fn io_port(base: u16, len: u8) -> Vec<u8> {
    let mut out = vec![0x47];
    out.push(0x01); // 16-bit decode
    out.extend_from_slice(&base.to_le_bytes());
    out.extend_from_slice(&base.to_le_bytes());
    out.push(0x01); // alignment
    out.push(len);
    out
}

/// `DWordMemory (…, base, base+len-1, 0, len)` — 26 bytes, non-cacheable and
/// read/write, which is what an MMIO aperture is.
pub fn dword_memory(base: u32, len: u32) -> Vec<u8> {
    let mut out = vec![0x87];
    out.extend_from_slice(&0x0017u16.to_le_bytes()); // payload length
    out.push(0x00); // resource type: memory
    out.push(RES_GENERAL_FLAGS);
    out.push(0x01); // read/write, non-cacheable
    out.extend_from_slice(&0u32.to_le_bytes()); // granularity
    out.extend_from_slice(&base.to_le_bytes()); // minimum
    out.extend_from_slice(&(base + len - 1).to_le_bytes()); // maximum
    out.extend_from_slice(&0u32.to_le_bytes()); // translation offset
    out.extend_from_slice(&len.to_le_bytes());
    out
}

/// The `EndTag` every `ResourceTemplate` finishes with. Checksum 0 means "not
/// checked", which is what every real firmware emits.
pub fn end_tag() -> Vec<u8> {
    vec![0x79, 0x00]
}

/// `ResourceTemplate () { descriptors… }`: the descriptors plus an `EndTag`,
/// wrapped in a buffer.
pub fn resource_template(descriptors: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for descriptor in descriptors {
        bytes.extend_from_slice(descriptor);
    }
    bytes.extend_from_slice(&end_tag());
    buffer(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_length_widths_match_the_grammar() {
        // 1-byte form: value is the total, top two bits zero.
        assert_eq!(pkg_length(0), vec![0x01]);
        assert_eq!(pkg_length(0x3e), vec![0x3f]);
        // 0x3f payload no longer fits in the 1-byte form (total would be 0x40).
        let two = pkg_length(0x3f);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0] & 0xc0, 0x40, "lead byte announces one follower");
        // Total is little-endian nibble-then-bytes: 0x3f + 2 = 0x41.
        assert_eq!(two[0] & 0x0f, 0x01);
        assert_eq!(two[1], 0x04);
        let three = pkg_length(0x2000);
        assert_eq!(three.len(), 3);
        assert_eq!(three[0] & 0xc0, 0x80);
        let total = usize::from(three[0] & 0x0f)
            | (usize::from(three[1]) << 4)
            | (usize::from(three[2]) << 12);
        assert_eq!(total, 0x2000 + 3);
    }

    #[test]
    fn eisa_id_matches_iasl_for_pnp0a03() {
        // The one value that matters: a PCI root bridge's _HID.
        assert_eq!(eisa_id("PNP0A03"), vec![0x0c, 0x41, 0xd0, 0x0a, 0x03]);
        assert_eq!(eisa_id("PNP0A08"), vec![0x0c, 0x41, 0xd0, 0x0a, 0x08]);
        assert_eq!(
            eisa_id("nope"),
            dword(0),
            "wrong length is refused, not panicked"
        );
    }

    #[test]
    fn name_segments_are_four_padded_bytes() {
        assert_eq!(&name_seg("_S5"), b"_S5_");
        assert_eq!(&name_seg("PCI0"), b"PCI0");
        assert_eq!(&name_seg("TOOLONGNAME"), b"TOOL");
        assert_eq!(root_name("_S5"), b"\\_S5_".to_vec());
        assert_eq!(root_path2("_SB", "PCI0"), b"\\\x2e_SB_PCI0".to_vec());
    }

    #[test]
    fn integers_use_the_narrowest_encoding() {
        assert_eq!(integer(0), vec![0x00]);
        assert_eq!(integer(5), vec![0x0a, 0x05]);
        assert_eq!(integer(0x1234), vec![0x0b, 0x34, 0x12]);
        assert_eq!(integer(0x1_0000), vec![0x0c, 0x00, 0x00, 0x01, 0x00]);
    }

    /// `Name (_S5, Package (4) { 5, 5, 0, 0 })`, the object a guest `poweroff`
    /// dies without.
    #[test]
    fn s5_package_is_the_bytes_iasl_emits() {
        let s5 = name_path(
            &root_name("_S5"),
            &package(&[integer(5), integer(5), integer(0), integer(0)]),
        );
        assert_eq!(
            s5,
            vec![
                0x08, // NameOp
                b'\\', b'_', b'S', b'5', b'_', // \_S5_
                0x12, // PackageOp
                0x08, // PkgLength: 8 bytes, itself included
                0x04, // 4 elements
                0x0a, 0x05, 0x0a, 0x05, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn resource_descriptors_have_their_spec_lengths() {
        assert_eq!(word_bus_number(0, 0).len(), 16);
        assert_eq!(io_port(0xcf8, 8).len(), 8);
        assert_eq!(dword_memory(0xc000_0000, 0x1000_0000).len(), 26);
        // A template is a buffer: BufferOp + PkgLength + size + payload.
        let crs = resource_template(&[word_bus_number(0, 0), io_port(0xcf8, 8)]);
        assert_eq!(crs[0], BUFFER_OP);
        assert_eq!(crs[2], BYTE_PREFIX);
        assert_eq!(crs[3] as usize, 16 + 8 + 2);
        assert_eq!(crs.len(), 4 + 16 + 8 + 2);
        assert_eq!(crs[crs.len() - 2], 0x79, "EndTag");
    }

    /// The maximum of a `DWordMemory` range is inclusive; an off-by-one here
    /// would either hide a page from the guest or overlap the next window.
    #[test]
    fn dword_memory_range_is_inclusive() {
        let d = dword_memory(0xc000_0000, 0x1000_0000);
        let min = u32::from_le_bytes(d[10..14].try_into().unwrap());
        let max = u32::from_le_bytes(d[14..18].try_into().unwrap());
        let len = u32::from_le_bytes(d[22..26].try_into().unwrap());
        assert_eq!(min, 0xc000_0000);
        assert_eq!(max, 0xcfff_ffff);
        assert_eq!(len, 0x1000_0000);
        assert_eq!(max - min + 1, len);
    }
}
