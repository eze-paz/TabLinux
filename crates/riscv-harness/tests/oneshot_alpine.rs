//! One clean Alpine boot attempt: flat vmlinuz-lts.raw + initramfs + generated DTB,
//! direct S-mode entry (a0=hartid, a1=dtb) per the RISC-V Linux boot protocol.
//! On any fatal trap, dumps enough state to diagnose against objdump.

use riscv_core::execute::Bus;
use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1 << 30; // 1GB — must match gen_dtb_v2 memory node

#[test]
fn oneshot_alpine() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");

    let text_offset = u64::from_le_bytes(kernel[0x08..0x10].try_into().unwrap());
    let kernel_load = DRAM_BASE + text_offset;

    let mut bus = DeviceBus::new(DRAM_SIZE);
    bus.load_blob(kernel_load, &kernel);
    // Patch setup_smp BUG: c.ebreak (0x9002) -> c.nop (0x0001) at VA 0xffffffff80a058c2
    let bug_dram_offset = (kernel_load - DRAM_BASE) as usize + 0xa058c2;
    let bug_dram = bus.get_dram_mut();
    bug_dram[bug_dram_offset..bug_dram_offset+2].copy_from_slice(&[0x01, 0x00]);

    // initrd near the top of RAM, 64K aligned, clear of the kernel's runtime footprint
    let initrd_load = (DRAM_BASE + (DRAM_SIZE as u64) - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;

    // DTB just below the initrd
    let out = Command::new("python3")
        .arg(format!("{root}/kernels/gen_dtb_v2.py"))
        .arg(format!("{initrd_load:#x}")).arg(format!("{initrd_end:#x}"))
        .current_dir(format!("{root}/kernels")).output().expect("gen dtb");
    assert!(out.status.success(), "dtb gen: {}", String::from_utf8_lossy(&out.stderr));
    let dtb = std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb");
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    eprintln!("kernel {:#x}+{:#x}  initrd {:#x}..{:#x}  dtb {:#x}+{:#x}",
        kernel_load, kernel.len(), initrd_load, initrd_end, dtb_load, dtb.len());

    // Check static key at VA 0xffffffff81230020
    // DRAM offset: physical = kernel_load + (VA - PAGE_OFFSET), then - DRAM_BASE
    let static_key_dram_off = (kernel_load - DRAM_BASE) as usize + (0xffffffff81230020usize - 0xffffffff80000000usize);
    let static_key_val = { let d = bus.get_dram(); let off = static_key_dram_off; u64::from_le_bytes(d[off..off+8].try_into().unwrap()) };
    eprintln!("[INIT] static_key(0x81230020) at DRAM+0x{:x} = 0x{:016x}",
        static_key_dram_off, static_key_val);

    // Patch the stub function at 0x80221a04 to return mcycle instead of 0
    // Replace: li a0,0(2b); c.addi sp,16(2b); c.jr ra(2b)
    // With: csrr a0,mcycle(4b); c.addi sp,16(2b); c.jr ra(2b)
    let stub_fn_file_off = 0x221a04usize;
    let stub_fn_dram_off = (kernel_load - DRAM_BASE) as usize + stub_fn_file_off;
    let dram = bus.get_dram_mut();
    let patch: [u8; 8] = [0x73, 0x25, 0x00, 0xb0, // csrr a0, mcycle
                           0x41, 0x01,             // c.addi sp, 16
                           0x82, 0x80];            // c.jr ra
    dram[stub_fn_dram_off + 10..stub_fn_dram_off + 18].copy_from_slice(&patch);
    eprintln!("[INIT] Patched stub at 0x80221a04+10 -> returns mcycle");

    let mut s = Supervisor::new(kernel_load, 0);
    // This test exists to dump the path into a fault, and the pc ring and the
    // unique-PC set are that dump. They are off by default because they cost
    // ~9% of a boot; here the whole point is to pay it.
    s.trace_enabled = true;
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0);        // a0 = hartid
    s.cpu.write_reg(11, dtb_load); // a1 = dtb
    s.cpu.write_reg(2, DRAM_BASE + DRAM_SIZE as u64 - 0x10000); // sp = top of DRAM - 64K
    s.medeleg = 0xB1FF;
    s.mideleg = 0x2A2; // 0x222 | 0x80 (delegate MTIP to S-mode)


    eprintln!("[ENTRY regs] a0(hartid)={:#x} a1(dtb)={:#x} a2(sp)={:#x} a5={:#x} pc={:#x} mhartid={}",
        s.cpu.read_reg(10), s.cpu.read_reg(11), s.cpu.read_reg(2), s.cpu.read_reg(15), s.cpu.pc, s.mhartid);

    // Dump entry bytes + boot_cpu_hartid global to settle the entry-point question
    let entry_bytes = { let d = bus.get_dram(); let base = (s.cpu.pc - DRAM_BASE) as usize; d[base..base+8].to_vec() };
    eprintln!("[ENTRY bytes @pc={:#x}] {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
        s.cpu.pc, entry_bytes[0],entry_bytes[1],entry_bytes[2],entry_bytes[3],entry_bytes[4],entry_bytes[5],entry_bytes[6],entry_bytes[7]);
    let bch_phys = kernel_load + 0x1558078u64; // boot_cpu_hartid (VA 0xffffffff81558078) file offset 0x1358078
    let bch = { let d = bus.get_dram(); let o = (bch_phys - DRAM_BASE) as usize; u64::from_le_bytes(d[o..o+8].try_into().unwrap()) };
    eprintln!("[ENTRY boot_cpu_hartid @phys {:#x}] = {:#x}", bch_phys, bch);
    let mut console = String::new();
    let mut prev_console = 0usize;
    let mut prev_uart = 0usize;
    let mut trap_count: u64 = 0;
    let mut prev_ecall_count: u64 = 0;
    let mut prev_ecall_a7: u64 = 0;
    let mut same_trap: u32 = 0;
    let dump_pt = |bus: &mut dyn Bus, vaddr: u64, satp_ppn: u64| {
        let root = satp_ppn << 12;
        eprintln!("  PT walk vaddr={:#018x} root_ppn={:#x} root_pa={:#x}", vaddr, satp_ppn, root);
        let vpn = [(vaddr >> 12) & 0x1FF, (vaddr >> 21) & 0x1FF, (vaddr >> 30) & 0x1FF];
        let mut table = root;
        for level in (0..=2).rev() {
            let pte_addr = table + (vpn[level] * 8) as u64;
            let pte = bus.read_u64(pte_addr);
            let ppn = (pte >> 10) & 0xFFFF_FFFF_FFFF;
            let v = pte & 1; let r = (pte >> 1) & 1; let w = (pte >> 2) & 1; let x = (pte >> 3) & 1;
            eprintln!("    L{} pte_addr={:#018x} pte={:#018x} ppn={:#x} V={} R={} W={} X={}", level, pte_addr, pte, ppn, v, r, w, x);
            if v == 0 { eprintln!("    -> NOT VALID, walk stops"); break; }
            if level == 0 { eprintln!("    -> leaf L0"); break; }
            if (pte & 0xE) == 0 { table = ppn << 12; } else {
                let shift = if level == 2 { 30 } else { 21 };
                let pa = (ppn << 12) | (vaddr & ((1u64 << shift) - 1));
                eprintln!("    -> leaf L{}, paddr={:#018x}", level, pa);
                break;
            }
        }
    };
    let dump_l1 = |bus: &mut dyn Bus, root_ppn: u64, label: &str| {
        let root = root_ppn << 12;
        let mut count = 0u64;
        for l2i in 0..512u64 {
            let l2a = root + l2i * 8;
            let l2 = bus.read_u64(l2a);
            if l2 & 1 == 0 { continue; }
            let r = (l2 >> 1) & 1; let w = (l2 >> 2) & 1; let x = (l2 >> 3) & 1;
            if r == 0 && w == 0 && x == 0 {
                let l1root = ((l2 >> 10) & 0xFFFF_FFFF_FFFF) << 12;
                for l1i in 0..512u64 {
                    let l1a = l1root + l1i * 8;
                    let l1 = bus.read_u64(l1a);
                    if l1 & 1 != 0 {
                        count += 1;
                        if count <= 24 { eprintln!("    [{}] L2#{} L1#{} pte={:#018x} ppn={:#x}", label, l2i, l1i, l1, (l1 >> 10) & 0xFFFF_FFFF_FFFF); }
                    }
                }
            } else {
                count += 1;
                if count <= 24 { eprintln!("    [{}] L2#{} LEAF pte={:#018x}", label, l2i, l2); }
            }
        }
        eprintln!("    [{}] total valid leaf PTEs = {}", label, count);
    };


    let mut last_trap_pc: u64 = 0;
    // Capture the moment the boot hart's pc collapses from a full 64-bit kernel VA
    // to a 32-bit value (the corruption that later lands it in the trampoline).
    let mut epg_ring: Vec<(u64, u64, u64)> = Vec::with_capacity(256);
    let mut epg_hit: bool = false;
    let mut last_pc: u64 = 0;
    const PC_RING_CAP: usize = 3_000_000;
    const PRE_PC_CAP: usize = 1_500_000;
    let mut pre_pc_ring: Vec<(u64, u32)> = Vec::with_capacity(PRE_PC_CAP);
    let mut pre_pc_head: usize = 0;
    let mut boot_pc_hist: Vec<(u64, u64)> = Vec::with_capacity(PC_RING_CAP);
    let mut boot_pc_head: usize = 0;
    let mut low_entry: u32 = 0;
    let mut was_in_low: bool = false;
    let mut prev_pre_low: bool = false;
    let max_steps: u64 = 30_000_000;
    let mut step: u64 = 0;
    while step < max_steps {
        let pre_pc = s.cpu.pc;
        bus.tick();
        let status = s.step(&mut bus);
        step += 1;
        if pre_pc_ring.len() < PRE_PC_CAP {
            pre_pc_ring.push((pre_pc, s.last_fetched_raw));
        } else {
            pre_pc_ring[pre_pc_head] = (pre_pc, s.last_fetched_raw);
            pre_pc_head = (pre_pc_head + 1) % PRE_PC_CAP;
        }
        prev_pre_low = pre_pc >= 0x8000_0000u64 && pre_pc < 0x8200_0000u64;
        if prev_console == 0 && s.console_len > 0 {
            eprintln!("[console starts at step {}] pc={:#018x} satp={:#x} mmu_on={}", step, s.cpu.pc, s.satp.to_bits(), s.satp.mode == 8);
        }
        if step % 1000000 == 0 {
            eprintln!("[step {}] pc={:#018x} satp={:#x} mmu_on={} in_low={}", step, s.cpu.pc, s.satp.to_bits(), s.satp.mode == 8, (s.cpu.pc >= 0x8000_0000u64 && s.cpu.pc < 0x8200_0000u64));
        }

        // Detect pc truncation: MMU on, prev pc was a full 64-bit kernel VA,
        // current pc collapsed to a 32-bit value (high bits lost).
        let cur_pc = s.cpu.pc;
        epg_ring.push((cur_pc, s.last_fetch_paddr, s.last_fetched_raw as u64));
        if epg_ring.len() > 256 { epg_ring.remove(0); }
        if !epg_hit && s.satp.mode == 8 && last_pc >= 0xffff_ffff_8000_0000u64
            && cur_pc >= 0x8000_0000u64 && cur_pc < 0x1_0000_0000u64 {
            epg_hit = true;
            eprintln!("\n=== PC TRUNCATED at step {}: {:#018x} -> {:#018x} (satp={:#x}) ===",
                step, last_pc, cur_pc, s.satp.to_bits());
            for (i, (vpc, ppc, raw)) in epg_ring.iter().enumerate() {
                eprintln!("    {:>4}  pc={:#018x} phys={:#010x} raw={:#010x}", i, vpc, ppc, raw);
            }
            eprintln!("    regs: ra={:#018x} sp={:#018x} tp={:#018x} s0={:#018x} s1={:#018x} a0={:#018x} a1={:#018x} a2={:#018x}",
                s.cpu.read_reg(1), s.cpu.read_reg(2), s.cpu.read_reg(4),
                s.cpu.read_reg(8), s.cpu.read_reg(9), s.cpu.read_reg(10),
                s.cpu.read_reg(11), s.cpu.read_reg(12));
            break;
        }
        // Track boot hart virtual+physical pc every step to see the jump that
        // sends it into the trampoline / early_pg_dir.
        if boot_pc_hist.len() < PC_RING_CAP {
            boot_pc_hist.push((cur_pc, s.last_fetch_paddr));
        } else {
            boot_pc_hist[boot_pc_head] = (cur_pc, s.last_fetch_paddr);
            boot_pc_head = (boot_pc_head + 1) % PC_RING_CAP;
        }
        // First transition back into the LOW 32-bit range [0x80000000,0x82000000)
        // after the kernel has been at full 64-bit VAs. Skip the initial kernel
        // load (step 1) and the intentional relocate trampoline.
        let in_low = cur_pc >= 0x8000_0000u64 && cur_pc < 0x8200_0000u64;
        if in_low && !was_in_low {
            low_entry += 1;
            if low_entry >= 1 {
                eprintln!("\n=== BOOT HART PC -> LOW at step {}: pc={:#018x} (prev={:#018x}, satp={:#x}, mmu_on={}) ===",
                    step, cur_pc, last_pc, s.satp.to_bits(), s.satp.mode == 8);
                let n = boot_pc_hist.len();
                let start = n.saturating_sub(64);
                for (i, &(vpc, ppc)) in boot_pc_hist[start..n].iter().enumerate() {
                    let mark = if vpc >= 0x8000_0000u64 && vpc < 0x8200_0000u64 { "  <== LOW" } else { "" };
                    eprintln!("    {:>4}  pre_pc={:#018x} phys={:#010x}{}", start+i, vpc, ppc, mark);
                }
                eprintln!("    regs: ra={:#018x} sp={:#018x} tp={:#018x} s0={:#018x} s1={:#018x} a0={:#018x} a1={:#018x} a2={:#018x} a7={:#x} a6={:#x}",
                    s.cpu.read_reg(1), s.cpu.read_reg(2), s.cpu.read_reg(4),
                    s.cpu.read_reg(8), s.cpu.read_reg(9), s.cpu.read_reg(10),
                    s.cpu.read_reg(11), s.cpu.read_reg(12), s.last_ecall_a7, s.last_ecall_a6);
                if low_entry <= 6 {
                    eprintln!("[LOW-ENTRY #{} step {}] pc={:#018x} prev={:#018x} satp={:#x} mmu_on={}",
                        low_entry, step, cur_pc, last_pc, s.satp.to_bits(), s.satp.mode == 8);
                }
            }
        }
        was_in_low = in_low;
        last_pc = cur_pc;

        if s.console_len > prev_console {
            let n = s.console_len.min(s.console_buf.len());
            console.push_str(&String::from_utf8_lossy(&s.console_buf[prev_console..n]));
            eprint!("{}", String::from_utf8_lossy(&s.console_buf[prev_console..n]));
            prev_console = n;
        }
        // Also capture UART console output
        if bus.uart_console.len() > prev_uart {
            let n = bus.uart_console.len();
            let uart_text = String::from_utf8_lossy(&bus.uart_console[prev_uart..n]);
            if !uart_text.is_empty() {
                console.push_str(&uart_text);
                eprint!("{}", uart_text);
            }
            prev_uart = n;
        }

        // Check static key value after boot init
        if step == 500_000 {
            let sk_val = { let d = bus.get_dram(); u64::from_le_bytes(d[static_key_dram_off..static_key_dram_off+8].try_into().unwrap()) };
            if sk_val != 0 {
                eprintln!("[step={}] static_key(0x81230020) = 0x{:016x} (ENABLED!)", step, sk_val);
            }
        }
        if s.ecall_count != prev_ecall_count {
            eprintln!("[ecall #{} step {}] a7={:#x} a6={:#x} a0={:#x}", s.ecall_count, step, s.last_ecall_a7, s.last_ecall_a6, s.last_ecall_a0);
            prev_ecall_count = s.ecall_count;
            prev_ecall_a7 = s.last_ecall_a7;
        }

        match status {
            Status::Running => {}
            Status::Wfi => { /* step() re-checks interrupts; ticking continues */ }
            Status::Trap(t) => {
                // A trap whose delivery redirected pc to a handler is normal
                // control flow (e.g. the intentional relocate_enable_mmu
                // trampoline fault). Only bail on a trap storm at one pc.
                trap_count += 1;
                if trap_count <= 12 {
                    eprintln!("[trap {} step {}] {:?} sepc={:#x} scause={:#x} stval={:#x} -> pc={:#x} priv={:?} satp={:#x} last_ecall_a7={:#x} ra={:#x} sp={:#x} tp={:#x} a0={:#x} a1={:#x} a5={:#x}",
                        trap_count, step, t, s.sepc, s.scause, s.stval, s.cpu.pc, s.priv_level, s.satp, s.last_ecall_a7, s.cpu.read_reg(1), s.cpu.read_reg(2), s.cpu.read_reg(4), s.cpu.read_reg(10), s.cpu.read_reg(11), s.cpu.read_reg(15));
                    if trap_count <= 2 {
                        let n = boot_pc_hist.len();
                        let mut trans: i64 = -1;
                        for i in 1..n {
                            let idx = (boot_pc_head + i) % n;
                            let pidx = (boot_pc_head + i - 1) % n;
                            let (vpc, _) = boot_pc_hist[idx];
                            let (prev, _) = boot_pc_hist[pidx];
                            let cur_low = vpc >= 0x8000_0000u64 && vpc < 0x8200_0000u64;
                            let prev_low = prev >= 0x8000_0000u64 && prev < 0x8200_0000u64;
                            if cur_low && !prev_low { trans = i as i64; break; }
                        }
                        if trans >= 0 {
                            let s0 = (trans as usize).saturating_sub(20);
                            let e0 = (trans as usize + 12).min(n);
                            eprintln!("  -- CORRUPTION TRANSITION (HIGH->LOW) at ring-pos {}:", trans);
                            for i in s0..e0 {
                                let idx = (boot_pc_head + i) % n;
                                let (vpc, ppc) = boot_pc_hist[idx];
                                let mark = if vpc >= 0x8000_0000u64 && vpc < 0x8200_0000u64 { "  <== LOW" } else { "" };
                                eprintln!("     {:>4}  vpc={:#018x} phys={:#010x}{}", i, vpc, ppc, mark);
                            }
                        } else {
                            eprintln!("  -- (no HIGH->LOW transition found in boot_pc_hist; n={})", n);
                    // Dump pre_pc_ring ground truth: head, first 5, last 5 (chronological).
                    let pn = pre_pc_ring.len();
                    eprintln!("  -- pre_pc_ring pn={} head={}", pn, pre_pc_head);
                    eprintln!("  -- pre_pc_ring FIRST 5:");
                    for i in 0..5.min(pn) {
                        let idx = (pre_pc_head + i) % pn;
                        let (vpc, raw) = pre_pc_ring[idx];
                        eprintln!("     {:>6}  pre_pc={:#018x} raw={:#010x}", i, vpc, raw);
                    }
                    eprintln!("  -- pre_pc_ring LAST 5:");
                    for i in (pn.saturating_sub(5))..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let (vpc, raw) = pre_pc_ring[idx];
                        eprintln!("     {:>6}  pre_pc={:#018x} raw={:#010x}", i, vpc, raw);
                    }
                    // Count LOW vs HIGH entries in the ring.
                    let mut lowc = 0usize; let mut highc = 0usize;
                    for i in 0..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let (vpc, _) = pre_pc_ring[idx];
                        if vpc >= 0x8000_0000u64 && vpc < 0x8200_0000u64 { lowc += 1; } else if vpc >= 0xffff_ffff_8000_0000u64 { highc += 1; }
                    }
                    eprintln!("  -- pre_pc_ring counts: LOW={} HIGH={} other={}", lowc, highc, pn - lowc - highc);
                    // Find entries whose NEXT chronological pc is in the LOW physical range
                    // [0x80000000,0x82000000) -- i.e. a jump into the trampoline / early_pg_dir.
                    for i in 0..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let nidx = (pre_pc_head + i + 1) % pn;
                        let (cur, craw) = pre_pc_ring[idx];
                        let (nxt, _) = pre_pc_ring[nidx];
                        if nxt >= 0x8000_0000u64 && nxt < 0x8200_0000u64 && cur >= 0xffff_ffff_8000_0000u64 {
                            eprintln!("  -- JUMP into LOW pc: caller pre_pc={:#018x} raw={:#010x} -> next={:#018x}", cur, craw, nxt);
                        }
                    }
                    // Find the instruction that jumped to 0x0 (jump-to-null). The next
                    // chronological pre_pc == 0x0 means this entry's instruction redirected
                    // control to a null address.
                    for i in 0..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let nidx = (pre_pc_head + i + 1) % pn;
                        let (cur, craw) = pre_pc_ring[idx];
                        let (nxt, _) = pre_pc_ring[nidx];
                        if nxt == 0x0 {
                            eprintln!("  -- JUMP TO 0x0: caller pre_pc={:#018x} raw={:#010x}", cur, craw);
                        }
                    }
                    // Dump last 30 entries (chronological) of pre_pc_ring.
                    eprintln!("  -- pre_pc_ring LAST 30 (chronological):");
                    let s0 = pn.saturating_sub(30);
                    for i in s0..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let (vpc, raw) = pre_pc_ring[idx];
                        eprintln!("     {:>7}  pre_pc={:#018x} raw={:#010x}", i, vpc, raw);
                    }
                    // Scan PRE-STEP pc ring (chronological) for the HIGH->LOW truncation jump.
                    let pn = pre_pc_ring.len();
                    let mut ptrans: i64 = -1;
                    for i in 1..pn {
                        let idx = (pre_pc_head + i) % pn;
                        let pidx = (pre_pc_head + i - 1) % pn;
                        let (cur, _) = pre_pc_ring[idx];
                        let (prev, _) = pre_pc_ring[pidx];
                        let cur_low = cur >= 0x8000_0000u64 && cur < 0x8200_0000u64;
                        let prev_low = prev >= 0x8000_0000u64 && prev < 0x8200_0000u64;
                        if cur_low && !prev_low { ptrans = i as i64; break; }
                    }
                    if ptrans >= 0 {
                        let s0 = (ptrans as usize).saturating_sub(24);
                        let e0 = ((ptrans as usize) + 12).min(pn);
                        eprintln!("  -- PRE-STEP HIGH->LOW TRANSITION at pre_pc_ring-pos {}:", ptrans);
                        for i in s0..e0 {
                            let idx = (pre_pc_head + i) % pn;
                            let (vpc, raw) = pre_pc_ring[idx];
                            let mark = if (vpc >= 0x8000_0000u64 && vpc < 0x8200_0000u64) { "  <== LOW" } else { "" };
                            eprintln!("     {:>6}  pre_pc={:#018x} raw={:#010x}{}", i, vpc, raw, mark);
                        }
                    } else {
                        eprintln!("  -- (no HIGH->LOW transition in pre_pc_ring either; pn={})", pn);
                    }
                        }
                        if trap_count == 1 {
                            dump_l1(&mut bus, 0x81763, "trampoline_pg_dir");
                            dump_l1(&mut bus, 0x81764, "swapper_early_pg_dir");
                        }
                        eprintln!("    MMU dbg: fail_vaddr={:#x} fail_reason={} fail_level={} fail_pte={:#x} satp_ppn={:#x} root_pte={:#x}",
                            s.mmu.dbg_fail_vaddr, s.mmu.dbg_fail_reason, s.mmu.dbg_fail_level, s.mmu.dbg_fail_pte, s.mmu.dbg_satp_ppn, s.mmu.dbg_root_pte);
                        eprintln!("    MMU walk: vaddr={:#x} root={:#x} pte_addr={:?} pte={:?} paddr={:#x} ppn={:#x} size={}",
                            s.mmu.dbg_walk_vaddr, s.mmu.dbg_walk_root,
                            s.mmu.dbg_walk_pte_addr.to_vec(), s.mmu.dbg_walk_pte.to_vec(),
                            s.mmu.dbg_walk_paddr, s.mmu.dbg_walk_ppn, s.mmu.dbg_walk_size);
                    }
                    // Dump the PC/raw ring buffer (last 48 instructions) to trace the path into the fault.
                    let idx = s.pc_trace_idx as usize;
                    let n = 48usize;
                    let start = if idx < n { 0 } else { idx - n };
                    eprintln!("  -- PC trace (last {} hdrs before trap):", n);
                    let mut k = start;
                    while k < idx {
                        let (pc, raw) = s.pc_trace[k % 256];
                        eprintln!("     {:>4}  pc={:#x} raw={:#010x}", k, pc, raw);
                        k += 1;
                    }
                    if s.phys_trans_idx > 0 {
                        eprintln!("  -- VIRTUAL->PHYSICAL JUMP transitions (last {}):", s.phys_trans_idx.min(64));
                        let n = s.phys_trans_idx.min(64);
                        let start = if s.phys_trans_idx < 64 { 0 } else { s.phys_trans_idx % 64 };
                        let mut k = 0;
                        while k < n {
                            let (jpc, jraw, ppc) = s.phys_transitions[(start + k) % 64];
                            eprintln!("     jal pc={:#x} raw={:#010x} -> phys pc={:#x}", jpc, jraw, ppc);
                            k += 1;
                        }
                    }
                    if s.phys_fault_captured {
                        eprintln!("  -- FAULT-TIME pc_trace snapshot (last 256 hdrs before the fetch fault at pc={:#x}); MMU on):", s.phys_fault_pc);
                        for k in 0..1024usize {
                            let (pc, raw) = s.phys_fault_ring[k];
                            eprintln!("     {:>4}  pc={:#x} raw={:#010x}", k, pc, raw);
                        }
                    }
                    // TRAMP_ENTRY: one-shot snapshot of the pc_trace ring taken the FIRST time the
                    // hart executed an instruction inside the secondary-hart trampoline region
                    // (0x80201000..0x80201200). This shows the SMP-bringup caller X that wrongly
                    // launched the boot hart down the secondary-start path.
                    if s.tramp_entry_captured {
                        eprintln!("  -- TRAMP_ENTRY ring (first entry into 0x80201000 trampoline; caller X is near the start):");
                        for k in 0..1024usize {
                            let (pc, raw) = s.tramp_entry_ring[k];
                            if pc != 0 {
                                eprintln!("     {:>4}  pc={:#x} raw={:#010x}", k, pc, raw);
                            }
                        }
                    } else {
                        eprintln!("  -- TRAMP_ENTRY ring: (never entered 0x80201000 trampoline region)");
                    }
                }
                if trap_count == 2 {
                    let dram = bus.get_dram();
                    let s4_virt = 0xffffffff_8102_fc88u64;
                    let s4_off = (s4_virt - 0xffffffff_8000_0000u64) as usize;
                    let s4_reg = s.cpu.read_reg(20);
                    eprintln!("\n=== DUMP s4/CPU table at {:#x} (s4 reg={:#x} dram offset {:#x}) ===", s4_virt, s4_reg, s4_off);
                    if s4_off + 64 <= dram.len() {
                        for i in 0..64 {
                            if i % 16 == 0 { eprint!("  {:#06x}: ", i); }
                            eprint!("{:02x} ", dram[s4_off + i]);
                            if i % 16 == 15 { eprintln!(); }
                        }
                    } else {
                        eprintln!("  OUT OF BOUNDS (dram len={})", dram.len());
                    }
                    let tp = s.cpu.read_reg(4);
                    let tp_virt = tp.wrapping_add(0x518);
                    if tp_virt >= 0xffffffff_8000_0000u64 {
                        let tp_off = (tp_virt - 0xffffffff_8000_0000u64) as usize;
                        eprintln!("=== DUMP tp+0x518 at {:#x} (tp={:#x} dram offset {:#x}) ===", tp_virt, tp, tp_off);
                        if tp_off + 64 <= dram.len() {
                            for i in 0..64 {
                                if i % 16 == 0 { eprint!("  {:#06x}: ", i); }
                                eprint!("{:02x} ", dram[tp_off + i]);
                                if i % 16 == 15 { eprintln!(); }
                            }
                        } else {
                            eprintln!("  OUT OF BOUNDS (dram len={})", dram.len());
                        }
                    } else {
                        eprintln!("=== DUMP tp+0x518: tp={:#x}, tp+0x518 below direct map ===", tp);
                    }
                }
                // Break at the FIRST re-entry into the trampoline (LOW pc) after step 2M,
                // so the pre_pc_ring captures the HIGH->LOW jump that starts it.
                if pre_pc >= 0x8000_0000u64 && pre_pc < 0x8200_0000u64 && step > 381_200 && !prev_pre_low {
                    eprintln!("\n=== BREAK at re-entry into LOW pc (trampoline) at step {}: pre_pc={:#018x} ===", step, pre_pc);
                    break;
                }
                if s.cpu.pc == last_trap_pc { same_trap += 1; } else { same_trap = 0; last_trap_pc = s.cpu.pc; }
                if same_trap < 100 { continue; }
                eprintln!("\n=== TRAP STORM step {} ===", step);
                eprintln!("trap: {:?}", t);
                eprintln!("pc={:#x} priv={:?}", s.cpu.pc, s.priv_level);
                eprintln!("sepc={:#x} scause={:#x} stval={:#x} stvec={:#x}", s.sepc, s.scause, s.stval, s.stvec);
                eprintln!("mepc={:#x} mcause={:#x} mtval={:#x}", s.mepc, s.mcause, s.mtval);
                eprintln!("satp={:#x} last_fetch_paddr={:#x} last_raw={:#010x}", s.satp.to_bits(), s.last_fetch_paddr, s.last_fetched_raw);
                for r in 0u8..32 {
                    eprintln!("  x{:<2} = {:#018x}", r, s.cpu.read_reg(r));
                }
                eprintln!("  sbi_ecalls={} last_a7={:#x} last_a6={:#x}", s.ecall_count, s.last_ecall_a7, s.last_ecall_a6);
                eprintln!("  mstatus.sie={} mstatus.mie={} mip={:#x} mie={:#x}", s.mstatus.sie, s.mstatus.mie, s.mip, s.mie);
                eprintln!("  medeleg={:#x} mideleg={:#x}", s.medeleg, s.mideleg);
                break;
            }
        }
        // Targeted outer-loop-header dump (VA 0xffffffff8000290e..0x80002956)
        if s.cpu.pc >= 0xffffffff8000290e && s.cpu.pc <= 0xffffffff80002956 && step % 10_000 == 0 {
            eprintln!("[loop step {}] pc={:#x} a1(T)={:#x} a5(T*1000)={:#x} s1={} s3={} ra={:#x}",
                step, s.cpu.pc, s.cpu.read_reg(11), s.cpu.read_reg(15),
                s.cpu.read_reg(9), s.cpu.read_reg(19), s.cpu.read_reg(1));
        }
        if step == 20_000_000 {
            let d = bus.get_dram();
            let base = 0x17781c0usize;
            eprintln!("=== timer-globals DRAM[0x{:x}..+0x80] ===", base);
            for i in (0..0x80).step_by(16) {
                let off = base + i;
                let mut line = String::new();
                for j in 0..16 { line.push_str(&format!("{:02x} ", d[off+j])); }
                eprintln!("  {:08x}: {}", off, line);
            }
        }
        if step % 20_000_000 == 0 {
            let goff = 0x15790d0usize;
            let gval = u64::from_le_bytes(bus.get_dram()[goff..goff+8].try_into().unwrap());
            let toff = 0x17781f8usize;
            let tval = if toff+4 <= bus.get_dram().len() { u32::from_le_bytes(bus.get_dram()[toff..toff+4].try_into().unwrap()) } else { 0 };
            let in_wait = s.cpu.pc >= 0xffffffff80202840 && s.cpu.pc <= 0xffffffff80202a00 || s.cpu.pc >= 0xffffffff80899b30 && s.cpu.pc <= 0xffffffff80899d20;
            eprintln!("[{}M steps] pc={:#x} sie={} ecalls={} mtime={} mtimecmp={} stimecmp={:#x} time_csr={} time_reads={} udelay_mult={:#x} T_timeout={} IN_WAIT={} clint_mtime_reads={} clint_mtimecmp_writes={} uart={}",
                step / 1_000_000, s.cpu.pc, s.mstatus.sie, s.ecall_count, bus.get_mtime(), bus.get_mtimecmp(), s.stimecmp, s.dbg_last_time, s.dbg_time_reads, gval, tval, in_wait,
                bus.get_clint_mtime_reads(), bus.get_clint_mtimecmp_writes(), bus.uart_console.len());
        }
    }

    // --- Manual sv39 page-table walk (bypass TLB) to diagnose wrong-PA mapping ---
    {
        let satp_ppn = s.satp.ppn;
        let root = satp_ppn << 12;
        eprintln!("\\n=== MANUAL sv39 walk: satp.mode={} satp.ppn={:#x} root={:#x} ===",
            s.satp.mode, satp_ppn, root);
        for &va in &[0xffffffff8089a090u64, 0xffffffff80201048u64] {
            let mut table_addr = root;
            let mut level: i32 = 2;
            let mut msg = format!("  VA {:#x}:\\n", va);
            let mut resolved_pa: Option<u64> = None;
            loop {
                let vpn: u64 = match level {
                    2 => (va >> 30) & 0x1ff,
                    1 => (va >> 21) & 0x1ff,
                    _ => (va >> 12) & 0x1ff,
                };
                let pte_addr = table_addr + vpn * 8;
                let pte = bus.read_u64(pte_addr);
                msg += &format!("    lvl{} vpn={:#x} pte_addr={:#x} pte={:#x}\\n", level, vpn, pte_addr, pte);
                if pte & 1 == 0 {
                    msg += "    -> NOT VALID (V=0)\\n";
                    break;
                }
                if (pte & 0xE) != 0 {
                    let ppn = (pte >> 10) & 0xFFFFFFFFFFF;
                    let pa = match level {
                        2 => (ppn << 12) | (va & 0x3FFF_FFFF),
                        1 => (ppn << 12) | (va & 0x1FF_FFFF),
                        _ => (ppn << 12) | (va & 0xFFF),
                    };
                    msg += &format!("    -> LEAF level {} ppn={:#x} PA={:#x}\\n", level, ppn, pa);
                    resolved_pa = Some(pa);
                    break;
                }
                let ppn = (pte >> 10) & 0xFFFFFFFFFFF;
                table_addr = ppn << 12;
                level -= 1;
                if level < 0 { msg += "    -> ran out of levels\\n"; break; }
            }
            eprintln!("{}", msg);
            if let Some(pa) = resolved_pa {
                let raw = bus.read_u32(pa);
                let file_off = pa.wrapping_sub(0x80200000);
                eprintln!("    DRAM[PA {:#x}] raw_instr={:#010x} (file_offset approx {:#x})", pa, raw, file_off);
            }
        }
        for i in 0u64..4 {
            let p = bus.read_u64(root + i*8);
            eprintln!("    root[{}] = {:#x}", i, p);
        }
    }

    // Scan DRAM for the LAST panic/bug message (ring buffer instance carries the real reason).
    {
        let dram = bus.get_dram();
        let pats: &[&[u8]] = &[b"Kernel panic - not syncing", b"not syncing", b"Oops -", b"BUG:"];
        for pat in pats {
            let mut last: Option<usize> = None;
            let plo = 0x1000000usize; let phi = dram.len().min(0x1900000usize);
            for i in plo..phi.saturating_sub(pat.len() + 1024) {
                if dram[i] == pat[0] && &dram[i..i + pat.len()] == *pat { last = Some(i); }
            }
            if let Some(i) = last {
                eprintln!("\n=== PANIC/BUG ({:?}) @ DRAM+0x{:x} (LAST match) ===", std::str::from_utf8(pat).unwrap(), i);
                let start = i.saturating_sub(128);
                let end = (i + 1024).min(dram.len());
                eprintln!("{}", String::from_utf8_lossy(&dram[start..end]));
                break;
            }
        }
    }


    // --- unique-PC recorder in intc/timer windows ---
    unsafe {
        let n = riscv_supervisor::supervisor::DBG_PC_SET_N;
        eprintln!("=== DBG_PC_SET ({} unique PCs in intc/timer windows) ===", n);
        for i in 0..n {
            eprintln!("  pc[{:#x}] = {:#x}", i, riscv_supervisor::supervisor::DBG_PC_SET[i]);
        }
    }

