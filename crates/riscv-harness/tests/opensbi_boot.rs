use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use riscv_core::types::Status;

/// Boot OpenSBI fw_payload.bin which includes a built-in test payload.
#[test]
fn test_opensbi_fw_payload_boot() {
    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let fw_path = std::path::Path::new(manifest_dir)
        .join("../../opensbi/opensbi/lp64/generic/firmware/fw_payload.bin");
    let fw = std::fs::read(&fw_path)
        .expect(&format!("OpenSBI fw_payload.bin not found at {:?}", fw_path));

    eprintln!("OpenSBI fw_payload.bin size: {} bytes ({}K)", fw.len(), fw.len()/1024);

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8000_0000u64;
    bus.load_blob(load_addr, &fw);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(11, 0);

    let mut last_pc = 0u64;
    let mut pc_stall = 0;
    let max_steps = 30_000;
    for i in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        match status {
            Status::Running => {
                if s.cpu.pc == last_pc {
                    pc_stall += 1;
                    if pc_stall > 5 {
                        eprintln!("PC stalled at {:#x} from steps {}-{}",
                                  s.cpu.pc, i - pc_stall, i);
                        break;
                    }
                } else {
                    pc_stall = 0;
                    last_pc = s.cpu.pc;
                }
            }
            Status::Trap(trap) => {
                if i > 500 {
                    eprintln!("TRAP at step {}: {:?}", i, trap);
                    eprintln!("  pc={:#x} a0={:#x} a1={:#x}",
                              s.cpu.pc, s.cpu.read_reg(10), s.cpu.read_reg(11));
                    break;
                }
            }
            Status::Wfi => {
                eprintln!("WFI at step {}", i);
                break;
            }
        }
        if i < 25 || (i > 5000 && i % 500 == 0) || (i > 9500 && i % 100 == 0) {
            eprintln!("step {}: PC={:#x} a0={:#x} a1={:#x} a2={:#x} sp={:#x}",
                      i, s.cpu.pc, s.cpu.read_reg(10), s.cpu.read_reg(11),
                      s.cpu.read_reg(12), s.cpu.read_reg(2));
        }
    }

    eprintln!("Final PC: {:#x}", s.cpu.pc);
    eprintln!("UART console: {:?}", String::from_utf8_lossy(&bus.uart_console));
}

/// Boot OpenSBI fw_jump.bin with our own payload at 0x80200000
#[test]
fn test_opensbi_fw_jump_boot() {
    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let fw_path = std::path::Path::new(manifest_dir)
        .join("../../opensbi/opensbi/lp64/generic/firmware/fw_jump.bin");
    let fw = std::fs::read(&fw_path)
        .expect(&format!("OpenSBI fw_jump.bin not found at {:?}", fw_path));

    eprintln!("OpenSBI fw_jump.bin size: {} bytes ({}K)", fw.len(), fw.len()/1024);

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8000_0000u64;
    bus.load_blob(load_addr, &fw);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Machine;
    s.mstatus.mpp = 3;
    s.cpu.write_reg(11, 0);

    let mut last_pc = 0u64;
    let mut pc_stall = 0;
    let max_steps = 10_000;
    for i in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        match status {
            Status::Running => {
                if s.cpu.pc == last_pc {
                    pc_stall += 1;
                    if pc_stall > 5 {
                        eprintln!("PC stalled at {:#x}", s.cpu.pc);
                        break;
                    }
                } else {
                    pc_stall = 0;
                    last_pc = s.cpu.pc;
                }
            }
            Status::Trap(trap) => {
                eprintln!("TRAP at step {}: {:?}", i, trap);
                break;
            }
            Status::Wfi => {
                eprintln!("WFI at step {}", i);
                break;
            }
        }
    }
    eprintln!("Final PC: {:#x}", s.cpu.pc);
    eprintln!("UART: {:?}", String::from_utf8_lossy(&bus.uart_console));
}
