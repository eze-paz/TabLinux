use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::Supervisor;

#[test]
fn test_time_csr_advances() {
    let mut bus = DeviceBus::new(1 << 30);
    let mut s = Supervisor::new(0x8000_0000, 0);
    s.priv_level = riscv_supervisor::Privilege::Machine;
    s.cpu.pc = 0x8000_0000;
    
    // Write instruction: csrrs x1, time, x0  (0xC01010F3)
    bus.write_u32(0x8000_0000, 0xC01010F3);
    
    // Step once to execute
    let status = s.step(&mut bus);
    assert!(matches!(status, riscv_core::types::Status::Running), "Execution failed: {:?}", status);
    
    let t0 = s.cpu.x[1];
    eprintln!("First time CSR read: {}", t0);
    
    // Tick bus 100 times
    for _ in 0..100 {
        bus.tick();
    }
    
    // Write instruction again and execute
    bus.write_u32(0x8000_0004, 0xC01010F3);
    s.cpu.pc = 0x8000_0004;
    let status = s.step(&mut bus);
    assert!(matches!(status, riscv_core::types::Status::Running), "Execution failed: {:?}", status);
    
    let t1 = s.cpu.x[1];
    eprintln!("Second time CSR read: {} (delta={})", t1, t1 - t0);
    
    assert!(t1 > t0, "time CSR did not advance! got {} then {}", t0, t1);
}
