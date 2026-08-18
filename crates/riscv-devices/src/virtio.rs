//! VirtIO MMIO transport (spec v1.2, "modern"/version 2) and split virtqueues.
//!
//! The QEMU virt devicetree we clone declares eight virtio-mmio slots at
//! `0x10001000 + slot * 0x1000` with PLIC hwirq `slot + 1`; `DeviceBus` maps
//! each slot to at most one [`VirtioMmio`].
//!
//! Everything here is synchronous: a queue notification is serviced inline and
//! the used ring is updated before the guest's MMIO store retires. That is
//! deliberate — it keeps the device model usable from a wasm build backed by
//! OPFS sync access handles, which are also synchronous. Virtio's asynchronous
//! contract means a backend that *cannot* answer immediately (an HTTP range
//! fetch, say) can be added later by deferring the used-ring push without the
//! guest noticing anything beyond a slower disk.

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub const VIRTIO_MMIO_BASE: u64 = 0x1000_1000;
pub const VIRTIO_MMIO_STRIDE: u64 = 0x1000;
pub const VIRTIO_MMIO_SLOTS: usize = 8;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const VENDOR: u32 = 0x554d_4551; // "QEMU"

/// Maximum descriptors per queue we advertise.
const QUEUE_NUM_MAX: u32 = 256;

/// Bit 32. Mandatory for a version-2 transport; also selects little-endian
/// layout for all rings, which is what this code assumes throughout.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;

/// A window onto guest DRAM, for chasing descriptor addresses (which are guest
/// physical) during DMA. There is no IOMMU, so this is a plain bounds-checked
/// slice.
pub struct GuestMem<'a> {
    pub dram: &'a mut [u8],
    pub base: u64,
}

impl GuestMem<'_> {
    fn window(&self, gpa: u64, len: usize) -> Option<core::ops::Range<usize>> {
        let off = gpa.checked_sub(self.base)? as usize;
        let end = off.checked_add(len)?;
        if end <= self.dram.len() {
            Some(off..end)
        } else {
            None
        }
    }

    pub fn read(&self, gpa: u64, out: &mut [u8]) {
        match self.window(gpa, out.len()) {
            Some(r) => out.copy_from_slice(&self.dram[r]),
            None => out.fill(0),
        }
    }

    pub fn write(&mut self, gpa: u64, src: &[u8]) {
        if let Some(r) = self.window(gpa, src.len()) {
            self.dram[r].copy_from_slice(src);
        }
    }

    pub fn read_u16(&self, gpa: u64) -> u16 {
        let mut b = [0u8; 2];
        self.read(gpa, &mut b);
        u16::from_le_bytes(b)
    }

    pub fn read_u32(&self, gpa: u64) -> u32 {
        let mut b = [0u8; 4];
        self.read(gpa, &mut b);
        u32::from_le_bytes(b)
    }

    pub fn read_u64(&self, gpa: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(gpa, &mut b);
        u64::from_le_bytes(b)
    }

    pub fn write_u16(&mut self, gpa: u64, v: u16) {
        self.write(gpa, &v.to_le_bytes());
    }

    pub fn write_u32(&mut self, gpa: u64, v: u32) {
        self.write(gpa, &v.to_le_bytes());
    }
}

/// One descriptor's guest buffer.
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
}

/// A walked descriptor chain, split by direction. `readable` is what the driver
/// wrote for us; `writable` is where our reply goes.
#[derive(Default, Debug)]
pub struct Chain {
    pub head: u16,
    pub readable: Vec<Buf>,
    pub writable: Vec<Buf>,
}

/// Gather `out.len()` bytes from a scatter list starting `skip` bytes in.
/// Returns how many bytes were actually available.
pub fn sg_read(mem: &GuestMem, bufs: &[Buf], mut skip: usize, out: &mut [u8]) -> usize {
    let mut done = 0usize;
    for b in bufs {
        let blen = b.len as usize;
        if skip >= blen {
            skip -= blen;
            continue;
        }
        let avail = blen - skip;
        let take = avail.min(out.len() - done);
        mem.read(b.addr + skip as u64, &mut out[done..done + take]);
        done += take;
        skip = 0;
        if done == out.len() {
            break;
        }
    }
    done
}

