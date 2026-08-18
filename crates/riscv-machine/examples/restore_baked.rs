//! Restore kernels/shell.snap and prove the baked-in setup survived: the 9p +
//! overlay modules are resident, the static network is up, and 9p mounts
//! WITHOUT re-insmod — i.e. the make_snapshot bake and the setupCommands(restore)
//! trim agree. Mirrors what the browser does on restore (rebind the disk, run
//! the trimmed setup), so a pass here means the shipped snapshot is safe.
//!
//!   cargo run --release -p riscv-machine --example restore_baked

use riscv_machine::Machine;
use riscv_devices::virtio_blk::BlockBackend;

struct FileDisk {
    data: Vec<u8>,
}
impl BlockBackend for FileDisk {
    fn capacity_sectors(&self) -> u64 {
        (self.data.len() / 512) as u64
    }
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool {
        let off = sector as usize * 512;
        if off + buf.len() > self.data.len() {
            return false;
        }
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
        true
    }
    fn write(&mut self, sector: u64, buf: &[u8]) -> bool {
        let off = sector as usize * 512;
        if off + buf.len() > self.data.len() {
            return false;
        }
        self.data[off..off + buf.len()].copy_from_slice(buf);
        true
    }
}

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let snap = std::fs::read(format!("{root}/kernels/shell.snap")).expect("snapshot");
    let disk = std::fs::read(format!("{root}/kernels/disk-ext4.img")).expect("disk");

    let mut m = Machine::restore(&snap).expect("RESTORE FAILED");
    // The browser rebinds its OPFS disk on restore; here, the module disk.
    assert!(m.bus.attach_blk_backend(Box::new(FileDisk { data: disk })), "no blk slot to rebind");
    eprintln!("[restored at {}M steps]", m.steps / 1_000_000);

    let mut console = String::new();
    // Settle at the prompt.
    for _ in 0..5 {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
    }

    // The trimmed restore setup: NO insmod, NO modprobe, NO ip — those are baked.
    let verify = concat!(
        "echo ---VERIFY---\n",
        "grep -q 9p /proc/modules && echo MOD_9P_RESIDENT || echo MOD_9P_MISSING\n",
        "grep -q overlay /proc/modules && echo MOD_OVERLAY_RESIDENT || echo MOD_OVERLAY_MISSING\n",
        "ip addr show eth0 2>&1 | grep -q 192.168.86.100 && echo NET_BAKED || echo NET_MISSING\n",
        "grep -q 192.168.86.1 /etc/resolv.conf 2>/dev/null && echo RESOLV_BAKED || echo RESOLV_MISSING\n",
        "mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk 2>&1\n",
        "mkdir -p /files\n",
        "mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files 2>&1 && echo 9P_MOUNTED || echo 9P_FAIL\n",
        "echo ===VDONE===\n",
    );
    m.console_input(verify.as_bytes());
    let start = m.steps;
    while !console.contains("===VDONE===") {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        assert!(m.steps - start < 4_000_000_000, "verify never finished:\n{console}");
    }

    let tail: String = console.chars().rev().take(900).collect::<String>().chars().rev().collect();
    eprintln!("--- tail ---\n{tail}\n--- end ---");

    let mod9p = console.contains("MOD_9P_RESIDENT");
    let modovl = console.contains("MOD_OVERLAY_RESIDENT");
    let net = console.contains("NET_BAKED");
    let mounted = console.contains("9P_MOUNTED");
    eprintln!("9p-module:{mod9p} overlay-module:{modovl} network:{net} 9p-mount:{mounted}");
    assert!(mod9p, "9p module not resident after restore — bake did not take");
    assert!(modovl, "overlay module not resident after restore");
    assert!(net, "static network not baked into the snapshot");
    assert!(mounted, "9p did not mount from the baked modules (no insmod ran)");
    eprintln!("BAKED SNAPSHOT VERIFIED");
}
