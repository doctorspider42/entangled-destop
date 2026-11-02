//! virtio-fs's protocol, FUSE, served by translating it to 9P2000.L for the
//! one file server this crate already has ([`crate::server::Server`]).
//!
//! Why a translation and not a second server: everything that keeps a guest
//! inside its share — single-component names, no symlink followed, bounded
//! tables, the read-only switch — lives in that server and is tested there.
//! A FUSE node is a 9P fid walked to it; an open file is a cloned fid opened
//! with `Tlopen`; a reply is the 9P answer re-encoded. The guest never names
//! a host path, and no host call happens here.
//!
//! The guest is untrusted: every request is parsed with bounds checks, an
//! unknown or malformed one is answered `-ENOSYS`/`-EINVAL`, and the node and
//! handle tables are bounded by the server's own fid limit.
use std::collections::HashMap;

use crate::server::{errno, Server};

/// FUSE opcodes this server answers.
mod op {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const MKDIR: u32 = 9;
    pub const UNLINK: u32 = 10;
    pub const RMDIR: u32 = 11;
    pub const RENAME: u32 = 12;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const FSYNCDIR: u32 = 30;
    pub const ACCESS: u32 = 34;
    pub const CREATE: u32 = 35;
    pub const DESTROY: u32 = 38;
    pub const BATCH_FORGET: u32 = 42;
    pub const RENAME2: u32 = 45;
}

/// `fuse_in_header`.
pub const IN_HEADER: usize = 40;
/// `fuse_out_header`.
pub const OUT_HEADER: usize = 16;
/// The most one READ or WRITE moves (`fuse_init_out.max_write`).
pub const MAX_WRITE: u32 = 128 * 1024;
const ROOT_NODE: u64 = 1;
const ROOT_FID: u32 = 0;
const NOFID: u32 = u32::MAX;
const ENOSYS: u32 = 38;
const MSIZE: u32 = 512 * 1024;
/// How long the guest may cache entries and attributes, in seconds.
const VALID_SECS: u64 = 1;

/// A FUSE request's reply, or none (FORGET is never answered).
pub type Reply = Option<Vec<u8>>;

struct Node {
    fid: u32,
    lookups: u64,
}

/// One guest's FUSE session over one share.
pub struct FuseServer {
    server: Server,
    nodes: HashMap<u64, Node>,
    /// Open files and directories: the handle is the opened fid.
    handles: HashMap<u64, u32>,
    next_node: u64,
    next_fid: u32,
    tag: u16,
}

impl FuseServer {
    /// A session over `server`, attached at its root.
    pub fn new(server: Server) -> Self {
        let mut this = Self {
            server,
            nodes: HashMap::new(),
            handles: HashMap::new(),
            next_node: ROOT_NODE + 1,
            next_fid: ROOT_FID + 1,
            tag: 0,
        };
        this.attach();
        this
    }

    fn attach(&mut self) {
        let mut v = Msg::new(wire_t::TVERSION, 0xffff);
        v.u32(MSIZE).str("9P2000.L");
        self.server.handle(&v.finish());
        let mut a = Msg::new(wire_t::TATTACH, 0);
        a.u32(ROOT_FID).u32(NOFID).str("").str("").u32(0);
        self.server.handle(&a.finish());
        self.nodes.insert(
            ROOT_NODE,
            Node {
                fid: ROOT_FID,
                lookups: 1,
            },
        );
    }

    /// Forgets the session — a device reset.
    pub fn reset(&mut self) {
        self.server.reset();
        self.nodes.clear();
        self.handles.clear();
        self.next_node = ROOT_NODE + 1;
        self.next_fid = ROOT_FID + 1;
        self.attach();
    }

    /// Whether the guest holds anything open beyond the root.
    pub fn busy(&self) -> bool {
        self.nodes.len() > 1 || !self.handles.is_empty()
    }

