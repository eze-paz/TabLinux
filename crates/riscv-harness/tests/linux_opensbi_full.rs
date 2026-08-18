use riscv_core::types::Status;
use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};

/// Boot Linux through OpenSBI fw_payload.bin (which includes a test payload)
/// We need fw_dynamic.bin or fw_jump.bin with kernel as payload
#[test]
fn test_opensbi_with_linux_payload() {
    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let fw_path = std::path::Path::new(manifest_dir)
        .join("../../opensbi/opensbi/lp64/generic/firmware/fw_payload.bin");
    let fw = std::fs::read(&fw_path)
        .expect(&format!("OpenSBI not found at {:?}", fw_path));

    let mut bus = DeviceBus::new(1 << 30);
    bus.load_blob(0x8000_0000u64, &fw);

    let mut s = Supervisor::new(0x8000_0000u64, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(11, 0); // a1 = dtb pointer (null for fw_payload built-in)

    let mut last_pc = 0u64;
    let mut pc_stall = 0;
    let max_steps = 5_000_000;

    for i in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        match status {
            Status::Running => {
                if s.cpu.pc == last_pc {
                    pc_stall += 1;
                    if pc_stall > 100 {
                        eprintln!("PC stalled at {:#x} for {} steps", s.cpu.pc, pc_stall);
                        break;
                    }
                } else {
                    pc_stall = 0;
                    last_pc = s.cpu.pc;
                }
            }
            Status::Trap(t) => {
                eprintln!("TRAP at step {}: {:?} pc={:#x} priv={:?}", i, t, s.cpu.pc, s.priv_level);
                eprintln!("  a0={:#x} a1={:#x} a2={:#x}", s.cpu.read_reg(10), s.cpu.read_reg(11), s.cpu.read_reg(12));
                break;
            }
            Status::Wfi => {
                if (s.mip & s.mie) == 0 {
                    eprintln!("WFI at step {} (no pending interrupts)", i);
                    break;
                }
            }
        }
        if i > 0 && i % 500_000 == 0 {
            eprintln!("[step {:>8}] pc={:#018x} priv={:?} uart={}",
                      i, s.cpu.pc, s.priv_level, bus.uart_console.len());
        }
    }

    eprintln!("\nFinal PC: {:#x}  UART: {:?}", s.cpu.pc, 
              String::from_utf8_lossy(&bus.uart_console));
}
