//! Drive virtio-blk exactly as the guest driver does — through MMIO stores and
//! a virtqueue built in guest DRAM — with no CPU and no kernel involved.
//!
//! This is the reason the backend sits behind a trait: the whole transport can
//! be exercised in milliseconds here instead of by booting Linux and reading
//! console output.

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec;

use crate::device_bus::DeviceBus;
use crate::virtio_blk::{MemBackend, VirtioBlk, SECTOR_SIZE};
use riscv_core::execute::Bus;

const DRAM: u64 = 0x8000_0000;
const SLOT0: u64 = 0x1000_1000;

// Guest-physical scratch layout for the test's virtqueue.
const DESC: u64 = DRAM + 0x1000;
const AVAIL: u64 = DRAM + 0x2000;
const USED: u64 = DRAM + 0x3000;
const HDR: u64 = DRAM + 0x4000;
const DATA: u64 = DRAM + 0x5000;
const STATUS: u64 = DRAM + 0x6000;
const QSIZE: u16 = 8;

/// Disk contents: sector N is filled with the byte N.
fn make_disk(sectors: usize) -> alloc::vec::Vec<u8> {
    let mut d = vec![0u8; sectors * SECTOR_SIZE];
    for s in 0..sectors {
        d[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE].fill(s as u8);
    }
    d
}

fn setup(sectors: usize) -> DeviceBus {
    let mut bus = DeviceBus::new(1 << 20);
    let blk = VirtioBlk::new(Box::new(MemBackend::new(make_disk(sectors))));
    let irq = bus.attach_virtio(Box::new(blk)).expect("slot");
    assert_eq!(irq, 1, "first slot must own hwirq 1 to match the devicetree");
    bus
}

/// Bring the device up the way `virtio_mmio.c` does.
fn negotiate(bus: &mut DeviceBus) {
    assert_eq!(bus.read_u32(SLOT0 + 0x000), 0x7472_6976, "magic");
    assert_eq!(bus.read_u32(SLOT0 + 0x004), 2, "version");
    assert_eq!(bus.read_u32(SLOT0 + 0x008), 2, "device id = block");

    bus.write_u32(SLOT0 + 0x070, 0); // reset
    bus.write_u32(SLOT0 + 0x070, 1); // ACKNOWLEDGE
    bus.write_u32(SLOT0 + 0x070, 3); // | DRIVER

    // VIRTIO_F_VERSION_1 lives in the upper feature word.
    bus.write_u32(SLOT0 + 0x014, 1);
    let hi = bus.read_u32(SLOT0 + 0x010);
    assert_eq!(hi & 1, 1, "device must offer VIRTIO_F_VERSION_1");
    bus.write_u32(SLOT0 + 0x024, 1);
    bus.write_u32(SLOT0 + 0x020, hi);

    bus.write_u32(SLOT0 + 0x070, 0xB); // | FEATURES_OK

    bus.write_u32(SLOT0 + 0x030, 0); // QueueSel = 0
    bus.write_u32(SLOT0 + 0x038, QSIZE as u32);
    bus.write_u32(SLOT0 + 0x080, DESC as u32);
    bus.write_u32(SLOT0 + 0x084, (DESC >> 32) as u32);
    bus.write_u32(SLOT0 + 0x090, AVAIL as u32);
    bus.write_u32(SLOT0 + 0x094, (AVAIL >> 32) as u32);
    bus.write_u32(SLOT0 + 0x0a0, USED as u32);
    bus.write_u32(SLOT0 + 0x0a4, (USED >> 32) as u32);
    bus.write_u32(SLOT0 + 0x044, 1); // QueueReady

    bus.write_u32(SLOT0 + 0x070, 0xF); // | DRIVER_OK
}

fn desc(bus: &mut DeviceBus, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
    bus.write_u64(DESC + i * 16, addr);
    bus.write_u32(DESC + i * 16 + 8, len);
    bus.write_u16(DESC + i * 16 + 12, flags);
    bus.write_u16(DESC + i * 16 + 14, next);
}

/// Build a 3-descriptor request chain (header, data, status) and kick it.
fn submit(bus: &mut DeviceBus, req_type: u32, sector: u64, data_len: u32, data_writable: bool) {
    bus.write_u32(HDR, req_type);
    bus.write_u32(HDR + 4, 0);
    bus.write_u64(HDR + 8, sector);
    bus.write_u8(STATUS, 0xFF); // poison, so we can tell it was written

    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    desc(bus, 0, HDR, 16, NEXT, 1);
    desc(bus, 1, DATA, data_len, NEXT | if data_writable { WRITE } else { 0 }, 2);
    desc(bus, 2, STATUS, 1, WRITE, 0);

    let idx = bus.read_u16(AVAIL + 2);
    bus.write_u16(AVAIL + 4 + (idx % QSIZE) as u64 * 2, 0); // chain head = desc 0
    bus.write_u16(AVAIL + 2, idx.wrapping_add(1));
    bus.write_u32(SLOT0 + 0x050, 0); // QueueNotify
    settle(bus);
}

