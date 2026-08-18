//! virtio-blk device (spec 5.2) on top of the shared MMIO transport.
//!
//! Storage lives behind [`BlockBackend`], which is the seam the wasm port hangs
//! off: natively it is a file, in a browser it will be an OPFS sync access
//! handle with an HTTP range-request backfill. The device model above it does
//! not change.

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::virtio::{sg_len, sg_read, sg_write, Chain, GuestMem, VirtioDevice, VIRTIO_F_VERSION_1};

pub const SECTOR_SIZE: usize = 512;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// VIRTIO_BLK_F_FLUSH. Advertised so the guest issues cache flushes rather than
/// assuming a writethrough device; ext4's journal correctness depends on it once
/// a backend does any buffering.
const F_FLUSH: u64 = 1 << 9;
/// VIRTIO_BLK_F_RO.
const F_RO: u64 = 1 << 5;

/// Backing store for a virtio-blk device, addressed in 512-byte sectors.
pub trait BlockBackend {
    fn capacity_sectors(&self) -> u64;
    /// Read `buf.len()` bytes starting at `sector`. Returns false on error.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool;
    /// Write `buf.len()` bytes starting at `sector`. Returns false on error.
    fn write(&mut self, sector: u64, buf: &[u8]) -> bool;
    fn flush(&mut self) -> bool {
        true
    }
    fn read_only(&self) -> bool {
        false
    }
}

/// In-memory backing store. Used by the unit tests and handy for a ramdisk.
pub struct MemBackend {
    pub data: Vec<u8>,
}

impl MemBackend {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl BlockBackend for MemBackend {
    fn capacity_sectors(&self) -> u64 {
        (self.data.len() / SECTOR_SIZE) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool {
        let off = sector as usize * SECTOR_SIZE;
        let Some(end) = off.checked_add(buf.len()) else {
            return false;
        };
        if end > self.data.len() {
            return false;
        }
        buf.copy_from_slice(&self.data[off..end]);
        true
    }

    fn write(&mut self, sector: u64, buf: &[u8]) -> bool {
        let off = sector as usize * SECTOR_SIZE;
        let Some(end) = off.checked_add(buf.len()) else {
            return false;
        };
        if end > self.data.len() {
            return false;
        }
        self.data[off..end].copy_from_slice(buf);
        true
    }
}

pub struct VirtioBlk {
    backend: Box<dyn BlockBackend>,
    /// Reported in the GET_ID response and /sys/block/vda/serial.
    serial: [u8; 20],
    pub reads: u64,
    pub writes: u64,
    /// Largest chain the transport has handed us, as (descriptors, bytes).
    /// A read request is normally 3 descriptors and one filesystem block; a
    /// chain far bigger than that means `walk` ran past the end of the chain
    /// into stale descriptors, and the DMA would land in buffers the driver
    /// no longer owns.
    pub max_descs: usize,
    pub max_bytes: usize,
}

impl VirtioBlk {
    pub fn new(backend: Box<dyn BlockBackend>) -> Self {
        let mut serial = [0u8; 20];
        let tag = b"riscv-vm-disk";
        serial[..tag.len()].copy_from_slice(tag);
        Self {
            backend,
            serial,
            reads: 0,
            writes: 0,
            max_descs: 0,
            max_bytes: 0,
        }
    }

    fn config(&self) -> [u8; 8] {
        // struct virtio_blk_config { le64 capacity; ... } — only capacity is
        // meaningful while we advertise no size/geometry feature bits.
        self.backend.capacity_sectors().to_le_bytes()
    }
}

impl VirtioDevice for VirtioBlk {
    fn attach_backend(&mut self, b: alloc::boxed::Box<dyn BlockBackend>) -> bool {
        VirtioBlk::attach_backend(self, b)
    }

    fn dev_state(&self) -> Option<alloc::vec::Vec<u8>> {
        // Capacity only. The contents live in the backend (a file, OPFS) and
        // persist independently of any snapshot; copying a whole disk into
        // every snapshot would be both enormous and a second source of truth.
        Some(self.backend.capacity_sectors().to_le_bytes().to_vec())
    }

    fn device_id(&self) -> u32 {
        2
    }