eprintln!("\n=== END step={} console {} bytes ===", step, console.len());

    // --- Explicit verdict: did we reach a booted kernel/userspace? ---
    if console.contains("Linux version") {
        println!("VERDICT: PASS");
    } else {
        eprintln!("VERDICT: FAIL -- no \"Linux version\" in console output (earlycon stopped early; see log_buf)");
    }

    // MMU walk-failure diagnostics (first failing VA)
    eprintln!("\n=== MMU walk-fail diag: count={} vaddr={:#x} pte_addr={:#x} pte={:#x} level={} reason={} satp.ppn={:#x} root[2]={:#x} ===",
        s.mmu.dbg_fail_count, s.mmu.dbg_fail_vaddr, s.mmu.dbg_fail_pte_addr, s.mmu.dbg_fail_pte, s.mmu.dbg_fail_level, s.mmu.dbg_fail_reason, s.mmu.dbg_satp_ppn, s.mmu.dbg_root_pte);
    eprintln!("=== ONE-SHOT WALK of VA {:#x}: root={:#x} ===", s.mmu.dbg_walk_vaddr, s.mmu.dbg_walk_root);
    for lvl in 0..3usize {
        eprintln!("   level {}: pte_addr={:#x} pte={:#x}", lvl, s.mmu.dbg_walk_pte_addr[lvl], s.mmu.dbg_walk_pte[lvl]);
    }
    eprintln!("   => paddr={:#x} ppn={:#x} size={}", s.mmu.dbg_walk_paddr, s.mmu.dbg_walk_ppn, s.mmu.dbg_walk_size);
    {
        // ---- Boot-log extraction: scan DRAM for kernel printk strings ----
        let dram = bus.get_dram();
        let scan_len = dram.len().min(0x2000_0000); // first 512 MB
        let _ = std::fs::write("/tmp/dram.bin", &dram[..scan_len]);
        let markers = ["Booting Linux", "Linux version", "Machine model",
                       "Command line", "clocksource", "riscv-timer",
                       "Timer interrupt", "Switched to", "Console:",
                       "console [", "printk:", "calibrating", "sched_clock",
                       "request_irq", "Failed to", "cannot", "panic",
                       "idle task", "sstc", "RISCV", "timer"];
        for m in markers {
            let mb = m.as_bytes();
            let mut off = 0usize;
            let mut hits = 0u32;
            while off + mb.len() <= scan_len && hits < 3 {
                if &dram[off..off+mb.len()] == mb {
                    let s = off.saturating_sub(96);
                    let e = (off + mb.len() + 192).min(scan_len);
                    eprintln!("=== '{}' @ dram+0x{:x} ===", m, off);
                    for b in &dram[s..e] {
                        eprint!("{}", if b.is_ascii_graphic() || *b==b' ' || *b==b'\n' || *b==b'\t' { *b as char } else { '.' });
                    }
                    eprintln!();
                    hits += 1;
                }
                off += 1;
            }
        }
    }

    if !console.contains("Linux version") {
        eprintln!("NOTE: console capture empty (uart={} bytes); boot log above extracted from DRAM", bus.uart_console.len());
    }
}
