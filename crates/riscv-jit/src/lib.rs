//! Compile straight-line RISC-V blocks to WebAssembly.
//!
//! The interpreter's remaining per-instruction cost is fetch, index, tag-check
//! and a `match` dispatch over ~200 variants. Ceiling probes ruled out the
//! alternatives: hoisting the interrupt sync to block boundaries is worth ~6%,
//! and per-instruction device work about 1.5%. What is left is the dispatch
//! cycle itself, and the only way to delete that is to emit code.
//!
//! Every design decision here came from a measurement, and two were
//! counter-intuitive enough that guessing would have produced the wrong system.
//!
//! ## One module, not one per block
//!
//! With a module per block the host's call site goes megamorphic: rotating
//! through 400 blocks cost **293 ns** per entry against **21 ns** repeatedly
//! entering one. At ~0.7 ns per compiled instruction that penalty needs
//! seven-instruction blocks merely to break even with the interpreter, and real
//! blocks are shorter. `compile_many` puts every block in one module behind a
//! single exported `dispatch`, dropping rotating entry to **38 ns**.
//!
//! ## Memory accesses are host calls, not an inlined TLB
//!
//! Loads and stores are mandatory: without them the JIT covers 47.8% of
//! executed instructions but in runs averaging 2.34, projecting to 1.48x. With
//! them, coverage is 82.5% in runs of 5.72 and the projection is 3.43x.
//!
//! The expensive way to emit one is QEMU's: inline a software TLB probe and
//! call out only on a miss. Measured first with a JS closure as the import, a
//! call looked like 18-32 ns, which says inline the TLB. That is the wrong
//! model — in production the slow path is exported by the emulator's own wasm
//! module, and a wasm-to-wasm imported call measured **6.0 ns** against a 42 ns
//! interpreted instruction. So every access is simply a call, and the entire
//! inlined-TLB effort is avoided.
//!
//! ## Guest registers stay in memory — measured four times
//!
//! Every guest instruction loads its operands from the register file and stores
//! its result back, so a three-operand add is three memory accesses to do one
//! addition. Keeping the hot registers in wasm locals is the obvious fix. It has
//! now been tried four times, in four shapes, and lost every time.
//!
//! The last attempt was a real allocator rather than another fast-path tier:
//! registers chosen per block by use count, filled once at entry, spilled at
//! exactly two exits (the fault return and the shared tail — the host's
//! load/store imports take (addr, val, pc) and never read the register file, so
//! a slow-path access needs no spill). It is correct: 400/400 on the difftest
//! and a byte-identical guest console. An interleaved ABBA A/B put it at a
//! **median 0.81x**, six of eight pairs below parity.
//!
//! The explanation that fits all four results: on V8's baseline tier a wasm
//! local is not a machine register. These blocks are large — ~55 instructions —
//! so the locals spill to the wasm frame and a "local" access is still a frame
//! load and store, now with fill/spill traffic on top and 72% more generated
//! code for the same 400 blocks.
//!
//! So register-file traffic is not the lever it appears to be, and this whole
//! family of ideas is closed. Do not spend a fifth attempt without first showing
//! the generated code reaches TurboFan.
//!
//! ## Faults cost nothing until they happen
//!
//! A guest access can page-fault, and the block must then stop at a precise
//! guest PC with earlier instructions committed and later ones not. Rather than
//! check a flag after every access, the host traps the wasm outright; the caller
//! catches it and resumes interpreting at the PC the host recorded. Each
//! access is passed its guest PC so the host can record it. Faults are rare, so
//! the whole cost sits on the rare path.
//!
//! ## Host contract
//!
//! ```text
//! (import "env" "mem"     (memory 1))
//! (import "env" "load8u"  (func (param i64 i64) (result i64)))  ; addr, pc
//! (import "env" "load16u" (func (param i64 i64) (result i64)))
//! (import "env" "load32u" (func (param i64 i64) (result i64)))
//! (import "env" "load64"  (func (param i64 i64) (result i64)))
//! (import "env" "store8"  (func (param i64 i64 i64)))           ; addr, val, pc
//! (import "env" "store16" (func (param i64 i64 i64)))
//! (import "env" "store32" (func (param i64 i64 i64)))
//! (import "env" "store64" (func (param i64 i64 i64)))
//! (func (export "dispatch") (param i32 i32 i64))                ; block, regs, pc
//! ```
//!
//! `regs` is the byte offset, within the host's own linear memory, of the
//! guest's `[u64; 32]`. `pc` is the guest PC the block starts at. `load`
//! returns raw bytes zero-extended; sign extension is emitted inline, which
//! keeps the host to one function per direction instead of seven.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use riscv_core::types::Instr;

/// Diagnostic counters: how many instructions the emitter processed, and how
/// many of those were folded away by macro-op fusion. Single hart, relaxed is
/// fine -- these are only read for a coarse firing-rate number.
pub static FUSE_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static FUSE_HIT: AtomicU64 = AtomicU64::new(0);

/// (instructions compiled, instructions eliminated by fusion) since start.
pub fn fuse_stats() -> (u64, u64) {
    (FUSE_TOTAL.load(Ordering::Relaxed), FUSE_HIT.load(Ordering::Relaxed))
}

/// Diagnostic counters for direct fall-through tail-call linking: blocks that
/// ended in a clean fall-through (candidates), and those whose physical
/// successor was in the same compile batch and page so a direct `return_call`
/// replaced the chain-table probe.
pub static LINK_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LINK_HIT: AtomicU64 = AtomicU64::new(0);

/// (fall-through candidates, direct-linked) since process start.
pub fn link_stats() -> (u64, u64) {
    (LINK_TOTAL.load(Ordering::Relaxed), LINK_HIT.load(Ordering::Relaxed))
}

/// Diagnostic for TLB-probe hoisting: multi-access groups formed, and those
/// that survived at least one induction-variable stride (nonzero accumulated
/// stride) — i.e. groups hoisting is responsible for.
pub static HOIST_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static HOIST_STRIDED: AtomicU64 = AtomicU64::new(0);

/// (groups formed, strided groups) since process start.
pub fn hoist_stats() -> (u64, u64) {
    (HOIST_TOTAL.load(Ordering::Relaxed), HOIST_STRIDED.load(Ordering::Relaxed))
}

/// SIMD-reach probe (static, at compile). Total plain stores compiled; stores
/// inside a same-value run (memset-vectorizable); loads whose value flows
/// straight into a store (memcpy-vectorizable). Counts instructions, not
/// dynamic executions -- a coarse gauge of whether a v128 fast path has
/// anything to bite on before building the emit.
pub static SIMD_TOTAL_STORES: AtomicU64 = AtomicU64::new(0);
pub static SIMD_MEMSET_STORES: AtomicU64 = AtomicU64::new(0);
pub static SIMD_COPY_PAIRS: AtomicU64 = AtomicU64::new(0);

pub fn simd_stats() -> (u64, u64, u64) {
    (
        SIMD_TOTAL_STORES.load(Ordering::Relaxed),
        SIMD_MEMSET_STORES.load(Ordering::Relaxed),
        SIMD_COPY_PAIRS.load(Ordering::Relaxed),
    )
}

/// Store value register (rs2) of a plain store, else None.
fn store_val(i: &Instr) -> Option<u8> {
    use Instr::*;
    match *i {
        Sb { rs2, .. } | Sh { rs2, .. } | Sw { rs2, .. } | Sd { rs2, .. } => Some(rs2),
        _ => None,
    }
}

/// Count memset-/memcpy-vectorizable patterns in one run, into the SIMD_*
/// counters. A memset run is >=2 consecutive stores sharing the store-value
/// register (interspersed base bumps allowed). A copy pair is a load into rX
/// immediately followed by a store of rX with no intervening use of rX.
fn count_simd_reach(insns: &[Src]) {
    let mut total = 0u64;
    let mut memset = 0u64;
    let mut copy = 0u64;
    // memset: track the current same-value store run.
    let mut run_val: Option<u8> = None;
    let mut run_len = 0u64;
    let mut flush = |run_len: &mut u64, memset: &mut u64| {
        if *run_len >= 2 {
            *memset += *run_len;
        }
        *run_len = 0;
    };
    for w in insns.windows(2) {
        // memcpy: ld rd,..(rA) ; sd rd,..(rB)
        if let (Some(rd), Some(v)) = (dest_reg(&w[0].0), store_val(&w[1].0)) {
            if matches!(w[0].0, Instr::Ld { .. } | Instr::Lw { .. } | Instr::Lwu { .. })
                && rd == v
                && rd != 0
            {
                copy += 1;
            }
        }
    }
    for (i, _, _) in insns {
        if let Some(v) = store_val(i) {
            total += 1;
            match run_val {
                Some(rv) if rv == v => run_len += 1,
                _ => {
                    flush(&mut run_len, &mut memset);
                    run_val = Some(v);
                    run_len = 1;
                }
            }
        } else if let Instr::Addi { rd, rs1, .. } = *i {
            // A base bump between fills does not break a memset run; any other
            // write to the value register does.
            if !(rd == rs1 && Some(rd) != run_val) {
                if run_val == Some(rd) {
                    flush(&mut run_len, &mut memset);
                    run_val = None;
                }
            }
        } else if let Some(rd) = dest_reg(i) {
            if run_val == Some(rd) {
                flush(&mut run_len, &mut memset);
                run_val = None;
            }
        }
    }
    flush(&mut run_len, &mut memset);
    SIMD_TOTAL_STORES.fetch_add(total, Ordering::Relaxed);
    SIMD_MEMSET_STORES.fetch_add(memset, Ordering::Relaxed);
    SIMD_COPY_PAIRS.fetch_add(copy, Ordering::Relaxed);
}

/// Diagnostic: register-file memory traffic emitted. Every guest register read
/// that hits memory is an I64Load the reg file lives in; every write an
/// I64Store. Sizes the register-residency lever -- these are the accesses that
/// promoting guest regs to wasm locals would turn into machine-register moves.
pub static REG_LOADS: AtomicU64 = AtomicU64::new(0);
pub static REG_STORES: AtomicU64 = AtomicU64::new(0);

/// (reg-file loads emitted, reg-file stores emitted) since process start.
pub fn reg_stats() -> (u64, u64) {
    (REG_LOADS.load(Ordering::Relaxed), REG_STORES.load(Ordering::Relaxed))
}
use wasm_encoder::{
    CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, GlobalType, ImportSection, Instruction as W, MemArg, MemoryType,
    Module, RefType, TableSection, TableType, TypeSection, ValType,
};

/// Imported functions occupy the low function index space, so defined
/// functions start after them.
///
/// One import per access width rather than one taking a size argument. The
/// width is known when the code is generated, so a size parameter would only
/// buy the host a `match` to execute on every single guest memory access.
const F_LOAD8U: u32 = 0;
const F_STORE8: u32 = 4;
/// CSR read/modify/write. One import, not one per kind: the kind is a constant
/// argument, unlike the access width which picks a whole different type.
const F_CSR: u32 = 8;
/// One import for the whole F/D extension: `fp(kind|r1<<8|r2<<16, arg, pc)`.
/// The host runs the interpreter's own FP path, so the math is identical by
/// construction; what compiling buys is skipping the ~20x interpreter round
/// trip and, more importantly, not truncating the trace at every FP
/// instruction -- the same lesson fence and CSR already taught.
const F_FP: u32 = 9;
const FIRST_DEFINED: u32 = 10;

/// Import names, in function-index order. The host must supply all of them.
pub const IMPORTS: [&str; 8] = [
    "load8u", "load16u", "load32u", "load64",
    "store8", "store16", "store32", "store64",
];

/// Locals in a block body. Params first, then declared locals.
const L_REGS: u32 = 0;
const L_PC: u32 = 1;
/// Scratch for a loaded value, so the fault check can run before the register
/// write.
const L_TMP: u32 = 2;
/// The guest PC control is going to, kept in a local as well as stored so the
/// chain probe does not have to read it back.
const L_NEXT: u32 = 3;
/// Address of the chain-table entry being probed.
const L_ENT: u32 = 4;
/// Guest address of the memory access being emitted.
const L_ADDR: u32 = 5;
/// Address of the TLB entry being probed.
const L_TENT: u32 = 6;
/// Host page base cached by a load-group leader (i32). See `plan_groups`.
const L_GHOST: u32 = 7;
/// Load-group probe result: nonzero means members of the current load group
/// may access linear memory directly without their own probe.
const L_GFLAG: u32 = 8;
/// Store-group equivalents. Separate locals because the read and write TLBs
/// differ, and a trace can have one group of each open at once.
const L_SHOST: u32 = 9;
const L_SFLAG: u32 = 10;
/// The value most recently written to an integer register by an ALU op, kept
/// for store-to-load forwarding: a `get` of that same register reads this
/// local instead of reloading the register file. The register file itself is
/// still written on every set, so memory stays authoritative at every exit.
const L_VAL: u32 = 11;

/// A v128 scratch local for the memset SIMD fast path, declared only when
/// SIMD_ON. It follows the register-residency locals (which only exist when
/// REG_RESIDENT_ON) so the two features never collide on an index.
const L_V128: u32 = if REG_RESIDENT_ON { REG_LOCAL_BASE + 32 } else { REG_LOCAL_BASE };

/// Return-address stack scratch locals (RAS_ON): an i64 (return PC / popped
/// key) and an i32 (entry address / stack pointer). They follow every other
/// optional local so indices never collide.
const L_RASPC: u32 = REG_LOCAL_BASE
    + if REG_RESIDENT_ON { 32 } else { 0 }
    + if SIMD_ON { 1 } else { 0 };
const L_RASE: u32 = L_RASPC + 1;

/// Return-address stack. A `jal ra` / `jalr ra` (call) pushes its return site's
/// resolved successor block; a `jalr x0, ra` (return) pops and tail-calls it
/// directly. Returns are a large share of chain stops (25-45%) and their target
/// PCs churn the shared hash chain table, so they miss it often and pay a host
/// round-trip (vcache miss -> MMU translate, ~7% of CPython wall time measured).
/// A dedicated stack predicts them perfectly and is immune to that churn.
///
/// Correctness is by fall-back: the pop tail-calls ONLY when the slot's key
/// equals the actual return target AND its generation is current AND there is
/// budget; any mismatch (unbalanced call/return from longjmp, recursion past
/// RAS depth, a stale block) drops through to the normal chain probe. So it can
/// only ever be a fast-path overlay -- it never changes what executes.
///
/// RESULT (2026-08-17): NULL, and the profiling loop is what showed it. Correct
/// (CPython pybench with deep fib recursion: JIT output CHECK byte-identical to
/// the RAS-off build; interp-vs-JIT identical on ls|md5sum). But re-profiling
/// pybench under `--prof` after the build showed the wall-time buckets barely
/// moved (block bodies 23.2->23.6, chain-lookup seam 3.0->2.9, run loop
/// 8.0->7.9, interpreter 27.2->27.1). The ceiling was never more than the
/// chain-lookup seam it targets -- ONLY ~3% of wall time -- because the shared
/// chain table (65536 entries) already tail-calls returns on a hit; returns are
/// a large share of chain STOPS but those stops mostly HIT cheaply, so the
/// host-round-trip fraction the RAS could convert is small and sub-noise. And
/// the push adds a chain-resolve probe at every call, so net is null-to-slightly
/// negative. The real CPython costs the profile names -- cold interpreter (27%,
/// mostly blocks-not-yet-hot), block-entry translation, device/interrupt poll --
/// are not returns and this does not touch them. Shipped OFF; kept flag-gated.
const RAS_ON: bool = false;

