//! Fuzzes the descriptor-chain walker (backlog MVP-1402, EPIC 3 safety rules).
//!
//! The input is interpreted as a *guest*: it programs the ring geometry and then
//! fills the descriptor table with raw bytes. Every field the guest controls —
//! table address, ring size, head index, `next` links, flags, buffer addresses
//! and lengths — is therefore attacker-chosen, which is exactly the threat model
//! `virtio_core::chain` is written against.
//!
//! The property under test: `walk` either returns segments or a `ChainError`,
//! bounded by `MAX_DESC_CHAIN_LEN`, and never panics, hangs or reads host memory.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_core::chain::{self, DESC_SIZE};
use virtio_core::testing::guest_memory;
use virtio_core::{ChainError, GuestMem, MAX_DESC_CHAIN_LEN};
use vm_memory::{Bytes, GuestAddress};

/// Guest memory for the fuzzer. Small so most guest-supplied addresses land
/// outside it and exercise the checked-access paths.
const MEM_SIZE: u64 = 0x1_0000;

#[derive(Debug, Arbitrary)]
struct Input {
    /// Where the driver claims its descriptor table is.
    desc_table: u64,
    /// Ring size as programmed by the driver — deliberately *not* forced to a
    /// power of two: `walk` must cope with whatever it is handed.
    queue_size: u16,
    /// First descriptor of the chain.
    head: u16,
    /// Raw descriptor table bytes, written wherever they fit.
    table: Vec<u8>,
    /// Offset the table bytes are written at, so the table can straddle the end
    /// of guest memory.
    table_at: u16,
}

fuzz_target!(|input: Input| {
    let mem: GuestMem = guest_memory(MEM_SIZE);
    // Ignore failures: a write that does not fit simply leaves that part of the
    // "table" as zeroes, which is itself a case worth walking.
    if !input.table.is_empty() {
        let _ = mem.write_slice(&input.table, GuestAddress(u64::from(input.table_at)));
    }

    match chain::walk(&mem, input.desc_table, input.queue_size, input.head) {
        Ok(segments) => {
            // A successful walk is bounded by both caps, never longer.
            let cap = usize::from(MAX_DESC_CHAIN_LEN.min(input.queue_size));
            assert!(
                segments.len() <= cap,
                "walk returned {} segments for queue_size {} (cap {cap})",
                segments.len(),
                input.queue_size
            );
            // Success implies the table was inside guest memory, so every
            // descriptor read stayed in bounds by construction.
            assert!(
                input.desc_table
                    < MEM_SIZE + u64::from(input.queue_size).saturating_mul(DESC_SIZE),
                "walk succeeded with a descriptor table at {:#x}",
                input.desc_table
            );
            // split_rw must classify the result without panicking either.
            match chain::split_rw(&segments) {
                Ok((readable, writable)) => {
                    assert_eq!(readable.len() + writable.len(), segments.len());
                    assert!(readable.iter().all(|s| !s.writable));
                    assert!(writable.iter().all(|s| s.writable));
                }
                Err(ChainError::WriteBeforeRead) => (),
                Err(other) => panic!("split_rw returned an unexpected error: {other}"),
            }
        }
        // Every rejection must be one of the documented, typed reasons.
        Err(
            ChainError::TooLong
            | ChainError::IndexOutOfRange { .. }
            | ChainError::DescriptorUnreadable { .. }
            | ChainError::DescriptorTableOverflow { .. }
            | ChainError::IndirectNotSupported,
        ) => (),
        Err(other) => panic!("unexpected chain error variant: {other}"),
    }
});
