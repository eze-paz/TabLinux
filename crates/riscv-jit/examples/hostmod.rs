//! A stand-in for the emulator's own wasm module, exporting the load/store
//! functions that generated blocks import.
//!
//! This exists because measuring against JS closures gives the wrong answer,
//! twice over. A wasm-to-JS call costs ~32 ns and a wasm-to-wasm one ~6 ns, so
//! a benchmark wired to JS imports says memory accesses are ruinous and pushes
//! the design towards inlining a software TLB. In production the host is the
//! emulator's own wasm module and the calls are wasm-to-wasm.
//!
//! The bodies mirror MemBus in difftest.rs exactly: mask the guest address into
//! a 4 KiB scratch region, align it down to the access width, and go. If the
//! two ever drift the differential test fails, which is the point of having
//! both.
//!
//!   cargo run --release -p riscv-jit --example hostmod

use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction as W, MemArg, MemoryType, Module, TypeSection, ValType,
};

/// Must match bench/jit-difftest.js and bench/jit-multi.js.
const MEM_BASE: u64 = 16384;
const MEM_BYTES: i32 = 4096;

/// Loads: (addr, pc) -> value, zero-extended. Stores: (addr, val, pc).
///
/// `pc` is unused here. It is in the signature because the real host records it
/// to unwind to when an access faults, and the generated code has to pass it
/// either way.
fn build() -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I64, ValType::I64], [ValType::I64]); // 0: load
    types.ty().function([ValType::I64, ValType::I64, ValType::I64], []); // 1: store

    let mut imports = ImportSection::new();
    imports.import("env", "mem", EntityType::Memory(MemoryType {
        minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None,
    }));

    let mut funcs = FunctionSection::new();
    for _ in 0..4 {
        funcs.function(0);
    }
    for _ in 0..4 {
        funcs.function(1);
    }

    let mut exports = ExportSection::new();
    for (i, name) in ["load8u", "load16u", "load32u", "load64"].iter().enumerate() {
        exports.export(name, ExportKind::Func, i as u32);
    }
    for (i, name) in ["store8", "store16", "store32", "store64"].iter().enumerate() {
        exports.export(name, ExportKind::Func, 4 + i as u32);
    }

    let mut code = CodeSection::new();

    // Address: (addr & (MEM_BYTES-1)) aligned down to the access width. The
    // MemArg offset then adds the region base, which the engine folds in.
    let addr = |f: &mut Function, size_log2: u32| {
        f.instruction(&W::LocalGet(0));
        f.instruction(&W::I32WrapI64);
        f.instruction(&W::I32Const((MEM_BYTES - 1) & !((1 << size_log2) - 1)));
        f.instruction(&W::I32And);
    };

    for size_log2 in 0..4u32 {
        let mut f = Function::new([]);
        addr(&mut f, size_log2);
        let m = MemArg { offset: MEM_BASE, align: size_log2, memory_index: 0 };
        f.instruction(&match size_log2 {
            0 => W::I64Load8U(m),
            1 => W::I64Load16U(m),
            2 => W::I64Load32U(m),
            _ => W::I64Load(m),
        });
        f.instruction(&W::End);
        code.function(&f);
    }

    for size_log2 in 0..4u32 {
        let mut f = Function::new([]);
        addr(&mut f, size_log2);
        f.instruction(&W::LocalGet(1)); // value
        let m = MemArg { offset: MEM_BASE, align: size_log2, memory_index: 0 };
        f.instruction(&match size_log2 {
            0 => W::I64Store8(m),
            1 => W::I64Store16(m),
            2 => W::I64Store32(m),
            _ => W::I64Store(m),
        });
        f.instruction(&W::End);
        code.function(&f);
    }

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

fn main() {
    std::fs::write("/tmp/jit_host.wasm", build()).unwrap();
    eprintln!("wrote /tmp/jit_host.wasm");
}
