//! Restore the snapshot and time each post-restore setup step separately, to
//! find where the ~18 s browser boot actually goes. Same engine/cold-JIT
//! behaviour as the browser, so the relative costs carry over.
//!
//!   cargo run --release -p riscv-machine --example restore_time

use riscv_machine::Machine;
use riscv_devices::virtio_blk::BlockBackend;

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

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let snap = std::fs::read(format!("{root}/kernels/shell.snap")).expect("snap");
    let disk = std::fs::read(format!("{root}/kernels/disk-ext4.img")).expect("disk");
    let mut m = Machine::restore(&snap).expect("restore");
    m.bus.attach_blk_backend(Box::new(FileDisk { data: disk }));

    let mut console = String::new();
    for _ in 0..5 { m.run(20_000_000); console.push_str(&String::from_utf8_lossy(&m.take_console())); }

    // Each step ends with `echo <TAG>`; we time wall-clock + guest steps between tags.
    let steps: &[(&str, &str)] = &[
        ("disk-mount",  "mkdir -p /mnt/disk; mount -t ext4 /dev/vda /mnt/disk 2>/dev/null; echo T_DISK"),
        ("9p-mount",    "mkdir -p /files; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files 2>/dev/null; echo T_9P"),
        ("overlays-x6", "for d in etc usr lib bin sbin var; do mkdir -p /mnt/disk/ovl/$d/u /mnt/disk/ovl/$d/w; mount -t overlay ovl-$d -o lowerdir=/$d,upperdir=/mnt/disk/ovl/$d/u,workdir=/mnt/disk/ovl/$d/w /$d 2>/dev/null; done; echo T_OVL"),
        ("apk-init",    "mkdir -p /etc/apk /lib/apk/db /var/cache/apk; touch /lib/apk/db/installed /etc/apk/world; echo T_APK"),
    ];

    // REALTIME=1 feeds a wall-clock into host_ns each slice, exactly as the
    // browser's set_host_ns does, so guest waits are held to real time.
    let realtime = std::env::var("REALTIME").is_ok();
    let wall = std::time::Instant::now();
    for (name, cmd) in steps {
        let tag = cmd.rsplit("echo ").next().unwrap().trim();
        let s0 = m.steps;
        let t0 = std::time::Instant::now();
        m.console_input(format!("{cmd}\n").as_bytes());
        let start = m.steps;
        while !console.contains(tag) {
            if realtime { m.host_ns = wall.elapsed().as_nanos() as u64; }
            m.run(20_000_000);
            console.push_str(&String::from_utf8_lossy(&m.take_console()));
            assert!(m.steps - start < 20_000_000_000, "step {name} hung");
        }
        let secs = t0.elapsed().as_secs_f64();
        let msteps = (m.steps - s0) / 1_000_000;
        eprintln!("{:<12} {:6.2}s  {:>6}M steps  ({:.0} MIPS)", name, secs, msteps, msteps as f64 / secs);
    }
    eprintln!("done");
}
