//! Block formation and hot-block selection.
//!
//! The compiler in `riscv-jit` turns a run of instructions into wasm. This is
//! the part that decides *which* runs, and when.
//!
//! Blocks are keyed on **physical** address, exactly as the decode cache is,
//! and for the same reason: a virtual key would have to be discarded on every
//! `sfence.vma` and `satp` write, which Linux does on each context switch, and
//! the flushing would cost more than the caching saves. Physical mappings only
//! change when memory changes, so `fence.i` is the only invalidator — the same
//! contract a real instruction cache runs under.
//!
//! Formation is lazy and driven by the interpreter: when it reports
//! `block_start` (a taken branch, jump, trap or return), the PC is a block
//! entry. A counter is bumped and, once it crosses `HOT`, the run is decoded
//! forward and queued for compilation. Cold code is never compiled, which keeps
//! compilation cost proportional to code that actually runs rather than code
//! that merely exists.

extern crate alloc;

use alloc::vec::Vec;
use riscv_core::types::Instr;
use riscv_jit::{is_compilable, Src};
use riscv_supervisor::Supervisor;
use riscv_core::execute::Bus;

/// Executions of a block entry before it is worth compiling.
///
/// Low enough that a loop pays for itself almost immediately, high enough that
/// straight-line setup code executed once is never compiled. The cost of being
/// wrong is asymmetric: compiling cold code wastes work permanently, while
/// waiting a few extra iterations on hot code costs microseconds.
const HOT: u32 = 16;

/// Longest trace compiled as one block.
///
/// Now that traces follow branches this is a real limit rather than a
/// formality: a hot loop body would otherwise be traced until it closed, and
/// deeply nested code could run long. Longer traces amortise the per-block
/// costs better but compile more code that may not all be reached.
const MAX_RUN: usize = 64;

/// What a block-cache slot held when a differently-tagged block start arrived.
/// See `Jit::slot_state`.
pub const SLOT_BINS: [&str; 4] = [
    "empty (first use)",
    "evicted Cold(1) (no progress lost)",
    "evicted Cold(2+) (hotness reset)",
    "evicted Queued/Compiled/Rejected",
];

/// Direct-mapped, sized to match the decode cache, which measured best at 32768
/// entries on the boot workload.
const SLOTS: usize = 262144;

#[derive(Clone, Copy, PartialEq)]
enum State {
    /// Seen, but not yet hot enough to compile.
    Cold(u32),
    /// Queued for compilation; the host has not installed it yet.
    Queued,
    /// Compiled: the ABSOLUTE index in the host's function table, the block's
    /// length in bytes, and how many instructions it retires.
    ///
    /// Absolute, not relative to a batch. Batches link at whatever the table
    /// length was at the time, so a per-batch index plus a single stored base
    /// is correct only for the first batch.
    ///
    /// Both the byte length and the instruction count are needed and they are
    /// not derivable from each other: the PC advances by bytes, the run loop's
    /// budget by instructions, and a block mixes 2- and 4-byte encodings.
    Compiled {
        /// Absolute slot in the host's function table.
        idx: u32,
        /// Encoded length, for advancing the PC on a fall-through.
        bytes: u32,
        /// Instructions retired, for the run loop's budget.
        insns: u32,
        /// Ends in a branch or jump, so the PC comes from the next-PC slot the
        /// block wrote rather than from `bytes`.
        branchy: bool,
    },
    /// Examined and found to start with an instruction the compiler cannot
    /// handle. Remembered so the same run is not decoded again on every visit.
    Rejected,
}

#[derive(Clone, Copy)]
struct Slot {
    tag: u64,
    state: State,
}

/// A compiled block ready to run.
#[derive(Clone, Copy)]
pub struct Hit {
    pub idx: u32,
    pub bytes: u32,
    pub insns: u32,
    pub branchy: bool,
}