/// Byte offset of the fault flag from the register-file base. Immediately after
/// the 32 registers, so it shares their cache line.
pub const FAULT_OFF: u64 = 32 * 8;

/// Byte offset of the next-PC slot, written by a block that ends in a branch.
pub const NEXT_PC_OFF: u64 = FAULT_OFF + 8;

/// Instructions retired by the current chain, accumulated by each block.
pub const INSNS_OFF: u64 = NEXT_PC_OFF + 8;
/// Chain budget; a block stops chaining once the count reaches it.
pub const BUDGET_OFF: u64 = INSNS_OFF + 8;

fn reg_off(r: u8) -> u64 {
    r as u64 * 8
}

/// A decoded instruction, its encoded width, and its byte offset from the
/// block's entry PC.
///
/// The offset is explicit rather than accumulated because a superblock follows
/// jumps: instruction k is not necessarily at the entry PC plus the widths
/// before it. It stays small because a trace never leaves one page.
pub type Src = (Instr, u8, i32);

/// Where the host's chain table lives, so generated code can find its successor
/// without returning to Rust.
///
/// Entries are 16 bytes: `key` (i64 guest PC), `gen` (i32), `idx` (i32 index
/// into the function table). `entries` must be a power of two.
#[derive(Clone, Copy)]
pub struct ChainCfg {
    pub base: u32,
    /// Address of the word holding the current chain generation.
    pub gen_addr: u32,
    pub entries: u32,
    /// Inlined TLB, if the host provides one.
    pub tlb: Option<TlbCfg>,
    /// Return-address stack: base of the entry array, address of the stack
    /// pointer word, and entry count (power of two). Same 16-byte entry layout
    /// as the chain table. Set to (0,0,0) to disable.
    pub ras_base: u32,
    pub ras_sp_addr: u32,
    pub ras_entries: u32,
}

/// Size of one chain-table entry, in bytes.
pub const CHAIN_ENTRY_BYTES: u32 = 16;

/// Size of one TLB entry: virtual page at 0, generation at 8, host page address
/// at 16. 32 bytes so indexing is a shift.
pub const TLB_ENTRY_BYTES: u32 = 32;

#[derive(Clone, Copy)]
pub struct FpCfg {
    /// Absolute address of the guest's f-register file, f0 at +0.
    pub fregs_base: u32,
    /// Address of a u32 the host keeps equal to (mstatus.FS == Dirty). The
    /// inline FP paths are valid only in that state: FS Off must trap, and
    /// Initial/Clean must transition to Dirty, both of which the host call
    /// handles. While already Dirty, an f-register write changes nothing the
    /// host tracks, so the pure-wasm path is safe.
    pub fs_word: u32,
}

/// Where the inlined TLBs live, so compiled accesses can skip the host.
#[derive(Clone, Copy)]
pub struct TlbCfg {
    pub read_base: u32,
    pub write_base: u32,
    pub entries: u32,
    /// The inlined TLB's own generation word. Deliberately NOT the chain
    /// table's: a single-page `sfence.vma` has to void block chaining, whose
    /// entries are keyed by virtual PC, but not cached data translations, which
    /// are per-page and are invalidated individually instead.
    pub gen_addr: u32,
}

/// The FP-arithmetic encodings that inline: sign-injection and raw moves on
/// doubles. Everything here is a pure bit operation that raises no fflags, so
/// the wasm cannot diverge from the FPU on flags -- the property that lets
/// these skip the host call without teaching wasm the flags model. Returns
/// (kind 0..=2 = fsgnj/fsgnjn/fsgnjx.d, 3 = fmv.x.d, 4 = fmv.d.x, rd, rs1, rs2).
fn inline_fp_kind(raw: u32) -> Option<(u32, u8, u8, u8)> {
    if raw & 0x7f != 0x53 {
        return None;
    }
    let rd = ((raw >> 7) & 0x1f) as u8;
    let funct3 = (raw >> 12) & 7;
    let rs1 = ((raw >> 15) & 0x1f) as u8;
    let rs2 = ((raw >> 20) & 0x1f) as u8;
    let funct7 = (raw >> 25) & 0x7f;
    match (funct7, funct3, rs2) {
        (0x11, 0..=2, _) => Some((funct3, rd, rs1, rs2)),
        (0x71, 0, 0) => Some((3, rd, rs1, 0)),
        (0x79, 0, 0) => Some((4, rd, rs1, 0)),
        _ => None,
    }
}

/// Is this instruction in the compilable subset?
pub fn is_compilable(i: &Instr) -> bool {
    use Instr::*;
    matches!(
        i,
        Lui { .. } | Addi { .. } | Slti { .. } | Sltiu { .. } | Xori { .. } | Ori { .. }
            | Andi { .. } | Slli { .. } | Srli { .. } | Srai { .. }
            | Add { .. } | Sub { .. } | Sll { .. } | Slt { .. } | Sltu { .. } | Xor { .. }
            | Srl { .. } | Sra { .. } | Or { .. } | And { .. }
            | Addiw { .. } | Slliw { .. } | Srliw { .. } | Sraiw { .. }
            | Addw { .. } | Subw { .. } | Sllw { .. } | Srlw { .. } | Sraw { .. }
            | Auipc { .. }
            | Lb { .. } | Lh { .. } | Lw { .. } | Ld { .. } | Lbu { .. } | Lhu { .. }
            | Lwu { .. }
            | Sb { .. } | Sh { .. } | Sw { .. } | Sd { .. }
            // A memory fence orders accesses between harts. There is one hart,
            // and compiled code performs its accesses in program order, so
            // there is nothing to order and this emits nothing at all. Worth
            // having anyway: `fence` is in every spinlock release, and while it
            // was uncompilable it truncated the trace of every critical section
            // in the kernel as well as costing an interpreter round trip.
            //
            // `fence.i` is deliberately NOT here. It is the one instruction
            // that invalidates compiled code, so it must reach the host.
            | Fence { .. }
            // Measured: mul is ~5% of interpreted instructions, div/rem is
            // ~0.1%. Only mul is worth compiling, and it is a plain i64.mul --
            // whereas div would need guards for divide-by-zero and the
            // MIN/-1 overflow that traps in wasm, for no measurable gain.
            //
            // The high-multiply family joined later, on Python-workload data:
            // CPython's long-integer arithmetic leans on mulhu, and a real
            // data job interpreted 34.8M of them in one session. wasm has no
            // 128-bit product, so these emit the four-multiply 32-bit split.
            | Mul { .. } | Mulw { .. } | Mulh { .. } | Mulhsu { .. } | Mulhu { .. }
            // One hart: an atomic is a load, an operation and a store, which is
            // exactly how the interpreter treats these. Same reasoning as
            // fence — the trace break they caused cost more than the round trip.
            | Lrw { .. } | Lrd { .. } | Scw { .. } | Scd { .. }
            | Amoswapw { .. } | Amoaddw { .. } | Amoxorw { .. } | Amoandw { .. }
            | Amoorw { .. } | Amominw { .. } | Amomaxw { .. } | Amominuw { .. }
            | Amomaxuw { .. }
            | Amoswapd { .. } | Amoaddd { .. } | Amoxord { .. } | Amoandd { .. }
            | Amoord { .. } | Amomind { .. } | Amomaxd { .. } | Amominud { .. }
            | Amomaxud { .. }
            // The F/D extension, via one host call into the interpreter's own
            // FPU. busybox awk does everything in doubles, and every one of
            // these used to cut the trace.
            | Flw { .. } | Fld { .. } | Fsw { .. } | Fsd { .. } | Fp { .. }
    ) || is_compilable_csr(i)
}

/// CSR ops are compilable via a host call — ~6% of wall time as an interpreter
/// round trip, and every one broke the trace — EXCEPT the read-only counters.
///
/// `time`/`cycle`/`instret` (0xC00..=0xC1F) read a value the run loop advances
/// with the device clock, and that clock only ticks at chain boundaries, not
/// per compiled instruction. A guest delay loop polling `rdtime` would see it
/// frozen inside a chain and spin — which is exactly what hung every cold boot
/// at "Mounting boot media" until this exclusion. Those bail to the
/// interpreter, where the loop ticks each step.
///
/// Writes to the interrupt-control CSRs (SIE bit, `sie`/`mie`, `sip`/`mip`) are
/// NOT excluded here even though they gate interrupt *delivery*: excluding them
/// cost ~40% of JIT MIPS, because `local_irq_save`/`restore` writes `sstatus`
/// constantly. Instead the host `csr` shim breaks the block after such a write
/// only when it actually unmasks an already-pending interrupt — see the `csr`
/// export in `riscv-wasm`. That keeps the common case compiled and reaches the
/// interpreter (which delivers on the next step) only in the rare case that
/// matters.
fn is_compilable_csr(i: &Instr) -> bool {
    use Instr::*;
    let csr = match *i {
        Csrrw { csr, .. } | Csrrs { csr, .. } | Csrrc { csr, .. }
        | Csrrwi { csr, .. } | Csrrsi { csr, .. } | Csrrci { csr, .. } => csr,
        _ => return false,
    };
    !(0xC00..=0xC1F).contains(&csr)
}

/// CSRs whose value gates interrupt delivery: the status registers' SIE bit,
/// the enable masks, and the pending registers. A write to any of these can
/// unmask an already-pending interrupt, which must then reach the interpreter
/// to be delivered. The `csr` host shim consults this after a write.
pub fn is_interrupt_csr(csr: u16) -> bool {
    matches!(
        csr,
        0x100 /* sstatus */ | 0x104 /* sie */ | 0x144 /* sip */
        | 0x300 /* mstatus */ | 0x304 /* mie */ | 0x344 /* mip */
    )
}

/// Can this instruction end a block, as its final compiled instruction?
///
/// Branches and jumps only, and only in last position: a block is straight-line
/// up to its terminator, so emitting a branch mid-block would leave everything
/// after it executing unconditionally. Kept out of `is_compilable` for exactly
/// that reason.
pub fn is_terminator(i: &Instr) -> bool {
    use Instr::*;
    matches!(
        i,
        Jal { .. } | Jalr { .. } | Beq { .. } | Bne { .. } | Blt { .. } | Bge { .. }
            | Bltu { .. } | Bgeu { .. }
    )
}

/// A memory instruction's role in a same-base access group.
///
/// Several accesses through one unchanged base register almost always land in
/// one page — CPython's frame and value-stack traffic, spill/fill sequences
/// against `sp`. One probe can therefore cover the whole group: the leader
/// proves the group's entire span `[base+min_imm, base+min_imm+span)` sits in
/// a single mapped page (the probe's in-page test with `size = span` is
/// exactly that proof) and caches the page's host base; members then access
/// linear memory directly. When the leader's probe misses — page boundary,
/// MMIO, cold TLB — every access falls back to its own full per-access path,
/// so behavior is unchanged, just slower by one flag test.
///
/// Soundness: a cached translation goes stale only when the address space
/// changes, and everything that can change it — a CSR write (`satp` compiles
/// inline via the host shim), `sfence.vma`, a privilege switch — either closes
/// the group in `plan_groups` or ends the trace entirely. Guest stores to page
/// tables do not invalidate in-flight translations until `sfence.vma` by the
/// ISA's own rules, and device DMA changes memory contents, not mappings.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Solo,
    Leader { store: bool, min_imm: i64, span: i32 },
    Member { store: bool },
    /// First `sd` of a strictly-consecutive same-value contiguous store run
    /// (memset). Emits the whole run as v128 stores; the rest are `VfillSkip`.
    Vfill { first_imm: i64, count: u16, fill: u8 },
    /// A member `sd` already emitted by its `Vfill` leader; emits nothing but
    /// still retires (instret counts it).
    VfillSkip,
}

/// Base register, immediate, access bytes, and direction of a plain load or
/// store. AMO/LR/SC and FP accesses are deliberately excluded: they take
/// different emission paths and are rare enough not to matter.
fn mem_access(i: &Instr) -> Option<(u8, i64, i64, bool)> {
    use Instr::*;
    Some(match *i {
        Lb { rs1, imm, .. } | Lbu { rs1, imm, .. } => (rs1, imm, 1, false),
        Lh { rs1, imm, .. } | Lhu { rs1, imm, .. } => (rs1, imm, 2, false),
        Lw { rs1, imm, .. } | Lwu { rs1, imm, .. } => (rs1, imm, 4, false),
        Ld { rs1, imm, .. } => (rs1, imm, 8, false),
        Sb { rs1, imm, .. } => (rs1, imm, 1, true),
        Sh { rs1, imm, .. } => (rs1, imm, 2, true),
        Sw { rs1, imm, .. } => (rs1, imm, 4, true),
        Sd { rs1, imm, .. } => (rs1, imm, 8, true),
        _ => return None,
    })
}

/// The integer register this instruction writes, for group invalidation.
/// Only the compilable, translation-neutral subset appears here; anything
/// else closes every open group in `plan_groups` instead.
fn dest_reg(i: &Instr) -> Option<u8> {
    use Instr::*;
    match *i {
        Lui { rd, .. } | Auipc { rd, .. } | Addi { rd, .. } | Slti { rd, .. }
        | Sltiu { rd, .. } | Xori { rd, .. } | Ori { rd, .. } | Andi { rd, .. }
        | Slli { rd, .. } | Srli { rd, .. } | Srai { rd, .. } | Add { rd, .. }
        | Sub { rd, .. } | Sll { rd, .. } | Slt { rd, .. } | Sltu { rd, .. }
        | Xor { rd, .. } | Srl { rd, .. } | Sra { rd, .. } | Or { rd, .. }
        | And { rd, .. } | Addiw { rd, .. } | Slliw { rd, .. } | Srliw { rd, .. }
        | Sraiw { rd, .. } | Addw { rd, .. } | Subw { rd, .. } | Sllw { rd, .. }
        | Srlw { rd, .. } | Sraw { rd, .. } | Mul { rd, .. } | Mulw { rd, .. }
        | Mulh { rd, .. } | Mulhsu { rd, .. } | Mulhu { rd, .. }
        | Lb { rd, .. } | Lh { rd, .. } | Lw { rd, .. } | Ld { rd, .. }
        | Lbu { rd, .. } | Lhu { rd, .. } | Lwu { rd, .. } | Jal { rd, .. } => Some(rd),
        _ => None,
    }
}

/// Group-probe CSE switch, for A/B builds. First measurement (member fallback
/// = full per-access path) was a consistent −12% on the Python workload —
/// suspect code bloat; the slim-fallback variant is what this now gates.
const GROUP_CSE_ON: bool = true;

