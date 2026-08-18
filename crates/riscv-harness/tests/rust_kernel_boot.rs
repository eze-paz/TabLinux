use riscv_devices::DeviceBus;
use riscv_supervisor::Supervisor;
use riscv_core::types::Status;

#[test]
fn test_rust_kernel_boot() {
    let data = include_bytes!("../../../crates/kernel_test/kernel_test.bin");
    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, data);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = riscv_supervisor::types::Privilege::Supervisor;
    s.cpu.write_reg(2, 0x81FF_0000);
    s.mie |= 1 << 7;
    s.mideleg |= 1 << 7;
    s.medeleg = 0xFFFF;

    let mut timer_fired = false;
    for i in 0..200 {
        bus.tick();
        let status = s.step(&mut bus);
        if bus.uart_console.contains(&b'T') {
            timer_fired = true;
            println!("TIMER FIRED step {}", i);
            break;
        }
        if i < 10 || (i > 40 && i < 65) {
            println!("step {:03}: pc={:08x} mtime={} mie={:08x} mip={:08x} status={:?}",
                i, s.cpu.pc, bus.get_mtime(), s.mie, s.mip, status);
        }
        match status {
            Status::Running => {}
            Status::Trap(t) => {
                println!("TRAP step {}: {:?} pc={:08x}", i, t, s.cpu.pc);
            }
            Status::Wfi => {}
        }
    }
    println!("uart={:?}", core::str::from_utf8(bus.get_uart_console()).unwrap_or(""));
    assert!(timer_fired, "Timer should fire and write 'T' to UART");
}