/// Entries in the chain table generated code probes. Power of two; the
/// compiler is told the mask.
///
/// 65536 (1 MiB): at 8192 the Python workload evicted 478k chain entries in a
/// 12s window — 60% of all probe misses were conflict evictions, each one a
/// host round trip. CPython's hot code footprint (ceval + object protocol)
/// simply has more live block-to-block edges than a shell workload.
pub const CHAIN_ENTRIES: usize = 65536;

/// Depth of the return-address stack (RAS). A stack, not a hash: a `jal ra`
/// pushes the return site's resolved block, a `jalr ra` (return) pops it and
/// tail-calls directly, so returns never churn or miss the shared chain table.
/// 256 covers guest call depth with room; deeper recursion wraps (those returns
/// simply fall back to the chain probe). Reuses `ChainEntry` for its slots.
pub const RAS_ENTRIES: usize = 256;

/// Entries per inlined-TLB table.
///
/// 4096 (128 KB per table): a real Python data job measured 26.6M conflict
/// evictions through 1024-entry tables in one session — a data workload's
/// working set is hundreds of pages where a shell's is dozens, and every
/// eviction is a host call on the next access to the evicted page.
pub const TLB_ENTRIES: usize = 4096;

/// One inlined-TLB entry, laid out exactly as generated code reads it: virtual
/// page at 0, generation at 8, host page address at 16.
///
/// 32 bytes so the index is a shift rather than a multiply, and `repr(C)`
/// because the layout is a contract with the compiler.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TlbEntry {
    pub vpn: u64,
    pub gen: u32,
    pub _pad: u32,
    pub base: u64,
    pub _pad2: u64,
}

impl TlbEntry {
    const EMPTY: TlbEntry = TlbEntry { vpn: u64::MAX, gen: 0, _pad: 0, base: 0, _pad2: 0 };
}

/// One chain-table entry, laid out exactly as generated code reads it: key at
/// 0, generation at 8, function-table index at 12.
///
/// `repr(C)` is part of the contract with the compiler, not tidiness.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ChainEntry {
    pub key: u64,
    pub gen: u32,
    pub idx: u32,
}

/// Entries in the virtually-keyed lookup cache. 4096 x 32 bytes is 128 KB and
/// stays in L2; the physical table it fronts is 512 KB and does not.
const VVCACHE: usize = 4096;

/// Slots in the exact key-page set, and how far an insert probes before
/// declaring the generation overflowed. ~4k live code pages against 16k
/// slots keeps both events rare.
const KEY_VPNS: usize = 16384;
const KEY_VPN_PROBES: usize = 8;

#[derive(Clone, Copy)]
struct VSlot {
    /// Virtual PC with the privilege level folded in, so user and supervisor
    /// mappings of the same address cannot alias.
    key: u64,
    /// Translation generation this was valid for.
    gen: u32,
    hit: Hit,
}