/// Register store-to-load forwarding switch. Measured −15% median on the
/// Python workload (2/8 pairs favored it): on the baseline wasm tier a local
/// is not a machine register, and the extra tee/branch bytes cost more than
/// the register-file reloads they save. Kept for a future TurboFan-tier
/// experiment; off in production builds.
const REG_FWD_ON: bool = false;

/// Macro-op fusion switch, for A/B builds. Fuses the two RISC-V idioms that a
/// compiler emits for every 32-bit constant and every PC-relative address:
///
///   lui   rd, hi ; addi rd, rd, lo   ->  rd = (hi + lo)            [const]
///   auipc rd, hi ; addi rd, rd, lo   ->  rd = pc + off + hi + lo   [addr]
///
/// Baseline emits both: the `lui`/`auipc` writes rd to the register file, then
/// `addi` reloads it (REG_FWD_ON is off), adds, and writes again. Both operands
/// are compile-time constants, so the fused form folds them into a single
/// I64Const and stores rd exactly once -- the intermediate store is dead
/// because nothing reads rd between two adjacent instructions. instret still
/// advances by two; only the emitted wasm shrinks.
///
/// Measured (shell `ls -la /bin | md5sum` loop, this box): correct (difftest
/// 412/412 through both import impls), but it fires on only 4.16% of compiled
/// instructions and each firing removes a handful of wasm ops, so the A/B came
/// back at -2.6% +/-3.2% against a null resolution of 4.9% -- i.e. no
/// resolvable difference. Same tier ceiling as REG_FWD_ON: shaving linear-
/// memory ops per instruction does not move Liftoff-tier wall clock. Off in
/// production; kept flag-gated for the TurboFan-tier experiment, where a folded
/// constant becomes a real immediate and the dead store a real removed store.
const FUSE_ON: bool = false;

/// Direct fall-through tail-call linking. The chain already tail-calls its
/// successor, but through a hash probe of the chain table on every block. When
/// a block ends in a clean fall-through AND its physical successor is another
/// block in the SAME compile batch and the SAME page, the successor's guest PC
/// is known at translation and its wasm function index is fixed, so the block
/// can `return_call` it directly and skip the probe entirely.
///
/// Correctness rests on two invariants of this JIT: table indices are never
/// reused (they only ever append), so a baked index cannot come to mean a
/// different block; and a trace never crosses a page, so a same-page successor
/// shares its predecessor's invalidation fate -- when the page's translation
/// generation bumps, both are orphaned together and neither is re-entered
/// (the host re-decodes at a fresh generation and compiles new blocks at new
/// indices). Fall-through also never changes privilege. Cross-page fall-through
/// is therefore excluded: the two halves could be invalidated independently.
/// The budget check is preserved, so interrupt latency is unchanged.
///
/// Measured (shell `ls -la /bin | md5sum` loop, this box): correct (full
/// interp-vs-JIT output identical over 400M instructions) and links 21.5% of
/// clean fall-through blocks -- but the A/B came back +0.3% +/-4.2% against a
/// 2.8% null, i.e. no resolvable difference. The linked edges are a minority of
/// all chain transitions (fall-through blocks are themselves a subset; most
/// blocks end at a jalr or branch this doesn't cover), and each saved probe is
/// only ~15 wasm ops. Same Liftoff-tier ceiling as FUSE_ON / REG_FWD_ON. Off in
/// production; flag-gated for the TurboFan-tier experiment and for a later
/// extension to direct branch/jal targets, where dispatch cost is larger.
///
/// NOTE: the fall-through address is `last.off + last.width`, NOT the sum of
/// widths -- a trace follows `jal` within a page so its bytes are not
/// contiguous. Getting that wrong linked to the wrong successor and the
/// interp-vs-JIT harness caught it as an output mismatch; see fallthrough_links.
const TAILLINK_ON: bool = false;

/// Register residency: cache guest-register reads in wasm locals instead of
/// reloading them from the linear-memory register file every time.
///
/// This is a WRITE-THROUGH read cache, which is what makes it safe: every
/// register write still stores to the memory reg file (so memory is always the
/// source of truth and no block exit, side exit, or fault bail needs to spill),
/// and additionally keeps the value in a per-register wasm local. Subsequent
/// reads take the local. The cache is invalidated only when a register is
/// written by a path other than `set_with` -- a load's destination, or a host
/// shim (csr/fp) -- so a stale local can never be served.
///
/// Why it can win where fusion could not (see OPTIMIZATION_LEDGER.md): the reg
/// file shares linear memory with guest data, so from TurboFan's view a guest
/// store may alias it, forcing a reload of every guest register after each
/// guest memory op. A wasm local cannot be aliased by a memory store, so
/// TurboFan keeps it in a machine register across the guest access. Measured
/// reg-file traffic is ~2.2 memory ops per compiled instruction, most of it
/// redundant reloads this removes.
///
/// RESULT (2026-08-17, shell md5sum, bench/mips.js/jit-fast-ab): NULL. Correct
/// (difftest 412/412 both import impls; full interp-vs-JIT output identical over
/// 400M insns) and it cuts EMITTED reg loads from 1.60 to 0.70 per compiled
/// instruction (−56%), yet the A/B is +0.5% ±3.6% vs a 3.8% null -- no
/// resolvable difference. Same lesson as [[FUSE_ON]]: the hot blocks are in
/// TurboFan (confirmed, worth +18%), and TurboFan already eliminates redundant
/// constant-offset reg-file loads and allocates the values to machine
/// registers, so removing them from the wasm bytecode changes the input to TF
/// but not its output. Bytecode load count is decoupled from wall clock at the
/// TF tier. Caveat: this caches only WRITTEN registers (a read miss does not
/// populate, to stay safe across runtime-conditional arms); the read-only-reg-
/// across-a-guest-store sub-case is untested and would need an entry-preload of
/// the block's read-set -- but given a 56% load cut moved nothing, that is very
/// unlikely to pay. Shipped OFF. The remaining headroom is NOT in per-block
/// codegen; see OPTIMIZATION_LEDGER.md (ASID keying, region formation).
const REG_RESIDENT_ON: bool = false;

/// First wasm local used to hold a resident guest register. Locals 0..=11 are
/// the two params plus the scratch set; the 32 register locals follow. The
/// local for guest register `r` is `REG_LOCAL_BASE + r` (slot 0 is unused).
const REG_LOCAL_BASE: u32 = 12;

fn reg_local(r: u8) -> u32 {
    REG_LOCAL_BASE + r as u32
}

/// Widest span a group may prove in-page at once. Must stay well under 4096 or
/// the probe's `(addr & 0xFFF) <= 4096 - span` test would never pass; past a
/// couple hundred bytes the probe also starts rejecting real same-page runs.
const GROUP_SPAN_MAX: i64 = 256;

/// TLB-probe hoisting across an induction variable. Without it a group closes
/// the moment its base register is written, so a striding loop the tracer
/// unrolled -- `ld 0(a0); addi a0,a0,8; ld 0(a0); ...` (memcpy, sha256, string
/// scans) -- re-probes the TLB every iteration even though every access stays
/// on the same page until a page crossing. With it, a *constant* `addi
/// base,base,K` no longer closes the group; the pass accumulates the stride and
/// treats each later access's offset as `acc_stride + imm`, so the leader's one
/// probe (with a span covering the whole strided range) serves them all. This
/// is sound because `group_host_addr` reconstructs each member's host address
/// from the cached page base and the member's OWN low-12-bit offset, and the
/// span check proves the whole range lies in one page -- exactly the same
/// guarantees the base-invariant groups already rely on. The span cap
/// libc-idiom SIMD: a run of consecutive `sd` instructions storing the SAME
/// value register to contiguous 8-byte slots through one base (the shape musl
/// and the kernel emit for memset / page-zeroing / struct fills) is emitted as
/// `v128.store`s of the splatted value -- one 16-byte machine store per two
/// guest `sd`s. Unlike the scalar bytecode levers, this is genuinely FEWER
/// operations (TurboFan lowers v128.store to one wide store, not two), so it is
/// not work TurboFan can redo from the scalar form. One TLB probe covers the
/// run; the v128 path runs only on a write-hit (page writable + whole run
/// in-page), so every covered byte is safe; a miss falls to per-store host
/// calls. instret still counts every guest `sd`.
///
/// RESULT (2026-08-17): NULL, for a DIFFERENT reason than the scalar bytecode
/// levers. Correct (interp-vs-JIT identical on ls|md5sum and on a memset-heavy
/// page-crossing dd+md5sum+tr stress) and it FIRES on the hot memset/clear_user
/// loops (md5sum vectorizes ~2.2k static sd, ~7% of stores; dd/page-zeroing
/// hit it dynamically). But the A/B is -1.5% ±3.8% on a memset-bound `dd
/// if=/dev/zero of=/dev/null bs=1M` (clean 2.7% null). Data movement is memory-
/// bandwidth / per-iteration-overhead bound, not instruction-ISSUE bound: a
/// v128.store moves the same 16 bytes as two i64.stores at the same bandwidth,
/// and the surrounding loop work (base bump, branch, instret) is untouched, so
/// halving the store *count* buys nothing. SIMD pays only when issue width is
/// the bottleneck (vectorizable COMPUTE), which the guest's scalar stream does
/// not hand us. Shipped OFF; the memset->v128 machinery is kept flag-gated.
const SIMD_ON: bool = false;

/// Diagnostic: guest `sd` instructions folded into a v128.store fast path.
pub static SIMD_VEC_STORES: AtomicU64 = AtomicU64::new(0);

/// Guest stores vectorized since process start.
pub fn simd_vec_stores() -> u64 {
    SIMD_VEC_STORES.load(Ordering::Relaxed)
}

/// (`GROUP_SPAN_MAX`) still bounds how many iterations one probe covers; the
/// group re-forms past that or at a page crossing (probe miss -> slow path).
///
/// RESULT (2026-08-17): NULL. Correct (interp-vs-JIT identical on ls|md5sum, on
/// a memcpy+sha256 page-crossing stress, over full runs) and it FIRES well --
/// ~30% of all multi-access groups become strided across every workload tried
/// (sha256 31%, md5sum 29%, memcpy 32%, gzip 29%). But the A/B is 0.0% ±1.1% on
/// memcpy (the most probe-dense load, tight CI) and -0.5% ±5.1% on sha256 --
/// no resolvable win. Same lesson as FUSE_ON / REG_RESIDENT_ON: a strided run
/// hits ONE guest page, so every member's probe reads the SAME TLB entry with
/// the SAME vpn, and TurboFan already CSEs those repeated entry loads/compares
/// on the hot (TurboFan-tier) path -- eliminating them in the wasm bytecode
/// changes TF's input, not its output. The four bytecode levers are unanimous:
/// the win is not in per-block codegen (see OPTIMIZATION_LEDGER.md). Shipped
/// OFF; the induction-stride analysis is kept flag-gated behind this const.
const TLB_HOIST_ON: bool = false;

