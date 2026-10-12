//! The server driven as the Linux client drives it, plus every way out of the
//! share a guest might try.

use super::*;
use crate::wire::{Reader, Writer};

struct Client {
    server: Server,
    dir: PathBuf,
}

/// What a reply decodes to: its type and body.
fn split(reply: &[u8]) -> (u8, Vec<u8>) {
    assert!(reply.len() >= wire::HEADER, "short reply");
    let size = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize;
    assert_eq!(size, reply.len(), "the size field is the reply's length");
    (reply[4], reply[wire::HEADER..].to_vec())
}

impl Client {
    fn new(tag: &str, read_only: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("virtio-9p-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("share")).unwrap();
        let mut client = Self {
            server: Server::new(&dir.join("share"), read_only).unwrap(),
            dir,
        };
        let (kind, body) = client.call(t::TVERSION, |w| {
            w.u32(65536).string("9P2000.L");
        });
        assert_eq!(kind, t::TVERSION + 1);
        let mut r = Reader::new(&body);
        assert_eq!(r.u32(), Ok(65536));
        assert_eq!(r.string(), Ok("9P2000.L"));
        let (kind, _) = client.call(t::TATTACH, |w| {
            w.u32(0).u32(!0).string("user").string("").u32(1000);
        });
        assert_eq!(kind, t::TATTACH + 1);
        client
    }

    fn share(&self) -> PathBuf {
        self.dir.join("share")
    }

    fn call(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> (u8, Vec<u8>) {
        let mut w = Writer::new(kind, 1);
        body(&mut w);
        split(&self.server.handle(&w.finish()))
    }

    /// The errno of an `Rlerror`, or `None` for success.
    fn errno(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> Option<u32> {
        let (got, body) = self.call(kind, body);
        (got == t::RLERROR).then(|| u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
    }

    /// Walks `names` from the root into `newfid`; the number of qids back.
    fn walk(&mut self, newfid: u32, names: &[&str]) -> Result<usize, u32> {
        let (kind, body) = self.call(t::TWALK, |w| {
            w.u32(0).u32(newfid).u16(names.len() as u16);
            for n in names {
                w.string(n);
            }
        });
        if kind == t::RLERROR {
            return Err(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        }
        Ok(usize::from(u16::from_le_bytes([body[0], body[1]])))
    }

    fn create(&mut self, dirfid: u32, name: &str) -> Option<u32> {
        self.errno(t::TLCREATE, |w| {
            w.u32(dirfid)
                .string(name)
                .u32(0o2 | 0o100)
                .u32(0o644)
                .u32(0);
        })
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> u32 {
        let (kind, body) = self.call(t::TWRITE, |w| {
            w.u32(fid).u64(offset).u32(data.len() as u32).bytes(data);
        });
        assert_eq!(kind, t::TWRITE + 1, "write refused");
        u32::from_le_bytes([body[0], body[1], body[2], body[3]])
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Vec<u8> {
        let (kind, body) = self.call(t::TREAD, |w| {
            w.u32(fid).u64(offset).u32(count);
        });
        assert_eq!(kind, t::TREAD + 1, "read refused");
        let n = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
        body[4..4 + n].to_vec()
    }

    fn readdir(&mut self, fid: u32) -> Vec<String> {
        let mut names = Vec::new();
        let mut offset = 0u64;
        loop {
            let (kind, body) = self.call(t::TREADDIR, |w| {
                w.u32(fid).u64(offset).u32(200);
            });
            assert_eq!(kind, t::TREADDIR + 1);
            let n = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            if n == 0 {
                return names;
            }
            let mut r = Reader::new(&body[4..4 + n]);
            while let Ok(_kind) = r.u8() {
                r.u32().unwrap();
                r.u64().unwrap();
                offset = r.u64().unwrap();
                r.u8().unwrap();
                names.push(r.string().unwrap().to_string());
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_file_is_created_written_read_back_and_listed() {
    let mut c = Client::new("rw", false);
    c.walk(1, &[]).unwrap();
    assert_eq!(c.create(1, "hello.txt"), None);
    assert_eq!(c.write(1, 0, b"hello, "), 7);
    assert_eq!(c.write(1, 7, b"guest"), 5);
    assert_eq!(c.read(1, 0, 100), b"hello, guest");
    assert_eq!(c.read(1, 7, 3), b"gue");
    assert_eq!(
        std::fs::read(c.share().join("hello.txt")).unwrap(),
        b"hello, guest"
    );

    assert_eq!(
        c.errno(t::TMKDIR, |w| {
            w.u32(0).string("sub").u32(0o755).u32(0);
        }),
        None
    );
    c.walk(2, &[]).unwrap();
    assert_eq!(
        c.errno(t::TLOPEN, |w| {
            w.u32(2).u32(0);
        }),
        None
    );
    assert_eq!(c.readdir(2), vec!["hello.txt", "sub"]);

    // Getattr: the size, and the owner is whoever attached.
    let (kind, body) = c.call(t::TGETATTR, |w| {
        w.u32(1).u64(0x7ff);
    });
    assert_eq!(kind, t::TGETATTR + 1);
    let mut r = Reader::new(&body);
    r.u64().unwrap();
    r.u8().unwrap();
    r.u32().unwrap();
    r.u64().unwrap();
    let mode = r.u32().unwrap();
    assert_eq!(mode & 0o170000, 0o100000, "a regular file");
    assert_eq!(r.u32(), Ok(1000), "uid");
    r.u32().unwrap();
    r.u64().unwrap();
    r.u64().unwrap();
    assert_eq!(r.u64(), Ok(12), "size");
}

#[test]
fn a_rename_keeps_open_fids_pointing_at_the_file() {
    let mut c = Client::new("rename", false);
    c.walk(1, &[]).unwrap();
    assert_eq!(c.create(1, "draft"), None);
    c.write(1, 0, b"text");
    // The editor's dance: write a temporary, rename it over the real name.
    assert_eq!(
        c.errno(t::TRENAMEAT, |w| {
            w.u32(0).string("draft").u32(0).string("final");
        }),
        None
    );
    assert!(c.share().join("final").exists());
    // The fid follows, so a getattr on it still works.
    assert_eq!(
        c.errno(t::TGETATTR, |w| {
            w.u32(1).u64(0x7ff);
        }),
        None
    );
    assert_eq!(
        c.errno(t::TUNLINKAT, |w| {
            w.u32(0).string("final").u32(0);
        }),
        None
    );
    assert!(!c.share().join("final").exists());
}

#[test]
fn no_walk_leaves_the_share() {
    let mut c = Client::new("escape", false);
    std::fs::write(c.dir.join("secret"), b"host only").unwrap();
    // `..` at the root stays at the root.
    assert_eq!(c.walk(5, &[".."]), Ok(1));
    c.errno(t::TLOPEN, |w| {
        w.u32(5).u32(0);
    });
    assert!(!c.readdir(5).contains(&"secret".to_string()));
    // A partial walk: one qid back, and no new fid.
    assert_eq!(c.walk(6, &["..", "secret"]), Ok(1));
    assert_eq!(
        c.errno(t::TCLUNK, |w| {
            w.u32(6);
        }),
        Some(errno::EBADF)
    );
    // Names are single components.
    assert_eq!(c.walk(7, &["../secret"]), Err(errno::EINVAL));
    assert_eq!(c.walk(7, &["a/b"]), Err(errno::EINVAL));
    assert_eq!(c.create(0, "../planted"), Some(errno::EINVAL));
    assert_eq!(c.create(0, ""), Some(errno::EINVAL));
    assert!(!c.dir.join("planted").exists());
    // A symlink pointing out cannot be created.
    assert_eq!(
        c.errno(t::TSYMLINK, |w| {
            w.u32(0).string("out").string("../secret").u32(0);
        }),
        Some(errno::EPERM)
    );
    assert_eq!(
        c.errno(t::TSYMLINK, |w| {
            w.u32(0).string("out").string("/etc").u32(0);
        }),
        Some(errno::EPERM)
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_on_the_host_is_never_followed() {
    let mut c = Client::new("links", false);
    std::fs::create_dir(c.dir.join("outside")).unwrap();
    std::fs::write(c.dir.join("outside/secret"), b"host only").unwrap();
    std::os::unix::fs::symlink(c.dir.join("outside"), c.share().join("door")).unwrap();
    // Walking *to* the link is fine (it can be stat'ed and removed)...
    assert_eq!(c.walk(1, &["door"]), Ok(1));
    // ...but nothing goes through it.
    assert_eq!(
        c.walk(2, &["door", "secret"]),
        Ok(1),
        "the walk stops at the link"
    );
    assert_eq!(
        c.errno(t::TLOPEN, |w| {
            w.u32(1).u32(0);
        }),
        Some(errno::ELOOP)
    );
    assert_eq!(c.create(1, "planted"), Some(errno::ELOOP));
    assert!(!c.dir.join("outside/planted").exists());

    // The swap: a fid on a real directory whose directory becomes a link.
    std::fs::create_dir(c.share().join("real")).unwrap();
    assert_eq!(c.walk(3, &["real"]), Ok(1));
    std::fs::remove_dir(c.share().join("real")).unwrap();
    std::os::unix::fs::symlink(c.dir.join("outside"), c.share().join("real")).unwrap();
    assert_eq!(c.create(3, "planted"), Some(errno::ELOOP));
    assert!(!c.dir.join("outside/planted").exists());
}

#[test]
fn a_read_only_share_refuses_every_change() {
    let mut c = Client::new("ro", true);
    std::fs::write(c.share().join("doc"), b"read me").unwrap();
    assert_eq!(c.create(0, "new"), Some(errno::EROFS));
    assert_eq!(
        c.errno(t::TMKDIR, |w| {
            w.u32(0).string("d").u32(0o755).u32(0);
        }),
        Some(errno::EROFS)
    );
    c.walk(1, &["doc"]).unwrap();
    assert_eq!(
        c.errno(t::TLOPEN, |w| {
            w.u32(1).u32(oflag::RDWR);
        }),
        Some(errno::EROFS)
    );
    assert_eq!(
        c.errno(t::TLOPEN, |w| {
            w.u32(1).u32(0);
        }),
        None
    );
    assert_eq!(c.read(1, 0, 100), b"read me");
    assert_eq!(
        c.errno(t::TUNLINKAT, |w| {
            w.u32(0).string("doc").u32(0);
        }),
        Some(errno::EROFS)
    );
}

#[test]
fn malformed_messages_are_answered_not_fatal() {
    let mut c = Client::new("bad", false);
    // Too short for a header.
    let (kind, _) = split(&c.server.handle(&[1, 2, 3]));
    assert_eq!(kind, t::RLERROR);
    // A walk whose name runs off the end of the message.
    let mut w = Writer::new(t::TWALK, 9);
    w.u32(0).u32(1).u16(1).u16(500);
    let (kind, _) = split(&c.server.handle(&w.finish()));
    assert_eq!(kind, t::RLERROR);
    // Too many names in one walk.
    assert_eq!(c.walk(2, &["a"; MAX_WALK + 1]), Err(errno::EINVAL));
    // A fid that does not exist.
    assert_eq!(
        c.errno(t::TCLUNK, |w| {
            w.u32(4242);
        }),
        Some(errno::EBADF)
    );
    // An unknown message type.
    assert_eq!(c.errno(250, |_| {}), Some(errno::EOPNOTSUPP));
    // A read larger than msize is cut to it.
    c.walk(3, &[]).unwrap();
    assert_eq!(c.create(3, "big"), None);
    c.write(3, 0, &vec![7u8; 70_000]);
    assert!(c.read(3, 0, u32::MAX).len() <= 65536);
}

#[test]
fn the_fid_table_is_bounded() {
    let mut c = Client::new("fids", false);
    let mut refused = None;
    for fid in 1..=(MAX_FIDS as u32 + 1) {
        if let Err(e) = c.walk(fid, &[]) {
            refused = Some((fid, e));
            break;
        }
    }
    assert_eq!(
        refused,
        Some((MAX_FIDS as u32, errno::ENFILE)),
        "the attach holds one"
    );
    // A version message resets the session.
    c.call(t::TVERSION, |w| {
        w.u32(8192).string("9P2000.L");
    });
    assert_eq!(c.server.fid_count(), 0);
}

#[cfg(windows)]
#[test]
fn windows_device_names_and_streams_are_not_names() {
    assert!(!valid_name("CON"));
    assert!(!valid_name("nul.txt"));
    assert!(!valid_name("COM1"));
    assert!(!valid_name("file:stream"));
    assert!(!valid_name("trailing."));
    assert!(valid_name("console.log"));
    assert!(valid_name("COM10"));
}

#[test]
fn every_guest_user_is_shown_a_writable_share_and_a_read_only_one_is_not() {
    assert_eq!(present_mode(0o040_755, false), 0o040_777);
    assert_eq!(present_mode(0o100_644, false), 0o100_666);
    assert_eq!(
        present_mode(0o100_755, false),
        0o100_777,
        "execute bits kept"
    );
    assert_eq!(
        present_mode(0o120_777, false),
        0o120_777,
        "links as they are"
    );
    assert_eq!(present_mode(0o040_755, true), 0o040_555);
    assert_eq!(present_mode(0o100_644, true), 0o100_444);
}

#[test]
fn the_mounts_anonymous_attach_is_shown_as_root_never_as_an_unmapped_uid() {
    let mut c = Client::new("anon", false);
    let (kind, _) = c.call(t::TATTACH, |w| {
        w.u32(9).u32(!0).string("").string("").u32(u32::MAX);
    });
    assert_eq!(kind, t::TATTACH + 1);
    let (_, body) = c.call(t::TGETATTR, |w| {
        w.u32(9).u64(0x7ff);
    });
    let mut r = Reader::new(&body);
    r.u64().unwrap();
    r.u8().unwrap();
    r.u32().unwrap();
    r.u64().unwrap();
    r.u32().unwrap();
    assert_eq!(r.u32(), Ok(0), "uid");
    assert_eq!(r.u32(), Ok(0), "gid");
}

/// The fuzz target's property on stable, with a fixed seed: thousands of
/// semi-structured messages — real message types, small fids, names drawn from
/// a list of hostile ones — and afterwards nothing exists outside the share.
#[test]
fn a_storm_of_hostile_messages_leaves_nothing_outside_the_share() {
    let mut c = Client::new("storm", false);
    std::fs::write(c.dir.join("sentinel"), b"outside").unwrap();
    const NAMES: [&str; 10] = ["a", "b", "..", ".", "../a", "a/b", "", "sub", "CON", "x\\y"];
    const KINDS: [u8; 18] = [
        t::TWALK,
        t::TLOPEN,
        t::TLCREATE,
        t::TREAD,
        t::TWRITE,
        t::TREADDIR,
        t::TMKDIR,
        t::TSYMLINK,
        t::TLINK,
        t::TRENAME,
        t::TRENAMEAT,
        t::TUNLINKAT,
        t::TREMOVE,
        t::TCLUNK,
        t::TGETATTR,
        t::TSETATTR,
        t::TREADLINK,
        t::TFSYNC,
    ];
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..20_000 {
        let kind = KINDS[(next() % KINDS.len() as u64) as usize];
        let fid = (next() % 6) as u32;
        let fid2 = (next() % 6) as u32;
        let name = NAMES[(next() % NAMES.len() as u64) as usize];
        let name2 = NAMES[(next() % NAMES.len() as u64) as usize];
        let num = next();
        let mut w = Writer::new(kind, 7);
        match kind {
            t::TWALK => {
                w.u32(fid).u32(fid2).u16(2).string(name).string(name2);
            }
            t::TLOPEN => {
                w.u32(fid).u32((num % 4) as u32);
            }
            t::TLCREATE | t::TMKDIR => {
                w.u32(fid).string(name).u32(0o2 | 0o100).u32(0o644).u32(0);
            }
            t::TSYMLINK => {
                w.u32(fid).string(name).string(name2).u32(0);
            }
            t::TLINK | t::TRENAME => {
                w.u32(fid).u32(fid2).string(name);
            }
            t::TRENAMEAT => {
                w.u32(fid).string(name).u32(fid2).string(name2);
            }
            t::TUNLINKAT => {
                w.u32(fid).string(name).u32((num & 0x200) as u32);
            }
            t::TREAD | t::TWRITE | t::TREADDIR => {
                w.u32(fid).u64(num % 64).u32(8).bytes(b"payload!");
            }
            t::TGETATTR => {
                w.u32(fid).u64(0x7ff);
            }
            t::TSETATTR => {
                w.u32(fid).u32(0x8).u32(0).u32(0).u32(0).u64(num % 32);
                w.u64(0).u64(0).u64(0).u64(0);
            }
            t::TFSYNC => {
                w.u32(fid).u32(0);
            }
            _ => {
                w.u32(fid);
            }
        }
        let reply = c.server.handle(&w.finish());
        split(&reply);
        // Keep a root fid around to walk from.
        if next() % 50 == 0 {
            c.call(t::TATTACH, |w| {
                w.u32(0).u32(!0).string("").string("").u32(1000);
            });
        }
    }
    let mut outside: Vec<String> = std::fs::read_dir(&c.dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    outside.sort();
    assert_eq!(
        outside,
        ["sentinel", "share"],
        "something escaped the share"
    );
}
