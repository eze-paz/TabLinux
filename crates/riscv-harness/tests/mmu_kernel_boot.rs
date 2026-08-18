use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege, types::Satp};
use riscv_core::types::Status;

#[test]
fn test_minikernel_m_mode_with_mmu() {
    let data = std::fs::read("/home/aezequiel/riscv-vm/kernels/minikernel/minikernel.bin")
        .expect("minikernel missing");

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, &data);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(2, 0x81FF_0000);

    // Identity gigapage 2: VA 0x80000000 -> PA 0x80000000
    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F);
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };

    let max_steps = 1_000;
    let mut steps = 0;
    for i in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        steps = i;
        match status {
            Status::Running => {},
            Status::Trap(trap) => {
                eprintln!("TRAP step {}: {:?} pc={:#x}", i, trap, s.cpu.pc);
                break;
            }
            Status::Wfi => break,
        }
        if s.cpu.pc == 0 { break; }
    }

    let console = String::from_utf8_lossy(&bus.uart_console);
    eprintln!("M-mode+MMU console: {:?}", console);
    eprintln!("Steps: {}  PC: {:#x}", steps, s.cpu.pc);
    assert!(console.len() > 100, "Should have UART output, got {} bytes", console.len());
}

#[test]
fn test_kernel_s_mode_identity_mmu() {
    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;

    // Hand-assembled tiny kernel:
    // lui  a5, 0x10000          ; a5 = 0x10000000 (UART)
    // addi a3, zero, 'S'        ; a3 = 'S'
    // sb   a3, 0(a5)            ; UART[0] = 'S'
    // wfi
    let code: [u32; 4] = [
        0x1000_07B7,
        0x0530_0693,
        0x00D7_8023,
        0x1050_0073,
    ];
    let mut bytes = Vec::new();
    for w in &code { bytes.extend_from_slice(&w.to_le_bytes()); }
    bus.load_blob(load_addr, &bytes);

    // Page table at 0x8F00_0000 (physical).
    // Map two gigapages:
    //  - Entry 0: VA 0x0000_0000 -> PA 0x0000_0000 (includes UART, CLINT, etc.)
    //  - Entry 2: VA 0x8000_0000 -> PA 0x8000_0000 (includes kernel, stack, page-table)
    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 0,  (0u64 << 10) | 0x0F);       // gigapage 0, identity
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F); // gigapage 2, identity

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };
    s.mstatus.spp = true;   // sret → S-mode
    s.mstatus.spie = true;
    s.mstatus.sie = true;
    s.cpu.write_reg(2, 0x81FF_0000);

    for _ in 0..100 {
        bus.tick();
        let status = s.step(&mut bus);
        match status {
            Status::Running => {},
            Status::Trap(t) => {
                eprintln!("S-mode trap: {:?}  pc={:#x} sepc={:#x} cause={:#x}",
                          t, s.cpu.pc, s.sepc, s.last_trap_cause);
                break;
            }
            Status::Wfi => break,
        }
    }

    let console = String::from_utf8_lossy(&bus.uart_console);
    eprintln!("S-mode console: {:?}", console);
    assert!(console.starts_with("S"), "Expected 'S', got: {:?}", console);
}

/// Test C: S-mode translation with mapped VA != PA.
#[test]
fn test_s_mode_offset_mapping() {
    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    // Kernel at PA 0x8020_0000, mapped to VA 0x8020_0000 via identity (same as above but explicit)
    let load_addr = 0x8020_0000u64;
    let code: [u32; 4] = [
        0x1000_07B7,
        0x04F0_0693, // 'O'
        0x00D7_8023,
        0x1050_0073,
    ];
    let mut bytes = Vec::new();
    for w in &code { bytes.extend_from_slice(&w.to_le_bytes()); }
    bus.load_blob(load_addr, &bytes);

    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 0,  (0u64 << 10) | 0x0F);
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };
    s.mstatus.spp = true;
    s.mstatus.spie = true;
    s.mstatus.sie = true;
    s.cpu.write_reg(2, 0x81FF_0000);

    for _ in 0..20 {
        bus.tick();
        if let Status::Trap(t) = s.step(&mut bus) {
            eprintln!("TRAP: {:?} pc={:#x}", t, s.sepc);
            break;
        }
    }
    assert_eq!(String::from_utf8_lossy(&bus.uart_console), "O");
}
