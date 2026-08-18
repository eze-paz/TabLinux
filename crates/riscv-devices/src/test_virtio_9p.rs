//! Drives the 9p server through a realistic message sequence and checks every
//! reply's wire format, before any of it has to survive a real Linux client.
//!
//! A miscompiled reply against the kernel shows up as a mount that hangs or a
//! directory that reads garbage, thousands of instructions from the cause.
//! Here the same bug is one failed assert.

extern crate std;
use crate::virtio_9p::Virtio9p;
use std::vec::Vec;

/// A tiny 9p client: builds T-messages, calls the server's `dispatch` (exposed
/// for tests), and parses R-messages.
struct Client {
    dev: Virtio9p,
    tag: u16,
}

struct RCur<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> RCur<'a> {
    fn u8(&mut self) -> u8 {
        let v = self.b[self.p];
        self.p += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        let v = u16::from_le_bytes(self.b[self.p..self.p + 2].try_into().unwrap());
        self.p += 2;
        v
    }
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }
    fn u64(&mut self) -> u64 {
        let v = u64::from_le_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        v
    }
    fn qid(&mut self) -> (u8, u64) {
        let t = self.u8();
        let _v = self.u32();
        let path = self.u64();
        (t, path)
    }
    fn strn(&mut self) -> std::string::String {
        let n = self.u16() as usize;
        let s = std::string::String::from_utf8_lossy(&self.b[self.p..self.p + n]).into_owned();
        self.p += n;
        s
    }
}

struct WBuf {
    b: Vec<u8>,
}
impl WBuf {
    fn new() -> Self {
        WBuf { b: Vec::new() }
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
    fn strn(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.b.extend_from_slice(s.as_bytes());
    }
    fn raw(&mut self, s: &[u8]) {
        self.b.extend_from_slice(s);
    }
}

impl Client {
    fn new() -> Self {
        Client { dev: Virtio9p::new("shared"), tag: 1 }
    }

