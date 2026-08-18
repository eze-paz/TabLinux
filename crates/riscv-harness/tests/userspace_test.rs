//! Test RVC decoder with VALID 16-bit encodings

use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use riscv_core::compressed;

#[test]
fn test_rvc_decoder_valid_instructions() {
    let mut bus = DeviceBus::new(1 << 20);
    let mut s = Supervisor::new(0x8000_0000u64, 0);
    
    s.priv_level = Privilege::User;
    s.mstatus.mpp = 0;
    s.mstatus.mpie = true;
    s.mstatus.mie = true;
    
    let trap_addr = 0x80001000u64;
    let trap_code: [u16; 2] = [0x0021, 0x9082];
    for (i, &word) in trap_code.iter().enumerate() {
        bus.write_u16(trap_addr + (i * 2) as u64, word);
    }
    s.stvec = trap_addr;
    
    // VALID 16-bit RVC instructions with CORRECT encodings:
    // C.ADDI x4, 16: q=01, f3=000, rd=4, imm[5:0]=16 -> 0x0240
    let addi_x4_16: u16 = 0x0241;
    // C.ADDI x5, 16: q=01, f3=000, rd=5, imm=16 -> 0x02c0
    let addi_x5_16: u16 = 0x02c1;
    // C.ADDI x7, 10: q=01, f3=000, rd=7, imm=10 -> 0x03a8
    let addi_x7_10: u16 = 0x03a9;
    // C.ADDI x8, 26: q=01, f3=000, rd=8, imm=26 -> 0x0468
    let addi_x8_26: u16 = 0x0469;
    
    let test_code: [u16; 7] = [
        addi_x4_16,   // 0: x4 = 16
        addi_x5_16,   // 1: x5 = 16
        addi_x5_16,   // 2: x5 = 32
        addi_x4_16,   // 3: x4 = 32
        addi_x7_10,   // 4: x7 = 10
        addi_x8_26,   // 5: x8 = 26
        0x0000,       // 6: c.j loop
    ];
    
    eprintln!("=== Decoding verification ===");
    for (i, &word) in test_code.iter().enumerate() {
        let decoded = compressed::decompress(word);
        eprintln!("  [{}] {:#06x} -> {:?}", i, word, decoded);
    }
    
    // Store test program
    let program_addr = 0x80000000u64;
    for (i, &word) in test_code.iter().enumerate() {
        bus.write_u16(program_addr + (i * 2) as u64, word);
    }
    
    eprintln!("\n=== Execution trace ===");
    let mut step = 0u64;
    while step < 10 {
        let pc_before = s.cpu.pc;
        s.step(&mut bus);
        step += 1;
        
        let raw = bus.read_u16(pc_before);
        let decoded = compressed::decompress(raw);
        eprintln!("Step {}: pc={:#x} -> {:#x} decoded={:?}", step, pc_before, s.cpu.pc, decoded);
        
        if step >= 7 { break; }
    }
    
    eprintln!("\n=== Final register state ===");
    eprintln!("  x4: {:#018x} (expected: 32)", s.cpu.read_reg(4));
    eprintln!("  x5: {:#018x} (expected: 32)", s.cpu.read_reg(5));
    eprintln!("  x7: {:#018x} (expected: 10)", s.cpu.read_reg(7));
    eprintln!("  x8: {:#018x} (expected: 26)", s.cpu.read_reg(8));
    
    assert_eq!(s.cpu.read_reg(4), 32, "x4 should be 32");
    assert_eq!(s.cpu.read_reg(5), 32, "x5 should be 32");
    assert_eq!(s.cpu.read_reg(7), 10, "x7 should be 10");
    assert_eq!(s.cpu.read_reg(8), 26, "x8 should be 26");
    
    eprintln!("\n✓ All valid RVC instructions work!");
}