/// Scatter `src` into a buffer list starting `skip` bytes in. Returns bytes written.
pub fn sg_write(mem: &mut GuestMem, bufs: &[Buf], mut skip: usize, src: &[u8]) -> usize {
    let mut done = 0usize;
    for b in bufs {
        let blen = b.len as usize;
        if skip >= blen {
            skip -= blen;
            continue;
        }
        let avail = blen - skip;
        let take = avail.min(src.len() - done);
        let addr = b.addr + skip as u64;
        mem.write(addr, &src[done..done + take]);
        done += take;
        skip = 0;
        if done == src.len() {
            break;
        }
    }
    done
}

pub fn sg_len(bufs: &[Buf]) -> usize {
    bufs.iter().map(|b| b.len as usize).sum()
}

/// A file operation a device needs the host (JS) to perform asynchronously,
/// then feed back via `supply`. Kinds: 0 = read file bytes, 1 = list directory.
pub struct HostReq {
    pub id: u32,
    pub kind: u8,
    pub path: alloc::string::String,
    pub off: u64,
    pub len: u32,
}

/// A guest mutation for the host to replay onto its backing store (OPFS, then
/// Dropbox). `op`: 0 = write/create `path` with `data`; 1 = delete `path`;
/// 2 = create directory `path` (no data).
pub struct FileChange {
    pub op: u8,
    pub path: alloc::string::String,
    pub data: alloc::vec::Vec<u8>,
}

/// Device-specific behaviour behind the shared MMIO transport.
pub trait VirtioDevice {
    fn device_id(&self) -> u32;
    fn features(&self) -> u64;
    fn num_queues(&self) -> usize;
    /// Device configuration space at MMIO offset 0x100, byte addressed.
    fn config_read(&self, off: usize) -> u8;
    fn config_write(&mut self, _off: usize, _val: u8) {}
    /// Service one available chain. Returns the number of bytes written into
    /// the chain's device-writable buffers (the used ring's `len`).
    ///
    /// A device may instead take the chain and defer it — it has no reply yet
    /// because it is waiting on the host (an async OPFS read for 9p). It signals
    /// that by returning 0 here and `true` from `deferred_this()` on the same
    /// call; the transport then leaves the descriptor uncompleted, and the
    /// device raises the completion later through `supply`, exactly as a real
    /// controller defers a slow I/O and raises the IRQ once the data lands.
    fn handle(&mut self, queue: usize, chain: &Chain, mem: &mut GuestMem) -> u32;

    /// Did the most recent `handle` defer rather than complete? Polled by the
    /// transport right after `handle`. A flag rather than an `Option` return so
    /// the other devices' `handle` signatures stay untouched.
    fn deferred_this(&mut self) -> bool {
        false
    }

    /// Host requests a deferred device raised (9p reads/listings it needs
    /// fetched). Drained by the bus and handed to JS; empty for other devices.
    fn take_host_reqs(&mut self) -> alloc::vec::Vec<HostReq> {
        alloc::vec::Vec::new()
    }

    /// The host supplies data for a request `take_host_reqs` handed out. The
    /// device fills it in and produces the deferred reply, returning
    /// `(queue, head, len)` so the transport can complete the held chain.
    fn supply(&mut self, _id: u32, _payload: &[u8], _mem: &mut GuestMem) -> Option<(usize, u16, u32)> {
        None
    }
    /// Called once per bus tick so a device can push unsolicited data (an
    /// arriving network frame, say). Returns true if it made a queue used.
    fn poll(&mut self, _mem: &mut GuestMem) -> bool {
        false
    }

    /// Snapshot the device-specific state, or None if this device kind does
    /// not support snapshots (a snapshot containing it must fail, not lie).
    fn dev_state(&self) -> Option<alloc::vec::Vec<u8>> {
        None
    }
    /// Restore what `dev_state` produced.
    fn load_dev_state(&mut self, _bytes: &[u8]) -> Option<()> {
        Some(())
    }

