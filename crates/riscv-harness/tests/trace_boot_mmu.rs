//! Trace boot with focus on post-MMU progress
use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1 << 30;

#[test]
fn trace_boot_mmu() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts-decompressed.bin")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");
    let mut bus = DeviceBus::new(DRAM_SIZE);
    bus.load_blob(0x80200000, &kernel);
    let initrd_load = (DRAM_BASE + (DRAM_SIZE as u64) - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;
    let out = Command::new("python3")
        .arg(format!("{root}/kernels/gen_dtb_v2.py"))
        .arg(format!("{initrd_load:#x}")).arg(format!("{initrd_end:#x}"))
        .current_dir(format!("{root}/kernels")).output().expect("gen dtb");
    assert!(out.status.success());
    let dtb = std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb");
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    let mut s = Supervisor::new(0x80200000, 0);
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0);
    s.cpu.write_reg(11, dtb_load);
    s.cpu.write_reg(2, DRAM_BASE + DRAM_SIZE as u64 - 0x10000);
    s.medeleg = 0xB1FF;
    s.mideleg = 0x2A2;

    let mut prev_console = 0usize;
    let max_steps = 10_000_000;
    let mut trap_count = 0u64;
    let mut last_pc = 0u64;
    let mut stall_count = 0u64;

    // Track specific addresses
    let mut mmu_enabled = false;
    let mut ecall_history: Vec<(u64, u64)> = Vec::new(); // (step, a7)

    for step in 0..max_steps {
        bus.tick();
        let status = s.step(&mut bus);

        if s.console_len > prev_console {
            let n = s.console_len.min(s.console_buf.len());
            eprint!("{}", String::from_utf8_lossy(&s.console_buf[prev_console..n]));
            prev_console = n;
        }

        // Track ecalls
        static mut PREV_ECALL_COUNT: u64 = 0;
        unsafe {
            if s.ecall_count > PREV_ECALL_COUNT {
                PREV_ECALL_COUNT = s.ecall_count;
                ecall_history.push((step, s.last_ecall_a7));
            }
        }

        // Detect MMU enable (first InstructionPageFault)
        if !mmu_enabled {
            if let Status::Trap(t) = &status {
                use riscv_core::types::Trap::*;
                use riscv_core::types::Exception::*;
                if matches!(t, Exception(InstructionPageFault)) {
                    mmu_enabled = true;
                    eprintln!("\n[MMU-ENABLE] at step {} pc={:#x} -> stvec={:#x}", step, s.cpu.pc, s.stvec);
                }
            }
        }

        // PC stall detection
        if s.cpu.pc == last_pc {
            stall_count += 1;
            if stall_count == 1000 {
                eprintln!("\n[STALL-1K] step {} pc={:#x} priv={:?} ecalls={} sie={}", 
                    step, s.cpu.pc, s.priv_level, s.ecall_count, s.mstatus.sie);
            }
            if stall_count == 10000 {
                eprintln!("\n[STALL-10K] step {} pc={:#x} - breaking out", step, s.cpu.pc);
                break;
            }
        } else {
            stall_count = 0;
        }
        last_pc = s.cpu.pc;

        match status {
            Status::Running => {}
            Status::Wfi => {
                if (s.mip & s.mie) == 0 {
                    eprintln!("\n[WFI-DEADLOCK] step {} pc={:#x} mip=0x{:x} mie=0x{:x}", step, s.cpu.pc, s.mip, s.mie);
                    break;
                }
            }
            Status::Trap(t) => {
                trap_count += 1;
                if trap_count <= 10 {
                    use riscv_core::types::Trap::*;
                    let detail = match t {
                        Exception(e) => format!("Exception({:?})", e),
                        Interrupt(i) => format!("Interrupt({:?})", i),
                    };
                    eprintln!("\n[TRAP-{} step {}] {} sepc={:#x} scause={:#x} stval={:#x} -> pc={:#x}",
                        trap_count, step, detail, s.sepc, s.scause, s.stval, s.cpu.pc);
                    if trap_count == 2 {
                        // Dump registers around the trap
                        eprintln!("  a0={:#x} a1={:#x} a2={:#x} a3={:#x}", 
                            s.cpu.read_reg(10), s.cpu.read_reg(11), s.cpu.read_reg(12), s.cpu.read_reg(13));
                        eprintln!("  s0={:#x} s1={:#x} s2={:#x} s3={:#x}", 
                            s.cpu.read_reg(8), s.cpu.read_reg(9), s.cpu.read_reg(18), s.cpu.read_reg(19));
                        eprintln!("  tp={:#x} sp={:#x} gp={:#x}", s.cpu.read_reg(4), s.cpu.read_reg(2), s.cpu.read_reg(3));
                    }
                }
                if trap_count > 100 {
                    eprintln!("\n[TRAP-STORM] step {} - {} traps", step, trap_count);
                    break;
                }
            }
        }
        if step % 2_000_000 == 0 && step > 0 {
            eprintln!("\n[{}M] pc={:#x} ecalls={} traps={} sie={} console={}B", 
                step/1_000_000, s.cpu.pc, s.ecall_count, trap_count, s.mstatus.sie, s.console_len);
        }
    }

    eprintln!("\n=== SUMMARY ===");
    eprintln!("steps={} ecalls={} traps={} console={}B", max_steps, s.ecall_count, trap_count, s.console_len);
    eprintln!("ecall_history: {:?}", ecall_history);
    eprintln!("pc={:#x} priv={:?}", s.cpu.pc, s.priv_level);
    let console = String::from_utf8_lossy(&s.console_buf[..s.console_len.min(2000)]);
    eprintln!("Console: {:?}", console);
}
