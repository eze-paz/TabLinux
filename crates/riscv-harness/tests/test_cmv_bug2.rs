use riscv_core::types::Status;
use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

fn run_cmv(raw: u16, desc: &str) {
    let mut bus = DeviceBus::new(1 << 20);
    bus.write_u16(0x80000000, raw);
    
    let mut s = Supervisor::new(0x80000000u64, 0);
    s.priv_level = Privilege::Machine;
    
    // Set all regs to unique values to detect which one gets written
    for i in 0..32 {
        s.cpu.write_reg(i, 0x1000 + i as u64);
    }
    
    s.step(&mut bus);
    
    eprintln!("\n{} (raw={:#06x}):", desc, raw);
    eprintln!("  bits[11:7]={} bits[6:2]={}", (raw >> 7) & 0x1F, (raw >> 2) & 0x1F);
    
    // Find which register changed
    for i in 0..32 {
        let val = s.cpu.read_reg(i);
        if val != 0x1000 + i as u64 {
            eprintln!("  x{} changed to {:#x}", i, val);
        }
    }
}

#[test]
fn test_cmv_bug() {
    // c.mv sp, s1: rd=2(sp), rs2=9(s1)
    run_cmv(0x8526, "c.mv sp, s1");
    
    // c.mv s1, sp: rd=9(s1), rs2=2(sp)
    run_cmv(0x8922, "c.mv s1, sp");
    
    // c.mv a0, s1: rd=10(a0), rs2=9(s1)
    run_cmv(0x852A, "c.mv a0, s1");
    
    // c.mv s0, s1: rd=8(s0), rs2=9(s1)
    run_cmv(0x8522, "c.mv s0, s1");
    
    panic!("done");
}
