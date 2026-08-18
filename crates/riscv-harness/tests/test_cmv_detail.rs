use riscv_core::types::Status;
use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn test_cmv_detail() {
    let mut bus = DeviceBus::new(1 << 20);
    
    // c.mv sp, s1 = 0x8526 (q=10, funct4=1000, rd=2, rs2=9)
    bus.write_u16(0x80000000, 0x8526);
    
    let mut s = Supervisor::new(0x80000000u64, 0);
    s.priv_level = Privilege::Machine;
    
    // Set registers to trackable values
    s.cpu.write_reg(0, 0x9999);   // x0 should stay 0
    s.cpu.write_reg(2, 0x1234);   // sp
    s.cpu.write_reg(9, 0xABCD);   // s1
    s.cpu.write_reg(10, 0x7777);  // a0
    
    eprintln!("Before step:");
    eprintln!("  x0  = {:#x}", s.cpu.read_reg(0));
    eprintln!("  x2  = {:#x}", s.cpu.read_reg(2));
    eprintln!("  x9  = {:#x}", s.cpu.read_reg(9));
    eprintln!("  x10 = {:#x}", s.cpu.read_reg(10));
    
    // Fetch and decode manually
    let raw = bus.read_u16(0x80000000);
    eprintln!("Raw instruction: {:#06x}", raw);
    eprintln!("  q = {}", raw & 0b11);
    eprintln!("  funct4 = {:#04b}", (raw >> 12) & 0b1111);
    eprintln!("  rd = {} (bits[11:7] = {:#05b})", (raw >> 7) & 0b11111, (raw >> 7) & 0b11111);
    eprintln!("  rs2 = {} (bits[6:2] = {:#05b})", (raw >> 2) & 0b11111, (raw >> 2) & 0b11111);
    
    let status = s.step(&mut bus);
    
    eprintln!("After step:");
    eprintln!("  x0  = {:#x} (expected 0)", s.cpu.read_reg(0));
    eprintln!("  x2  = {:#x} (expected 0xabcd)", s.cpu.read_reg(2));
    eprintln!("  x9  = {:#x} (expected 0xabcd)", s.cpu.read_reg(9));
    eprintln!("  x10 = {:#x} (expected 0x7777)", s.cpu.read_reg(10));
    eprintln!("  PC  = {:#x}", s.cpu.pc);
    eprintln!("  status = {:?}", status);
    
    // The assertion will fail to show the bug
    assert_eq!(s.cpu.read_reg(2), 0xABCD, "sp should be s1");
    assert_eq!(s.cpu.read_reg(10), 0x7777, "a0 should NOT change");
}