    /// Serves one request (header included).
    pub fn handle(&mut self, request: &[u8]) -> Reply {
        let head = request.get(..IN_HEADER)?;
        let opcode = le32(head, 4);
        let unique = le64(head, 8);
        let nodeid = le64(head, 16);
        let body = &request[IN_HEADER..];
        let result = match opcode {
            op::FORGET => {
                self.forget(nodeid, le64(body, 0));
                return None;
            }
            op::BATCH_FORGET => {
                let count = le32(body, 0) as usize;
                for i in 0..count.min(body.len().saturating_sub(8) / 16) {
                    self.forget(le64(body, 8 + i * 16), le64(body, 16 + i * 16));
                }
                return None;
            }
            op::INIT => Ok(init_out(body)),
            op::DESTROY => Ok(Vec::new()),
            op::LOOKUP => self.lookup(nodeid, name(body, 0)),
            op::GETATTR => self.node_fid(nodeid).and_then(|fid| self.attr_out(fid)),
            op::SETATTR => self.setattr(nodeid, body),
            op::OPEN | op::OPENDIR => self.open(nodeid, le32(body, 0), opcode == op::OPENDIR),
            op::READ => self.read(body),
            op::WRITE => self.write(body),
            op::READDIR => self.readdir(body),
            op::RELEASE | op::RELEASEDIR => {
                if let Some(fid) = self.handles.remove(&le64(body, 0)) {
                    self.clunk(fid);
                }
                Ok(Vec::new())
            }
            op::FLUSH | op::ACCESS => Ok(Vec::new()),
            op::FSYNC | op::FSYNCDIR => self.fsync(body),
            op::STATFS => self.statfs(),
            op::MKDIR => self.mkdir(nodeid, body),
            op::CREATE => self.create(nodeid, body),
            op::UNLINK => self.unlink(nodeid, name(body, 0), 0),
            op::RMDIR => self.unlink(nodeid, name(body, 0), 0x200),
            op::RENAME => self.rename(nodeid, le64(body, 0), &body[body.len().min(8)..]),
            op::RENAME2 => self.rename(nodeid, le64(body, 0), &body[body.len().min(16)..]),
            _ => Err(ENOSYS),
        };
        Some(match result {
            Ok(payload) => out(unique, 0, &payload),
            Err(code) => out(unique, code, &[]),
        })
    }

    fn call(&mut self, msg: Msg) -> Result<Vec<u8>, u32> {
        let reply = self.server.handle(&msg.finish());
        match reply.get(4) {
            Some(&t) if t == wire_t::RLERROR => Err(le32(&reply, 7).max(1)),
            Some(_) => Ok(reply),
            None => Err(errno::EIO),
        }
    }

    fn msg(&mut self, kind: u8) -> Msg {
        self.tag = self.tag.wrapping_add(1) % 0xfffe;
        Msg::new(kind, self.tag)
    }

    fn alloc_fid(&mut self) -> u32 {
        let fid = self.next_fid;
        self.next_fid = self.next_fid.wrapping_add(1).max(ROOT_FID + 1);
        fid
    }

    fn node_fid(&self, nodeid: u64) -> Result<u32, u32> {
        self.nodes.get(&nodeid).map(|n| n.fid).ok_or(errno::EBADF)
    }

    fn clunk(&mut self, fid: u32) {
        let mut m = self.msg(wire_t::TCLUNK);
        m.u32(fid);
        let _ = self.call(m);
    }

    /// A new fid walked from `from` along `names` (none: a clone).
    fn walk(&mut self, from: u32, names: &[&[u8]]) -> Result<u32, u32> {
        let fid = self.alloc_fid();
        let mut m = self.msg(wire_t::TWALK);
        m.u32(from).u32(fid).u16(names.len() as u16);
        for n in names {
            m.bytes_str(n);
        }
        let reply = self.call(m)?;
        if usize::from(le16(&reply, 7)) != names.len() {
            self.clunk(fid);
            return Err(errno::ENOENT);
        }
        Ok(fid)
    }

    fn forget(&mut self, nodeid: u64, count: u64) {
        if nodeid == ROOT_NODE {
            return;
        }
        let gone = match self.nodes.get_mut(&nodeid) {
            Some(node) => {
                node.lookups = node.lookups.saturating_sub(count);
                node.lookups == 0
            }
            None => false,
        };
        if gone {
            if let Some(node) = self.nodes.remove(&nodeid) {
                self.clunk(node.fid);
            }
        }
    }

