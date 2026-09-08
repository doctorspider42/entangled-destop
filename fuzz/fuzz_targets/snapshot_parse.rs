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
//! Three layers, because the container and the sections fail differently:
//! the header and index (`SnapshotReader::open`), each section decoder against
//! the raw bytes directly (so shapes the container never happened to produce
//! are exercised too), and finally a whole valid snapshot with the fuzzer's
//! bytes spliced into it — which is the only way to reach the section decoders
//! *through* the digest checks.
//!
//! ```bash
//! cargo +nightly fuzz run snapshot_parse -- -max_total_time=120
//! ```

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use vm_snapshot::codec::{Reader, Writer};
use vm_snapshot::{cpu, devices, meta, SectionKind, SnapshotReader, SnapshotWriter};

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

    // And the writer, so a value that round-trips cannot be one the writer
    // refuses to emit: anything the reader accepted above was re-encoded there.
    let mut writer = Writer::new();
    writer.blob(data);
    let mut back = Reader::new(writer.as_bytes());
    assert_eq!(back.blob("fuzz", usize::MAX).unwrap(), data);
});