pub struct Jit {
    slots: Vec<Slot>,
    /// Probed directly by generated code, which tail-calls the successor when
    /// it finds a live entry. Filled by `lookup`.
    pub chain: Vec<ChainEntry>,
    /// Return-address stack: `RAS_ENTRIES` `ChainEntry` slots (key = return PC).
    /// A call pushes its resolved return block; a return pops and tail-calls it,
    /// gen- and PC-validated, so a return that the callee's churn evicted from
    /// the shared chain table still hits here. Any mismatch falls back to the
    /// chain probe, so it is a pure fast-path overlay.
    pub chain_ras: Vec<ChainEntry>,
    /// RAS stack pointer (next free slot), in linear memory so both the pushing
    /// and popping blocks reach it. Wraps modulo `RAS_ENTRIES`.
    pub ras_sp: u32,
    /// Physical page numbers (`paddr >> 12`) that have compiled blocks. A store
    /// to one of these is self-modifying code and must invalidate; a `fence.i`
    /// with none of them written since is spurious and can skip the discard.
    /// Set on install, cleared on flush.
    code_pages: alloc::collections::BTreeSet<u64>,
    /// A store has hit a code page since the last flush, so the next `fence.i`
    /// must actually discard. False = the guest's `fence.i` did not touch any
    /// live compiled code and the discard is skippable. See `SELECTIVE_FENCEI`.
    pub smc_dirty: bool,
    /// Diagnostic: how many times the whole block cache was flushed (fence.i /
    /// icache-gen change / restore). Each flush zeros 262144 slots and re-warms.
    pub flushes: u64,
    /// Current generation: translation generation and privilege combined.
    /// Generated code compares against this word, so it lives in linear memory
    /// at a fixed address the compiler is told.
    pub chain_gen: u32,
    /// Generation the inlined data TLB validates against. Separate from
    /// `chain_gen` so a single-page `sfence.vma` does not void every cached
    /// translation; see `Supervisor::data_trans_gen`. Privilege is still folded
    /// in -- an entry cached in user mode must not be reused by the kernel
    /// without re-checking SUM.
    pub data_gen: u32,
    /// 1 while mstatus.FS == Dirty. Compiled FP fast paths are gated on it;
    /// kept current by `refresh_gen`, which runs at every compiled-code entry,
    /// and by the host's csr/fp shims, which cover FS changes inside a chain.
    pub fs_word: u32,
    /// Virtual-PC cache in front of `slots`, so a chain hop costs a probe
    /// rather than an MMU translation.
    vcache: Vec<VSlot>,
    /// Inlined TLBs probed directly by compiled loads and stores. Separate
    /// tables because read and write permissions differ.
    pub tlb_r: Vec<TlbEntry>,
    pub tlb_w: Vec<TlbEntry>,
    /// Blocks decoded and awaiting compilation by the host: physical address
    /// and the run itself.
    pending: Vec<(u64, Vec<Src>)>,
    /// How many blocks the host has installed so far; also the next table index.
    installed: u32,
    /// Exact set of virtual pages that hold chain/vcache keys, so a
    /// single-page `sfence.vma` — 147k per real Python session, nearly all
    /// naming DATA pages (allocator munmaps) — can skip the wholesale chain
    /// invalidation that cost ~760 chain stops per flush.
    ///
    /// Exact, not a hash bitmap: a 256 KB munmap flushes 64 pages one sfence
    /// at a time, so per-page false positives COMPOUND — with ~4k live code
    /// pages in a 64k-bit bloom the odds of some page aliasing were ~95% per
    /// munmap, and the first version of this filter measured as a placebo.
    ///
    /// Open-addressed, entry = `vpn << 12 | trans_gen & 0xfff`: stale
    /// generations self-invalidate (no clearing on satp writes, which happen
    /// per context switch), and a 12-bit generation wrap can only produce a
    /// false positive. If insertion ever fails, `key_vpns_full` makes every
    /// query conservative until the generation moves.
    key_vpns: Vec<u64>,
    /// `trans_gen + 1` while the set has overflowed for that generation, else
    /// 0 — a query in the overflowed generation answers "maybe" to everything.
    key_vpns_full: u32,
    /// Diagnostics: single-page sfences that skipped chain invalidation vs
    /// took the conservative bump.
    pub sfence_skips: u64,
    pub sfence_hits: u64,

    // Counters, for reporting rather than for the hot path.
    pub formed: u64,
    pub rejected: u64,
    pub entries: u64,
    /// Diagnostic: what the block-cache slot held when a block start arrived.
    /// See `SLOT_BINS`. Always counted -- one increment on a cold path.
    ///
    /// The slot is direct-mapped on `(paddr >> 1) & (SLOTS - 1)`, which with
    /// 262144 slots distinguishes code only within a 512 KB window of PHYSICAL
    /// address space. A guest with a gigabyte of RAM scatters its code far more
    /// widely than that, and a collision does not merely miss -- it resets the
    /// hotness counter to 1, so a block executed regularly can be kept
    /// permanently below the compile threshold by an unrelated neighbour.
    /// Bins 2 and 3 are what that would look like.
    pub slot_state: [u64; 4],
    /// Block entries that found a compiled block ready to run.
    pub hits: u64,
    /// Guest instructions those hits covered.
    pub hit_insns: u64,
}