    /// Give a restored device its backing store back. Only virtio-blk
    /// implements this: a snapshot records the disk's size but never its
    /// contents, so the host must re-supply the store before the guest reads.
    fn attach_backend(&mut self, _b: alloc::boxed::Box<dyn crate::virtio_blk::BlockBackend>) -> bool {
        false
    }

    /// Which queue the device pushes unsolicited data onto, if any. A network
    /// device names its receive queue here; storage returns None.
    fn rx_queue(&self) -> Option<usize> {
        None
    }

    /// Is there inbound data waiting for a guest buffer? Takes &mut because
    /// answering may mean pulling from a backend.
    fn has_rx(&mut self) -> bool {
        false
    }

    /// Fill one guest-provided receive buffer. Returns bytes written.
    fn fill_rx(&mut self, _chain: &Chain, _mem: &mut GuestMem) -> u32 {
        0
    }

    // ── virtio-9p shared-folder access, for the host to seed and flush ──────
    // Default no-ops; only Virtio9p implements them. The host reaches the tree
    // through the transport it already holds, the same way attach_backend hands
    // a disk its bytes.

    /// Create or overwrite a file in the share. Returns false if this device is
    /// not a 9p device.
    fn p9_put(&mut self, _path: &str, _data: &[u8]) -> bool {
        false
    }
    /// Make a directory (and parents) in the share.
    fn p9_mkdir(&mut self, _path: &str) {}
    /// Every file in the share as (path, bytes), for flushing to OPFS.
    fn p9_list(&self) -> alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<u8>)> {
        alloc::vec::Vec::new()
    }
    /// A counter that moves on every mutation, so the host knows when to flush.
    fn p9_dirty(&self) -> u64 {
        0
    }
    /// Convert this 9p device to lazy on-demand mode, resetting to an empty
    /// unlisted root. Used on the restore path, where the device comes back from
    /// a snapshot in the seeded (non-lazy) mode. No-op for other devices.
    fn p9_set_lazy(&mut self) {}

    /// Drain the guest mutations a lazy 9p device recorded, for the host to
    /// replay onto OPFS/Dropbox. Empty for other devices.
    fn p9_take_changes(&mut self) -> alloc::vec::Vec<FileChange> {
        alloc::vec::Vec::new()
    }
}

/// A serviced chain waiting to be reported in the used ring.
///
/// The DMA has already happened; only the completion is held back. Real
/// hardware cannot finish a request before the kick instruction retires, and
/// pretending otherwise means the driver's completion handler runs nested
/// inside its own submit path — which breaks any code that submits several
/// requests before waiting on the first (`ext4_bread_batch` being the one that
/// found this). It is also the shape the async backends need: an HTTP range
/// fetch will simply push its completion many ticks later.
#[derive(Clone, Copy)]
struct Completion {
    queue: usize,
    head: u16,
    len: u32,
    due: u64,
}

/// Ticks between a kick and its completion. One tick is one retired
/// instruction, so this is a plausible "fast NVMe" latency and is long enough
/// that the driver always returns from its submit path first.
const COMPLETION_LATENCY: u64 = 2000;

#[derive(Clone, Copy, Default)]
struct QueueState {
    num: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    /// Index into the avail ring we have consumed up to. Free-running, wraps
    /// with the driver's `avail.idx`.
    last_avail: u16,
}

const MAX_QUEUES: usize = 4;

/// The virtio-mmio register file plus queue plumbing for one device.
pub struct VirtioMmio {
    pub dev: Box<dyn VirtioDevice>,
    /// PLIC hwirq for this slot.
    pub irq: usize,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    status: u32,
    queue_sel: usize,
    queues: [QueueState; MAX_QUEUES],
    interrupt_status: u32,
    config_generation: u32,
    /// Serviced-but-not-yet-reported chains, oldest first.
    pending: Vec<Completion>,
    now: u64,
}

