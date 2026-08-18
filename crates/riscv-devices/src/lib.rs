#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

pub mod device_bus;
pub mod virtio;
pub mod virtio_9p;
pub mod virtio_blk;
pub mod virtio_net;
pub use device_bus::DeviceBus;
pub use virtio::{VirtioDevice, VirtioMmio};
pub use virtio_9p::{P9Fs, Virtio9p};
pub use virtio_blk::{BlockBackend, MemBackend, VirtioBlk};
pub use virtio_net::{NetBackend, VirtioNet};

/// Timestamp a device event in emulated milliseconds — the clock the guest
/// measures latency against, and the only one worth tracing when the question
/// is why a round trip inside the guest looks slow.
#[cfg(any(feature = "std", test))]
pub fn trace_ms(what: &str, mtime: u64) {
    extern crate std;
    std::eprintln!("[dev t] {what} at {:.3} ms\r", mtime as f64 / 10_000.0);
}

#[cfg(not(any(feature = "std", test)))]
pub fn trace_ms(_what: &str, _mtime: u64) {}

#[cfg(test)]
mod test_device_bus;
#[cfg(test)]
mod test_virtio;
#[cfg(test)]
mod test_virtio_net;
#[cfg(test)]
mod test_virtio_9p;
