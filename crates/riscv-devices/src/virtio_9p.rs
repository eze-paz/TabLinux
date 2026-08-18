//! virtio-9p (device id 9) speaking 9P2000.L, over the shared MMIO transport.
//!
//! This is a shared folder: the guest mounts it with
//!   mount -t 9p -o trans=virtio,version=9p2000.L,<tag> /mnt/shared
//! and every file operation becomes a 9p message on the request queue, which
//! `handle` services synchronously against an in-memory tree.
//!
//! Why in-memory rather than straight to OPFS: `handle` runs inside the guest's
//! queue-notify MMIO store, with nowhere to await, and OPFS's directory API is
//! promise-only. So the filesystem lives here as plain Rust, serviced
//! synchronously, and the host mirrors it to/from OPFS out of band (seed at
//! attach, read changes back to flush). The block device solves the same
//! sync/async split with a sync access handle; a whole filesystem has no sync
//! handle, so it is held here instead.
//!
//! 9P2000.L, not the older 9P2000/.u: it is what modern Linux mounts by default
//! and it carries real errno values (Rlerror), stat (Tgetattr) and directory
//! reads (Treaddir) as first-class messages rather than the string-encoded
//! stat of legacy 9p.

extern crate alloc;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use alloc::string::ToString;
use crate::virtio::{
    sg_len, sg_read, sg_write, Buf, Chain, GuestMem, HostReq, VirtioDevice, VIRTIO_F_VERSION_1,
};

// 9P2000.L message types (Linux include/net/9p/9p.h).
const TLERROR: u8 = 6;
const RLERROR: u8 = 7;
const TSTATFS: u8 = 8;
const TLOPEN: u8 = 12;
const TLCREATE: u8 = 14;
const TSYMLINK: u8 = 16;
const TMKNOD: u8 = 18;
const TREADLINK: u8 = 22;
const TGETATTR: u8 = 24;
const TSETATTR: u8 = 26;
const TXATTRWALK: u8 = 30;
const TREADDIR: u8 = 40;
const TFSYNC: u8 = 50;
const TLINK: u8 = 70;
const TMKDIR: u8 = 72;
const TRENAMEAT: u8 = 74;
const TUNLINKAT: u8 = 76;
const TVERSION: u8 = 100;
const TATTACH: u8 = 104;
const TFLUSH: u8 = 108;
const TWALK: u8 = 110;
const TREAD: u8 = 116;
const TWRITE: u8 = 118;
const TCLUNK: u8 = 120;
const TREMOVE: u8 = 122;

// Errno values the guest understands (asm-generic).
const ENOENT: u32 = 2;
const EIO: u32 = 5;
const EBADF: u32 = 9;
const ENOMEM: u32 = 12;
const EEXIST: u32 = 17;
const ENOTDIR: u32 = 20;
const EISDIR: u32 = 21;
const EINVAL: u32 = 22;
const ENOTEMPTY: u32 = 39;
const ENOSYS: u32 = 38;
/// Not a real errno: a handler returns this to signal it parked the request on
/// the host (see `Virtio9p::defer_pending`). `dispatch` never frames it.
const DEFER: u32 = 0xFFFF_FFFF;

// Unix mode bits.
const S_IFDIR: u32 = 0o40000;
const S_IFREG: u32 = 0o100000;

// QID type bits.
const QTDIR: u8 = 0x80;
const QTFILE: u8 = 0x00;

// Linux d_type values for Treaddir entries.
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

/// Our upper bound on msize. The guest proposes one at Tversion; we answer with
/// the smaller. 128 KiB comfortably covers a readdir of a large directory and a
/// bulk read without the client having to fragment much.
const MSIZE_MAX: u32 = 128 * 1024;

/// One node in the tree. Index into `P9Fs::inodes` is the qid path.
struct Inode {
    is_dir: bool,
    /// File contents (empty for a directory).
    data: Vec<u8>,
    /// Directory entries: name -> inode index. Empty for a file.
    children: Vec<(String, u32)>,
    parent: u32,
    /// Unix permission bits (the S_IF* type is added from `is_dir`).
    perm: u32,
    mtime: u64,
    /// Lazy mode: whether contents are present — a file's `data`, a directory's
    /// `children`. Always true in the seeded (non-lazy) mode. In lazy mode a
    /// node starts as an unloaded stub and is filled on first access by a host
    /// fetch (see `Virtio9p`'s defer/supply path).
    loaded: bool,
    /// A stub file's size, learnt from its parent's directory listing, so
    /// getattr can answer without faulting the bytes in.
    size: u64,
}

