//! Boot Alpine all the way to userspace: flat vmlinuz-lts.raw + initramfs-lts +
//! generated DTB, direct S-mode entry (a0=hartid, a1=dtb) per the RISC-V Linux
//! boot protocol.
//!
//! Unlike `oneshot_alpine`, this driver applies NO binary patches to the kernel
//! image and keeps no multi-million-entry trace rings, so it can run a realistic
//! step budget. When it stalls or traps it symbolizes the PC against
//! `kernels/boot/System.map-6.18.35-0-lts` instead of printing raw addresses.
//!
//! Run with:  cargo test --release -p riscv-harness --test boot_to_userspace -- --nocapture

#[allow(unused_imports)] use riscv_core::execute::Bus;
use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege};
use std::collections::BTreeMap;
use std::process::Command;

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 1 << 30; // 1GB — must match gen_dtb_v2's memory node

/// Kernel symbol table, for turning a PC into `function+0xoff`.
struct Symbols {
    addrs: Vec<u64>,
    names: Vec<String>,
}

impl Symbols {
    fn load(path: &str) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut pairs: Vec<(u64, String)> = text
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let addr = u64::from_str_radix(f.next()?, 16).ok()?;
                let _kind = f.next()?;
                Some((addr, f.next()?.to_string()))
            })
            .collect();
        pairs.sort_by_key(|p| p.0);
        Some(Self {
            addrs: pairs.iter().map(|p| p.0).collect(),
            names: pairs.into_iter().map(|p| p.1).collect(),
        })
    }

    fn lookup(&self, pc: u64) -> String {
        match self.addrs.binary_search(&pc) {
            Ok(i) => self.names[i].clone(),
            Err(0) => format!("{pc:#x}"),
            Err(i) => format!("{}+{:#x}", self.names[i - 1], pc - self.addrs[i - 1]),
        }
    }
}

