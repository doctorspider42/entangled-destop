//! The snapshot container: a fixed header, a run of sections, and an index at
//! the end that names and digests every one of them.
//!
//! ```text
//!   0        magic "ENTGLSNP", format version, flags, host, arch,
//!            index offset + length + SHA-256                       (72 bytes)
//!   72       section payloads, back to back, in the order written
//!   index    one 56-byte entry per section: kind, version, instance,
//!            offset, length, SHA-256 of the payload
//! ```
//!
//! **Why the index is at the end.** The largest section is guest memory, and it
//! is streamed: the writer does not know how long it will be until it has
//! written it. Putting the index last means one forward pass over multi-gigabyte
//! data and a single four-field patch of the header afterwards.
//!
//! **Why every section carries its own digest.** A snapshot restored from a
//! half-written file is a guest with a hole in it, and the hole is not visible
//! from the outside — a torn memory section reads as plausible pages. The
//! digests turn that into a refusal. They are integrity checks, not
//! authentication: a snapshot is not a security boundary, and nothing here
//! claims it is. What they buy is that *every* corruption is a typed error
//! rather than a guest that misbehaves an hour later.

use std::io::{Read, Seek, SeekFrom, Write};

use sha2::{Digest, Sha256};

use crate::codec::{Reader, Writer};
use crate::error::{Result, SnapshotError};

/// First eight bytes of every snapshot file.
pub const MAGIC: [u8; 8] = *b"ENTGLSNP";

/// The format this build writes, and the only one it reads.
///
/// Bumped whenever the meaning of anything already in the file changes.
/// Restoring a different version is a named refusal, not a best effort: a
/// snapshot is a whole machine, and half of one is worse than none.
pub const VERSION: u32 = 1;

/// Bytes of fixed header before the first section.
pub const HEADER_LEN: u64 = 72;

/// Bytes per section-index entry.
const INDEX_ENTRY_LEN: usize = 56;

/// Sections one snapshot may carry. Far above what a real VM produces (one per
/// vCPU, one per virtio slot, two memory regions, a dozen machine devices).
const MAX_SECTIONS: usize = 4096;

/// Largest section [`SnapshotReader::read_section`] will materialise. Guest
/// memory does not go through it — it is streamed — so this bounds device and
/// CPU state only, where the biggest single item is a 4 KiB XSAVE area.
pub const MAX_SECTION_BYTES: usize = 64 << 20;

/// Chunk the streaming reader and writer move memory in.
const STREAM_CHUNK: usize = 1 << 20;

/// Which hypervisor took a snapshot.
///
/// Part of the header rather than an afterthought, because it is a **refusal
/// rule**: a saved vCPU carries that hypervisor's own opaque blobs (KVM's
/// `kvm_lapic_state` page, WHP's interrupt-controller state) and the other host
/// cannot load them. Naming the host is what turns "the guest came back subtly
/// wrong" into "this snapshot was taken on KVM".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    KvmLinux,
    WhpWindows,
}

impl HostKind {
    /// The host this build runs on, or `None` on a development OS with neither
    /// backend (where a snapshot can still be parsed and inspected).
    pub const fn current() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            Some(HostKind::KvmLinux)
        }
        #[cfg(windows)]
        {
            Some(HostKind::WhpWindows)
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            None
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            HostKind::KvmLinux => "Linux/KVM",
            HostKind::WhpWindows => "Windows/WHP",
        }
    }

    const fn code(self) -> u32 {
        match self {
            HostKind::KvmLinux => 1,
            HostKind::WhpWindows => 2,
        }
    }

    fn from_code(code: u32) -> Result<Self> {
        match code {
            1 => Ok(HostKind::KvmLinux),
            2 => Ok(HostKind::WhpWindows),
            other => Err(SnapshotError::BadValue {
                what: "snapshot host kind",
                value: u64::from(other),
            }),
        }
    }
}

/// Guest architecture. One value today; present so a snapshot from a future
/// aarch64 port is refused by name rather than misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
}

