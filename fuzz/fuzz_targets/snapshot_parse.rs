//! Fuzzes the snapshot parser ([ADR-0006](../../docs/adr/0006-suspend-restore.md)).
//!
//! A snapshot is host-side data, not guest-side — but it is still **untrusted
//! input**, and for reasons that have nothing to do with an attacker. It
//! outlives the build that wrote it. It gets copied between machines, truncated
//! by a full disk, half-synced by a host that lost power, and handed over by
//! someone else. Every one of those has to produce a typed error, and the
//! parser is the only thing standing between a corrupt file and the machinery
//! that puts a kernel back on top of live disks.
//!
//! What is asserted:
//!
//! * **no panic, no abort, no out-of-bounds read** on any input at all;
//! * **no allocation driven by an unchecked length** — the fuzz profile has
//!   `debug-assertions` and `overflow-checks` on, so an arithmetic slip on a
//!   length field aborts rather than wrapping into a plausible-looking one, and
//!   a target that tried to honour a 2^60-entry count would be killed by the
//!   OOM limit rather than passing;
//! * **an exact re-encode** for everything that decodes. A codec whose halves
//!   disagree writes a snapshot it cannot read back, which is the one bug that
//!   would not show up until someone tried to resume.
//!
//! Four layers, because the container and the sections fail differently:
//! the header and index (`SnapshotReader::open`), each section decoder against
//! the raw bytes directly (so shapes the container never happened to produce
//! are exercised too), a whole valid snapshot with the fuzzer's bytes spliced
//! into it — the only way to reach the section decoders *through* the digest
//! checks — and the **memory section**, which needs a layer of its own.
//!
//! Guest memory is the one section that is streamed rather than materialised,
//! and since ADR-0006's compressed framing it is also the one that decompresses
//! somebody else's bytes into a buffer somebody else's number sized. So
//! `memory_section` builds a section whose region header is *correct* for a
//! small guest and hands the fuzzer everything after it: the block lengths, the
//! codec, the LZ4 payload and the run headers inside it. What is asserted there
//! is no panic, no allocation the length checks did not bound, and — for
//! anything that decodes — that saving the restored guest and restoring it
//! again produces the same bytes, which is the memory section's version of the
//! exact re-encode the others get.
//!
//! ```bash
//! cargo +nightly fuzz run snapshot_parse -- -max_total_time=120
//! ```

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vm_snapshot::codec::{Reader, Writer};
use vm_snapshot::{cpu, devices, memory, meta, SectionKind, SnapshotReader, SnapshotWriter};

/// The guest the memory layer restores into. Small, because every input
/// allocates one and the decoder's bounds are what is under test, not the
/// allocator.
const GUEST_BYTES: u64 = 1 << 20;

/// Decodes `data` with every section decoder, and re-encodes whatever worked.
fn every_section(data: &[u8]) {
    if let Ok(state) = cpu::decode(data) {
        assert_eq!(cpu::encode(&state), data, "cpu re-encode must be exact");
    }
    if let Ok(clock) = cpu::decode_clock(data) {
        assert_eq!(
            cpu::encode_clock(&clock),
            data,
            "clock re-encode must be exact"
        );
    }
    if let Ok(chip) = cpu::decode_host_irqchip(data) {
        assert_eq!(
            cpu::encode_host_irqchip(&chip),
            data,
            "host irqchip re-encode must be exact"
        );
    }
    if let Ok(state) = meta::Metadata::decode(data) {
        assert_eq!(state.encode(), data, "metadata re-encode must be exact");
        // The shape check compares against itself, which must always agree —
        // and walks every field, which is what exercises the comparison paths.
        assert!(state.shape.check(&state.shape).is_ok());
    }
    if let Ok(state) = devices::decode_virtio(data) {
        assert_eq!(
            devices::encode_virtio(&state),
            data,
            "virtio re-encode must be exact"
        );
    }
    if let Ok(state) = devices::decode_serial(data) {
        assert_eq!(devices::encode_serial(&state), data);
    }
    if let Ok(state) = devices::decode_platform(data) {
        assert_eq!(devices::encode_platform(&state), data);
    }
    if let Ok(state) = devices::decode_acpi_pm(data) {
        assert_eq!(devices::encode_acpi_pm(&state), data);
    }
    if let Ok(state) = devices::decode_pflash(data) {
        assert_eq!(devices::encode_pflash(&state), data);
    }
    if let Ok(state) = devices::decode_pci_root(data) {
        assert_eq!(devices::encode_pci_root(&state), data);
    }
    if let Ok(state) = devices::decode_irqchip(data) {
        assert_eq!(devices::encode_irqchip(&state), data);
    }
    if let Ok(state) = devices::decode_reset(data) {
        assert_eq!(devices::encode_reset(&state), data);
    }
    // The primitive layer itself, so a length field that no section happens to
    // put first is still reached.
    let mut reader = Reader::new(data);
    let _ = reader.blob("fuzz", 1 << 20);
    let _ = reader.string("fuzz", 1 << 16);
    let _ = reader.count("fuzz", 1 << 16, 1);
}

