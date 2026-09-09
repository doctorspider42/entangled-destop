//! What a memory dump costs, measured against itself
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! `#[ignore]`d, because it allocates gigabytes and takes seconds. It exists so
//! the numbers in ADR-0006 have a **reference**: the same synthetic guest, the
//! same fill pattern, the same host, before and after a change. A wall-clock
//! figure taken from a real Ubuntu suspend moves with the guest's mood; this
//! one moves only when this code does.
//!
//! ```bash
//! # both hosts, release — a debug build measures the wrong thing by 10x
//! cargo test -p vm-snapshot --release --test memory_bench -- --ignored --nocapture
//! ```
//!
//! `ENTANGLED_BENCH_MIB` overrides the large guest's size (default 2048).
//!
//! **On WSL, halve your trust in the absolute numbers.** WSL2's
//! `CLOCK_MONOTONIC` runs a wandering few thousand ppm fast on this machine
//! (see the `dev-environment` skill), so a Linux figure here is up to ~4 %
//! long. Ratios between two runs on the same host in the same minute are
//! unaffected, which is what a before/after comparison needs.

use std::time::{Duration, Instant};

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vm_snapshot::format::{HostKind, SnapshotReader, SnapshotWriter};
use vm_snapshot::memory::{self, PAGE};

/// Fraction of pages a "touched" guest has written, as a percentage. 25 % is
/// what an installed Ubuntu desktop measured at (ADR-0006's table).
const TOUCHED_PERCENT: u64 = 25;

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

/// A guest whose non-zero pages are scattered the way a real one's are.
///
/// Not one contiguous blob: a run of touched pages followed by a run of
/// untouched ones is the shape that makes the run list long, and the run list
/// is what the encoder spends its time on. The pattern is deterministic (a
/// 64-bit LCG), so two runs of this benchmark scan exactly the same bytes.
fn touched_guest(bytes: u64) -> (GuestMemoryMmap, u64) {
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), bytes as usize)]).unwrap();
    let pages = bytes / PAGE;
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut page = vec![0u8; PAGE as usize];
    let mut touched = 0u64;
    for index in 0..pages {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if (seed >> 33) % 100 >= TOUCHED_PERCENT {
            continue;
        }
        // Real pages are neither uniformly non-zero nor uniformly random. A
        // page of kernel pointers is five-eighths zero bytes with entropy in
        // between, which is roughly what this makes: the zero runs give the
        // codec something to find and the pseudorandom bytes deny it the rest,
        // for a ratio in the same neighbourhood as a real guest's. Two details
        // matter for honesty — the zero test's early exit means the *position*
        // of the first non-zero byte changes the scan cost, and a page filled
        // with a repeating pattern would compress by twenty times and make the
        // compression figure a fiction.
        for chunk in page.chunks_mut(8) {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            for (at, slot) in chunk.iter_mut().enumerate() {
                *slot = if at < 3 {
                    (seed >> (8 * at)) as u8 | 1
                } else {
                    0
                };
            }
        }
        mem.write_slice(&page, GuestAddress(index * PAGE)).unwrap();
        touched += 1;
    }
    (mem, touched * PAGE)
}

struct Run {
    save: Duration,
    restore: Duration,
    file_bytes: u64,
    saved_bytes: u64,
    runs: u64,
}

fn measure(mem: &GuestMemoryMmap, total: u64) -> Run {
    let dir = std::env::temp_dir().join(format!("entangled-membench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mem.bin");

    let started = Instant::now();
    let file = std::fs::File::create(&path).unwrap();
    let mut writer =
        SnapshotWriter::create(file, HostKind::current().unwrap_or(HostKind::KvmLinux))
            .expect("writer");
    let stats = memory::save(mem, &mut writer).expect("save");
    let (file_bytes, file) = writer.finish().expect("finish");
    // The user waits for this too: a snapshot that is only in the page cache
    // when the machine loses power is not a snapshot.
    file.sync_all().unwrap();
    drop(file);
    let save = started.elapsed();

    let target: GuestMemoryMmap =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), total as usize)]).unwrap();
    let started = Instant::now();
    let mut reader = SnapshotReader::open(std::fs::File::open(&path).unwrap()).expect("open");
    let back = memory::restore(&target, &mut reader).expect("restore");
    let restore = started.elapsed();
    assert_eq!(back.saved_bytes, stats.saved_bytes);

    let _ = std::fs::remove_file(&path);
    Run {
        save,
        restore,
        file_bytes,
        saved_bytes: stats.saved_bytes,
        runs: stats.runs,
    }
}