#[test]
fn boot_to_userspace() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let syms = Symbols::load(&format!("{root}/kernels/boot/System.map-6.18.35-0-lts"));
    let sym = |pc: u64| match &syms {
        Some(s) => s.lookup(pc),
        None => format!("{pc:#x}"),
    };

    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");

    let text_offset = u64::from_le_bytes(kernel[0x08..0x10].try_into().unwrap());
    let kernel_load = DRAM_BASE + text_offset;

    let mut bus = DeviceBus::new(DRAM_SIZE);
    bus.load_blob(kernel_load, &kernel);

    // initrd near the top of RAM, 64K aligned, clear of the kernel's footprint
    let initrd_load =
        (DRAM_BASE + (DRAM_SIZE as u64) - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;

    let out = Command::new("python3")
        .arg(format!("{root}/kernels/gen_dtb_v2.py"))
        .arg(format!("{initrd_load:#x}"))
        .arg(format!("{initrd_end:#x}"))
        .current_dir(format!("{root}/kernels"))
        .output()
        .expect("gen dtb");
    assert!(out.status.success(), "dtb gen: {}", String::from_utf8_lossy(&out.stderr));
    let dtb = std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb");
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    eprintln!(
        "kernel {kernel_load:#x}+{:#x}  initrd {initrd_load:#x}..{initrd_end:#x}  dtb {dtb_load:#x}+{:#x}",
        kernel.len(),
        dtb.len()
    );

    let mut s = Supervisor::new(kernel_load, 0);
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0); // a0 = hartid
    s.cpu.write_reg(11, dtb_load); // a1 = dtb
    s.cpu.write_reg(2, DRAM_BASE + DRAM_SIZE as u64 - 0x10000); // sp
    s.medeleg = 0xB1FF;
    s.mideleg = 0x2A2;

    // Budget: Alpine's early init (memset of the 16M log buffer, memmap init for
    // 1GB, initramfs gunzip+cpio) costs hundreds of millions of instructions.
    let max_steps: u64 = std::env::var("MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000_000_000u64);

    let mut console = String::new();
    let mut printed = 0usize;
    let mut prev_uart = 0usize;
    let mut prev_sbi = 0usize;
    let mut traps: u64 = 0;
    let mut trap_hist: BTreeMap<(String, u64), u64> = BTreeMap::new();
    let mut step: u64 = 0;
    let mut stall_pc = 0u64;
    let mut stall_since = 0u64;
    let mut last_progress_step = 0u64;
    let mut last_ecalls = 0u64;
    let mut wfis: u64 = 0;
    let mut user_syscalls: u64 = 0;
    let mut typed = false;
    let mut shell_ok = false;
    const SHELL_SCRIPT: &str = concat!(
        "echo HELLO-FROM-RISCV-USERSPACE\n",
        "uname -a\n",
        "cat /proc/cpuinfo\n",
        "ls /\n",
        // Brackets the one thing worth pricing: a wholly idle guest second.
        // If the WFI fast-forward works, almost no host work happens in here.
        "echo IDLE-BEGIN\n",
        "sleep 1\n",
        "echo IDLE-END\n",
        "echo EXIT-CODE=$?\n",
    );
    // (step, wall clock, wfis, fast-forwards, emulated ticks skipped) at IDLE-BEGIN
    let mut idle_mark: Option<(u64, std::time::Instant, u64, u64, u64)> = None;
    let mut idle_done = false;
    let mut ffs: u64 = 0;
    let mut ff_ticks: u64 = 0;
    let mut wfi_noop: u64 = 0;
    const STALL_STEPS: u64 = 800_000_000;
    let mut in_userspace_at: Option<u64> = None;
    let mut hot: BTreeMap<String, u64> = BTreeMap::new();

    let flush_console = |console: &mut String, prev: &mut usize| {
        while let Some(nl) = console[*prev..].find('\n') {
            let line = &console[*prev..*prev + nl];
            eprintln!("| {line}");
            *prev += nl + 1;
        }
    };

    while step < max_steps {
        let pre_pc = s.cpu.pc;
        bus.tick();
        match s.step(&mut bus) {
            Status::Running => {}
            Status::Wfi => {
                // Single hart: only the timer can wake us. Skip emulated time
                // straight to the earlier of the two armed deadlines rather than
                // spinning the idle loop for the billions of steps a NOHZ idle
                // of a few seconds would otherwise cost.
                wfis += 1;
                let next = s.stimecmp.min(bus.get_mtimecmp());
                let before = bus.read_mtime();
                if next != u64::MAX {
                    bus.fast_forward_mtime(next);
                }
                // A WFI that advances nothing is the failure mode worth
                // counting: the hart parks, time does not move, and the idle
                // loop is paid for at full interpreter price.
                let after = bus.read_mtime();
                if after > before {
                    ffs += 1;
                    ff_ticks += after - before;
                } else {
                    wfi_noop += 1;
                }
            }
            Status::Trap(t) => {
                traps += 1;
                if matches!(t, riscv_core::types::Trap::Exception(
                    riscv_core::types::Exception::EnvironmentCallFromU)) {
                    user_syscalls += 1;
                }
                // Keep this path allocation-free: once userspace is running there
                // are hundreds of thousands of traps, and a format!+map-insert per
                // trap dominates the whole run.
                //
                // Only *kernel* faults are worth naming individually — a fault at a
                // user PC is ordinary demand paging, and every user PC is distinct.
                let is_kernel_pc = pre_pc >= 0xffff_ffc0_0000_0000;
                if is_kernel_pc && !matches!(t, riscv_core::types::Trap::Interrupt(_)) {
                    let key = (format!("{t:?}"), pre_pc);
                    let n = {
                        let c = trap_hist.entry(key).or_insert(0);
                        *c += 1;
                        *c
                    };
                    if n <= 3 {
                        eprintln!(
                            "[trap {traps} step {step}] {t:?} at {} (pc={pre_pc:#x}) stval={:#x} -> {}",
                            sym(pre_pc),
                            s.stval,
                            sym(s.cpu.pc)
                        );
                    }
                    if n == 4 {
                        eprintln!("[trap] ...further {t:?} at {} suppressed", sym(pre_pc));
                    }
                    if n > 200_000 {
                        eprintln!("[FATAL] trap storm at {} — aborting", sym(pre_pc));
                        break;
                    }
                }
            }
        }

        // First entry into U-mode == userspace reached.
        if in_userspace_at.is_none() && s.priv_level == Privilege::User {
            in_userspace_at = Some(step);
            eprintln!("\n*** USERSPACE REACHED at step {step} (pc={:#x}) ***\n", s.cpu.pc);
        }

        // Drain both console sources: the 8250 UART MMIO writes and the SBI DBCN
        // buffer the supervisor collects.
        let mut console_grew = false;
        if bus.uart_console.len() > prev_uart {
            let n = bus.uart_console.len();
            console.push_str(&String::from_utf8_lossy(&bus.uart_console[prev_uart..n]));
            prev_uart = n;
            last_progress_step = step;
            console_grew = true;
        }
        if s.console_len > prev_sbi {
            let n = s.console_len.min(s.console_buf.len());
            console.push_str(&String::from_utf8_lossy(&s.console_buf[prev_sbi..n]));
            prev_sbi = n;
            last_progress_step = step;
            console_grew = true;
        }
        // Only touch the console string when it actually grew: scanning a
        // 30 KB transcript on every one of billions of steps costs more than
        // the emulator itself.
        if console_grew {
            flush_console(&mut console, &mut printed);

            // Once the initramfs rescue shell is up, type a few commands at it so
            // the run proves userspace actually *executes*, not merely that it
            // started.
            if !typed && console.contains("emergency recovery shell") {
                typed = true;
                bus.uart_push_input(SHELL_SCRIPT.as_bytes());
                eprintln!("\n[harness] fed {} bytes of shell input\n", SHELL_SCRIPT.len());
            }
            // Match the marker as a line of its own. The shell echoes every
            // command it is fed, so a plain `contains` fires on the echoed
            // `echo IDLE-BEGIN` — and since the whole script is pushed at once,
            // both markers appear before `sleep` has even started.
            let has_line = |c: &str, m: &str| {
                c.lines().any(|l| l.trim_end_matches('\r') == m)
            };
            if idle_mark.is_none() && !idle_done && has_line(&console, "IDLE-BEGIN") {
                idle_mark = Some((step, std::time::Instant::now(), wfis, ffs, ff_ticks));
            }
            if let Some((s0, t0, w0, f0, k0)) = idle_mark {
                if has_line(&console, "IDLE-END") {
                    idle_mark = None;
                    idle_done = true;
                    let wall = t0.elapsed().as_secs_f64();
                    let emul = (ff_ticks - k0) as f64 / 10_000_000.0
                        + (step - s0) as f64 / 1e8;
                    eprintln!(
                        "\n[idle] guest `sleep 1` cost {} host instructions in {:.2}s wall\n\
                         [idle] emulated time advanced {:.3}s ({:.3}s skipped by fast-forward)\n\
                         [idle] {} WFIs, {} advanced time, {} advanced nothing\n",
                        step - s0, wall, emul,
                        (ff_ticks - k0) as f64 / 10_000_000.0,
                        wfis - w0, ffs - f0, (wfis - w0) - (ffs - f0),
                    );
                }
            }
            // The last thing the script echoes; once we see it the shell has run
            // every command, so there is nothing left to wait for.
            if typed && console.contains("EXIT-CODE=0") {
                shell_ok = true;
                break;
            }
        }

        // Hang detector: sample the PC every 4M steps. Console output is a poor
        // liveness signal on its own — module decompression (`inflate_fast`) runs
        // for hundreds of millions of steps in silence — so retiring a syscall
        // counts as progress too.
        if step % 4_000_000 == 0 && step > 0 {
            *hot.entry(sym(s.cpu.pc)).or_insert(0) += 1;
            if user_syscalls != last_ecalls {
                last_ecalls = user_syscalls;
                last_progress_step = step;
            }
            if step - last_progress_step >= STALL_STEPS {
                eprintln!("\n[STALL] no console output and no syscall for {STALL_STEPS} steps; pc={} ({:#x})", sym(s.cpu.pc), s.cpu.pc);
                let mut v: Vec<_> = hot.iter().collect();
                v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
                for (name, n) in v.into_iter().take(15) {
                    eprintln!("  {n:5} samples  {name}");
                }
                // Dense trace of the loop we are actually stuck in.
                eprintln!("[STALL] dense trace of the next 200k steps:");
                let mut loop_hist: BTreeMap<String, u64> = BTreeMap::new();
                let mut privs: BTreeMap<String, u64> = BTreeMap::new();
                for _ in 0..200_000 {
                    let f = sym(s.cpu.pc);
                    let f = f.split('+').next().unwrap_or(&f).to_string();
                    *loop_hist.entry(f).or_insert(0) += 1;
                    *privs.entry(format!("{:?}", s.priv_level)).or_insert(0) += 1;
                    bus.tick();
                    let st = s.step(&mut bus);
                    if let Status::Wfi = st {
                        let next = s.stimecmp.min(bus.get_mtimecmp());
                        if next != u64::MAX {
                            bus.fast_forward_mtime(next);
                        }
                    }
                }
                let mut lv: Vec<_> = loop_hist.iter().collect();
                lv.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
                eprintln!("  distinct PCs in window: {}", loop_hist.len());
                for (name, n) in lv.into_iter().take(25) {
                    eprintln!("  {n:7} {name}");
                }
                eprintln!("  privilege mix: {privs:?}");
                break;
            }
        }
        let _ = (&mut stall_pc, &mut stall_since);
        step += 1;
    }

    // Flush any trailing partial line.
    if printed < console.len() {
        eprintln!("| {}", &console[printed..]);
    }

    eprintln!("\n=== summary ===");
    eprintln!("steps executed : {step}");
    eprintln!("console bytes  : {}", console.len());
    eprintln!("traps          : {traps}");
    eprintln!("syscalls (U)   : {user_syscalls}");
    eprintln!("wfi parks      : {wfis} ({ffs} skipped time, {wfi_noop} did not)");
    let mut v: Vec<_> = trap_hist.iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for ((t, pc), n) in v.into_iter().take(12) {
        eprintln!("  {n:>10}x {t} at {}", sym(*pc));
    }
    match in_userspace_at {
        Some(st) => eprintln!("USERSPACE      : reached at step {st}"),
        None => eprintln!("USERSPACE      : NOT reached"),
    }
    eprintln!("SHELL          : {}", if shell_ok { "ran all commands" } else { "did not complete" });

    assert!(in_userspace_at.is_some(), "kernel never entered U-mode");
    assert!(shell_ok, "the initramfs shell did not run the scripted commands to completion");
    assert!(
        console.contains("riscv64") && console.contains("HELLO-FROM-RISCV-USERSPACE"),
        "expected `uname -a` and `echo` output from the guest shell"
    );

    let mut hotv: Vec<_> = hot.iter().collect();
    hotv.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    eprintln!("hot PCs (sampled every 4M steps):");
    for (name, n) in hotv.into_iter().take(12) {
        eprintln!("  {n:5} {name}");
    }
}
