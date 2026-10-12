//! The 9P2000.L file server: one shared host directory, served to one guest.
//!
//! Portable by construction: every host operation is `std::fs`, with the few
//! that differ per host (positional I/O, inode numbers, mode bits) behind
//! small `cfg` helpers at the bottom.
//!
//! **The guest is untrusted, and here that means the host's filesystem.** A
//! fid names a path as a list of components *relative to the share's root*,
//! never a host path, and three rules keep every access inside the root:
//!
//! * **Names are single components.** A name the guest sends must not be
//!   empty, `.` or `..`, and must not contain a separator or NUL; on Windows
//!   it must also not contain a character the host would interpret (`:` opens
//!   an alternate data stream) or be a device name (`CON`, `NUL`, `COM1`…).
//!   `..` in a walk pops a component and stops at the root.
//! * **No symlink is ever followed.** Every resolution `lstat`s each
//!   component of the path, and one that is a symlink anywhere but at the end
//!   refuses the request with `ELOOP`; at the end only the operations that
//!   act on the link itself (`getattr`, `readlink`, `unlinkat`, `rename`) may
//!   name it. Re-checking the whole path on every request is what defeats the
//!   swap trick — walk to a real directory, replace it with a link, use the old
//!   fid — and requests are served one at a time, so the guest cannot race its
//!   own check. A symlink the guest *creates* must have a relative target
//!   without `..`, so it does not point a host program out of the share
//!   either.
//! * **Every table is bounded.** Fids ([`MAX_FIDS`]), names per walk
//!   ([`MAX_WALK`]), directory entries held for one listing
//!   ([`MAX_DIR_ENTRIES`]) and the message size ([`MAX_MSIZE`]).
//!
//! Ownership and permissions are presented, not mapped. Every file appears to
//! belong to the user who attached (Linux's default `access=user` attaches per
//! user), and every directory reads `rwx` and every file `rw` for everyone —
//! the host's execute bits kept, every write bit gone on a read-only share.
//! The guest's own permission check must pass, because the host's is the one
//! that counts (the VMM runs as the host user), and the inode Linux caches is
//! shared by every guest user: a share mounted by root shows its root as
//! root's, and with the host's `0755` the desktop user could not create
//! anything in it. (The mount's own anonymous attach is shown as root too —
//! see `attach`.)

use std::collections::HashMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::wire::{self, t, Qid, Reader, WireError, Writer, QTDIR, QTFILE, QTSYMLINK};

/// Largest message the server agrees to (`Tversion` negotiates down to it).
pub const MAX_MSIZE: u32 = 512 * 1024;
/// Smallest message size that leaves room for a useful reply.
pub const MIN_MSIZE: u32 = 4096;
/// Live fids one guest may hold.
pub const MAX_FIDS: usize = 16 * 1024;
/// Names in one `Twalk` (the protocol's own limit).
pub const MAX_WALK: usize = 16;
/// Entries one directory listing holds; a larger directory is listed this far.
pub const MAX_DIR_ENTRIES: usize = 64 * 1024;

/// Linux errno values the protocol carries.
pub mod errno {
    pub const EPERM: u32 = 1;
    pub const ENOENT: u32 = 2;
    pub const EIO: u32 = 5;
    pub const EBADF: u32 = 9;
    pub const EBUSY: u32 = 16;
    pub const EACCES: u32 = 13;
    pub const EEXIST: u32 = 17;
    pub const ENOTDIR: u32 = 20;
    pub const EISDIR: u32 = 21;
    pub const EINVAL: u32 = 22;
    pub const ENFILE: u32 = 23;
    pub const ENOSPC: u32 = 28;
    pub const EROFS: u32 = 30;
    pub const ENAMETOOLONG: u32 = 36;
    pub const ENOTEMPTY: u32 = 39;
    pub const ELOOP: u32 = 40;
    pub const EPROTO: u32 = 71;
    pub const EOPNOTSUPP: u32 = 95;
}

/// Linux open flags (`Tlopen`, `Tlcreate`).
mod oflag {
    pub const ACCMODE: u32 = 0o3;
    pub const WRONLY: u32 = 0o1;
    pub const RDWR: u32 = 0o2;
    pub const EXCL: u32 = 0o200;
    pub const TRUNC: u32 = 0o1000;
    pub const APPEND: u32 = 0o2000;
}

/// `Tsetattr` valid bits.
mod setattr {
    pub const MODE: u32 = 0x1;
    pub const SIZE: u32 = 0x8;
    pub const ATIME: u32 = 0x10;
    pub const MTIME: u32 = 0x20;
    pub const ATIME_SET: u32 = 0x80;
    pub const MTIME_SET: u32 = 0x100;
}