impl Inode {
    fn dir(parent: u32) -> Self {
        Inode { is_dir: true, data: Vec::new(), children: Vec::new(), parent, perm: 0o755, mtime: 0, loaded: true, size: 0 }
    }
    fn file(parent: u32) -> Self {
        Inode { is_dir: false, data: Vec::new(), children: Vec::new(), parent, perm: 0o644, mtime: 0, loaded: true, size: 0 }
    }
    /// An unlisted directory: it exists, but its children are not known until a
    /// host listing faults them in.
    fn stub_dir(parent: u32) -> Self {
        Inode { is_dir: true, data: Vec::new(), children: Vec::new(), parent, perm: 0o755, mtime: 0, loaded: false, size: 0 }
    }
    /// An unread file: its size is known from the parent listing, its bytes are
    /// not fetched until first read.
    fn stub_file(parent: u32, size: u64) -> Self {
        Inode { is_dir: false, data: Vec::new(), children: Vec::new(), parent, perm: 0o644, mtime: 0, loaded: false, size }
    }
    fn mode(&self) -> u32 {
        self.perm | if self.is_dir { S_IFDIR } else { S_IFREG }
    }
    fn qid_type(&self) -> u8 {
        if self.is_dir { QTDIR } else { QTFILE }
    }
}

/// A client handle onto an inode. Walk creates them, clunk drops them.
#[derive(Clone, Copy)]
struct Fid {
    inode: u32,
}

/// The in-memory 9p filesystem.
pub struct P9Fs {
    inodes: Vec<Inode>,
    fids: BTreeMap<u32, Fid>,
    msize: u32,
    /// Bumped whenever the tree changes, so the host knows to flush to OPFS.
    dirty: u64,
}

impl P9Fs {
    pub fn new() -> Self {
        // Inode 0 is the root, its own parent.
        let root = Inode::dir(0);
        P9Fs { inodes: vec![root], fids: BTreeMap::new(), msize: 8192, dirty: 0 }
    }

    fn alloc(&mut self, node: Inode) -> u32 {
        self.inodes.push(node);
        (self.inodes.len() - 1) as u32
    }

    fn child(&self, dir: u32, name: &str) -> Option<u32> {
        self.inodes[dir as usize]
            .children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, i)| *i)
    }

    /// Resolve a `/`-separated path to its inode, from the root. None if any
    /// component is missing.
    fn resolve(&self, path: &str) -> Option<u32> {
        let mut cur = 0u32;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self.child(cur, seg)?;
        }
        Some(cur)
    }

    /// The `/`-separated OPFS-relative path of an inode, walked up through its
    /// parents. The root (inode 0) is the empty string. Used to name the file
    /// or directory a lazy fetch needs.
    fn path_of(&self, mut ino: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        while ino != 0 {
            let p = self.inodes[ino as usize].parent;
            if let Some((name, _)) =
                self.inodes[p as usize].children.iter().find(|(_, i)| *i == ino)
            {
                parts.push(name);
            }
            ino = p;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Populate a directory's children from a host listing: repeated records of
    /// `namelen[u16 LE] | name | flags[u8] (bit0 = is_dir) | size[u64 LE]`.
    /// Entries already present (e.g. from an earlier partial walk) are left
    /// alone; new ones become unloaded stubs.
    fn apply_listing(&mut self, dir: u32, payload: &[u8]) {
        let mut p = 0usize;
        while p + 2 <= payload.len() {
            let nl = u16::from_le_bytes([payload[p], payload[p + 1]]) as usize;
            p += 2;
            if p + nl + 1 + 8 > payload.len() {
                break;
            }
            let name = match core::str::from_utf8(&payload[p..p + nl]) {
                Ok(s) => s.to_string(),
                Err(_) => break,
            };
            p += nl;
            let flags = payload[p];
            p += 1;
            let size = u64::from_le_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            if name.is_empty() || name == "." || name == ".." || self.child(dir, &name).is_some() {
                continue;
            }
            let node = if flags & 1 != 0 { Inode::stub_dir(dir) } else { Inode::stub_file(dir, size) };
            let n = self.alloc(node);
            self.inodes[dir as usize].children.push((name, n));
        }
    }

    // ── host-side helpers, for seeding and flushing against OPFS ────────────

    /// Create or overwrite a file at a `/`-separated path, making parent
    /// directories as needed. Used to seed the tree from OPFS at attach.
    pub fn put_file(&mut self, path: &str, data: Vec<u8>) {
        let mut cur = 0u32;
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return;
        }
        for seg in &parts[..parts.len() - 1] {
            cur = match self.child(cur, seg) {
                Some(i) if self.inodes[i as usize].is_dir => i,
                _ => {
                    let n = self.alloc(Inode::dir(cur));
                    self.inodes[cur as usize].children.push(((*seg).into(), n));
                    n
                }
            };
        }
        let leaf = parts[parts.len() - 1];
        match self.child(cur, leaf) {
            Some(i) if !self.inodes[i as usize].is_dir => self.inodes[i as usize].data = data,
            Some(_) => {} // a directory of that name exists; leave it
            None => {
                let n = self.alloc(Inode::file(cur));
                self.inodes[n as usize].data = data;
                self.inodes[cur as usize].children.push((leaf.into(), n));
            }
        }
    }

    pub fn mkdir_p(&mut self, path: &str) {
        let mut cur = 0u32;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = match self.child(cur, seg) {
                Some(i) if self.inodes[i as usize].is_dir => i,
                _ => {
                    let n = self.alloc(Inode::dir(cur));
                    self.inodes[cur as usize].children.push((seg.into(), n));
                    n
                }
            };
        }
    }

    /// Every file in the tree as (path, bytes), for flushing to OPFS.
    pub fn list_files(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        self.walk_files(0, String::new(), &mut out);
        out
    }

    fn walk_files(&self, dir: u32, prefix: String, out: &mut Vec<(String, Vec<u8>)>) {
        for (name, idx) in &self.inodes[dir as usize].children {
            let node = &self.inodes[*idx as usize];
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                alloc::format!("{prefix}/{name}")
            };
            if node.is_dir {
                self.walk_files(*idx, path, out);
            } else {
                out.push((path, node.data.clone()));
            }
        }
    }

    /// A monotonically increasing counter of mutations. The host polls it and
    /// flushes to OPFS when it moves.
    pub fn dirty_counter(&self) -> u64 {
        self.dirty
    }
}

