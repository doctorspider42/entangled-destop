//! Dumps the generated ACPI tables to files so they can be checked with an
//! external ACPI tool instead of only against our own expectations.
//!
//! Ignored by default (it writes files and needs `iasl` to be interesting):
//!
//! ```text
//! cargo test -p machine-x86 --test acpi_dump -- --ignored --nocapture
//! iasl -d $TMPDIR/entangled-acpi/dsdt.aml      # disassemble the AML
//! iasl -ta $TMPDIR/entangled-acpi/*.dat        # re-assemble and check
//! ```
//!
//! `ENTANGLED_ACPI_DUMP` overrides the output directory. See
//! `.claude/skills/acpi-machine/SKILL.md`.

use std::path::PathBuf;

use machine_x86::acpi::AcpiTables;

#[test]
#[ignore = "writes files; run explicitly to inspect the tables with iasl"]
fn dump_tables_for_iasl() {
    let vcpus: u32 = std::env::var("ENTANGLED_ACPI_VCPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let dir: PathBuf = std::env::var("ENTANGLED_ACPI_DUMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("entangled-acpi"));
    std::fs::create_dir_all(&dir).expect("cannot create the dump directory");

    let tables = AcpiTables::new(vcpus, machine_x86::layout::pci_mmio64_base(2048 << 20))
        .expect("table generation failed");
    println!(
        "base {:#x} ({} bytes)",
        tables.base_address(),
        tables.blob().len()
    );
    std::fs::write(dir.join("acpi-blob.bin"), tables.blob()).unwrap();

    // Each table separately, with the extension iasl expects for a raw table.
    for (name, address) in [
        ("rsdp", tables.rsdp_address()),
        ("xsdt", tables.xsdt_address()),
        ("facp", tables.fadt_address()),
        ("facs", tables.facs_address()),
        ("apic", tables.madt_address()),
        ("dsdt", tables.dsdt_address()),
    ] {
        // The length lives at offset 4 of an SDT and of the FACS, but at offset
        // 20 of the RSDP (offset 4 there is still part of the signature).
        let len_at = if name == "rsdp" { 20 } else { 4 };
        let header = tables
            .table_at(address, len_at + 4)
            .expect("table outside the blob");
        let len = u32::from_le_bytes(header[len_at..len_at + 4].try_into().unwrap()) as usize;
        let bytes = tables.table_at(address, len).expect("truncated table");
        let path = dir.join(format!("{name}.dat"));
        std::fs::write(&path, bytes).unwrap();
        println!(
            "{name:>4} {address:#010x} {len:>5} bytes -> {}",
            path.display()
        );
        if name == "dsdt" {
            std::fs::write(dir.join("dsdt.aml"), bytes).unwrap();
        }
    }
}
