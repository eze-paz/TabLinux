//! Portable machine assembly: kernel + initrd + devicetree, no host services.
//!
//! `riscv-vm` shells out to `python3` to build the devicetree and to `stty` for
//! the terminal, neither of which exists in a browser. This crate does the same
//! boot with nothing but bytes handed in by the caller, so it builds for
//! wasm32-unknown-unknown as-is.
//!
//! The devicetree is passed in pre-built. Only the initrd addresses have to be
//! patched at load time, since where the initramfs lands depends on its size,
//! and `fdt` below does that in place.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::vec::Vec;
use riscv_core::execute::Bus as _;
use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{types::Privilege, Supervisor};

pub mod fdt;
pub mod jit;

pub const DRAM_BASE: u64 = 0x8000_0000;
/// Selective `fence.i`: discard compiled blocks only when the guest has actually
/// stored to a code page since the last flush (`Jit::smc_dirty`). A `fence.i`
/// with no intervening code-page write is spurious -- musl's dynamic linker and
/// the kernel issue it defensively -- and the whole hot cache is still valid, so
/// the discard-all is pure re-warming waste. Measured: with the old always-flush,
/// CPython ran 799 discards and 51.3M cold-interpreted instructions; skipping the
/// spurious ones is byte-identical output and drops the cold tail ~70%.
///
/// Correctness: a COMPILED store to a code page sets `smc_dirty` (see the JIT
/// store shim), so any block whose page was written is still discarded at the
/// next `fence.i`. Code pages are dropped from the write TLB when they form, so
/// such a store cannot bypass the check through a stale write-TLB entry, and the
/// RISC-V spec requires `fence.i` before executing modified code, so this
/// invalidates exactly when it must. LIMITATION: a store executed by the
/// INTERPRETER (not compiled code) is not yet flagged, so a guest that both
/// self-modifies code AND does so from cold/interpreted paths would need this
/// off. Normal software -- CPython, shells, Linux userspace -- never writes the
/// pages it executes, so this does not arise; `false` restores the
/// unconditional discard for anything that does.
const SELECTIVE_FENCEI: bool = true;

pub struct Machine {
    pub bus: DeviceBus,
    /// Block formation and hot-block selection. Inert unless `jit_enabled`.
    pub jit: crate::jit::Jit,
    /// Where the host installed the first compiled block in its function table.
    pub jit_table_base: u32,
    pub jit_enabled: bool,
    /// Guest PC of the access that faulted inside a compiled block.
    pub jit_fault_pc: u64,
    /// Instructions retired by compiled code, counted by the blocks themselves.
    /// Rust cannot count tail-called successors any other way.
    pub jit_chain_insns: u64,
    /// Times the host entered compiled code, i.e. chains rather than blocks.
    pub jit_chains: u64,
    /// A chain just ended with no compiled block at the next PC. The next
    /// interpreted instruction is the obstacle; the one after it is worth
    /// offering to the JIT.
    jit_retry: bool,
    /// Last `Supervisor::icache_gen` the JIT was flushed for. See `run`.
    jit_icache_gen: u32,
    pub cpu: Supervisor,
    /// Everything the guest has written to the UART, as raw bytes.
    pub console: Vec<u8>,
    console_taken: usize,
    sbi_taken: usize,
    pub steps: u64,
    /// Diagnostic: interpreted instructions binned by why they landed there.
    /// Only filled while `interp_hist_on`.
    pub interp_hist: [u64; 10],
    /// Fine-grained opcode-category histogram of every INTERPRETED instruction
    /// (all of them when the JIT is off). Answers "which instruction class does
    /// the guest actually execute most" — the compiled path has no per-opcode
    /// counter, but the interpreter sees the same instruction stream. See
    /// `op_bin` / `OP_HIST_BINS`. Filled while `interp_hist_on`.
    pub op_hist: [u64; 24],
    /// Whether to fill `interp_hist`. Off by default: doing so costs a second
    /// translate, fetch and decode of every interpreted instruction.
    pub interp_hist_on: bool,
    /// Diagnostic: why the compiled tail-call probe missed, and why chains
    /// ended. See `CHAIN_MISS_BINS`. Filled while `interp_hist_on`.
    pub chain_miss: [u64; 9],
    /// Diagnostic: why an inlined-TLB probe missed and fell to the host. See
    /// `TLB_MISS_BINS`. Filled while `interp_hist_on`.
    pub tlb_miss: [u64; 9],
    /// Host monotonic time in nanoseconds, set by the caller before `run`.
    ///
    /// Zero means the caller does not supply one, and then idle skipping keeps
    /// its historical unbounded behaviour -- which every native test and
    /// benchmark depends on, so they are unaffected by any of this.
    ///
    /// With a host clock, the guest clock is held to real time. Without one the
    /// emulator cannot tell that it is racing: it parks on WFI, jumps mtime to
    /// the next deadline, and nothing bounds how OFTEN it jumps, so an idle
    /// guest ran its clock ~117x real time and then paid to emulate every timer
    /// tick it had invented. `sleep 3` returned in 0.31s.
    pub host_ns: u64,
    /// `host_ns` when idle time was last granted, so the allowance is the real
    /// time that passed rather than a fixed constant.
    last_idle_host_ns: u64,
    /// Diagnostic: how long the guest asked to wait each time it parked on WFI,
    /// binned by duration. See `IDLE_WAIT_BINS`.
    ///
    /// The shape decides where boot's idle time comes from. Many short waits
    /// mean device completion latency; a few long ones mean the guest is
    /// sitting on a timeout. Those want opposite fixes, and guessing which it
    /// is has a poor record.
    pub idle_waits: [u64; 7],
    /// Total guest time skipped while idle, in mtime ticks.
    pub idle_skipped: u64,
    /// Guest mtime the caller may sleep until, or 0 if the guest is not waiting.
    /// Set when `run` returns early because the guest has nothing to do.
    pub idle_until: u64,
    /// Instructions a chain may retire before returning to the run loop.
    ///
    /// The run loop is the only place interrupts are checked, so this is the
    /// guest's worst-case interrupt latency measured in instructions, and it
    /// trades that latency against the per-boundary cost of leaving compiled
    /// code. Runtime rather than a constant so it can be swept; read once per
    /// chain, not per instruction, so it is not on the hot path.
    ///
    /// Default 8192. A sweep across CPU-bound guest workloads (sha256, awk,
    /// shell arithmetic, gzip) measured 1.1-1.5x more JIT throughput going from
    /// 1024 to 8192 as host round-trips dropped; the remaining gain to 65536
    /// was small. At ~250 MIPS 8192 instructions is ~33us of worst-case
    /// interrupt latency, far under a 1ms timer tick. A gen-changing
    /// instruction (satp/sfence/fence.i) is never compiled, so it already ends
    /// a chain -- a longer chain can never skip an invalidation.
    pub chain_max: u64,

