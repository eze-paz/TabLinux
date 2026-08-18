//! The wasm build is only as good as this crate, and this crate is only
//! believable if the same images that boot natively also boot through
//! `Machine`. So boot them, here, where a failure is a stack trace rather than
//! a blank canvas in a browser tab.
//!
//! Deliberately stops at "Unpacking initramfs" rather than running to a shell:
//! that milestone is what proves the two things this crate actually does —
//! placing the images, and rewriting the devicetree's initrd addresses to match.
//! Booting all the way to userspace is `riscv-harness`'s job and costs 45s.

use riscv_machine::{BootImages, Machine};

fn root() -> String {
    format!("{}/../..", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn boots_far_enough_to_prove_the_devicetree_is_right() {
    let root = root();
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("dtb");

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });

    let mut console = String::new();
    for _ in 0..60 {
        m.run(5_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        if console.contains("Unpacking initramfs") {
            break;
        }
    }

    // The banner proves the kernel got a devicetree it could parse at all; a
    // bad `a1` or a mangled blob dies silently before this.
    assert!(
        console.contains("Linux version"),
        "kernel never printed its banner; console so far:\n{console}"
    );
    // This one is the point: the kernel only unpacks an initramfs it found via
    // linux,initrd-start/end, which `fdt::patch_initrd` wrote.
    assert!(
        console.contains("Unpacking initramfs"),
        "kernel booted but never found the initrd — devicetree patch is wrong:\n{console}"
    );
}