/// A snapshot whose sections are all `data`, so the container's checks pass and
/// the section decoders are reached the way a restore reaches them.
fn spliced(data: &[u8]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    let Ok(mut writer) = SnapshotWriter::create(&mut buf, vm_snapshot::HostKind::KvmLinux) else {
        return Vec::new();
    };
    for kind in [
        SectionKind::Metadata,
        SectionKind::Cpu,
        SectionKind::Clock,
        SectionKind::Memory,
        SectionKind::IrqChip,
        SectionKind::Serial,
        SectionKind::Platform,
        SectionKind::AcpiPm,
        SectionKind::Pflash,
        SectionKind::PciRoot,
        SectionKind::Virtio,
        SectionKind::ResetControl,
        SectionKind::HostIrqChip,
    ] {
        if writer.put(kind, 1, 0, data).is_err() {
            return Vec::new();
        }
    }
    if writer.finish().is_err() {
        return Vec::new();
    }
    buf.into_inner()
}

/// Reads whatever the container will give up, section by section.
fn walk(bytes: &[u8]) {
    let Ok(mut reader) = SnapshotReader::open(Cursor::new(bytes)) else {
        return;
    };
    // The index is internally consistent by now; everything below is a
    // *content* check.
    let entries: Vec<(SectionKind, u32)> = reader
        .sections()
        .iter()
        .map(|entry| (entry.kind, entry.instance))
        .collect();
    for (kind, instance) in entries {
        if kind == SectionKind::Memory {
            // Streamed rather than materialised, exactly as a restore does it.
            let _ = reader.stream_section(kind, instance, |_| Ok(()));
            continue;
        }
        if let Ok(payload) = reader.read_section(kind, instance) {
            every_section(&payload);
        }
    }
    let _ = reader.require_native();
}

/// A snapshot with one memory section: a region header this build agrees with,
/// then `data` as the block stream.
///
/// The header has to be right or every input dies at the first comparison; the
/// codec is taken from the fuzzer so the unknown-codec refusal and both real
/// codecs are all reachable.
fn memory_section(data: &[u8]) -> Vec<u8> {
    let codec = data.first().copied().unwrap_or(1) as u32 % 4;
    let mut buf = Cursor::new(Vec::new());
    let Ok(mut writer) = SnapshotWriter::create(&mut buf, vm_snapshot::HostKind::KvmLinux) else {
        return Vec::new();
    };
    let mut payload = Writer::new();
    payload
        .u64(0)
        .u64(GUEST_BYTES)
        .u32(memory::PAGE as u32)
        .u32(codec);
    let mut payload = payload.into_bytes();
    payload.extend_from_slice(data);
    if writer
        .put(SectionKind::Memory, memory::MEMORY_VERSION, 0, &payload)
        .is_err()
    {
        return Vec::new();
    }
    if writer.finish().is_err() {
        return Vec::new();
    }
    buf.into_inner()
}

fn guest() -> GuestMemoryMmap {
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), GUEST_BYTES as usize)]).unwrap()
}

fn read_guest(mem: &GuestMemoryMmap) -> Vec<u8> {
    let mut out = vec![0u8; GUEST_BYTES as usize];
    mem.read_slice(&mut out, GuestAddress(0)).unwrap();
    out
}

/// Restores `bytes` into a fresh guest and, if that worked, proves the guest
/// survives another round trip through this build's own writer.
fn memory_round_trip(bytes: &[u8]) {
    let Ok(mut reader) = SnapshotReader::open(Cursor::new(bytes)) else {
        return;
    };
    let first = guest();
    if memory::restore(&first, &mut reader).is_err() {
        return;
    }
    let before = read_guest(&first);
    for codec in [memory::Codec::None, memory::Codec::Lz4Block] {
        for workers in [1usize, 3] {
            let mut out = Cursor::new(Vec::new());
            let Ok(mut writer) = SnapshotWriter::create(&mut out, vm_snapshot::HostKind::KvmLinux)
            else {
                return;
            };
            let options = memory::SaveOptions { workers, codec };
            memory::save_with(&first, &mut writer, options).expect("re-saving a restored guest");
            writer.finish().expect("finishing a re-save");
            let again = out.into_inner();
            let second = guest();
            let mut reader = SnapshotReader::open(Cursor::new(&again))
                .expect("this build must read what it just wrote");
            memory::restore(&second, &mut reader).expect("this build must restore its own file");
            assert!(
                read_guest(&second) == before,
                "a guest that round-tripped through {codec:?} with {workers} workers changed"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Layer 1: arbitrary bytes as a whole snapshot. Almost all of these are
    // refused at the magic; the interesting ones are the mutations of a real
    // file the corpus keeps.
    walk(data);

    // Layer 2: arbitrary bytes as one section payload, with no container in
    // the way.
    every_section(data);

    // Layer 3: arbitrary bytes *inside* a well-formed container, so the
    // decoders are reached through the digests rather than around them.
    let inside = spliced(data);
    if !inside.is_empty() {
        walk(&inside);
    }

    // Layer 4: the memory section, whose block framing decompresses the
    // fuzzer's bytes into a buffer the fuzzer's numbers size.
    let section = memory_section(data);
    if !section.is_empty() {
        memory_round_trip(&section);
    }

    // And the writer, so a value that round-trips cannot be one the writer
    // refuses to emit: anything the reader accepted above was re-encoded there.
    let mut writer = Writer::new();
    writer.blob(data);
    let mut back = Reader::new(writer.as_bytes());
    assert_eq!(back.blob("fuzz", usize::MAX).unwrap(), data);
});
