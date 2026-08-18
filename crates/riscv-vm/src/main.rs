//! Interactive front end: boot Alpine and hand the emulated UART to your terminal.
//!
//!   cargo run --release -p riscv-vm
//!
//! Your keystrokes go to the guest and the guest's output comes back, so this is
//! an actual Linux session rather than a scripted test. Press `Ctrl-A` then `x`
//! to kill the machine (same escape qemu uses), `Ctrl-A` then `a` to send a
//! literal Ctrl-A.
//!
//! Flags:
//!   --kernel <path>   default kernels/vmlinuz-lts.raw
//!   --initrd <path>   default kernels/boot/initramfs-lts
//!   --mem <MiB>       default 1024
//!   --quiet           drop kernel log level to 3 so boot is less chatty

use riscv_core::execute::Bus as _;
use riscv_hostnet as hostnet;
use riscv_core::types::Status;
use riscv_devices::{BlockBackend, DeviceBus};
use riscv_supervisor::{types::Privilege, Supervisor};
use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc;

const DRAM_BASE: u64 = 0x8000_0000;

/// Puts the terminal in raw mode for the lifetime of the VM and restores the
/// previous settings on the way out — including on panic, since `Drop` still
/// runs while unwinding. Leaving a shell in raw mode is deeply annoying.
struct RawTerminal {
    saved: Option<String>,
}

impl RawTerminal {
    fn enable() -> Self {
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        if saved.is_some() {
            // -icanon -echo: deliver keys one at a time, don't double-print them.
            // -isig: let the guest see Ctrl-C instead of killing the emulator.
            // -ixon: Ctrl-S/Ctrl-Q belong to the guest too.
            let _ = Command::new("stty")
                .args(["raw", "-echo", "-isig", "-ixon"])
                .stdin(std::process::Stdio::inherit())
                .status();
        }
        Self { saved }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(s) = &self.saved {
            let _ = Command::new("stty")
                .arg(s)
                .stdin(std::process::Stdio::inherit())
                .status();
        }
        let _ = std::io::stdout().flush();
    }
}