impl Default for Jit {
    fn default() -> Self {
        Self::new()
    }
}

impl Jit {
    pub fn new() -> Self {
        Self {
            slots: alloc::vec![Slot { tag: u64::MAX, state: State::Cold(0) }; SLOTS],
            // gen 0 means "never valid", and live generations start at 1, so a
            // zeroed table cannot false-hit on a guest PC of 0.
            chain: alloc::vec![ChainEntry { key: 0, gen: 0, idx: 0 }; CHAIN_ENTRIES],
            chain_ras: alloc::vec![ChainEntry { key: 0, gen: 0, idx: 0 }; RAS_ENTRIES],
            ras_sp: 0,
            code_pages: alloc::collections::BTreeSet::new(),
            smc_dirty: false,
            flushes: 0,
            chain_gen: 1,
            data_gen: 1,
            fs_word: 0,
            tlb_r: alloc::vec![TlbEntry::EMPTY; TLB_ENTRIES],
            tlb_w: alloc::vec![TlbEntry::EMPTY; TLB_ENTRIES],
            vcache: alloc::vec![
                VSlot {
                    key: u64::MAX,
                    gen: u32::MAX,
                    hit: Hit { idx: 0, bytes: 0, insns: 0, branchy: false },
                };
                VVCACHE
            ],
            pending: Vec::new(),
            key_vpns: alloc::vec![u64::MAX; KEY_VPNS],
            key_vpns_full: 0,
            sfence_skips: 0,
            sfence_hits: 0,
            installed: 0,
            formed: 0,
            rejected: 0,
            entries: 0,
            slot_state: [0; 4],
            hits: 0,
            hit_insns: 0,
        }
    }

    /// Shifted by one because RVC permits 2-byte alignment: bit 0 of a physical
    /// instruction address carries no information, bit 1 does.
    fn index(paddr: u64) -> usize {
        ((paddr >> 1) as usize) & (SLOTS - 1)
    }

    /// Recompute the generation word from the machine's current state.
    ///
    /// Called before entering compiled code. Any change invalidates every chain
    /// entry at once, without touching the table.
    pub fn refresh_gen(&mut self, cpu: &Supervisor) {
        // +1 so the word is never 0, which is the "empty entry" marker.
        self.chain_gen = 1 + ((cpu.trans_gen << 2) | (cpu.priv_level as u32 & 3));
        self.data_gen = 1 + ((cpu.data_trans_gen << 2) | (cpu.priv_level as u32 & 3));
        self.fs_word = cpu.fs_is_dirty() as u32;
    }

    #[inline]
    fn key_vpn_slot(vpn: u64, probe: usize) -> usize {
        // Fibonacci hash spreads consecutive pages; linear probe from there.
        ((vpn.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 50) as usize + probe) & (KEY_VPNS - 1)
    }

    /// Record that this virtual page holds a chain/vcache key.
    fn note_key_page(&mut self, va: u64, trans_gen: u32) {
        let vpn = va >> 12;
        let packed = (vpn << 12) | (trans_gen & 0xFFF) as u64;
        for p in 0..KEY_VPN_PROBES {
            let s = Self::key_vpn_slot(vpn, p);
            let e = self.key_vpns[s];
            if e == packed {
                return;
            }
            // Free, or dead (stale generation): claim it.
            if e == u64::MAX || (e & 0xFFF) != (trans_gen & 0xFFF) as u64 {
                self.key_vpns[s] = packed;
                return;
            }
        }
        // Every candidate slot holds a live entry for another page. Losing
        // this page would be unsound (a false NEGATIVE), so stop trusting the
        // set for the rest of this generation. The next real bump kills all
        // entries anyway, so exactness recovers on its own.
        self.key_vpns_full = trans_gen.wrapping_add(1);
    }

