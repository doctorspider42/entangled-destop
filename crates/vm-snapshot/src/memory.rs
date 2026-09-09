//! Guest RAM, sparsely.
//!
//! One section per memory region — a guest larger than the 32-bit MMIO hole has
//! two, low RAM up to 3 GiB and the remainder at 4 GiB — encoded as a run of
//! `(offset, length, bytes)` triples covering only the pages that are not
//! entirely zero:
//!
//! ```text
//!   gpa u64 | region length u64 | page size u32 | codec u32
//!   ( raw length u64 | stored length u64 | stored bytes ) *  until the section ends
//!
//!   a block, once decoded:
//!   ( offset u64 | length u64 | length bytes ) *  until the block ends
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
//!
//! # How the scan is fast (ADR-0006, the dirty-page round)
//!
//! Two changes, neither of which touches the format — a snapshot written by
//! this code is byte for byte the one the single-threaded version wrote:
//!
//! * **The guest is scanned where it lies.** The first version read every
//!   region into a 1 MiB scratch buffer and looked for zeroes there, which is a
//!   full-RAM `memcpy` performed to discover that three quarters of it is zero.
//!   `slab_bytes` takes the bounds-checked `VolatileSlice` the region already
//!   offers and reads through it in place.
//! * **Slabs are scanned in parallel.** The region is cut into [`SLAB`]-sized
//!   pieces handed to a small pool; each returns the encoded runs for its own
//!   piece and the writer emits them **in slab order**, so the output does not
//!   depend on how the threads were scheduled. Hashing and writing stay on one
//!   thread — a SHA-256 over a section is inherently serial — and overlap the
//!   scanning of the slabs behind them.
//!
//! Runs never span a slab boundary, exactly as they never spanned the old scan
//! chunk: a run that reaches the end of one slab is closed and the next slab
//! starts a new one. The restore side rejoins them because it writes by offset,
//! so the only visible effect is a slightly longer run list.
//!
//! # And then the file itself
//!
//! With the scan no longer the cost, what a suspend waits on is the write.
//! Guest RAM compresses, so each slab's runs go through **LZ4 block
//! compression** on the worker that scanned them — the one part of the pipeline
//! with cores to spare — and the writer hashes and writes what comes out. Two
//! consequences worth stating plainly:
//!
//! * the section's shape changed, so [`MEMORY_VERSION`] is 2 and a version-1
//!   memory section is a named refusal. That is the cost of the change, and it
//!   is the reason the compression is a *block* framing around the same run
//!   encoding rather than a new one: everything about how a run is spelled is
//!   unchanged, which keeps the diff — and the risk — to the framing;
//! * a codec is a decoder that allocates on somebody else's numbers, so both
//!   lengths of every block are bounded before anything is reserved, and a
//!   block that decompresses to a different length than it claims is refused
//!   rather than used short.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Mutex;

use vm_memory::{
    Bytes, GuestAddress, GuestMemory, GuestMemoryRegion, MemoryRegionAddress, VolatileSlice,
};
use vmm_core::hv::DirtyPages;

use crate::codec::{Reader, Writer};
use crate::error::{Result, SnapshotError};
use crate::format::{SectionKind, SnapshotReader, SnapshotWriter};

/// Version of the memory section's encoding.
///
/// 2 since the compressed block framing. The parallel scan on its own did not
/// bump it — the same runs, in the same order, with the same bytes — because a
/// version buys a refusal, and a refusal that protects nothing costs somebody
/// their snapshot for no reason. Compression does change what is in the file,
/// so it does.
pub const MEMORY_VERSION: u32 = 2;

/// How a memory block's bytes are stored.
///
/// Part of the format: the value is written into every region header and an
/// unknown one is refused, so a future codec is a refusal on an old build
/// rather than a mis-decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Stored bytes are the bytes. Both lengths must agree.
    None,
    /// LZ4 block format (no frame header: the block's own length and the
    /// section's SHA-256 already say what a frame would).
    Lz4Block,
}

impl Codec {
    const fn code(self) -> u32 {
        match self {
            Codec::None => 0,
            Codec::Lz4Block => 1,
        }
    }

    fn from_code(code: u32) -> Result<Self> {
        match code {
            0 => Ok(Codec::None),
            1 => Ok(Codec::Lz4Block),
            other => Err(SnapshotError::BadValue {
                what: "memory block codec",
                value: u64::from(other),
            }),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::Lz4Block => "lz4-block",
        }
    }
}

/// Set to `0` to write an uncompressed snapshot. For measurement, and for a
/// host where the CPU is scarcer than the disk.
pub const COMPRESS_ENV: &str = "ENTANGLED_SNAPSHOT_COMPRESS";

/// Largest block a decoder will decompress, and therefore the largest buffer a
/// file can make it allocate. A writer produces at most one slab of runs plus
/// their headers; this is an order of magnitude of headroom above that, so a
/// snapshot written by any plausible future slab size still reads, and a
/// corrupt length still cannot ask for a gigabyte.
const MAX_BLOCK_RAW: u64 = 64 << 20;

/// Largest *stored* block. LZ4's worst case is a little above its input, and an
/// uncompressed block is exactly its input.
const MAX_BLOCK_STORED: u64 = MAX_BLOCK_RAW + (1 << 20);

/// Granularity zero-detection works at. The architectural page: coarser would
/// keep zeroes, finer would multiply the run list for no gain.
pub const PAGE: u64 = 4096;

