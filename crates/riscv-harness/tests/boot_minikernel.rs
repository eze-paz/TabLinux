use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

#[test]
fn boot_minikernel() {
    let kernel = std::fs::read(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../kernels/minikernel/minikernel.bin")
    ).expect("minikernel.bin missing");

    let mut bus = DeviceBus::new(1 << 30);
    bus.load_blob(0x8020_0000u64, &kernel);

    let mut s = Supervisor::new(0x8020_0000u64, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(10, 0);
    s.cpu.write_reg(11, 0);

    let mut uart_buf = Vec::new();
    let max_steps = 100_000;

    for step in 0..max_steps {
        bus.tick();
        if bus.check_timer_interrupt() { s.mip |= 1 << 7; }

        let status = s.step(&mut bus);

        // Capture UART output
        if bus.uart_console.len() > uart_buf.len() {
            uart_buf.extend_from_slice(&bus.uart_console[uart_buf.len()..]);
        }

        match status {
            Status::Running => {}
            Status::Trap(t) => {
                eprintln!("[TRAP at step {} pc={:#x}: {:?}]", step, s.cpu.pc, t);
                break;
            }
            Status::Wfi => {
                if (s.mip & s.mie) == 0 {
                    eprintln!("[WFI at step {}]", step);
                    break;
                }
            }
        }
    }

    let text = String::from_utf8_lossy(&uart_buf);
    eprintln!("UART output: {:?}", text);
    assert!(text.contains("MiniRV"), "Expected 'MiniRV', got: {:?}", text);
}
