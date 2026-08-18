//! Diagnostic: trace last 1000 steps before Alpine trap storm

use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1 << 30;

#[test]
fn alpine_trap_diag() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");

    let text_offset = u64::from_le_bytes(kernel[0x08..0x10].try_into().unwrap());
    let kernel_load = DRAM_BASE + text_offset;

    let mut bus = DeviceBus::new(DRAM_SIZE);
    bus.load_blob(kernel_load, &kernel);

    let initrd_load = (DRAM_BASE + (DRAM_SIZE as u64) - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;

    let out = Command::new("python3")
        .arg(format!("{root}/kernels/gen_dtb_v2.py"))
        .arg(format!("{initrd_load:#x}")).arg(format!("{initrd_end:#x}"))
        .current_dir(format!("{root}/kernels")).output().expect("gen dtb");
    assert!(out.status.success(), "dtb gen: {}", String::from_utf8_lossy(&out.stderr));
    let dtb = std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb");
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    let mut s = Supervisor::new(kernel_load, 0);
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0);
    s.cpu.write_reg(11, dtb_load);
    s.medeleg = 0xB1FF;
    s.mideleg = 0x222;

    // Trace buffer: last N steps
    const TRACE_LEN: usize = 2000;
    let mut trace_pc: [u64; TRACE_LEN] = [0; TRACE_LEN];
    let mut trace_raw: [u32; TRACE_LEN] = [0; TRACE_LEN];
    let mut trace_step: [u64; TRACE_LEN] = [0; TRACE_LEN];
    let mut trace_idx: usize = 0;

    let mut trap_count: u64 = 0;
    let mut same_trap: u32 = 0;
    let mut last_trap_pc: u64 = 0;
    let max_steps: u64 = 200_000_000;
    let mut step: u64 = 0;

    while step < max_steps {
        bus.tick();
        let status = s.step(&mut bus);
        step += 1;

        // Record trace
        trace_pc[trace_idx] = s.cpu.pc;
        trace_raw[trace_idx] = s.last_fetched_raw;
        trace_step[trace_idx] = step;
        trace_idx = (trace_idx + 1) % TRACE_LEN;

        match status {
            Status::Running => {}
            Status::Wfi => {}
            Status::Trap(t) => {
                trap_count += 1;
                if s.cpu.pc == last_trap_pc { same_trap += 1; } else { same_trap = 0; last_trap_pc = s.cpu.pc; }
                if same_trap >= 100 {
                    eprintln!("\n=== TRAP STORM at step {} ===", step);
                    eprintln!("trap: {:?}", t);
                    eprintln!("pc={:#x} priv={:?}", s.cpu.pc, s.priv_level);
                    eprintln!("sepc={:#x} scause={:#x} stval={:#x} stvec={:#x}", s.sepc, s.scause, s.stval, s.stvec);
                    eprintln!("satp={:#x}", s.satp.to_bits());
                    
                    // Dump last 200 trace entries
                    eprintln!("\n=== LAST 200 STEPS ===");
                    for i in 0..200 {
                        let idx = (trace_idx + TRACE_LEN - 200 + i) % TRACE_LEN;
                        if trace_step[idx] == 0 { continue; }
                        let raw = trace_raw[idx];
                        let q = raw & 0b11;
                        let width = if q == 0b11 { 4 } else { 2 };
                        eprintln!("step {:>10} pc={:#018x} raw={:0>width$} (q={:02b})", 
                            trace_step[idx], trace_pc[idx], raw, q, width = if width == 4 { 8 } else { 4 });
                    }
                    
                    // Also dump a few steps before the first trap
                    eprintln!("\n=== FIRST TRAP context ===");
                    eprintln!("trap_count={} last_trap_pc={:#x}", trap_count, last_trap_pc);
                    break;
                }
            }
        }
    }

    eprintln!("\n=== END step={} ===", step);
    assert!(step < max_steps, "Did not hit trap storm");
}
