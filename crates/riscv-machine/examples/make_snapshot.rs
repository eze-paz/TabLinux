//! Boot Alpine to the rescue shell and write kernels/shell.snap[.gz].
//!
//! The browser then downloads a booted machine instead of interpreting the
//! boot: minutes become seconds. Regenerate after any change to the images or
//! to the snapshot format (the loader refuses mismatched versions).
//!
//!   cargo run --release -p riscv-machine --example make_snapshot

use riscv_machine::{BootImages, Machine};

/// Size of the persistent disk. The browser must create an OPFS file of
/// exactly this size — `attach_disk` refuses a mismatch, because a disk that
/// changed size under a filesystem corrupts it quietly.
pub const DISK_MB: usize = 256;

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    // The signature-stripped initramfs boots faster and the result is
    // identical once the shell is up; fall back to the pristine one.
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-nosig"))
        .or_else(|_| std::fs::read(format!("{root}/kernels/boot/initramfs-lts")))
        .expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("boot.dtb");

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        // Default 256 MiB — a disposable shell fits comfortably and the tab
        // footprint is ~4x smaller than the old 1 GiB. Override with
        // DRAM_MB=1024 for a roomier machine.
        dram_bytes: std::env::var("DRAM_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(256usize) << 20,
    });
    m.bus
        .attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        .expect("net slot");

    // A disk has to exist at boot or the guest never sees one: virtio-mmio has
    // no hotplug, and Linux probes the slots exactly once. We back it with the
    // real seed image (which carries the 9p modules under /mod) so the bake
    // script below can `insmod` from it — then it is unmounted before the save,
    // so the snapshot still records no mounted filesystem (only the capacity),
    // and the browser rebinds its own same-sized OPFS disk on restore.
    let disk = std::fs::read(format!("{root}/kernels/disk-ext4.img"))
        .expect("kernels/disk-ext4.img (run kernels/mkdisk.py first)");
    assert_eq!(disk.len(), DISK_MB * 1024 * 1024, "disk image is not {DISK_MB} MiB");
    m.bus
        .attach_virtio(Box::new(riscv_devices::VirtioBlk::new(Box::new(
            riscv_devices::MemBackend::new(disk),
        ))))
        .expect("blk slot");

    // The 9p share must be present at boot so the guest probes its virtio slot;
    // virtio-mmio has no hotplug. Its tree is host-side and empty here — the
    // browser re-seeds it from OPFS on restore and mounts it fresh, so the
    // snapshot deliberately captures it unmounted (a mounted 9p would freeze
    // in-kernel fids that the restored, empty device could not honour).
    m.bus.attach_9p("shared").expect("9p slot");

    let t0 = std::time::Instant::now();
    let mut console = String::new();
    while !console.contains("recovery shell") {
        m.run(50_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        assert!(m.steps < 4_000_000_000, "never reached the shell:\n{console}");
    }
    m.run(20_000_000);
    console.push_str(&String::from_utf8_lossy(&m.take_console()));

    // Bake the disk-independent setup into the snapshot so the browser skips it
    // on every restore: load the kernel modules (once resident they live in
    // kernel RAM, which the snapshot captures) and bring the static network up.
    // The disk is mounted only long enough to read the modules, then UNMOUNTED
    // and synced so the saved image still has no mounted filesystem — the
    // browser rebinds its own disk and mounts it fresh. The overlay mounts, the
    // 9p mount and anything that writes to the persistent disk stay in the
    // browser's post-restore script (see setupCommands' `restore` branch).
    let bake = concat!(
        "mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk 2>&1\n",
        "insmod /mnt/disk/mod/netfs.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet_virtio.ko 2>&1\n",
        "insmod /mnt/disk/mod/9p.ko 2>&1\n",
        "modprobe overlay 2>&1\n",
        "sync; umount /mnt/disk 2>&1\n",
        "ip link set eth0 up 2>&1\n",
        "ip addr add 192.168.86.100/24 dev eth0 2>&1\n",
        "ip route add default via 192.168.86.1 2>&1\n",
        // resolv.conf goes to tmpfs /etc, which becomes the overlay's lowerdir on
        // restore, so the baked copy shows through — one fewer line at boot.
        "echo nameserver 192.168.86.1 > /etc/resolv.conf 2>&1\n",
        // Save the tty with echo OFF and kernel printk hushed, so EVERY
        // post-restore setup command — including the first one — is silent. The
        // setup script re-enables both at its tty step. (Output still shows: this
        // only stops the shell echoing typed input, so ===BAKED=== is still seen.)
        "stty -F /dev/ttyS0 -echo 2>&1; dmesg -n 1 2>&1\n",
        "echo ===BAKED===\n",
    );
    m.console_input(bake.as_bytes());
    let start = m.steps;
    while !console.contains("===BAKED===") {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        assert!(m.steps - start < 4_000_000_000, "bake never finished:\n{console}");
    }
    // Confirm the modules actually loaded and the disk is back off before saving.
    assert!(!console.contains("can't open") && !console.contains("insmod: "), "a module failed to load:\n{console}");
    // Let the shell settle back at its read() so the snapshot resumes at a quiet
    // prompt with nothing half-typed.
    m.run(20_000_000);
    console.push_str(&String::from_utf8_lossy(&m.take_console()));

    eprintln!("shell + bake at {}M steps, {:.0}s; saving…", m.steps / 1_000_000, t0.elapsed().as_secs_f64());
    let snap = m.save().expect("save");
    let path = format!("{root}/kernels/shell.snap");
    std::fs::write(&path, &snap).expect("write");
    eprintln!("{path}: {} MiB", snap.len() / (1024 * 1024));

    // .gz alongside, for serving: Chrome inflates it with DecompressionStream,
    // so the wasm side needs no decompressor.
    let st = std::process::Command::new("gzip")
        .args(["-kf6", &path])
        .status()
        .expect("gzip");
    assert!(st.success());
    let gz = std::fs::metadata(format!("{path}.gz")).expect("gz").len();
    eprintln!("{path}.gz: {} MiB", gz / (1024 * 1024));
}