/// Run the bus forward until deferred completions have retired.
fn settle(bus: &mut DeviceBus) {
    for _ in 0..4096 {
        bus.tick();
    }
}

#[test]
fn blk_reports_capacity_in_config_space() {
    let mut bus = setup(64);
    negotiate(&mut bus);
    let cap = bus.read_u32(SLOT0 + 0x100) as u64 | ((bus.read_u32(SLOT0 + 0x104) as u64) << 32);
    assert_eq!(cap, 64, "capacity is in 512-byte sectors");
}

#[test]
fn blk_read_returns_disk_contents() {
    let mut bus = setup(64);
    negotiate(&mut bus);
    submit(&mut bus, 0 /* T_IN */, 7, SECTOR_SIZE as u32, true);

    assert_eq!(bus.read_u8(STATUS), 0, "status must be VIRTIO_BLK_S_OK");
    for i in 0..SECTOR_SIZE as u64 {
        assert_eq!(bus.read_u8(DATA + i), 7, "sector 7 is filled with 7s");
    }
}

#[test]
fn blk_write_reaches_the_backend_and_reads_back() {
    let mut bus = setup(64);
    negotiate(&mut bus);

    for i in 0..SECTOR_SIZE as u64 {
        bus.write_u8(DATA + i, 0xA5);
    }
    submit(&mut bus, 1 /* T_OUT */, 3, SECTOR_SIZE as u32, false);
    assert_eq!(bus.read_u8(STATUS), 0, "write status");

    // Read it back through the device to prove it hit the backend.
    for i in 0..SECTOR_SIZE as u64 {
        bus.write_u8(DATA + i, 0);
    }
    submit(&mut bus, 0, 3, SECTOR_SIZE as u32, true);
    assert_eq!(bus.read_u8(STATUS), 0);
    for i in 0..SECTOR_SIZE as u64 {
        assert_eq!(bus.read_u8(DATA + i), 0xA5, "written data must persist");
    }
}

#[test]
fn blk_multi_sector_transfer() {
    let mut bus = setup(64);
    negotiate(&mut bus);
    let len = (SECTOR_SIZE * 4) as u32;
    submit(&mut bus, 0, 10, len, true);
    assert_eq!(bus.read_u8(STATUS), 0);
    for s in 0..4u64 {
        let off = s * SECTOR_SIZE as u64;
        assert_eq!(bus.read_u8(DATA + off), (10 + s) as u8);
        assert_eq!(bus.read_u8(DATA + off + 511), (10 + s) as u8);
    }
}

#[test]
fn blk_updates_used_ring_and_raises_irq() {
    let mut bus = setup(64);
    negotiate(&mut bus);
    assert_eq!(bus.read_u16(USED + 2), 0, "used.idx starts at 0");
    assert!(!bus.check_external_interrupt(), "no interrupt before any request");

    submit(&mut bus, 0, 1, SECTOR_SIZE as u32, true);

    assert_eq!(bus.read_u16(USED + 2), 1, "used.idx advances");
    assert_eq!(bus.read_u32(USED + 4), 0, "used entry names the chain head");
    assert_eq!(
        bus.read_u32(USED + 8),
        SECTOR_SIZE as u32 + 1,
        "used len counts data + status byte"
    );
    assert_eq!(bus.read_u32(SLOT0 + 0x060), 1, "InterruptStatus: used buffer");

    // The PLIC line only drops once the driver acknowledges.
    bus.write_u32(SLOT0 + 0x064, 1); // InterruptACK
    assert_eq!(bus.read_u32(SLOT0 + 0x060), 0);
}

#[test]
fn blk_rejects_out_of_range_sector() {
    let mut bus = setup(8);
    negotiate(&mut bus);
    submit(&mut bus, 0, 9999, SECTOR_SIZE as u32, true);
    assert_eq!(bus.read_u8(STATUS), 1, "must report VIRTIO_BLK_S_IOERR");
}

#[test]
fn blk_rejects_unknown_request_type() {
    let mut bus = setup(8);
    negotiate(&mut bus);
    submit(&mut bus, 0xDEAD, 0, SECTOR_SIZE as u32, true);
    assert_eq!(bus.read_u8(STATUS), 2, "must report VIRTIO_BLK_S_UNSUPP");
}