    fn getattr(&mut self, fid: u32) -> Result<Vec<u8>, u32> {
        let mut m = self.msg(wire_t::TGETATTR);
        m.u32(fid).u64(0x3fff);
        let r = self.call(m)?;
        // Rgetattr: valid[8] qid[13] mode uid gid nlink rdev size blksize
        // blocks atime(2) mtime(2) ctime(2) btime(2) gen data_version.
        let at = 7 + 8;
        let qid_path = le64(&r, at + 5);
        let b = at + 13;
        let mut attr = Vec::with_capacity(88);
        put64(&mut attr, qid_path); // ino
        put64(&mut attr, le64(&r, b + 28)); // size
        put64(&mut attr, le64(&r, b + 44)); // blocks
        put64(&mut attr, le64(&r, b + 52)); // atime
        put64(&mut attr, le64(&r, b + 68)); // mtime
        put64(&mut attr, le64(&r, b + 84)); // ctime
        put32(&mut attr, le64(&r, b + 60) as u32);
        put32(&mut attr, le64(&r, b + 76) as u32);
        put32(&mut attr, le64(&r, b + 92) as u32);
        put32(&mut attr, le32(&r, b)); // mode
        put32(&mut attr, le64(&r, b + 12) as u32); // nlink
        put32(&mut attr, le32(&r, b + 4)); // uid
        put32(&mut attr, le32(&r, b + 8)); // gid
        put32(&mut attr, le64(&r, b + 20) as u32); // rdev
        put32(&mut attr, le64(&r, b + 36).min(u64::from(u32::MAX)) as u32); // blksize
        put32(&mut attr, 0); // flags
        Ok(attr)
    }

    fn attr_out(&mut self, fid: u32) -> Result<Vec<u8>, u32> {
        let attr = self.getattr(fid)?;
        let mut o = Vec::with_capacity(104);
        put64(&mut o, VALID_SECS);
        put32(&mut o, 0);
        put32(&mut o, 0);
        o.extend_from_slice(&attr);
        Ok(o)
    }

    fn entry_out(&mut self, fid: u32) -> Result<Vec<u8>, u32> {
        let attr = match self.getattr(fid) {
            Ok(attr) => attr,
            Err(e) => {
                self.clunk(fid);
                return Err(e);
            }
        };
        let nodeid = self.next_node;
        self.next_node += 1;
        self.nodes.insert(nodeid, Node { fid, lookups: 1 });
        let mut o = Vec::with_capacity(128);
        put64(&mut o, nodeid);
        put64(&mut o, 0); // generation
        put64(&mut o, VALID_SECS);
        put64(&mut o, VALID_SECS);
        put32(&mut o, 0);
        put32(&mut o, 0);
        o.extend_from_slice(&attr);
        Ok(o)
    }

    fn lookup(&mut self, parent: u64, name: Option<&[u8]>) -> Result<Vec<u8>, u32> {
        let name = name.ok_or(errno::EINVAL)?;
        let pfid = self.node_fid(parent)?;
        let fid = self.walk(pfid, &[name])?;
        self.entry_out(fid)
    }

    fn open(&mut self, nodeid: u64, flags: u32, dir: bool) -> Result<Vec<u8>, u32> {
        let nfid = self.node_fid(nodeid)?;
        let fid = self.walk(nfid, &[])?;
        // Linux open flags pass through; O_DIRECTORY for a directory.
        let flags = (flags & !0o100) | if dir { 0o200000 } else { 0 };
        let mut m = self.msg(wire_t::TLOPEN);
        m.u32(fid).u32(flags);
        if let Err(e) = self.call(m) {
            self.clunk(fid);
            return Err(e);
        }
        self.handles.insert(u64::from(fid), fid);
        let mut o = Vec::with_capacity(16);
        put64(&mut o, u64::from(fid));
        put32(&mut o, 0);
        put32(&mut o, 0);
        Ok(o)
    }

    fn handle_fid(&self, fh: u64) -> Result<u32, u32> {
        self.handles.get(&fh).copied().ok_or(errno::EBADF)
    }

    fn read(&mut self, body: &[u8]) -> Result<Vec<u8>, u32> {
        let fid = self.handle_fid(le64(body, 0))?;
        let size = le32(body, 16).min(MAX_WRITE);
        let mut m = self.msg(wire_t::TREAD);
        m.u32(fid).u64(le64(body, 8)).u32(size);
        let r = self.call(m)?;
        let count = le32(&r, 7) as usize;
        Ok(r.get(11..11 + count).unwrap_or_default().to_vec())
    }