fn report(label: &str, total: u64, run: &Run) {
    println!(
        "[membench] {label:>10}  guest {:>7.0} MiB  saved {:>7.1} MiB ({:>4.1}%) in {} runs  \
         file {:>7.1} MiB ({:>4.2}x)  save {:>7.2?}  restore {:>7.2?}  ={:>6.0} MiB/s scanned",
        mib(total),
        mib(run.saved_bytes),
        run.saved_bytes as f64 / total as f64 * 100.0,
        run.runs,
        mib(run.file_bytes),
        if run.file_bytes == 0 {
            0.0
        } else {
            run.saved_bytes as f64 / run.file_bytes as f64
        },
        run.save,
        run.restore,
        mib(total) / run.save.as_secs_f64(),
    );
}

/// The reference measurement: a small guest and a desktop-sized one, saved and
/// restored, with the throughput the whole scan achieved.
#[test]
#[ignore = "allocates gigabytes; run explicitly, in release"]
fn what_a_memory_dump_costs() {
    let big_mib: u64 = std::env::var("ENTANGLED_BENCH_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    if cfg!(debug_assertions) {
        println!(
            "[membench] WARNING: this is a debug build. The zero scan is the hot loop and \
             opt-level=0 ruins it — expect ~10x. Re-run with --release."
        );
    }
    for (label, mib_size) in [("bootstrap", 256u64), ("desktop", big_mib)] {
        let total = mib_size << 20;
        let (mem, touched) = touched_guest(total);
        // Two passes: the first pays for first-touch page faults on the target
        // allocation and warms the file's directory entry, and reporting a
        // number that includes those would be measuring the operating system.
        let _warm = measure(&mem, total);
        let run = measure(&mem, total);
        assert_eq!(run.saved_bytes, touched);
        report(label, total, &run);
    }
}

/// An all-zero guest is the other end of the range, and the one the run-list
/// design exists for: the scan still walks every byte, and the file is nothing.
#[test]
#[ignore = "allocates gigabytes; run explicitly, in release"]
fn what_an_untouched_guest_costs() {
    let total = 2048u64 << 20;
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), total as usize)]).unwrap();
    let run = measure(&mem, total);
    assert_eq!(run.saved_bytes, 0);
    report("all-zero", total, &run);
}

/// A sanity check that runs in the normal suite: the encoder and the decoder
/// agree on a guest built the way the benchmark builds one, at a size that
/// crosses whatever internal chunking the scanner uses.
#[test]
fn the_benchmark_guest_round_trips() {
    let total = 24 << 20;
    let (mem, touched) = touched_guest(total);
    let run = measure(&mem, total);
    assert_eq!(run.saved_bytes, touched);
    assert!(
        run.runs > 100,
        "the fill pattern produced {} runs",
        run.runs
    );

    // And byte for byte, which `measure` only checks the length of.
    let mut source = vec![0u8; total as usize];
    mem.read_slice(&mut source, GuestAddress(0)).unwrap();
    let dir = std::env::temp_dir().join(format!("entangled-membench-rt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mem.bin");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer =
        SnapshotWriter::create(file, HostKind::current().unwrap_or(HostKind::KvmLinux)).unwrap();
    memory::save(&mem, &mut writer).unwrap();
    writer.finish().unwrap();
    let target: GuestMemoryMmap =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), total as usize)]).unwrap();
    let mut reader = SnapshotReader::open(std::fs::File::open(&path).unwrap()).unwrap();
    memory::restore(&target, &mut reader).unwrap();
    let mut back = vec![0u8; total as usize];
    target.read_slice(&mut back, GuestAddress(0)).unwrap();
    assert!(back == source, "the restored guest is not the saved one");
    let _ = std::fs::remove_dir_all(&dir);
}