    /// Retired instructions by privilege (index = `Privilege as usize`: U=0,
    /// S=1, M=3). Chains attribute exactly — the chain generation encodes the
    /// privilege, so a chain never spans a switch. A profiler counter, not
    /// machine state: deliberately absent from the snapshot.
    pub prof_insns: [u64; 4],
    /// Instructions attributed to the virtual-clock idle cycle (short spans
    /// between WFI parks — see the classification in `run`). Profiler only.
    pub prof_idle_insns: u64,
    /// Total retired instructions at the previous WFI park.
    prof_idle_mark: u64,
    /// A device interrupt (not the timer) was pending at the previous WFI
    /// park, i.e. the span that follows it is servicing I/O, not idling.
    prof_wake_device: bool,
    /// Parks by wake source: [timer-or-nothing, device]. Diagnostic for the
    /// classifier itself.
    pub prof_parks: [u64; 2],
    /// Opt-in for the deep-idle handback in `run` (the browser worker sets it;
    /// native callers keep historical run-to-budget behaviour).
    pub idle_handback: bool,
    /// A realtime-paced wait is in progress: `last_idle_host_ns` was anchored
    /// when it began, and drops when its deadline is reached. Without this
    /// the allowance banked every busy second as idle credit.
    idle_pacing: bool,
    /// The current park has already been reported once, so this entry must
    /// fall through and jump. One-shot per park; see `run`.
    idle_reported: bool,
}

/// Bin names for `Machine::interp_hist`, in order.
pub const INTERP_BINS: [&str; 10] = [
    "mul", "div/rem", "atomic", "csr", "fence", "system", "fp",
    "cold (compilable)", "other", "fence.i",
];

/// Bin names for `Machine::op_hist`, in order. Kept parallel to `op_bin`.
pub const OP_HIST_BINS: [&str; 24] = [
    "load8", "load16", "load32", "load64",   // 0..4
    "store8", "store16", "store32", "store64", // 4..8
    "branch", "jal", "jalr",                 // 8..11
    "addi", "add/sub", "logic-reg", "logic-imm", "shift", "slt", // 11..17
    "lui", "auipc", "word-op",               // 17..20
    "mul", "div/rem", "fp", "other",         // 20..24
];

/// Classify an instruction into an `op_hist` bin: a finer split than
/// `interp_bin`, aimed at "which instruction class dominates execution".
fn op_bin(i: &riscv_core::types::Instr) -> usize {
    use riscv_core::types::Instr::*;
    match i {
        Lb { .. } | Lbu { .. } => 0,
        Lh { .. } | Lhu { .. } => 1,
        Lw { .. } | Lwu { .. } => 2,
        Ld { .. } => 3,
        Sb { .. } => 4,
        Sh { .. } => 5,
        Sw { .. } => 6,
        Sd { .. } => 7,
        Beq { .. } | Bne { .. } | Blt { .. } | Bge { .. } | Bltu { .. } | Bgeu { .. } => 8,
        Jal { .. } => 9,
        Jalr { .. } => 10,
        Addi { .. } => 11,
        Add { .. } | Sub { .. } => 12,
        And { .. } | Or { .. } | Xor { .. } => 13,
        Andi { .. } | Ori { .. } | Xori { .. } => 14,
        Sll { .. } | Srl { .. } | Sra { .. } | Slli { .. } | Srli { .. } | Srai { .. } => 15,
        Slt { .. } | Sltu { .. } | Slti { .. } | Sltiu { .. } => 16,
        Lui { .. } => 17,
        Auipc { .. } => 18,
        Addiw { .. } | Slliw { .. } | Srliw { .. } | Sraiw { .. }
        | Addw { .. } | Subw { .. } | Sllw { .. } | Srlw { .. } | Sraw { .. } => 19,
        Mul { .. } | Mulh { .. } | Mulhsu { .. } | Mulhu { .. } | Mulw { .. } => 20,
        Div { .. } | Divu { .. } | Rem { .. } | Remu { .. }
        | Divw { .. } | Divuw { .. } | Remw { .. } | Remuw { .. } => 21,
        Fp { .. } | Flw { .. } | Fld { .. } | Fsw { .. } | Fsd { .. } => 22,
        _ => 23,
    }
}

