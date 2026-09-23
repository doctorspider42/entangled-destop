//! The Rust half of `tools/venus-protocol/harness/run_differential.py`.
//!
//! Not part of the default test run (`#[ignore]`): it needs the case file
//! the C harness writes, and the harness script runs it with
//! `--ignored`. Run by hand it explains itself and fails.
//!
//! For every positive case — a command encoded by Mesa's generated *driver*
//! encoder, and a reply to it encoded by the generated *renderer* — this
//! requires that the generated Rust
//!
//! 1. decodes the command, consuming exactly its bytes;
//! 2. re-encodes it, as the driver would, to identical bytes;
//! 3. decodes the C reply into that command, consuming exactly its bytes;
//! 4. re-encodes the reply to identical bytes.
//!
//! The re-encoded replies go back to the C harness, whose driver-side reply
//! decoder must accept every one. For every poisoned case — a pNext chain
//! carrying a structure the protocol knows but this crate did not generate —
//! decoding must refuse, and with `UnknownPnextStype`.

use std::fs;
use std::io::Write;

use virtio_gpu::venus::protocol::{Command, ProtocolError};
use virtio_gpu::venus::wire::{Decoder, Encoder, WireError};

struct Record {
    command: u32,
    poisoned: bool,
    seed: u64,
    bytes: Vec<u8>,
    reply: Vec<u8>,
}

fn take<'a>(data: &mut &'a [u8], n: usize) -> &'a [u8] {
    let (head, tail) = data.split_at(n);
    *data = tail;
    head
}

fn u32_of(data: &mut &[u8]) -> u32 {
    u32::from_le_bytes(take(data, 4).try_into().expect("4 bytes"))
}

fn records(mut data: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let command = u32_of(&mut data);
        let poisoned = u32_of(&mut data) != 0;
        let seed = u64::from_le_bytes(take(&mut data, 8).try_into().expect("8 bytes"));
        let n = u32_of(&mut data) as usize;
        let bytes = take(&mut data, n).to_vec();
        let n = u32_of(&mut data) as usize;
        let reply = take(&mut data, n).to_vec();
        out.push(Record {
            command,
            poisoned,
            seed,
            bytes,
            reply,
        });
    }
    out
}

#[test]
#[ignore = "driven by tools/venus-protocol/harness/run_differential.py"]
fn generated_rust_matches_the_generated_c_byte_for_byte() {
    let (Ok(cases), Ok(replies)) = (
        std::env::var("VENUS_DIFF_CASES"),
        std::env::var("VENUS_DIFF_REPLIES"),
    ) else {
        panic!(
            "run this through tools/venus-protocol/harness/run_differential.py, which sets \
             VENUS_DIFF_CASES and VENUS_DIFF_REPLIES"
        );
    };
    let data = fs::read(&cases).expect("case file");
    let mut out = fs::File::create(&replies).expect("reply file");

    let mut per_command: std::collections::BTreeMap<&'static str, (u32, u32)> =
        std::collections::BTreeMap::new();
    let mut failures = Vec::new();
    let (mut positive, mut refused) = (0u32, 0u32);

    for rec in records(&data) {
        let what = format!("command #{} seed {:#x}", rec.command, rec.seed);
        let mut dec = Decoder::new(&rec.bytes);
        let decoded = Command::decode_next(&mut dec);

        if rec.poisoned {
            match decoded {
                Err(ProtocolError::Wire(WireError::UnknownPnextStype { .. })) => refused += 1,
                other => failures.push(format!(
                    "{what}: a chain with an unadmitted sType was not refused as one: {other:?}"
                )),
            }
            continue;
        }

        let (header, mut command) = match decoded {
            Ok(ok) => ok,
            Err(err) => {
                failures.push(format!("{what}: decode refused a C-encoded command: {err}"));
                out.write_all(&0u32.to_le_bytes()).expect("write");
                continue;
            }
        };
        let name = command.name();
        let entry = per_command.entry(name).or_default();
        entry.0 += 1;
        let mut ok = true;
        if dec.remaining() != 0 {
            failures.push(format!(
                "{what} ({name}): {} command bytes left over",
                dec.remaining()
            ));
            ok = false;
        }
        let mut enc = Encoder::new();
        match command.encode_command(&mut enc, header.flags) {
            Ok(()) => {
                let again = enc.finish().expect("finish");
                if again != rec.bytes {
                    failures.push(format!(
                        "{what} ({name}): command re-encodes differently\n  C:    {:02x?}\n  Rust: {:02x?}",
                        rec.bytes, again
                    ));
                    ok = false;
                }
            }
            Err(err) => {
                failures.push(format!("{what} ({name}): command re-encode refused: {err}"));
                ok = false;
            }
        }

        let mut rdec = Decoder::new(&rec.reply);
        let mut rust_reply = Vec::new();
        match command.decode_reply(&mut rdec) {
            Ok(()) if rdec.remaining() == 0 => match command.reply_bytes(1 << 24) {
                Ok(bytes) => {
                    if bytes != rec.reply {
                        failures.push(format!(
                            "{what} ({name}): reply re-encodes differently\n  C:    {:02x?}\n  Rust: {:02x?}",
                            rec.reply, bytes
                        ));
                        ok = false;
                    }
                    rust_reply = bytes;
                }
                Err(err) => {
                    failures.push(format!("{what} ({name}): reply encode refused: {err}"));
                    ok = false;
                }
            },
            Ok(()) => {
                failures.push(format!(
                    "{what} ({name}): {} reply bytes left over",
                    rdec.remaining()
                ));
                ok = false;
            }
            Err(err) => {
                failures.push(format!("{what} ({name}): the C reply was refused: {err}"));
                ok = false;
            }
        }
        out.write_all(&u32::try_from(rust_reply.len()).expect("len").to_le_bytes())
            .expect("write");
        out.write_all(&rust_reply).expect("write");
        if ok {
            entry.1 += 1;
            positive += 1;
        }
    }

    for (name, (seen, passed)) in &per_command {
        println!("  {name:<45} {passed:>5} / {seen:<5} round-tripped");
    }
    println!(
        "rust: {positive} commands and replies byte-identical to the C, {refused} poisoned chains \
         refused, {} failures",
        failures.len()
    );
    for failure in failures.iter().take(20) {
        println!("FAIL {failure}");
    }
    assert!(
        failures.is_empty(),
        "{} differential failures",
        failures.len()
    );
}
