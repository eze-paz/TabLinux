//! Integration prototype: can the Rust host call generated blocks with no JS
//! in the hot path?
//!
//! This is the one unknown the whole integration rests on. The emulator's run
//! loop is Rust compiled to wasm and the compiled blocks are a separate module.
//! If entering a block has to go Rust -> JS -> generated wasm, that is a
//! wasm-to-JS boundary each way at roughly 32 ns, which would eat most of what
//! the JIT gains on the short blocks real code produces.
//!
//! The escape is that a wasm module may declare an active element segment on an
//! *imported* table. The generated module imports this module's
//! `__indirect_function_table` and writes its blocks into it at a known base.
//! In Rust on wasm a function pointer is a table index, so calling block `i` is
//! a `transmute` of `base + i` followed by an ordinary indirect call.
//!
//! Built as a plain cdylib with `--export-table` and `--export-memory`; no
//! wasm-bindgen, because the point is to measure the raw mechanism.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// Guest registers, and a scratch region standing in for guest RAM. Both live
/// in this module's linear memory, which the generated module imports — that
/// sharing is what lets compiled code touch guest state directly.
#[no_mangle]
pub static mut REGS: [u64; 33] = [0; 33]; // 33: slot 32 holds the fault flag generated code reads at regs+256

pub const MEM_BYTES: usize = 4096;
#[no_mangle]
pub static mut MEM: [u8; MEM_BYTES] = [0; MEM_BYTES];

/// Mask and align an access into the scratch region, matching MemBus in
/// difftest.rs and the JS harness byte for byte.
#[inline(always)]
fn map(addr: u64, size_log2: u32) -> usize {
    let a = (addr as usize) & (MEM_BYTES - 1);
    a & !((1usize << size_log2) - 1)
}

/// The memory path generated code imports. `pc` is unused here; the real host
/// records it so a faulting access can unwind to a precise guest PC.
///
/// `#[no_mangle] extern "C"` so these appear as plain wasm exports the
/// generated module can import by name.
macro_rules! loader {
    ($name:ident, $ty:ty, $shift:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(addr: u64, _pc: u64) -> u64 {
            let o = map(addr, $shift);
            unsafe {
                let p = core::ptr::addr_of!(MEM) as *const u8;
                let mut buf = [0u8; core::mem::size_of::<$ty>()];
                core::ptr::copy_nonoverlapping(p.add(o), buf.as_mut_ptr(), buf.len());
                <$ty>::from_le_bytes(buf) as u64
            }
        }
    };
}

macro_rules! storer {
    ($name:ident, $ty:ty, $shift:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(addr: u64, val: u64, _pc: u64) {
            let o = map(addr, $shift);
            unsafe {
                let p = core::ptr::addr_of_mut!(MEM) as *mut u8;
                let b = (val as $ty).to_le_bytes();
                core::ptr::copy_nonoverlapping(b.as_ptr(), p.add(o), b.len());
            }
        }
    };
}

loader!(load8u, u8, 0);
loader!(load16u, u16, 1);
loader!(load32u, u32, 2);
loader!(load64, u64, 3);
storer!(store8, u8, 0);
storer!(store16, u16, 1);
storer!(store32, u32, 2);
storer!(store64, u64, 3);

/// Byte offset of the register file, for the host to pass to a block.
#[no_mangle]
pub extern "C" fn regs_ptr() -> u32 {
    unsafe { core::ptr::addr_of!(REGS) as u32 }
}

#[no_mangle]
pub extern "C" fn mem_ptr() -> u32 {
    unsafe { core::ptr::addr_of!(MEM) as u32 }
}

/// Call block `index`, counted from `table_base`.
///
/// The transmute is the crux: on wasm a `fn` pointer is a table index, so an
/// integer index becomes a callable function. It is only sound because the
/// generated module has installed a function of exactly this signature at that
/// slot — which is why block indices and table slots are kept one-to-one, and
/// why runs that fail to compile still occupy a slot with a no-op body rather
/// than shifting everything after them.
#[no_mangle]
pub extern "C" fn call_block(table_base: u32, index: u32, regs: u32, pc: u64) {
    unsafe {
        let f: extern "C" fn(u32, u64) = core::mem::transmute(table_base + index);
        f(regs, pc);
    }
}

/// Enter `index` `iters` times, so the measurement excludes any JS loop
/// overhead and reflects what the emulator's own run loop would see.
#[no_mangle]
pub extern "C" fn call_block_n(table_base: u32, index: u32, regs: u32, pc: u64, iters: u32) {
    for _ in 0..iters {
        call_block(table_base, index, regs, pc);
    }
}

/// Rotate through `count` blocks `iters` times: the realistic pattern, and the
/// one that exposed a 293 ns dispatch cost when each block was its own module.
#[no_mangle]
pub extern "C" fn call_blocks_rotating(
    table_base: u32,
    count: u32,
    regs: u32,
    pc: u64,
    iters: u32,
) {
    let mut i = 0u32;
    for _ in 0..iters {
        call_block(table_base, i, regs, pc);
        i += 1;
        if i == count {
            i = 0;
        }
    }
}