impl Arch {
    pub const fn current() -> Self {
        Arch::X86_64
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86-64",
        }
    }

    const fn code(self) -> u32 {
        1
    }

    fn from_code(code: u32) -> Result<Self> {
        match code {
            1 => Ok(Arch::X86_64),
            other => Err(SnapshotError::BadValue {
                what: "snapshot architecture",
                value: u64::from(other),
            }),
        }
    }
}

/// What one section holds.
///
/// The discriminants are part of the format. An unknown one is a refusal: a
/// section this build cannot decode is state the guest is expecting and would
/// silently not get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionKind {
    /// The VM's identity and shape, and the fingerprints every refusal is
    /// checked against. Always the first section.
    Metadata,
    /// One vCPU's full architectural state. `instance` is the vCPU index.
    Cpu,
    /// VM-wide time: the guest clock and how the TSC was left.
    Clock,
    /// One guest RAM region, zero pages skipped. `instance` is the region index.
    Memory,
    /// The 8259/8254/IOAPIC set, on a host that runs them in userspace.
    IrqChip,
    /// The 16550 register file.
    Serial,
    /// The firmware platform stub: RTC/CMOS and the host-bridge config latch.
    Platform,
    /// The ACPI PM register block, its timer and its latches.
    AcpiPm,
    /// The CFI flash command state machine (the contents live in the file).
    Pflash,
    /// The PCI root bus: the config address latch and every function's
    /// register file.
    PciRoot,
    /// One virtio transport slot. `instance` is the slot index.
    Virtio,
    /// The guest reset controls' latches.
    ResetControl,
}

impl SectionKind {
    const fn code(self) -> u32 {
        match self {
            SectionKind::Metadata => 1,
            SectionKind::Cpu => 2,
            SectionKind::Clock => 3,
            SectionKind::Memory => 4,
            SectionKind::IrqChip => 5,
            SectionKind::Serial => 6,
            SectionKind::Platform => 7,
            SectionKind::AcpiPm => 8,
            SectionKind::Pflash => 9,
            SectionKind::PciRoot => 10,
            SectionKind::Virtio => 11,
            SectionKind::ResetControl => 12,
        }
    }

    fn from_code(code: u32) -> Result<Self> {
        Ok(match code {
            1 => SectionKind::Metadata,
            2 => SectionKind::Cpu,
            3 => SectionKind::Clock,
            4 => SectionKind::Memory,
            5 => SectionKind::IrqChip,
            6 => SectionKind::Serial,
            7 => SectionKind::Platform,
            8 => SectionKind::AcpiPm,
            9 => SectionKind::Pflash,
            10 => SectionKind::PciRoot,
            11 => SectionKind::Virtio,
            12 => SectionKind::ResetControl,
            other => return Err(SnapshotError::UnknownSection { kind: other }),
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            SectionKind::Metadata => "metadata",
            SectionKind::Cpu => "cpu",
            SectionKind::Clock => "clock",
            SectionKind::Memory => "memory",
            SectionKind::IrqChip => "irqchip",
            SectionKind::Serial => "serial",
            SectionKind::Platform => "platform",
            SectionKind::AcpiPm => "acpi-pm",
            SectionKind::Pflash => "pflash",
            SectionKind::PciRoot => "pci-root",
            SectionKind::Virtio => "virtio",
            SectionKind::ResetControl => "reset-control",
        }
    }
}

/// One row of the section index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionEntry {
    pub kind: SectionKind,
    /// Per-section version, so one device's encoding can change without
    /// invalidating the whole format.
    pub version: u32,
    /// Which one: vCPU index, memory region, virtio slot. 0 for singletons.
    pub instance: u32,
    pub offset: u64,
    pub len: u64,
    pub digest: [u8; 32],
}

impl SectionEntry {
    fn label(&self) -> String {
        format!("{}[{}]", self.kind.as_str(), self.instance)
    }
}

// ------------------------------------------------------------------- writing