/// Assign a `Role` to every instruction of a trace.
///
/// Walks the trace exactly as `emit_body` does. A group is consecutive plain
/// loads (or stores — tracked separately) through one base register, closed by
/// a write to that base, by any instruction that could change address
/// translation or has effects this pass does not model (CSR, FP, AMO, LR/SC),
/// or by the span cap. Conditional-branch guards and `jal` do not close
/// groups: a side exit simply never executes the members after it, and the
/// leader's cached translation cannot go stale across them.
fn plan_groups(insns: &[Src]) -> Vec<Role> {
    let mut roles = alloc::vec![Role::Solo; insns.len()];

    // Open group per direction: (base, min_eff, max_end_eff, acc_stride, idxs).
    // Offsets are EFFECTIVE: relative to the base's value at the leader, so an
    // induction-variable `addi base,base,K` between accesses shifts subsequent
    // offsets by the accumulated stride rather than closing the group. Without
    // TLB hoisting the stride stays 0 and this is exactly the old base-invariant
    // grouping.
    let mut open: [Option<(u8, i64, i64, i64, Vec<usize>)>; 2] = [None, None];

    fn close(
        slot: &mut Option<(u8, i64, i64, i64, Vec<usize>)>,
        store: bool,
        roles: &mut [Role],
        insns: &[Src],
    ) {
        if let Some((_, min, max_end, acc, idxs)) = slot.take() {
            if idxs.len() < 2 {
                return;
            }
            HOIST_TOTAL.fetch_add(1, Ordering::Relaxed);
            if acc != 0 {
                HOIST_STRIDED.fetch_add(1, Ordering::Relaxed);
            }

            // memset SIMD: a store group that is strictly consecutive `sd`, no
            // stride, ascending contiguous 8-byte offsets, all storing the SAME
            // register. Strict consecutiveness is what makes it safe -- nothing
            // between the stores can rewrite the fill register or read a
            // location a reordered store would change -- and it is exactly the
            // shape musl/kernel memset unrolls to. Emitted as v128 stores.
            let n = idxs.len();
            let contiguous = (max_end - min) == (n as i64) * 8;
            let consecutive = idxs.iter().enumerate().all(|(j, &ix)| ix == idxs[0] + j);
            let fill = store_val(&insns[idxs[0]].0);
            if SIMD_ON
                && store
                && acc == 0
                && n <= u16::MAX as usize
                && contiguous
                && consecutive
                && fill.is_some()
                && idxs.iter().enumerate().all(|(j, &ix)| {
                    matches!(insns[ix].0, Instr::Sd { rs2, imm, .. }
                        if imm == min + (j as i64) * 8 && Some(rs2) == fill)
                })
            {
                roles[idxs[0]] = Role::Vfill { first_imm: min, count: n as u16, fill: fill.unwrap() };
                for &k in &idxs[1..] {
                    roles[k] = Role::VfillSkip;
                }
                return;
            }

            roles[idxs[0]] = Role::Leader { store, min_imm: min, span: (max_end - min) as i32 };
            for &k in &idxs[1..] {
                roles[k] = Role::Member { store };
            }
        }
    }

    for (k, (i, _, _)) in insns.iter().enumerate() {
        if !is_compilable(i) && !is_terminator(i) {
            break; // emit_body stops here too
        }

        if let Some((rs1, imm, size, st)) = mem_access(i) {
            let d = st as usize;
            let joined = match &mut open[d] {
                // Effective offset includes the base's accumulated stride.
                Some((base, min, max_end, acc, idxs)) if *base == rs1 => {
                    let eff = *acc + imm;
                    let nmin = eff.min(*min);
                    let nmax = (eff + size).max(*max_end);
                    if nmax - nmin <= GROUP_SPAN_MAX {
                        *min = nmin;
                        *max_end = nmax;
                        idxs.push(k);
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !joined {
                close(&mut open[d], st, &mut roles, insns);
                open[d] = Some((rs1, imm, imm + size, 0, alloc::vec![k]));
            }
        } else if !matches!(
            i,
            Instr::Beq { .. } | Instr::Bne { .. } | Instr::Blt { .. } | Instr::Bge { .. }
                | Instr::Bltu { .. } | Instr::Bgeu { .. } | Instr::Jal { .. }
                | Instr::Jalr { .. } | Instr::Fence { .. }
        ) && dest_reg(i).is_none()
        {
            // CSR (can rewrite satp), FP, AMO, LR/SC, anything unmodeled:
            // cached translations may no longer be trusted past this point.
            close(&mut open[0], false, &mut roles, insns);
            close(&mut open[1], true, &mut roles, insns);
        }

        // Induction variable: a small constant `addi base,base,K` extends the
        // group's stride instead of closing it, so the leader's probe keeps
        // covering later strided accesses. The addi still executes normally;
        // members recompute their own address, so only the accumulated offset
        // bookkeeping changes here.
        let mut strided = [false, false];
        if TLB_HOIST_ON {
            if let Instr::Addi { rd, rs1, imm } = *i {
                if rd == rs1 && rd != 0 && imm.abs() <= GROUP_SPAN_MAX {
                    for d in 0..2 {
                        if let Some(g) = open[d].as_mut() {
                            if g.0 == rd {
                                g.3 += imm;
                                strided[d] = true;
                            }
                        }
                    }
                }
            }
        }

        // A write to a group's base register ends that group — after the
        // access above joined, so `ld a0, 0(a0)` is still a member — UNLESS the
        // write was the induction-variable stride we just folded in.
        if let Some(rd) = dest_reg(i) {
            if rd != 0 {
                for d in 0..2 {
                    if !strided[d] && open[d].as_ref().is_some_and(|g| g.0 == rd) {
                        close(&mut open[d], d == 1, &mut roles, insns);
                    }
                }
            }
        }
    }
    close(&mut open[0], false, &mut roles, insns);
    close(&mut open[1], true, &mut roles, insns);
    roles
}

/// The operation an atomic read-modify-write applies to the loaded word.
#[derive(Clone, Copy)]
enum AmoOp {
    Swap,
    Add,
    Xor,
    And,
    Or,
    Min,
    Max,
    MinU,
    MaxU,
}

struct Emit {
    f: Function,
    /// Byte offset of the current instruction from the block's start PC.
    off: i64,
    chain: Option<ChainCfg>,
    tlb: Option<TlbCfg>,
    /// Inline the flag-free FP subset when the host provides the addresses.
    fp: Option<FpCfg>,
    /// Address of the shared generation word, used by both the chain probe and
    /// the TLB probe.
    gen_addr: u32,
    /// Role of the instruction currently being emitted, from `plan_groups`.
    /// Set by `emit_body` before each instruction; only load/store read it.
    role: Role,
    /// Register whose current value `L_VAL` holds, for forwarding. `None`
    /// whenever anything other than `set_with` may have written a register —
    /// loads, CSR and FP host shims, atomics.
    fwd: Option<u8>,
    /// Register-residency read cache: `cached[r]` is true when local
    /// `reg_local(r)` holds the current value of guest register `r`. Reset per
    /// block (each block is a fresh function whose locals start zeroed and hold
    /// nothing until loaded). Only consulted when `REG_RESIDENT_ON`.
    cached: [bool; 32],
}

impl Emit {
    /// Bail out of the block if the host flagged a fault on the last access.
    ///
    /// The host has already recorded the faulting guest PC; the interpreter
    /// resumes there and re-executes the instruction, which faults again and
    /// takes the trap through the normal path. Everything before it in the
    /// block has committed, which is exactly right -- those instructions did
    /// execute.
    fn bail_if_faulted(&mut self) {
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I32Load(MemArg {
            offset: FAULT_OFF,
            align: 2,
            memory_index: 0,
        }));
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        self.f.instruction(&W::Return);
        self.f.instruction(&W::End);
    }
}

impl Emit {
    /// Drop a register from the residency cache: its local no longer mirrors
    /// memory, so the next read must reload. Used after any write to `r` that
    /// does not go through `set_with` (a load destination, a host shim).
    fn invalidate_reg(&mut self, r: u8) {
        if REG_RESIDENT_ON && r < 32 {
            self.cached[r as usize] = false;
        }
    }

    /// Drop every register from the residency cache. Used after a host shim
    /// (csr/fp) that may write registers this side does not track individually.
    fn invalidate_all_regs(&mut self) {
        if REG_RESIDENT_ON {
            self.cached = [false; 32];
        }
    }

    fn get(&mut self, r: u8) {
        if r == 0 {
            self.f.instruction(&W::I64Const(0));
        } else if REG_RESIDENT_ON && self.cached[r as usize] {
            // Held in a local -- a machine register after TurboFan, and
            // unaliasable by the guest memory ops since its last write, which is
            // the whole point. `cached` is set ONLY by a main-path write
            // (`set_with` or a load destination), never lazily by a read: a read
            // can be emitted inside a runtime-conditional arm (e.g. a store's
            // TLB hit/miss arms both read rs2), and a tee there would populate
            // the local on only one path, leaving a sibling arm reading garbage.
            self.f.instruction(&W::LocalGet(reg_local(r)));
        } else if REG_FWD_ON && self.fwd == Some(r) {
            // The value was just computed by `set_with` and is still in L_VAL;
            // skip the register-file reload.
            self.f.instruction(&W::LocalGet(L_VAL));
        } else {
            REG_LOADS.fetch_add(1, Ordering::Relaxed);
            self.f.instruction(&W::LocalGet(L_REGS));
            self.f.instruction(&W::I64Load(MemArg {
                offset: reg_off(r),
                align: 3,
                memory_index: 0,
            }));
        }
    }

    fn set_with(&mut self, r: u8, body: impl FnOnce(&mut Self)) {
        if r == 0 {
            // Nothing in the arithmetic subset has a side effect beyond its
            // register write, so a write to x0 can be skipped entirely. Loads
            // are the exception and go through `load`, which still performs the
            // access because it can fault.
            return;
        }
        self.f.instruction(&W::LocalGet(L_REGS));
        // `body` runs before `fwd`/the cache moves to `r`: a `get(r)` inside it
        // must see the PREVIOUS value of `r`, wherever that lives.
        body(self);
        if REG_RESIDENT_ON {
            // Write through: the value goes to both the local (so later reads
            // take it) and the memory reg file (so memory stays coherent for
            // every exit and bail, with no spill needed).
            self.f.instruction(&W::LocalTee(reg_local(r)));
            self.cached[r as usize] = true;
        } else if REG_FWD_ON {
            self.f.instruction(&W::LocalTee(L_VAL));
        }
        REG_STORES.fetch_add(1, Ordering::Relaxed);
        self.f.instruction(&W::I64Store(MemArg {
            offset: reg_off(r),
            align: 3,
            memory_index: 0,
        }));
        self.fwd = Some(r);
    }

    /// Emit a CSR op as a host call: `csr(csr, rd, src, val, kind, pc)`.
    ///
    /// `val` pushes the operand — reg[rs1] for the register forms, the zero-
    /// extended immediate for the *i forms. The host writes rd and the CSR and
    /// refreshes the generation word; all this side does is push the arguments,
    /// call, and check the fault flag, since a bad privilege or read-only write
    /// must bail the block exactly as a faulting load does.
    fn csr_op(&mut self, csr: u16, rd: u8, src: u8, kind: u32, val: impl FnOnce(&mut Self)) {
        self.f.instruction(&W::I32Const(csr as i32));
        self.f.instruction(&W::I32Const(rd as i32));
        self.f.instruction(&W::I32Const(src as i32));
        val(self);
        self.f.instruction(&W::I32Const(kind as i32));
        self.pc();
        self.f.instruction(&W::Call(F_CSR));
        // The host shim wrote `rd` behind the wasm's back.
        if self.fwd == Some(rd) {
            self.fwd = None;
        }
        self.invalidate_all_regs();
        self.bail_if_faulted();
    }

    /// Emit an FP instruction as the host call `fp(kind|r1<<8|r2<<16, arg, pc)`.
    ///
    /// kind 0..=3 are fld/flw/fsd/fsw (r1 = rd or rs2, r2 = rs1, arg = imm);
    /// kind 4 is everything else in the extension, with arg carrying the raw
    /// encoding for the host FPU. The host bails on FS-off, illegal encodings
    /// and page faults, with nothing committed, so the fault check resumes the
    /// interpreter AT this instruction and the trap is taken there.
    fn fp_call(&mut self, kind: u32, r1: u8, r2: u8, arg: i64) {
        let packed = kind as i64 | ((r1 as i64) << 8) | ((r2 as i64) << 16);
        self.f.instruction(&W::I64Const(packed));
        self.f.instruction(&W::I64Const(arg));
        self.pc();
        self.f.instruction(&W::Call(F_FP));
        // fmv.x.d / fcvt / fclass kinds write an integer register host-side;
        // cheaper to always drop the forward than to decode which.
        self.fwd = None;
        self.invalidate_all_regs();
        self.bail_if_faulted();
    }

    /// Gate an inline FP path on mstatus.FS == Dirty, falling back to the
    /// host call otherwise. One absolute load and a branch per FP
    /// instruction; the fallback handles the FS-off trap and the
    /// Initial/Clean -> Dirty transition, neither of which the inline path
    /// may do.
    fn fp_gated(
        &mut self,
        c: FpCfg,
        host: (u32, u8, u8, i64),
        inline: impl FnOnce(&mut Self),
    ) {
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg {
            offset: c.fs_word as u64,
            align: 2,
            memory_index: 0,
        }));
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        inline(self);
        self.f.instruction(&W::Else);
        let (k, r1, r2, arg) = host;
        self.fp_call(k, r1, r2, arg);
        self.f.instruction(&W::End);
    }

    /// Push f[r]. The f-register file sits at a baked absolute address, so
    /// this is one load with a constant offset -- same cost as an x-register.
    fn fget(&mut self, c: FpCfg, r: u8) {
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I64Load(MemArg {
            offset: c.fregs_base as u64 + r as u64 * 8,
            align: 3,
            memory_index: 0,
        }));
    }

    /// fld/flw: the integer load path, destination redirected to f[rd], with
    /// NaN-boxing for the 32-bit form. Fault semantics identical to `load`:
    /// the value goes through the scratch local and the bail check runs
    /// before the register write.
    fn fp_load(&mut self, c: FpCfg, rd: u8, rs1: u8, imm: i64, size_log2: i32, boxed: bool) {
        self.addr(rs1, imm);
        self.f.instruction(&W::LocalSet(L_ADDR));
        if let Some(t) = self.tlb {
            self.tlb_probe(&t, 1 << size_log2, false);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I64)));
            self.tlb_host_addr();
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&if size_log2 == 2 {
                W::I64Load32U(m)
            } else {
                W::I64Load(m)
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
        }
        self.f.instruction(&W::LocalSet(L_TMP));
        self.bail_if_faulted();
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::LocalGet(L_TMP));
        if boxed {
            self.f.instruction(&W::I64Const(0xFFFF_FFFF_0000_0000u64 as i64));
            self.f.instruction(&W::I64Or);
        }
        self.f.instruction(&W::I64Store(MemArg {
            offset: c.fregs_base as u64 + rd as u64 * 8,
            align: 3,
            memory_index: 0,
        }));
    }

    /// fsd/fsw: the integer store path with the value read from f[rs2].
    fn fp_store(&mut self, c: FpCfg, rs1: u8, rs2: u8, imm: i64, size_log2: i32) {
        self.addr(rs1, imm);
        self.f.instruction(&W::LocalSet(L_ADDR));
        if let Some(t) = self.tlb {
            self.tlb_probe(&t, 1 << size_log2, true);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
            self.tlb_host_addr();
            self.fget(c, rs2);
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&if size_log2 == 2 {
                W::I64Store32(m)
            } else {
                W::I64Store(m)
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.fget(c, rs2);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.fget(c, rs2);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
        }
        self.bail_if_faulted();
    }

    /// fsgnj/fsgnjn/fsgnjx.d: (a & !SIGN) | (f(b) & SIGN). Pure bit ops, and
    /// the FPU's own arm for these raises no flags, so this is exact.
    fn fp_sgnj(&mut self, c: FpCfg, kind: u32, rd: u8, rs1: u8, rs2: u8) {
        const SIGN: i64 = i64::MIN;
        self.f.instruction(&W::I32Const(0));
        self.fget(c, rs1);
        self.f.instruction(&W::I64Const(!SIGN));
        self.f.instruction(&W::I64And);
        self.fget(c, rs2);
        if kind == 2 {
            self.fget(c, rs1);
            self.f.instruction(&W::I64Xor);
        }
        self.f.instruction(&W::I64Const(SIGN));
        self.f.instruction(&W::I64And);
        if kind == 1 {
            // !b & SIGN == (b & SIGN) ^ SIGN
            self.f.instruction(&W::I64Const(SIGN));
            self.f.instruction(&W::I64Xor);
        }
        self.f.instruction(&W::I64Or);
        self.f.instruction(&W::I64Store(MemArg {
            offset: c.fregs_base as u64 + rd as u64 * 8,
            align: 3,
            memory_index: 0,
        }));
    }

    /// fmv.x.d: f[rs1] -> x[rd], raw bits. Skipped entirely for x0, which has
    /// no other effect -- the FPU arm raises nothing.
    fn fp_mv_x_d(&mut self, c: FpCfg, rd: u8, rs1: u8) {
        if rd == 0 {
            return;
        }
        if self.fwd == Some(rd) {
            self.fwd = None;
        }
        // Written to the reg file directly below, not through set_with.
        self.invalidate_reg(rd);
        self.f.instruction(&W::LocalGet(L_REGS));
        self.fget(c, rs1);
        self.f.instruction(&W::I64Store(MemArg {
            offset: reg_off(rd),
            align: 3,
            memory_index: 0,
        }));
    }

    /// fmv.d.x: x[rs1] -> f[rd], raw bits.
    fn fp_mv_d_x(&mut self, c: FpCfg, rd: u8, rs1: u8) {
        self.f.instruction(&W::I32Const(0));
        self.get(rs1);
        self.f.instruction(&W::I64Store(MemArg {
            offset: c.fregs_base as u64 + rd as u64 * 8,
            align: 3,
            memory_index: 0,
        }));
    }

    /// Guest PC of the instruction being emitted.
    fn pc(&mut self) {
        self.f.instruction(&W::LocalGet(L_PC));
        self.f.instruction(&W::I64Const(self.off));
        self.f.instruction(&W::I64Add);
    }

    /// Address operand: `reg[rs1] + imm`.
    fn addr(&mut self, rs1: u8, imm: i64) {
        self.get(rs1);
        self.f.instruction(&W::I64Const(imm));
        self.f.instruction(&W::I64Add);
    }

    /// Push `mulhu(L_TMP, L_ADDR)` — the high 64 bits of the unsigned 128-bit
    /// product — via the four-multiply 32-bit split. Clobbers L_NEXT. No
    /// partial sum can overflow: ah·bl ≤ (2³²−1)² and the added low words are
    /// each < 2³², which still fits u64.
    fn mulhu_of_scratch(&mut self) {
        const LO: i64 = 0xFFFF_FFFF;
        let half = |e: &mut Self, local: u32, high: bool| {
            e.f.instruction(&W::LocalGet(local));
            if high {
                e.f.instruction(&W::I64Const(32));
                e.f.instruction(&W::I64ShrU);
            } else {
                e.f.instruction(&W::I64Const(LO));
                e.f.instruction(&W::I64And);
            }
        };
        // u = ah*bl + ((al*bl) >> 32)
        half(self, L_TMP, true);
        half(self, L_ADDR, false);
        self.f.instruction(&W::I64Mul);
        half(self, L_TMP, false);
        half(self, L_ADDR, false);
        self.f.instruction(&W::I64Mul);
        self.f.instruction(&W::I64Const(32));
        self.f.instruction(&W::I64ShrU);
        self.f.instruction(&W::I64Add);
        self.f.instruction(&W::LocalSet(L_NEXT));
        // v = al*bh + (u & LO)
        half(self, L_TMP, false);
        half(self, L_ADDR, true);
        self.f.instruction(&W::I64Mul);
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I64Const(LO));
        self.f.instruction(&W::I64And);
        self.f.instruction(&W::I64Add);
        // result = ah*bh + (u >> 32) + (v >> 32)
        self.f.instruction(&W::I64Const(32));
        self.f.instruction(&W::I64ShrU);
        half(self, L_TMP, true);
        half(self, L_ADDR, true);
        self.f.instruction(&W::I64Mul);
        self.f.instruction(&W::I64Add);
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I64Const(32));
        self.f.instruction(&W::I64ShrU);
        self.f.instruction(&W::I64Add);
    }

    /// Subtract `(signed(local_s) < 0 ? local_v : 0)` from the value on the
    /// stack, branchlessly: `(s >> 63)` is all-ones exactly when negative.
    fn mulh_sign_term(&mut self, local_s: u32, local_v: u32) {
        self.f.instruction(&W::LocalGet(local_s));
        self.f.instruction(&W::I64Const(63));
        self.f.instruction(&W::I64ShrS);
        self.f.instruction(&W::LocalGet(local_v));
        self.f.instruction(&W::I64And);
        self.f.instruction(&W::I64Sub);
    }

    /// One TLB probe covering a whole access group. On a hit, caches the host
    /// page base and raises the group flag; members then skip their own probes.
    /// On a miss, lowers the flag — every member takes its full per-access
    /// fallback and behavior is identical to ungrouped code.
    fn group_probe(&mut self, store: bool, rs1: u8, min_imm: i64, span: i32) {
        let Some(c) = self.tlb else { return };
        let (host, flag) = if store { (L_SHOST, L_SFLAG) } else { (L_GHOST, L_GFLAG) };
        self.addr(rs1, min_imm);
        self.f.instruction(&W::LocalSet(L_ADDR));
        self.tlb_probe(&c, span, store);
        self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I32)));
        self.f.instruction(&W::LocalGet(L_TENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 16, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::LocalSet(host));
        self.f.instruction(&W::I32Const(1));
        self.f.instruction(&W::Else);
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::End);
        self.f.instruction(&W::LocalSet(flag));
    }

    /// Host address of a group member's access: the leader's cached page base
    /// plus the address's offset within the page. The leader proved the whole
    /// group is in that one page, so `& 0xFFF` is against the same page base.
    fn group_host_addr(&mut self, host: u32) {
        self.f.instruction(&W::LocalGet(host));
        self.f.instruction(&W::LocalGet(L_ADDR));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::I32Const(0xFFF));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Add);
    }

    /// The load value for the address in `L_ADDR`, left on the stack
    /// zero-extended: probe-then-direct when the TLB is inlined, host call
    /// otherwise. The per-access path, also the fallback under a missed group.
    fn load_value(&mut self, size_log2: i32) {
        if let Some(c) = self.tlb {
            self.tlb_probe(&c, 1 << size_log2, false);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I64)));
            self.tlb_host_addr();
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&match size_log2 {
                0 => W::I64Load8U(m),
                1 => W::I64Load16U(m),
                2 => W::I64Load32U(m),
                _ => W::I64Load(m),
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
        }
    }

    /// A load into `rd`, with `ext` applying the guest's sign extension.
    ///
    /// `rd == 0` still performs the access: `lb x0, 0(a0)` is a real load that
    /// can fault, and skipping it would lose the trap.
    fn load(&mut self, rd: u8, rs1: u8, imm: i64, size_log2: i32, ext: Option<W<'static>>) {
        let role = self.role;
        if let Role::Leader { store: false, min_imm, span } = role {
            self.group_probe(false, rs1, min_imm, span);
        }

        // Address first, into a local: both the probe and the slow path need
        // it, and it must not be recomputed (rs1 may be about to be written).
        self.addr(rs1, imm);
        self.f.instruction(&W::LocalSet(L_ADDR));

        let grouped = self.tlb.is_some()
            && matches!(role, Role::Leader { store: false, .. } | Role::Member { store: false });
        if grouped {
            self.f.instruction(&W::LocalGet(L_GFLAG));
            self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I64)));
            self.group_host_addr(L_GHOST);
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&match size_log2 {
                0 => W::I64Load8U(m),
                1 => W::I64Load16U(m),
                2 => W::I64Load32U(m),
                _ => W::I64Load(m),
            });
            self.f.instruction(&W::Else);
            // Slim fallback: straight to the host, no second probe. The group
            // flag is only down on a page boundary, MMIO, or a cold TLB — all
            // cases the host services anyway, and keeping this arm tiny keeps
            // the block's code size close to ungrouped.
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.load_value(size_log2);
        }

        if rd == 0 {
            // Still performed: `lb x0, 0(a0)` is a real access that can fault.
            self.f.instruction(&W::Drop);
            self.bail_if_faulted();
            return;
        }
        // Through a local rather than left on the stack, so the fault check
        // happens before the register write. A faulting load must not clobber
        // its destination.
        if self.fwd == Some(rd) {
            // The load overwrites the register L_VAL was forwarding.
            self.fwd = None;
        }
        self.f.instruction(&W::LocalSet(L_TMP));
        self.bail_if_faulted();
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_TMP));
        if let Some(e) = ext {
            self.f.instruction(&e);
        }
        if REG_RESIDENT_ON {
            // The loaded (extended) value is rd's new value: cache it in the
            // local as well as writing the reg file.
            self.f.instruction(&W::LocalTee(reg_local(rd)));
            self.cached[rd as usize] = true;
        }
        self.f.instruction(&W::I64Store(MemArg {
            offset: reg_off(rd),
            align: 3,
            memory_index: 0,
        }));
    }

    /// Write the guest PC control is going to. A block ending in a branch
    /// returns after this, and the host reads the slot.
    fn set_next_pc(&mut self, body: impl FnOnce(&mut Self)) {
        body(self);
        self.f.instruction(&W::LocalSet(L_NEXT));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I64Store(MemArg {
            offset: NEXT_PC_OFF,
            align: 3,
            memory_index: 0,
        }));
    }

    /// Probe the chain table for the successor and tail-call it.
    ///
    /// Falls through to a normal return when there is no valid entry, which is
    /// how a chain ends: the host reads the next PC from the slot and decides
    /// what to do. Emitted at the end of every block.
    /// Add this block's instruction count to the chain total.
    fn count_insns(&mut self, n: i64) {
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: INSNS_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I64Const(n));
        self.f.instruction(&W::I64Add);
        self.f.instruction(&W::I64Store(MemArg { offset: INSNS_OFF, align: 3, memory_index: 0 }));
    }

    fn chain_to_successor(&mut self) {
        let Some(c) = self.chain else { return };

        // entry = base + ((next_pc >> 1) & (entries - 1)) * 16
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::I32Const(1));
        self.f.instruction(&W::I32ShrU);
        self.f.instruction(&W::I32Const((c.entries - 1) as i32));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Const(CHAIN_ENTRY_BYTES as i32));
        self.f.instruction(&W::I32Mul);
        self.f.instruction(&W::I32Const(c.base as i32));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::LocalSet(L_ENT));

        // key == next_pc
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 0, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I64Eq);

        // && gen == current gen
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I32Load(MemArg { offset: 8, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg {
            offset: c.gen_addr as u64,
            align: 2,
            memory_index: 0,
        }));
        self.f.instruction(&W::I32Eq);
        self.f.instruction(&W::I32And);

        // && the chain still has budget. Without this a chain would run until
        // the guest happened to reach uncompiled code, and a spinning kernel
        // loop would never let a timer interrupt through.
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: INSNS_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: BUDGET_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I64LtU);
        self.f.instruction(&W::I32And);

        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I32Load(MemArg { offset: 12, align: 2, memory_index: 0 }));
        // Tail call: a chain runs thousands of blocks deep and ordinary calls
        // would nest the wasm stack until it overflowed.
        self.f.instruction(&W::ReturnCallIndirect { type_index: 0, table_index: 0 });
        self.f.instruction(&W::End);
    }

    /// Direct fall-through link: if there is still budget, tail-call the known
    /// successor by its wasm function index, skipping the chain-table probe.
    ///
    /// Emitted on the fall-through path only, before the shared `End`, so side
    /// exits (which `br` to that `End`) branch over it and still take the probe.
    /// When budget is exhausted this does nothing and control falls through to
    /// `chain_to_successor`, which returns to the host so an interrupt can land.
    /// The successor is entered with the same `(regs, next_pc)` the probe would
    /// have passed it -- `next_pc` is already in `L_NEXT` from `set_next_pc`.
    fn link_to(&mut self, func_idx: u32) {
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: INSNS_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: BUDGET_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I64LtU);
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::ReturnCall(func_idx));
        self.f.instruction(&W::End);
    }

    /// RAS push, emitted at a call (`jal`/`jalr` writing a link register). The
    /// return site is `L_PC + off + ret_extra`; store {return_pc, gen, resolved
    /// idx} at the RAS top and bump the stack pointer. The idx is resolved by a
    /// one-shot probe of the CHAIN table for the return PC -- 0 (invalid) the
    /// first time through, filled once the return block has been compiled, so
    /// the second call/return cycle onward hits.
    fn ras_push(&mut self, ret_extra: i64) {
        let Some(c) = self.chain else { return };
        if c.ras_entries == 0 {
            return;
        }
        // return_pc -> L_RASPC
        self.here(ret_extra);
        self.f.instruction(&W::LocalSet(L_RASPC));
        // RAS slot addr = ras_base + (sp & (ras_entries-1)) * 16 -> L_RASE
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.ras_sp_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const((c.ras_entries - 1) as i32));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Const(CHAIN_ENTRY_BYTES as i32));
        self.f.instruction(&W::I32Mul);
        self.f.instruction(&W::I32Const(c.ras_base as i32));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::LocalSet(L_RASE));
        // slot.key = return_pc
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::LocalGet(L_RASPC));
        self.f.instruction(&W::I64Store(MemArg { offset: 0, align: 3, memory_index: 0 }));
        // slot.gen = current gen
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.gen_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Store(MemArg { offset: 8, align: 2, memory_index: 0 }));
        // chain slot for return_pc -> L_ENT
        self.f.instruction(&W::LocalGet(L_RASPC));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::I32Const(1));
        self.f.instruction(&W::I32ShrU);
        self.f.instruction(&W::I32Const((c.entries - 1) as i32));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Const(CHAIN_ENTRY_BYTES as i32));
        self.f.instruction(&W::I32Mul);
        self.f.instruction(&W::I32Const(c.base as i32));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::LocalSet(L_ENT));
        // slot.idx = (chain.key==return_pc && chain.gen==cur) ? chain.idx : 0
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 0, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_RASPC));
        self.f.instruction(&W::I64Eq);
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I32Load(MemArg { offset: 8, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.gen_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Eq);
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I32)));
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I32Load(MemArg { offset: 12, align: 2, memory_index: 0 }));
        self.f.instruction(&W::Else);
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::End);
        self.f.instruction(&W::I32Store(MemArg { offset: 12, align: 2, memory_index: 0 }));
        // sp++
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.ras_sp_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(1));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::I32Store(MemArg { offset: c.ras_sp_addr as u64, align: 2, memory_index: 0 }));
    }

    /// RAS pop + return, emitted in place of `chain_to_successor` for a block
    /// that ends in a return (`jalr x0, ra`). Pops the RAS and tail-calls the
    /// cached block ONLY if its key equals the actual return target (`L_NEXT`),
    /// its generation is current, its idx is live, and there is budget; any
    /// mismatch drops through and the caller runs the normal chain probe.
    fn ras_return(&mut self) {
        let Some(c) = self.chain else { return };
        if c.ras_entries == 0 {
            return;
        }
        // newsp = sp - 1; store it; keep in L_ENT.
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.ras_sp_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(1));
        self.f.instruction(&W::I32Sub);
        self.f.instruction(&W::LocalTee(L_ENT));
        self.f.instruction(&W::I32Store(MemArg { offset: c.ras_sp_addr as u64, align: 2, memory_index: 0 }));
        // slot addr = ras_base + (newsp & mask)*16 -> L_RASE
        self.f.instruction(&W::LocalGet(L_ENT));
        self.f.instruction(&W::I32Const((c.ras_entries - 1) as i32));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Const(CHAIN_ENTRY_BYTES as i32));
        self.f.instruction(&W::I32Mul);
        self.f.instruction(&W::I32Const(c.ras_base as i32));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::LocalSet(L_RASE));
        // key == L_NEXT
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::I64Load(MemArg { offset: 0, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::I64Eq);
        // && gen current
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::I32Load(MemArg { offset: 8, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg { offset: c.gen_addr as u64, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Eq);
        self.f.instruction(&W::I32And);
        // && idx != 0
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::I32Load(MemArg { offset: 12, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Ne);
        self.f.instruction(&W::I32And);
        // && budget
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: INSNS_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::I64Load(MemArg { offset: BUDGET_OFF, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I64LtU);
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        self.f.instruction(&W::LocalGet(L_REGS));
        self.f.instruction(&W::LocalGet(L_NEXT));
        self.f.instruction(&W::LocalGet(L_RASE));
        self.f.instruction(&W::I32Load(MemArg { offset: 12, align: 2, memory_index: 0 }));
        self.f.instruction(&W::ReturnCallIndirect { type_index: 0, table_index: 0 });
        self.f.instruction(&W::End);
    }

    /// Absolute guest PC of the instruction being emitted.
    fn here(&mut self, extra: i64) {
        self.f.instruction(&W::LocalGet(L_PC));
        self.f.instruction(&W::I64Const(self.off + extra));
        self.f.instruction(&W::I64Add);
    }

    /// Push a branch's condition as an i32: 1 when the branch is taken.
    fn branch_cond(&mut self, rs1: u8, rs2: u8, cmp: W<'static>) {
        self.get(rs1);
        self.get(rs2);
        self.f.instruction(&cmp);
    }

    /// A branch the trace did not follow to completion: leave if control goes
    /// the other way.
    ///
    /// `count` is how many instructions have run on this path, including this
    /// branch, since the run loop's budget is in instructions and paths that
    /// reach different exits have run different amounts.
    fn branch_guard(
        &mut self,
        rs1: u8,
        rs2: u8,
        imm: i64,
        width: u8,
        cmp: W<'static>,
        followed_taken: bool,
        count: i64,
    ) {
        let off = self.off;
        let leave_to = if followed_taken { off + width as i64 } else { off + imm };

        self.branch_cond(rs1, rs2, cmp);
        if followed_taken {
            // The trace assumed taken, so it leaves when the branch is not.
            self.f.instruction(&W::I32Eqz);
        }
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
        self.set_next_pc(|e| {
            e.f.instruction(&W::LocalGet(L_PC));
            e.f.instruction(&W::I64Const(leave_to));
            e.f.instruction(&W::I64Add);
        });
        self.count_insns(count);
        // Depth 1: label 0 is this `if`, label 1 is the block around the body.
        self.f.instruction(&W::Br(1));
        self.f.instruction(&W::End);
    }

    /// A `jal` in the middle of a trace: write the link register and carry on.
    /// It always goes where the trace went, so there is nothing to guard.
    fn jal_link_only(&mut self, rd: u8, width: u8) {
        let w = width as i64;
        self.set_with(rd, |e| e.here(w));
    }

    /// A conditional branch: taken goes to here+imm, not taken to here+width.
    ///
    /// `select` pops its condition last, so the operands go on in the order
    /// taken, not-taken, condition -- getting that backwards silently inverts
    /// every branch in the guest.
    fn branch(&mut self, rs1: u8, rs2: u8, imm: i64, width: u8, cmp: W<'static>) {
        let (off, w) = (self.off, width as i64);
        self.set_next_pc(|e| {
            e.f.instruction(&W::LocalGet(L_PC));
            e.f.instruction(&W::I64Const(off + imm));
            e.f.instruction(&W::I64Add);
            e.f.instruction(&W::LocalGet(L_PC));
            e.f.instruction(&W::I64Const(off + w));
            e.f.instruction(&W::I64Add);
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&cmp);
            e.f.instruction(&W::Select);
        });
    }

    /// Emit the TLB probe, leaving a condition on the stack: true when the
    /// access can be done as a direct load or store from linear memory.
    ///
    /// Also leaves the guest address in `L_ADDR` and the entry address in
    /// `L_TENT`, both of which the caller needs on either branch.
    ///
    /// `size` is in bytes and need not be a power of two: a group leader passes
    /// its whole span, proving every member's access sits in this page.
    fn tlb_probe(&mut self, c: &TlbCfg, size: i32, store: bool) {
        let base = if store { c.write_base } else { c.read_base };

        // entry = base + ((addr >> 12) & (entries - 1)) * 32
        self.f.instruction(&W::LocalGet(L_ADDR));
        self.f.instruction(&W::I64Const(12));
        self.f.instruction(&W::I64ShrU);
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::I32Const((c.entries - 1) as i32));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Const(TLB_ENTRY_BYTES as i32));
        self.f.instruction(&W::I32Mul);
        self.f.instruction(&W::I32Const(base as i32));
        self.f.instruction(&W::I32Add);
        self.f.instruction(&W::LocalSet(L_TENT));

        // vpn matches
        self.f.instruction(&W::LocalGet(L_TENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 0, align: 3, memory_index: 0 }));
        self.f.instruction(&W::LocalGet(L_ADDR));
        self.f.instruction(&W::I64Const(12));
        self.f.instruction(&W::I64ShrU);
        self.f.instruction(&W::I64Eq);

        // && generation matches. `c.gen_addr`, not the chain's word.
        self.f.instruction(&W::LocalGet(L_TENT));
        self.f.instruction(&W::I32Load(MemArg { offset: 8, align: 2, memory_index: 0 }));
        self.f.instruction(&W::I32Const(0));
        self.f.instruction(&W::I32Load(MemArg {
            offset: c.gen_addr as u64,
            align: 2,
            memory_index: 0,
        }));
        self.f.instruction(&W::I32Eq);
        self.f.instruction(&W::I32And);

        // && the access fits inside the page. A multi-byte access near the end
        // of a page would otherwise read into whatever follows it in the host's
        // memory, which is a different guest page entirely.
        if size > 1 {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.f.instruction(&W::I32WrapI64);
            self.f.instruction(&W::I32Const(0xFFF));
            self.f.instruction(&W::I32And);
            self.f.instruction(&W::I32Const(4096 - size));
            self.f.instruction(&W::I32LeU);
            self.f.instruction(&W::I32And);
        }
    }

    /// Address in linear memory for a hit: page base + offset within the page.
    fn tlb_host_addr(&mut self) {
        self.f.instruction(&W::LocalGet(L_TENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 16, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::LocalGet(L_ADDR));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::I32Const(0xFFF));
        self.f.instruction(&W::I32And);
        self.f.instruction(&W::I32Add);
    }

    /// Load from the address already in `L_ADDR`, leaving the value on the
    /// stack zero-extended. Same probe-then-fall-back shape as `load`, but
    /// without computing an address or writing a register — the atomics need
    /// the value in hand before they can decide what to store.
    fn load_at_addr(&mut self, size_log2: i32) {
        let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
        if let Some(c) = self.tlb {
            self.tlb_probe(&c, 1 << size_log2, false);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I64)));
            self.tlb_host_addr();
            self.f.instruction(&match size_log2 {
                2 => W::I64Load32U(m),
                _ => W::I64Load(m),
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.pc();
            self.f.instruction(&W::Call(F_LOAD8U + size_log2 as u32));
        }
    }

    /// Store to the address already in `L_ADDR`, with `value` emitting the word
    /// to write. `value` runs in both arms, so it must be a pure computation.
    fn store_at_addr(&mut self, size_log2: i32, value: impl Fn(&mut Self)) {
        let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
        if let Some(c) = self.tlb {
            self.tlb_probe(&c, 1 << size_log2, true);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
            self.tlb_host_addr();
            value(self);
            self.f.instruction(&match size_log2 {
                2 => W::I64Store32(m),
                _ => W::I64Store(m),
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            value(self);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            value(self);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
        }
    }

    /// An atomic read-modify-write: load, operate, store back, old value to rd.
    ///
    /// There is one hart, so there is nothing to be atomic against, and this is
    /// precisely what the interpreter does — which is the correctness bar here,
    /// since the boot test compares the two byte for byte.
    ///
    /// Compiling it is worth less for the interpreter round trip it saves than
    /// for the trace it stops breaking: atomics are all through the kernel's
    /// locking and refcounting, and while they were uncompilable every one of
    /// those paths ended a trace.
    fn amo(&mut self, rd: u8, rs1: u8, rs2: u8, size_log2: i32, op: AmoOp) {
        let word = size_log2 == 2;

        self.get(rs1);
        self.f.instruction(&W::LocalSet(L_ADDR));

        self.load_at_addr(size_log2);
        self.f.instruction(&W::LocalSet(L_TMP));
        // Before the store: a faulting load must not go on to write memory.
        self.bail_if_faulted();

        self.store_at_addr(size_log2, |e| e.amo_value(rs2, word, op));
        self.bail_if_faulted();

        if rd != 0 {
            if self.fwd == Some(rd) {
                self.fwd = None;
            }
            // Written to the reg file directly below, not through set_with.
            self.invalidate_reg(rd);
            self.f.instruction(&W::LocalGet(L_REGS));
            self.f.instruction(&W::LocalGet(L_TMP));
            // The .w forms deliver the old value sign-extended from bit 31.
            if word {
                self.f.instruction(&W::I64Extend32S);
            }
            self.f.instruction(&W::I64Store(MemArg {
                offset: reg_off(rd),
                align: 3,
                memory_index: 0,
            }));
        }
    }

    /// The value an AMO writes back: `op(old, reg[rs2])`, old being in `L_TMP`.
    fn amo_value(&mut self, rs2: u8, word: bool, op: AmoOp) {
        // Arithmetic and bitwise ops need no narrowing: the store writes the
        // low 32 bits for a .w, and those depend only on the low 32 bits of the
        // operands. The comparisons are the exception — a 64-bit compare of
        // 32-bit quantities is only correct once both sides are extended the
        // same way, signed or unsigned as the opcode dictates.
        let old = |e: &mut Self, signed: bool| {
            e.f.instruction(&W::LocalGet(L_TMP));
            if word && signed {
                e.f.instruction(&W::I64Extend32S);
            }
        };
        let src = |e: &mut Self, signed: bool| {
            e.get(rs2);
            if word {
                if signed {
                    e.f.instruction(&W::I64Extend32S);
                } else {
                    e.f.instruction(&W::I64Const(0xffff_ffff));
                    e.f.instruction(&W::I64And);
                }
            }
        };

        match op {
            AmoOp::Swap => self.get(rs2),
            AmoOp::Add => {
                old(self, false);
                self.get(rs2);
                self.f.instruction(&W::I64Add);
            }
            AmoOp::Xor => {
                old(self, false);
                self.get(rs2);
                self.f.instruction(&W::I64Xor);
            }
            AmoOp::And => {
                old(self, false);
                self.get(rs2);
                self.f.instruction(&W::I64And);
            }
            AmoOp::Or => {
                old(self, false);
                self.get(rs2);
                self.f.instruction(&W::I64Or);
            }
            // select pops (a, b, cond) and yields a when cond is true.
            AmoOp::Min | AmoOp::Max | AmoOp::MinU | AmoOp::MaxU => {
                let signed = matches!(op, AmoOp::Min | AmoOp::Max);
                old(self, signed);
                src(self, signed);
                old(self, signed);
                src(self, signed);
                self.f.instruction(&match op {
                    AmoOp::Min => W::I64LtS,
                    AmoOp::Max => W::I64GtS,
                    AmoOp::MinU => W::I64LtU,
                    _ => W::I64GtU,
                });
                self.f.instruction(&W::Select);
            }
        }
    }

    /// Store the value of `rs2` to the address in `L_ADDR`: the per-access
    /// probe-then-direct path, also the fallback under a missed group.
    fn store_value(&mut self, rs2: u8, size_log2: i32) {
        if let Some(c) = self.tlb {
            self.tlb_probe(&c, 1 << size_log2, true);
            self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
            self.tlb_host_addr();
            self.get(rs2);
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&match size_log2 {
                0 => W::I64Store8(m),
                1 => W::I64Store16(m),
                2 => W::I64Store32(m),
                _ => W::I64Store(m),
            });
            self.f.instruction(&W::Else);
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.get(rs2);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.get(rs2);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
        }
    }

    fn store(&mut self, rs1: u8, rs2: u8, imm: i64, size_log2: i32) {
        let role = self.role;
        if let Role::Leader { store: true, min_imm, span } = role {
            self.group_probe(true, rs1, min_imm, span);
        }

        self.addr(rs1, imm);
        self.f.instruction(&W::LocalSet(L_ADDR));

        let grouped = self.tlb.is_some()
            && matches!(role, Role::Leader { store: true, .. } | Role::Member { store: true });
        if grouped {
            self.f.instruction(&W::LocalGet(L_SFLAG));
            self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));
            self.group_host_addr(L_SHOST);
            self.get(rs2);
            let m = MemArg { offset: 0, align: size_log2 as u32, memory_index: 0 };
            self.f.instruction(&match size_log2 {
                0 => W::I64Store8(m),
                1 => W::I64Store16(m),
                2 => W::I64Store32(m),
                _ => W::I64Store(m),
            });
            self.f.instruction(&W::Else);
            // Slim fallback, same reasoning as the load member's.
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.get(rs2);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + size_log2 as u32));
            self.f.instruction(&W::End);
        } else {
            self.store_value(rs2, size_log2);
        }
        self.bail_if_faulted();
    }
}

