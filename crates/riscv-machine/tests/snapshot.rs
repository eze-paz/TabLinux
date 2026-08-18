//! Snapshot correctness, proven by determinism.
//!
//! The machine is fully deterministic: same images, same inputs, same
//! instruction stream. So the strongest possible test is also the simplest —
//! run A boots straight through N+M steps; run B boots N steps, saves,
//! restores into a completely fresh machine, and runs M more. If one
//! architectural bit failed to round-trip, the streams diverge and the console
//! outputs or final PCs differ. No per-field assertions to forget.
//!
//! N is chosen past MMU-on, module loading, and virtio-net probe, so the
//! snapshot covers satp, negotiated virtqueues, and live interrupt state, not
//! just early-boot trivia.

use riscv_machine::{BootImages, Machine};

const N: u64 = 700_000_000; // deep enough for MMU + modules + virtio probe
const M: u64 = 50_000_000;

fn boot() -> (Machine, Vec<u8>, Vec<u8>, Vec<u8>) {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("dtb");
    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });
    m.bus
        .attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        .expect("net slot");
    (m, kernel, initrd, dtb)
}

#[test]
fn restore_is_indistinguishable_from_never_having_stopped() {
    // Run A: straight through.
    let (mut a, ..) = boot();
    let mut console_a = Vec::new();
    a.run(N);
    a.take_console(); // pre-snapshot output is not part of the comparison
    a.run(M);
    console_a.extend(a.take_console());

    // Run B: boot, save, restore into a fresh machine, continue.
    let (mut b0, ..) = boot();
    b0.run(N);
    let snap = b0.save().expect("save");
    drop(b0);
    let mut b = Machine::restore(&snap).expect("restore");
    assert_eq!(b.steps, N, "step counter must survive");
    b.run(M);
    let console_b = b.take_console();

    assert_eq!(
        a.cpu.cpu.pc, b.cpu.cpu.pc,
        "PCs diverged: the snapshot missed architectural state"
    );
    assert_eq!(a.cpu.cpu.x, b.cpu.cpu.x, "register files diverged");
    assert_eq!(
        String::from_utf8_lossy(&console_a),
        String::from_utf8_lossy(&console_b),
        "console output diverged after restore"
    );

    eprintln!(
        "snapshot: {} MiB sparse (of {} MiB DRAM), identical after {}M more steps",
        snap.len() / (1024 * 1024),
        1024,
        M / 1_000_000
    );
}