/// `AT_REMOVEDIR` for `Tunlinkat`.
const AT_REMOVEDIR: u32 = 0x200;
/// `Rgetattr`'s "basic" mask: everything up to `blocks`.
const GETATTR_BASIC: u64 = 0x7ff;
/// `V9FS_MAGIC`, what `statfs` reports as the filesystem type.
const V9FS_MAGIC: u32 = 0x0102_1997;

type Reply = Result<Vec<u8>, u32>;

struct DirEntry {
    name: String,
    qid: Qid,
    kind: u8,
}

enum Open {
    File { file: File, append: bool },
    Dir { listing: Option<Vec<DirEntry>> },
}

struct Fid {
    /// Components below the root; empty is the root.
    path: Vec<String>,
    uid: u32,
    open: Option<Open>,
}

/// One share's server state.
pub struct Server {
    root: PathBuf,
    read_only: bool,
    msize: u32,
    fids: HashMap<u32, Fid>,
}

fn io_errno(e: &io::Error) -> u32 {
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return code as u32;
    }
    match e.kind() {
        io::ErrorKind::NotFound => errno::ENOENT,
        io::ErrorKind::PermissionDenied => errno::EACCES,
        io::ErrorKind::AlreadyExists => errno::EEXIST,
        io::ErrorKind::DirectoryNotEmpty => errno::ENOTEMPTY,
        io::ErrorKind::NotADirectory => errno::ENOTDIR,
        io::ErrorKind::IsADirectory => errno::EISDIR,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidFilename => errno::EINVAL,
        io::ErrorKind::StorageFull => errno::ENOSPC,
        io::ErrorKind::ReadOnlyFilesystem => errno::EROFS,
        _ => errno::EIO,
    }
}

fn wire_errno(_: WireError) -> u32 {
    errno::EPROTO
}

/// Is `name` one component the host will take literally?
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 {
        return false;
    }
    if name.contains(['/', '\\', '\0']) {
        return false;
    }
    if cfg!(windows) {
        if name.contains([':', '*', '?', '"', '<', '>', '|'])
            || name.chars().any(|c| (c as u32) < 32)
            || name.ends_with(['.', ' '])
        {
            return false;
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            return false;
        }
    }
    true
}

/// A symlink target the guest may create: relative, and never climbing.
fn valid_link_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with(['/', '\\'])
        && !target.contains(['\0', ':', '\\'])
        && target.split('/').all(|c| c != "..")
}

fn fnv(parts: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes().iter().chain(b"/") {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

fn unix_time(t: io::Result<SystemTime>) -> (u64, u64) {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), u64::from(d.subsec_nanos())))
        .unwrap_or((0, 0))
}

fn qid_of(meta: &Metadata, path: &[String]) -> Qid {
    let kind = if meta.file_type().is_symlink() {
        QTSYMLINK
    } else if meta.is_dir() {
        QTDIR
    } else {
        QTFILE
    };
    Qid {
        kind,
        version: unix_time(meta.modified()).0 as u32,
        path: host::inode(meta).unwrap_or_else(|| fnv(path)),
    }
}

/// The mode the guest is shown: see the module docs.
fn present_mode(mode: u32, read_only: bool) -> u32 {
    const S_IFMT: u32 = 0o170_000;
    const S_IFDIR: u32 = 0o040_000;
    const S_IFREG: u32 = 0o100_000;
    let mode = match mode & S_IFMT {
        S_IFDIR => mode | 0o777,
        S_IFREG => mode | 0o666,
        _ => mode,
    };
    if read_only {
        mode & !0o222
    } else {
        mode
    }
}

/// The `d_type` a directory entry carries.
fn dirent_type(qid: Qid) -> u8 {
    match qid.kind {
        QTDIR => 4,
        QTSYMLINK => 10,
        _ => 8,
    }
}