    fn write(&mut self, body: &[u8]) -> Result<Vec<u8>, u32> {
        let fid = self.handle_fid(le64(body, 0))?;
        let size = le32(body, 16) as usize;
        let data = body.get(40..40 + size).ok_or(errno::EINVAL)?;
        let mut m = self.msg(wire_t::TWRITE);
        m.u32(fid).u64(le64(body, 8)).u32(size as u32).raw(data);
        let r = self.call(m)?;
        let mut o = Vec::with_capacity(8);
        put32(&mut o, le32(&r, 7));
        put32(&mut o, 0);
        Ok(o)
    }

    fn readdir(&mut self, body: &[u8]) -> Result<Vec<u8>, u32> {
        let fid = self.handle_fid(le64(body, 0))?;
        let size = le32(body, 16).min(MAX_WRITE) as usize;
        let mut m = self.msg(wire_t::TREADDIR);
        m.u32(fid).u64(le64(body, 8)).u32(size as u32);
        let r = self.call(m)?;
        let count = le32(&r, 7) as usize;
        let data = r.get(11..11 + count).unwrap_or_default();
        // 9P entries: qid[13] offset[8] type[1] name[s]; FUSE dirents: ino
        // off namelen type name, padded to 8.
        let mut o = Vec::new();
        let mut at = 0usize;
        while at + 24 <= data.len() {
            let ino = le64(data, at + 5);
            let off = le64(data, at + 13);
            let kind = data[at + 21];
            let len = usize::from(le16(data, at + 22));
            let Some(name) = data.get(at + 24..at + 24 + len) else {
                break;
            };
            let entry = (24 + len).next_multiple_of(8);
            if o.len() + entry > size {
                break;
            }
            put64(&mut o, ino);
            put64(&mut o, off);
            put32(&mut o, len as u32);
            put32(&mut o, u32::from(kind));
            o.extend_from_slice(name);
            o.resize(o.len() + (entry - 24 - len), 0);
            at += 24 + len;
        }
        Ok(o)
    }

    fn fsync(&mut self, body: &[u8]) -> Result<Vec<u8>, u32> {
        let fid = self.handle_fid(le64(body, 0))?;
        let mut m = self.msg(wire_t::TFSYNC);
        m.u32(fid).u32(le32(body, 8) & 1);
        self.call(m).map(|_| Vec::new())
    }

    fn statfs(&mut self) -> Result<Vec<u8>, u32> {
        let mut m = self.msg(wire_t::TSTATFS);
        m.u32(ROOT_FID);
        let r = self.call(m)?;
        // Rstatfs: type bsize blocks bfree bavail files ffree fsid namelen.
        let mut o = Vec::with_capacity(80);
        for off in [15, 23, 31, 39, 47] {
            put64(&mut o, le64(&r, off));
        }
        put32(&mut o, le32(&r, 11)); // bsize
        put32(&mut o, le32(&r, 63)); // namelen
        put32(&mut o, le32(&r, 11)); // frsize
        o.resize(80, 0);
        Ok(o)
    }

    fn mkdir(&mut self, parent: u64, body: &[u8]) -> Result<Vec<u8>, u32> {
        let name = name(body, 8).ok_or(errno::EINVAL)?;
        let pfid = self.node_fid(parent)?;
        let mut m = self.msg(wire_t::TMKDIR);
        m.u32(pfid).bytes_str(name).u32(le32(body, 0)).u32(0);
        self.call(m)?;
        let fid = self.walk(pfid, &[name])?;
        self.entry_out(fid)
    }

    fn create(&mut self, parent: u64, body: &[u8]) -> Result<Vec<u8>, u32> {
        let name = name(body, 16).ok_or(errno::EINVAL)?;
        let pfid = self.node_fid(parent)?;
        // Tlcreate turns the fid it is given into the open file.
        let open = self.walk(pfid, &[])?;
        let mut m = self.msg(wire_t::TLCREATE);
        m.u32(open)
            .bytes_str(name)
            .u32(le32(body, 0))
            .u32(le32(body, 4))
            .u32(0);
        if let Err(e) = self.call(m) {
            self.clunk(open);
            return Err(e);
        }
        let node = match self.walk(pfid, &[name]) {
            Ok(fid) => fid,
            Err(e) => {
                self.clunk(open);
                return Err(e);
            }
        };
        let mut o = self.entry_out(node)?;
        self.handles.insert(u64::from(open), open);
        put64(&mut o, u64::from(open));
        put32(&mut o, 0);
        put32(&mut o, 0);
        Ok(o)
    }