/// A request the device parked waiting on the host, with everything needed to
/// finish it once the bytes arrive: the chain it came in on (`head` +
/// `writable` buffers, cloned since the borrowed `Chain` is gone by then) and
/// the original T-message, which is simply re-run against the now-filled tree.
struct Held {
    id: u32,
    head: u16,
    writable: Vec<Buf>,
    msg: Vec<u8>,
    /// 0 = read file bytes into `target`; 1 = list directory `target`.
    kind: u8,
    target: u32,
}

/// The virtio device wrapping a `P9Fs`. The mount tag is device config space.
pub struct Virtio9p {
    fs: P9Fs,
    tag: Vec<u8>,
    /// Lazy mode: serve the tree on demand from the host (OPFS/Dropbox) rather
    /// than from a seeded snapshot. Off by default so the seeded share and the
    /// native tests behave exactly as before.
    lazy: bool,
    /// Set by a handler that cannot answer yet: (kind, path, off, len, target).
    /// `dispatch` sees it and returns no reply; `handle`/`supply` turn it into a
    /// host request and a parked chain.
    defer_pending: Option<(u8, String, u64, u32, u32)>,
    /// Chains parked on the host. At most a handful outstanding (the guest
    /// blocks on each 9p RPC), but the client may pipeline a few tags.
    held: Vec<Held>,
    /// Requests awaiting the host, drained by `take_host_reqs`.
    host_reqs: Vec<HostReq>,
    /// Whether the most recent `handle` deferred; read+cleared by `deferred_this`.
    deferred: bool,
    next_id: u32,
    /// Lazy write-back: guest mutations to propagate to the host (OPFS/Dropbox),
    /// keyed by path so repeated writes to one file collapse to a single entry.
    /// Value is the op — 0 = written/created (bytes taken at drain), 1 = deleted,
    /// 2 = directory created. Drained by `take_changes`.
    dirty_changes: BTreeMap<String, u8>,
}

impl Virtio9p {
    pub fn new(tag: &str) -> Self {
        Virtio9p {
            fs: P9Fs::new(),
            tag: tag.as_bytes().to_vec(),
            lazy: false,
            defer_pending: None,
            held: Vec::new(),
            host_reqs: Vec::new(),
            deferred: false,
            next_id: 1,
            dirty_changes: BTreeMap::new(),
        }
    }

    /// Record a guest mutation for write-back (lazy mode only). op: 0 = write/
    /// create, 1 = delete, 2 = mkdir. A delete supersedes an earlier write to
    /// the same path, and vice-versa — last op wins, which is what the host
    /// should replay.
    fn mark(&mut self, path: String, op: u8) {
        if self.lazy && !path.is_empty() {
            self.dirty_changes.insert(path, op);
        }
    }

    /// A lazy device rooted at an unlisted directory: nothing is seeded, and the
    /// tree is faulted in from the host on first access.
    pub fn new_lazy(tag: &str) -> Self {
        let mut d = Self::new(tag);
        d.lazy = true;
        d.fs.inodes[0].loaded = false;
        d
    }