impl Server {
    /// A server for `root`, which must be an existing directory.
    pub fn new(root: &Path, read_only: bool) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Self {
            root,
            read_only,
            msize: MIN_MSIZE,
            fids: HashMap::new(),
        })
    }

    /// The negotiated message size.
    pub fn msize(&self) -> u32 {
        self.msize
    }

    /// Live fids, for tests and diagnostics.
    pub fn fid_count(&self) -> usize {
        self.fids.len()
    }

    /// Forgets every fid — a device reset.
    pub fn reset(&mut self) {
        self.fids.clear();
        self.msize = MIN_MSIZE;
    }

    /// Serves one T-message (header included) and returns the R-message.
    pub fn handle(&mut self, msg: &[u8]) -> Vec<u8> {
        if msg.len() < wire::HEADER {
            return wire::lerror(0xffff, errno::EPROTO);
        }
        let kind = msg[4];
        let tag = u16::from_le_bytes([msg[5], msg[6]]);
        let mut r = Reader::new(&msg[wire::HEADER..]);
        match self.dispatch(kind, tag, &mut r) {
            Ok(reply) => {
                tracing::trace!(kind, len = reply.len(), "9p request served");
                reply
            }
            Err(code) => {
                tracing::debug!(kind, errno = code, "9p request refused");
                wire::lerror(tag, code)
            }
        }
    }

    fn dispatch(&mut self, kind: u8, tag: u16, r: &mut Reader<'_>) -> Reply {
        match kind {
            t::TVERSION => self.version(tag, r),
            t::TATTACH => self.attach(tag, r),
            t::TWALK => self.walk(tag, r),
            t::TCLUNK => {
                let fid = r.u32().map_err(wire_errno)?;
                self.fids.remove(&fid).ok_or(errno::EBADF)?;
                Ok(Writer::new(t::TCLUNK + 1, tag).finish())
            }
            t::TFLUSH => Ok(Writer::new(t::TFLUSH + 1, tag).finish()),
            t::TGETATTR => self.getattr(tag, r),
            t::TSETATTR => self.setattr(tag, r),
            t::TLOPEN => self.lopen(tag, r),
            t::TLCREATE => self.lcreate(tag, r),
            t::TREAD => self.read(tag, r),
            t::TWRITE => self.write(tag, r),
            t::TREADDIR => self.readdir(tag, r),
            t::TSTATFS => self.statfs(tag, r),
            t::TMKDIR => self.mkdir(tag, r),
            t::TSYMLINK => self.symlink(tag, r),
            t::TLINK => self.link(tag, r),
            t::TREADLINK => self.readlink(tag, r),
            t::TRENAME => self.rename(tag, r),
            t::TRENAMEAT => self.renameat(tag, r),
            t::TUNLINKAT => self.unlinkat(tag, r),
            t::TREMOVE => self.remove(tag, r),
            t::TFSYNC => self.fsync(tag, r),
            t::TLOCK => {
                // Advisory locks are granted: one guest, and the host's own
                // programs do not take POSIX locks on a share's files.
                Ok(Writer::new(t::TLOCK + 1, tag).u8(0).finish())
            }
            t::TGETLOCK => self.getlock(tag, r),
            t::TMKNOD => Err(errno::EPERM),
            t::TAUTH | t::TXATTRWALK | t::TXATTRCREATE => Err(errno::EOPNOTSUPP),
            _ => Err(errno::EOPNOTSUPP),
        }
    }

    // ------------------------------------------------------------ paths

    fn host_path(&self, path: &[String]) -> PathBuf {
        let mut out = self.root.clone();
        for part in path {
            out.push(part);
        }
        out
    }

    /// The host path for `path`, after checking that no component is a
    /// symlink (the last one may be when `final_link` allows it).
    fn resolve(&self, path: &[String], final_link: bool) -> Result<PathBuf, u32> {
        let mut at = self.root.clone();
        for (i, part) in path.iter().enumerate() {
            at.push(part);
            let last = i + 1 == path.len();
            match std::fs::symlink_metadata(&at) {
                Ok(meta) if meta.file_type().is_symlink() && !(last && final_link) => {
                    return Err(errno::ELOOP)
                }
                Ok(_) => {}
                Err(e) => return Err(io_errno(&e)),
            }
        }
        Ok(at)
    }

    fn fid(&self, fid: u32) -> Result<&Fid, u32> {
        self.fids.get(&fid).ok_or(errno::EBADF)
    }

    fn fid_mut(&mut self, fid: u32) -> Result<&mut Fid, u32> {
        self.fids.get_mut(&fid).ok_or(errno::EBADF)
    }

    fn insert_fid(&mut self, id: u32, fid: Fid) -> Result<(), u32> {
        if !self.fids.contains_key(&id) && self.fids.len() >= MAX_FIDS {
            return Err(errno::ENFILE);
        }
        self.fids.insert(id, fid);
        Ok(())
    }

    /// A child of a directory fid: its components and host path, with the
    /// directory itself checked.
    fn child(&self, dfid: u32, name: &str) -> Result<(Vec<String>, PathBuf), u32> {
        if !valid_name(name) {
            return Err(errno::EINVAL);
        }
        let dir = self.fid(dfid)?;
        let dir_host = self.resolve(&dir.path, false)?;
        if !dir_host.is_dir() {
            return Err(errno::ENOTDIR);
        }
        let mut path = dir.path.clone();
        path.push(name.to_string());
        Ok((path, dir_host.join(name)))
    }

    fn writable(&self) -> Result<(), u32> {
        if self.read_only {
            Err(errno::EROFS)
        } else {
            Ok(())
        }
    }

    /// Points every fid at or below `from` at `to` instead, after a rename.
    fn rebase_fids(&mut self, from: &[String], to: &[String]) {
        for fid in self.fids.values_mut() {
            if fid.path.starts_with(from) {
                let mut path = to.to_vec();
                path.extend_from_slice(&fid.path[from.len()..]);
                fid.path = path;
            }
        }
    }

    // ------------------------------------------------------------ session

    fn version(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let msize = r.u32().map_err(wire_errno)?;
        let version = r.string().map_err(wire_errno)?;
        // A version message resets the session.
        self.fids.clear();
        self.msize = msize.clamp(MIN_MSIZE, MAX_MSIZE);
        let agreed = if version.starts_with("9P2000.L") {
            "9P2000.L"
        } else {
            "unknown"
        };
        Ok(Writer::new(t::TVERSION + 1, tag)
            .u32(self.msize)
            .string(agreed)
            .finish())
    }

    fn attach(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let _afid = r.u32().map_err(wire_errno)?;
        let _uname = r.string().map_err(wire_errno)?;
        let _aname = r.string().map_err(wire_errno)?;
        let n_uname = r.u32().map_err(wire_errno)?;
        // Linux's mount attaches once with no user at all (`-1`) before any
        // user's own attach. Shown to the guest, that uid is *unmapped*, and
        // the kernel refuses every write to an inode whose owner is unmapped,
        // whatever its mode (`HAS_UNMAPPED_ID` in `inode_permission`): the
        // desktop user's first create in a fresh mount failed with EACCES
        // before reaching the host (measured 2026-10-12). Root it is.
        let uid = if n_uname == u32::MAX { 0 } else { n_uname };
        let meta = std::fs::metadata(&self.root).map_err(|e| io_errno(&e))?;
        let qid = qid_of(&meta, &[]);
        self.insert_fid(
            fid,
            Fid {
                path: Vec::new(),
                uid,
                open: None,
            },
        )?;
        Ok(Writer::new(t::TATTACH + 1, tag).qid(qid).finish())
    }

    fn walk(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let newfid = r.u32().map_err(wire_errno)?;
        let count = usize::from(r.u16().map_err(wire_errno)?);
        if count > MAX_WALK {
            return Err(errno::EINVAL);
        }
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            names.push(r.string().map_err(wire_errno)?);
        }
        let (start, uid) = {
            let f = self.fid(fid)?;
            (f.path.clone(), f.uid)
        };
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(errno::EBADF);
        }
        // The starting point must itself still be a real path.
        self.resolve(&start, false)?;
        let mut path = start;
        let mut qids = Vec::with_capacity(count);
        for (i, name) in names.iter().enumerate() {
            let next = match *name {
                ".." => {
                    let mut p = path.clone();
                    p.pop();
                    p
                }
                "." => path.clone(),
                other if valid_name(other) => {
                    let mut p = path.clone();
                    p.push(other.to_string());
                    p
                }
                _ if i == 0 => return Err(errno::EINVAL),
                _ => break,
            };
            // Descending is only through a directory, never through a link.
            let host = self.host_path(&next);
            let meta = match std::fs::symlink_metadata(&host) {
                Ok(meta) => meta,
                Err(e) if i == 0 => return Err(io_errno(&e)),
                Err(_) => break,
            };
            qids.push(qid_of(&meta, &next));
            path = next;
            if !meta.is_dir() && i + 1 < names.len() {
                break;
            }
        }
        if qids.len() == names.len() {
            self.insert_fid(
                newfid,
                Fid {
                    path,
                    uid,
                    open: None,
                },
            )?;
        }
        let mut w = Writer::new(t::TWALK + 1, tag);
        w.u16(qids.len() as u16);
        for q in qids {
            w.qid(q);
        }
        Ok(w.finish())
    }

    // ------------------------------------------------------------ attributes

    fn getattr(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let _mask = r.u64().map_err(wire_errno)?;
        let f = self.fid(fid)?;
        let host = self.resolve(&f.path, true)?;
        let meta = std::fs::symlink_metadata(&host).map_err(|e| io_errno(&e))?;
        let qid = qid_of(&meta, &f.path);
        let (atime, atime_ns) = unix_time(meta.accessed());
        let (mtime, mtime_ns) = unix_time(meta.modified());
        let (ctime, ctime_ns) = host::ctime(&meta).unwrap_or((mtime, mtime_ns));
        tracing::trace!(
            path = ?f.path,
            mode = format_args!("{:o}", present_mode(host::mode(&meta), self.read_only)),
            uid = f.uid,
            "9p getattr"
        );
        let mut w = Writer::new(t::TGETATTR + 1, tag);
        w.u64(GETATTR_BASIC)
            .qid(qid)
            .u32(present_mode(host::mode(&meta), self.read_only))
            .u32(f.uid)
            .u32(f.uid)
            .u64(host::nlink(&meta))
            .u64(0)
            .u64(meta.len())
            .u64(4096)
            .u64(host::blocks(&meta))
            .u64(atime)
            .u64(atime_ns)
            .u64(mtime)
            .u64(mtime_ns)
            .u64(ctime)
            .u64(ctime_ns)
            .u64(0)
            .u64(0)
            .u64(0)
            .u64(0);
        Ok(w.finish())
    }

    fn setattr(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let valid = r.u32().map_err(wire_errno)?;
        let mode = r.u32().map_err(wire_errno)?;
        let _uid = r.u32().map_err(wire_errno)?;
        let _gid = r.u32().map_err(wire_errno)?;
        let size = r.u64().map_err(wire_errno)?;
        let atime = (r.u64().map_err(wire_errno)?, r.u64().map_err(wire_errno)?);
        let mtime = (r.u64().map_err(wire_errno)?, r.u64().map_err(wire_errno)?);
        let changes = setattr::MODE | setattr::SIZE | setattr::ATIME | setattr::MTIME;
        if valid & changes != 0 {
            self.writable()?;
        }
        let path = self.fid(fid)?.path.clone();
        let host = self.resolve(&path, false)?;
        if valid & setattr::MODE != 0 {
            host::set_mode(&host, mode).map_err(|e| io_errno(&e))?;
        }
        if valid & setattr::SIZE != 0 {
            let f = self.fid_mut(fid)?;
            match &f.open {
                Some(Open::File { file, .. }) => file.set_len(size),
                _ => OpenOptions::new()
                    .write(true)
                    .open(&host)
                    .and_then(|file| file.set_len(size)),
            }
            .map_err(|e| io_errno(&e))?;
        }
        if valid & (setattr::ATIME | setattr::MTIME) != 0 {
            let at = |(s, ns): (u64, u64), set: bool| {
                if set {
                    UNIX_EPOCH + Duration::new(s, (ns % 1_000_000_000) as u32)
                } else {
                    SystemTime::now()
                }
            };
            let mut times = std::fs::FileTimes::new();
            if valid & setattr::ATIME != 0 {
                times = times.set_accessed(at(atime, valid & setattr::ATIME_SET != 0));
            }
            if valid & setattr::MTIME != 0 {
                times = times.set_modified(at(mtime, valid & setattr::MTIME_SET != 0));
            }
            // A directory cannot be opened for writing everywhere; its times
            // are a nicety, not something to fail a `touch` over.
            if let Ok(file) = OpenOptions::new().write(true).open(&host) {
                file.set_times(times).map_err(|e| io_errno(&e))?;
            }
        }
        Ok(Writer::new(t::TSETATTR + 1, tag).finish())
    }

    fn statfs(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        self.fid(fid)?;
        let (free, total) = disk_image::disk_space(&self.root).unwrap_or((0, 0));
        Ok(Writer::new(t::TSTATFS + 1, tag)
            .u32(V9FS_MAGIC)
            .u32(4096)
            .u64(total / 4096)
            .u64(free / 4096)
            .u64(free / 4096)
            .u64(0)
            .u64(0)
            .u64(fnv(&[self.root.display().to_string()]))
            .u32(255)
            .finish())
    }

    // ------------------------------------------------------------ open, I/O

    fn iounit(&self) -> u32 {
        self.msize.saturating_sub(24)
    }

    fn open_file(&self, host: &Path, flags: u32, create: Option<bool>) -> Result<File, u32> {
        let access = flags & oflag::ACCMODE;
        let write = access == oflag::WRONLY || access == oflag::RDWR;
        let mut options = OpenOptions::new();
        options
            .read(access != oflag::WRONLY)
            .write(write || create.is_some());
        if write && flags & oflag::TRUNC != 0 {
            options.truncate(true);
        }
        match create {
            Some(true) => {
                options.create_new(true);
            }
            Some(false) => {
                options.create(true);
            }
            None => {}
        }
        options.open(host).map_err(|e| io_errno(&e))
    }

    fn lopen(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let flags = r.u32().map_err(wire_errno)?;
        let path = self.fid(fid)?.path.clone();
        if self.fid(fid)?.open.is_some() {
            return Err(errno::EBADF);
        }
        let host = self.resolve(&path, false)?;
        let meta = std::fs::metadata(&host).map_err(|e| io_errno(&e))?;
        let open = if meta.is_dir() {
            Open::Dir { listing: None }
        } else {
            let access = flags & oflag::ACCMODE;
            if access != 0 || flags & oflag::TRUNC != 0 {
                self.writable()?;
            }
            Open::File {
                file: self.open_file(&host, flags, None)?,
                append: flags & oflag::APPEND != 0,
            }
        };
        let qid = qid_of(&meta, &path);
        let iounit = self.iounit();
        self.fid_mut(fid)?.open = Some(open);
        Ok(Writer::new(t::TLOPEN + 1, tag)
            .qid(qid)
            .u32(iounit)
            .finish())
    }

    fn lcreate(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        let flags = r.u32().map_err(wire_errno)?;
        let mode = r.u32().map_err(wire_errno)?;
        let _gid = r.u32().map_err(wire_errno)?;
        self.writable()?;
        let (path, host) = self.child(fid, name)?;
        let file = self.open_file(&host, flags, Some(flags & oflag::EXCL != 0))?;
        let _ = host::set_mode(&host, mode);
        let meta = file.metadata().map_err(|e| io_errno(&e))?;
        let qid = qid_of(&meta, &path);
        let iounit = self.iounit();
        let f = self.fid_mut(fid)?;
        f.path = path;
        f.open = Some(Open::File {
            file,
            append: flags & oflag::APPEND != 0,
        });
        Ok(Writer::new(t::TLCREATE + 1, tag)
            .qid(qid)
            .u32(iounit)
            .finish())
    }

    fn read(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let offset = r.u64().map_err(wire_errno)?;
        let count = r.u32().map_err(wire_errno)?;
        let room = self.msize.saturating_sub(11);
        let count = count.min(room) as usize;
        let f = self.fid(fid)?;
        let Some(Open::File { file, .. }) = &f.open else {
            return Err(errno::EBADF);
        };
        let mut data = vec![0u8; count];
        let mut got = 0;
        while got < count {
            let at = offset.checked_add(got as u64).ok_or(errno::EINVAL)?;
            match host::read_at(file, &mut data[got..], at) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_errno(&e)),
            }
        }
        let mut w = Writer::new(t::TREAD + 1, tag);
        w.u32(got as u32).bytes(&data[..got]);
        Ok(w.finish())
    }

    fn write(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let offset = r.u64().map_err(wire_errno)?;
        let count = r.u32().map_err(wire_errno)?;
        let data = r.bytes(count as usize).map_err(wire_errno)?;
        self.writable()?;
        let f = self.fid(fid)?;
        let Some(Open::File { file, append }) = &f.open else {
            return Err(errno::EBADF);
        };
        let mut at = if *append {
            file.metadata().map_err(|e| io_errno(&e))?.len()
        } else {
            offset
        };
        let mut done = 0;
        while done < data.len() {
            match host::write_at(file, &data[done..], at) {
                Ok(0) => break,
                Ok(n) => {
                    done += n;
                    at = at.saturating_add(n as u64);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if done == 0 => return Err(io_errno(&e)),
                Err(_) => break,
            }
        }
        Ok(Writer::new(t::TWRITE + 1, tag).u32(done as u32).finish())
    }

    fn fsync(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let _datasync = r.u32().map_err(wire_errno)?;
        if let Some(Open::File { file, .. }) = &self.fid(fid)?.open {
            file.sync_all().map_err(|e| io_errno(&e))?;
        }
        Ok(Writer::new(t::TFSYNC + 1, tag).finish())
    }

    fn getlock(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let _kind = r.u8().map_err(wire_errno)?;
        let start = r.u64().map_err(wire_errno)?;
        let length = r.u64().map_err(wire_errno)?;
        let proc_id = r.u32().map_err(wire_errno)?;
        let client = r.string().map_err(wire_errno)?;
        self.fid(fid)?;
        // Nothing holds a lock: F_UNLCK.
        Ok(Writer::new(t::TGETLOCK + 1, tag)
            .u8(2)
            .u64(start)
            .u64(length)
            .u32(proc_id)
            .string(client)
            .finish())
    }

    fn readdir(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let offset = r.u64().map_err(wire_errno)?;
        let count = r.u32().map_err(wire_errno)?;
        let count = count.min(self.msize.saturating_sub(11)) as usize;
        let path = self.fid(fid)?.path.clone();
        let host = self.resolve(&path, false)?;
        let f = self.fid_mut(fid)?;
        let Some(Open::Dir { listing }) = &mut f.open else {
            return Err(errno::EBADF);
        };
        // A listing is taken at offset 0 and served from until the next one,
        // so the cookies stay stable while the guest pages through it.
        if offset == 0 || listing.is_none() {
            let mut entries = Vec::new();
            let read = std::fs::read_dir(&host).map_err(|e| io_errno(&e))?;
            for entry in read.flatten().take(MAX_DIR_ENTRIES) {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                let mut child = path.clone();
                child.push(name.clone());
                let qid = qid_of(&meta, &child);
                entries.push(DirEntry {
                    name,
                    kind: dirent_type(qid),
                    qid,
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            *listing = Some(entries);
        }
        let entries = listing.as_deref().unwrap_or(&[]);
        let mut body = Vec::new();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, entry) in entries.iter().enumerate().skip(start) {
            let size = wire::QID_LEN + 8 + 1 + 2 + entry.name.len();
            if body.len() + size > count {
                break;
            }
            let mut e = Writer::new(0, 0);
            e.qid(entry.qid)
                .u64(index as u64 + 1)
                .u8(entry.kind)
                .string(&entry.name);
            body.extend_from_slice(&e.finish()[wire::HEADER..]);
        }
        let mut w = Writer::new(t::TREADDIR + 1, tag);
        w.u32(body.len() as u32).bytes(&body);
        Ok(w.finish())
    }

    // ------------------------------------------------------------ namespace

    fn mkdir(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let dfid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        let mode = r.u32().map_err(wire_errno)?;
        let _gid = r.u32().map_err(wire_errno)?;
        self.writable()?;
        let (path, host) = self.child(dfid, name)?;
        std::fs::create_dir(&host).map_err(|e| io_errno(&e))?;
        let _ = host::set_mode(&host, mode);
        let meta = std::fs::symlink_metadata(&host).map_err(|e| io_errno(&e))?;
        Ok(Writer::new(t::TMKDIR + 1, tag)
            .qid(qid_of(&meta, &path))
            .finish())
    }

    fn symlink(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let dfid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        let target = r.string().map_err(wire_errno)?;
        let _gid = r.u32().map_err(wire_errno)?;
        self.writable()?;
        if !valid_link_target(target) {
            return Err(errno::EPERM);
        }
        let (path, host) = self.child(dfid, name)?;
        host::symlink(target, &host).map_err(|e| io_errno(&e))?;
        let meta = std::fs::symlink_metadata(&host).map_err(|e| io_errno(&e))?;
        Ok(Writer::new(t::TSYMLINK + 1, tag)
            .qid(qid_of(&meta, &path))
            .finish())
    }

    fn link(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let dfid = r.u32().map_err(wire_errno)?;
        let fid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        self.writable()?;
        let target = self.resolve(&self.fid(fid)?.path.clone(), false)?;
        let (_, host) = self.child(dfid, name)?;
        std::fs::hard_link(&target, &host).map_err(|e| io_errno(&e))?;
        Ok(Writer::new(t::TLINK + 1, tag).finish())
    }

    fn readlink(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let host = self.resolve(&self.fid(fid)?.path.clone(), true)?;
        let target = std::fs::read_link(&host).map_err(|e| io_errno(&e))?;
        let target = target.to_str().ok_or(errno::EINVAL)?.replace('\\', "/");
        if target.len() > usize::from(u16::MAX) {
            return Err(errno::ENAMETOOLONG);
        }
        Ok(Writer::new(t::TREADLINK + 1, tag).string(&target).finish())
    }

    fn rename(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        let dfid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        self.writable()?;
        let from = self.fid(fid)?.path.clone();
        if from.is_empty() {
            return Err(errno::EINVAL);
        }
        let from_host = self.resolve(&from, true)?;
        let (to, to_host) = self.child(dfid, name)?;
        if to.starts_with(&from) && to.len() > from.len() {
            return Err(errno::EINVAL);
        }
        std::fs::rename(&from_host, &to_host).map_err(|e| io_errno(&e))?;
        self.rebase_fids(&from, &to);
        Ok(Writer::new(t::TRENAME + 1, tag).finish())
    }

    fn renameat(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let olddir = r.u32().map_err(wire_errno)?;
        let oldname = r.string().map_err(wire_errno)?;
        let newdir = r.u32().map_err(wire_errno)?;
        let newname = r.string().map_err(wire_errno)?;
        self.writable()?;
        let (from, from_host) = self.child(olddir, oldname)?;
        let (to, to_host) = self.child(newdir, newname)?;
        if to.starts_with(&from) && to.len() > from.len() {
            return Err(errno::EINVAL);
        }
        // The source itself may be a link (renaming one moves the link).
        std::fs::symlink_metadata(&from_host).map_err(|e| io_errno(&e))?;
        std::fs::rename(&from_host, &to_host).map_err(|e| io_errno(&e))?;
        self.rebase_fids(&from, &to);
        Ok(Writer::new(t::TRENAMEAT + 1, tag).finish())
    }

    fn unlink_host(host: &Path, dir: bool) -> Result<(), u32> {
        let meta = std::fs::symlink_metadata(host).map_err(|e| io_errno(&e))?;
        if dir {
            if !meta.is_dir() {
                return Err(errno::ENOTDIR);
            }
            std::fs::remove_dir(host)
        } else {
            if meta.is_dir() {
                return Err(errno::EISDIR);
            }
            std::fs::remove_file(host)
        }
        .map_err(|e| io_errno(&e))
    }

    fn unlinkat(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let dfid = r.u32().map_err(wire_errno)?;
        let name = r.string().map_err(wire_errno)?;
        let flags = r.u32().map_err(wire_errno)?;
        self.writable()?;
        let (_, host) = self.child(dfid, name)?;
        Self::unlink_host(&host, flags & AT_REMOVEDIR != 0)?;
        Ok(Writer::new(t::TUNLINKAT + 1, tag).finish())
    }

    fn remove(&mut self, tag: u16, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().map_err(wire_errno)?;
        // The fid is clunked whether or not the remove works (the protocol's
        // rule).
        let f = self.fids.remove(&fid).ok_or(errno::EBADF)?;
        self.writable()?;
        if f.path.is_empty() {
            return Err(errno::EBUSY);
        }
        let host = self.resolve(&f.path, true)?;
        let dir = std::fs::symlink_metadata(&host)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        drop(f);
        Self::unlink_host(&host, dir)?;
        Ok(Writer::new(t::TREMOVE + 1, tag).finish())
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

/// Host-specific helpers.
#[cfg(unix)]
mod host {
    use std::fs::{File, Metadata};
    use std::io;
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use std::path::Path;

    pub fn inode(meta: &Metadata) -> Option<u64> {
        Some(meta.ino())
    }
    pub fn mode(meta: &Metadata) -> u32 {
        meta.mode()
    }
    pub fn nlink(meta: &Metadata) -> u64 {
        meta.nlink()
    }
    pub fn blocks(meta: &Metadata) -> u64 {
        meta.blocks()
    }
    pub fn ctime(meta: &Metadata) -> Option<(u64, u64)> {
        Some((meta.ctime().max(0) as u64, meta.ctime_nsec().max(0) as u64))
    }
    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
    }
    pub fn read_at(file: &File, buf: &mut [u8], at: u64) -> io::Result<usize> {
        file.read_at(buf, at)
    }
    pub fn write_at(file: &File, buf: &[u8], at: u64) -> io::Result<usize> {
        file.write_at(buf, at)
    }
    pub fn symlink(target: &str, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

#[cfg(windows)]
mod host {
    use std::fs::{File, Metadata};
    use std::io;
    use std::os::windows::fs::FileExt;
    use std::path::Path;

    pub fn inode(_: &Metadata) -> Option<u64> {
        None
    }
    pub fn mode(meta: &Metadata) -> u32 {
        let base = if meta.file_type().is_symlink() {
            0o120_777
        } else if meta.is_dir() {
            0o040_755
        } else {
            0o100_644
        };
        if meta.permissions().readonly() {
            base & !0o222
        } else {
            base
        }
    }
    pub fn nlink(_: &Metadata) -> u64 {
        1
    }
    pub fn blocks(meta: &Metadata) -> u64 {
        meta.len().div_ceil(512)
    }
    pub fn ctime(_: &Metadata) -> Option<(u64, u64)> {
        None
    }
    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        // Windows has one bit to give: read-only, when no write bit is set.
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        std::fs::set_permissions(path, permissions)
    }
    pub fn read_at(file: &File, buf: &mut [u8], at: u64) -> io::Result<usize> {
        file.seek_read(buf, at)
    }
    pub fn write_at(file: &File, buf: &[u8], at: u64) -> io::Result<usize> {
        file.seek_write(buf, at)
    }
    pub fn symlink(target: &str, link: &Path) -> io::Result<()> {
        // Creating a symlink on Windows needs a privilege a desktop user
        // usually lacks; report it as the permission error it is.
        std::os::windows::fs::symlink_file(target.replace('/', "\\"), link)
    }
}
