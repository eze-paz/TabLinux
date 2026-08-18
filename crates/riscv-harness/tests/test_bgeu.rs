//! Minimal test for bgeu x18,x18,+6

use riscv_core::types::Instr;
use riscv_core::execute::{Cpu, Bus};

struct TestBus;
impl Bus for TestBus {
    fn read_u8(&self, _addr: u64) -> u8 { 0 }
    fn read_u16(&self, _addr: u64) -> u16 { 0 }
    fn read_u32(&self, _addr: u64) -> u32 { 0 }
    fn read_u64(&self, _addr: u64) -> u64 { 0 }
    fn write_u8(&mut self, _addr: u64, _val: u8) {}
    fn write_u16(&mut self, _addr: u64, _val: u16) {}
    fn write_u32(&mut self, _addr: u64, _val: u32) {}
    fn write_u64(&mut self, _addr: u64, _val: u64) {}
    fn check_timer_interrupt(&self) -> bool { false }
}

#[test]
fn test_bgeu_self() {
    let mut cpu = Cpu::new(0x1000);
    let mut bus = TestBus;
    
    // bgeu x18, x18, +6
    let instr = Instr::Bgeu { rs1: 18, rs2: 18, imm: 6 };
    
    // Test with x18 = 1
    cpu.write_reg(18, 1);
    cpu.pc = 0x1000;
    cpu.execute_width(instr, 4, &mut bus);
    assert_eq!(cpu.pc, 0x1006, "bgeu x18,x18,+6 with x18=1 should branch to 0x1006");
    
    // Test with x18 = 0
    cpu.write_reg(18, 0);
    cpu.pc = 0x1000;
    cpu.execute_width(instr, 4, &mut bus);
    assert_eq!(cpu.pc, 0x1006, "bgeu x18,x18,+6 with x18=0 should branch to 0x1006");
    
    // Test with x18 = max
    cpu.write_reg(18, u64::MAX);
    cpu.pc = 0x1000;
    cpu.execute_width(instr, 4, &mut bus);
    assert_eq!(cpu.pc, 0x1006, "bgeu x18,x18,+6 with x18=MAX should branch to 0x1006");
}