    pub fn fs_mut(&mut self) -> &mut P9Fs {
        &mut self.fs
    }
    pub fn fs(&self) -> &P9Fs {
        &self.fs
    }

    /// Test-only access to the raw T->R path, without the virtqueue.
    #[cfg(any(test, feature = "std"))]
    pub fn dispatch_for_test(&mut self, msg: &[u8]) -> Vec<u8> {
        self.dispatch(msg)
    }
}

/// Little-endian cursor over a received T-message body.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, p: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.b.get(self.p..self.p + 2)?;
        self.p += 2;
        Some(u16::from_le_bytes(s.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        let s = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(String::from_utf8_lossy(s).into_owned())
    }
}

/// Builder for an R-message body (everything after the 7-byte header).
struct W {
    b: Vec<u8>,
}

impl W {
    fn new() -> Self {
        W { b: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.b.extend_from_slice(s.as_bytes());
    }
    fn qid(&mut self, ty: u8, path: u64) {
        self.u8(ty);
        self.u32(0); // version
        self.u64(path);
    }
}

impl Virtio9p {
    /// Park the current request on the host and return the `DEFER` sentinel.
    /// `kind` 0 reads file bytes into `target`; `kind` 1 lists directory
    /// `target`. The errno is never framed (see `dispatch`).
    fn defer(&mut self, kind: u8, target: u32, off: u64, len: u32) -> Result<(u8, W), u32> {
        let path = self.fs.path_of(target);
        self.defer_pending = Some((kind, path, off, len, target));
        Err(DEFER)
    }

    /// Parse one T-message and produce the full R-message (with header).
    fn dispatch(&mut self, msg: &[u8]) -> Vec<u8> {
        if msg.len() < 7 {
            return Vec::new();
        }
        let ty = msg[4];
        let tag = u16::from_le_bytes(msg[5..7].try_into().unwrap());
        let mut c = Cur::new(&msg[7..]);

        // Each arm returns Ok((rtype, body)) or Err(errno).
        let result: Result<(u8, W), u32> = self.handle_msg(ty, &mut c);

        // A handler that parked the request on the host produces no reply now;
        // `handle`/`supply` inspect `defer_pending` and take it from here.
        if self.defer_pending.is_some() {
            return Vec::new();
        }

        let (rtype, body) = match result {
            Ok(pair) => pair,
            Err(errno) => {
                let mut w = W::new();
                w.u32(errno);
                (RLERROR, w)
            }
        };
        frame(rtype, tag, &body.b)
    }