/// Deterministic xorshift, so a failure is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn upto(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Submit a request whose data payload is split across several descriptors at
/// arbitrary byte boundaries — which is what the block layer actually produces
/// once a bio spans page fragments. A device that assumes one sector-aligned
/// data descriptor passes the simple tests above and corrupts memory here.
fn submit_split(
    bus: &mut DeviceBus,
    req_type: u32,
    sector: u64,
    chunks: &[u32],
    data_writable: bool,
) {
    bus.write_u32(HDR, req_type);
    bus.write_u32(HDR + 4, 0);
    bus.write_u64(HDR + 8, sector);
    bus.write_u8(STATUS, 0xFF);

    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    let wflag = if data_writable { WRITE } else { 0 };

    desc(bus, 0, HDR, 16, NEXT, 1);
    let mut addr = DATA;
    let n = chunks.len();
    for (i, &len) in chunks.iter().enumerate() {
        let idx = 1 + i as u64;
        desc(bus, idx, addr, len, NEXT | wflag, (idx + 1) as u16);
        addr += len as u64;
    }
    desc(bus, 1 + n as u64, STATUS, 1, WRITE, 0);

    let idx = bus.read_u16(AVAIL + 2);
    bus.write_u16(AVAIL + 4 + (idx % QSIZE) as u64 * 2, 0);
    bus.write_u16(AVAIL + 2, idx.wrapping_add(1));
    bus.write_u32(SLOT0 + 0x050, 0);
    settle(bus);
}

/// Split `total` into `parts` pieces at arbitrary (not sector-aligned) offsets.
fn split(rng: &mut Rng, total: u32, parts: usize) -> alloc::vec::Vec<u32> {
    let mut cuts: alloc::vec::Vec<u32> = (0..parts - 1)
        .map(|_| 1 + rng.upto(total as u64 - 1) as u32)
        .collect();
    cuts.sort_unstable();
    let mut out = alloc::vec::Vec::new();
    let mut prev = 0u32;
    for c in cuts {
        out.push(c - prev);
        prev = c;
    }
    out.push(total - prev);
    out.retain(|&l| l > 0);
    out
}

#[test]
fn blk_scatter_gather_reads_match_backend() {
    let mut bus = setup(128);
    negotiate(&mut bus);
    let mut rng = Rng(0x1234_5678_9abc_def1);

    for iter in 0..200 {
        let nsec = 1 + rng.upto(6) as u32;
        let total = nsec * SECTOR_SIZE as u32;
        let sector = rng.upto(128 - nsec as u64);
        let parts = 1 + rng.upto(4) as usize;
        let chunks = split(&mut rng, total, parts);

        // Poison the landing zone AND a guard band past it, so neither stale
        // data from the previous iteration nor an overrun can pass for success.
        const GUARD: u64 = 64;
        let poison = 8 * SECTOR_SIZE as u64 + GUARD;
        for i in 0..poison {
            bus.write_u8(DATA + i, 0x5A);
        }
        submit_split(&mut bus, 0, sector, &chunks, true);

        assert_eq!(bus.read_u8(STATUS), 0, "iter {iter}: status");
        for s in 0..nsec as u64 {
            for byte in [0u64, 1, 255, 511] {
                let off = s * SECTOR_SIZE as u64 + byte;
                assert_eq!(
                    bus.read_u8(DATA + off),
                    (sector + s) as u8,
                    "iter {iter}: sector {} byte {byte} (chunks {chunks:?})",
                    sector + s
                );
            }
        }
        // Nothing may be written past the payload.
        for g in 0..GUARD {
            assert_eq!(
                bus.read_u8(DATA + total as u64 + g),
                0x5A,
                "iter {iter}: DMA overran the data descriptors by {} bytes \
                 (sector {sector}, {nsec} sectors, chunks {chunks:?})",
                g + 1
            );
        }
    }
}

#[test]
fn blk_scatter_gather_writes_round_trip() {
    let mut bus = setup(128);
    negotiate(&mut bus);
    let mut rng = Rng(0xfeed_face_dead_beef);

    for iter in 0..200 {
        let nsec = 1 + rng.upto(6) as u32;
        let total = nsec * SECTOR_SIZE as u32;
        let sector = rng.upto(128 - nsec as u64);
        let parts = 1 + rng.upto(4) as usize;
        let chunks = split(&mut rng, total, parts);
        let tag = (iter * 7 + 1) as u8;

        for i in 0..total as u64 {
            bus.write_u8(DATA + i, tag ^ (i as u8));
        }
        submit_split(&mut bus, 1, sector, &chunks, false);
        assert_eq!(bus.read_u8(STATUS), 0, "iter {iter}: write status");

        for i in 0..total as u64 {
            bus.write_u8(DATA + i, 0);
        }
        submit_split(&mut bus, 0, sector, &[total], true);
        assert_eq!(bus.read_u8(STATUS), 0, "iter {iter}: readback status");
        for i in 0..total as u64 {
            assert_eq!(
                bus.read_u8(DATA + i),
                tag ^ (i as u8),
                "iter {iter}: byte {i} at sector {sector} (chunks {chunks:?})"
            );
        }
    }
}

#[test]
fn empty_slot_reads_as_zero_magic() {
    let mut bus = setup(8);
    // Slot 1 has nothing attached; the driver must see magic 0 and skip it.
    assert_eq!(bus.read_u32(SLOT0 + 0x1000), 0);
}