/// Bin names for `Machine::idle_waits`, in order. A 10 MHz timebase, so these
/// are guest-time durations.
pub const IDLE_WAIT_BINS: [&str; 7] = [
    "< 10us", "10-100us", "100us-1ms", "1-5ms", "5-15ms", "15-100ms", "> 100ms",
];

/// Bin names for `Machine::chain_miss`, in order.
///
/// Bins 0..=5 are an exclusive taxonomy of **tail-call probe misses**: a
/// compiled block reached its end, probed the chain table for its successor,
/// and returned to the host instead of tail-calling. Bin 6 is a *sub-count* of
/// those, not a seventh category. Bins 7 and 8 count chain *ends*, which are a
/// different event with a different denominator: a chain that ended on a fault
/// never reached a probe at all.
///
/// The question this exists to answer. Entries die either because something
/// overwrote the slot (0, 1) or because the generation word moved (2, 3, 4).
/// `chain_gen` is *derived* -- `1 + ((trans_gen << 2) | priv)` -- rather than
/// bumped, so returning to a previous privilege level revalidates every entry
/// stamped under it. That makes "syscalls invalidate the chain table" a claim
/// to check rather than assume: a satp write moving `trans_gen` (bin 3) and
/// plain eviction (bin 1) are the competing explanations, and they point at
/// different fixes -- ASIDs for the first, table geometry for the second.
pub const CHAIN_MISS_BINS: [&str; 9] = [
    "probe: slot empty",
    "probe: evicted",
    "probe: gen priv",
    "probe: gen trans",
    "probe: gen both",
    "probe: valid, wasm budget",
    "  of those: no block",
    "end: fault",
    "end: cap",
];

/// Bin names for `Machine::tlb_miss`, in order.
///
/// Every compiled load and store first probes an inlined TLB in linear memory,
/// and falls back to a host call when that misses. The probe tests three
/// things -- the virtual page, the generation word, and that the access fits
/// inside the page -- so a miss is attributable, and the attribution picks the
/// fix.
///
/// The generation bins are the interesting ones, because the inline TLB stamps
/// its entries with `chain_gen`, which folds in the PRIVILEGE LEVEL. Unlike the
/// chain table -- where privilege was measured at exactly 0% of misses, since a
/// privilege change traps out through uncompilable code and no compiled block
/// ever probes across the boundary -- inline TLB entries are expected to
/// survive across traps and returns, and stamping them with privilege means
/// they do not. If bin 2 is large, every trap is throwing away the guest's
/// entire data translation cache, and the fix is to stamp entries with a word
/// that does not carry privilege.
pub const TLB_MISS_BINS: [&str; 9] = [
    "slot empty",
    "evicted",
    "gen: priv",
    "gen: trans",
    "gen: both",
    "valid, crosses page",
    "valid, fits (unexpected)",
    "MMIO (never cached)",
    "translation faulted",
];

/// Which bin an interpreted instruction belongs to.
fn interp_bin(i: &riscv_core::types::Instr) -> usize {
    use riscv_core::types::Instr::*;
    match i {
        Mul { .. } | Mulh { .. } | Mulhsu { .. } | Mulhu { .. } | Mulw { .. } => 0,
        Div { .. } | Divu { .. } | Rem { .. } | Remu { .. }
        | Divw { .. } | Divuw { .. } | Remw { .. } | Remuw { .. } => 1,
        Lrw { .. } | Scw { .. } | Lrd { .. } | Scd { .. }
        | Amoswapw { .. } | Amoaddw { .. } | Amoxorw { .. } | Amoandw { .. }
        | Amoorw { .. } | Amominw { .. } | Amomaxw { .. } | Amominuw { .. }
        | Amomaxuw { .. }
        | Amoswapd { .. } | Amoaddd { .. } | Amoxord { .. } | Amoandd { .. }
        | Amoord { .. } | Amomind { .. } | Amomaxd { .. } | Amominud { .. }
        | Amomaxud { .. } => 2,
        Csrrw { .. } | Csrrs { .. } | Csrrc { .. }
        | Csrrwi { .. } | Csrrsi { .. } | Csrrci { .. } => 3,
        Fence { .. } => 4,
        // Split out: fence is a no-op we can compile, fence.i invalidates
        // compiled code and must always reach the host. Very different levers.
        FenceI => 9,
        Ecall | Ebreak | Mret | Sret | Uret | Wfi | SfenceVma { .. } | Unimp => 5,
        Fp { .. } => 6,
        // Compilable, so it is here only because its block never got hot.
        // Codegen cannot touch this bin; block selection can.
        other if riscv_jit::is_compilable(other) => 7,
        _ => 8,
    }
}

pub struct BootImages<'a> {
    /// Flat RISC-V Image (already decompressed).
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    /// Devicetree blob; its initrd addresses are patched to match.
    pub dtb: &'a [u8],
    pub dram_bytes: usize,
}