    fn handle_msg(&mut self, ty: u8, c: &mut Cur) -> Result<(u8, W), u32> {
        match ty {
            TVERSION => {
                let msize = c.u32().ok_or(EINVAL)?;
                let version = c.str().ok_or(EINVAL)?;
                self.fs.msize = msize.min(MSIZE_MAX);
                // Clear any prior session state; a fresh Tversion resets it.
                self.fs.fids.clear();
                let mut w = W::new();
                w.u32(self.fs.msize);
                // Only .L is offered. An unknown proposal gets "unknown", which
                // makes the client fall back or fail cleanly.
                if version == "9P2000.L" {
                    w.str("9P2000.L");
                } else {
                    w.str("unknown");
                }
                Ok((TVERSION + 1, w))
            }
            TATTACH => {
                let fid = c.u32().ok_or(EINVAL)?;
                let _afid = c.u32();
                let _uname = c.str();
                let _aname = c.str();
                // .L carries n_uname[4] after aname.
                let _n_uname = c.u32();
                self.fs.fids.insert(fid, Fid { inode: 0 });
                let mut w = W::new();
                w.qid(self.fs.inodes[0].qid_type(), 0);
                Ok((TATTACH + 1, w))
            }
            TWALK => {
                let fid = c.u32().ok_or(EINVAL)?;
                let newfid = c.u32().ok_or(EINVAL)?;
                let nwname = c.u16().ok_or(EINVAL)? as usize;
                let start = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                let mut cur = start;
                let mut qids: Vec<(u8, u64)> = Vec::new();
                for _ in 0..nwname {
                    let name = c.str().ok_or(EINVAL)?;
                    let next = if name == ".." {
                        self.fs.inodes[cur as usize].parent
                    } else if name == "." {
                        cur
                    } else {
                        // Resolving a name needs this directory's entries. In
                        // lazy mode, fault them in and re-run the whole walk.
                        if self.lazy
                            && self.fs.inodes[cur as usize].is_dir
                            && !self.fs.inodes[cur as usize].loaded
                        {
                            return self.defer(1, cur, 0, 0);
                        }
                        match self.fs.child(cur, &name) {
                            Some(i) => i,
                            None => break, // partial walk: return what we have
                        }
                    };
                    cur = next;
                    qids.push((self.fs.inodes[cur as usize].qid_type(), cur as u64));
                }
                // A walk that matched no component still binds newfid (to the
                // start), which is how the client clones a fid.
                if qids.len() == nwname {
                    self.fs.fids.insert(newfid, Fid { inode: cur });
                } else if qids.is_empty() {
                    return Err(ENOENT);
                }
                let mut w = W::new();
                w.u16(qids.len() as u16);
                for (t, p) in qids {
                    w.qid(t, p);
                }
                Ok((TWALK + 1, w))
            }
            TGETATTR => {
                let fid = c.u32().ok_or(EINVAL)?;
                let _mask = c.u64();
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                let node = &self.fs.inodes[ino as usize];
                // An unread stub knows its size from the parent listing; a
                // loaded file (and any directory) reports what it actually holds.
                let size = if node.loaded || node.is_dir { node.data.len() as u64 } else { node.size };
                let mut w = W::new();
                w.u64(0x0000_07ff); // P9_STATS_BASIC: the fields below
                w.qid(node.qid_type(), ino as u64);
                w.u32(node.mode());
                w.u32(0); // uid
                w.u32(0); // gid
                w.u64(if node.is_dir { 2 } else { 1 }); // nlink
                w.u64(0); // rdev
                w.u64(size);
                w.u64(4096); // blksize
                w.u64(size.div_ceil(512)); // blocks
                for _ in 0..3 {
                    // atime, mtime, ctime (sec, nsec each)
                    w.u64(node.mtime);
                    w.u64(0);
                }
                w.u64(0); // btime_sec  (not in BASIC, but the reply is fixed-size)
                w.u64(0); // btime_nsec
                w.u64(0); // gen
                w.u64(0); // data_version
                Ok((TGETATTR + 1, w))
            }
            TLOPEN => {
                let fid = c.u32().ok_or(EINVAL)?;
                let _flags = c.u32();
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                let node = &self.fs.inodes[ino as usize];
                let mut w = W::new();
                w.qid(node.qid_type(), ino as u64);
                w.u32(0); // iounit: 0 = use msize
                Ok((TLOPEN + 1, w))
            }
            TLCREATE => {
                let dfid = c.u32().ok_or(EINVAL)?;
                let name = c.str().ok_or(EINVAL)?;
                let _flags = c.u32();
                let mode = c.u32().ok_or(EINVAL)?;
                let _gid = c.u32();
                let dir = self.fs.fids.get(&dfid).ok_or(EBADF)?.inode;
                if !self.fs.inodes[dir as usize].is_dir {
                    return Err(ENOTDIR);
                }
                if self.fs.child(dir, &name).is_some() {
                    return Err(EEXIST);
                }
                let mut node = Inode::file(dir);
                node.perm = mode & 0o777;
                let n = self.fs.alloc(node);
                self.fs.inodes[dir as usize].children.push((name, n));
                // lcreate leaves newfid (== dfid) pointing at the new file.
                self.fs.fids.insert(dfid, Fid { inode: n });
                self.fs.dirty += 1;
                let p = self.fs.path_of(n);
                self.mark(p, 0);
                let mut w = W::new();
                w.qid(QTFILE, n as u64);
                w.u32(0);
                Ok((TLCREATE + 1, w))
            }
            TMKDIR => {
                let dfid = c.u32().ok_or(EINVAL)?;
                let name = c.str().ok_or(EINVAL)?;
                let _mode = c.u32();
                let _gid = c.u32();
                let dir = self.fs.fids.get(&dfid).ok_or(EBADF)?.inode;
                if !self.fs.inodes[dir as usize].is_dir {
                    return Err(ENOTDIR);
                }
                if self.fs.child(dir, &name).is_some() {
                    return Err(EEXIST);
                }
                let n = self.fs.alloc(Inode::dir(dir));
                self.fs.inodes[dir as usize].children.push((name, n));
                self.fs.dirty += 1;
                let p = self.fs.path_of(n);
                self.mark(p, 2);
                let mut w = W::new();
                w.qid(QTDIR, n as u64);
                Ok((TMKDIR + 1, w))
            }
            TREAD => {
                let fid = c.u32().ok_or(EINVAL)?;
                let offset = c.u64().ok_or(EINVAL)? as usize;
                let count = c.u32().ok_or(EINVAL)? as usize;
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                if self.fs.inodes[ino as usize].is_dir {
                    return Err(EISDIR);
                }
                // First read of a stub: fetch the whole file from the host, then
                // this same read is re-run against the now-loaded bytes.
                if self.lazy && !self.fs.inodes[ino as usize].loaded {
                    return self.defer(0, ino, offset as u64, count as u32);
                }
                let node = &self.fs.inodes[ino as usize];
                let end = offset.saturating_add(count).min(node.data.len());
                let slice = if offset < node.data.len() { &node.data[offset..end] } else { &[] };
                let mut w = W::new();
                w.u32(slice.len() as u32);
                w.b.extend_from_slice(slice);
                Ok((TREAD + 1, w))
            }
            TWRITE => {
                let fid = c.u32().ok_or(EINVAL)?;
                let offset = c.u64().ok_or(EINVAL)? as usize;
                let count = c.u32().ok_or(EINVAL)? as usize;
                let data = c.b.get(c.p..c.p + count).ok_or(EINVAL)?;
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                let node = &mut self.fs.inodes[ino as usize];
                if node.is_dir {
                    return Err(EISDIR);
                }
                let end = offset + count;
                if end > node.data.len() {
                    node.data.resize(end, 0);
                }
                node.data[offset..end].copy_from_slice(data);
                // Writes land in the in-memory overlay; the file is now backed
                // by that, not by any lazy stub.
                node.loaded = true;
                node.size = node.data.len() as u64;
                self.fs.dirty += 1;
                let p = self.fs.path_of(ino);
                self.mark(p, 0);
                let mut w = W::new();
                w.u32(count as u32);
                Ok((TWRITE + 1, w))
            }
            TREADDIR => {
                let fid = c.u32().ok_or(EINVAL)?;
                let offset = c.u64().ok_or(EINVAL)?;
                let count = c.u32().ok_or(EINVAL)? as usize;
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                if !self.fs.inodes[ino as usize].is_dir {
                    return Err(ENOTDIR);
                }
                // An unlisted directory faults its entries in first, then this
                // readdir is re-run against the populated children.
                if self.lazy && !self.fs.inodes[ino as usize].loaded {
                    return self.defer(1, ino, 0, 0);
                }
                // Synthesise "." and ".." ahead of the real entries, and use a
                // 1-based running index as the offset cookie so the client can
                // resume. entry i is emitted when its index > offset.
                let mut entries: Vec<(String, u32, u8)> = Vec::new();
                entries.push((".".into(), ino, DT_DIR));
                let parent = self.fs.inodes[ino as usize].parent;
                entries.push(("..".into(), parent, DT_DIR));
                for (name, idx) in &self.fs.inodes[ino as usize].children {
                    let t = if self.fs.inodes[*idx as usize].is_dir { DT_DIR } else { DT_REG };
                    entries.push((name.clone(), *idx, t));
                }

                let mut data: Vec<u8> = Vec::new();
                for (i, (name, idx, dt)) in entries.iter().enumerate() {
                    let cookie = (i + 1) as u64;
                    if cookie <= offset {
                        continue;
                    }
                    // dirent: qid[13] offset[8] type[1] name[s]
                    let entry_len = 13 + 8 + 1 + 2 + name.len();
                    if data.len() + entry_len > count {
                        break;
                    }
                    let qt = self.fs.inodes[*idx as usize].qid_type();
                    data.extend_from_slice(&[qt]);
                    data.extend_from_slice(&0u32.to_le_bytes());
                    data.extend_from_slice(&(*idx as u64).to_le_bytes());
                    data.extend_from_slice(&cookie.to_le_bytes());
                    data.push(*dt);
                    data.extend_from_slice(&(name.len() as u16).to_le_bytes());
                    data.extend_from_slice(name.as_bytes());
                }
                let mut w = W::new();
                w.u32(data.len() as u32);
                w.b.extend_from_slice(&data);
                Ok((TREADDIR + 1, w))
            }
            TSETATTR => {
                let fid = c.u32().ok_or(EINVAL)?;
                let valid = c.u32().ok_or(EINVAL)?;
                let mode = c.u32().ok_or(EINVAL)?;
                let _uid = c.u32();
                let _gid = c.u32();
                let size = c.u64().ok_or(EINVAL)?;
                // atime/mtime sec+nsec follow; ignored.
                let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                let node = &mut self.fs.inodes[ino as usize];
                const ATTR_MODE: u32 = 1 << 0;
                const ATTR_SIZE: u32 = 1 << 3;
                if valid & ATTR_MODE != 0 {
                    node.perm = mode & 0o777;
                }
                if valid & ATTR_SIZE != 0 {
                    node.data.resize(size as usize, 0);
                    node.size = size;
                    self.fs.dirty += 1;
                    let p = self.fs.path_of(ino);
                    self.mark(p, 0);
                }
                Ok((TSETATTR + 1, W::new()))
            }
            TCLUNK => {
                let fid = c.u32().ok_or(EINVAL)?;
                self.fs.fids.remove(&fid);
                Ok((TCLUNK + 1, W::new()))
            }
            TREMOVE | TUNLINKAT => {
                // TREMOVE takes a fid; TUNLINKAT takes (dfid, name, flags). Note
                // the path before unlinking — afterwards the parent link is gone.
                if ty == TREMOVE {
                    let fid = c.u32().ok_or(EINVAL)?;
                    let ino = self.fs.fids.get(&fid).ok_or(EBADF)?.inode;
                    let parent = self.fs.inodes[ino as usize].parent;
                    let path = self.fs.path_of(ino);
                    self.unlink_child(parent, ino)?;
                    self.fs.fids.remove(&fid);
                    self.mark(path, 1);
                } else {
                    let dfid = c.u32().ok_or(EINVAL)?;
                    let name = c.str().ok_or(EINVAL)?;
                    let _flags = c.u32();
                    let dir = self.fs.fids.get(&dfid).ok_or(EBADF)?.inode;
                    let target = self.fs.child(dir, &name).ok_or(ENOENT)?;
                    let path = self.fs.path_of(target);
                    self.unlink_child(dir, target)?;
                    self.mark(path, 1);
                }
                self.fs.dirty += 1;
                Ok((ty + 1, W::new()))
            }
            TSTATFS => {
                let _fid = c.u32().ok_or(EINVAL)?;
                let mut w = W::new();
                w.u32(0x0187_6967); // f_type, arbitrary 9p magic
                w.u32(4096); // bsize
                w.u64(1 << 20); // blocks
                w.u64(1 << 19); // bfree
                w.u64(1 << 19); // bavail
                w.u64(self.fs.inodes.len() as u64); // files
                w.u64(1 << 20); // ffree
                w.u64(0); // fsid
                w.u32(255); // namelen
                Ok((TSTATFS + 1, w))
            }
            TFSYNC => Ok((TFSYNC + 1, W::new())),
            TFLUSH => Ok((TFLUSH + 1, W::new())),
            // Extended-attribute walk: reply with a zero-length attr so the
            // client sees "no xattrs" rather than an error, which is what Linux
            // wants for a filesystem that has none.
            TXATTRWALK => {
                let _fid = c.u32();
                let _newfid = c.u32();
                let _name = c.str();
                let mut w = W::new();
                w.u64(0);
                Ok((TXATTRWALK + 1, w))
            }
            // Not implemented: symlinks, hardlinks, mknod, rename, readlink,
            // legacy Terror. The client gets ENOSYS and copes (e.g. cp without
            // -a just skips ownership it cannot set).
            TSYMLINK | TMKNOD | TREADLINK | TLINK | TRENAMEAT | TLERROR => Err(ENOSYS),
            _ => Err(ENOSYS),
        }
    }