/// Writes a snapshot: sections in order, then the index, then the header patch.
pub struct SnapshotWriter<W: Write + Seek> {
    out: W,
    /// Where the next section starts.
    pos: u64,
    index: Vec<SectionEntry>,
    host: HostKind,
}

impl<W: Write + Seek> SnapshotWriter<W> {
    /// Starts a snapshot, leaving room for the header that is patched in by
    /// [`Self::finish`].
    pub fn create(mut out: W, host: HostKind) -> Result<Self> {
        out.seek(SeekFrom::Start(0))
            .map_err(SnapshotError::io("seeking to the header"))?;
        out.write_all(&[0u8; HEADER_LEN as usize])
            .map_err(SnapshotError::io("reserving the header"))?;
        Ok(Self {
            out,
            pos: HEADER_LEN,
            index: Vec::new(),
            host,
        })
    }

    /// Appends a section whose payload is already in memory.
    pub fn put(
        &mut self,
        kind: SectionKind,
        version: u32,
        instance: u32,
        payload: &[u8],
    ) -> Result<()> {
        let mut section = self.section(kind, version, instance)?;
        section
            .write_all(payload)
            .map_err(SnapshotError::io("writing a section"))?;
        section.finish()
    }

    /// Opens a section for streaming. The section is recorded when the returned
    /// writer's [`SectionWriter::finish`] is called.
    pub fn section(
        &mut self,
        kind: SectionKind,
        version: u32,
        instance: u32,
    ) -> Result<SectionWriter<'_, W>> {
        if self.index.len() >= MAX_SECTIONS {
            return Err(SnapshotError::TooLarge {
                what: "section index",
                value: self.index.len() as u64 + 1,
                max: MAX_SECTIONS as u64,
            });
        }
        Ok(SectionWriter {
            owner: self,
            kind,
            version,
            instance,
            hasher: Sha256::new(),
            written: 0,
        })
    }

    /// Writes the index and the header. Returns the file's total length.
    pub fn finish(mut self) -> Result<u64> {
        let mut index = Writer::with_capacity(8 + self.index.len() * INDEX_ENTRY_LEN);
        index.count(self.index.len());
        for entry in &self.index {
            index
                .u32(entry.kind.code())
                .u32(entry.version)
                .u32(entry.instance)
                .u32(0)
                .u64(entry.offset)
                .u64(entry.len);
            index.raw(&entry.digest);
        }
        let index = index.into_bytes();
        let index_offset = self.pos;
        self.out
            .write_all(&index)
            .map_err(SnapshotError::io("writing the section index"))?;

        let mut header = Writer::with_capacity(HEADER_LEN as usize);
        header.raw(&MAGIC);
        header
            .u32(VERSION)
            .u32(0)
            .u32(self.host.code())
            .u32(Arch::current().code())
            .u64(index_offset)
            .u64(index.len() as u64);
        header.raw(&Sha256::digest(&index));
        let header = header.into_bytes();
        debug_assert_eq!(header.len() as u64, HEADER_LEN);

        self.out
            .seek(SeekFrom::Start(0))
            .map_err(SnapshotError::io("seeking back to the header"))?;
        self.out
            .write_all(&header)
            .map_err(SnapshotError::io("writing the header"))?;
        self.out
            .flush()
            .map_err(SnapshotError::io("flushing the snapshot"))?;
        Ok(index_offset + index.len() as u64)
    }
}

/// A section being streamed into the file.
pub struct SectionWriter<'a, W: Write + Seek> {
    owner: &'a mut SnapshotWriter<W>,
    kind: SectionKind,
    version: u32,
    instance: u32,
    hasher: Sha256,
    written: u64,
}

impl<W: Write + Seek> Write for SectionWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.owner.out.write_all(buf)?;
        self.hasher.update(buf);
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.owner.out.flush()
    }
}

