//! Alpine boot without initrd — isolate whether BUG_ON is initrd-related

use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1 << 30;

#[test]
fn alpine_no_initrd() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");

    let text_offset = u64::from_le_bytes(kernel[0x08..0x10].try_into().unwrap());
    let kernel_load = DRAM_BASE + text_offset;

    let mut bus = DeviceBus::new(DRAM_SIZE);
    bus.load_blob(kernel_load, &kernel);

    // No initrd — use minimal DTB
    let out = Command::new("python3")
        .arg(format!("{root}/kernels/gen_dtb_v2.py"))
        .arg("0").arg("0")
        .current_dir(format!("{root}/kernels")).output().expect("gen dtb");
    assert!(out.status.success(), "dtb gen: {}", String::from_utf8_lossy(&out.stderr));
    let dtb = std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb");
    let dtb_load = (kernel_load + kernel.len() as u64 + 0x10000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    let mut s = Supervisor::new(kernel_load, 0);
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0);
    s.cpu.write_reg(11, dtb_load);
    s.medeleg = 0xB1FF;
    s.mideleg = 0x222;

    let mut prev_console = 0usize;
    let mut trap_count: u64 = 0;
    let mut same_trap: u32 = 0;
    let mut last_trap_pc: u64 = 0;
    let max_steps: u64 = 50_000_000;
    let mut step: u64 = 0;

    while step < max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        step += 1;

        if s.console_len > prev_console {
            let n = s.console_len.min(s.console_buf.len());
            let text = String::from_utf8_lossy(&s.console_buf[prev_console..n]);
            eprint!("{}", text);
            prev_console = n;
        }

        match status {
            Status::Running => {}
            Status::Wfi => {}
            Status::Trap(t) => {
                trap_count += 1;
                if trap_count <= 12 {
                    eprintln!("[trap {} step {}] {:?} sepc={:#x} scause={:#x} stval={:#x} -> pc={:#x}",
                        trap_count, step, t, s.sepc, s.scause, s.stval, s.cpu.pc);
                }
                if s.cpu.pc == last_trap_pc { same_trap += 1; } else { same_trap = 0; last_trap_pc = s.cpu.pc; }
                if same_trap >= 100 {
                    eprintln!("\n=== TRAP STORM step {} ===", step);
                    break;
                }
            }
        }
        if step % 5_000_000 == 0 {
            eprintln!("[{}M steps] pc={:#x} priv={:?} console={}B traps={}",
                step / 1_000_000, s.cpu.pc, s.priv_level, s.console_len, trap_count);
        }
    }

    let console = String::from_utf8_lossy(&s.console_buf[..s.console_len.min(8192)]);
    eprintln!("\n=== END step={} console={} bytes ===", step, console.len());
    eprintln!("Console: {}", &console[..console.len().min(500)]);
}
