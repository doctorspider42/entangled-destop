//! Guest RAM, sparsely.
//!
//! One section per memory region — a guest larger than the 32-bit MMIO hole has
//! two, low RAM up to 3 GiB and the remainder at 4 GiB — encoded as a run of
//! `(offset, length, bytes)` triples covering only the pages that are not
//! entirely zero:
//!
//! ```text
//!   gpa u64 | region length u64 | page size u32 | reserved u32
//!   ( offset u64 | length u64 | length bytes ) *   until the section ends
//! ```
//!
//! **Why skipping zeroes is not an optimisation.** A guest is handed
//! zero-filled RAM and touches a fraction of it; an idle 4 GiB desktop VM has
//! a few hundred megabytes of non-zero pages. Writing the rest would make every
//! suspend a 4 GiB write and every resume a 4 GiB read, which is the difference
//! between suspend being usable and being a feature nobody turns on. The
//! restore side needs no special case: both hypervisors hand out zero-filled
//! pages, so a page that was skipped is already what it was.
//!
//! **Why runs rather than file holes.** A hole-punched file would have the
//! guest's *apparent* size even when almost none of it is allocated, and the
//! first `cp` of it would expand to the full size. The run list keeps the file
//! itself small, and the snapshot is still marked sparse
//! (`disk_image::ops::mark_sparse`) because the sections around the memory are
//! written by seeking.

use std::io::Write;

use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryRegion};

use crate::codec::{Reader, Writer};
use crate::error::{Result, SnapshotError};
use crate::format::{SectionKind, SnapshotReader, SnapshotWriter};

/// Version of the memory section's encoding.
pub const MEMORY_VERSION: u32 = 1;

/// Granularity zero-detection works at. The architectural page: coarser would
/// keep zeroes, finer would multiply the run list for no gain.
pub const PAGE: u64 = 4096;

/// How much memory is read from the guest at a time while scanning.
const SCAN_CHUNK: usize = 1 << 20;

/// Fixed part at the front of a memory section.
const REGION_HEADER: usize = 8 + 8 + 4 + 4;

/// What one save moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryStats {
    /// Guest RAM in total.
    pub total_bytes: u64,
    /// Bytes actually written (the non-zero pages).
    pub saved_bytes: u64,
    /// How many contiguous non-zero runs they formed.
    pub runs: u64,
}

impl MemoryStats {
    /// Saved bytes as a percentage of guest RAM, for the log line.
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.saved_bytes as f64 / self.total_bytes as f64) * 100.0
    }

    fn add(&mut self, other: MemoryStats) {
        self.total_bytes += other.total_bytes;
        self.saved_bytes += other.saved_bytes;
        self.runs += other.runs;
    }
}

/// Writes every region of `mem` into `out`, one section each.
///
/// Must be called with the VM quiesced: nothing may write guest memory while it
/// is being read, or the snapshot is a torn picture of a machine that never
/// existed. That is exactly what the lifecycle seam's pause guarantees
/// (ADR-0005), which is why suspend is built on it rather than beside it.
pub fn save<M, W>(mem: &M, out: &mut SnapshotWriter<W>) -> Result<MemoryStats>
where
    M: GuestMemory,
    W: Write + std::io::Seek,
{
    let mut total = MemoryStats::default();
    for (index, region) in mem.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| SnapshotError::TooLarge {
            what: "memory region index",
            value: u64::MAX,
            max: u64::from(u32::MAX),
        })?;
        total.add(save_region(mem, region, index, out)?);
    }
    Ok(total)
}