/// Replace the first occurrence of `from` with `to`, which must be the same
/// length. Same-length is the whole point: a devicetree property can be
/// rewritten in place without moving anything after it.
fn patch_same_len(buf: &mut [u8], from: &[u8], to: &[u8]) -> bool {
    assert_eq!(from.len(), to.len(), "in-place patch must not change length");
    match buf.windows(from.len()).position(|w| w == from) {
        Some(at) => {
            buf[at..at + to.len()].copy_from_slice(to);
            true
        }
        None => false,
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Drain frames the guest transmitted and queue whatever the host stack can
/// answer. This stands in for v86's `fake_network.js`, which is what will sit
/// here once the wasm build hosts it.
fn pump_net(bus: &DeviceBus, verbose: bool) {
    let Some(q) = bus.net.clone() else { return };
    loop {
        let f = q.borrow_mut().to_host.pop_front();
        let Some(f) = f else { break };
        let et = if f.len() >= 14 { (f[12] as u16) << 8 | f[13] as u16 } else { 0 };
        let kind = match et {
            0x0806 => "ARP",
            0x0800 => "IPv4",
            0x86DD => "IPv6",
            _ => "?",
        };
        if verbose {
            eprintln!("[net tx] {} bytes, ethertype {et:#06x} ({kind})\r", f.len());
        }
        if std::env::var("RISCV_NET_TIME").is_ok() {
            // Emulated milliseconds, which is the clock the guest measures RTT
            // against. Wall clock would tell us nothing about why ping reports
            // hundreds of ms.
            eprintln!("[net t] tx visible at {:.3} ms\r", bus.read_mtime() as f64 / 10_000.0);
        }
        if let Some(reply) = hostnet::respond(&f) {
            if verbose {
                eprintln!("[net rx] {} byte reply\r", reply.len());
            }
            q.borrow_mut().to_guest.push_back(reply);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

    let kernel_path =
        arg_value(&args, "--kernel").unwrap_or_else(|| format!("{root}/kernels/vmlinuz-lts.raw"));
    let initrd_path =
        arg_value(&args, "--initrd").unwrap_or_else(|| format!("{root}/kernels/boot/initramfs-lts"));
    let mem_mib: usize = arg_value(&args, "--mem")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let quiet = args.iter().any(|a| a == "--quiet");

    let kernel = std::fs::read(&kernel_path)
        .unwrap_or_else(|e| panic!("cannot read kernel {kernel_path}: {e}"));
    let initrd = std::fs::read(&initrd_path)
        .unwrap_or_else(|e| panic!("cannot read initrd {initrd_path}: {e}"));

    let dram_size = mem_mib << 20;
    let text_offset = u64::from_le_bytes(kernel[0x08..0x10].try_into().unwrap());
    let kernel_load = DRAM_BASE + text_offset;

    let mut bus = DeviceBus::new(dram_size);
    bus.load_blob(kernel_load, &kernel);

    let initrd_load =
        (DRAM_BASE + dram_size as u64 - initrd.len() as u64 - 0x100_0000) & !0xFFFFu64;
    bus.load_blob(initrd_load, &initrd);
    let initrd_end = initrd_load + initrd.len() as u64;

    // Attach a disk if one was given (or if the default image exists), and tell
    // the initramfs to switch_root into it.
    let default_disk = format!("{root}/kernels/rootfs.ext4");
    let disk = arg_value(&args, "--disk").or_else(|| {
        if args.iter().any(|a| a == "--no-disk") || !std::path::Path::new(&default_disk).exists() {
            None
        } else {
            Some(default_disk)
        }
    });

    // --no-root attaches the disk but leaves the guest in the initramfs, which
    // is how you poke at a filesystem the kernel refuses to boot from.
    let use_as_root = disk.is_some() && !args.iter().any(|a| a == "--no-root");

    // Prefer the pre-built devicetree. Shelling out to python3 to regenerate an
    // identical blob on every run was the last thing keeping this front end
    // from working anywhere the wasm build does, and it is the only reason a
    // Python interpreter was required to boot the VM at all.
    //
    // Booting off the disk still needs the generator: it appends `root=`,
    // `rootfstype=` and a modules list to the command line, and growing a
    // string property means re-laying-out the blob. Dropping the loglevel does
    // not — "loglevel=7" and "loglevel=3" are the same length, so that one is
    // an in-place byte swap.
    let prebuilt = format!("{root}/kernels/boot.dtb");
    let dtb = if !use_as_root && std::path::Path::new(&prebuilt).exists() {
        let mut d = std::fs::read(&prebuilt).expect("boot.dtb");
        if quiet {
            patch_same_len(&mut d, b"loglevel=7", b"loglevel=3");
        }
        // Where the initramfs lands depends on its size, so these two are the
        // only properties that cannot be baked in.
        riscv_machine::fdt::patch_initrd(&mut d, initrd_load, initrd_end);
        d
    } else {
        let mut dtb_cmd = Command::new("python3");
        dtb_cmd
            .arg(format!("{root}/kernels/gen_dtb_v2.py"))
            .arg(format!("{initrd_load:#x}"))
            .arg(format!("{initrd_end:#x}"))
            .current_dir(format!("{root}/kernels"));
        if quiet {
            dtb_cmd.env("BOOT_LOGLEVEL", "3");
        }
        if use_as_root {
            dtb_cmd.env("ROOT_DEVICE", "/dev/vda");
        }
        let out = dtb_cmd.output().expect("running gen_dtb_v2.py");
        assert!(out.status.success(), "dtb gen failed: {}", String::from_utf8_lossy(&out.stderr));
        std::fs::read(format!("{root}/kernels/virt.dtb")).expect("virt.dtb")
    };
    let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
    bus.load_blob(dtb_load, &dtb);

    if let Some(path) = &disk {
        match riscv_devices::virtio_blk::file_backend::FileBackend::open(path, false) {
            Ok(be) => {
                let sectors = be.capacity_sectors();
                let blk = riscv_devices::VirtioBlk::new(Box::new(be));
                let irq = bus.attach_virtio(Box::new(blk)).expect("a free virtio slot");
                eprintln!(
                    "riscv-vm: disk {} ({} MiB) on virtio-mmio, hwirq {irq} -> /dev/vda",
                    path.rsplit('/').next().unwrap_or(path),
                    sectors * 512 / (1024 * 1024)
                );
            }
            Err(e) => eprintln!("riscv-vm: cannot open disk {path}: {e} — booting without it"),
        }
    }

    // --net attaches a virtio-net card. The host side is currently a sink that
    // counts frames; the browser build will hand these queues to v86s
    // fake_network.js, which terminates TCP and forwards it over WISP.
    let want_net = args.iter().any(|a| a == "--net");
    // --net-verbose narrates every frame in each direction.
    let net_verbose = args.iter().any(|a| a == "--net-verbose");
    if want_net {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        if let Some((irq, _q)) = bus.attach_virtio_net(mac) {
            eprintln!(
                "riscv-vm: virtio-net on hwirq {irq}, MAC 52:54:00:12:34:56 -> eth0"
            );
        }
    }

    let mut s = Supervisor::new(kernel_load, 0);
    s.priv_level = Privilege::Supervisor;
    s.cpu.write_reg(10, 0); // a0 = hartid
    s.cpu.write_reg(11, dtb_load); // a1 = dtb
    s.cpu.write_reg(2, DRAM_BASE + dram_size as u64 - 0x10000); // sp
    s.medeleg = 0xB1FF;
    s.mideleg = 0x2A2;

    eprintln!(
        "riscv-vm: {mem_mib} MiB, kernel {} ({} KiB), initrd {} KiB",
        kernel_path.rsplit('/').next().unwrap_or(&kernel_path),
        kernel.len() / 1024,
        initrd.len() / 1024
    );
    eprintln!("riscv-vm: Ctrl-A x to quit, Ctrl-A a to send a literal Ctrl-A\r");

    let _raw = RawTerminal::enable();

    // stdin blocks, so read it on its own thread and hand bytes to the VM loop.
    let (tx, rx) = mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for b in &buf[..n] {
                        if tx.send(*b).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut prev_uart = 0usize;
    let mut prev_sbi = 0usize;
    let mut escape_armed = false;
    let mut running = true;

    // Polling stdin and flushing stdout on every instruction would cost more
    // than the emulation. At ~20M steps/s this still services the terminal a few
    // thousand times a second, which is far below human perception.
    const IO_POLL_INTERVAL: u64 = 4096;
    let mut step: u64 = 0;
    let net_time = std::env::var("RISCV_NET_TIME").is_ok();
    bus.net_trace = net_time;
    let mut prev_rx_empty = true;

    // RISCV_BUG_TRACE=1 records recent 64-bit stores and, on the first kernel
    // BUG() (an ebreak taken in S-mode), reports who last wrote the pointer the
    // kernel choked on. Costs a branch per store, so it is opt-in.
    let bug_trace = std::env::var("RISCV_BUG_TRACE").is_ok();
    if bug_trace {
        s.enable_store_trace(1 << 20);
        eprintln!("riscv-vm: store tracing on (1M entries)\r");
    }
    // RISCV_IRQ_REGCHECK=1 verifies that a plain interrupt return restores the
    // whole register file. The first violation names the register and the pc.
    if std::env::var("RISCV_IRQ_REGCHECK").is_ok() {
        s.check_irq_regs = true;
        eprintln!("riscv-vm: interrupt register-preservation check on\r");
    }
    // RISCV_WATCH_VALUE=<mask>:<expect> in hex reports every 64-bit store whose
    // value matches. Finds a packed field being written without needing to know
    // its address, which matters when module load addresses move between boots.
    if let Ok(spec) = std::env::var("RISCV_WATCH_VALUE") {
        let mut it = spec.split(':');
        let hex = |v: Option<&str>| {
            v.and_then(|x| u64::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok())
        };
        s.watch_mask = hex(it.next()).unwrap_or(0);
        s.watch_expect = hex(it.next()).unwrap_or(0);
        // Optional third field: require some bit set outside the mask, so a
        // packed field can be found without matching its bare payload.
        s.watch_outside_nonzero = it.next().map(|v| v.trim() == "1").unwrap_or(false);
        // Optional 4th/5th fields: only record stores from this pc range. Loaded
        // modules live below the kernel proper, so 0xffffffff00000000 ..
        // 0xffffffff7fffffff isolates module code from all the vmlinux noise.
        if let Some(lo) = hex(it.next()) {
            s.watch_pc_lo = lo;
        }
        if let Some(hi) = hex(it.next()) {
            s.watch_pc_hi = hi;
        }
        s.enable_store_trace(1);
        eprintln!(
            "riscv-vm: value watch mask={:#x} expect={:#x}\r",
            s.watch_mask, s.watch_expect
        );
    }
    // RISCV_WATCH_PCOFF=lo:hi matches the low 12 bits of pc. Module load
    // addresses move between boots but page offsets do not, so this pins a
    // specific instruction inside a module without knowing where it landed.
    if let Ok(spec) = std::env::var("RISCV_WATCH_PCOFF") {
        let mut it = spec.split(':');
        let hex = |v: Option<&str>| {
            v.and_then(|x| u64::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok())
        };
        // Comma-separated page offsets. The FIRST is the anchor: nothing is
        // recorded until it runs, and its page is then latched, so a hotter
        // function elsewhere in the module sharing an offset cannot drown it.
        for (i, f) in spec.split(',').take(8).enumerate() {
            if let Some(v) = hex(Some(f)) {
                s.watch_pcoff[i] = v;
            }
        }
        s.enable_store_trace(1);
        eprintln!("riscv-vm: pc-offset watch (anchor first) {:x?}\r", s.watch_pcoff);
    }
    let mut watch_reported = 0usize;
    let mut watch_loads_reported = 0usize;
    let mut reported_irq_mismatch = false;
    let mut reported_bug = false;

    while running {
        bus.tick();
        if bug_trace && !reported_bug && s.priv_level == Privilege::Supervisor {
            // Peek before stepping: an ebreak here is a BUG()/BUG_ON firing.
            if s.cpu.pc != 0 {
                let raw = s.last_fetched_raw;
                if raw as u16 == 0x9002 {
                    reported_bug = true;
                    let bh = s.cpu.read_reg(11); // a1
                    eprintln!("\r\n=== kernel BUG at pc={:#x}, a0={:#x} a1={:#x} ===\r",
                        s.cpu.pc, s.cpu.read_reg(10), bh);
                    let writers = s.stores_to(bh);
                    eprintln!("stores to the a1 pointer itself: {}\r", writers.len());
                    // The interesting question is who wrote the *slot* that held
                    // this pointer, so scan for stores whose VALUE was this bh.
                    let mut n = 0;
                    for k in 0..s.store_ring.len() {
                        let i = (s.store_head + k) % s.store_ring.len();
                        let (pc, addr, val) = s.store_ring[i];
                        if val == bh && pc != 0 {
                            eprintln!("  bh value {:#x} stored to {:#x} by pc={:#x}\r", val, addr, pc);
                            n += 1;
                            if n > 12 { break; }
                        }
                    }
                    if n == 0 {
                        eprintln!("  nothing in the trace ever stored that value — \
                                   the register was not loaded from traced memory\r");
                    }
                    if s.check_irq_regs {
                        eprintln!(
                            "irq reg-check at BUG: {} snapshots, {} verified returns, mismatch={}\r",
                            s.irq_snaps, s.irq_compares, s.irq_mismatch.is_some()
                        );
                    }
                    eprintln!("last 40 stores before the BUG (pc -> [addr] = val):\r");
                    let n_ring = s.store_ring.len();
                    for k in n_ring.saturating_sub(40)..n_ring {
                        let i = (s.store_head + k) % n_ring;
                        let (pc, addr, val) = s.store_ring[i];
                        if pc != 0 {
                            eprintln!("  {pc:#x} -> [{addr:#x}] = {val:#x}\r");
                        }
                    }
                    // RISCV_WATCH_PC=lo:hi replays every traced store whose pc
                    // falls in that range, newest last. Used to ask "what did
                    // this exact instruction write?" — e.g. the `sd a0,0(s3)`
                    // in ext4_bread_batch that stores ext4_getblk's return.
                    if let Ok(spec) = std::env::var("RISCV_WATCH_PC") {
                        let mut it = spec.split(':');
                        let lo = it.next().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
                        let hi = it.next().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(lo);
                        eprintln!("stores from pc {lo:#x}..={hi:#x}:\r");
                        let mut hits: Vec<(u64, u64, u64)> = Vec::new();
                        for k in 0..s.store_ring.len() {
                            let i = (s.store_head + k) % s.store_ring.len();
                            let e = s.store_ring[i];
                            if e.0 >= lo && e.0 <= hi && e.0 != 0 {
                                hits.push(e);
                            }
                        }
                        for (pc, addr, val) in hits.iter().rev().take(16).rev() {
                            eprintln!("  pc={pc:#x} -> [{addr:#x}] = {val:#x}\r");
                        }
                        eprintln!("  ({} total in ring)\r", hits.len());
                    }
                }
            }
        }
        if let Status::Wfi = s.step(&mut bus) {
            // The guest is about to sleep, so answer anything it just sent
            // before deciding whether time may move. Waiting for the ordinary
            // IO_POLL_INTERVAL here is what made replies land a full NOHZ idle
            // period late.
            pump_net(&bus, net_verbose);

            // Single hart: only the timer can wake us, so jump emulated time to
            // the next deadline instead of spinning through the idle loop.
            // Without this an idle shell burns billions of instructions doing
            // nothing and the machine feels frozen.
            //
            // But only when nothing is already waiting for the guest. A queued
            // frame is an event that has *happened*; skipping the clock past it
            // backdates its delivery and inflates every round trip to the length
            // of a NOHZ idle.
            // When the queued reply actually reaches the guest. The gap between
            // this and "tx visible" is the half of the round trip we control.
            if net_time {
                let empty = bus.net.as_ref().is_none_or(|n| n.borrow().to_guest.is_empty());
                if empty != prev_rx_empty {
                    if empty {
                        eprintln!(
                            "[net t] rx delivered at {:.3} ms\r",
                            bus.read_mtime() as f64 / 10_000.0
                        );
                    }
                    prev_rx_empty = empty;
                }
            }

            //
            // How far is `idle_skip_mtime`'s call, since it depends on what the
            // devices have outstanding.
            let next = s.stimecmp.min(bus.get_mtimecmp());
            if next != u64::MAX {
                bus.idle_skip_mtime(next);
            }
        }

        while watch_loads_reported < s.watch_loads.len() {
            let (pc, addr, val) = s.watch_loads[watch_loads_reported];
            watch_loads_reported += 1;
            eprintln!("[load ] pc={pc:#x} <- [{addr:#x}] = {val:#x} ({val})\r");
        }
        while watch_reported < s.watch_hits.len() {
            let (pc, addr, val) = s.watch_hits[watch_reported];
            watch_reported += 1;
            eprintln!(
                "[watch] pc={pc:#x} -> [{addr:#x}] = {val:#018x}  (top5={:#x} low59={})\r",
                val >> 59,
                val & 0x07FF_FFFF_FFFF_FFFF
            );
        }

        if !reported_irq_mismatch {
            if let Some((sepc, reg, before, after)) = s.irq_mismatch {
                reported_irq_mismatch = true;
                const NAMES: [&str; 32] = [
                    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1",
                    "a2", "a3", "a4", "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7",
                    "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
                ];
                eprintln!(
                    "\r\n*** interrupt return did NOT restore x{reg} ({}) ***\r\n\
                     resuming pc = {sepc:#x}\r\n  before trap = {before:#x}\r\n  after  sret = {after:#x}\r",
                    NAMES[reg as usize]
                );
            }
        }

        step += 1;
        if step % IO_POLL_INTERVAL != 0 {
            continue;
        }

        pump_net(&bus, net_verbose);

        let mut wrote = false;
        if bus.uart_console.len() > prev_uart {
            let n = bus.uart_console.len();
            let _ = stdout.write_all(&bus.uart_console[prev_uart..n]);
            prev_uart = n;
            wrote = true;
        }
        if s.console_len > prev_sbi {
            let n = s.console_len.min(s.console_buf.len());
            let _ = stdout.write_all(&s.console_buf[prev_sbi..n]);
            prev_sbi = n;
            wrote = true;
        }
        if wrote {
            let _ = stdout.flush();
        }

        let mut input: Vec<u8> = Vec::new();
        for b in rx.try_iter() {
            match (escape_armed, b) {
                (false, 0x01) => escape_armed = true, // Ctrl-A
                (true, b'x') => {
                    running = false;
                    break;
                }
                (true, b'a') => {
                    escape_armed = false;
                    input.push(0x01);
                }
                (true, other) => {
                    escape_armed = false;
                    input.push(other);
                }
                (false, other) => input.push(other),
            }
        }
        if !input.is_empty() {
            bus.uart_push_input(&input);
        }
    }

    // Flush whatever the guest printed just before we stopped.
    if bus.uart_console.len() > prev_uart {
        let _ = stdout.write_all(&bus.uart_console[prev_uart..]);
    }
    let _ = stdout.flush();
    drop(_raw);
    if s.check_irq_regs {
        eprintln!(
            "riscv-vm: irq reg-check: {} snapshots, {} verified returns, mismatch={}",
            s.irq_snaps,
            s.irq_compares,
            s.irq_mismatch.is_some()
        );
    }
    eprintln!("\nriscv-vm: halted after {step} instructions");
}
