use riscv_devices::DeviceBus;
use riscv_core::execute::Bus;
use riscv_supervisor::{Supervisor, types::Privilege};
use riscv_core::types::Status;

#[test]
fn test_minikernel_boot() {
    let data = std::fs::read("/home/aezequiel/riscv-vm/kernels/minikernel/minikernel.bin")
        .expect("minikernel missing");

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, &data);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(2, 0x81FF_0000);

    let max_steps = 1_000;
    let mut steps = 0;
    for i in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        steps = i;
        match status {
            Status::Running => {},
            Status::Trap(trap) => {
                println!("TRAP step {}: {:?} pc={:#x}", i, trap, s.cpu.pc);
                break;
            }
            Status::Wfi => {
                println!("WFI at step {} pc={:#x}", i, s.cpu.pc);
                break;
            }
        }
        if s.cpu.pc == 0 {
            println!("PC hit zero at step {}", i);
            break;
        }
    }

    let console = String::from_utf8_lossy(&bus.uart_console);
    println!("UART console: {:?}", console);
    println!("Steps: {}", steps);
    println!("PC: {:#x}", s.cpu.pc);

    // Should have emitted many 'A's via UART
    assert!(console.len() > 100, "Should have UART output, got {} bytes", console.len());
    assert!(console.starts_with("A"), "Should start with 'A', got: {:?}", console);
    assert!(s.cpu.pc == 0x80200014 || s.cpu.pc == 0x80200010, "Should loop at sb instruction");
}
