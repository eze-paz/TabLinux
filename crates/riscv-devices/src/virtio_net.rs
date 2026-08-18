//! virtio-net device (spec 5.1) on top of the shared MMIO transport.
//!
//! Two virtqueues: 0 is receive, 1 is transmit. Every buffer on both carries a
//! 12-byte `virtio_net_hdr` in front of the Ethernet frame — with
//! VIRTIO_F_VERSION_1 negotiated the header is always the full 12 bytes,
//! including `num_buffers`, whether or not mergeable RX buffers are in use.
//!
//! Frames enter and leave through [`NetBackend`], which is the seam the wasm
//! port hangs off, exactly as [`crate::virtio_blk::BlockBackend`] is for
//! storage. Natively that is a loopback or a test double; in a browser it
//! becomes v86's `fake_network.js` (ARP, DHCP, ICMP, DNS-over-HTTPS and TCP
//! termination) feeding `wisp_network.js` for egress. The device model does not
//! change — the browser cannot emit raw packets, so TCP has to be terminated
//! host-side either way, and that logic already exists in JavaScript.

extern crate alloc;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use crate::virtio::{sg_len, sg_read, sg_write, Chain, GuestMem, VirtioDevice, VIRTIO_F_VERSION_1};

/// Size of `struct virtio_net_hdr` under VIRTIO_F_VERSION_1.
pub const NET_HDR_LEN: usize = 12;

/// Largest Ethernet frame we will carry (1500 MTU + 14 header + VLAN slack).
pub const MAX_FRAME: usize = 1600;

/// VIRTIO_NET_F_MAC — the config space carries a MAC the driver should adopt.
const F_MAC: u64 = 1 << 5;

pub const RX_QUEUE: usize = 0;
pub const TX_QUEUE: usize = 1;

/// Where Ethernet frames come from and go to.
///
/// `transmit` is called with a frame the guest is sending. `receive` is polled
/// for frames to hand the guest; returning None means nothing is waiting.
pub trait NetBackend {
    fn transmit(&mut self, frame: &[u8]);
    fn receive(&mut self) -> Option<Vec<u8>>;
}

/// Loops every transmitted frame straight back to the guest.
///
/// Useless as a network — the guest will see its own ARP requests come back at
/// it — but it proves the full path end to end without needing a host stack,
/// and the tests use it.
pub struct LoopbackBackend {
    queue: VecDeque<Vec<u8>>,
}

impl LoopbackBackend {
    pub fn new() -> Self {
        Self { queue: VecDeque::new() }
    }
}

impl Default for LoopbackBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetBackend for LoopbackBackend {
    fn transmit(&mut self, frame: &[u8]) {
        self.queue.push_back(frame.to_vec());
    }
    fn receive(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }
}

/// Records what the guest sent and lets a test inject inbound frames.
pub struct CaptureBackend {
    pub sent: Vec<Vec<u8>>,
    pub inbound: VecDeque<Vec<u8>>,
}

impl CaptureBackend {
    pub fn new() -> Self {
        Self { sent: Vec::new(), inbound: VecDeque::new() }
    }
}

impl Default for CaptureBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetBackend for CaptureBackend {
    fn transmit(&mut self, frame: &[u8]) {
        self.sent.push(frame.to_vec());
    }
    fn receive(&mut self) -> Option<Vec<u8>> {
        self.inbound.pop_front()
    }
}

pub struct VirtioNet {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
    /// A frame pulled from the backend that is waiting for the guest to post an
    /// RX buffer. Held here so `receive` is not called again until it lands.
    pending_rx: Option<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
}

impl VirtioNet {
    pub fn new(backend: Box<dyn NetBackend>, mac: [u8; 6]) -> Self {
        Self { backend, mac, pending_rx: None, tx_frames: 0, rx_frames: 0 }
    }

    /// struct virtio_net_config: mac[6], then le16 status, then le16
    /// max_virtqueue_pairs. Only the MAC is meaningful while we advertise
    /// neither STATUS nor MQ, but the driver may still read the whole thing.
    fn config(&self) -> [u8; 10] {
        let mut c = [0u8; 10];
        c[..6].copy_from_slice(&self.mac);
        c
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        1
    }