    fn unlink_child(&mut self, dir: u32, target: u32) -> Result<(), u32> {
        if self.fs.inodes[target as usize].is_dir
            && !self.fs.inodes[target as usize].children.is_empty()
        {
            return Err(ENOTEMPTY);
        }
        self.fs.inodes[dir as usize].children.retain(|(_, i)| *i != target);
        Ok(())
    }
}

/// Prepend the 7-byte 9p header (size, type, tag) to a body.
fn frame(ty: u8, tag: u16, body: &[u8]) -> Vec<u8> {
    let size = (7 + body.len()) as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_le_bytes());
    out.push(ty);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(body);
    out
}

impl VirtioDevice for Virtio9p {
    fn device_id(&self) -> u32 {
        9
    }

    /// A snapshot records only the mount tag; the shared tree is host-side and
    /// re-seeded from OPFS on restore, exactly as the disk's bytes are. The
    /// device comes back present (so the guest's boot-time probe still matches)
    /// but empty, and the setup script re-mounts fresh — which is also why the
    /// snapshot is taken before anything is mounted, so no stale fids survive.
    fn dev_state(&self) -> Option<Vec<u8>> {
        Some(self.tag.clone())
    }

    fn p9_put(&mut self, path: &str, data: &[u8]) -> bool {
        self.fs.put_file(path, data.to_vec());
        true
    }
    fn p9_mkdir(&mut self, path: &str) {
        self.fs.mkdir_p(path);
    }
    fn p9_list(&self) -> Vec<(String, Vec<u8>)> {
        self.fs.list_files()
    }
    fn p9_dirty(&self) -> u64 {
        self.fs.dirty_counter()
    }
    fn p9_set_lazy(&mut self) {
        // The restored tree was empty (the snapshot is taken before any mount),
        // so a fresh empty root is equivalent and keeps this simple.
        self.lazy = true;
        self.fs = P9Fs::new();
        self.fs.inodes[0].loaded = false;
        self.held.clear();
        self.host_reqs.clear();
        self.defer_pending = None;
        self.deferred = false;
        self.next_id = 1;
        self.dirty_changes.clear();
    }

