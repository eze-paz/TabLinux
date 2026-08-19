//! Interpreter throughput, in MIPS. The instrument for making the emulator
//! faster without guessing.
//!
//!   cargo run --release -p riscv-machine --example bench [user|boot]
//!
//! Two workloads, because they measure different code and disagree:
//!
//! * `user` (default) restores kernels/shell.snap and runs `yes > /dev/null`,
//!   a tight userspace loop. It measures fetch, decode and dispatch and very
//!   little else.
//! * `boot` cold-boots Alpine to the rescue shell: decompression, MMU setup,
//!   device probe, initramfs, thousands of traps and page faults. This is the
//!   one that resembles what `apk` spends its time in.
//!
//! Gating the per-instruction debug tracing was worth ~9% of a boot and
//! *nothing* on `user`. A change to kernel or device code that is measured only
//! on `user` has not been measured.
//!
//! Design notes, because a benchmark that measures the wrong thing is worse
//! than none:
//!
//! * **MIPS is computed from the steps actually retired**, not from the budget
//!   handed to `run()`. `run()` returns early when the guest idles with a
//!   network frame outstanding, so those are not the same number, and using the
//!   budget silently understates a slice that stopped short.
//! * **The guest must be busy.** At an idle prompt the hart parks in WFI and
//!   the clock skips forward, so "instructions per second" would measure the
//!   idle path — flattering and meaningless.
//! * **Many short slices, headline from the fastest few.** Interference on a
//!   dev box is bursty. Over a long slice every pass catches some of it, so
//!   there is no clean sample to pick; over ~150ms slices a good fraction land
//!   in a quiet window. Averaging the top 5% keeps a single lucky slice from
//!   setting the number, and the spread across those samples is printed so an
//!   unusable run announces itself instead of being quietly believed.
//!
//! `boot` is scored differently, and the reason matters. A boot is not
//! homogeneous — self-decompression, device probe and page-fault storms run at
//! genuinely different speeds — so the fastest 5% of its slices are the fastest
//! *phase*, not the quietest *window*, and that estimator reported a 17.9%
//! spread on it. What a boot is instead is **deterministic**: two builds execute
//! the same instruction sequence, so slice `i` does identical work in both.
//! `boot` therefore writes one row per slice and bench/boot-ab.py compares them
//! pairwise, taking the median of the per-slice speedup ratios. Interference
//! inflates individual slices in one run or the other; a median over hundreds of
//! paired ratios does not care.
//!
//! Never compare two builds by running one after the other — host load drifts
//! over minutes and has produced a confident wrong answer here before. Use
//! ./ab.sh, which interleaves them.

use riscv_machine::{BootImages, Machine};
use std::time::Instant;

/// CPU time consumed by this process, in seconds.
///
/// /proc/self/schedstat field 0 is nanoseconds spent on a CPU. /proc/self/stat
/// only has utime/stime in 10ms USER_HZ ticks, which quantises a 150ms slice to
/// 7% — worse than the effects we are trying to resolve. Nanoseconds are what
/// make slices this short usable.
///
/// Wall clock counts every moment the OS ran something else; CPU time only
/// advances while we are on a core. It is not a complete defence — an SMT
/// sibling or an E-core still bills full CPU time for reduced throughput, which
/// is the bimodality visible in the worst passes — but it removes the largest
/// term. Linux-only; elsewhere this falls back to wall clock.
fn cpu_secs() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/schedstat").ok()?;
    let ns: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(ns / 1e9)
}

/// ~150ms of emulated work at current speeds. Small enough that quiet windows
/// exist, large enough to time. At 100M per slice the spread was 43%.
const SLICE: u64 = 2_000_000;

/// Warm-up for `user`: the host CPU reaching its boost clock and the guest's
/// loop settling both outlast the first slices.
const USER_WARMUP: u64 = 150_000_000;
const USER_PASSES: usize = 200;

/// Slices at the start of a boot to exclude from the statistics. The host is
/// still ramping and the guest is doing self-decompression that is not
/// representative of the rest. They are still executed — this only drops them
/// from the sample.
const BOOT_SKIP: usize = 30;

/// How many times to time `reference_work()` at startup to establish the
/// host's best speed. The minimum is used: interference can only make it
/// slower, so the fastest observation is the least contaminated.
const REF_CALIBRATE: usize = 15;

