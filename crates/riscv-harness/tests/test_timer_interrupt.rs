use riscv_core::execute::Bus;
use riscv_core::types::{Status, Trap, Interrupt};
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn test_timer_interrupt_delivery() {
    let mut bus = DeviceBus::new(1 << 30);
    let mut s = Supervisor::new(0x8000_0000, 0);
    s.priv_level = Privilege::Supervisor;
    
    // Write jal x0, 0 (infinite loop) at PC so CPU spins until interrupt fires.
    bus.write_u32(0x8000_0000, 0x0000_006f);
    
    // Enable timer interrupt
    s.mie |= 1 << 7;   // MTIE
    s.mstatus.sie = true;
    s.mie |= 1 << 5;   // STIE
    s.mideleg |= 1 << 7; // Delegate MTIP to S-mode
    
    // Set mtimecmp to 100
    bus.write_u64(0x0200_4000, 100);
    bus.write_u64(0x0200_4008, 0);
    
    let mut trap_found = false;
    for step in 0..200 {
        bus.tick();
        if bus.check_timer_interrupt() {
            s.mip |= 1 << 7;
        } else {
            s.mip &= !(1 << 7);
        }
        
        if step >= 98 && step <= 102 {
            eprintln!("step={} mtime={} mtimecmp={} mip={:#x} mie={:#x} mideleg={:#x} pc={:#x} sie={}",
                step, bus.get_mtime(), bus.read_mtime(), s.mip, s.mie, s.mideleg, s.cpu.pc, s.mstatus.sie);
        }
        
        let status = s.step(&mut bus);
        match status {
            Status::Trap(Trap::Interrupt(Interrupt::SupervisorTimer)) => {
                eprintln!("SupervisorTimer trap at step {} pc={:#x}", step, s.cpu.pc);
                trap_found = true;
                break;
            }
            Status::Trap(t) => {
                eprintln!("Unexpected trap at step {} pc={:#x}: {:?}", step, s.cpu.pc, t);
                break;
            }
            _ => {}
        }
    }
    
    assert!(trap_found, "SupervisorTimer interrupt was never delivered! mtime={} mtimecmp={} mip={:#x} mie={:#x} mideleg={:#x} pc={:#x}",
            bus.get_mtime(), bus.read_mtime(), s.mip, s.mie, s.mideleg, s.cpu.pc);
}