impl<W: Write + Seek> SectionWriter<'_, W> {
    /// Bytes written into this section so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Records the section in the index.
    pub fn finish(self) -> Result<()> {
        let entry = SectionEntry {
            kind: self.kind,
            version: self.version,
            instance: self.instance,
            offset: self.owner.pos,
            len: self.written,
            digest: self.hasher.finalize().into(),
        };
        self.owner.pos += self.written;
        self.owner.index.push(entry);
        Ok(())
    }
}

// ------------------------------------------------------------------- reading

/// Reads a snapshot's header and index, and then whichever sections are asked
/// for.
#[derive(Debug)]
pub struct SnapshotReader<R: Read + Seek + std::fmt::Debug> {
    source: R,
    host: HostKind,
    arch: Arch,
    index: Vec<SectionEntry>,
    file_len: u64,
}

impl<R: Read + Seek + std::fmt::Debug> SnapshotReader<R> {
    /// Parses the header and the section index.
    ///
    /// Everything a hostile or corrupt file could say is checked here: the
    /// magic, the version, unknown flags, the index's own digest, and every
    /// section's offset and length against the actual file length. A caller
    /// that gets a reader back knows the index is internally consistent.
    pub fn open(mut source: R) -> Result<Self> {
        let file_len = source
            .seek(SeekFrom::End(0))
            .map_err(SnapshotError::io("measuring the snapshot"))?;
        if file_len < HEADER_LEN {
            return Err(SnapshotError::Truncated {
                what: "snapshot header",
                need: HEADER_LEN,
                have: file_len,
            });
        }
        source
            .seek(SeekFrom::Start(0))
            .map_err(SnapshotError::io("seeking to the header"))?;
        let mut header = [0u8; HEADER_LEN as usize];
        source
            .read_exact(&mut header)
            .map_err(SnapshotError::io("reading the header"))?;

        let mut r = Reader::new(&header);
        let magic = r.take("magic", 8)?;
        if magic != MAGIC {
            return Err(SnapshotError::NotASnapshot {
                expected: "ENTGLSNP",
            });
        }
        let version = r.u32("format version")?;
        if version != VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                found: version,
                expected: VERSION,
            });
        }
        let flags = r.u32("header flags")?;
        if flags != 0 {
            return Err(SnapshotError::UnknownFlags { flags });
        }
        let host = HostKind::from_code(r.u32("host kind")?)?;
        let arch = Arch::from_code(r.u32("architecture")?)?;
        let index_offset = r.u64("index offset")?;
        let index_len = r.u64("index length")?;
        let mut index_digest = [0u8; 32];
        index_digest.copy_from_slice(r.take("index digest", 32)?);
        r.finish("snapshot header")?;

        let index_end = index_offset
            .checked_add(index_len)
            .ok_or(SnapshotError::BadValue {
                what: "index offset + length",
                value: index_offset,
            })?;
        if index_offset < HEADER_LEN || index_end > file_len {
            return Err(SnapshotError::Truncated {
                what: "section index",
                need: index_end,
                have: file_len,
            });
        }
        let max_index = 8 + MAX_SECTIONS * INDEX_ENTRY_LEN;
        if index_len > max_index as u64 {
            return Err(SnapshotError::TooLarge {
                what: "section index",
                value: index_len,
                max: max_index as u64,
            });
        }
        source
            .seek(SeekFrom::Start(index_offset))
            .map_err(SnapshotError::io("seeking to the section index"))?;
        // Bounded above by `max_index`, which is why this allocation is safe to
        // size from the file.
        let mut index_bytes = vec![0u8; index_len as usize];
        source
            .read_exact(&mut index_bytes)
            .map_err(SnapshotError::io("reading the section index"))?;
        if Sha256::digest(&index_bytes).as_slice() != index_digest {
            return Err(SnapshotError::Corrupt {
                section: "section index".into(),
            });
        }

        let mut r = Reader::new(&index_bytes);
        let count = r.count("section index", MAX_SECTIONS, INDEX_ENTRY_LEN)?;
        let mut index = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = SectionKind::from_code(r.u32("section kind")?)?;
            let version = r.u32("section version")?;
            let instance = r.u32("section instance")?;
            let reserved = r.u32("section reserved")?;
            if reserved != 0 {
                return Err(SnapshotError::BadValue {
                    what: "section reserved word",
                    value: u64::from(reserved),
                });
            }
            let offset = r.u64("section offset")?;
            let len = r.u64("section length")?;
            let mut digest = [0u8; 32];
            digest.copy_from_slice(r.take("section digest", 32)?);
            let end = offset.checked_add(len).ok_or(SnapshotError::BadValue {
                what: "section offset + length",
                value: offset,
            })?;
            // A section must live between the header and the index; anything
            // else is a file whose parts overlap or point outside itself.
            if offset < HEADER_LEN || end > index_offset {
                return Err(SnapshotError::Truncated {
                    what: "section payload",
                    need: end,
                    have: index_offset,
                });
            }
            let entry = SectionEntry {
                kind,
                version,
                instance,
                offset,
                len,
                digest,
            };
            if index
                .iter()
                .any(|e: &SectionEntry| e.kind == kind && e.instance == instance)
            {
                return Err(SnapshotError::DuplicateSection {
                    section: kind.as_str().into(),
                    instance,
                });
            }
            index.push(entry);
        }
        r.finish("section index")?;

        Ok(Self {
            source,
            host,
            arch,
            index,
            file_len,
        })
    }

    pub fn host(&self) -> HostKind {
        self.host
    }

    pub fn arch(&self) -> Arch {
        self.arch
    }

    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    pub fn sections(&self) -> &[SectionEntry] {
        &self.index
    }

    /// Refuses a snapshot taken on a different hypervisor or architecture.
    pub fn require_native(&self) -> Result<()> {
        let host = HostKind::current().ok_or_else(|| SnapshotError::ForeignHost {
            snapshot: self.host.as_str().into(),
            host: "a host with no hypervisor backend".into(),
        })?;
        if host != self.host {
            return Err(SnapshotError::ForeignHost {
                snapshot: self.host.as_str().into(),
                host: host.as_str().into(),
            });
        }
        if self.arch != Arch::current() {
            return Err(SnapshotError::ForeignArch {
                snapshot: self.arch.as_str().into(),
                host: Arch::current().as_str().into(),
            });
        }
        Ok(())
    }

    /// Every instance of `kind` present, in index order.
    pub fn instances(&self, kind: SectionKind) -> Vec<u32> {
        let mut found: Vec<u32> = self
            .index
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.instance)
            .collect();
        found.sort_unstable();
        found
    }

    fn entry(&self, kind: SectionKind, instance: u32) -> Option<SectionEntry> {
        self.index
            .iter()
            .find(|e| e.kind == kind && e.instance == instance)
            .copied()
    }

    /// Reads one section into memory and verifies its digest.
    pub fn read_section(&mut self, kind: SectionKind, instance: u32) -> Result<Vec<u8>> {
        let entry = self
            .entry(kind, instance)
            .ok_or(SnapshotError::MissingSection(kind.as_str()))?;
        if entry.len > MAX_SECTION_BYTES as u64 {
            return Err(SnapshotError::TooLarge {
                what: "section payload",
                value: entry.len,
                max: MAX_SECTION_BYTES as u64,
            });
        }
        self.source
            .seek(SeekFrom::Start(entry.offset))
            .map_err(SnapshotError::io("seeking to a section"))?;
        // Bounded by `MAX_SECTION_BYTES` *and* by the file length the index
        // check enforced, so this cannot be steered into a huge allocation.
        let mut bytes = vec![0u8; entry.len as usize];
        self.source
            .read_exact(&mut bytes)
            .map_err(SnapshotError::io("reading a section"))?;
        if Sha256::digest(&bytes).as_slice() != entry.digest {
            return Err(SnapshotError::Corrupt {
                section: entry.label(),
            });
        }
        Ok(bytes)
    }

    /// Reads one section in chunks, verifying the digest **after** the last
    /// chunk.
    ///
    /// The callback therefore sees bytes that have not been verified yet, which
    /// is unavoidable for a multi-gigabyte memory section: buffering it to check
    /// first would defeat the point. The contract is that the caller's writes go
    /// somewhere it is prepared to throw away — for guest memory, a VM that is
    /// never started, because a failed restore never spawns a vCPU.
    pub fn stream_section(
        &mut self,
        kind: SectionKind,
        instance: u32,
        mut sink: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<u64> {
        let entry = self
            .entry(kind, instance)
            .ok_or(SnapshotError::MissingSection(kind.as_str()))?;
        self.source
            .seek(SeekFrom::Start(entry.offset))
            .map_err(SnapshotError::io("seeking to a section"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; STREAM_CHUNK];
        let mut left = entry.len;
        while left > 0 {
            let want = left.min(STREAM_CHUNK as u64) as usize;
            let chunk = &mut buf[..want];
            self.source
                .read_exact(chunk)
                .map_err(SnapshotError::io("reading a streamed section"))?;
            hasher.update(&chunk[..]);
            sink(chunk)?;
            left -= want as u64;
        }
        if hasher.finalize().as_slice() != entry.digest {
            return Err(SnapshotError::Corrupt {
                section: entry.label(),
            });
        }
        Ok(entry.len)
    }

    /// The section version, for a decoder that wants to check it itself.
    pub fn section_version(&self, kind: SectionKind, instance: u32) -> Option<u32> {
        self.entry(kind, instance).map(|e| e.version)
    }

    /// Refuses a section written at a different version than this build reads.
    pub fn require_version(&self, kind: SectionKind, instance: u32, expected: u32) -> Result<()> {
        match self.section_version(kind, instance) {
            Some(found) if found == expected => Ok(()),
            Some(found) => Err(SnapshotError::SectionVersion {
                section: kind.as_str(),
                found,
                expected,
            }),
            None => Err(SnapshotError::MissingSection(kind.as_str())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn write_sample() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        w.put(SectionKind::Metadata, 1, 0, b"meta").unwrap();
        w.put(SectionKind::Cpu, 1, 0, b"cpu0").unwrap();
        w.put(SectionKind::Cpu, 1, 1, b"cpu1").unwrap();
        {
            let mut section = w.section(SectionKind::Memory, 1, 0).unwrap();
            section.write_all(&[0xab; 4096]).unwrap();
            assert_eq!(section.written(), 4096);
            section.finish().unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn a_snapshot_round_trips_through_the_container() {
        let bytes = write_sample();
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        assert_eq!(r.host(), HostKind::KvmLinux);
        assert_eq!(r.arch(), Arch::X86_64);
        assert_eq!(r.instances(SectionKind::Cpu), vec![0, 1]);
        assert_eq!(r.read_section(SectionKind::Metadata, 0).unwrap(), b"meta");
        assert_eq!(r.read_section(SectionKind::Cpu, 1).unwrap(), b"cpu1");

        let mut seen = 0u64;
        let len = r
            .stream_section(SectionKind::Memory, 0, |chunk| {
                assert!(chunk.iter().all(|&b| b == 0xab));
                seen += chunk.len() as u64;
                Ok(())
            })
            .unwrap();
        assert_eq!(len, 4096);
        assert_eq!(seen, 4096);
    }

    #[test]
    fn a_missing_section_is_named() {
        let bytes = write_sample();
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let err = r.read_section(SectionKind::Pflash, 0).unwrap_err();
        assert!(
            matches!(err, SnapshotError::MissingSection("pflash")),
            "{err}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_snapshot_is_refused() {
        let err = SnapshotReader::open(Cursor::new(vec![0u8; 256])).unwrap_err();
        assert!(matches!(err, SnapshotError::NotASnapshot { .. }), "{err}");
    }

    #[test]
    fn an_empty_or_short_file_is_refused_as_truncated() {
        for len in [0usize, 1, 8, 71] {
            let err = SnapshotReader::open(Cursor::new(vec![0u8; len])).unwrap_err();
            assert!(
                matches!(err, SnapshotError::Truncated { .. }),
                "{len}: {err}"
            );
        }
    }

    #[test]
    fn a_wrong_format_version_is_refused_by_number() {
        let mut bytes = write_sample();
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        let err = SnapshotReader::open(Cursor::new(&bytes)).unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotError::UnsupportedVersion {
                    found: 99,
                    expected: VERSION
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn unknown_header_flags_are_refused() {
        let mut bytes = write_sample();
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        let err = SnapshotReader::open(Cursor::new(&bytes)).unwrap_err();
        assert!(
            matches!(err, SnapshotError::UnknownFlags { flags: 1 }),
            "{err}"
        );
    }

    /// The refusal that matters most: a snapshot that was cut short — a full
    /// disk, an interrupted copy — must not restore a guest with a hole in it.
    #[test]
    fn a_truncated_file_is_refused() {
        let bytes = write_sample();
        for cut in [80usize, 200, bytes.len() - 1] {
            let err = SnapshotReader::open(Cursor::new(&bytes[..cut])).unwrap_err();
            assert!(
                matches!(
                    err,
                    SnapshotError::Truncated { .. }
                        | SnapshotError::Corrupt { .. }
                        | SnapshotError::Io { .. }
                ),
                "cut at {cut}: {err}"
            );
        }
    }

    /// A flipped bit inside a section is caught by that section's digest, not
    /// by the guest an hour later.
    #[test]
    fn a_corrupt_section_is_caught_by_its_digest() {
        let mut bytes = write_sample();
        // The first section payload starts right after the header.
        bytes[HEADER_LEN as usize] ^= 0xff;
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let err = r.read_section(SectionKind::Metadata, 0).unwrap_err();
        assert!(matches!(err, SnapshotError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn a_corrupt_streamed_section_is_caught_too() {
        let mut bytes = write_sample();
        let at = HEADER_LEN as usize + 4 + 4 + 4 + 100;
        bytes[at] ^= 0xff;
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let err = r
            .stream_section(SectionKind::Memory, 0, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, SnapshotError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn a_tampered_index_is_caught_by_the_header_digest() {
        let bytes = write_sample();
        let index_offset = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let mut bytes = bytes;
        bytes[index_offset + 8] ^= 0xff;
        let err = SnapshotReader::open(Cursor::new(&bytes)).unwrap_err();
        assert!(matches!(err, SnapshotError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn an_unknown_section_kind_is_refused_rather_than_skipped() {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        w.put(SectionKind::Metadata, 1, 0, b"meta").unwrap();
        w.finish().unwrap();
        let mut bytes = buf.into_inner();
        // Rewrite the one index entry's kind, then re-digest the index so the
        // failure is the unknown kind rather than the checksum.
        let index_offset = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let index_len = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        bytes[index_offset + 8..index_offset + 12].copy_from_slice(&4242u32.to_le_bytes());
        let digest = Sha256::digest(&bytes[index_offset..index_offset + index_len]);
        bytes[40..72].copy_from_slice(&digest);
        let err = SnapshotReader::open(Cursor::new(&bytes)).unwrap_err();
        assert!(
            matches!(err, SnapshotError::UnknownSection { kind: 4242 }),
            "{err}"
        );
    }

    #[test]
    fn the_host_refusal_names_both_sides() {
        let mut buf = Cursor::new(Vec::new());
        let other = match HostKind::current() {
            Some(HostKind::KvmLinux) => HostKind::WhpWindows,
            _ => HostKind::KvmLinux,
        };
        let mut w = SnapshotWriter::create(&mut buf, other).unwrap();
        w.put(SectionKind::Metadata, 1, 0, b"meta").unwrap();
        w.finish().unwrap();
        let bytes = buf.into_inner();
        let r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let err = r.require_native().unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotError::ForeignHost { .. } | SnapshotError::ForeignArch { .. }
            ),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains(other.as_str()), "{text}");
    }
}
