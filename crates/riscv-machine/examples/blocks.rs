//! Run the real block former over a boot and check what it produces.
//!
//! The coverage probe measured an idealised subset scan: for every retired
//! instruction, is it compilable, and how long are the runs. This measures the
//! thing that will actually run — lazy formation driven by the interpreter's
//! `block_start` flag, keyed on physical address, with a hotness threshold.
//! Those differ, and the difference is what matters:
//!
//!   * only blocks entered `HOT` times are formed, so the idealised coverage is
//!     an upper bound
//!   * blocks are keyed physically and the table is direct-mapped, so
//!     collisions evict
//!   * a run stops at a page boundary as well as at the first uncompilable
//!     instruction
//!
//! Also asserts the invariants the compiler relies on, because a violation here
//! becomes silent guest corruption later: every instruction in a formed run is
//! compilable, no run is empty, no run spans a page.

use riscv_machine::jit::Jit;
use riscv_machine::{BootImages, Machine};

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-nosig"))
        .or_else(|_| std::fs::read(format!("{root}/kernels/boot/initramfs-lts")))
        .expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("boot.dtb");

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });
    m.bus
        .attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        .expect("net slot");
    m.bus
        .attach_virtio(Box::new(riscv_devices::VirtioBlk::new(Box::new(
            riscv_devices::MemBackend::new(vec![0u8; 256 * 1024 * 1024]),
        ))))
        .expect("blk slot");

    let mut jit = Jit::new();
    let mut collected: Vec<(u64, Vec<(riscv_core::types::Instr, u8)>)> = Vec::new();

    eprint!("booting");
    let mut console = String::new();
    let mut steps = 0u64;
    // A bounded slice of the boot rather than all of it: this steps one
    // instruction at a time so the former sees every entry, which is far slower
    // than Machine::run. 200M reaches well past kernel init into userspace.
    while steps < 200_000_000 {
        // Step one instruction at a time so the block former sees every entry.
        // Far slower than Machine::run, which is fine: this is a probe.
        for _ in 0..2_000_000 {
            m.bus.tick();
            if m.cpu.block_start {
                jit.on_block_start(&mut m.cpu, &mut m.bus);
            }
            m.cpu.step(&mut m.bus);
            steps += 1;
        }
        collected.extend(jit.take_pending());
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        eprint!(".");

    }
    collected.extend(jit.take_pending());
    eprintln!();

    // Invariants. A violation is silent guest corruption once these runs are
    // being compiled, so fail loudly here instead.
    for (paddr, run) in &collected {
        assert!(!run.is_empty(), "empty run queued at {paddr:#x}");
        for (i, _) in run {
            assert!(
                riscv_machine::jit::compilable(i),
                "uncompilable instruction in a formed run at {paddr:#x}"
            );
        }
        let bytes: u64 = run.iter().map(|(_, w)| *w as u64).sum();
        assert!(
            (paddr >> 12) == ((paddr + bytes - 1) >> 12),
            "run at {paddr:#x} spans a page boundary"
        );
    }

    let total: usize = collected.iter().map(|(_, v)| v.len()).sum();
    let max = collected.iter().map(|(_, v)| v.len()).max().unwrap_or(0);

    println!("instructions retired   {steps}");
    println!("block entries seen     {}", jit.entries);
    println!("runs formed            {}", collected.len());
    println!("runs rejected          {}", jit.rejected);
    println!("instructions in runs   {total}");
    println!("mean run length        {:.2}", total as f64 / collected.len() as f64);
    println!("longest run            {max}");
    println!("\nall invariants hold: non-empty, fully compilable, page-contained");
}