/// How much of a region one worker scans at a time.
///
/// The trade is between parallelism and the buffers in flight: a worker's
/// output is at most one slab of bytes, and the writer may be holding a few
/// while it waits for the one it needs next. 4 MiB keeps the worst case in tens
/// of megabytes and is still 1024 pages, so the run list barely notices the
/// boundaries.
pub const SLAB: u64 = 4 << 20;

/// Below this a region is scanned on the calling thread. Spawning a pool to
/// look at a few megabytes costs more than it saves.
const PARALLEL_FLOOR: u64 = 2 * SLAB;

/// Most workers the scan will use. Beyond this the writer is the bottleneck —
/// one SHA-256, one file — and the extra threads only enlarge the reorder
/// buffer.
const MAX_WORKERS: usize = 8;

/// Overrides the worker count, for measurement and for a host where a suspend
/// must not take every core.
pub const THREADS_ENV: &str = "ENTANGLED_SNAPSHOT_THREADS";

/// Fixed part at the front of a memory section.
const REGION_HEADER: usize = 8 + 8 + 4 + 4;

/// How a save is allowed to spend the machine.
///
/// Explicit rather than read from the environment deep inside the scan, so a
/// test can pin both and prove the output does not depend on either — which is
/// the property the whole parallel scheme rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOptions {
    /// Threads that scan and compress. One means the calling thread only.
    pub workers: usize,
    pub codec: Codec,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

impl SaveOptions {
    /// What a real suspend uses: every core up to [`MAX_WORKERS`], LZ4, unless
    /// [`THREADS_ENV`] or [`COMPRESS_ENV`] says otherwise.
    pub fn from_env() -> Self {
        let workers = match std::env::var(THREADS_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            Some(n) => n.max(1),
            None => std::thread::available_parallelism()
                .map(|n| n.get().min(MAX_WORKERS))
                .unwrap_or(1),
        };
        let codec = match std::env::var(COMPRESS_ENV).as_deref() {
            Ok("0") | Ok("no") | Ok("off") | Ok("false") => Codec::None,
            _ => Codec::Lz4Block,
        };
        Self { workers, codec }
    }
}

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
    M::R: Sync,
    W: Write + std::io::Seek,
{
    save_with(mem, out, SaveOptions::from_env())
}

/// [`save`] with the worker count and the codec chosen by the caller.
pub fn save_with<M, W>(
    mem: &M,
    out: &mut SnapshotWriter<W>,
    options: SaveOptions,
) -> Result<MemoryStats>
where
    M: GuestMemory,
    M::R: Sync,
    W: Write + std::io::Seek,
{
    let mut total = MemoryStats::default();
    for (index, region) in mem.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| SnapshotError::TooLarge {
            what: "memory region index",
            value: u64::MAX,
            max: u64::from(u32::MAX),
        })?;
        total.add(save_region(region, index, out, options)?);
    }
    Ok(total)
}

fn save_region<R, W>(
    region: &R,
    index: u32,
    out: &mut SnapshotWriter<W>,
    options: SaveOptions,
) -> Result<MemoryStats>
where
    R: GuestMemoryRegion + Sync,
    W: Write + std::io::Seek,
{
    let gpa = region.start_addr().0;
    let len = region.len();
    let codec = options.codec;
    let mut section = out.section(SectionKind::Memory, MEMORY_VERSION, index)?;
    let mut header = Writer::with_capacity(REGION_HEADER);
    header.u64(gpa).u64(len).u32(PAGE as u32).u32(codec.code());
    section
        .write_all(header.as_bytes())
        .map_err(SnapshotError::io("writing a memory region header"))?;

    let mut stats = MemoryStats {
        total_bytes: len,
        ..MemoryStats::default()
    };
    let slabs = len.div_ceil(SLAB);
    let workers = options.workers.min(slabs as usize).max(1);
    if len < PARALLEL_FLOOR || workers <= 1 {
        let mut staging = Staging::default();
        for slab in 0..slabs {
            let offset = slab * SLAB;
            let runs = scan_slab(region, offset, slab_len(len, slab))?;
            if runs.is_empty() {
                continue;
            }
            let block = build_block(region, &runs, codec, &mut staging, &mut stats)?;
            write_block(&mut section, block)?;
        }
    } else {
        scan_in_parallel(region, len, slabs, workers, codec, &mut section, &mut stats)?;
    }
    section.finish()?;
    Ok(stats)
}

/// How long slab `slab` is: a whole [`SLAB`] except for the last one.
fn slab_len(region_len: u64, slab: u64) -> u64 {
    (region_len - slab * SLAB).min(SLAB)
}

/// One slab's non-zero runs, as `(offset in the region, length)` pairs.
///
/// Deliberately **not** the bytes. An earlier attempt had each worker build the
/// encoded run bytes and hand them to the writer, which cost 512 MiB of
/// allocation and one extra copy per byte on a desktop-sized guest and made
/// four threads *slower* than one (measured; see ADR-0006). What the parallel
/// part is good at is touching two gigabytes to find out which quarter of it
/// matters; the answer is a few thousand pairs, and the writer reads the bytes
/// straight out of guest memory when it needs them.
type Runs = Vec<(u64, u64)>;