    fn p9_take_changes(&mut self) -> Vec<crate::virtio::FileChange> {
        let drained = core::mem::take(&mut self.dirty_changes);
        let mut out = Vec::with_capacity(drained.len());
        for (path, op) in drained {
            let data = if op == 0 {
                // A write/create: grab the current bytes. If the path is gone
                // (created then removed before drain), skip it — the delete
                // entry, if any, carries the truth.
                match self.fs.resolve(&path) {
                    Some(ino) if !self.fs.inodes[ino as usize].is_dir => {
                        self.fs.inodes[ino as usize].data.clone()
                    }
                    _ => continue,
                }
            } else {
                Vec::new()
            };
            out.push(crate::virtio::FileChange { op, path, data });
        }
        out
    }

    fn features(&self) -> u64 {
        // VIRTIO_9P_MOUNT_TAG (bit 0): config space carries a mount tag.
        VIRTIO_F_VERSION_1 | 1
    }

    fn num_queues(&self) -> usize {
        1
    }

    /// Config space: tag_len[2] then the tag bytes.
    fn config_read(&self, off: usize) -> u8 {
        match off {
            0 => self.tag.len() as u8,
            1 => (self.tag.len() >> 8) as u8,
            _ => self.tag.get(off - 2).copied().unwrap_or(0),
        }
    }

