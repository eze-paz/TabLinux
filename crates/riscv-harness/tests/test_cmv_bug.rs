use riscv_core::types::Status;
use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn test_cmv_sp_s1() {
    let mut bus = DeviceBus::new(1 << 20);
    
    // c.mv sp, s1 = 0x8526
    bus.write_u16(0x80000000, 0x8526);
    
    let mut s = Supervisor::new(0x80000000u64, 0);
    s.priv_level = Privilege::Machine;
    
    s.cpu.write_reg(2, 0x1234);  // sp
    s.cpu.write_reg(9, 0xABCD);  // s1
    s.cpu.write_reg(10, 0x7777); // a0
    
    let status = s.step(&mut bus);
    
    let sp = s.cpu.read_reg(2);
    let s1 = s.cpu.read_reg(9);
    let a0 = s.cpu.read_reg(10);
    
    eprintln!("After c.mv sp, s1:");
    eprintln!("  sp = {:#x} (expected {:#x})", sp, 0xABCD);
    eprintln!("  s1 = {:#x} (expected {:#x})", s1, 0xABCD);
    eprintln!("  a0 = {:#x} (expected {:#x})", a0, 0x7777);
    eprintln!("  PC = {:#x}", s.cpu.pc);
    eprintln!("  status = {:?}", status);
    
    assert_eq!(sp, 0xABCD, "sp should be s1");
    assert_eq!(a0, 0x7777, "a0 should NOT change");
}

#[test]
fn test_cmv_a0_s1() {
    let mut bus = DeviceBus::new(1 << 20);
    
    // c.mv a0, s1 = 0x852A (rd=10, rs2=9)
    bus.write_u16(0x80000000, 0x852A);
    
    let mut s = Supervisor::new(0x80000000u64, 0);
    s.priv_level = Privilege::Machine;
    
    s.cpu.write_reg(10, 0x7777); // a0
    s.cpu.write_reg(9, 0xABCD);  // s1
    
    let status = s.step(&mut bus);
    
    let a0 = s.cpu.read_reg(10);
    let s1 = s.cpu.read_reg(9);
    
    eprintln!("After c.mv a0, s1:");
    eprintln!("  a0 = {:#x} (expected {:#x})", a0, 0xABCD);
    eprintln!("  s1 = {:#x} (expected {:#x})", s1, 0xABCD);
    eprintln!("  PC = {:#x}", s.cpu.pc);
    
    assert_eq!(a0, 0xABCD, "a0 should be s1");
}