/// The host bytes of a range of the region, read through its own bounds check.
///
/// The whole point of the exercise: no copy. `get_slice` is the checked API —
/// it refuses an offset or a length outside the region — and what comes back is
/// a view of the very pages the guest runs on.
fn region_bytes<R: GuestMemoryRegion>(region: &R, offset: u64, len: u64) -> Result<&[u8]> {
    let len = usize::try_from(len).map_err(|_| SnapshotError::BadValue {
        what: "memory slab length",
        value: len,
    })?;
    let slice: VolatileSlice<'_, _> =
        region
            .get_slice(MemoryRegionAddress(offset), len)
            .map_err(|e| {
                SnapshotError::Restore(format!("reading guest memory at offset {offset:#x}: {e}"))
            })?;
    let guard = slice.ptr_guard();
    // SAFETY: `get_slice` has just bounds-checked `offset..offset + len`
    // against this region, so the pointer it hands back addresses exactly `len`
    // bytes of the region's own mapping — anonymous `mmap` on Linux,
    // `VirtualAlloc(MEM_COMMIT)` on Windows, both zero-filled by the kernel, so
    // none of it is uninitialised. The borrowed slice cannot outlive `region`,
    // which the caller holds.
    //
    // Reading it as a plain slice rather than through the volatile accessors is
    // sound because of this module's contract, restated in `save`: a snapshot is
    // taken at a lifecycle stop point with every vCPU parked and every host
    // worker quiesced (ADR-0005), so nothing writes guest memory while it is
    // read. A save taken without that guarantee is a torn picture of a machine
    // that never existed whether the read went through `read_slice` or not; this
    // makes the requirement explicit instead of paying a full-RAM `memcpy` to
    // hide it.
    Ok(unsafe { std::slice::from_raw_parts(guard.as_ptr(), len) })
}

/// Finds the non-zero runs of one slab. The expensive part, and the parallel
/// one: it reads every byte of the slab and returns a handful of pairs.
fn scan_slab<R: GuestMemoryRegion>(region: &R, offset: u64, len: u64) -> Result<Runs> {
    let bytes = region_bytes(region, offset, len)?;
    let mut runs = Runs::new();
    let mut run_start: Option<usize> = None;
    let mut at = 0usize;
    let want = bytes.len();
    while at < want {
        let page_end = (at + PAGE as usize).min(want);
        let empty = is_zero(&bytes[at..page_end]);
        match (empty, run_start) {
            (false, None) => run_start = Some(at),
            (true, Some(start)) => {
                runs.push((offset + start as u64, (at - start) as u64));
                run_start = None;
            }
            _ => {}
        }
        at = page_end;
    }
    if let Some(start) = run_start {
        runs.push((offset + start as u64, (want - start) as u64));
    }
    Ok(runs)
}

/// Whether a page is entirely zero.
///
/// Word at a time rather than byte at a time: `align_to` hands back the aligned
/// middle as `u64`s, which is eight times fewer comparisons and vectorises
/// cleanly, and the unaligned ends are at most seven bytes each. Guest pages are
/// page aligned in practice, so the ends are usually empty.
fn is_zero(page: &[u8]) -> bool {
    // SAFETY: `u64` has no invalid bit patterns and no padding, so any correctly
    // aligned run of eight initialised bytes is a valid `u64`. `align_to` is
    // what guarantees the alignment; the prefix and suffix it leaves over are
    // checked as bytes.
    let (head, words, tail) = unsafe { page.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// The two buffers one slab's worth of work needs, kept across slabs so a
/// whole snapshot costs two allocations per thread rather than two per slab.
///
/// Not a micro-optimisation: an earlier version allocated a fresh buffer per
/// slab and, on a desktop-sized guest, spent more time in the allocator's
/// `mmap`/`munmap` path than in the scan — four threads came out *slower* than
/// one (measured; ADR-0006).
#[derive(Default)]
struct Staging {
    /// The runs, spelled the way the format spells them, before compression.
    raw: Vec<u8>,
    /// What the codec made of them.
    stored: Vec<u8>,
}

/// What one worker hands the writer: which slab, what the block decodes to,
/// the stored bytes, and the counts for that slab alone.
type Ready = (u64, u64, Vec<u8>, MemoryStats);

/// One block ready for the file: the length it decodes to, and its bytes.
struct Block<'a> {
    raw_len: u64,
    stored: &'a [u8],
}

/// Gathers one slab's runs into `staging.raw` and compresses them.
///
/// The gather is the only copy of guest memory in the whole save, and it earns
/// itself twice over: it turns a hundred thousand small writes into one, and it
/// gives the codec a block big enough to find matches in.
fn build_block<'a, R: GuestMemoryRegion>(
    region: &R,
    runs: &Runs,
    codec: Codec,
    staging: &'a mut Staging,
    stats: &mut MemoryStats,
) -> Result<Block<'a>> {
    staging.raw.clear();
    for &(offset, len) in runs {
        let mut header = Writer::with_capacity(16);
        header.u64(offset).u64(len);
        staging.raw.extend_from_slice(header.as_bytes());
        staging
            .raw
            .extend_from_slice(region_bytes(region, offset, len)?);
        stats.saved_bytes += len;
        stats.runs += 1;
    }
    let raw_len = staging.raw.len() as u64;
    // A wholly zero slab: nothing to store, and nothing for a codec to be
    // asked to do with an empty input. The caller writes no block at all.
    if raw_len == 0 {
        return Ok(Block {
            raw_len: 0,
            stored: &staging.raw,
        });
    }
    match codec {
        Codec::None => Ok(Block {
            raw_len,
            stored: &staging.raw,
        }),
        Codec::Lz4Block => {
            let bound = lz4_flex::block::get_maximum_output_size(staging.raw.len());
            staging.stored.resize(bound, 0);
            let written = lz4_flex::block::compress_into(&staging.raw, &mut staging.stored)
                .map_err(|e| SnapshotError::Restore(format!("compressing a memory block: {e}")))?;
            staging.stored.truncate(written);
            Ok(Block {
                raw_len,
                stored: &staging.stored,
            })
        }
    }
}