fn save_region<M, W>(
    mem: &M,
    region: &M::R,
    index: u32,
    out: &mut SnapshotWriter<W>,
) -> Result<MemoryStats>
where
    M: GuestMemory,
    W: Write + std::io::Seek,
{
    let gpa = region.start_addr().0;
    let len = region.len();
    let mut section = out.section(SectionKind::Memory, MEMORY_VERSION, index)?;
    let mut header = Writer::with_capacity(REGION_HEADER);
    header.u64(gpa).u64(len).u32(PAGE as u32).u32(0);
    section
        .write_all(header.as_bytes())
        .map_err(SnapshotError::io("writing a memory region header"))?;

    let mut stats = MemoryStats {
        total_bytes: len,
        ..MemoryStats::default()
    };
    let mut buf = vec![0u8; SCAN_CHUNK];
    // The run currently being built, as an offset into the region plus how much
    // of `pending` belongs to it. Runs never span a chunk boundary in the
    // buffer, but they do span it in the file: a run that reaches the end of one
    // chunk is flushed and the next chunk starts a new one. Coalescing across
    // chunks would need the previous chunk's bytes to still be in hand.
    let mut offset = 0u64;
    while offset < len {
        let want = (len - offset).min(SCAN_CHUNK as u64) as usize;
        let chunk = &mut buf[..want];
        mem.read_slice(chunk, GuestAddress(gpa + offset))
            .map_err(|e| {
                SnapshotError::Restore(format!("reading guest memory at {gpa:#x}: {e}"))
            })?;

        let mut run_start: Option<usize> = None;
        let mut at = 0usize;
        while at < want {
            let page_end = (at + PAGE as usize).min(want);
            let page = &chunk[at..page_end];
            let empty = page.iter().all(|&b| b == 0);
            match (empty, run_start) {
                (false, None) => run_start = Some(at),
                (true, Some(start)) => {
                    write_run(&mut section, offset + start as u64, &chunk[start..at])?;
                    stats.saved_bytes += (at - start) as u64;
                    stats.runs += 1;
                    run_start = None;
                }
                _ => {}
            }
            at = page_end;
        }
        if let Some(start) = run_start {
            write_run(&mut section, offset + start as u64, &chunk[start..want])?;
            stats.saved_bytes += (want - start) as u64;
            stats.runs += 1;
        }
        offset += want as u64;
    }
    section.finish()?;
    Ok(stats)
}

fn write_run<W: Write>(out: &mut W, offset: u64, bytes: &[u8]) -> Result<()> {
    let mut header = Writer::with_capacity(16);
    header.u64(offset).u64(bytes.len() as u64);
    out.write_all(header.as_bytes())
        .map_err(SnapshotError::io("writing a memory run header"))?;
    out.write_all(bytes)
        .map_err(SnapshotError::io("writing a memory run"))
}

/// Reads every memory section back into `mem`.
///
/// The memory must be freshly allocated (both hypervisors zero it), because
/// only the non-zero runs are in the file. A restore that reused a dirty
/// allocation would leave the previous contents in every skipped page.
pub fn restore<M, R>(mem: &M, input: &mut SnapshotReader<R>) -> Result<MemoryStats>
where
    M: GuestMemory,
    R: std::io::Read + std::io::Seek + std::fmt::Debug,
{
    let regions: Vec<(u64, u64)> = mem.iter().map(|r| (r.start_addr().0, r.len())).collect();
    let present = input.instances(SectionKind::Memory);
    if present.len() != regions.len() {
        return Err(SnapshotError::Mismatch {
            field: "memory regions".into(),
            snapshot: present.len().to_string(),
            current: regions.len().to_string(),
        });
    }
    let mut total = MemoryStats::default();
    for (index, &(gpa, len)) in regions.iter().enumerate() {
        let index = index as u32;
        input.require_version(SectionKind::Memory, index, MEMORY_VERSION)?;
        total.add(restore_region(mem, input, index, gpa, len)?);
    }
    Ok(total)
}

/// Parser state for one streamed memory section.
///
/// The section arrives in fixed-size chunks that have nothing to do with the
/// run boundaries inside it, so this is a small state machine rather than a
/// straight-line decode: it accumulates the 16-byte header of the next run
/// across a chunk boundary, then copies bytes straight into guest memory until
/// the run is done.
struct RunParser<'a, M: GuestMemory> {
    mem: &'a M,
    gpa: u64,
    region_len: u64,
    /// Bytes of the fixed region header still to be consumed.
    header_left: usize,
    header: Vec<u8>,
    /// Bytes of the current run header collected so far.
    pending: Vec<u8>,
    /// Where the current run writes next, and how much of it is left.
    write_at: u64,
    run_left: u64,
    stats: MemoryStats,
}

