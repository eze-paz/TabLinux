use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

/// Test c.bnez on a properly aligned boundary with sufficient memory.
/// DeviceBus DRAM starts at 0x80000000, so we must use addresses in that range.
#[test]
fn test_c_bnez_taken() {
    let addr = 0x8000_0100u64;
    let mut bus = DeviceBus::new(1 << 20); // 1MB DRAM at 0x80000000
    bus.write_u16(addr, 0xEA61); // c.bnez a2, +208
    
    let mut s = Supervisor::new(addr, 0);
    s.priv_level = Privilege::Machine;
    s.cpu.write_reg(12, 0x817140c8u64);  // a2 = non-zero
    
    let status = s.step(&mut bus);
    
    // c.bnez should branch by 208 bytes: 0x80000100 + 208 = 0x800001D0
    assert_eq!(s.cpu.pc, 0x8000_01D0, "c.bnez should be taken when a2 is non-zero");
    assert!(matches!(status, riscv_core::types::Status::Running));
}

#[test]
fn test_c_bnez_not_taken() {
    let addr = 0x8000_0100u64;
    let mut bus = DeviceBus::new(1 << 20);
    bus.write_u16(addr, 0xEA61);
    
    let mut s = Supervisor::new(addr, 0);
    s.priv_level = Privilege::Machine;
    s.cpu.write_reg(12, 0u64);  // a2 = zero
    
    let status = s.step(&mut bus);
    
    // c.bnez should fall through: 0x80000100 + 2 = 0x80000102
    assert_eq!(s.cpu.pc, 0x8000_0102, "c.bnez should fall through when a2 is zero");
    assert!(matches!(status, riscv_core::types::Status::Running));
}

#[test]
fn test_c_beqz_taken() {
    let addr = 0x8000_0100u64;
    let mut bus = DeviceBus::new(1 << 20);
    bus.write_u16(addr, 0xCA61); // c.beqz a2, +208
    
    let mut s = Supervisor::new(addr, 0);
    s.priv_level = Privilege::Machine;
    s.cpu.write_reg(12, 0u64);  // a2 = zero
    
    let status = s.step(&mut bus);
    
    // c.beqz should branch by 208 bytes
    assert_eq!(s.cpu.pc, 0x8000_01D0, "c.beqz should be taken when a2 is zero");
    assert!(matches!(status, riscv_core::types::Status::Running));
}

#[test]
fn test_c_beqz_not_taken() {
    let addr = 0x8000_0100u64;
    let mut bus = DeviceBus::new(1 << 20);
    bus.write_u16(addr, 0xCA61);
    
    let mut s = Supervisor::new(addr, 0);
    s.priv_level = Privilege::Machine;
    s.cpu.write_reg(12, 0x1234u64);  // a2 = non-zero
    
    let status = s.step(&mut bus);
    
    // c.beqz should fall through
    assert_eq!(s.cpu.pc, 0x8000_0102, "c.beqz should fall through when a2 is non-zero");
    assert!(matches!(status, riscv_core::types::Status::Running));
}