    fn unlink(&mut self, parent: u64, name: Option<&[u8]>, flags: u32) -> Result<Vec<u8>, u32> {
        let name = name.ok_or(errno::EINVAL)?;
        let pfid = self.node_fid(parent)?;
        let mut m = self.msg(wire_t::TUNLINKAT);
        m.u32(pfid).bytes_str(name).u32(flags);
        self.call(m).map(|_| Vec::new())
    }

    fn rename(&mut self, parent: u64, newdir: u64, names: &[u8]) -> Result<Vec<u8>, u32> {
        let old = name(names, 0).ok_or(errno::EINVAL)?;
        let new = name(names, old.len() + 1).ok_or(errno::EINVAL)?;
        let ofid = self.node_fid(parent)?;
        let nfid = self.node_fid(newdir)?;
        let mut m = self.msg(wire_t::TRENAMEAT);
        m.u32(ofid).bytes_str(old).u32(nfid).bytes_str(new);
        self.call(m).map(|_| Vec::new())
    }

    fn setattr(&mut self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, u32> {
        let fid = self.node_fid(nodeid)?;
        let valid = le32(body, 0);
        // FUSE: MODE 1, UID 2, GID 4, SIZE 8, ATIME 16, MTIME 32, ATIME_NOW
        // 128, MTIME_NOW 256. 9P: the same low bits, ATIME/MTIME meaning
        // "now" unless ATIME_SET (128) / MTIME_SET (256) come with them.
        let mut v = valid & 0xf;
        if valid & 16 != 0 {
            v |= 16 | 128;
        }
        if valid & 32 != 0 {
            v |= 32 | 256;
        }
        if valid & 128 != 0 {
            v |= 16;
        }
        if valid & 256 != 0 {
            v |= 32;
        }
        let mut m = self.msg(wire_t::TSETATTR);
        m.u32(fid)
            .u32(v)
            .u32(le32(body, 68))
            .u32(le32(body, 76))
            .u32(le32(body, 80))
            .u64(le64(body, 16))
            .u64(le64(body, 32))
            .u64(u64::from(le32(body, 56)))
            .u64(le64(body, 40))
            .u64(u64::from(le32(body, 60)));
        self.call(m)?;
        self.attr_out(fid)
    }
}

/// 9P message kinds (the crate's `wire::t`, plus `Rlerror`).
mod wire_t {
    pub use crate::wire::t::*;
}

/// A 9P T-message under construction.
struct Msg(Vec<u8>);

impl Msg {
    fn new(kind: u8, tag: u16) -> Self {
        let mut v = vec![0, 0, 0, 0, kind];
        v.extend_from_slice(&tag.to_le_bytes());
        Self(v)
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn str(&mut self, s: &str) -> &mut Self {
        self.bytes_str(s.as_bytes())
    }
    fn bytes_str(&mut self, s: &[u8]) -> &mut Self {
        self.u16(s.len().min(usize::from(u16::MAX)) as u16);
        self.0.extend_from_slice(s);
        self
    }
    fn raw(&mut self, data: &[u8]) -> &mut Self {
        self.0.extend_from_slice(data);
        self
    }
    fn finish(&self) -> Vec<u8> {
        let mut v = self.0.clone();
        let len = v.len() as u32;
        v[..4].copy_from_slice(&len.to_le_bytes());
        v
    }
}

/// `fuse_init_out`: protocol 7.31, nothing optional asked for.
fn init_out(body: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(64);
    put32(&mut o, 7);
    put32(&mut o, le32(body, 4).min(31));
    put32(&mut o, le32(body, 8)); // max_readahead, as offered
    put32(&mut o, 0); // flags
    o.extend_from_slice(&16u16.to_le_bytes()); // max_background
    o.extend_from_slice(&12u16.to_le_bytes()); // congestion_threshold
    put32(&mut o, MAX_WRITE);
    put32(&mut o, 1); // time_gran
    o.extend_from_slice(&((MAX_WRITE / 4096) as u16).to_le_bytes()); // max_pages
    o.resize(64, 0);
    o
}

fn out(unique: u64, error: u32, payload: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(OUT_HEADER + payload.len());
    put32(&mut o, (OUT_HEADER + payload.len()) as u32);
    o.extend_from_slice(&(-(error as i32)).to_le_bytes());
    put64(&mut o, unique);
    o.extend_from_slice(payload);
    o
}

/// The NUL-terminated name at `at`.
fn name(body: &[u8], at: usize) -> Option<&[u8]> {
    let rest = body.get(at..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    (end > 0).then(|| &rest[..end])
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn le64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| {
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        put32(&mut r, (IN_HEADER + body.len()) as u32);
        put32(&mut r, opcode);
        put64(&mut r, 99);
        put64(&mut r, nodeid);
        r.resize(IN_HEADER, 0);
        r.extend_from_slice(body);
        r
    }

    fn error(reply: &[u8]) -> i32 {
        i32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]])
    }