    /// Might this virtual page hold a chain or vcache key? Exact within a
    /// generation: matches only a recorded (vpn, gen) pair, so a data page
    /// can never alias a code page. False positives only via the 12-bit
    /// generation wrap or the overflow flag — both safe, both rare.
    pub fn page_may_have_keys(&self, va: u64, trans_gen: u32) -> bool {
        if self.key_vpns_full == trans_gen.wrapping_add(1) {
            return true;
        }
        let vpn = va >> 12;
        let packed = (vpn << 12) | (trans_gen & 0xFFF) as u64;
        (0..KEY_VPN_PROBES).any(|p| self.key_vpns[Self::key_vpn_slot(vpn, p)] == packed)
    }

    pub fn fs_word_addr(&self) -> u32 {
        core::ptr::addr_of!(self.fs_word) as u32
    }

    /// Invalidate one page's inlined-TLB entries, as `sfence.vma rs1` asks.
    ///
    /// Generation 0 is the empty marker, so this is a store rather than a
    /// table walk. Both directions go: the read and write tables are separate
    /// because their permission checks differ, but the mapping being retired
    /// is the same mapping in each.
    pub fn invalidate_tlb_page(&mut self, vaddr: u64) {
        let vpn = vaddr >> 12;
        let i = (vpn as usize) & (TLB_ENTRIES - 1);
        if self.tlb_r[i].vpn == vpn {
            self.tlb_r[i].gen = 0;
        }
        if self.tlb_w[i].vpn == vpn {
            self.tlb_w[i].gen = 0;
        }
    }

    pub fn data_gen_addr(&self) -> u32 {
        core::ptr::addr_of!(self.data_gen) as u32
    }

    /// Byte offsets the compiler needs, as absolute addresses in linear memory.
    pub fn chain_base(&self) -> u32 {
        self.chain.as_ptr() as u32
    }

    pub fn ras_base(&self) -> u32 {
        self.chain_ras.as_ptr() as u32
    }

    pub fn ras_sp_addr(&self) -> u32 {
        core::ptr::addr_of!(self.ras_sp) as u32
    }

    /// Does physical page `ppn` (= paddr >> 12) hold compiled blocks?
    pub fn is_code_page(&self, ppn: u64) -> bool {
        self.code_pages.contains(&ppn)
    }

    /// A store landed on a code page: real self-modifying code, so the next
    /// `fence.i` must discard rather than skip.
    pub fn note_code_write(&mut self) {
        self.smc_dirty = true;
    }

    pub fn tlb_r_base(&self) -> u32 {
        self.tlb_r.as_ptr() as u32
    }

    pub fn tlb_w_base(&self) -> u32 {
        self.tlb_w.as_ptr() as u32
    }

    /// Record a resolved translation so the next access to this page is a plain
    /// load from linear memory rather than a call into the host.
    ///
    /// Silently does nothing for MMIO, which has no linear-memory address and
    /// must keep reaching the bus.
    pub fn fill_tlb(&mut self, vaddr: u64, host_page: u32, store: bool) {
        let vpn = vaddr >> 12;
        let i = (vpn as usize) & (TLB_ENTRIES - 1);
        let e = TlbEntry {
            vpn,
            gen: self.data_gen,
            _pad: 0,
            base: host_page as u64,
            _pad2: 0,
        };
        if store {
            self.tlb_w[i] = e;
        } else {
            self.tlb_r[i] = e;
        }
    }

    pub fn chain_gen_addr(&self) -> u32 {
        core::ptr::addr_of!(self.chain_gen) as u32
    }

