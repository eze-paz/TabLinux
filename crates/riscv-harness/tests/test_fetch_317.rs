use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn test_fetch_at_802001a4() {
    let mut bus = DeviceBus::new(1 << 32);
    bus.write_u32(0x802001a4, 0x00a00317);
    let pte = 0x2000000Fu64;
    bus.write_u64(0x8171f000 + 2 * 8, pte);
    bus.write_u64(0x8171f000 + 510 * 8, pte);

    let mut s = Supervisor::new(0x802001a4, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp.from_bits(0x800000000008171f);

    match s.debug_fetch(&mut bus, 0x802001a4) {
        Ok((paddr, half, width, raw)) => {
            eprintln!("debug_fetch: paddr={:#x} half={:#06x} width={} raw={:#010x}", paddr, half, width, raw);
        }
        Err(e) => {
            eprintln!("debug_fetch failed: {:#x}", e);
        }
    }
}