    #[test]
    fn a_file_is_looked_up_opened_read_written_and_listed() {
        let dir = std::env::temp_dir().join(format!("entangled-fuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), b"hello world").unwrap();
        let mut fs = FuseServer::new(Server::new(&dir, false).unwrap());

        let init = fs
            .handle(&request(op::INIT, 0, &[7, 0, 0, 0, 38, 0, 0, 0]))
            .unwrap();
        assert_eq!(error(&init), 0);
        assert_eq!(le32(&init, OUT_HEADER), 7);

        let entry = fs
            .handle(&request(op::LOOKUP, ROOT_NODE, b"hello.txt\0"))
            .unwrap();
        assert_eq!(error(&entry), 0);
        let node = le64(&entry, OUT_HEADER);
        assert_eq!(le64(&entry, OUT_HEADER + 40 + 8), 11, "size");

        let missing = fs
            .handle(&request(op::LOOKUP, ROOT_NODE, b"nope\0"))
            .unwrap();
        assert_eq!(error(&missing), -(errno::ENOENT as i32));
        // `..` at the root stays at the root (the 9P server's rule).
        let up = fs.handle(&request(op::LOOKUP, ROOT_NODE, b"..\0")).unwrap();
        let root = fs
            .handle(&request(op::GETATTR, ROOT_NODE, &[0; 16]))
            .unwrap();
        assert_eq!(error(&up), 0);
        assert_eq!(
            le64(&up, OUT_HEADER + 40),
            le64(&root, OUT_HEADER + 16),
            "no way out of the share"
        );

        let open = fs
            .handle(&request(op::OPEN, node, &[2, 0, 0, 0, 0, 0, 0, 0]))
            .unwrap();
        assert_eq!(error(&open), 0);
        let fh = le64(&open, OUT_HEADER);
        let mut read = Vec::new();
        put64(&mut read, fh);
        put64(&mut read, 6);
        put32(&mut read, 100);
        read.resize(40, 0);
        let data = fs.handle(&request(op::READ, node, &read)).unwrap();
        assert_eq!(&data[OUT_HEADER..], b"world");

        let mut write = Vec::new();
        put64(&mut write, fh);
        put64(&mut write, 0);
        put32(&mut write, 5);
        write.resize(40, 0);
        write.extend_from_slice(b"HELLO");
        let wrote = fs.handle(&request(op::WRITE, node, &write)).unwrap();
        assert_eq!(le32(&wrote, OUT_HEADER), 5);
        assert_eq!(
            std::fs::read(dir.join("hello.txt")).unwrap(),
            b"HELLO world"
        );

        let dir_open = fs
            .handle(&request(op::OPENDIR, ROOT_NODE, &[0; 8]))
            .unwrap();
        let dfh = le64(&dir_open, OUT_HEADER);
        let mut list = Vec::new();
        put64(&mut list, dfh);
        put64(&mut list, 0);
        put32(&mut list, 4096);
        list.resize(40, 0);
        let listing = fs.handle(&request(op::READDIR, ROOT_NODE, &list)).unwrap();
        let names = String::from_utf8_lossy(&listing[OUT_HEADER..]).into_owned();
        assert!(names.contains("hello.txt"), "{names:?}");

        assert!(fs
            .handle(&request(op::FORGET, node, &[1, 0, 0, 0, 0, 0, 0, 0]))
            .is_none());
        let unknown = fs.handle(&request(9999, ROOT_NODE, &[])).unwrap();
        assert_eq!(error(&unknown), -(ENOSYS as i32));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