    fn handle(&mut self, _queue: usize, chain: &Chain, mem: &mut GuestMem) -> u32 {
        let req_len = sg_len(&chain.readable);
        if req_len < 7 {
            return 0;
        }
        let mut msg = vec![0u8; req_len];
        sg_read(mem, &chain.readable, 0, &mut msg);

        self.defer_pending = None;
        let reply = self.dispatch(&msg);

        // The handler parked this request on the host. Keep the chain (its head
        // and writable buffers) and the message so `supply` can finish it later,
        // and tell the transport not to complete the descriptor now.
        if let Some((kind, path, off, len, target)) = self.defer_pending.take() {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            self.host_reqs.push(HostReq { id, kind, path, off, len });
            self.held.push(Held {
                id,
                head: chain.head,
                writable: chain.writable.clone(),
                msg,
                kind,
                target,
            });
            self.deferred = true;
            return 0;
        }

        // The client sized the writable buffers to msize; never overrun them.
        let cap = sg_len(&chain.writable);
        let n = reply.len().min(cap);
        sg_write(mem, &chain.writable, 0, &reply[..n]);
        n as u32
    }

    fn deferred_this(&mut self) -> bool {
        core::mem::take(&mut self.deferred)
    }

    fn take_host_reqs(&mut self) -> Vec<HostReq> {
        core::mem::take(&mut self.host_reqs)
    }

    fn supply(&mut self, id: u32, payload: &[u8], mem: &mut GuestMem) -> Option<(usize, u16, u32)> {
        let pos = self.held.iter().position(|h| h.id == id)?;
        let held = self.held.remove(pos);

        // Fill in what the host fetched, then re-run the original T-message: it
        // now finds the bytes/entries present and produces a real reply.
        match held.kind {
            0 => {
                let node = &mut self.fs.inodes[held.target as usize];
                node.data = payload.to_vec();
                node.size = node.data.len() as u64;
                node.loaded = true;
            }
            1 => {
                self.fs.apply_listing(held.target, payload);
                self.fs.inodes[held.target as usize].loaded = true;
            }
            _ => {}
        }

        self.defer_pending = None;
        let reply = self.dispatch(&held.msg);

        // Re-running can defer again — a multi-level walk needs the next
        // directory listed. Re-park on the same chain and wait for that fetch.
        if let Some((kind, path, off, len, target)) = self.defer_pending.take() {
            let nid = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            self.host_reqs.push(HostReq { id: nid, kind, path, off, len });
            self.held.push(Held {
                id: nid,
                head: held.head,
                writable: held.writable,
                msg: held.msg,
                kind,
                target,
            });
            return None;
        }

        let cap = sg_len(&held.writable);
        let n = reply.len().min(cap);
        sg_write(mem, &held.writable, 0, &reply[..n]);
        Some((0, held.head, n as u32))
    }
}
