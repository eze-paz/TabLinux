//! Boot the real Alpine guest, attach a virtio-9p device, load the 9p modules
//! and mount the share — then read a host-seeded file and write one back.
//!
//! This is the oracle the unit test cannot be: it drives the actual Linux
//! 9pnet_virtio client, which is the only thing that can tell me my replies are
//! wire-correct in every field the kernel reads. Native, because iterating this
//! in the browser would be minutes per run.
//!
//!   cargo run --release -p riscv-machine --example test_9p
//!
//! Needs a disk image with the 9p modules under /mod (see the debugfs step in
//! the session notes): pass its path as argv[1], default /tmp/disk9p.img.

use riscv_machine::{BootImages, Machine};
use riscv_devices::{Virtio9p, VirtioBlk};
use riscv_devices::virtio_blk::BlockBackend;

/// File-backed block device, so the guest can read the injected modules.
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
    let disk_path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/disk9p.img".into());

    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("boot.dtb");
    let disk = std::fs::read(&disk_path).unwrap_or_else(|_| panic!("disk image {disk_path}"));

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });
    m.bus.attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).expect("net slot");
    m.bus
        .attach_virtio(Box::new(VirtioBlk::new(Box::new(FileDisk { data: disk }))))
        .expect("blk slot");

    // The share, seeded with one file so the first `cat` has something and a
    // subdir so `ls` shows both kinds.
    let mut p9 = Virtio9p::new("shared");
    p9.fs_mut().put_file("hello.txt", b"hi from the host\n".to_vec());
    p9.fs_mut().mkdir_p("sub");
    m.bus.attach_virtio(Box::new(p9)).expect("9p slot");

    // Boot to the rescue shell.
    let mut console = String::new();
    while !console.contains("recovery shell") {
        m.run(50_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        assert!(m.steps < 5_000_000_000, "never reached the shell:\n{console}");
    }
    m.run(20_000_000);
    console.push_str(&String::from_utf8_lossy(&m.take_console()));
    eprintln!("[booted at {}M steps]", m.steps / 1_000_000);

    // Drive the mount. insmod in dependency order — the transport (9pnet_virtio)
    // binds the already-probed device id 9, and 9p.ko is the filesystem `mount
    // -t 9p` needs. `2>&1` so any module or mount error lands in the console we
    // assert on. The sentinels bracket the output we care about.
    let script = concat!(
        "mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk 2>&1\n",
        "insmod /mnt/disk/mod/netfs.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet_virtio.ko 2>&1\n",
        "insmod /mnt/disk/mod/9p.ko 2>&1\n",
        "echo MODS_LOADED $?\n",
        "mkdir -p /mnt/shared\n",
        "mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /mnt/shared 2>&1\n",
        "echo MOUNTED $?\n",
        "echo --- LS ---; ls -la /mnt/shared 2>&1\n",
        "echo --- CAT ---; cat /mnt/shared/hello.txt 2>&1\n",
        "echo from-the-guest > /mnt/shared/reply.txt 2>&1\n",
        "echo --- READBACK ---; cat /mnt/shared/reply.txt 2>&1\n",
        "echo ===9P_DONE===\n",
    );
    m.console_input(script.as_bytes());

    // Run until the final sentinel or a step budget.
    let start = m.steps;
    while !console.contains("===9P_DONE===") {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        if m.steps - start > 8_000_000_000 {
            eprintln!("--- console so far ---\n{console}");
            panic!("9p script did not finish");
        }
    }

    // Show the tail so a failure is legible.
    let tail: String = console.chars().rev().take(1200).collect::<String>().chars().rev().collect();
    eprintln!("--- console tail ---\n{tail}\n--- end ---");

    let ok_mount = console.contains("MOUNTED 0");
    let ok_cat = console.contains("hi from the host");
    let ok_readback = console.contains("from-the-guest");
    eprintln!("mount ok: {ok_mount}   cat ok: {ok_cat}   readback ok: {ok_readback}");
    assert!(ok_mount, "9p mount failed");
    assert!(ok_cat, "did not read the host-seeded file");
    assert!(ok_readback, "guest write did not read back");
    eprintln!("9P WORKS");
}