    /// Look up a compiled block by virtual PC, without translating.
    ///
    /// Falls back to the physical path on a miss and caches the result. Returns
    /// `None` when there is no compiled block, which is also what happens after
    /// any event that bumped `trans_gen`.
    pub fn lookup(&mut self, cpu: &mut Supervisor, bus: &mut dyn Bus) -> Option<Hit> {
        let key = cpu.cpu.pc ^ ((cpu.priv_level as u64) << 62);
        let i = ((key >> 1) as usize) & (VVCACHE - 1);
        let v = self.vcache[i];
        if v.key == key && v.gen == cpu.trans_gen {
            self.hits += 1;
            self.hit_insns += v.hit.insns as u64;
            return Some(v.hit);
        }
        let hit = self.on_block_start(cpu, bus)?;
        self.hits += 1;
        self.hit_insns += hit.insns as u64;
        self.vcache[i] = VSlot { key, gen: cpu.trans_gen, hit };
        // Record it where generated code can find it, so the next jump here
        // tail-calls instead of coming back through Rust.
        let c = (cpu.cpu.pc >> 1) as usize & (CHAIN_ENTRIES - 1);
        self.chain[c] = ChainEntry {
            key: cpu.cpu.pc,
            gen: self.chain_gen,
            idx: hit.idx,
        };
        // Both keys just inserted live on this virtual page; a single-page
        // sfence naming it must take the conservative path.
        self.note_key_page(cpu.cpu.pc, cpu.trans_gen);
        Some(hit)
    }

    /// Drop everything. `fence.i` means the guest rewrote instruction memory,
    /// so every decode and every compiled block derived from it is stale.
    ///
    /// Blocks already installed in the host table are left in place — wasm
    /// cannot un-instantiate a module — but nothing will reach them again,
    /// because the slots that named them are gone.
    pub fn flush(&mut self) {
        self.flushes += 1;
        for s in self.slots.iter_mut() {
            *s = Slot { tag: u64::MAX, state: State::Cold(0) };
        }
        self.pending.clear();
        for v in self.vcache.iter_mut() {
            v.gen = u32::MAX;
        }
        for e in self.tlb_r.iter_mut().chain(self.tlb_w.iter_mut()) {
            *e = TlbEntry::EMPTY;
        }
        // The chain table above all: it is the one generated code reads on its
        // own, so an entry surviving a discard points at a function-table slot
        // the host has since emptied, and the tail call lands on null.
        for c in self.chain.iter_mut() {
            c.gen = 0;
            c.key = 0;
        }
        // The RAS caches block indices too; a discard empties the table it
        // points into, so it must be voided alongside the chain.
        for c in self.chain_ras.iter_mut() {
            c.gen = 0;
            c.key = 0;
        }
        self.ras_sp = 0;
        // No blocks left, so no code pages, and the code-write flag resets.
        self.code_pages.clear();
        self.smc_dirty = false;
        // The count has to go back to zero with everything else. The host reads
        // it as "blocks currently held" to decide when to discard; leaving it
        // cumulative means the threshold, once crossed, is crossed forever and
        // the host discards on every call -- which turns the JIT off while
        // still paying for it.
        self.installed = 0;
    }