    fn features(&self) -> u64 {
        let mut f = VIRTIO_F_VERSION_1 | F_FLUSH;
        if self.backend.read_only() {
            f |= F_RO;
        }
        f
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn config_read(&self, off: usize) -> u8 {
        self.config().get(off).copied().unwrap_or(0)
    }

    fn handle(&mut self, _queue: usize, chain: &Chain, mem: &mut GuestMem) -> u32 {
        let nd = chain.readable.len() + chain.writable.len();
        let nb = sg_len(&chain.readable) + sg_len(&chain.writable);
        #[cfg(feature = "std")]
        if nd > self.max_descs || nb > self.max_bytes {
            // Report only new maxima, so this stays quiet once it has settled.
            std::eprintln!("[virtio-blk] chain high-water: {nd} descs, {nb} bytes\r");
        }
        if nd > self.max_descs { self.max_descs = nd; }
        if nb > self.max_bytes { self.max_bytes = nb; }

        // Layout: 16-byte header (readable), data, then a 1-byte status the
        // device writes. The status byte is always the tail of the writable
        // list; everything before it is payload for a read request.
        let mut hdr = [0u8; 16];
        if sg_read(mem, &chain.readable, 0, &mut hdr) != 16 {
            return 0;
        }
        let req_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());

        let writable_total = sg_len(&chain.writable);
        if writable_total == 0 {
            return 0;
        }
        let data_cap = writable_total - 1; // last byte is status

        let status = match req_type {
            T_IN => {
                let want = data_cap - (data_cap % SECTOR_SIZE);
                if want == 0 {
                    S_IOERR
                } else if sector.saturating_mul(1) + (want / SECTOR_SIZE) as u64
                    > self.backend.capacity_sectors()
                {
                    S_IOERR
                } else {
                    let mut buf = vec![0u8; want];
                    if self.backend.read(sector, &mut buf) {
                        sg_write(mem, &chain.writable, 0, &buf);
                        self.reads += 1;
                        S_OK
                    } else {
                        S_IOERR
                    }
                }
            }
            T_OUT => {
                if self.backend.read_only() {
                    S_IOERR
                } else {
                    let payload = sg_len(&chain.readable).saturating_sub(16);
                    let want = payload - (payload % SECTOR_SIZE);
                    if want == 0 {
                        S_IOERR
                    } else {
                        let mut buf = vec![0u8; want];
                        sg_read(mem, &chain.readable, 16, &mut buf);
                        if self.backend.write(sector, &buf) {
                            self.writes += 1;
                            S_OK
                        } else {
                            S_IOERR
                        }
                    }
                }
            }
            T_FLUSH => {
                if self.backend.flush() {
                    S_OK
                } else {
                    S_IOERR
                }
            }
            T_GET_ID => {
                let n = data_cap.min(self.serial.len());
                sg_write(mem, &chain.writable, 0, &self.serial[..n]);
                S_OK
            }
            _ => S_UNSUPP,
        };

        // Status goes in the final writable byte.
        sg_write(mem, &chain.writable, writable_total - 1, &[status]);

        match req_type {
            T_IN => (data_cap - (data_cap % SECTOR_SIZE)) as u32 + 1,
            T_GET_ID => data_cap.min(self.serial.len()) as u32 + 1,
            _ => 1,
        }
    }
}

/// A file-backed disk. Only available with the `std` feature, since the wasm
/// build supplies its own backend.
#[cfg(feature = "std")]
pub mod file_backend {
    use super::{BlockBackend, SECTOR_SIZE};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;

    pub struct FileBackend {
        file: File,
        sectors: u64,
        read_only: bool,
    }

    impl FileBackend {
        pub fn open(path: impl AsRef<Path>, read_only: bool) -> std::io::Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;
            let sectors = file.metadata()?.len() / SECTOR_SIZE as u64;
            Ok(Self {
                file,
                sectors,
                read_only,
            })
        }
    }

    impl BlockBackend for FileBackend {
        fn capacity_sectors(&self) -> u64 {
            self.sectors
        }

        fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool {
            self.file
                .seek(SeekFrom::Start(sector * SECTOR_SIZE as u64))
                .and_then(|_| self.file.read_exact(buf))
                .is_ok()
        }

        fn write(&mut self, sector: u64, buf: &[u8]) -> bool {
            if self.read_only {
                return false;
            }
            self.file
                .seek(SeekFrom::Start(sector * SECTOR_SIZE as u64))
                .and_then(|_| self.file.write_all(buf))
                .is_ok()
        }

        fn flush(&mut self) -> bool {
            self.file.flush().is_ok()
        }

        fn read_only(&self) -> bool {
            self.read_only
        }
    }
}

/// A stand-in used only while restoring a snapshot.
///
/// A snapshot stores the disk's *size*, never its contents: the bytes live
/// wherever the backend keeps them (a file, OPFS) and outlive the snapshot by
/// design. So a restored machine has a virtio-blk device the guest already
/// probed at boot, and no backing store until the host supplies one — which it
/// must do before the guest touches the disk. Reads fail rather than return
/// zeros, because silently serving zeros for a filesystem the guest believes in
/// is the worst possible outcome.
pub struct DetachedBackend {
    pub sectors: u64,
}

impl BlockBackend for DetachedBackend {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }
    fn read(&mut self, _sector: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn write(&mut self, _sector: u64, _buf: &[u8]) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
}

impl VirtioBlk {
    /// Swap in the real backing store after a restore. The capacity must match
    /// what the snapshot recorded, or the guest's idea of the disk and the
    /// host's would disagree — which surfaces much later as filesystem damage.
    pub fn attach_backend(&mut self, backend: alloc::boxed::Box<dyn BlockBackend>) -> bool {
        if backend.capacity_sectors() != self.backend.capacity_sectors() {
            return false;
        }
        self.backend = backend;
        true
    }
}
