use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn debug_opensbi_boot() {
    let fw_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../opensbi/opensbi/lp64/generic/firmware/fw_jump.bin");
    let fw = std::fs::read(fw_path).expect("fw_jump.bin not found");
    
    let mut bus = DeviceBus::new(1 << 30);
    bus.load_blob(0x8000_0000u64, &fw);
    
    let mut s = Supervisor::new(0x8000_0000u64, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(10, 0);
    s.cpu.write_reg(11, 0);
    
    let mut prev_pc = 0u64;
    for step in 0..15000 {
        bus.tick();
        let status = s.step(&mut bus);
        
        // Print last 20 steps before WFI
        if step > 9460 || (s.cpu.pc != prev_pc && step % 100 == 0) {
            eprintln!("step {:5}: pc=0x{:08x} priv={:?} mstatus=0x{:016x} mie=0x{:x} mip=0x{:x}",
                      step, s.cpu.pc, s.priv_level, s.mstatus.to_bits(), s.mie, s.mip);
            prev_pc = s.cpu.pc;
        }
        
        match status {
            Status::Running => {}
            Status::Trap(t) => {
                eprintln!("TRAP at step {}: {:?}", step, t);
                break;
            }
            Status::Wfi => {
                eprintln!("WFI at step {} pc=0x{:x} mip=0x{:x} mie=0x{:x}", 
                          step, s.cpu.pc, s.mip, s.mie);
                if (s.mip & s.mie) == 0 {
                    eprintln!("  -> No pending interrupts, deadlock!");
                    break;
                }
            }
        }
    }
    
    eprintln!("Console: {:?}", String::from_utf8_lossy(&bus.uart_console));
}