    /// The interpreter has arrived at a block entry. Returns the block to run,
    /// or `None` to keep interpreting.
    pub fn on_block_start(
        &mut self,
        cpu: &mut Supervisor,
        bus: &mut dyn Bus,
    ) -> Option<Hit> {
        self.entries += 1;

        // A block is identified by its *physical* address, so the PC has to be
        // translated. Doing so can itself fault, in which case there is no
        // block here and the interpreter will take the trap on its own.
        let paddr = cpu.debug_translate(bus, riscv_supervisor::AccessType::Instruction, cpu.cpu.pc).ok()?;

        let i = Self::index(paddr);
        let slot = self.slots[i];
        if slot.tag != paddr {
            // Miss, or a collision evicting whatever was here. Which of those
            // it is decides whether cold code is genuinely cold or is being
            // held below the threshold by an aliasing neighbour.
            self.slot_state[if slot.tag == u64::MAX {
                0
            } else {
                match slot.state {
                    State::Cold(1) => 1,
                    State::Cold(_) => 2,
                    _ => 3,
                }
            }] += 1;
            self.slots[i] = Slot { tag: paddr, state: State::Cold(1) };
            return None;
        }

        match slot.state {
            State::Compiled { idx, bytes, insns, branchy } => {
                Some(Hit { idx, bytes, insns, branchy })
            }
            State::Queued | State::Rejected => None,
            State::Cold(n) if n + 1 < HOT => {
                self.slots[i].state = State::Cold(n + 1);
                None
            }
            State::Cold(_) => {
                // Hot enough. Decode forward and queue it.
                let run = self.decode_run(cpu, bus);
                if run.is_empty() {
                    self.slots[i].state = State::Rejected;
                    self.rejected += 1;
                } else {
                    self.slots[i].state = State::Queued;
                    self.formed += 1;
                    self.pending.push((paddr, run));
                }
                None
            }
        }
    }

    /// Decode forward from the current PC to the first instruction the compiler
    /// cannot handle.
    ///
    /// Stops at the page boundary as well. A run may not span pages: the next
    /// page is a separate translation that could fault or map somewhere else
    /// entirely, and the block is keyed on the physical address of its first
    /// instruction only.
    /// Trace the hot path forward from the current PC.
    ///
    /// Follows jumps and the likely side of branches rather than stopping at the
    /// first control transfer, so a trace covers a whole loop body instead of
    /// one basic block. Stops at an indirect jump, an uncompilable instruction,
    /// the page boundary, an address already traced, or the cap.
    fn decode_run(&self, cpu: &mut Supervisor, bus: &mut dyn Bus) -> Vec<Src> {
        use riscv_core::types::Instr::*;

        let mut out: Vec<Src> = Vec::new();
        let start = cpu.cpu.pc;
        let mut pc = start;
        // Offsets already traced, so a loop closes instead of unrolling
        // forever. Small and linear-scanned: traces are tens of entries.
        let mut seen: Vec<i32> = Vec::new();

        while out.len() < MAX_RUN {
            // One page, one translation. See the note on the module above.
            if (pc >> 12) != (start >> 12) {
                break;
            }
            let off = pc.wrapping_sub(start) as i32;
            if seen.contains(&off) {
                break;
            }
            seen.push(off);

            let Ok((_paddr, half, width, raw)) = cpu.debug_fetch(bus, pc) else {
                break;
            };
            let instr = if width == 2 {
                match riscv_core::compressed::decompress(half) {
                    Some(i) => i,
                    None => break,
                }
            } else {
                riscv_core::decode::decode(raw)
            };

            // An instruction may start in this page and end in the next: with
            // the C extension a 32-bit encoding needs only 2-byte alignment.
            if ((pc + width as u64 - 1) >> 12) != (start >> 12) {
                break;
            }

            if is_compilable(&instr) {
                out.push((instr, width, off));
                // A CSR ends the trace even though it is compilable. It may
                // rewrite satp and change which physical page this virtual PC
                // maps to — and the instructions already decoded past it were
                // fetched under the OLD mapping, so continuing the block would
                // run stale code. Ending here lets the next block be fetched
                // fresh: the CSR host call refreshes the generation word, so a
                // translation change de-links the chain and returns to the run
                // loop, while an ordinary CSR (which changes no mapping) still
                // chains straight through to its successor.
                //
                // Found the hard way — compiling CSR mid-block left every cold
                // boot spinning just before "Mounting boot media", because the
                // first satp write that turns on paging landed mid-trace.
                if matches!(
                    instr,
                    Csrrw { .. } | Csrrs { .. } | Csrrc { .. }
                        | Csrrwi { .. } | Csrrsi { .. } | Csrrci { .. }
                ) {
                    break;
                }
                pc = pc.wrapping_add(width as u64);
                continue;
            }
            if !riscv_jit::is_terminator(&instr) {
                break;
            }

            out.push((instr, width, off));

            // Where the trace goes next.
            let next = match instr {
                // Unconditional and direct: just follow it, no guard needed.
                Jal { imm, .. } => (pc as i64).wrapping_add(imm) as u64,
                // Indirect: the target is a register, unknowable here.
                Jalr { .. } => break,
                // A backward branch is a loop and is taken nearly always; a
                // forward one is usually a skip. Guess accordingly -- a wrong
                // guess costs a side exit, not correctness.
                Beq { imm, .. } | Bne { imm, .. } | Blt { imm, .. } | Bge { imm, .. }
                | Bltu { imm, .. } | Bgeu { imm, .. } => {
                    if imm < 0 {
                        (pc as i64).wrapping_add(imm) as u64
                    } else {
                        pc.wrapping_add(width as u64)
                    }
                }
                _ => break,
            };
            pc = next;
        }
        out
    }

