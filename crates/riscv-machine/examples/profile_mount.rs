//! Sampling profiler for the post-restore setup. Restores the snapshot, runs the
//! overlay mounts + apk-init in small slices, samples the guest PC at each slice
//! boundary, and resolves the hot addresses against kernels/System.map. Kernel
//! text (ext4, block, vfs, copy-up core) resolves to names; overlay/9p live in
//! loadable modules above the kernel text, so they bucket as "[module]".
//!
//!   cargo run --release -p riscv-machine --example profile_mount

use riscv_machine::Machine;
use riscv_devices::virtio_blk::BlockBackend;
use std::collections::HashMap;

struct FileDisk { data: Vec<u8> }
impl BlockBackend for FileDisk {
    fn capacity_sectors(&self) -> u64 { (self.data.len() / 512) as u64 }
    fn read(&mut self, s: u64, b: &mut [u8]) -> bool {
        let o = s as usize * 512; if o + b.len() > self.data.len() { return false; }
        b.copy_from_slice(&self.data[o..o + b.len()]); true
    }
    fn write(&mut self, s: u64, b: &[u8]) -> bool {
        let o = s as usize * 512; if o + b.len() > self.data.len() { return false; }
        self.data[o..o + b.len()].copy_from_slice(b); true
    }
}

const KTEXT_LO: u64 = 0xffff_ffff_8000_0000;
const KTEXT_HI: u64 = 0xffff_ffff_80c2_0a62;

fn load_symbols(path: &str) -> Vec<(u64, String)> {
    let text = std::fs::read_to_string(path).expect("System.map");
    let mut syms: Vec<(u64, String)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let addr = u64::from_str_radix(it.next()?, 16).ok()?;
            let ty = it.next()?;
            let name = it.next()?;
            if matches!(ty, "T" | "t" | "W" | "w") { Some((addr, name.to_string())) } else { None }
        })
        .collect();
    syms.sort_by_key(|(a, _)| *a);
    syms
}

fn resolve<'a>(syms: &'a [(u64, String)], pc: u64) -> &'a str {
    if !(KTEXT_LO..=KTEXT_HI).contains(&pc) {
        return "[module: overlay/9p/etc]";
    }
    let i = syms.partition_point(|(a, _)| *a <= pc);
    if i == 0 { "[below text]" } else { &syms[i - 1].1 }
}

fn run_step(m: &mut Machine, console: &mut String, cmd: &str, tag: &str, hist: &mut HashMap<String, u32>, syms: &[(u64, String)]) -> (u64, u64) {
    m.console_input(format!("{cmd}\n").as_bytes());
    let s0 = m.steps;
    let mut samples = 0u64;
    while !console.contains(tag) {
        m.run(15_000); // small slices → many PC samples
        *hist.entry(resolve(syms, m.cpu.cpu.pc).to_string()).or_default() += 1;
        samples += 1;
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        assert!(m.steps - s0 < 30_000_000_000, "step hung");
    }
    (m.steps - s0, samples)
}

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let syms = load_symbols(&format!("{root}/kernels/System.map"));
    eprintln!("[{} text symbols]", syms.len());
    let snap = std::fs::read(format!("{root}/kernels/shell.snap")).expect("snap");
    let disk_path = std::env::var("DISK").unwrap_or_else(|_| format!("{root}/kernels/disk-ext4.img"));
    eprintln!("[disk: {disk_path}]");
    let disk = std::fs::read(&disk_path).expect("disk");
    let mut m = Machine::restore(&snap).expect("restore");
    m.bus.attach_blk_backend(Box::new(FileDisk { data: disk }));

    let mut console = String::new();
    for _ in 0..5 { m.run(20_000_000); console.push_str(&String::from_utf8_lossy(&m.take_console())); }

    // Prep (untimed): disk + 9p, so the overlay step profiles cleanly.
    run_step(&mut m, &mut console, "mkdir -p /mnt/disk; mount -t ext4 /dev/vda /mnt/disk 2>/dev/null; mkdir -p /files; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files 2>/dev/null; echo T_PREP", "T_PREP", &mut HashMap::new(), &syms);

    let mut hist: HashMap<String, u32> = HashMap::new();
    let (osteps, osamp) = run_step(&mut m, &mut console,
        "for d in etc usr lib bin sbin var; do mkdir -p /mnt/disk/ovl/$d/u /mnt/disk/ovl/$d/w; mount -t overlay ovl-$d -o lowerdir=/$d,upperdir=/mnt/disk/ovl/$d/u,workdir=/mnt/disk/ovl/$d/w /$d 2>/dev/null; done; echo T_OVL",
        "T_OVL", &mut hist, &syms);
    let (asteps, asamp) = run_step(&mut m, &mut console,
        "mkdir -p /etc/apk /lib/apk/db /var/cache/apk; touch /lib/apk/db/installed /etc/apk/world; echo T_APK",
        "T_APK", &mut hist, &syms);

    eprintln!("overlays: {}M steps, {} samples", osteps / 1_000_000, osamp);
    eprintln!("apk-init: {}M steps, {} samples", asteps / 1_000_000, asamp);
    let total: u32 = hist.values().sum();
    let mut top: Vec<_> = hist.into_iter().collect();
    top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    eprintln!("\n--- hottest (share of setup time) ---");
    for (name, c) in top.into_iter().take(25) {
        eprintln!("{:5.1}%  {}", 100.0 * c as f64 / total as f64, name);
    }
}
