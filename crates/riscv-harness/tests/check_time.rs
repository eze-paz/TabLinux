use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;

#[test]
fn check_time_advances() {
    let mut bus = DeviceBus::new(1 << 30);
    let t0 = bus.read_u64(0x0200_bff8); // mtime
    eprintln!("mtime via bus = {}", t0);
    
    for _ in 0..1000 {
        bus.tick();
    }
    let t1 = bus.read_u64(0x0200_bff8);
    eprintln!("mtime after 1000 ticks = {} (delta={})", t1, t1 - t0);
    assert!(t1 > t0, "mtime did not advance!");
}
