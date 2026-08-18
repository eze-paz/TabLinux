//! Boot actual userspace in U-mode

use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn boot_userspace() {
    let mut bus = DeviceBus::new(1 << 20);
    
    // RISC-V machine code (little-endian):
    // ADDI x10, x0, 1: 0x00100513
    // ADDI x11, x0, 2: 0x00200593
    // ADD x12, x10, x11: 0x00b50633
    // JALR x0, 0(ra): 0x00008067
    
    let user_code: &[u8] = &[
        0x13, 0x05, 0x10, 0x00,  // addi x10, x0, 1
        0x93, 0x05, 0x20, 0x00,  // addi x11, x0, 2
        0x33, 0x06, 0xb5, 0x00,  // add x12, x10, x11
        0x67, 0x80, 0x00, 0x00,  // jalr x0, 0(ra)
    ];
    
    let user_code_addr = 0x8000_0000u64;
    bus.load_blob(user_code_addr, user_code);
    
    eprintln!("=== Userspace boot test ===");
    
    let mut s = Supervisor::new(0x8000_0000u64, 0);
    s.priv_level = Privilege::User;
    s.mstatus.mpp = 0;
    s.mstatus.mpie = true;
    s.mstatus.mie = true;
    s.mstatus.mprv = true;
    
    // Set ra (x1) to point past the program so jalr returns to "done"
    s.cpu.write_reg(1, 0x8000_0010u64);
    
    let trap_addr = 0x8000_1000u64;
    s.stvec = trap_addr;
    
    eprintln!("Initial PC: {:#x}", s.cpu.pc);
    
    let mut steps = 0u64;
    let max_steps = 100;
    while steps < max_steps {
        s.step(&mut bus);
        steps += 1;
        
        eprintln!("Step {}: PC={:#x}", steps, s.cpu.pc);
        
        if s.cpu.pc >= 0x8000_0018 {
            eprintln!("\nUserspace completed execution");
            eprintln!("Final registers:");
            eprintln!("  x10: {:#010x} (expected: 1)", s.cpu.read_reg(10));
            eprintln!("  x11: {:#010x} (expected: 2)", s.cpu.read_reg(11));
            eprintln!("  x12: {:#010x} (expected: 3)", s.cpu.read_reg(12));
            
            assert_eq!(s.cpu.read_reg(10), 1, "x10 should be 1");
            assert_eq!(s.cpu.read_reg(11), 2, "x11 should be 2");
            assert_eq!(s.cpu.read_reg(12), 3, "x12 should be 3");
            
            eprintln!("All userspace assertions passed!");
            return;
        }
    }
    
    panic!("Userspace did not complete in {} steps", max_steps);
}
