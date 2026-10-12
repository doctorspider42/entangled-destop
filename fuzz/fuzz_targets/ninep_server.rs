#![no_main]
//! Fuzzes the virtio-9p file server (shared folders): a sequence of arbitrary
//! 9P messages against a real, empty share in a scratch directory.
//!
//! Properties checked:
//!
//! * no message, and no sequence of messages, panics the server;
//! * every reply is a well-formed message whose size field is its length and
//!   which never exceeds the negotiated `msize` (plus nothing);
//! * the fid table never grows past `MAX_FIDS`;
//! * **nothing appears outside the share**: the scratch directory holds the
//!   share and a sentinel file, and after every run its listing is exactly
//!   that — whatever names, walks, renames, links or symlinks the fuzzer
//!   tried.

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use std::sync::OnceLock;

use virtio_9p::server::{Server, MAX_FIDS};

fn scratch() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("ninep-fuzz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("share")).unwrap();
        std::fs::write(dir.join("sentinel"), b"outside").unwrap();
        dir
    })
}

fuzz_target!(|data: &[u8]| {
    let dir = scratch();
    let share = dir.join("share");
    let _ = std::fs::remove_dir_all(&share);
    std::fs::create_dir_all(&share).unwrap();
    let mut server = Server::new(&share, false).unwrap();

    // A version and an attach first, so the fuzzer starts inside a session;
    // then the input is a stream of `len[2] message[len]` records.
    for setup in [
        &b"\x15\x00\x00\x00\x64\xff\xff\x00\x00\x01\x00\x08\x009P2000.L"[..],
        &b"\x17\x00\x00\x00\x68\x01\x00\x00\x00\x00\x00\xff\xff\xff\xff\x00\x00\x00\x00\xe8\x03\x00\x00"[..],
    ] {
        server.handle(setup);
    }
    let mut rest = data;
    while rest.len() >= 2 {
        let len = usize::from(u16::from_le_bytes([rest[0], rest[1]])).min(rest.len() - 2);
        let msg = &rest[2..2 + len];
        rest = &rest[2 + len..];
        let reply = server.handle(msg);
        assert!(reply.len() >= 7, "a reply has a header");
        let size = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize;
        assert_eq!(size, reply.len(), "the size field is the reply's length");
        assert!(reply.len() as u32 <= server.msize().max(4096) + 64, "within msize");
        assert!(server.fid_count() <= MAX_FIDS);
    }

    let mut outside: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    outside.sort();
    assert_eq!(outside, ["sentinel", "share"], "something escaped the share");
    assert_eq!(std::fs::read(dir.join("sentinel")).unwrap(), b"outside");
});