impl Machine {
    pub fn new(img: BootImages<'_>) -> Self {
        let text_offset = u64::from_le_bytes(img.kernel[0x08..0x10].try_into().unwrap());
        let kernel_load = DRAM_BASE + text_offset;

        let mut bus = DeviceBus::new(img.dram_bytes);
        bus.load_blob(kernel_load, img.kernel);

        // Initrd near the top of RAM, 64K aligned, clear of the kernel's
        // runtime footprint.
        let initrd_load = (DRAM_BASE + img.dram_bytes as u64
            - img.initrd.len() as u64
            - 0x100_0000)
            & !0xFFFFu64;
        bus.load_blob(initrd_load, img.initrd);
        let initrd_end = initrd_load + img.initrd.len() as u64;

        let mut dtb = img.dtb.to_vec();
        // Before anything else: the blob ships with the size it was generated
        // at, and a kernel told it has more RAM than exists hangs during early
        // reservations with no useful message. See fdt::patch_memory.
        fdt::patch_memory(&mut dtb, DRAM_BASE, img.dram_bytes as u64);
        fdt::patch_initrd(&mut dtb, initrd_load, initrd_end);
        let dtb_load = (initrd_load - dtb.len() as u64 - 0x1000) & !0xFFFu64;
        bus.load_blob(dtb_load, &dtb);

        let mut cpu = Supervisor::new(kernel_load, 0);
        cpu.priv_level = Privilege::Supervisor;
        cpu.cpu.write_reg(10, 0); // a0 = hartid
        cpu.cpu.write_reg(11, dtb_load); // a1 = devicetree
        cpu.cpu.write_reg(2, DRAM_BASE + img.dram_bytes as u64 - 0x10000); // sp
        cpu.medeleg = 0xB1FF;
        cpu.mideleg = 0x2A2;

        Self {
            bus,
            jit: crate::jit::Jit::new(),
            jit_table_base: 0,
            jit_enabled: false,
            jit_fault_pc: 0,
            jit_chain_insns: 0,
            jit_chains: 0,
            jit_retry: false,
            jit_icache_gen: 0,
            cpu,
            console: Vec::new(),
            console_taken: 0,
            sbi_taken: 0,
            steps: 0,
            interp_hist: [0; 10],
            op_hist: [0; 24],
            interp_hist_on: false,
            chain_miss: [0; 9],
            tlb_miss: [0; 9],
            host_ns: 0,
            last_idle_host_ns: 0,
            idle_waits: [0; 7],
            idle_skipped: 0,
            idle_until: 0,
            chain_max: 8192,
            prof_insns: [0; 4],
            prof_idle_insns: 0,
            prof_idle_mark: 0,
            prof_wake_device: false,
            prof_parks: [0; 2],
            idle_handback: false,
            idle_pacing: false,
            idle_reported: false,
        }
    }

    /// Enable or disable the deep-idle handback at runtime. The browser turns
    /// it OFF while network traffic is flowing: a guest waiting on a TCP
    /// segment parks with its next deadline being its own protocol TIMEOUT
    /// (apk's 10s fetch timer, the resolver's 5s), and the handback's
    /// uncapped re-entry jump teleports the clock straight onto it — "DNS:
    /// transient error" and "Operation timed out" mid-download, even though
    /// the data was milliseconds away. With the handback off, waits advance
    /// in MAX_IDLE_SKIP hops per spin iteration, the pre-handback behavior
    /// under which real replies always won the race against guest timeouts.
    pub fn set_idle_handback(&mut self, on: bool) {
        self.idle_handback = on;
        if !on {
            // A park reported but not yet jumped must not take the uncapped
            // path on its re-entry.
            self.idle_reported = false;
        }
    }