/// If `a`,`b` are a fusible `lui`/`auipc` + `addi` pair -- adjacent, with the
/// `addi` reading and overwriting the register the first wrote -- return the
/// destination, whether the folded constant is PC-relative, and the constant
/// itself. The arithmetic mirrors `emit` exactly (`imm as i64` for lui/auipc,
/// the signed `imm` for addi, all i64-wrapping) so the fused value is
/// bit-identical to executing the two instructions in sequence.
fn fusible(a: &Src, b: &Src) -> Option<(u8, bool, i64)> {
    use Instr::*;
    let (d2, rs1, lo) = match b.0 {
        Addi { rd, rs1, imm } => (rd, rs1, imm),
        _ => return None,
    };
    match a.0 {
        // rd == 0 is excluded: writes to x0 are discarded, so there is nothing
        // to fold and `set_with` would emit nothing either way.
        Lui { rd, imm } if rd != 0 && rd == rs1 && rd == d2 => {
            Some((rd, false, (imm as i64).wrapping_add(lo)))
        }
        Auipc { rd, imm } if rd != 0 && rd == rs1 && rd == d2 => {
            // auipc's value is pc + off + imm; the addi then adds lo. off, imm
            // and lo are all known now, so the whole thing is pc + one const.
            Some((rd, true, (a.2 as i64).wrapping_add(imm as i64).wrapping_add(lo)))
        }
        _ => None,
    }
}

