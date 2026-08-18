//! Load kernels/shell.snap and report whether it restores. Exists because a
//! failed restore in the browser is a silent `undefined` and a 2-minute reboot.

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(format!("{root}/kernels/shell.snap")).expect("snapshot");
    println!("{} bytes on disk", bytes.len());
    match riscv_machine::Machine::restore(&bytes) {
        Some(m) => {
            println!("restored OK at step {}, pc {:#x}", m.steps, m.cpu.cpu.pc);
            let slots: Vec<String> = (0..8)
                .filter_map(|i| m.bus.virtio_device_id(i).map(|id| format!("slot{i}=id{id}")))
                .collect();
            println!("virtio: {}", slots.join(" "));
        }
        None => println!("RESTORE FAILED"),
    }
}