    /// Blocks waiting to be compiled. Taking them transfers ownership; the
    /// caller must follow with `installed` for the same count, in order.
    pub fn take_pending(&mut self) -> Vec<(u64, Vec<Src>)> {
        core::mem::take(&mut self.pending)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Record that the host installed `paddrs` at consecutive table slots
    /// beginning at `table_base`.
    ///
    /// Index and table slot are kept one-to-one, which is why a run that failed
    /// to compile still consumes a slot on the compiler side: an index shift
    /// would silently run some other block's code.
    pub fn installed(
        &mut self,
        paddrs: &[u64],
        bytes: &[u32],
        insns: &[u32],
        branchy: &[bool],
        table_base: u32,
    ) {
        debug_assert_eq!(paddrs.len(), bytes.len());
        debug_assert_eq!(paddrs.len(), insns.len());
        debug_assert_eq!(paddrs.len(), branchy.len());
        let mut new_code_page = false;
        for (k, paddr) in paddrs.iter().enumerate() {
            let i = Self::index(*paddr);
            // The slot may have been evicted by a collision while the batch was
            // in flight. Only claim it if it is still ours and still queued.
            if self.slots[i].tag == *paddr && self.slots[i].state == State::Queued {
                self.slots[i].state = State::Compiled {
                    idx: table_base + k as u32,
                    bytes: bytes[k],
                    insns: insns[k],
                    branchy: branchy[k],
                };
            }
            // Record the physical page as code, for the selective fence.i flush.
            new_code_page |= self.code_pages.insert(*paddr >> 12);
            self.installed += 1;
        }
        // If a page just became code, any WRITE-TLB entry that cached it (from
        // when it was data) would let a store bypass the code-write check. Drop
        // the write TLB so stores to it re-resolve through the host and are
        // seen. Only the write side, and only when a page newly turned to code
        // -- after the working set stabilises this stops firing.
        if new_code_page {
            for e in self.tlb_w.iter_mut() {
                e.gen = 0;
            }
        }
    }

    pub fn installed_count(&self) -> u32 {
        self.installed
    }
}

/// Statistics for the formation probe, which reports what the real block former
/// sees rather than what an idealised subset scan predicts.
pub struct Stats {
    pub entries: u64,
    pub formed: u64,
    pub rejected: u64,
    pub total_insns: usize,
    pub max_len: usize,
}

impl Jit {
    pub fn stats(&self, pending_seen: &[(u64, Vec<Src>)]) -> Stats {
        Stats {
            entries: self.entries,
            formed: self.formed,
            rejected: self.rejected,
            total_insns: pending_seen.iter().map(|(_, v)| v.len()).sum(),
            max_len: pending_seen.iter().map(|(_, v)| v.len()).max().unwrap_or(0),
        }
    }
}

/// Re-exported so callers do not need a direct dependency on the compiler.
pub fn compilable(i: &Instr) -> bool {
    is_compilable(i)
}