impl Emit {
    /// Store a folded lui/auipc+addi constant into `rd` in a single write.
    fn fused_const(&mut self, rd: u8, pc_relative: bool, konst: i64) {
        self.set_with(rd, |e| {
            if pc_relative {
                e.f.instruction(&W::LocalGet(L_PC));
                e.f.instruction(&W::I64Const(konst));
                e.f.instruction(&W::I64Add);
            } else {
                e.f.instruction(&W::I64Const(konst));
            }
        });
    }

    /// Emit a strictly-consecutive same-value `sd` run (memset) as v128 stores:
    /// one 16-byte machine store per two guest stores, plus a scalar tail for an
    /// odd count. Stores `fill` to `reg[rs1] + first_imm + 8*j` for j in 0..count.
    /// One TLB probe covers the run; the wide path runs only on a write-hit with
    /// the whole run proven in-page (so every byte is writable and cannot
    /// fault), and a miss stores each word through the host shim.
    fn memset_v128(&mut self, rs1: u8, fill: u8, first_imm: i64, count: u16) {
        let Some(c) = self.tlb else { return };
        let m = count as i64;
        self.addr(rs1, first_imm);
        self.f.instruction(&W::LocalSet(L_ADDR));
        self.tlb_probe(&c, (m * 8) as i32, true);
        self.f.instruction(&W::If(wasm_encoder::BlockType::Empty));

        // Hit: cache the host page base, splat the fill value into a v128.
        self.f.instruction(&W::LocalGet(L_TENT));
        self.f.instruction(&W::I64Load(MemArg { offset: 16, align: 3, memory_index: 0 }));
        self.f.instruction(&W::I32WrapI64);
        self.f.instruction(&W::LocalSet(L_SHOST));
        self.get(fill);
        self.f.instruction(&W::I64x2Splat);
        self.f.instruction(&W::LocalSet(L_V128));
        // Each store's host address is page_base + (first low-12 bits + its
        // displacement); the run is proven in-page so no low-bit carry escapes.
        let host_off = |e: &mut Self, disp: i64| {
            e.f.instruction(&W::LocalGet(L_SHOST));
            e.f.instruction(&W::LocalGet(L_ADDR));
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32Const(0xFFF));
            e.f.instruction(&W::I32And);
            if disp != 0 {
                e.f.instruction(&W::I32Const(disp as i32));
                e.f.instruction(&W::I32Add);
            }
            e.f.instruction(&W::I32Add);
        };
        for p in 0..(m / 2) {
            host_off(self, p * 16);
            self.f.instruction(&W::LocalGet(L_V128));
            self.f.instruction(&W::V128Store(MemArg { offset: 0, align: 0, memory_index: 0 }));
        }
        if m % 2 == 1 {
            host_off(self, (m - 1) * 8);
            self.get(fill);
            self.f.instruction(&W::I64Store(MemArg { offset: 0, align: 3, memory_index: 0 }));
        }