impl<M: GuestMemory> RunParser<'_, M> {
    fn feed(&mut self, mut chunk: &[u8]) -> Result<()> {
        while !chunk.is_empty() {
            if self.header_left > 0 {
                let take = self.header_left.min(chunk.len());
                self.header.extend_from_slice(&chunk[..take]);
                self.header_left -= take;
                chunk = &chunk[take..];
                if self.header_left == 0 {
                    self.check_region_header()?;
                }
                continue;
            }
            if self.run_left > 0 {
                let take = (self.run_left.min(chunk.len() as u64)) as usize;
                self.mem
                    .write_slice(&chunk[..take], GuestAddress(self.write_at))
                    .map_err(|e| {
                        SnapshotError::Restore(format!(
                            "writing guest memory at {:#x}: {e}",
                            self.write_at
                        ))
                    })?;
                self.write_at += take as u64;
                self.run_left -= take as u64;
                chunk = &chunk[take..];
                continue;
            }
            let take = (16 - self.pending.len()).min(chunk.len());
            self.pending.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.pending.len() == 16 {
                let mut r = Reader::new(&self.pending);
                let offset = r.u64("memory run offset")?;
                let len = r.u64("memory run length")?;
                let end = offset.checked_add(len).ok_or(SnapshotError::BadValue {
                    what: "memory run offset + length",
                    value: offset,
                })?;
                if end > self.region_len {
                    return Err(SnapshotError::BadValue {
                        what: "memory run past the end of its region",
                        value: end,
                    });
                }
                self.write_at = self.gpa + offset;
                self.run_left = len;
                self.stats.saved_bytes += len;
                self.stats.runs += 1;
                self.pending.clear();
            }
        }
        Ok(())
    }

    fn check_region_header(&mut self) -> Result<()> {
        let mut r = Reader::new(&self.header);
        let gpa = r.u64("memory region gpa")?;
        let len = r.u64("memory region length")?;
        let page = r.u32("memory region page size")?;
        let reserved = r.u32("memory region reserved")?;
        if reserved != 0 {
            return Err(SnapshotError::BadValue {
                what: "memory region reserved word",
                value: u64::from(reserved),
            });
        }
        if page == 0 {
            return Err(SnapshotError::BadValue {
                what: "memory region page size",
                value: 0,
            });
        }
        if gpa != self.gpa || len != self.region_len {
            return Err(SnapshotError::Mismatch {
                field: "memory region".into(),
                snapshot: format!("{len} bytes at {gpa:#x}"),
                current: format!("{} bytes at {:#x}", self.region_len, self.gpa),
            });
        }
        Ok(())
    }

    /// A section that ended mid-run or mid-header is truncated, whatever the
    /// digest says about the bytes that did arrive.
    fn finish(self) -> Result<MemoryStats> {
        if self.header_left > 0 || self.run_left > 0 || !self.pending.is_empty() {
            return Err(SnapshotError::Truncated {
                what: "memory section",
                need: self.run_left + self.header_left as u64 + self.pending.len() as u64,
                have: 0,
            });
        }
        Ok(self.stats)
    }
}

