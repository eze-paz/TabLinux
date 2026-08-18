use riscv_core::execute::Bus;
use riscv_devices::DeviceBus;
use riscv_supervisor::{Supervisor, types::Privilege, types::Satp};
use riscv_core::types::Status;

#[test]
fn debug_timer_kernel() {
    let data = std::fs::read("/home/aezequiel/riscv-vm/kernels/sbi_test/timer_test.bin")
        .expect("timer_test.bin missing");

    let mut bus = DeviceBus::new(256 * 1024 * 1024);
    let load_addr = 0x8020_0000u64;
    bus.load_blob(load_addr, &data);

    let pt_base = 0x8F00_0000u64;
    bus.write_u64(pt_base + 0,  (0u64 << 10) | 0x0F);
    bus.write_u64(pt_base + 16, (0x80_000u64 << 10) | 0x0F);

    let mut s = Supervisor::new(load_addr, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: pt_base >> 12 };
    s.mstatus.spp = true;
    s.mstatus.spie = true;
    s.mstatus.sie = true;
    s.mideleg |= 1 << 7;
    s.cpu.write_reg(2, 0x81FF_0000);

    for i in 0..60 {
        bus.tick();
        let status = s.step(&mut bus);
        let instr_at_pc = bus.read_u32(s.cpu.pc);
        eprintln!("step {:2}: PC={:#018x} status={:?}  pc={:#018x}  a3={:#018x}  instr[PC]={:#010x}",
            i, s.cpu.pc, status, s.cpu.pc, s.cpu.read_reg(13), instr_at_pc);
    }
    eprintln!("\nmtimecmp={} mtime={} mip={:#x} mie={:#x}",
        bus.read_u64(0x0200_4000), bus.read_u64(0x0200_BFF8), s.mip, s.mie);
    eprintln!("sepc={:#018x} scause={:#x}", s.sepc, s.scause);
    eprintln!("uart={:?}", String::from_utf8_lossy(&bus.uart_console));
}
