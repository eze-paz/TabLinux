use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn test_step_at_802001a4() {
    let mut bus = DeviceBus::new(1 << 32);
    bus.write_u32(0x802001a4, 0x00a00317);
    let pte = 0x2000000Fu64;
    bus.write_u64(0x8171f000 + 2 * 8, pte);
    bus.write_u64(0x8171f000 + 510 * 8, pte);

    let mut s = Supervisor::new(0x802001a4, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp.from_bits(0x800000000008171f);

    eprintln!("Before: pc={:#x}", s.cpu.pc);
    let status = s.step(&mut bus);
    eprintln!("After:  pc={:#x} status={:?}", s.cpu.pc, status);
    assert_eq!(s.cpu.pc, 0x802001a8, "auipc should advance PC by 4");
}