        self.f.instruction(&W::Else);
        // Miss: store each word through the host shim (which records any fault).
        for j in 0..m {
            self.f.instruction(&W::LocalGet(L_ADDR));
            self.f.instruction(&W::I64Const(j * 8));
            self.f.instruction(&W::I64Add);
            self.get(fill);
            self.pc();
            self.f.instruction(&W::Call(F_STORE8 + 3));
        }
        self.f.instruction(&W::End);
        self.bail_if_faulted();
        SIMD_VEC_STORES.fetch_add(m as u64, Ordering::Relaxed);
    }
}

fn emit(e: &mut Emit, i: &Instr, width: u8) -> bool {
    use Instr::*;
    match *i {
        Lui { rd, imm } => e.set_with(rd, |e| {
            e.f.instruction(&W::I64Const(imm as i64));
        }),
        // auipc needs the guest PC, which is why block bodies take one.
        Auipc { rd, imm } => {
            let off = e.off;
            e.set_with(rd, |e| {
                e.f.instruction(&W::LocalGet(L_PC));
                e.f.instruction(&W::I64Const(off));
                e.f.instruction(&W::I64Add);
                e.f.instruction(&W::I64Const(imm as i64));
                e.f.instruction(&W::I64Add);
            })
        }
        Addi { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64Add);
        }),
        Xori { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64Xor);
        }),
        Ori { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64Or);
        }),
        Andi { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64And);
        }),
        Slti { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64LtS);
            e.f.instruction(&W::I64ExtendI32U);
        }),
        Sltiu { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64LtU);
            e.f.instruction(&W::I64ExtendI32U);
        }),
        Slli { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(shamt as i64));
            e.f.instruction(&W::I64Shl);
        }),
        Srli { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(shamt as i64));
            e.f.instruction(&W::I64ShrU);
        }),
        Srai { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(shamt as i64));
            e.f.instruction(&W::I64ShrS);
        }),
        Add { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Add);
        }),
        Sub { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Sub);
        }),
        Sll { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Shl);
        }),
        Srl { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64ShrU);
        }),
        Sra { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64ShrS);
        }),
        Xor { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Xor);
        }),
        Or { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Or);
        }),
        And { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64And);
        }),
        Slt { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64LtS);
            e.f.instruction(&W::I64ExtendI32U);
        }),
        Sltu { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64LtU);
            e.f.instruction(&W::I64ExtendI32U);
        }),
        Addiw { rd, rs1, imm } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64Add);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Addw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Add);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Subw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Sub);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Slliw { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32Const(shamt as i32));
            e.f.instruction(&W::I32Shl);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Srliw { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32Const(shamt as i32));
            e.f.instruction(&W::I32ShrU);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Sraiw { rd, rs1, shamt } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32Const(shamt as i32));
            e.f.instruction(&W::I32ShrS);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Sllw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.get(rs2);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32Shl);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Srlw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.get(rs2);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32ShrU);
            e.f.instruction(&W::I64ExtendI32S);
        }),
        Sraw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::I32WrapI64);
            e.get(rs2);
            e.f.instruction(&W::I32WrapI64);
            e.f.instruction(&W::I32ShrS);
            e.f.instruction(&W::I64ExtendI32S);
        }),

        // Loads. The host returns raw bytes zero-extended; the signed forms
        // extend inline, keeping the host to one function per direction.
        Lb { rd, rs1, imm } => e.load(rd, rs1, imm, 0, Some(W::I64Extend8S)),
        Lh { rd, rs1, imm } => e.load(rd, rs1, imm, 1, Some(W::I64Extend16S)),
        Lw { rd, rs1, imm } => e.load(rd, rs1, imm, 2, Some(W::I64Extend32S)),
        Ld { rd, rs1, imm } => e.load(rd, rs1, imm, 3, None),
        Lbu { rd, rs1, imm } => e.load(rd, rs1, imm, 0, None),
        Lhu { rd, rs1, imm } => e.load(rd, rs1, imm, 1, None),
        Lwu { rd, rs1, imm } => e.load(rd, rs1, imm, 2, None),

        Sb { rs1, rs2, imm } => e.store(rs1, rs2, imm, 0),
        Sh { rs1, rs2, imm } => e.store(rs1, rs2, imm, 1),
        Sw { rs1, rs2, imm } => e.store(rs1, rs2, imm, 2),
        Sd { rs1, rs2, imm } => e.store(rs1, rs2, imm, 3),

        // Single hart, in-order emission: nothing to order. See is_compilable.
        Fence { .. } => {}

        // CSR ops. `src` is the register/immediate carried through so the host
        // can decide whether a write happens (csrrs x0 does not); `val` is its
        // value. kind: 0 = write, 1 = set, 2 = clear.
        Fld { rd, rs1, imm } => match e.fp {
            Some(c) => e.fp_gated(c, (0, rd, rs1, imm), |e| e.fp_load(c, rd, rs1, imm, 3, false)),
            None => e.fp_call(0, rd, rs1, imm),
        },
        Flw { rd, rs1, imm } => match e.fp {
            Some(c) => e.fp_gated(c, (1, rd, rs1, imm), |e| e.fp_load(c, rd, rs1, imm, 2, true)),
            None => e.fp_call(1, rd, rs1, imm),
        },
        Fsd { rs1, rs2, imm } => match e.fp {
            Some(c) => e.fp_gated(c, (2, rs2, rs1, imm), |e| e.fp_store(c, rs1, rs2, imm, 3)),
            None => e.fp_call(2, rs2, rs1, imm),
        },
        Fsw { rs1, rs2, imm } => match e.fp {
            Some(c) => e.fp_gated(c, (3, rs2, rs1, imm), |e| e.fp_store(c, rs1, rs2, imm, 2)),
            None => e.fp_call(3, rs2, rs1, imm),
        },
        Fp { raw } => match (e.fp, inline_fp_kind(raw)) {
            (Some(c), Some((k, rd, rs1, rs2))) => {
                e.fp_gated(c, (4, 0, 0, raw as i64), move |e| match k {
                    0..=2 => e.fp_sgnj(c, k, rd, rs1, rs2),
                    3 => e.fp_mv_x_d(c, rd, rs1),
                    _ => e.fp_mv_d_x(c, rd, rs1),
                })
            }
            _ => e.fp_call(4, 0, 0, raw as i64),
        },
        Csrrw { rd, rs1, csr } => e.csr_op(csr, rd, rs1, 0, |e| e.get(rs1)),
        Csrrs { rd, rs1, csr } => e.csr_op(csr, rd, rs1, 1, |e| e.get(rs1)),
        Csrrc { rd, rs1, csr } => e.csr_op(csr, rd, rs1, 2, |e| e.get(rs1)),
        Csrrwi { rd, zimm, csr } => e.csr_op(csr, rd, zimm, 0, |e| { e.f.instruction(&W::I64Const(zimm as i64)); }),
        Csrrsi { rd, zimm, csr } => e.csr_op(csr, rd, zimm, 1, |e| { e.f.instruction(&W::I64Const(zimm as i64)); }),
        Csrrci { rd, zimm, csr } => e.csr_op(csr, rd, zimm, 2, |e| { e.f.instruction(&W::I64Const(zimm as i64)); }),

        // LR/SC with one hart: a load and a store that always succeeds. That is
        // what the interpreter does, and matching it exactly is the bar — an SC
        // that reported failure here but success there would diverge the guest.
        Lrw { rd, rs1, .. } => e.load(rd, rs1, 0, 2, Some(W::I64Extend32S)),
        Lrd { rd, rs1, .. } => e.load(rd, rs1, 0, 3, None),
        Scw { rd, rs1, rs2, .. } => {
            e.store(rs1, rs2, 0, 2);
            e.set_with(rd, |e| { e.f.instruction(&W::I64Const(0)); });
        }
        Scd { rd, rs1, rs2, .. } => {
            e.store(rs1, rs2, 0, 3);
            e.set_with(rd, |e| { e.f.instruction(&W::I64Const(0)); });
        }

        Amoswapw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Swap),
        Amoaddw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Add),
        Amoxorw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Xor),
        Amoandw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::And),
        Amoorw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Or),
        Amominw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Min),
        Amomaxw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::Max),
        Amominuw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::MinU),
        Amomaxuw { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 2, AmoOp::MaxU),

        Amoswapd { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Swap),
        Amoaddd { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Add),
        Amoxord { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Xor),
        Amoandd { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::And),
        Amoord { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Or),
        Amomind { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Min),
        Amomaxd { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::Max),
        Amominud { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::MinU),
        Amomaxud { rd, rs1, rs2, .. } => e.amo(rd, rs1, rs2, 3, AmoOp::MaxU),

        Mul { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Mul);
        }),

        // The high 64 bits of a 128-bit product, which wasm cannot produce
        // directly: the classic four-multiply 32-bit split, unsigned first.
        // The signed forms then subtract the wrap-around terms — for two's
        // complement, mulh(a,b) = mulhu(a,b) - (a<0 ? b : 0) - (b<0 ? a : 0),
        // with `(x >> 63) & y` computing each conditional term branchlessly.
        // Operands go through scratch locals (L_TMP = a, L_ADDR = b, L_NEXT =
        // the middle partial sum), all dead between instructions.
        Mulhu { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::LocalSet(L_TMP));
            e.get(rs2);
            e.f.instruction(&W::LocalSet(L_ADDR));
            e.mulhu_of_scratch();
        }),
        Mulh { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::LocalSet(L_TMP));
            e.get(rs2);
            e.f.instruction(&W::LocalSet(L_ADDR));
            e.mulhu_of_scratch();
            e.mulh_sign_term(L_TMP, L_ADDR);
            e.mulh_sign_term(L_ADDR, L_TMP);
        }),
        Mulhsu { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.f.instruction(&W::LocalSet(L_TMP));
            e.get(rs2);
            e.f.instruction(&W::LocalSet(L_ADDR));
            e.mulhu_of_scratch();
            // Only rs1 is signed here.
            e.mulh_sign_term(L_TMP, L_ADDR);
        }),
        // The low 32 bits of a product depend only on the low 32 bits of the
        // operands, so the full-width multiply gives the right answer and only
        // the sign extension from bit 31 is left to do.
        Mulw { rd, rs1, rs2 } => e.set_with(rd, |e| {
            e.get(rs1);
            e.get(rs2);
            e.f.instruction(&W::I64Mul);
            e.f.instruction(&W::I64Extend32S);
        }),

        // Terminators. Always last in a block; the block former guarantees it.
        Beq { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64Eq),
        Bne { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64Ne),
        Blt { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64LtS),
        Bge { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64GeS),
        Bltu { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64LtU),
        Bgeu { rs1, rs2, imm } => e.branch(rs1, rs2, imm, width, W::I64GeU),

        Jal { rd, imm } => {
            let (off, w) = (e.off, width as i64);
            // The link register takes the *fall-through* address, and it has to
            // be written before the jump target is computed for jalr-style
            // aliasing safety. jal has no source register, so order is free
            // here, but keeping both jumps in the same shape avoids surprises.
            e.set_with(rd, |e| e.here(w));
            e.set_next_pc(|e| {
                e.f.instruction(&W::LocalGet(L_PC));
                e.f.instruction(&W::I64Const(off + imm));
                e.f.instruction(&W::I64Add);
            });
            // A call (writes a link register) pushes its return site on the RAS.
            if RAS_ON && (rd == 1 || rd == 5) {
                e.ras_push(w);
            }
        }
        Jalr { rd, rs1, imm } => {
            let (off, w) = (e.off, width as i64);
            // Target first, into the scratch local: `jalr ra, ra, 0` reads and
            // writes the same register, and computing the target after writing
            // the link would jump to the return address instead of the callee.
            e.get(rs1);
            e.f.instruction(&W::I64Const(imm));
            e.f.instruction(&W::I64Add);
            // The ISA clears bit 0 of the target.
            e.f.instruction(&W::I64Const(!1i64));
            e.f.instruction(&W::I64And);
            e.f.instruction(&W::LocalSet(L_TMP));
            e.set_with(rd, |e| e.here(w));
            e.set_next_pc(|e| {
                e.f.instruction(&W::LocalGet(L_TMP));
            });
            // An indirect call (writes a link register) pushes its return site.
            if RAS_ON && (rd == 1 || rd == 5) {
                e.ras_push(w);
            }
            let _ = off;
        }

        _ => return false,
    }
    true
}

