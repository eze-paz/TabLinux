use riscv_core::types::{Status, Trap, Exception};
use riscv_devices::DeviceBus;
use riscv_supervisor::Supervisor;
use riscv_supervisor::types::{Privilege, Satp};
use std::fs;
use std::env;
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1024 * 1024 * 1024; // 1GB
const PT_PHYS: u64 = 0x8010_0000;

fn store_u64(dram: &mut [u8], addr: u64, val: u64) {
    let off = (addr - DRAM_BASE) as usize;
    dram[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

fn make_huge_pte(phys: u64, flags: u64) -> u64 {
    let ppn2 = phys >> 30;
    let ppn1 = (phys >> 21) & 0x1FF;
    (ppn2 << 28) | (ppn1 << 19) | (0 << 10) | flags
}

fn setup_page_tables(bus: &mut DeviceBus) {
    let l1_base = PT_PHYS + 0x1000;
    let dram = bus.get_dram_mut();
    // Zero out page table memory (2 pages)
    for addr in (PT_PHYS..PT_PHYS + 0x2000).step_by(8) {
        store_u64(dram, addr, 0);
    }
    // L0 root: point to L1 table
    store_u64(dram, PT_PHYS + 510 * 8, ((l1_base >> 12) << 10) | 0x01);
    // L1: 10 huge pages mapping 0x8020_0000..0x81a0_0000 RWX
    for i in 0..64u64 {
        let pte = make_huge_pte(0x8020_0000 + i * 0x20_0000, 0x0F);
        store_u64(dram, l1_base + i * 8, pte);
    }
}

fn is_pe_image(k: &[u8]) -> bool {
    if k.len() < 0x40 {
        return false;
    }
    let pe_offset = u32::from_le_bytes(k[0x3c..0x40].try_into().unwrap()) as usize;
    k.len() >= pe_offset + 4 && &k[pe_offset..pe_offset + 4] == b"PE\0\0"
}

fn boot_pe(
    bus: &mut DeviceBus,
    pe: &[u8],
    initrd_path: Option<&str>,
    kernel_dir: &str,
) -> (u64, u64, u64) {
    // Parse PE header
    let pe_offset = u32::from_le_bytes(pe[0x3c..0x40].try_into().unwrap()) as u64;
    let opt_header_size = u16::from_le_bytes(
        pe[(pe_offset + 0x14) as usize..(pe_offset + 0x16) as usize]
            .try_into()
            .unwrap(),
    ) as u64;
    let section_table = pe_offset + 0x18 + opt_header_size;
    let num_sections = u16::from_le_bytes(
        pe[(pe_offset + 0x6) as usize..(pe_offset + 0x8) as usize]
            .try_into()
            .unwrap(),
    ) as usize;
    let entry_point_rva = u32::from_le_bytes(
        pe[(pe_offset + 0x28) as usize..(pe_offset + 0x2c) as usize]
            .try_into()
            .unwrap(),
    ) as u64;

    println!(
        "PE image: offset={:#x}, sections={}, entry_rva={:#x}",
        pe_offset, num_sections, entry_point_rva
    );

    // Load sections
    for i in 0..num_sections {
        let sec = section_table + i as u64 * 40;
        let vaddr = u32::from_le_bytes(pe[(sec + 0xc) as usize..(sec + 0x10) as usize].try_into().unwrap()) as u64;
        let raw_size = u32::from_le_bytes(pe[(sec + 0x10) as usize..(sec + 0x14) as usize].try_into().unwrap()) as u64;
        let raw_ptr = u32::from_le_bytes(pe[(sec + 0x14) as usize..(sec + 0x18) as usize].try_into().unwrap()) as u64;
        let vsize = u32::from_le_bytes(pe[(sec + 0x8) as usize..(sec + 0xc) as usize].try_into().unwrap()) as u64;
        let phys = 0x8020_0000 + vaddr;
        let size = raw_size.min(vsize) as usize;
        if size > 0 && (raw_ptr as usize + size) <= pe.len() {
            bus.load_blob(phys, &pe[raw_ptr as usize..raw_ptr as usize + size]);
            println!("  section {}: vaddr={:#x} -> phys={:#x}, {} bytes", i, vaddr, phys, size);
        }
    }

    // Patch satp_mode from Sv57 to Sv39
    store_u64(bus.get_dram_mut(), 0x8122ff00, 0x8000000000000000u64);
    println!("Patched satp_mode at 0x8122ff00 -> Sv39");

    // Setup page tables
    setup_page_tables(bus);
    println!("Page tables set up at {:#x}", PT_PHYS);

    // Load initrd
    let initrd = if let Some(path) = initrd_path {
        fs::read(path).expect("Failed to read initrd")
    } else {
        // Try default initrd path
        let default = format!("{}/boot/initramfs-lts", kernel_dir);
        fs::read(&default).expect("Failed to read default initrd")
    };
    let initrd_load = (DRAM_BASE + (DRAM_SIZE as u64) - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;
    println!("Initrd loaded at {:#x} - {:#x} ({} bytes)", initrd_load, initrd_end, initrd.len());

    // Generate DTB
    let out = Command::new("python3")
        .arg("gen_dtb_v2.py".to_string())
        .arg(format!("{initrd_load:#x}"))
        .arg(format!("{initrd_end:#x}"))
        .current_dir(kernel_dir)
        .output()
        .expect("Failed to run gen_dtb_v2.py");
    if !out.status.success() {
        eprintln!("gen_dtb_v2.py stderr: {}", String::from_utf8_lossy(&out.stderr));
        panic!("DTB generation failed");
    }

    let dtb = fs::read(format!("{}/virt.dtb", kernel_dir)).expect("Failed to read virt.dtb");
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);
    println!("DTB loaded at {:#x} ({} bytes)", dtb_load, dtb.len());

    // Entry point: PE entry point RVA (kernel VA, MMU will translate)
    let kernel_entry = 0xffff_ffff_8000_0000u64 + entry_point_rva;
    println!("Kernel entry point (VA): {:#x}", kernel_entry);

    (kernel_entry, dtb_load, initrd_load)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <kernel> [dtb] [--initrd <initrd>] [--kernel-dir <dir>]", args[0]);
        std::process::exit(1);
    }

    let kernel_path = &args[1];
    let mut dtb_path: Option<&str> = None;
    let mut initrd_path: Option<&str> = None;
    let mut kernel_dir: Option<&str> = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--initrd" => {
                i += 1;
                if i < args.len() {
                    initrd_path = Some(&args[i]);
                }
            }
            "--kernel-dir" => {
                i += 1;
                if i < args.len() {
                    kernel_dir = Some(&args[i]);
                }
            }
            _ => {
                if dtb_path.is_none() && !args[i].starts_with("--") {
                    dtb_path = Some(&args[i]);
                }
            }
        }
        i += 1;
    }

    // Determine kernel directory (for gen_dtb_v2.py and default initrd)
    let kernel_dir_str = kernel_dir
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Derive from kernel path: if kernel is in kernels/ or kernels/boot/, use that dir
            let p = std::path::Path::new(kernel_path);
            if let Some(parent) = p.parent() {
                let parent_str = parent.to_string_lossy();
                if parent_str.ends_with("/kernels") || parent_str.ends_with("/boot") {
                    if parent_str.ends_with("/boot") {
                        parent.parent().unwrap().to_string_lossy().to_string()
                    } else {
                        parent_str.to_string()
                    }
                } else {
                    parent_str.to_string()
                }
            } else {
                ".".to_string()
            }
        });

    let k = fs::read(kernel_path).expect("Failed to read kernel");
    println!("Kernel: {} bytes from {}", k.len(), kernel_path);

    let mut bus = DeviceBus::new(DRAM_SIZE);

    // Detect image type and boot accordingly
    let (kernel_load, dtb_addr, use_pe) = if is_pe_image(&k) {
        println!("Detected PE image");
        let (entry, dtb, _initrd) = boot_pe(&mut bus, &k, initrd_path, &kernel_dir_str);
        (entry, dtb, true)
    } else {
        // Detect Linux kernel header
        let (text_offset, _image_size) = if k.len() >= 64 {
            let magic = u64::from_le_bytes([
                k[0x30], k[0x31], k[0x32], k[0x33],
                k[0x34], k[0x35], k[0x36], k[0x37],
            ]);
            if magic == 0x5643534952u64 {
                let text_offset = u64::from_le_bytes([
                    k[0x08], k[0x09], k[0x0A], k[0x0B],
                    k[0x0C], k[0x0D], k[0x0E], k[0x0F],
                ]);
                let image_size = u64::from_le_bytes([
                    k[0x10], k[0x11], k[0x12], k[0x13],
                    k[0x14], k[0x15], k[0x16], k[0x17],
                ]);
                println!("Linux RISC-V kernel detected: text_offset={:#x}, image_size={:#x}", text_offset, image_size);
                (text_offset, image_size as usize)
            } else {
                println!("Not a Linux kernel image, loading as flat binary");
                (0x0, k.len())
            }
        } else {
            (0x0, k.len())
        };

        let kernel_load = DRAM_BASE + text_offset;
        let kernel_off = (kernel_load - DRAM_BASE) as usize;
        bus.get_dram_mut()[kernel_off..kernel_off + k.len()].copy_from_slice(&k);

        // Load DTB if provided
        let dtb_addr = if let Some(path) = dtb_path {
            let dtb = fs::read(path).expect("Failed to read DTB");
            let dtb_load = (kernel_load - (dtb.len() as u64 + 0xFFF)) & !0xFFF;
            let dtb_off = (dtb_load - DRAM_BASE) as usize;
            bus.get_dram_mut()[dtb_off..dtb_off + dtb.len()].copy_from_slice(&dtb);
            println!("DTB loaded at {:#x} ({} bytes)", dtb_load, dtb.len());
            dtb_load
        } else {
            0
        };

        (kernel_load, dtb_addr, false)
    };

    // Create Supervisor
    let mut s = Supervisor::new(kernel_load, 0);
    s.priv_level = Privilege::Supervisor;

    if use_pe {
        // PE boot: enable MMU, set a0 to page table base
        s.satp = Satp { mode: 8, asid: 0, ppn: PT_PHYS >> 12 };
        s.cpu.write_reg(10, PT_PHYS); // a0 = page table base (required by Alpine kernel)
        println!("MMU enabled: satp={:#x}", s.satp.to_bits());
    } else {
        s.cpu.write_reg(10, 0); // a0 = hartid
    }

    if dtb_addr != 0 {
        s.cpu.write_reg(11, dtb_addr); // a1 = dtb
    }
    s.cpu.write_reg(2, 0x81FF_0000); // sp = stack pointer
    s.medeleg = 0xB1FF; // Delegate all common exceptions to S-mode
    s.mideleg = 0x222;  // Delegate S-mode interrupts

    println!("Booting at {:#x} in {:?} mode...", s.cpu.pc, s.priv_level);

    let mut prev_console_len = 0;
    let mut prev_pc = 0u64;
    let mut prev_mcause = s.mcause;
    let mut prev_scause = s.scause;
    let mut _prev_mepc = s.mepc;
    let mut _prev_sepc = s.sepc;
    let mut trap_count = 0;
    let max_steps = 50_000_000;
    const PC_HIST: usize = 50;
    let mut pc_history: [u64; PC_HIST] = [0; PC_HIST];
    let mut hist_idx: usize = 0;
    let mut hist_len: usize = 0;

    for step in 0..max_steps {
        pc_history[hist_idx] = s.cpu.pc;
        hist_idx = (hist_idx + 1) % PC_HIST;
        if hist_len < PC_HIST { hist_len += 1; }

        bus.tick();
        if bus.check_timer_interrupt() {
            s.mip |= 1 << 7; // MTIP
        }

        let status = s.step(&mut bus);

        if s.mcause != prev_mcause {
            eprintln!("[M-TRAP #{}] mcause={:#x} mepc={:#x} at step={}", trap_count, s.mcause, s.mepc, step);
            prev_mcause = s.mcause;
            _prev_mepc = s.mepc;
            trap_count += 1;
        }
        if s.scause != prev_scause {
            eprintln!("[S-TRAP #{}] scause={:#x} sepc={:#x} last_trap_epc={:#x} satp={:#x} priv={:?} at step={}",
                trap_count, s.scause, s.sepc, s.last_trap_epc, s.satp.to_bits(), s.priv_level, step);
            eprintln!("Last {} PCs before trap:", hist_len);
            for i in 0..hist_len {
                let idx = (hist_idx + PC_HIST - hist_len + i) % PC_HIST;
                eprintln!("  {:3}: 0x{:012x}", i + 1, pc_history[idx]);
            }
            prev_scause = s.scause;
            _prev_sepc = s.sepc;
            trap_count += 1;
        }

        // Print new console output from Supervisor console_buf
        if s.console_len > prev_console_len {
            let new_bytes = &s.console_buf[prev_console_len..s.console_len.min(4096)];
            if let Ok(text) = std::str::from_utf8(new_bytes) {
                print!("{}", text);
            } else {
                for b in new_bytes {
                    print!("[{:02x}]", b);
                }
            }
            prev_console_len = s.console_len;
        }

        if step > 0 && step % 500_000 == 0 {
            if s.cpu.pc != prev_pc {
                eprintln!("[step {:>10}] pc=0x{:012x} priv={:?}", step, s.cpu.pc, s.priv_level);
                prev_pc = s.cpu.pc;
            }
        }

        match status {
            Status::Running => {}
            Status::Trap(Trap::Exception(Exception::Breakpoint)) => {
                eprintln!("\n[EBREAK at step {} pc={:#x}]", step, s.cpu.pc);
                eprintln!("Last {} PCs:", hist_len);
                for i in 0..hist_len {
                    let idx = (hist_idx + PC_HIST - hist_len + i) % PC_HIST;
                    eprintln!("  {:3}: 0x{:012x}", i + 1, pc_history[idx]);
                }
                eprintln!("Registers:");
                for i in 0..32 {
                    eprintln!("  x{:2} = 0x{:016x}", i, s.cpu.read_reg(i as u8));
                }
                break;
            }
            Status::Wfi => {}
            Status::Trap(t) => {
                let mcause = s.mcause;
                eprintln!("\n[TRAP at step {} pc={:#x} mcause={}: {:?}]", step, s.cpu.pc, mcause, t);

                // Dump page table memory if using PE boot
                if use_pe {
                    let pt_addr = PT_PHYS;
                    let pt_off = (pt_addr - DRAM_BASE) as usize;
                    eprintln!("Page table at {:#x}:", pt_addr);
                    for j in 0..16 {
                        let pte = u64::from_le_bytes([
                            bus.get_dram()[pt_off + j * 8], bus.get_dram()[pt_off + j * 8 + 1],
                            bus.get_dram()[pt_off + j * 8 + 2], bus.get_dram()[pt_off + j * 8 + 3],
                            bus.get_dram()[pt_off + j * 8 + 4], bus.get_dram()[pt_off + j * 8 + 5],
                            bus.get_dram()[pt_off + j * 8 + 6], bus.get_dram()[pt_off + j * 8 + 7],
                        ]);
                        if pte != 0 {
                            eprintln!("  PTE[{}]: {:#016x} (V={} R={} W={} X={} PPN={:#x})",
                                j, pte, pte & 1, (pte >> 1) & 1, (pte >> 2) & 1, (pte >> 3) & 1, (pte >> 10) & 0xFFFFFFFFFF);
                        }
                    }
                }

                // Dump memory around faulting PC
                let fault_off = (s.sepc.saturating_sub(DRAM_BASE)) as usize;
                if fault_off + 64 <= bus.get_dram().len() {
                    eprintln!("Memory around sepc={:#x}:", s.sepc);
                    for j in 0..8 {
                        let addr = fault_off + j * 8;
                        let val = u64::from_le_bytes([
                            bus.get_dram()[addr], bus.get_dram()[addr + 1],
                            bus.get_dram()[addr + 2], bus.get_dram()[addr + 3],
                            bus.get_dram()[addr + 4], bus.get_dram()[addr + 5],
                            bus.get_dram()[addr + 6], bus.get_dram()[addr + 7],
                        ]);
                        eprintln!("  {:#x}: {:#016x}", DRAM_BASE + addr as u64, val);
                    }
                }

                break;
            }
        }
    }

    eprintln!("\n--- Boot finished ---");
    eprintln!("Console output length: {} bytes", s.console_len);
    if s.console_len > 0 {
        let output = std::str::from_utf8(&s.console_buf[..s.console_len.min(4096)])
            .unwrap_or("<invalid utf8>");
        eprintln!("Console output (first 4KB): {}", output);
    }
    eprintln!("Final PC: {:#x}", s.cpu.pc);
}