    /// Send a T-message body of the given type, return (rtype, body-after-header).
    fn call(&mut self, ty: u8, body: &WBuf) -> (u8, Vec<u8>) {
        let mut msg = Vec::new();
        let size = (7 + body.b.len()) as u32;
        msg.extend_from_slice(&size.to_le_bytes());
        msg.push(ty);
        msg.extend_from_slice(&self.tag.to_le_bytes());
        msg.extend_from_slice(&body.b);
        let reply = self.dev.dispatch_for_test(&msg);
        assert!(reply.len() >= 7, "reply too short for type {ty}");
        let rsize = u32::from_le_bytes(reply[0..4].try_into().unwrap()) as usize;
        assert_eq!(rsize, reply.len(), "R-message size field wrong for T={ty}");
        let rtype = reply[4];
        (rtype, reply[7..].to_vec())
    }
}

const RLERROR: u8 = 7;

#[test]
fn mount_ls_cat_write_readback() {
    let mut cl = Client::new();
    // Seed a file and a subdirectory so the very first ls/cat has something.
    cl.dev.fs_mut().put_file("hello.txt", b"hi from the host\n".to_vec());
    cl.dev.fs_mut().mkdir_p("sub");

    // Tversion
    let mut w = WBuf::new();
    w.u32(16384);
    w.strn("9P2000.L");
    let (rt, body) = cl.call(100, &w);
    assert_eq!(rt, 101, "Rversion");
    let mut c = RCur { b: &body, p: 0 };
    assert!(c.u32() <= 16384);
    assert_eq!(c.strn(), "9P2000.L", "must negotiate 9P2000.L");

    // Tattach fid=0 -> root qid must be a directory
    let mut w = WBuf::new();
    w.u32(0); // fid
    w.u32(0xffff_ffff); // afid = NOFID
    w.strn("root");
    w.strn("shared");
    w.u32(0); // n_uname
    let (rt, body) = cl.call(104, &w);
    assert_eq!(rt, 105, "Rattach");
    let mut c = RCur { b: &body, p: 0 };
    assert_eq!(c.qid().0, 0x80, "root is a directory (QTDIR)");

    // Twalk root(0) -> hello.txt into fid 1
    let mut w = WBuf::new();
    w.u32(0); // fid
    w.u32(1); // newfid
    w.u16(1); // nwname
    w.strn("hello.txt");
    let (rt, body) = cl.call(110, &w);
    assert_eq!(rt, 111, "Rwalk");
    let mut c = RCur { b: &body, p: 0 };
    assert_eq!(c.u16(), 1, "one qid walked");
    assert_eq!(c.qid().0, 0x00, "hello.txt is a file (QTFILE)");

    // Tgetattr fid 1 -> size matches the seeded contents
    let mut w = WBuf::new();
    w.u32(1);
    w.u64(0x0000_07ff);
    let (rt, body) = cl.call(24, &w);
    assert_eq!(rt, 25, "Rgetattr");
    let mut c = RCur { b: &body, p: 0 };
    let _valid = c.u64();
    let _qid = c.qid();
    let mode = c.u32();
    assert_eq!(mode & 0o170000, 0o100000, "regular file bit set");
    let _uid = c.u32();
    let _gid = c.u32();
    let _nlink = c.u64();
    let _rdev = c.u64();
    assert_eq!(c.u64(), b"hi from the host\n".len() as u64, "size in getattr");

    // Tlopen fid 1, then Tread -> exact bytes
    let mut w = WBuf::new();
    w.u32(1);
    w.u32(0); // O_RDONLY
    let (rt, _b) = cl.call(12, &w);
    assert_eq!(rt, 13, "Rlopen");

    let mut w = WBuf::new();
    w.u32(1); // fid
    w.u64(0); // offset
    w.u32(4096); // count
    let (rt, body) = cl.call(116, &w);
    assert_eq!(rt, 117, "Rread");
    let mut c = RCur { b: &body, p: 0 };
    let n = c.u32() as usize;
    assert_eq!(&body[4..4 + n], b"hi from the host\n", "file contents read back");

    // Twalk root -> (clone into fid 2), Tlopen dir, Treaddir: expect ., .., hello.txt, sub
    let mut w = WBuf::new();
    w.u32(0);
    w.u32(2);
    w.u16(0); // nwname = 0: clone the fid
    let (rt, _b) = cl.call(110, &w);
    assert_eq!(rt, 111, "Rwalk clone");
    let mut w = WBuf::new();
    w.u32(2);
    w.u32(0);
    let (rt, _b) = cl.call(12, &w);
    assert_eq!(rt, 13, "Rlopen dir");
    let mut w = WBuf::new();
    w.u32(2);
    w.u64(0);
    w.u32(4096);
    let (rt, body) = cl.call(40, &w);
    assert_eq!(rt, 41, "Rreaddir");
    let mut c = RCur { b: &body, p: 0 };
    let count = c.u32() as usize;
    let mut names = Vec::new();
    let data = &body[4..4 + count];
    let mut dc = RCur { b: data, p: 0 };
    while dc.p < data.len() {
        let _qid = dc.qid();
        let _off = dc.u64();
        let _dt = dc.u8();
        names.push(dc.strn());
    }
    assert!(names.contains(&".".into()), "readdir has .");
    assert!(names.contains(&"..".into()), "readdir has ..");
    assert!(names.contains(&"hello.txt".into()), "readdir has hello.txt");
    assert!(names.contains(&"sub".into()), "readdir has sub");

    // Guest-side create + write, then host reads it back through the tree.
    // Twalk root clone into fid 3, Tlcreate "new.txt"
    let mut w = WBuf::new();
    w.u32(0);
    w.u32(3);
    w.u16(0);
    cl.call(110, &w);
    let mut w = WBuf::new();
    w.u32(3); // dfid (becomes the new file's fid)
    w.strn("new.txt");
    w.u32(0o102); // O_RDWR|O_CREAT-ish flags (ignored)
    w.u32(0o644); // mode
    w.u32(0); // gid
    let (rt, _b) = cl.call(14, &w);
    assert_eq!(rt, 15, "Rlcreate");
    let mut w = WBuf::new();
    w.u32(3); // fid now points at new.txt
    w.u64(0); // offset
    w.u32(11); // count
    w.raw(b"guest wrote"); // data follows count
    let (rt, body) = cl.call(118, &w);
    assert_eq!(rt, 119, "Rwrite");
    let mut c = RCur { b: &body, p: 0 };
    assert_eq!(c.u32(), 11, "wrote 11 bytes");

    // The host now sees the file the guest created.
    let files = cl.dev.fs().list_files();
    let found = files.iter().find(|(p, _)| p == "new.txt");
    assert!(found.is_some(), "host sees guest-created file");
    assert_eq!(found.unwrap().1, b"guest wrote", "host reads guest's bytes");

    // A walk to a missing name returns Rlerror(ENOENT), not a malformed reply.
    let mut w = WBuf::new();
    w.u32(0);
    w.u32(9);
    w.u16(1);
    w.strn("does-not-exist");
    let (rt, body) = cl.call(110, &w);
    assert_eq!(rt, RLERROR, "missing path -> Rlerror");
    let mut c = RCur { b: &body, p: 0 };
    assert_eq!(c.u32(), 2, "ENOENT");
}