    /// Run up to `budget` instructions. Returns the number actually executed —
    /// short only if the hart parked in WFI with no deadline, which cannot
    /// happen once Linux has armed its tick.
    pub fn run(&mut self, budget: u64) -> u64 {
        /// Shortest guest wait worth handing back to the host, in mtime ticks.
        /// 5ms at the devicetree's 10 MHz timebase -- comfortably above a
        /// browser's setTimeout granularity, comfortably below the 10ms kernel
        /// tick that governs a genuinely idle guest.
        const SHORT_WAIT: u64 = 50_000;

        let mut n = 0;
        self.idle_until = 0;
        while n < budget {
            self.bus.tick();

            // The guest rewrote instruction memory. Compiled blocks are keyed
            // on physical address, so nothing else invalidates them — not a
            // satp write, not an sfence, which is the point — and without this
            // they outlive the code they were compiled from.
            //
            // Linux does exactly that during early boot: alternatives patching
            // and static keys rewrite kernel text in place and then fence.i.
            // The JIT went on running the pre-patch version and the boot
            // stopped dead, every time, at
            //
            //     Mountpoint-cache hash table entries: ...
            //
            // Only cold boots were affected, which is why it survived so long:
            // the snapshot resumes long after the last patch, and both boot
            // harnesses restore that snapshot rather than booting.
            //
            // Safe to do here rather than inside the chain: fence.i is not
            // compilable, so it always ends a block and is executed by the
            // interpreter — it can never fire between two chained blocks.
            // A single-page sfence.vma named one page rather than the whole
            // address space. Retire just that page's cached translation; the
            // rest of the inlined TLB stays live, which is the point.
            #[cfg(target_arch = "wasm32")]
            if self.cpu.pending_flush_n != 0 {
                // Per-page data-TLB invalidation, plus the chain decision the
                // supervisor deferred: a single-page sfence no longer bumps
                // `trans_gen` itself. Only a page that might hold a chain or
                // vcache key forces the old wholesale invalidation — nearly
                // all of these name data pages (allocator munmaps), and each
                // needless bump cost ~760 chain stops. Safe to decide here:
                // sfence is uncompilable, so control always passes through
                // this loop before any compiled code can probe a chain entry,
                // and `refresh_gen` below runs after this block.
                let mut chain_hit = false;
                for i in 0..self.cpu.pending_flush_n as usize {
                    let va = self.cpu.pending_flush[i];
                    self.jit.invalidate_tlb_page(va);
                    chain_hit |= self.jit.page_may_have_keys(va, self.cpu.trans_gen);
                }
                self.cpu.pending_flush_n = 0;
                if chain_hit {
                    self.jit.sfence_hits += 1;
                    // Drop only the current space's chain entries, with a fresh
                    // unique generation so it cannot alias another space's.
                    self.cpu.advance_chain_gen();
                } else {
                    self.jit.sfence_skips += 1;
                }
            }

            #[cfg(target_arch = "wasm32")]
            if self.cpu.icache_gen != self.jit_icache_gen {
                self.jit_icache_gen = self.cpu.icache_gen;
                // Selective: discard only if a code page was actually written.
                if !SELECTIVE_FENCEI || self.jit.smc_dirty {
                    self.jit.flush();
                }
            }

            // A compiled block covers a run of instructions in one call. Only
            // consulted at block starts -- where the PC arrived rather than
            // fell through -- so this costs nothing on the straight-line path.
            #[cfg(target_arch = "wasm32")]
            if self.jit_enabled && self.cpu.block_start
                && !self.cpu.interrupt_pending(&mut self.bus)
            {
                // Interrupts are delivered by the interpreter's `step`, and
                // compiled code never runs it. Checking here — at every chain
                // boundary — is what lets a timer or external interrupt break
                // into a purely compiled loop. Without it, a compiled
                // `csrr sstatus`/branch idle-wait spins forever: it was only
                // ever interrupted before because the CSR forced an interpreter
                // step, and compiling CSRs removed that accident. When one is
                // pending, fall through to `step` below, which takes the trap.
                //
                // Generated code compares chain entries against this word.
                // Recomputed here rather than hooked into every satp write and
                // trap: it is one shift and an or, and this is the only place
                // compiled code is entered.
                self.jit.refresh_gen(&self.cpu);
                if let Some(hit) = self.jit.lookup(&mut self.cpu, &mut self.bus) {
                    let pl = self.cpu.priv_level as usize & 3;
                    let executed = self.run_chain(hit, budget - n);
                    self.prof_insns[pl] += executed;
                    // The loop already ticked once for this iteration; credit
                    // the rest of the chain so emulated time tracks retired
                    // instructions rather than run-loop iterations.
                    self.bus.tick_n(executed.saturating_sub(1));
                    n += executed;
                    continue;
                }
            }

            #[cfg(target_arch = "wasm32")]
            let retry = core::mem::take(&mut self.jit_retry);

            // Off unless something asked for it. Binning an interpreted
            // instruction means translating, fetching and decoding it a second
            // time purely to look at it — affordable for a measurement run,
            // and pure waste on every other one. It shipped switched on, which
            // is exactly the mistake the comment on `interp_hist` warns about.
            // Not gated on jit_enabled: with the JIT off the interpreter runs
            // every instruction, so this becomes a full opcode histogram; with
            // it on, only the fallbacks reach here, which is the old behaviour.
            #[cfg(target_arch = "wasm32")]
            if self.interp_hist_on {
                let pc = self.cpu.cpu.pc;
                if let Ok((_p, half, width, raw)) = self.cpu.debug_fetch(&mut self.bus, pc) {
                    let decoded = if width == 2 {
                        riscv_core::compressed::decompress(half)
                    } else {
                        Some(riscv_core::decode::decode(raw))
                    };
                    if let Some(d) = decoded {
                        self.interp_hist[interp_bin(&d)] += 1;
                        self.op_hist[op_bin(&d)] += 1;
                    }
                }
            }

            // Attributed before the step: a trap inside it changes the level,
            // and the instruction that trapped belongs to where it ran.
            self.prof_insns[self.cpu.priv_level as usize & 3] += 1;
            if let Status::Wfi = self.cpu.step(&mut self.bus) {
                // The guest has gone idle with a frame waiting for the host, so
                // hand control back now rather than skipping the clock forward
                // while the answer sits unsent. The host side is asynchronous
                // everywhere this runs — a JS event loop in the browser, the
                // caller's loop natively — and neither can respond until we
                // return.
                if self.bus.net.as_ref().is_some_and(|n| !n.borrow().to_host.is_empty()) {
                    self.steps += n;
                    self.drain_console();
                    return n;
                }

                // Deliver any in-flight virtio completion now: nothing advances
                // the device clock while the guest sleeps, so a completion left
                // in its latency window would strand its interrupt and the guest
                // would never wake. Then STILL jump the timer to its next
                // deadline. Doing both matters — firing without advancing time
                // lets ongoing block writeback (submit, complete, submit again)
                // spin against this loop forever, never letting the clock move;
                // advancing as well keeps time progressing so such work drains.
                self.bus.flush_virtio_completions();
                let next = self.cpu.stimecmp.min(self.bus.get_mtimecmp());
                if next != u64::MAX {
                    // 10 MHz timebase: 10_000 ticks is a millisecond.
                    let before = self.bus.diag_mtime();
                    let wait = next.saturating_sub(before);

                    // Idle-loop attribution, for the profiler only. Under the
                    // virtual clock an idle guest spins WFI -> jump -> timer
                    // tick -> WFI, retiring real instructions that mean "there
                    // was nothing to do" — which INVERTS the MIPS reading
                    // (idle looks fast, work looks slow).
                    //
                    // Two conditions, both required. A short SPAN (under 20k
                    // instructions — several tick handlers, well under any
                    // real request) alone is not enough: I/O-bound work also
                    // runs in short bursts between waits, and the first cut of
                    // this classified `ls -laR | md5sum` as 96% idle. The wait
                    // LENGTH cannot separate them either — `next` is always a
                    // timer deadline, because virtio completions were just
                    // delivered by the flush above rather than scheduled ahead.
                    // What does separate them is what WOKE the previous park:
                    // a device interrupt pending right after the flush means
                    // the span that followed was servicing I/O, however short.
                    // Only timer-woken short spans are the idle cycle.
                    // Heuristic, so it feeds a metric, never a decision.
                    {
                        let total = self.steps + n;
                        let span = total - self.prof_idle_mark;
                        if span < 20_000 && !self.prof_wake_device {
                            self.prof_idle_insns += span;
                        }
                        self.prof_idle_mark = total;
                        // What is waking the guest from THIS park, for the
                        // next span's classification. The timer has not fired
                        // yet (the jump below is what reaches it), so anything
                        // already pending is a device. RAW pending, not
                        // deliverability: Linux parks WFI with interrupts
                        // globally disabled (irq_disable; wfi; irq_enable), so
                        // an enables-respecting check reads false at exactly
                        // the parks this wants to classify.
                        self.prof_wake_device = self.bus.check_external_interrupt();
                        self.prof_parks[self.prof_wake_device as usize] += 1;
                    }
                    self.idle_waits[match wait {
                        0..=99 => 0,
                        100..=999 => 1,
                        1_000..=9_999 => 2,
                        10_000..=49_999 => 3,
                        50_000..=149_999 => 4,
                        150_000..=999_999 => 5,
                        _ => 6,
                    }] += 1;
                    if self.host_ns == 0 {
                        // Deep-idle handback, so a browser tab at an idle
                        // prompt can sleep instead of spinning the WFI ->
                        // jump -> tick cycle at full speed. Three conditions:
                        // the caller opted in (native tests and benches keep
                        // historical behaviour); the guest's own deadline is
                        // at least 50ms of guest time away — the kernel is
                        // TICKLESS-idle, which real work never is (work-
                        // adjacent parks are microseconds to one 10ms tick);
                        // and no device interrupt is pending after the flush
                        // above (an I/O completion must be serviced NOW, not
                        // after a host nap). One-shot per park: the re-entry
                        // falls through and jumps exactly as before, so a
                        // caller that never sleeps sees identical behaviour
                        // one pump later, and a livelock is impossible.
                        if self.idle_handback
                            && wait >= 500_000
                            && !self.prof_wake_device
                            && !self.idle_reported
                        {
                            self.idle_reported = true;
                            self.idle_until = next;
                            self.steps += n;
                            self.drain_console();
                            return n;
                        }
                        if core::mem::take(&mut self.idle_reported) {
                            // The host slept on this park; the wait is over.
                            // One uncapped jump — paying MAX_IDLE_SKIP-sized
                            // hops once per host nap ran the guest clock at
                            // ~2% of real time (sleep 5 took minutes).
                            self.bus.idle_skip_mtime_to(next);
                        } else {
                            // No host clock: historical behaviour.
                            self.bus.idle_skip_mtime(next);
                        }
                    } else if wait < SHORT_WAIT {
                        // Too short to be worth handing back. Yielding costs the
                        // host a timer round trip, and a browser clamps a nested
                        // setTimeout to whole milliseconds, so a 0.2ms guest wait
                        // would cost several milliseconds of real time -- which
                        // measured as 31% off an interactive workload, because
                        // pipes and process creation wait like this constantly.
                        //
                        // Skipped the old way, unbounded. The clock error that
                        // reintroduces is bounded by the sum of these short waits
                        // rather than unbounded, and every wait long enough to
                        // matter -- a sleep, or an idle prompt, whose kernel tick
                        // is 10ms -- is above the threshold and still held to
                        // real time.
                        self.bus.idle_skip_mtime(next);
                    } else {
                        // Grant exactly the real time that has passed — SINCE
                        // THIS WAIT BEGAN, not since the last grant ever. The
                        // old anchor banked every busy second as idle credit:
                        // after 30s of computing, the next `sleep 5` had 30s
                        // of allowance and completed instantly (measured 51ms
                        // wall). The anchor now resets when a wait starts and
                        // the pacing flag drops when it completes, so each
                        // wait is paced 1:1 against real time on its own.
                        // The devicetree declares a 10 MHz timebase, so one
                        // tick is 100ns.
                        if !self.idle_pacing {
                            self.idle_pacing = true;
                            self.last_idle_host_ns = self.host_ns;
                        }
                        let allowance =
                            self.host_ns.saturating_sub(self.last_idle_host_ns) / 100;
                        self.last_idle_host_ns = self.host_ns;
                        if !self.bus.idle_skip_mtime_realtime(next, allowance) {
                            // The guest is still waiting on a deadline that real
                            // time has not reached. Hand control back so the host
                            // can sleep instead of spinning: this is what stops a
                            // page at an idle prompt from burning a core.
                            self.idle_until = next;
                            self.idle_skipped += self.bus.diag_mtime().saturating_sub(before);
                            self.steps += n;
                            self.drain_console();
                            return n;
                        }
                        // The wait's deadline was reached: this pacing episode
                        // is over. The next park re-anchors to the host clock.
                        self.idle_pacing = false;
                    }
                    self.idle_skipped += self.bus.diag_mtime().saturating_sub(before);
                }
            }
            #[cfg(target_arch = "wasm32")]
            if retry {
                // The obstacle has been stepped over; what follows is often a
                // compilable run, and without this it would be interpreted to
                // the end of the basic block.
                self.cpu.block_start = true;
            }
            n += 1;
        }
        self.steps += n;
        self.drain_console();
        n
    }

