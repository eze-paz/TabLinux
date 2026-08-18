//! What does a guest memory access cost in generated code?
//!
//! Coverage says loads and stores are mandatory: without them the JIT projects
//! to 1.48x, with them 3.43x. There are two ways to emit one, and they differ
//! enormously in build cost:
//!
//!   1. Call an imported host function per access. Trivial to write, and the
//!      host already has `translate` and the bus. Costs a cross-module call.
//!   2. Inline a software TLB probe and fall back to (1) only on a miss. This
//!      is what QEMU does. Much faster if import calls are expensive, and a
//!      great deal more work -- the TLB has to be laid out in linear memory,
//!      kept in sync, and a fault needs a precise guest PC to unwind to.
//!
//! Build (2) only if (1) is too slow. This emits both shapes so the difference
//! can be measured instead of assumed.
//!
//!   cargo run --release -p riscv-jit --example importcost && node jit-import.js

use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    ImportSection, Instruction as W, MemArg, MemoryType, Module, TypeSection, ValType,
};

const N: usize = 12;

/// Twelve loads, each through an imported host function.
fn via_import() -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I64], [ValType::I64]); // 0: host load
    types.ty().function([ValType::I32], []); // 1: run(regs)

    let mut imports = ImportSection::new();
    imports.import("env", "mem", EntityType::Memory(MemoryType {
        minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None,
    }));
    imports.import("env", "load", EntityType::Function(0));

    let mut funcs = FunctionSection::new();
    funcs.function(1);

    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 1); // func 0 is the import

    let mut f = Function::new([]);
    for k in 0..N {
        f.instruction(&W::LocalGet(0));
        // address = a plausible guest address, varied per access
        f.instruction(&W::I64Const(0x8000_0000 + (k as i64) * 8));
        f.instruction(&W::Call(0));
        f.instruction(&W::I64Store(MemArg { offset: (k as u64) * 8, align: 3, memory_index: 0 }));
    }
    f.instruction(&W::End);

    let mut code = CodeSection::new();
    code.function(&f);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

/// Twelve loads straight out of linear memory: the speed of light for this,
/// what an inlined TLB probe would approach on a hit.
fn inline_loads() -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], []);

    let mut imports = ImportSection::new();
    imports.import("env", "mem", EntityType::Memory(MemoryType {
        minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None,
    }));

    let mut funcs = FunctionSection::new();
    funcs.function(0);

    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);

    let mut f = Function::new([]);
    for k in 0..N {
        f.instruction(&W::LocalGet(0));
        f.instruction(&W::LocalGet(0));
        f.instruction(&W::I32Load(MemArg { offset: 2048 + (k as u64) * 8, align: 2, memory_index: 0 }));
        f.instruction(&W::I64ExtendI32U);
        f.instruction(&W::I64Store(MemArg { offset: (k as u64) * 8, align: 3, memory_index: 0 }));
    }
    f.instruction(&W::End);

    let mut code = CodeSection::new();
    code.function(&f);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

/// A stand-in for the emulator's own wasm module: exports the `load` that
/// generated code imports.
///
/// This matters because the first measurement used a JS closure as the import
/// and got 18.3 ns, which is a wasm-to-JS call and not what the real system
/// does. In production the slow path is a function exported by the Rust module,
/// and V8 compiles a wasm-to-wasm imported call very differently. Measuring the
/// wrong one would have sent this straight into building an inlined TLB.
fn host_module() -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I64], [ValType::I64]);

    let mut imports = ImportSection::new();
    imports.import("env", "mem", EntityType::Memory(MemoryType {
        minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None,
    }));

    let mut funcs = FunctionSection::new();
    funcs.function(0);

    let mut exports = ExportSection::new();
    exports.export("load", ExportKind::Func, 0);

    // Do a real load so this is not optimised into nothing: mask the guest
    // address down into the shared memory and read it, which is roughly the
    // shape of the work the real slow path ends with.
    let mut f = Function::new([]);
    f.instruction(&W::LocalGet(0));
    f.instruction(&W::I32WrapI64);
    f.instruction(&W::I32Const(0xFFF));
    f.instruction(&W::I32And);
    f.instruction(&W::I64Load(MemArg { offset: 4096, align: 3, memory_index: 0 }));
    f.instruction(&W::End);

    let mut code = CodeSection::new();
    code.function(&f);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

fn main() {
    std::fs::write("/tmp/imp_host.wasm", host_module()).unwrap();
    std::fs::write("/tmp/imp_call.wasm", via_import()).unwrap();
    std::fs::write("/tmp/imp_inline.wasm", inline_loads()).unwrap();
    eprintln!("wrote /tmp/imp_call.wasm and /tmp/imp_inline.wasm ({N} accesses each)");
    eprintln!("now run: node jit-import.js");
}