    fn features(&self) -> u64 {
        // Deliberately minimal: no checksum offload, no GSO, no mergeable RX
        // buffers. Every one of those would mean the guest hands us frames we
        // must fix up or split, and none of it buys anything when the far side
        // is a userspace TCP stack.
        VIRTIO_F_VERSION_1 | F_MAC
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn config_read(&self, off: usize) -> u8 {
        self.config().get(off).copied().unwrap_or(0)
    }

    /// Transmit path. The guest posts [hdr][frame] as device-readable buffers
    /// on queue 1; there is nothing to write back, so the used length is 0.
    fn handle(&mut self, queue: usize, chain: &Chain, mem: &mut GuestMem) -> u32 {
        if queue != TX_QUEUE {
            // A buffer arriving on the RX queue is the driver offering us space,
            // not sending anything. `poll` consumes those.
            return 0;
        }
        let total = sg_len(&chain.readable);
        if total <= NET_HDR_LEN {
            return 0;
        }
        let len = (total - NET_HDR_LEN).min(MAX_FRAME);
        let mut frame = vec![0u8; len];
        sg_read(mem, &chain.readable, NET_HDR_LEN, &mut frame);
        self.backend.transmit(&frame);
        self.tx_frames += 1;
        0
    }

    fn dev_state(&self) -> Option<Vec<u8>> {
        let mut w = riscv_core::state::Writer::default();
        w.bytes(&self.mac);
        match &self.pending_rx {
            Some(f) => {
                w.bool(true);
                w.bytes(f);
            }
            None => w.bool(false),
        }
        Some(w.buf)
    }

    fn load_dev_state(&mut self, bytes: &[u8]) -> Option<()> {
        let mut r = riscv_core::state::Reader::new(bytes);
        let mac = r.bytes()?;
        self.mac.copy_from_slice(mac.get(..6)?);
        self.pending_rx = if r.bool()? { Some(r.bytes()?.to_vec()) } else { None };
        Some(())
    }

    fn rx_queue(&self) -> Option<usize> {
        Some(RX_QUEUE)
    }

    fn has_rx(&mut self) -> bool {
        if self.pending_rx.is_none() {
            self.pending_rx = self.backend.receive();
        }
        self.pending_rx.is_some()
    }

    /// Fill one guest RX buffer with the pending frame. Returns the number of
    /// bytes written, which is what goes in the used ring.
    fn fill_rx(&mut self, chain: &Chain, mem: &mut GuestMem) -> u32 {
        let Some(frame) = self.pending_rx.take() else {
            return 0;
        };
        let capacity = sg_len(&chain.writable);
        if capacity < NET_HDR_LEN + frame.len() {
            // Too small to hold the frame. Without mergeable RX buffers there
            // is nowhere to put the rest, so drop it rather than deliver a
            // truncated frame the guest would treat as real.
            return 0;
        }
        let hdr = [0u8; NET_HDR_LEN];
        sg_write(mem, &chain.writable, 0, &hdr);
        sg_write(mem, &chain.writable, NET_HDR_LEN, &frame);
        self.rx_frames += 1;
        (NET_HDR_LEN + frame.len()) as u32
    }
}

use alloc::rc::Rc;
use core::cell::RefCell;

/// Frames in flight between the guest and whatever is acting as the network.
///
/// Two queues rather than a callback, because the host side is asynchronous in
/// every real deployment: natively a test drains them, and in the browser JS
/// polls them from the event loop. Single-threaded, hence Rc/RefCell.
#[derive(Default)]
pub struct NetQueues {
    /// Guest transmitted these; the host should send them on.
    pub to_host: VecDeque<Vec<u8>>,
    /// Host received these; the guest should be given them.
    pub to_guest: VecDeque<Vec<u8>>,
}

pub type SharedNet = Rc<RefCell<NetQueues>>;

/// A NetBackend that just moves frames between the shared queues.
pub struct SharedBackend(pub SharedNet);

impl NetBackend for SharedBackend {
    fn transmit(&mut self, frame: &[u8]) {
        self.0.borrow_mut().to_host.push_back(frame.to_vec());
    }
    fn receive(&mut self) -> Option<Vec<u8>> {
        self.0.borrow_mut().to_guest.pop_front()
    }
}

impl VirtioNet {
    /// For snapshots: the held-back inbound frame, if any.
    pub fn pending_rx(&self) -> Option<&Vec<u8>> {
        self.pending_rx.as_ref()
    }
    pub fn set_pending_rx(&mut self, f: Option<Vec<u8>>) {
        self.pending_rx = f;
    }
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }
}