/// Stop the boot after this many slices instead of running it to the shell.
///
/// This is a thermal limit, not a statistical one. A full boot is ~1.2G
/// instructions and pins a 15W laptop part for over two minutes; four of those
/// back to back (one ABBA block) heats the package until it throttles, and the
/// throttling is what a null test then measures. Two runs of the *same binary*
/// came out 101.7s and 141.5s that way. 250 slices is 500M instructions -- past
/// decompression and well into device probe and page-fault work, still
/// deterministic, and short enough that the clock stays put.
const BOOT_MAX_SLICES: usize = 250;

/// A fixed lump of integer work, used to calibrate the host at the moment of
/// each measurement.
///
/// CPU time is immune to being descheduled but not to running at a different
/// clock: a slice measured while the core is at 1.2 GHz looks slower than the
/// same code at 3.5 GHz, and on a thermally-limited laptop under a hypervisor
/// the clock moves constantly and invisibly. Hardware counters would settle it,
/// but WSL2 does not virtualise the PMU -- `perf_event_open` with
/// `PERF_TYPE_HARDWARE` returns ENOENT, so cycles retired are simply not
/// available here.
///
/// This is the substitute. Running a known quantity of work immediately next to
/// the measurement gives a local yardstick: if the core is at half speed, both
/// the reference and the emulator take twice as long, and their ratio does not
/// move. `black_box` keeps the optimiser from folding it away or hoisting it out
/// of the loop, which would silently turn the yardstick into a constant.
#[inline(never)]
fn reference_work() -> u64 {
    let mut a: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..3_000_000u64 {
        a = a.wrapping_mul(6364136223846793005).wrapping_add(i);
        a ^= a >> 29;
        a = std::hint::black_box(a);
    }
    a
}

/// The host's best reference time, measured now. Everything is expressed
/// relative to this, so a slice run while the core is throttled is scaled back
/// up to what it would have been at full speed.
fn calibrate() -> f64 {
    let mut best = f64::MAX;
    for _ in 0..REF_CALIBRATE {
        let t = reference_secs();
        if t < best {
            best = t;
        }
    }
    best
}

/// Seconds of CPU time the reference work takes right now.
fn reference_secs() -> f64 {
    let c0 = cpu_secs();
    let t0 = Instant::now();
    std::hint::black_box(reference_work());
    let wall = t0.elapsed().as_secs_f64();
    match (cpu_secs(), c0) {
        (Some(a), Some(b)) if a > b => a - b,
        _ => wall,
    }
}

/// One timed slice: (steps retired, normalised cpu seconds), or None if the
/// guest stopped making progress.
///
/// "Normalised" means scaled to the host speed seen during calibration. If the
/// reference beside this slice ran 1.4x slower than its best, the core was
/// slow, and this slice's time is divided by 1.4 to compensate.
fn slice(m: &mut Machine, ref_base: f64) -> Option<(u64, f64)> {
    let c0 = cpu_secs();
    let t0 = Instant::now();
    let steps = m.run(SLICE);
    let wall = t0.elapsed().as_secs_f64();
    let secs = match (cpu_secs(), c0) {
        (Some(a), Some(b)) if a > b => a - b,
        _ => wall,
    };
    let refs = reference_secs();
    if steps == 0 || secs <= 0.0 || refs <= 0.0 {
        return None;
    }
    // Guard against a reference that came out faster than calibration: that is
    // measurement noise, not a core that exceeded its own best, and letting it
    // inflate a slice would manufacture speedups.
    let slowdown = (refs / ref_base).max(1.0);
    Some((steps, secs / slowdown))
}

fn report(mut mips: Vec<f64>, note: &str) {
    if mips.len() < 20 {
        println!("too few samples ({}) to say anything", mips.len());
        std::process::exit(1);
    }
    mips.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = mips[mips.len() / 2];

    // Mean of the fastest 5%, not the single best: one slice can be lucky in a
    // way that does not repeat, but ten agreeing slices are a property of the
    // code. Interference can only ever make a pass slower, so the fast end is
    // the least contaminated estimate of what the interpreter can do; the
    // median mostly reports how busy the laptop was.
    let top = &mips[mips.len() - mips.len() / 20..];
    let best = top.iter().sum::<f64>() / top.len() as f64;

    // Spread across that same top 5%, because a whole-range spread is dominated
    // by the worst pass and so never answers the question that matters: do the
    // fast samples agree with each other?
    let spread = (top[top.len() - 1] - top[0]) / top[top.len() - 1] * 100.0;

    println!();
    println!(
        "MIPS top-5% {best:.2}   median {median:.2}   worst {:.2}   top-spread {spread:.1}%   {note}",
        mips[0]
    );
    println!("BENCH mips={best:.2}");
    if spread > 5.0 {
        println!(
            "NOTE: {spread:.1}% spread across the top 5%. Deltas smaller than that \
             are noise; close other work and re-run before believing one."
        );
    }
}