    fn drain_console(&mut self) {
        if self.bus.uart_console.len() > self.console_taken {
            let n = self.bus.uart_console.len();
            self.console.extend_from_slice(&self.bus.uart_console[self.console_taken..n]);
            self.console_taken = n;
        }
        if self.cpu.console_len > self.sbi_taken {
            let n = self.cpu.console_len.min(self.cpu.console_buf.len());
            self.console.extend_from_slice(&self.cpu.console_buf[self.sbi_taken..n]);
            self.sbi_taken = n;
        }
    }

    /// Console output produced since the last call.
    pub fn take_console(&mut self) -> Vec<u8> {
        self.drain_console();
        core::mem::take(&mut self.console)
    }

    /// Feed keystrokes to the guest's UART.
    pub fn console_input(&mut self, bytes: &[u8]) {
        self.bus.uart_push_input(bytes);
    }
}

// ---------------------------------------------------------------------------
// Snapshots: the whole machine as bytes, so a browser can download a booted
// system instead of spending minutes interpreting the boot.
// ---------------------------------------------------------------------------

use riscv_core::state::{Reader, Writer};

/// Bumped whenever any save_state layout changes. A mismatch refuses to load:
/// snapshots are regenerable caches, never migration targets.
const SNAP_MAGIC: &[u8; 8] = b"RVSNAP01";

