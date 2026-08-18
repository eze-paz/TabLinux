use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege, types::Satp};
use riscv_core::types::Status;

#[test]
fn test_sbi_console_from_s_mode() {
    let data = std::fs::read("/home/aezequiel/riscv-vm/kernels/sbi_test/sbi_test.bin")
        .expect("sbi_test.bin missing");

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, &data);

    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 0,  (0u64 << 10) | 0x0F);
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };
    s.mstatus.spp = true;
    s.mstatus.spie = true;
    s.mstatus.sie = false;
    s.cpu.write_reg(2, 0x81FF_0000);

    for i in 0..200 {
        bus.tick();
        let status = s.step(&mut bus);
        match status {
            Status::Running => {},
            Status::Trap(t) => {
                eprintln!("TRAP step {}: {:?} pc={:#x}", i, t, s.cpu.pc);
                break;
            }
            Status::Wfi => break,
        }
    }

    let console = String::from_utf8_lossy(&s.console_buf[..s.console_len]);
    assert!(s.console_len > 0, "Expected console output");
    assert_eq!(console, "SBI OK!\n");
}

#[test]
fn test_sbi_timer_interrupt() {
    let data = std::fs::read("/home/aezequiel/riscv-vm/kernels/sbi_test/timer_test.bin")
        .expect("timer_test.bin missing");

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, &data);

    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 0,  (0u64 << 10) | 0x0F);
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };
    s.mstatus.spp = true;
    s.mstatus.spie = true;
    s.mstatus.sie = true;
    s.mideleg |= 1 << 7;
    s.cpu.write_reg(2, 0x81FF_0000);

    let mut uart_steps = 0;
    let mut old_uart_len = 0;
    let mut old_sepc = s.sepc;
    let mut timer_steps = 0;

    for i in 0..500 {
        bus.tick();
        let _status = s.step(&mut bus);
        let uart_len = bus.uart_console.len();
        // Count trap entries by checking sepc changes
        if s.sepc != old_sepc {
            old_sepc = s.sepc;
            eprintln!("step {}: sepc -> {:#x} scause={:#x}", i, s.sepc, s.scause);
        }
        if uart_len > old_uart_len {
            old_uart_len = uart_len;
            uart_steps = i;
            break;
        }
    }

    eprintln!("uart_steps={} uart={:?}", uart_steps, String::from_utf8_lossy(&bus.uart_console));
    eprintln!("ecalls={} mtimecmp={} mtime={} scause={:#x}",
        s.ecall_count, bus.read_u64(0x0200_4000), bus.read_u64(0x0200_BFF8), s.scause);

    assert!(uart_steps > 0, "UART should have 'T' from timer trap handler");
    assert_eq!(bus.uart_console[0], b'T', "First UART byte should be 'T'");
}