/// Emit the body for a run, returning it and how many leading instructions it
/// covers. `None` if the first instruction was already outside the subset.
fn emit_body(
    insns: &[Src],
    chain: Option<ChainCfg>,
    fp: Option<FpCfg>,
    // wasm function index of the physical fall-through successor, when it is in
    // the same batch and page (see TAILLINK_ON). Only consulted when the block
    // ends in a clean whole-trace fall-through.
    link_target: Option<u32>,
) -> Option<(Function, usize)> {
    // Locals after the two params: scratch i64, next-PC i64, chain-entry
    // i32, guest address i64, TLB-entry i32, the four group-cache i32s
    // (load host base + flag, store host base + flag), forwarded value i64.
    let mut locals = alloc::vec![
        (2, ValType::I64),
        (1, ValType::I32),
        (1, ValType::I64),
        (1, ValType::I32),
        (4, ValType::I32),
        (1, ValType::I64),
    ];
    // Then, for register residency, 32 i64 locals at REG_LOCAL_BASE.. holding
    // guest registers. Declared only when the feature is on so the off-path
    // wasm is byte-for-byte unchanged.
    if REG_RESIDENT_ON {
        locals.push((32, ValType::I64));
    }
    // One v128 scratch (L_V128) for the memset SIMD fast path.
    if SIMD_ON {
        locals.push((1, ValType::V128));
    }
    // Return-address-stack scratch: L_RASPC (i64) then L_RASE (i32).
    if RAS_ON {
        locals.push((1, ValType::I64));
        locals.push((1, ValType::I32));
    }
    let mut e = Emit {
        f: Function::new(locals),
        off: 0,
        chain,
        tlb: chain.and_then(|c| c.tlb),
        fp,
        gen_addr: chain.map(|c| c.gen_addr).unwrap_or(0),
        role: Role::Solo,
        fwd: None,
        cached: [false; 32],
    };
    // Group accesses only when the TLB is inlined; without it there is no
    // probe to share and every access is a host call anyway.
    let roles = if GROUP_CSE_ON && e.tlb.is_some() {
        plan_groups(insns)
    } else {
        alloc::vec![Role::Solo; insns.len()]
    };
    // Side exits branch out of this, to the chain probe below.
    e.f.instruction(&W::Block(wasm_encoder::BlockType::Empty));

    let mut n = 0;
    let mut skip_next = false;
    for (k, (i, width, off)) in insns.iter().enumerate() {
        // The previous iteration fused this instruction into its predecessor.
        if skip_next {
            skip_next = false;
            continue;
        }
        e.off = *off as i64;
        e.role = roles[k];
        let last = k + 1 == insns.len();

        if is_compilable(i) {
            // memset SIMD: the leader emits the whole run as v128 stores; the
            // members emit nothing but still retire. Both keep instret exact.
            match e.role {
                Role::Vfill { first_imm, count, fill } => {
                    let rs1 = mem_access(i).map(|(b, ..)| b).unwrap_or(0);
                    e.memset_v128(rs1, fill, first_imm, count);
                    n += 1;
                    continue;
                }
                Role::VfillSkip => {
                    n += 1;
                    continue;
                }
                _ => {}
            }
            // Peephole: fold lui/auipc + addi into one store before emitting.
            // Both are Role::Solo (never load/store group members), so this
            // disturbs no group probe. instret still counts both.
            if FUSE_ON && !last {
                if let Some((rd, pc_rel, konst)) = fusible(&insns[k], &insns[k + 1]) {
                    e.fused_const(rd, pc_rel, konst);
                    n += 2;
                    FUSE_TOTAL.fetch_add(2, Ordering::Relaxed);
                    FUSE_HIT.fetch_add(1, Ordering::Relaxed);
                    skip_next = true;
                    continue;
                }
            }
            FUSE_TOTAL.fetch_add(1, Ordering::Relaxed);
            if !emit(&mut e, i, *width) {
                break;
            }
            n += 1;
            continue;
        }
        if !is_terminator(i) {
            break;
        }

        // The final terminator ends the trace and sets the next PC outright.
        if last {
            if emit(&mut e, i, *width) {
                n += 1;
            }
            break;
        }

        // Mid-trace: the trace followed one way, so guard the other.
        use Instr::*;
        n += 1;
        match *i {
            Jal { rd, .. } => {
                e.jal_link_only(rd, *width);
                // A mid-trace call pushes its return site on the RAS.
                if RAS_ON && (rd == 1 || rd == 5) {
                    e.ras_push(*width as i64);
                }
            }
            Beq { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64Eq, imm < 0, n as i64),
            Bne { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64Ne, imm < 0, n as i64),
            Blt { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64LtS, imm < 0, n as i64),
            Bge { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64GeS, imm < 0, n as i64),
            Bltu { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64LtU, imm < 0, n as i64),
            Bgeu { rs1, rs2, imm } => e.branch_guard(rs1, rs2, imm, *width, W::I64GeU, imm < 0, n as i64),
            // Indirect jumps are never mid-trace; the tracer stops at them.
            _ => {
                n -= 1;
                break;
            }
        }
    }
    if n == 0 {
        return None;
    }
    // A block that fell through has not written its next PC. Doing it for every
    // block keeps the chain probe uniform and gives the host one rule for
    // advancing the PC.
    let ends_fallthrough = !insns[..n].last().map(|(i, _, _)| is_terminator(i)).unwrap_or(false);
    if ends_fallthrough {
        // One past the last instruction: its own offset plus its width.
        let (_, w, off) = insns[n - 1];
        let end = off as i64 + w as i64;
        e.set_next_pc(|e| {
            e.f.instruction(&W::LocalGet(L_PC));
            e.f.instruction(&W::I64Const(end));
            e.f.instruction(&W::I64Add);
        });
    }
    e.count_insns(n as i64);

    // Direct fall-through link. Only for a clean whole-trace fall-through: if
    // the block bailed early (n < len) the successor PC is not the batch-
    // computed one, and if it ended at a terminator there is no fall-through.
    if TAILLINK_ON && ends_fallthrough && n == insns.len() {
        LINK_TOTAL.fetch_add(1, Ordering::Relaxed);
        if let Some(idx) = link_target {
            LINK_HIT.fetch_add(1, Ordering::Relaxed);
            e.link_to(idx);
        }
    }

    // RAS return: a block ending in `jalr x0, ra` pops the return-address stack
    // and tail-calls the predicted successor. Inside the block, before the End,
    // so mid-trace side exits (which `br` to the End) skip it and take the
    // normal probe -- only the actual return reaches here. On a RAS miss it
    // drops through to the shared chain probe below.
    if RAS_ON {
        if let Some((Instr::Jalr { rd, rs1, .. }, _, _)) = insns.get(n - 1).copied() {
            if rd == 0 && (rs1 == 1 || rs1 == 5) {
                e.ras_return();
            }
        }
    }

    // Shared tail: every exit, side or final, arrives here with its next PC
    // set and its own instruction count already added.
    e.f.instruction(&W::End);
    e.chain_to_successor();
    e.f.instruction(&W::End);
    Some((e.f, n))
}

fn common_imports(imports: &mut ImportSection) {
    imports.import(
        "env",
        "mem",
        EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    );
    // Order matters: these become function indices 0..8, which the emitter
    // computes from the access width.
    for name in IMPORTS.iter().take(4) {
        imports.import("env", name, EntityType::Function(1));
    }
    for name in IMPORTS.iter().skip(4) {
        imports.import("env", name, EntityType::Function(2));
    }
    // Index 8 (F_CSR). Its own type, so it goes in explicitly rather than
    // through the load/store loops above.
    imports.import("env", "csr", EntityType::Function(3));
    // Index 9 (F_FP). Same shape as a store: two operands and the pc.
    imports.import("env", "fp", EntityType::Function(2));
}

fn common_types() -> TypeSection {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32, ValType::I64], []); // 0: block(regs, pc)
    types
        .ty()
        .function([ValType::I64, ValType::I64], [ValType::I64]); // 1: load(addr, pc)
    types
        .ty()
        .function([ValType::I64, ValType::I64, ValType::I64], []); // 2: store(addr, val, pc)
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I64, ValType::I32, ValType::I64],
        [],
    ); // 3: csr(csr, rd, src, val, kind, pc)
    types
}

/// For each run, the wasm function index of its physical fall-through successor
/// when that successor is another run in this same batch on the same page.
///
/// Function index of run `j` is `FIRST_DEFINED + j` (the imports occupy
/// `0..FIRST_DEFINED`, then the run bodies in order). Same-page only: the trace
/// never crosses a page, so a successor on the same page shares the
/// predecessor's invalidation fate; a successor on the next page could be
/// invalidated independently and is left to the probe.
fn fallthrough_links(runs: &[&[Src]], paddrs: &[u64]) -> alloc::vec::Vec<Option<u32>> {
    use alloc::collections::BTreeMap;
    if !TAILLINK_ON || paddrs.len() != runs.len() {
        return alloc::vec![None; runs.len()];
    }
    let mut start: BTreeMap<u64, u32> = BTreeMap::new();
    for (i, &pa) in paddrs.iter().enumerate() {
        // A batch should not hold two runs at one physical start; if it does,
        // the first wins, which is harmless (both are the same guest code).
        start.entry(pa).or_insert(i as u32);
    }
    runs.iter()
        .enumerate()
        .map(|(i, run)| {
            // The fall-through address is one past the LAST instruction, using
            // its byte offset -- NOT the sum of widths. A trace follows `jal`
            // jumps within the page, so its instructions are not contiguous and
            // the two differ. This is the same `end` emit_body sets as next_pc.
            let (_, w, off) = *run.last()?;
            let end = off as u64 + w as u64;
            let succ = paddrs[i].wrapping_add(end);
            // Successor must land on the same page, i.e. not roll into the next.
            if (succ & !0xFFF) != (paddrs[i] & !0xFFF) {
                return None;
            }
            start.get(&succ).map(|&j| FIRST_DEFINED + j)
        })
        .collect()
}

/// Compile many runs into a module that installs its blocks into the *host's*
/// function table, starting at `table_base`.
///
/// This is the form the emulator uses. The host's run loop is Rust compiled to
/// wasm; if entering a block had to go Rust -> JS -> generated wasm it would
/// cost a wasm-to-JS call each way, around 32 ns, and give back most of what
/// the JIT gained. Declaring an active element segment on an imported table
/// puts the block functions directly into the host's own indirect table, and a
/// function pointer in Rust/wasm *is* a table index -- so the host transmutes
/// `table_base + i` and calls it, with no JS in the hot path.
///
/// The host must export its table as `__indirect_function_table` (link with
/// `--export-table`) and must have reserved `runs.len()` slots at `table_base`.
///
/// There is no exported dispatcher here: the host indexes the table itself.
pub fn compile_many_into_table(
    runs: &[&[Src]],
    // Physical start address of each run, parallel to `runs`. Used to find
    // same-page fall-through successors within this batch for direct linking.
    paddrs: &[u64],
    // Formerly baked into the element-segment offset; now supplied at
    // instantiation via the imported `table_base` global, so the module is
    // relocatable. Kept in the signature so callers that grow the host table to
    // `table_base + n` before instantiating read as before.
    _table_base: u32,
    chain: Option<ChainCfg>,
    fp: Option<FpCfg>,
) -> Option<(Vec<u8>, Vec<usize>)> {
    let links = fallthrough_links(runs, paddrs);
    let (bodies, covered) = emit_all(runs, chain, fp, &links)?;

    let n = bodies.len() as u32;
    let types = common_types();
    let mut imports = ImportSection::new();
    common_imports(&mut imports);
    imports.import(
        "env",
        "__indirect_function_table",
        EntityType::Table(TableType {
            element_type: RefType::FUNCREF,
            table64: false,
            // We write n slots starting at a base supplied at instantiation, so
            // the static lower bound the host must meet is just n; the actual
            // [base, base+n) fit is checked against the real table when the
            // element segment runs.
            minimum: n as u64,
            maximum: None,
            shared: false,
        }),
    );
    // The install slot is NOT baked in: the element-segment offset reads an
    // imported i32 global the host sets at instantiation. That makes the module
    // relocatable — the same compiled bytes can be installed at any base — which
    // is what lets a compiled block be cached and reused across sessions, where
    // the table layout differs. Imported globals have their own index space, so
    // this is global 0 and does not shift the function indices below.
    imports.import(
        "env",
        "table_base",
        EntityType::Global(GlobalType { val_type: ValType::I32, mutable: false, shared: false }),
    );

    let mut funcs = FunctionSection::new();
    for _ in 0..n {
        funcs.function(0);
    }

    let idxs: Vec<u32> = (FIRST_DEFINED..FIRST_DEFINED + n).collect();
    let mut elems = ElementSection::new();
    elems.active(
        Some(0),
        &ConstExpr::global_get(0),
        Elements::Functions((&idxs[..]).into()),
    );

    let mut code = CodeSection::new();
    emit_code(&mut code, bodies);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&elems);
    m.section(&code);
    Some((m.finish(), covered))
}

/// Emit every run's body, or `None` if not one of them was compilable.
fn emit_all(
    runs: &[&[Src]],
    chain: Option<ChainCfg>,
    fp: Option<FpCfg>,
    // Per-run fall-through link target (wasm function index), or None. Empty
    // means "no linking", i.e. every run gets None.
    links: &[Option<u32>],
) -> Option<(Vec<Option<Function>>, Vec<usize>)> {
    let mut bodies = Vec::new();
    let mut covered = Vec::with_capacity(runs.len());
    let mut any = false;
    for (i, run) in runs.iter().enumerate() {
        let link = links.get(i).copied().flatten();
        count_simd_reach(run);
        match emit_body(run, chain, fp, link) {
            Some((f, n)) => {
                covered.push(n);
                bodies.push(Some(f));
                any = true;
            }
            None => {
                covered.push(0);
                bodies.push(None);
            }
        }
    }
    if any { Some((bodies, covered)) } else { None }
}

/// Runs that compiled to nothing still take a slot, with a body that does
/// nothing. The host must not call those, but a no-op is a far better failure
/// than an index shift silently running some other block's code.
fn emit_code(code: &mut CodeSection, bodies: Vec<Option<Function>>) {
    for b in bodies {
        match b {
            Some(f) => {
                code.function(&f);
            }
            None => {
                let mut f = Function::new([]);
                f.instruction(&W::End);
                code.function(&f);
            }
        }
    }
}

/// Compile many runs into one self-contained module with its own table and an
/// exported `dispatch(index, regs, pc)`.
///
/// Used by the Node harnesses, which have no host module to borrow a table
/// from. The emulator uses `compile_many_into_table` instead.
pub fn compile_many(runs: &[&[Src]], fp: Option<FpCfg>) -> Option<(Vec<u8>, Vec<usize>)> {
    // No chaining: the harnesses that use this have no host table to chain
    // through, and they measure single-block cost deliberately. No linking
    // either -- that needs the host table and physical addresses.
    let (bodies, covered) = emit_all(runs, None, fp, &[])?;

    let mut types = common_types();
    types
        .ty()
        .function([ValType::I32, ValType::I32, ValType::I64], []); // 3: dispatch

    let mut imports = ImportSection::new();
    common_imports(&mut imports);

    let n = bodies.len() as u32;
    let mut funcs = FunctionSection::new();
    for _ in 0..n {
        funcs.function(0);
    }
    funcs.function(3);

    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        table64: false,
        minimum: n as u64,
        maximum: Some(n as u64),
        shared: false,
    });

    let idxs: Vec<u32> = (FIRST_DEFINED..FIRST_DEFINED + n).collect();
    let mut elems = ElementSection::new();
    elems.active(
        Some(0),
        &ConstExpr::i32_const(0),
        Elements::Functions((&idxs[..]).into()),
    );

    let mut exports = ExportSection::new();
    exports.export("dispatch", ExportKind::Func, FIRST_DEFINED + n);

    let mut code = CodeSection::new();
    emit_code(&mut code, bodies);
    let mut d = Function::new([]);
    d.instruction(&W::LocalGet(1)); // regs
    d.instruction(&W::LocalGet(2)); // pc
    d.instruction(&W::LocalGet(0)); // block index
    d.instruction(&W::CallIndirect {
        type_index: 0,
        table_index: 0,
    });
    d.instruction(&W::End);
    code.function(&d);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&tables);
    // Section order is fixed by the spec: table (4), export (7), element (9),
    // code (10). Emitting element before export is a CompileError, not a
    // warning.
    m.section(&exports);
    m.section(&elems);
    m.section(&code);
    Some((m.finish(), covered))
}

/// Compile a single run into its own module, for tests.
///
/// **Not** how blocks should be compiled in production: one module per block
/// makes the host's call site megamorphic, measured at 293 ns per entry against
/// 38 ns through `compile_many`.
pub fn compile(insns: &[Src], fp: Option<FpCfg>) -> Option<(Vec<u8>, usize)> {
    let (f, n) = emit_body(insns, None, fp, None)?;

    let types = common_types();
    let mut imports = ImportSection::new();
    common_imports(&mut imports);

    let mut funcs = FunctionSection::new();
    funcs.function(0);

    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, FIRST_DEFINED);

    let mut code = CodeSection::new();
    code.function(&f);

    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    Some((m.finish(), n))
}
