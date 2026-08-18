//! Drive virtio-net through MMIO stores and virtqueues, as the guest driver
//! does, with no CPU and no kernel involved.

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

use crate::device_bus::DeviceBus;
use crate::virtio_net::{NET_HDR_LEN, RX_QUEUE, TX_QUEUE};
use riscv_core::execute::Bus;

const DRAM: u64 = 0x8000_0000;
const SLOT0: u64 = 0x1000_1000;
const QSIZE: u16 = 8;
const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

// Per-queue scratch. Queue 0 (rx) and queue 1 (tx) need separate rings.
fn desc_base(q: u64) -> u64 { DRAM + 0x1000 + q * 0x4000 }
fn avail_base(q: u64) -> u64 { DRAM + 0x2000 + q * 0x4000 }
fn used_base(q: u64) -> u64 { DRAM + 0x3000 + q * 0x4000 }
fn buf_base(q: u64) -> u64 { DRAM + 0x20000 + q * 0x4000 }

fn setup() -> DeviceBus {
    let mut bus = DeviceBus::new(1 << 20);
    let (irq, _shared) = bus.attach_virtio_net(MAC).expect("slot");
    assert_eq!(irq, 1);
    bus
}

/// Bring the device up the way virtio_mmio.c does, for both queues.
fn negotiate(bus: &mut DeviceBus) {
    assert_eq!(bus.read_u32(SLOT0 + 0x000), 0x7472_6976, "magic");
    assert_eq!(bus.read_u32(SLOT0 + 0x008), 1, "device id = net");

    bus.write_u32(SLOT0 + 0x070, 0);
    bus.write_u32(SLOT0 + 0x070, 3); // ACKNOWLEDGE | DRIVER

    bus.write_u32(SLOT0 + 0x014, 1);
    let hi = bus.read_u32(SLOT0 + 0x010);
    assert_eq!(hi & 1, 1, "VIRTIO_F_VERSION_1");
    bus.write_u32(SLOT0 + 0x024, 1);
    bus.write_u32(SLOT0 + 0x020, hi);
    bus.write_u32(SLOT0 + 0x014, 0);
    let lo = bus.read_u32(SLOT0 + 0x010);
    assert_eq!(lo & (1 << 5), 1 << 5, "VIRTIO_NET_F_MAC");
    bus.write_u32(SLOT0 + 0x024, 0);
    bus.write_u32(SLOT0 + 0x020, lo);

    bus.write_u32(SLOT0 + 0x070, 0xB); // FEATURES_OK

    for q in 0..2u64 {
        bus.write_u32(SLOT0 + 0x030, q as u32);
        bus.write_u32(SLOT0 + 0x038, QSIZE as u32);
        bus.write_u32(SLOT0 + 0x080, desc_base(q) as u32);
        bus.write_u32(SLOT0 + 0x084, (desc_base(q) >> 32) as u32);
        bus.write_u32(SLOT0 + 0x090, avail_base(q) as u32);
        bus.write_u32(SLOT0 + 0x094, (avail_base(q) >> 32) as u32);
        bus.write_u32(SLOT0 + 0x0a0, used_base(q) as u32);
        bus.write_u32(SLOT0 + 0x0a4, (used_base(q) >> 32) as u32);
        bus.write_u32(SLOT0 + 0x044, 1);
    }
    bus.write_u32(SLOT0 + 0x070, 0xF); // DRIVER_OK
}

fn desc(bus: &mut DeviceBus, q: u64, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
    let d = desc_base(q) + i * 16;
    bus.write_u64(d, addr);
    bus.write_u32(d + 8, len);
    bus.write_u16(d + 12, flags);
    bus.write_u16(d + 14, next);
}

fn publish(bus: &mut DeviceBus, q: u64, head: u16) {
    let idx = bus.read_u16(avail_base(q) + 2);
    bus.write_u16(avail_base(q) + 4 + (idx % QSIZE) as u64 * 2, head);
    bus.write_u16(avail_base(q) + 2, idx.wrapping_add(1));
}

fn settle(bus: &mut DeviceBus) {
    for _ in 0..4096 {
        bus.tick();
    }
}

#[test]
fn config_space_reports_the_mac() {
    let mut bus = setup();
    negotiate(&mut bus);
    let got: Vec<u8> = (0..6).map(|i| bus.read_u8(SLOT0 + 0x100 + i)).collect();
    assert_eq!(got, MAC, "driver must be able to read its MAC");
}

#[test]
fn transmitted_frame_reaches_the_backend() {
    let mut bus = setup();
    negotiate(&mut bus);

    // [hdr][frame], both device-readable, on the transmit queue.
    let hdr_at = buf_base(TX_QUEUE as u64);
    let frame_at = hdr_at + 64;
    let frame: Vec<u8> = (0..60u8).collect();
    for i in 0..NET_HDR_LEN as u64 {
        bus.write_u8(hdr_at + i, 0);
    }
    for (i, b) in frame.iter().enumerate() {
        bus.write_u8(frame_at + i as u64, *b);
    }
    desc(&mut bus, TX_QUEUE as u64, 0, hdr_at, NET_HDR_LEN as u32, 1, 1);
    desc(&mut bus, TX_QUEUE as u64, 1, frame_at, frame.len() as u32, 0, 0);
    publish(&mut bus, TX_QUEUE as u64, 0);
    bus.write_u32(SLOT0 + 0x050, TX_QUEUE as u32);
    settle(&mut bus);

    assert_eq!(bus.read_u16(used_base(TX_QUEUE as u64) + 2), 1, "tx buffer returned");
    let sent = bus.net_sent_frames().expect("net device attached");
    assert_eq!(sent.len(), 1, "exactly one frame reached the backend");
    assert_eq!(sent[0], frame, "the header must be stripped, the frame must not be");
}

