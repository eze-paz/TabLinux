//! Compile the fpl/fps loop shapes under the PRODUCTION configuration --
//! chain table and inline TLB present -- not just the bare one. The bare
//! compile() validated fine while the VM interpreted the same loop, and the
//! difference between the two configurations is exactly the TLB-hit fast path
//! the bare form never emits.
//!
//!   cargo run --release -p riscv-jit --example fpcheck

use riscv_core::types::Instr::{self, *};

fn check(name: &str, body: impl Fn(i32) -> (Instr, u8, i32)) {
    let fp = riscv_jit::FpCfg { fregs_base: 8192, fs_word: 8448 };
    let chain = riscv_jit::ChainCfg {
        base: 16384,
        gen_addr: 12288,
        entries: 8192,
        tlb: Some(riscv_jit::TlbCfg {
            read_base: 20480,
            write_base: 24576,
            entries: 1024,
            gen_addr: 12292,
        }),
        ras_base: 0,
        ras_sp_addr: 0,
        ras_entries: 0,
    };
    let mut insns: Vec<riscv_jit::Src> = (0..16).map(|k| body(k * 4)).collect();
    insns.push((Addi { rd: 5, rs1: 5, imm: -1 }, 4, 64));
    insns.push((Bne { rs1: 5, rs2: 0, imm: -68 }, 4, 68));

    let runs: Vec<&[riscv_jit::Src]> = vec![&insns[..]];
    match riscv_jit::compile_many_into_table(&runs, &[], 16, Some(chain), Some(fp)) {
        Some((w, covered)) => {
            std::fs::write(format!("/tmp/fpcheck_{name}.wasm"), &w).unwrap();
            println!("{name}: covered {:?} of {}, {} bytes", covered, insns.len(), w.len());
        }
        None => println!("{name}: whole batch REJECTED"),
    }
}

fn main() {
    check("fld", |off| (Fld { rd: 10, rs1: 2, imm: 0 }, 4, off));
    check("fsd", |off| (Fsd { rs1: 2, rs2: 10, imm: 0 }, 4, off));
}
