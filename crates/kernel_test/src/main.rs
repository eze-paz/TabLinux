#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let sp: u64 = 0x81ff0000;
    unsafe {
        core::arch::asm!(
            "mv sp, {sp}",
            sp = in(reg) sp,
        );
    }

    extern "C" { fn _trap(); }
    unsafe {
        core::arch::asm!(
            "csrw stvec, {addr}",
            addr = in(reg) _trap as u64,
        );
    }

    let msg = b"Hello Kernel";
    unsafe {
        core::arch::asm!(
            "li a7, 0x4442434E",
            "li a6, 0",
            "mv a0, {len}",
            "mv a1, {ptr}",
            "ecall",
            ptr = in(reg) msg.as_ptr(),
            len = in(reg) msg.len(),
            out("a7") _, out("a6") _, out("a0") _, out("a1") _,
        );
    }

    let timer_delta: u64 = 50;
    unsafe {
        core::arch::asm!(
            "li a7, 0x54494D45",
            "li a6, 0",
            "li a0, 0",
            "mv a1, {delta}",
            "ecall",
            delta = in(reg) timer_delta,
            out("a7") _, out("a6") _, out("a0") _, out("a1") _,
        );
    }

    unsafe {
        core::arch::asm!(
            "li t0, 32",
            "csrs sie, t0",
            "li t0, 2",
            "csrs sstatus, t0",
        );
    }

    loop {
        unsafe { core::arch::asm!("wfi"); }
    }
}

#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn _trap() {
    unsafe {
        core::arch::naked_asm!(
            ".align 4",
            "addi sp, sp, -16",
            "sd x10, 0(sp)",
            "sd x11, 8(sp)",
            "li x11, 0x10000000",
            "li x10, 84",
            "sd x10, 0(x11)",
            "ld x10, 0(sp)",
            "ld x11, 8(sp)",
            "addi sp, sp, 16",
            "sret",
        );
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