impl Machine {
    /// Serialize everything the next instruction depends on. Console output
    /// not yet collected is dropped — it belongs to the session that booted.
    pub fn save(&mut self) -> Result<Vec<u8>, &'static str> {
        let _ = self.take_console();
        let mut w = Writer::default();
        w.buf.extend_from_slice(SNAP_MAGIC);
        w.u64(self.steps);
        self.cpu.save_state(&mut w);
        self.bus.save_state(&mut w)?;
        Ok(w.buf)
    }

    /// Rebuild a machine from `save` output. No kernel, initrd or devicetree
    /// needed — RAM already contains everything the boot put there.
    pub fn restore(bytes: &[u8]) -> Option<Machine> {
        let mut r = Reader::new(bytes);
        if r.buf.get(..8)? != SNAP_MAGIC {
            return None;
        }
        r.pos = 8;
        let steps = r.u64()?;
        let mut cpu = Supervisor::new(DRAM_BASE, 0);
        cpu.load_state(&mut r)?;
        let bus = DeviceBus::load_state(&mut r)?;
        Some(Machine {
            bus,
            jit: crate::jit::Jit::new(),
            jit_table_base: 0,
            jit_enabled: false,
            jit_fault_pc: 0,
            jit_chain_insns: 0,
            jit_chains: 0,
            jit_retry: false,
            jit_icache_gen: 0,
            cpu,
            console: Vec::new(),
            console_taken: 0,
            sbi_taken: 0,
            steps,
            interp_hist: [0; 10],
            op_hist: [0; 24],
            interp_hist_on: false,
            chain_miss: [0; 9],
            tlb_miss: [0; 9],
            host_ns: 0,
            last_idle_host_ns: 0,
            idle_waits: [0; 7],
            idle_skipped: 0,
            idle_until: 0,
            chain_max: 8192,
            prof_insns: [0; 4],
            prof_idle_insns: 0,
            prof_idle_mark: 0,
            prof_wake_device: false,
            prof_parks: [0; 2],
            idle_handback: false,
            idle_pacing: false,
            idle_reported: false,
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl Machine {
    /// Enter compiled block `idx` and return how many guest instructions it
    /// retired.
    ///
    /// The transmute is the mechanism the whole JIT rests on: a function
    /// pointer on wasm is an index into the module's table, and the generated
    /// module installed its blocks into ours. It is sound only because block
    /// indices and table slots are kept one-to-one -- which is why a run that
    /// fails to compile still consumes a slot with a no-op body.
    /// Why the tail-call probe at the end of a compiled block missed.
    ///
    /// Reads the same slot generated code just read, so it must be called
    /// before `lookup` refills it. `pc` is the successor the probe was looking
    /// for. See `CHAIN_MISS_BINS` for what the answers mean.
    fn classify_chain_miss(&self, pc: u64) -> usize {
        let e = self.jit.chain[((pc >> 1) as usize) & (crate::jit::CHAIN_ENTRIES - 1)];
        // Generation 0 is the zeroed-table marker; live generations start at 1.
        if e.gen == 0 {
            return 0;
        }
        if e.key != pc {
            return 1;
        }
        if e.gen == self.jit.chain_gen {
            // Key and generation both matched, so the probe's third test is
            // what stopped it: the chain ran out of budget.
            return 5;
        }
        // Undo `refresh_gen`'s packing to see which half moved.
        let (was_trans, was_priv) = ((e.gen - 1) >> 2, (e.gen - 1) & 3);
        let (now_trans, now_priv) =
            ((self.jit.chain_gen - 1) >> 2, (self.jit.chain_gen - 1) & 3);
        match (was_trans != now_trans, was_priv != now_priv) {
            (false, true) => 2,
            (true, false) => 3,
            _ => 4,
        }
    }

    /// Run a chain of compiled blocks, returning the instructions retired.
    ///
    /// Each block that lands on another compiled block continues without
    /// returning to the run loop, which is where the entry cost lives. Stops on
    /// a fault, on reaching code that is not compiled, or at the cap.
    fn run_chain(&mut self, first: crate::jit::Hit, budget: u64) -> u64 {
        let chain_max = self.chain_max;

        // Budget for the wasm-side chain, read by every block's chain probe.
        self.cpu.cpu.jit_budget = budget.min(chain_max);

        self.jit_chains += 1;
        let mut hit = first;
        let mut done = 0u64;
        loop {
            let faulted_at = self.cpu.cpu.pc;
            done += self.run_compiled(hit);
            // A fault rewound the PC and cleared block_start so the interpreter
            // can take the trap; the chain has to end for that to happen.
            if self.cpu.cpu.pc == faulted_at || !self.cpu.block_start {
                if self.interp_hist_on {
                    self.chain_miss[7] += 1;
                }
                break;
            }
            // Classified before the cap check and before `lookup`, both of
            // which would destroy the evidence: the cap ends the chain whatever
            // the probe found, and `lookup` overwrites the slot being read.
            if self.interp_hist_on {
                let bin = self.classify_chain_miss(self.cpu.cpu.pc);
                self.chain_miss[bin] += 1;
            }
            if done >= budget || done >= chain_max {
                if self.interp_hist_on {
                    self.chain_miss[8] += 1;
                }
                break;
            }
            // The wasm chain consumed part of the budget; the Rust-side loop
            // picks up where it stopped.
            self.cpu.cpu.jit_budget = (budget - done).min(chain_max - done);
            // The virtually-keyed cache, not on_block_start: inside a chain
            // the mapping cannot have changed, so translating again per hop is
            // pure overhead.
            match self.jit.lookup(&mut self.cpu, &mut self.bus) {
                Some(next) => hit = next,
                None => {
                    // Nothing compiled here. Let the interpreter clear it and
                    // ask again immediately after.
                    if self.interp_hist_on {
                        self.chain_miss[6] += 1;
                    }
                    self.jit_retry = true;
                    break;
                }
            }
        }
        done
    }

    fn run_compiled(&mut self, hit: crate::jit::Hit) -> u64 {
        let crate::jit::Hit { idx, bytes, insns, branchy } = hit;
        let regs = core::ptr::addr_of!(self.cpu.cpu.x) as u32;
        self.cpu.cpu.jit_fault = 0;
        // The block, and anything it tail-calls, accumulates here.
        self.cpu.cpu.jit_insns = 0;
        // `idx` is already the absolute table slot; see Jit::installed.
        let entry = idx;
        unsafe {
            let f: extern "C" fn(u32, u64) = core::mem::transmute(entry);
            f(regs, self.cpu.cpu.pc);
        }
        if self.cpu.cpu.jit_fault != 0 {
            // An access faulted. Resume interpreting at the faulting
            // instruction; it will fault again and take the trap normally.
            // Instructions before it in the block have committed, which is
            // right -- they executed.
            self.cpu.cpu.jit_fault = 0;
            self.cpu.cpu.pc = self.jit_fault_pc;
            // NOT a block start. Leaving this true sends the next iteration
            // straight back into the same compiled block, which faults on the
            // same instruction again -- an infinite loop that burns the run
            // budget without the guest making any progress. The interpreter
            // has to execute this instruction so it can take the trap.
            self.cpu.block_start = false;
            return 1;
        }
        // Generated code does not touch the PC; the host advances it by the
        // block's length, which is why that length is recorded at install time.
        // Every block now writes where control goes, including ones that fall
        // through, so there is a single rule. `bytes` is still recorded for
        // diagnostics and for the fault path.
        let _ = (bytes, branchy, insns);
        self.cpu.cpu.pc = self.cpu.cpu.jit_next_pc;
        self.cpu.block_start = true;
        // What the whole chain retired, not just this block: successors were
        // tail-called and the host never saw them.
        self.jit_chain_insns += self.cpu.cpu.jit_insns;
        self.cpu.cpu.jit_insns
    }
}