impl VirtioMmio {
    pub fn new(dev: Box<dyn VirtioDevice>, irq: usize) -> Self {
        Self {
            dev,
            irq,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queues: [QueueState::default(); MAX_QUEUES],
            interrupt_status: 0,
            config_generation: 0,
            pending: Vec::new(),
            now: 0,
        }
    }

    /// Retire every pending completion right now, regardless of its remaining
    /// latency. Returns true if any fired.
    ///
    /// For the idle path: when the guest goes to WFI it is waiting for exactly
    /// these completions, and the run loop is about to fast-forward the timer
    /// clock past them. But completions retire on the *device* clock (`now`),
    /// which only advances when `tick` runs — and it does not run during an idle
    /// skip. Under the JIT that clock also advances only once per chain, so the
    /// gap between issuing a block read and going idle is easily shorter than
    /// the latency. Left alone, the completion's interrupt never fires and the
    /// guest sleeps forever. The latency is a fidelity nicety; delivering an
    /// in-flight completion the instant the guest can no longer make progress
    /// without it is the correct thing to do.
    pub fn flush_pending(&mut self, mem: &mut GuestMem) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        for c in core::mem::take(&mut self.pending) {
            if let Some(q) = self.queues.get(c.queue).copied() {
                Self::push_used(mem, &q, c.head, c.len);
            }
        }
        self.interrupt_status |= 1;
        true
    }

    /// Advance the device clock by `amount` and retire any completions whose
    /// latency has elapsed. Returns true if the interrupt line may have changed.
    ///
    /// `amount` is retired guest instructions since the last call. It matters
    /// under the JIT: there `tick_n` runs once per compiled chain rather than
    /// once per instruction, so a fixed `+1` would make the device clock crawl
    /// — a completion latency of 2000 would take 2000 chains instead of 2000
    /// instructions, long enough that a driver polling for its completion spins
    /// for whole seconds of wall time. Advancing by the instruction count keeps
    /// completions on the same timescale the interpreter sees.
    pub fn tick(&mut self, amount: u64, mem: &mut GuestMem) -> bool {
        self.now = self.now.wrapping_add(amount);
        if self.pending.is_empty() {
            return false;
        }
        let now = self.now;
        let mut fired = false;
        // Completions are appended in order, so everything due is at the front.
        while let Some(c) = self.pending.first().copied() {
            if c.due > now {
                break;
            }
            self.pending.remove(0);
            if let Some(q) = self.queues.get(c.queue).copied() {
                Self::push_used(mem, &q, c.head, c.len);
            }
            fired = true;
        }
        if fired {
            self.interrupt_status |= 1;
        }
        fired
    }

    /// Is this device's interrupt line asserted? The PLIC models level-triggered
    /// sources, and virtio's InterruptStatus/InterruptACK pair is exactly that.
    pub fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Are there completions still waiting out their latency?
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drain the file requests a deferred device raised, for the host to fulfil.
    pub fn take_host_reqs(&mut self) -> alloc::vec::Vec<HostReq> {
        self.dev.take_host_reqs()
    }

    /// Feed a host reply back to the device and complete the chain it was
    /// holding. Completed immediately (`due = now`): the guest has been blocked
    /// on this since the request went out, so there is no latency to simulate.
    pub fn supply(&mut self, id: u32, payload: &[u8], mem: &mut GuestMem) -> bool {
        if let Some((queue, head, len)) = self.dev.supply(id, payload, mem) {
            self.pending.push(Completion { queue, head, len, due: self.now });
            true
        } else {
            false
        }
    }

    pub fn read(&self, off: usize) -> u32 {
        if off >= 0x100 {
            // Config space reads come in at various widths; the caller has
            // already narrowed to a 4-byte aligned word.
            let b = off - 0x100;
            return u32::from_le_bytes([
                self.dev.config_read(b),
                self.dev.config_read(b + 1),
                self.dev.config_read(b + 2),
                self.dev.config_read(b + 3),
            ]);
        }
        let q = self.queues.get(self.queue_sel).copied().unwrap_or_default();
        match off {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => self.dev.device_id(),
            0x00c => VENDOR,
            0x010 => {
                let f = self.dev.features();
                if self.device_features_sel == 0 {
                    f as u32
                } else {
                    (f >> 32) as u32
                }
            }
            0x034 => QUEUE_NUM_MAX,
            0x044 => q.ready as u32,
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => self.config_generation,
            _ => 0,
        }
    }

    /// Handle an MMIO store. Returns true if the device's interrupt line may
    /// have changed and the caller should refresh the PLIC.
    pub fn write(&mut self, off: usize, val: u32, mem: &mut GuestMem) -> bool {
        if off >= 0x100 {
            self.dev.config_write(off - 0x100, val as u8);
            return false;
        }
        match off {
            0x014 => self.device_features_sel = val,
            0x020 => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & !0xFFFF_FFFF) | val as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            0x024 => self.driver_features_sel = val,
            0x030 => self.queue_sel = val as usize,
            0x038 => self.with_queue(|q| q.num = val as u16),
            0x044 => {
                let ready = val & 1 != 0;
                self.with_queue(|q| {
                    q.ready = ready;
                    if !ready {
                        q.last_avail = 0;
                    }
                });
            }
            0x050 => return self.notify(val as usize, mem),
            0x064 => {
                self.interrupt_status &= !val;
                return true;
            }
            0x070 => {
                self.status = val;
                if val == 0 {
                    // Driver reset: drop all queue state.
                    self.queues = [QueueState::default(); MAX_QUEUES];
                    self.interrupt_status = 0;
                    self.driver_features = 0;
                    return true;
                }
            }
            0x080 => self.with_queue(|q| q.desc = (q.desc & !0xFFFF_FFFF) | val as u64),
            0x084 => self.with_queue(|q| q.desc = (q.desc & 0xFFFF_FFFF) | ((val as u64) << 32)),
            0x090 => self.with_queue(|q| q.avail = (q.avail & !0xFFFF_FFFF) | val as u64),
            0x094 => self.with_queue(|q| q.avail = (q.avail & 0xFFFF_FFFF) | ((val as u64) << 32)),
            0x0a0 => self.with_queue(|q| q.used = (q.used & !0xFFFF_FFFF) | val as u64),
            0x0a4 => self.with_queue(|q| q.used = (q.used & 0xFFFF_FFFF) | ((val as u64) << 32)),
            _ => {}
        }
        false
    }

    fn with_queue(&mut self, f: impl FnOnce(&mut QueueState)) {
        if let Some(q) = self.queues.get_mut(self.queue_sel) {
            f(q);
        }
    }

    /// Drain every chain the driver has made available on `queue`.
    fn notify(&mut self, queue: usize, mem: &mut GuestMem) -> bool {
        if queue >= MAX_QUEUES || queue >= self.dev.num_queues() {
            return false;
        }
        // Buffers on the device's RX queue are the driver *offering space*,
        // not submitting work, so they must not be executed and retired here.
        // Doing that returns each one as a zero-length receive; the driver
        // drops the runt, refills the ring, kicks, and the guest disappears
        // into an infinite refill storm at 100% CPU — `ip link set eth0 up`
        // alone burned billions of instructions inside virtio_net module code.
        // It also starved the real receive path: `poll` could only deliver a
        // frame by winning a race against the next kick for the few buffers
        // posted between add and notify, which is why ping replies took
        // seconds to arrive when they arrived at all.
        //
        // The only correct response to an RX kick is "space just appeared":
        // deliver a pending inbound frame if one is waiting.
        if self.dev.rx_queue() == Some(queue) {
            return self.poll(mem);
        }
        let q = self.queues[queue];
        if !q.ready || q.num == 0 {
            return false;
        }

        #[cfg(feature = "std")]
        {
            // One-shot sanity print: a bogus desc/avail/used address would have
            // the device DMA into random guest memory, which looks exactly like
            // "the kernel's stack got corrupted".
            static ONCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
            if std::env::var("RISCV_VIRTIO_DEBUG").is_ok()
                && ONCE.swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                std::eprintln!(
                    "[virtio] q{queue} num={} desc={:#x} avail={:#x} used={:#x}\r",
                    q.num, q.desc, q.avail, q.used
                );
            }
        }
        let avail_idx = mem.read_u16(q.avail + 2);
        let mut last = q.last_avail;
        let mut serviced = false;

        // Bound the loop: a driver can only have `num` chains outstanding, and
        // a corrupt avail.idx must not spin the emulator forever.
        let mut budget = q.num as u32 * 2 + 1;
        while last != avail_idx && budget > 0 {
            budget -= 1;
            let ring_slot = (last % q.num) as u64;
            let head = mem.read_u16(q.avail + 4 + ring_slot * 2);
            if let Some(chain) = Self::walk(mem, &q, head) {
                // The transfer itself happens now; only the used-ring report is
                // deferred, exactly as a real device DMAs then raises IRQ later.
                let written = self.dev.handle(queue, &chain, mem);
                // A device waiting on the host keeps the chain and completes it
                // later via `supply`; the transport must not report it now.
                if !self.dev.deferred_this() {
                    let due = self.now.wrapping_add(COMPLETION_LATENCY);
                    self.pending.push(Completion { queue, head: chain.head, len: written, due });
                }
                serviced = true;
            }
            last = last.wrapping_add(1);
        }

        self.queues[queue].last_avail = last;
        // No interrupt yet — `tick` raises it once the latency has elapsed.
        serviced
    }

    /// Follow a descriptor chain from `head`, splitting buffers by direction.
    /// Indirect descriptors are followed one level, which is all the spec allows.
    fn walk(mem: &GuestMem, q: &QueueState, head: u16) -> Option<Chain> {
        let mut chain = Chain {
            head,
            ..Default::default()
        };
        let mut idx = head;
        let mut budget = q.num as u32 + 1;

        while budget > 0 {
            budget -= 1;
            if idx >= q.num {
                return None;
            }
            let d = q.desc + idx as u64 * 16;
            let addr = mem.read_u64(d);
            let len = mem.read_u32(d + 8);
            let flags = mem.read_u16(d + 12);
            let next = mem.read_u16(d + 14);

            if flags & DESC_F_INDIRECT != 0 {
                // The descriptor points at a table of descriptors in guest
                // memory; walk that table linearly instead of this one.
                let count = (len / 16).min(QUEUE_NUM_MAX) as u64;
                let mut i = 0u64;
                let mut inner = 0u64;
                while i < count {
                    let id = addr + inner * 16;
                    let iaddr = mem.read_u64(id);
                    let ilen = mem.read_u32(id + 8);
                    let iflags = mem.read_u16(id + 12);
                    let inext = mem.read_u16(id + 14);
                    let buf = Buf {
                        addr: iaddr,
                        len: ilen,
                    };
                    if iflags & DESC_F_WRITE != 0 {
                        chain.writable.push(buf);
                    } else {
                        chain.readable.push(buf);
                    }
                    if iflags & DESC_F_NEXT == 0 {
                        break;
                    }
                    inner = inext as u64;
                    i += 1;
                }
            } else {
                let buf = Buf { addr, len };
                if flags & DESC_F_WRITE != 0 {
                    chain.writable.push(buf);
                } else {
                    chain.readable.push(buf);
                }
            }

            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        Some(chain)
    }

    fn push_used(mem: &mut GuestMem, q: &QueueState, head: u16, len: u32) {
        debug_assert!(q.used != 0 && q.num != 0, "used ring not programmed");
        let used_idx = mem.read_u16(q.used + 2);
        let slot = (used_idx % q.num) as u64;
        mem.write_u32(q.used + 4 + slot * 8, head as u32);
        mem.write_u32(q.used + 4 + slot * 8 + 4, len);
        // The index update must be visible after the ring entry; single-threaded
        // here, so ordering is implicit.
        mem.write_u16(q.used + 2, used_idx.wrapping_add(1));
    }

    /// Give the device a chance to produce work on its own (RX frames, etc).
    pub fn poll(&mut self, mem: &mut GuestMem) -> bool {
        if self.status == 0 {
            return false;
        }
        if self.dev.poll(mem) {
            self.interrupt_status |= 1;
            return true;
        }

        // Receive path. The device cannot reach the virtqueues itself, so the
        // transport does the take/fill/complete dance on its behalf: pull a
        // buffer the driver has offered, let the device write into it, then
        // report it used. Bounded per call so a busy backend cannot starve the
        // CPU loop.
        let Some(q) = self.dev.rx_queue() else { return false };
        let mut delivered = false;
        for _ in 0..32 {
            if !self.dev.has_rx() {
                break;
            }
            let Some(chain) = self.take_avail(q, mem) else {
                break; // driver has posted no buffers; leave the frame pending
            };
            let written = self.dev.fill_rx(&chain, mem);
            self.complete(q, mem, chain.head, written);
            delivered = true;
        }
        delivered
    }

    /// Push a chain onto a device-driven queue (used by RX paths). Returns the
    /// chain the driver made available, if any.
    pub fn take_avail(&mut self, queue: usize, mem: &GuestMem) -> Option<Chain> {
        let q = self.queues.get(queue).copied()?;
        if !q.ready || q.num == 0 {
            return None;
        }
        let avail_idx = mem.read_u16(q.avail + 2);
        if q.last_avail == avail_idx {
            return None;
        }
        let ring_slot = (q.last_avail % q.num) as u64;
        let head = mem.read_u16(q.avail + 4 + ring_slot * 2);
        let chain = Self::walk(mem, &q, head)?;
        self.queues[queue].last_avail = q.last_avail.wrapping_add(1);
        Some(chain)
    }

    /// Complete a chain previously obtained from [`take_avail`].
    pub fn complete(&mut self, queue: usize, mem: &mut GuestMem, head: u16, len: u32) {
        if let Some(q) = self.queues.get(queue).copied() {
            Self::push_used(mem, &q, head, len);
            self.interrupt_status |= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot of the transport state: everything the driver negotiated. Queue
// addresses and last_avail are the parts that CANNOT be reconstructed -- the
// guest believes its rings live where it put them.
// ---------------------------------------------------------------------------

use riscv_core::state::{Reader, Writer};

impl VirtioMmio {
    pub fn save_state(&self, w: &mut Writer) {
        w.u32(self.device_features_sel);
        w.u64(self.driver_features);
        w.u32(self.driver_features_sel);
        w.u32(self.status);
        w.u64(self.queue_sel as u64);
        for q in &self.queues {
            w.u16(q.num);
            w.bool(q.ready);
            w.u64(q.desc);
            w.u64(q.avail);
            w.u64(q.used);
            w.u16(q.last_avail);
        }
        w.u32(self.interrupt_status);
        w.u32(self.config_generation);
        w.u64(self.now);
        w.u64(self.pending.len() as u64);
        for c in &self.pending {
            w.u64(c.queue as u64);
            w.u16(c.head);
            w.u32(c.len);
            w.u64(c.due);
        }
    }

    pub fn load_state(&mut self, r: &mut Reader) -> Option<()> {
        self.device_features_sel = r.u32()?;
        self.driver_features = r.u64()?;
        self.driver_features_sel = r.u32()?;
        self.status = r.u32()?;
        self.queue_sel = r.u64()? as usize;
        for q in &mut self.queues {
            q.num = r.u16()?;
            q.ready = r.bool()?;
            q.desc = r.u64()?;
            q.avail = r.u64()?;
            q.used = r.u64()?;
            q.last_avail = r.u16()?;
        }
        self.interrupt_status = r.u32()?;
        self.config_generation = r.u32()?;
        self.now = r.u64()?;
        let n = r.u64()? as usize;
        self.pending.clear();
        for _ in 0..n {
            self.pending.push(Completion {
                queue: r.u64()? as usize,
                head: r.u16()?,
                len: r.u32()?,
                due: r.u64()?,
            });
        }
        Some(())
    }
}