/// Userspace throughput: restore a booted machine and spin in `yes`.
fn user(root: &str) {
    let snap = std::fs::read(format!("{root}/kernels/shell.snap")).unwrap_or_else(|e| {
        eprintln!("cannot read kernels/shell.snap: {e}");
        eprintln!("generate it with: cargo run --release -p riscv-machine --example make_snapshot");
        std::process::exit(1);
    });
    let mut m = Machine::restore(&snap).expect("restore (regenerate the snapshot if this fails)");

    // `yes` is in every busybox, and redirecting to /dev/null keeps the work
    // inside the guest rather than turning this into a console benchmark.
    m.console_input(b"yes > /dev/null\n");

    eprint!("calibrating host... ");
    let ref_base = calibrate();
    eprintln!("{:.2}ms reference", ref_base * 1e3);

    eprint!("warming up {}M... ", USER_WARMUP / 1_000_000);
    m.run(USER_WARMUP);
    let _ = m.take_console();
    eprintln!("done");

    let mut mips = Vec::new();
    for pass in 1..=USER_PASSES {
        let Some((steps, secs)) = slice(&mut m, ref_base) else {
            eprintln!("guest stopped making progress at pass {pass}");
            break;
        };
        mips.push(steps as f64 / secs / 1e6);
        if pass % 20 == 0 {
            eprint!(".");
        }
        let _ = m.take_console();
    }
    eprintln!();
    report(mips, "[user]");
}

/// Kernel-path throughput: a cold boot, sliced.
fn boot(root: &str, csv: Option<String>) {
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
    // Match the device set the snapshot and the browser boot with: virtio-mmio
    // has no hotplug and Linux probes the slots once, so a machine booted
    // without these is a different machine.
    m.bus
        .attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        .expect("net slot");
    m.bus
        .attach_virtio(Box::new(riscv_devices::VirtioBlk::new(Box::new(
            riscv_devices::MemBackend::new(vec![0u8; 256 * 1024 * 1024]),
        ))))
        .expect("blk slot");

    eprint!("calibrating host... ");
    let ref_base = calibrate();
    eprintln!("{:.2}ms reference", ref_base * 1e3);

    eprint!("booting");
    let mut rows: Vec<(u64, f64)> = Vec::new();
    let mut console = String::new();
    let mut n = 0usize;
    let mut total_steps = 0u64;
    let mut total_secs = 0.0;
    while !console.contains("recovery shell") && n < BOOT_MAX_SLICES {
        let Some((steps, secs)) = slice(&mut m, ref_base) else {
            eprintln!("\nguest stopped making progress after {n} slices");
            break;
        };
        n += 1;
        total_steps += steps;
        total_secs += secs;
        if n > BOOT_SKIP {
            rows.push((steps, secs));
        }
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        if n % 40 == 0 {
            eprint!(".");
        }
        assert!(m.steps < 4_000_000_000, "never reached the shell");
    }
    eprintln!();

    // Total CPU time over a deterministic instruction sequence is directly
    // comparable between builds. It is the honest headline for a single run,
    // but it swallows every hiccup the host had, so it is the coarse view;
    // bench/boot-ab.py's paired median is the sensitive one.
    println!(
        "boot: {} Msteps in {:.1}s cpu ({:.2} MIPS overall)",
        total_steps / 1_000_000,
        total_secs,
        total_steps as f64 / total_secs / 1e6
    );
    println!("BOOTCPU secs={total_secs:.2} steps={total_steps}");

    // No MIPS headline here on purpose: averaging the fast end of a
    // heterogeneous workload measures which phase is fastest, not how fast the
    // emulator is. Use bench/boot-ab.py.
    if let Some(path) = csv {
        let mut out = String::from("slice,steps,cpu_secs\n");
        for (i, (steps, secs)) in rows.iter().enumerate() {
            out.push_str(&format!("{i},{steps},{secs:.9}\n"));
        }
        std::fs::write(&path, out).expect("write csv");
        eprintln!("wrote {} slices to {path}", rows.len());
    } else {
        eprintln!("(pass a csv path as the 2nd arg to enable paired comparison)");
    }
}

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    match std::env::args().nth(1).unwrap_or_else(|| "user".into()).as_str() {
        "user" => user(&root),
        "boot" => boot(&root, std::env::args().nth(2)),
        other => {
            eprintln!("unknown workload {other:?}; expected `user` or `boot`");
            std::process::exit(2);
        }
    }
}
