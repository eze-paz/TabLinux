use riscv_core::execute::Cpu;
use riscv_core::types::Instr;
use riscv_core::execute::Bus;

struct DummyBus;
impl Bus for DummyBus {
    fn read_u8(&self, _addr: u64) -> u8 { 0 }
    fn read_u16(&self, _addr: u64) -> u16 { 0 }
    fn read_u32(&self, _addr: u64) -> u32 { 0 }
    fn read_u64(&self, _addr: u64) -> u64 { 0 }
    fn write_u8(&mut self, _addr: u64, _val: u8) {}
    fn write_u16(&mut self, _addr: u64, _val: u16) {}
    fn write_u32(&mut self, _addr: u64, _val: u32) {}
    fn write_u64(&mut self, _addr: u64, _val: u64) {}
}

#[test]
fn test_sub_bug() {
    let mut cpu = Cpu::new(0);
    let mut bus = DummyBus;
    
    cpu.write_reg(20, 0x8000000000000000u64); // s4
    cpu.write_reg(10, 0xffffffff80e1c900u64); // a0
    
    let instr = Instr::Sub { rd: 10, rs1: 20, rs2: 10 };
    cpu.execute_width(instr, 4, &mut bus);
    
    let result = cpu.read_reg(10);
    println!("s4 = {:#x}", cpu.read_reg(20));
    println!("a0 before = {:#x}", 0xffffffff80e1c900u64);
    println!("a0 after  = {:#x}", result);
    println!("expected  = {:#x}", 0x800000007f1e3700u64);
    assert_eq!(result, 0x800000007f1e3700u64, "SUB result mismatch!");
}