/// Writes one block's framing and its bytes.
///
/// A slab with no runs — a wholly zero four megabytes, which is most of an idle
/// guest — writes **nothing at all**, not an empty block. That keeps the
/// all-zero case at the few hundred bytes it was before compression, and it is
/// what lets the decoder refuse a zero-length block outright rather than
/// looping on one.
fn write_block<W: Write>(out: &mut W, block: Block<'_>) -> Result<()> {
    debug_assert!(block.raw_len > 0, "an empty block must not be written");
    let mut header = Writer::with_capacity(16);
    header.u64(block.raw_len).u64(block.stored.len() as u64);
    out.write_all(header.as_bytes())
        .map_err(SnapshotError::io("writing a memory block header"))?;
    out.write_all(block.stored)
        .map_err(SnapshotError::io("writing a memory block"))
}

/// Scans the region's slabs on a pool and writes them **in slab order**.
///
/// The ordering is the whole contract: the file a four-thread run produces is
/// the file a one-thread run produces, so nothing downstream — the digest, the
/// restore, a diff of two snapshots — can tell how many cores were free.
///
/// The consumer always drains the channel into a reorder buffer before it waits
/// for the slab it needs next, which is what keeps a worker from blocking on a
/// full channel while the consumer blocks on that worker. What is buffered is
/// run *lists*, tens of bytes each, so the reorder buffer costs nothing whatever
/// order the slabs arrive in.
#[allow(clippy::too_many_arguments)]
fn scan_in_parallel<R, W>(
    region: &R,
    len: u64,
    slabs: u64,
    workers: usize,
    codec: Codec,
    out: &mut W,
    stats: &mut MemoryStats,
) -> Result<()>
where
    R: GuestMemoryRegion + Sync,
    W: Write,
{
    let next = AtomicU64::new(0);
    let failure: Mutex<Option<SnapshotError>> = Mutex::new(None);
    // Deep enough that a worker rarely blocks, small enough that the scan does
    // not run far ahead of a slow disk.
    let (tx, rx) = sync_channel::<Ready>(workers * 2);
    // Compressed blocks come back here to be filled again. A worker that finds
    // the pool empty allocates one; the writer puts a used one back unless the
    // pool is already deep enough. That is the whole recycling scheme, and it is
    // enough: the steady state is a fixed set of buffers going round, so a
    // multi-gigabyte guest costs a handful of allocations rather than one per
    // slab. (Allocating per slab is what made four threads slower than one in
    // the first attempt — ADR-0006.)
    let pool: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
    let depth = workers * 4;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let failure = &failure;
            let pool = &pool;
            scope.spawn(move || {
                let mut staging = Staging::default();
                loop {
                    let slab = next.fetch_add(1, Ordering::Relaxed);
                    if slab >= slabs {
                        return;
                    }
                    // Per-slab counts, summed by the writer in slab order, so
                    // the totals do not depend on the scheduling either.
                    let mut mine = MemoryStats::default();
                    let mut buf = pool
                        .lock()
                        .ok()
                        .and_then(|mut p| p.pop())
                        .unwrap_or_default();
                    buf.clear();
                    let outcome =
                        scan_slab(region, slab * SLAB, slab_len(len, slab)).and_then(|runs| {
                            let block = build_block(region, &runs, codec, &mut staging, &mut mine)?;
                            buf.extend_from_slice(block.stored);
                            Ok(block.raw_len)
                        });
                    match outcome {
                        // A closed channel means the writer gave up; stop
                        // scanning rather than working for nobody.
                        Ok(raw_len) => {
                            if tx.send((slab, raw_len, buf, mine)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            if let Ok(mut slot) = failure.lock() {
                                slot.get_or_insert(error);
                            }
                            // Claim the rest so the other workers stop too.
                            next.store(slabs, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);

        let mut pending: BTreeMap<u64, (u64, Vec<u8>, MemoryStats)> = BTreeMap::new();
        let mut wanted = 0u64;
        let mut result = Ok(());
        while wanted < slabs {
            match pending.remove(&wanted) {
                Some((raw_len, buf, mine)) => {
                    // A wholly zero slab produced no runs and writes nothing.
                    let outcome = if raw_len == 0 {
                        Ok(())
                    } else {
                        write_block(
                            out,
                            Block {
                                raw_len,
                                stored: &buf,
                            },
                        )
                    };
                    stats.saved_bytes += mine.saved_bytes;
                    stats.runs += mine.runs;
                    if let Ok(mut p) = pool.lock() {
                        if p.len() < depth {
                            p.push(buf);
                        }
                    }
                    if let Err(error) = outcome {
                        result = Err(error);
                        break;
                    }
                    wanted += 1;
                }
                None => match rx.recv() {
                    Ok((slab, raw_len, buf, mine)) => {
                        pending.insert(slab, (raw_len, buf, mine));
                    }
                    // Every sender is gone and the slab we need has not
                    // arrived: a worker failed, or one died.
                    Err(_) => break,
                },
            }
        }
        // Dropping the receiver stops the workers from blocking on a full
        // channel while the scope waits for them to finish.
        drop(rx);
        if let Ok(mut slot) = failure.lock() {
            if let Some(error) = slot.take() {
                return Err(error);
            }
        }
        result?;
        if wanted < slabs {
            return Err(SnapshotError::Restore(format!(
                "the memory scan produced {wanted} of {slabs} slabs"
            )));
        }
        Ok(())
    })
}

/// What a hypervisor's write log missed, measured against the pages a save
/// actually had to write (ADR-0006).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unreported {
    /// Pages a save would write: not entirely zero.
    pub nonzero_pages: u64,
    /// Of those, the ones the hypervisor's log reported.
    pub reported_pages: u64,
    /// Of those, the ones it did **not**.
    pub missing_pages: u64,
    /// Those pages in bytes.
    pub missing_bytes: u64,
}

/// Counts the pages a snapshot has to carry that the hypervisor's write log
/// never mentioned.
///
/// The number ADR-0006's argument turns on, computed on a real guest instead of
/// argued from first principles. Every page here is one this process wrote
/// through the mapping that backs guest RAM — a boot image, a virtio-blk read
/// completion, a received packet, a used-ring update — and no flag on either
/// hypervisor would have reported it. An incremental snapshot built on the log
/// would have left every one of them at whatever the base had.
///
/// A diagnostic: it walks all of guest RAM a second time, and is only reached
/// when somebody asked for the measurement.
pub fn unreported<M>(mem: &M, log: &[DirtyPages]) -> Result<Unreported>
where
    M: GuestMemory,
{
    let mut out = Unreported::default();
    for region in mem.iter() {
        let gpa = region.start_addr().0;
        let len = region.len();
        let mut offset = 0u64;
        while offset < len {
            let want = (len - offset).min(SLAB);
            let bytes = region_bytes(region, offset, want)?;
            let mut at = 0usize;
            while at < bytes.len() {
                let end = (at + PAGE as usize).min(bytes.len());
                if !is_zero(&bytes[at..end]) {
                    let page_gpa = gpa + offset + at as u64;
                    out.nonzero_pages += 1;
                    if log.iter().any(|r| r.contains_dirty_gpa(page_gpa)) {
                        out.reported_pages += 1;
                    } else {
                        out.missing_pages += 1;
                        out.missing_bytes += (end - at) as u64;
                    }
                }
                at = end;
            }
            offset += want;
        }
    }
    Ok(out)
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
/// block boundaries inside it, so the outer layer is a small state machine: the
/// region header, then, repeatedly, a 16-byte block header and the block's
/// stored bytes, both accumulated across chunk boundaries.
///
/// The **inner** layer is not a state machine any more, and that is the one
/// simplification the compressed framing bought: a block is decoded whole, so
/// the runs inside it are read by straight-line code with no cross-chunk
/// bookkeeping.
struct RunParser<'a, M: GuestMemory> {
    mem: &'a M,
    gpa: u64,
    region_len: u64,
    codec: Codec,
    /// Bytes of the fixed region header still to be consumed.
    header_left: usize,
    header: Vec<u8>,
    /// Bytes of the current block header collected so far.
    pending: Vec<u8>,
    /// What the current block decodes to, and how much of it is still to
    /// arrive.
    raw_len: u64,
    stored_left: u64,
    /// The current block's stored bytes as they arrive, and the buffer they
    /// decompress into. Both reused across blocks.
    stored: Vec<u8>,
    raw: Vec<u8>,
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
            if self.stored_left > 0 {
                let take = (self.stored_left.min(chunk.len() as u64)) as usize;
                self.stored.extend_from_slice(&chunk[..take]);
                self.stored_left -= take as u64;
                chunk = &chunk[take..];
                if self.stored_left == 0 {
                    self.apply_block()?;
                }
                continue;
            }
            let take = (16 - self.pending.len()).min(chunk.len());
            self.pending.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.pending.len() == 16 {
                self.start_block()?;
            }
        }
        Ok(())
    }

    /// Reads a block header and bounds both of its lengths **before** the
    /// buffers they size are reserved.
    fn start_block(&mut self) -> Result<()> {
        let mut r = Reader::new(&self.pending);
        let raw_len = r.u64("memory block raw length")?;
        let stored_len = r.u64("memory block stored length")?;
        self.pending.clear();
        if raw_len > MAX_BLOCK_RAW {
            return Err(SnapshotError::TooLarge {
                what: "memory block",
                value: raw_len,
                max: MAX_BLOCK_RAW,
            });
        }
        if stored_len > MAX_BLOCK_STORED {
            return Err(SnapshotError::TooLarge {
                what: "stored memory block",
                value: stored_len,
                max: MAX_BLOCK_STORED,
            });
        }
        if self.codec == Codec::None && stored_len != raw_len {
            return Err(SnapshotError::Mismatch {
                field: "uncompressed memory block length".into(),
                snapshot: stored_len.to_string(),
                current: raw_len.to_string(),
            });
        }
        // A block that decodes to nothing is a writer that emitted an empty
        // slab, which the writer above never does — and a decoder that accepted
        // it would loop on zero-length blocks for as long as the section lasts.
        if raw_len == 0 || stored_len == 0 {
            return Err(SnapshotError::BadValue {
                what: "empty memory block",
                value: raw_len,
            });
        }
        self.raw_len = raw_len;
        self.stored_left = stored_len;
        self.stored.clear();
        self.stored.reserve(stored_len as usize);
        Ok(())
    }

    /// Decodes one complete block and writes its runs into guest memory.
    fn apply_block(&mut self) -> Result<()> {
        let raw_len = self.raw_len as usize;
        let raw: &[u8] = match self.codec {
            Codec::None => &self.stored,
            Codec::Lz4Block => {
                self.raw.clear();
                self.raw.resize(raw_len, 0);
                let written = lz4_flex::block::decompress_into(&self.stored, &mut self.raw)
                    .map_err(|e| {
                        SnapshotError::Restore(format!("decompressing a memory block: {e}"))
                    })?;
                // A block that unpacks short is a block whose header lied, and
                // the bytes past `written` would restore as zeroes — a hole in
                // the guest that nothing else would ever notice.
                if written != raw_len {
                    return Err(SnapshotError::Mismatch {
                        field: "decompressed memory block length".into(),
                        snapshot: raw_len.to_string(),
                        current: written.to_string(),
                    });
                }
                &self.raw
            }
        };

        let mut at = 0usize;
        while at < raw.len() {
            if raw.len() - at < 16 {
                return Err(SnapshotError::Truncated {
                    what: "memory run header",
                    need: 16,
                    have: (raw.len() - at) as u64,
                });
            }
            let mut r = Reader::new(&raw[at..at + 16]);
            let offset = r.u64("memory run offset")?;
            let len = r.u64("memory run length")?;
            at += 16;
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
            let len = usize::try_from(len).map_err(|_| SnapshotError::BadValue {
                what: "memory run length",
                value: len,
            })?;
            if raw.len() - at < len {
                return Err(SnapshotError::Truncated {
                    what: "memory run",
                    need: len as u64,
                    have: (raw.len() - at) as u64,
                });
            }
            self.mem
                .write_slice(&raw[at..at + len], GuestAddress(self.gpa + offset))
                .map_err(|e| {
                    SnapshotError::Restore(format!(
                        "writing guest memory at {:#x}: {e}",
                        self.gpa + offset
                    ))
                })?;
            at += len;
            self.stats.saved_bytes += len as u64;
            self.stats.runs += 1;
        }
        self.raw_len = 0;
        Ok(())
    }

    fn check_region_header(&mut self) -> Result<()> {
        let mut r = Reader::new(&self.header);
        let gpa = r.u64("memory region gpa")?;
        let len = r.u64("memory region length")?;
        let page = r.u32("memory region page size")?;
        self.codec = Codec::from_code(r.u32("memory region codec")?)?;
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

    /// A section that ended mid-block or mid-header is truncated, whatever the
    /// digest says about the bytes that did arrive.
    fn finish(self) -> Result<MemoryStats> {
        if self.header_left > 0 || self.stored_left > 0 || !self.pending.is_empty() {
            return Err(SnapshotError::Truncated {
                what: "memory section",
                need: self.stored_left + self.header_left as u64 + self.pending.len() as u64,
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
        // Overwritten by the region header before a block is read; a section
        // that ends before its header is truncated, not uncompressed.
        codec: Codec::None,
        header_left: REGION_HEADER,
        header: Vec::with_capacity(REGION_HEADER),
        pending: Vec::with_capacity(16),
        raw_len: 0,
        stored_left: 0,
        stored: Vec::new(),
        raw: Vec::new(),
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
        let (total, _) = w.finish().unwrap();
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
    ///
    /// Each region is deliberately above [`PARALLEL_FLOOR`], so this is also the
    /// case where two regions are each scanned by a pool in turn — the run
    /// offsets are region-relative and a pool that leaked one region's offsets
    /// into the other's section would write the high guest's pages into low RAM.
    #[test]
    fn two_regions_stay_apart() {
        let high = 0x1_0000_0000u64;
        let each = (4 * SLAB) as usize;
        let build = || {
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), each), (GuestAddress(high), each)])
                .unwrap()
        };
        let mem = build();
        let target = build();
        // One mark per region per slab, so every worker has something to find.
        for slab in 0..4u64 {
            mem.write_obj(0xaau8, GuestAddress(slab * SLAB + PAGE))
                .unwrap();
            mem.write_obj(0xbbu8, GuestAddress(high + slab * SLAB + PAGE))
                .unwrap();
        }
        round_trip(&mem, &target);
        for slab in 0..4u64 {
            assert_eq!(
                target
                    .read_obj::<u8>(GuestAddress(slab * SLAB + PAGE))
                    .unwrap(),
                0xaa,
                "low region, slab {slab}"
            );
            assert_eq!(
                target
                    .read_obj::<u8>(GuestAddress(high + slab * SLAB + PAGE))
                    .unwrap(),
                0xbb,
                "high region, slab {slab}"
            );
        }
        // And nothing bled across: the low region's other slabs are still zero.
        assert_eq!(target.read_obj::<u8>(GuestAddress(SLAB - PAGE)).unwrap(), 0);
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

    // ------------------------------------------------ the compressed framing

    /// A guest with enough shape to exercise several slabs and both codecs.
    fn patterned(bytes: usize) -> GuestMemoryMmap {
        let mem = memory(bytes);
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut page = vec![0u8; PAGE as usize];
        for index in 0..(bytes as u64 / PAGE) {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if seed % 3 == 0 {
                continue;
            }
            for (at, slot) in page.iter_mut().enumerate() {
                *slot = if at % 4 == 0 {
                    (seed >> (at % 56)) as u8 | 1
                } else {
                    0
                };
            }
            mem.write_slice(&page, GuestAddress(index * PAGE)).unwrap();
        }
        mem
    }

    fn save_bytes(mem: &GuestMemoryMmap, options: SaveOptions) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        save_with(mem, &mut w, options).unwrap();
        w.finish().unwrap();
        buf.into_inner()
    }

    fn read_back(bytes: &[u8], target: &GuestMemoryMmap) -> Result<MemoryStats> {
        let mut r = SnapshotReader::open(Cursor::new(bytes))?;
        restore(target, &mut r)
    }

    /// **The property the whole parallel scheme rests on.** One thread and
    /// eight threads must write the same file, byte for byte — otherwise the
    /// digest, a diff of two snapshots, and any future incremental would all
    /// depend on how busy the machine was.
    #[test]
    fn the_thread_count_does_not_change_the_file() {
        let mem = patterned(20 << 20);
        for codec in [Codec::None, Codec::Lz4Block] {
            let one = save_bytes(&mem, SaveOptions { workers: 1, codec });
            let many = save_bytes(&mem, SaveOptions { workers: 8, codec });
            assert!(
                one == many,
                "{} bytes with one worker, {} with eight, codec {}",
                one.len(),
                many.len(),
                codec.as_str()
            );
            let target = memory(20 << 20);
            read_back(&many, &target).expect("restore");
            let mut a = vec![0u8; 20 << 20];
            let mut b = vec![0u8; 20 << 20];
            mem.read_slice(&mut a, GuestAddress(0)).unwrap();
            target.read_slice(&mut b, GuestAddress(0)).unwrap();
            assert!(
                a == b,
                "the restored guest is not the saved one ({})",
                codec.as_str()
            );
        }
    }

    /// Compression is smaller, and both codecs restore the same guest.
    #[test]
    fn both_codecs_round_trip_and_lz4_is_smaller() {
        let mem = patterned(20 << 20);
        let plain = save_bytes(
            &mem,
            SaveOptions {
                workers: 2,
                codec: Codec::None,
            },
        );
        let packed = save_bytes(
            &mem,
            SaveOptions {
                workers: 2,
                codec: Codec::Lz4Block,
            },
        );
        assert!(
            packed.len() < plain.len(),
            "lz4 produced {} bytes against {} uncompressed",
            packed.len(),
            plain.len()
        );
        for bytes in [&plain, &packed] {
            let target = memory(20 << 20);
            let stats = read_back(bytes, &target).expect("restore");
            assert!(stats.saved_bytes > 0);
            let mut a = vec![0u8; 20 << 20];
            let mut b = vec![0u8; 20 << 20];
            mem.read_slice(&mut a, GuestAddress(0)).unwrap();
            target.read_slice(&mut b, GuestAddress(0)).unwrap();
            assert!(a == b);
        }
    }

    /// Builds a memory section by hand so the decoder's refusals can be reached
    /// through a container whose digests are correct — the failure under test is
    /// then the one named, not a checksum.
    fn handmade(codec: u32, blocks: &[(u64, u64, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        {
            let mut section = w.section(SectionKind::Memory, MEMORY_VERSION, 0).unwrap();
            let mut header = Writer::with_capacity(REGION_HEADER);
            header.u64(0).u64(4 << 20).u32(PAGE as u32).u32(codec);
            section.write_all(header.as_bytes()).unwrap();
            for (raw_len, stored_len, bytes) in blocks {
                let mut head = Writer::with_capacity(16);
                head.u64(*raw_len).u64(*stored_len);
                section.write_all(head.as_bytes()).unwrap();
                section.write_all(bytes).unwrap();
            }
            section.finish().unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    fn refuse(bytes: &[u8]) -> SnapshotError {
        let target = memory(4 << 20);
        read_back(bytes, &target).expect_err("this snapshot should have been refused")
    }

    /// **Every new refusal the compressed framing brought, by name.**
    #[test]
    fn the_block_framing_refuses_by_name() {
        // A codec this build does not know. Not a mis-decode: a refusal.
        let err = refuse(&handmade(7, &[]));
        assert!(
            matches!(
                &err,
                SnapshotError::BadValue {
                    what: "memory block codec",
                    value: 7
                }
            ),
            "{err}"
        );

        // A raw length no writer could have produced, offered before any buffer
        // is reserved for it.
        let err = refuse(&handmade(1, &[(MAX_BLOCK_RAW + 1, 4, vec![0u8; 4])]));
        assert!(
            matches!(
                &err,
                SnapshotError::TooLarge {
                    what: "memory block",
                    ..
                }
            ),
            "{err}"
        );
        let err = refuse(&handmade(1, &[(16, MAX_BLOCK_STORED + 1, Vec::new())]));
        assert!(
            matches!(
                &err,
                SnapshotError::TooLarge {
                    what: "stored memory block",
                    ..
                }
            ),
            "{err}"
        );

        // An uncompressed block whose two lengths disagree: one of them is a
        // lie and there is no way to tell which.
        let err = refuse(&handmade(0, &[(32, 16, vec![0u8; 16])]));
        assert!(
            matches!(&err, SnapshotError::Mismatch { field, .. }
                if field == "uncompressed memory block length"),
            "{err}"
        );

        // A zero-length block: the writer never emits one, and a decoder that
        // took it would make no progress.
        let err = refuse(&handmade(1, &[(0, 0, Vec::new())]));
        assert!(
            matches!(
                &err,
                SnapshotError::BadValue {
                    what: "empty memory block",
                    ..
                }
            ),
            "{err}"
        );

        // Bytes that are not an LZ4 block.
        let err = refuse(&handmade(1, &[(4096, 8, vec![0xffu8; 8])]));
        assert!(
            matches!(&err, SnapshotError::Restore(text) if text.contains("decompressing")),
            "{err}"
        );

        // A block that unpacks *short* of what it claims. The bytes past the end
        // would restore as zeroes — a hole in the guest nothing else would see.
        let short = lz4_flex::block::compress(&[0u8; 64]);
        let err = refuse(&handmade(1, &[(4096, short.len() as u64, short)]));
        assert!(
            matches!(&err, SnapshotError::Mismatch { field, .. }
                if field == "decompressed memory block length"),
            "{err}"
        );

        // A block whose contents end mid-run.
        let mut inner = Writer::with_capacity(16);
        inner.u64(0).u64(4096);
        let raw = inner.into_bytes();
        let stored = lz4_flex::block::compress(&raw);
        let err = refuse(&handmade(
            1,
            &[(raw.len() as u64, stored.len() as u64, stored)],
        ));
        assert!(
            matches!(
                &err,
                SnapshotError::Truncated {
                    what: "memory run",
                    ..
                }
            ),
            "{err}"
        );

        // A run that points past the end of the region it belongs to.
        let mut inner = Writer::with_capacity(16);
        inner.u64((4 << 20) - 16).u64(4096);
        let mut raw = inner.into_bytes();
        raw.extend_from_slice(&[1u8; 4096]);
        let stored = lz4_flex::block::compress(&raw);
        let err = refuse(&handmade(
            1,
            &[(raw.len() as u64, stored.len() as u64, stored)],
        ));
        assert!(
            matches!(
                &err,
                SnapshotError::BadValue {
                    what: "memory run past the end of its region",
                    ..
                }
            ),
            "{err}"
        );

        // A section that stops in the middle of a block.
        let raw = vec![7u8; 64];
        let stored = lz4_flex::block::compress(&raw);
        let mut short = stored.clone();
        short.pop();
        let err = refuse(&handmade(
            1,
            &[(raw.len() as u64, stored.len() as u64, short)],
        ));
        assert!(
            matches!(
                &err,
                SnapshotError::Truncated {
                    what: "memory section",
                    ..
                }
            ),
            "{err}"
        );
    }

    /// A memory section from before the compressed framing is refused by
    /// version, not decoded as though its reserved word were a codec.
    #[test]
    fn a_version_1_memory_section_is_refused() {
        let mut buf = Cursor::new(Vec::new());
        let mut w = SnapshotWriter::create(&mut buf, HostKind::KvmLinux).unwrap();
        w.put(SectionKind::Memory, 1, 0, &[0u8; REGION_HEADER])
            .unwrap();
        w.finish().unwrap();
        let err = refuse(&buf.into_inner());
        match err {
            SnapshotError::SectionVersion {
                section,
                found,
                expected,
            } => {
                assert_eq!(section, "memory");
                assert_eq!(found, 1);
                assert_eq!(expected, MEMORY_VERSION);
            }
            other => panic!("expected a section-version refusal, got {other}"),
        }
    }

    /// A wholly zero slab writes no block at all, which is what keeps an idle
    /// guest's snapshot at a few hundred bytes even with a codec in the path.
    #[test]
    fn a_zero_slab_writes_no_block() {
        let mem = memory(16 << 20);
        for workers in [1usize, 4] {
            let bytes = save_bytes(
                &mem,
                SaveOptions {
                    workers,
                    codec: Codec::Lz4Block,
                },
            );
            assert!(
                bytes.len() < 4096,
                "{workers} workers produced {} bytes",
                bytes.len()
            );
            let target = memory(16 << 20);
            let stats = read_back(&bytes, &target).expect("restore");
            assert_eq!(stats.saved_bytes, 0);
            assert_eq!(stats.runs, 0);
        }
    }

    /// The diagnostic that measures ADR-0006's gap: a page that is non-zero but
    /// absent from the log is counted as missing, and one that is in the log is
    /// not.
    #[test]
    fn unreported_counts_the_pages_a_log_did_not_mention() {
        let mem = memory(1 << 20);
        // Three non-zero pages: 0, 5 and 9.
        for page in [0u64, 5, 9] {
            mem.write_obj(0xa5u8, GuestAddress(page * PAGE)).unwrap();
        }
        // A log that saw only page 5 — which is what a hypervisor reports when
        // the other two were written by the VMM.
        let log = vec![DirtyPages::new(0, 1 << 20, vec![1u64 << 5; 4]).unwrap()];
        let out = unreported(&mem, &log).unwrap();
        assert_eq!(out.nonzero_pages, 3);
        assert_eq!(out.reported_pages, 1);
        assert_eq!(out.missing_pages, 2);
        assert_eq!(out.missing_bytes, 2 * PAGE);

        // An empty log misses all of them; a log that saw everything misses
        // none.
        let out = unreported(&mem, &[DirtyPages::clean(0, 1 << 20)]).unwrap();
        assert_eq!((out.nonzero_pages, out.missing_pages), (3, 3));
        let all = vec![DirtyPages::new(0, 1 << 20, vec![u64::MAX; 4]).unwrap()];
        let out = unreported(&mem, &all).unwrap();
        assert_eq!((out.nonzero_pages, out.missing_pages), (3, 0));
    }

    /// A run that spans a slab boundary is split in the file and rejoined in the
    /// guest — the parallel scan's one visible effect on the run list.
    #[test]
    fn a_run_across_a_slab_boundary_is_rejoined() {
        let total = (4 * SLAB) as usize;
        let mem = memory(total);
        let start = SLAB - 2 * PAGE;
        let filler = vec![0xc3u8; 4 * PAGE as usize];
        mem.write_slice(&filler, GuestAddress(start)).unwrap();
        let bytes = save_bytes(
            &mem,
            SaveOptions {
                workers: 4,
                codec: Codec::Lz4Block,
            },
        );
        let target = memory(total);
        let stats = read_back(&bytes, &target).expect("restore");
        assert_eq!(
            stats.runs, 2,
            "the boundary should split the run in the file"
        );
        assert_eq!(stats.saved_bytes, 4 * PAGE);
        let mut back = vec![0u8; 4 * PAGE as usize];
        target.read_slice(&mut back, GuestAddress(start)).unwrap();
        assert!(back == filler);
    }
}