#[test]
fn received_frame_reaches_the_guest() {
    let mut bus = setup();
    negotiate(&mut bus);

    // Driver offers one writable buffer on the receive queue.
    let rx_at = buf_base(RX_QUEUE as u64);
    desc(&mut bus, RX_QUEUE as u64, 0, rx_at, 256, 2 /* WRITE */, 0);
    publish(&mut bus, RX_QUEUE as u64, 0);

    let frame: Vec<u8> = (0..48u8).map(|i| i ^ 0xA5).collect();
    bus.net_inject_frame(&frame).expect("net device attached");
    settle(&mut bus);

    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 1, "rx buffer consumed");
    let len = bus.read_u32(used_base(RX_QUEUE as u64) + 8);
    assert_eq!(len as usize, NET_HDR_LEN + frame.len(), "used len counts header + frame");

    for i in 0..NET_HDR_LEN as u64 {
        assert_eq!(bus.read_u8(rx_at + i), 0, "virtio_net_hdr must be zeroed");
    }
    let got: Vec<u8> = (0..frame.len() as u64)
        .map(|i| bus.read_u8(rx_at + NET_HDR_LEN as u64 + i))
        .collect();
    assert_eq!(got, frame, "frame must land right after the header");
}

/// The bug every earlier test walked around: `publish` never writes
/// QueueNotify, but the real driver kicks after posting RX buffers. The
/// transport used to treat that kick like a submission, retiring every offered
/// buffer as a zero-length receive — the driver dropped the runts, refilled,
/// kicked again, and `ip link set eth0 up` dissolved into an infinite refill
/// storm that also starved genuine deliveries into seconds-long ping RTTs.
#[test]
fn rx_kick_must_not_eat_the_offered_buffers() {
    let mut bus = setup();
    negotiate(&mut bus);

    // Driver offers two empty buffers and kicks, exactly like virtnet_open.
    let rx_at = buf_base(RX_QUEUE as u64);
    desc(&mut bus, RX_QUEUE as u64, 0, rx_at, 256, 2 /* WRITE */, 0);
    publish(&mut bus, RX_QUEUE as u64, 0);
    desc(&mut bus, RX_QUEUE as u64, 1, rx_at + 0x400, 256, 2, 0);
    publish(&mut bus, RX_QUEUE as u64, 1);
    bus.write_u32(SLOT0 + 0x050, RX_QUEUE as u32);
    settle(&mut bus);

    assert_eq!(
        bus.read_u16(used_base(RX_QUEUE as u64) + 2),
        0,
        "no frame is pending, so offered RX buffers must stay offered, \
         not come back as zero-length receives"
    );

    // And they must still be usable for a real frame afterwards.
    let frame: Vec<u8> = (0..40u8).collect();
    bus.net_inject_frame(&frame).expect("net device attached");
    settle(&mut bus);
    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 1, "delivery still works");
    assert_eq!(
        bus.read_u32(used_base(RX_QUEUE as u64) + 8) as usize,
        NET_HDR_LEN + frame.len()
    );
}

/// The other half of the same kick: if a frame is already waiting because the
/// ring was empty, the kick that offers new space is the moment to deliver it.
#[test]
fn rx_kick_delivers_a_frame_that_was_waiting() {
    let mut bus = setup();
    negotiate(&mut bus);

    let frame: Vec<u8> = (0..32u8).map(|i| i | 0x40).collect();
    bus.net_inject_frame(&frame).expect("net device attached");
    settle(&mut bus);
    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 0, "nowhere to put it yet");

    let rx_at = buf_base(RX_QUEUE as u64);
    desc(&mut bus, RX_QUEUE as u64, 0, rx_at, 256, 2, 0);
    publish(&mut bus, RX_QUEUE as u64, 0);
    bus.write_u32(SLOT0 + 0x050, RX_QUEUE as u32);

    // Synchronously, on the kick itself — no settle. The whole point is that
    // delivery must not wait for the next background poll.
    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 1, "delivered on the kick");
    assert_eq!(bus.read_u8(rx_at + NET_HDR_LEN as u64), 0x40);
}

#[test]
fn frame_waits_when_the_guest_has_posted_no_buffers() {
    let mut bus = setup();
    negotiate(&mut bus);

    // No RX descriptors published yet.
    let frame = vec![0x11u8; 40];
    bus.net_inject_frame(&frame).unwrap();
    settle(&mut bus);
    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 0, "nothing to deliver into");

    // Now offer a buffer; the held frame must still arrive rather than be lost.
    let rx_at = buf_base(RX_QUEUE as u64);
    desc(&mut bus, RX_QUEUE as u64, 0, rx_at, 256, 2, 0);
    publish(&mut bus, RX_QUEUE as u64, 0);
    settle(&mut bus);

    assert_eq!(bus.read_u16(used_base(RX_QUEUE as u64) + 2), 1, "held frame delivered late");
    assert_eq!(bus.read_u8(rx_at + NET_HDR_LEN as u64), 0x11);
}

#[test]
fn oversized_frame_is_dropped_not_truncated() {
    let mut bus = setup();
    negotiate(&mut bus);

    // A buffer far too small for the frame. Without mergeable RX buffers there
    // is nowhere to put the rest, and half a frame would look real to the guest.
    let rx_at = buf_base(RX_QUEUE as u64);
    desc(&mut bus, RX_QUEUE as u64, 0, rx_at, 32, 2, 0);
    publish(&mut bus, RX_QUEUE as u64, 0);

    bus.net_inject_frame(&vec![0x77u8; 500]).unwrap();
    settle(&mut bus);

    let len = bus.read_u32(used_base(RX_QUEUE as u64) + 8);
    assert_eq!(len, 0, "a frame that does not fit must be dropped, reporting 0 bytes");
}