fn restore_region<M, R>(
    mem: &M,
    input: &mut SnapshotReader<R>,
    index: u32,
    gpa: u64,
    len: u64,
) -> Result<MemoryStats>
where
    M: GuestMemory,
    R: std::io::Read + std::io::Seek + std::fmt::Debug,
{
    let mut parser = RunParser {
        mem,
        gpa,
        region_len: len,
        header_left: REGION_HEADER,
        header: Vec::with_capacity(REGION_HEADER),
        pending: Vec::with_capacity(16),
        write_at: 0,
        run_left: 0,
        stats: MemoryStats {
            total_bytes: len,
            ..MemoryStats::default()
        },
    };
    input.stream_section(SectionKind::Memory, index, |chunk| parser.feed(chunk))?;
    parser.finish()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use vm_memory::GuestMemoryMmap;

    use super::*;
    use crate::format::{HostKind, SnapshotReader, SnapshotWriter};

    fn memory(bytes: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), bytes)]).unwrap()
    }

    fn round_trip(source: &GuestMemoryMmap, target: &GuestMemoryMmap) -> (MemoryStats, u64) {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        let stats = save(source, &mut w).unwrap();
        let total = w.finish().unwrap();
        let bytes = buf.into_inner();
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let back = restore(target, &mut r).unwrap();
        assert_eq!(back.saved_bytes, stats.saved_bytes);
        (stats, total)
    }

    #[test]
    fn an_all_zero_guest_writes_almost_nothing() {
        let mem = memory(8 << 20);
        let target = memory(8 << 20);
        let (stats, file_len) = round_trip(&mem, &target);
        assert_eq!(stats.saved_bytes, 0);
        assert_eq!(stats.runs, 0);
        assert_eq!(stats.total_bytes, 8 << 20);
        assert!(
            file_len < 4096,
            "an empty 8 MiB guest produced {file_len} bytes"
        );
    }

    #[test]
    fn scattered_pages_come_back_byte_for_byte() {
        let mem = memory(4 << 20);
        let target = memory(4 << 20);
        let marks: [(u64, u8); 5] = [
            (0, 0x11),
            (PAGE, 0x22),
            (17 * PAGE, 0x33),
            ((1 << 20) + 3, 0x44),
            ((4 << 20) - 1, 0x55),
        ];
        for (at, value) in marks {
            mem.write_obj(value, GuestAddress(at)).unwrap();
        }
        let (stats, _) = round_trip(&mem, &target);
        // Five marks in four distinct pages, two of which are adjacent and
        // therefore one run.
        assert_eq!(stats.runs, 4, "{stats:?}");
        assert_eq!(stats.saved_bytes, 5 * PAGE);
        for (at, value) in marks {
            assert_eq!(
                target.read_obj::<u8>(GuestAddress(at)).unwrap(),
                value,
                "byte at {at:#x}"
            );
        }
        // And the pages that were zero are still zero.
        assert_eq!(target.read_obj::<u8>(GuestAddress(100 * PAGE)).unwrap(), 0);
    }

    #[test]
    fn a_full_guest_saves_everything() {
        let mem = memory(2 << 20);
        let target = memory(2 << 20);
        let filler: Vec<u8> = (0..(2usize << 20)).map(|i| (i % 251 + 1) as u8).collect();
        mem.write_slice(&filler, GuestAddress(0)).unwrap();
        let (stats, _) = round_trip(&mem, &target);
        assert_eq!(stats.saved_bytes, 2 << 20);
        let mut back = vec![0u8; 2 << 20];
        target.read_slice(&mut back, GuestAddress(0)).unwrap();
        assert_eq!(back, filler);
    }

    /// A run that crosses the 1 MiB scan chunk is split into two runs in the
    /// file, and must still land contiguously in the restored guest.
    #[test]
    fn a_run_across_the_scan_chunk_is_rejoined() {
        let mem = memory(4 << 20);
        let target = memory(4 << 20);
        let start = (1u64 << 20) - 2 * PAGE;
        let filler = vec![0xa5u8; 4 * PAGE as usize];
        mem.write_slice(&filler, GuestAddress(start)).unwrap();
        round_trip(&mem, &target);
        let mut back = vec![0u8; 4 * PAGE as usize];
        target.read_slice(&mut back, GuestAddress(start)).unwrap();
        assert_eq!(back, filler);
    }

    /// A guest with the >3 GiB split: two regions, two sections, and the high
    /// one's contents must not land in the low one.
    #[test]
    fn two_regions_stay_apart() {
        let low = 0xc000_0000u64;
        let high = 0x1_0000_0000u64;
        let build = || {
            GuestMemoryMmap::from_ranges(&[
                (GuestAddress(0), (4 << 20) as usize),
                (GuestAddress(high), (4 << 20) as usize),
            ])
            .unwrap()
        };
        let _ = low;
        let mem = build();
        let target = build();
        mem.write_obj(0xaau8, GuestAddress(PAGE)).unwrap();
        mem.write_obj(0xbbu8, GuestAddress(high + PAGE)).unwrap();
        round_trip(&mem, &target);
        assert_eq!(target.read_obj::<u8>(GuestAddress(PAGE)).unwrap(), 0xaa);
        assert_eq!(
            target.read_obj::<u8>(GuestAddress(high + PAGE)).unwrap(),
            0xbb
        );
    }

    /// Restoring into a differently-shaped guest is refused rather than
    /// half-applied.
    #[test]
    fn a_different_memory_shape_is_refused() {
        let mem = memory(2 << 20);
        mem.write_obj(1u8, GuestAddress(0)).unwrap();
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        save(&mem, &mut w).unwrap();
        w.finish().unwrap();
        let bytes = buf.into_inner();

        let target = memory(4 << 20);
        let mut r = SnapshotReader::open(Cursor::new(&bytes)).unwrap();
        let err = restore(&target, &mut r).unwrap_err();
        assert!(matches!(err, SnapshotError::Mismatch { .. }), "{err}");
    }
}
