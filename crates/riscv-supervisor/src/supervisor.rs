//! Supervisor-layer CPU: extends base core with M/S modes, CSRs, MMU, interrupts

use alloc::vec::Vec;
use alloc::vec;
use riscv_core::{decode::decode, execute::{Bus, Cpu}, types::*};
use crate::types::*;
use crate::mmu::Mmu;

/// Decoded-instruction cache entries. Direct-mapped and indexed by
/// `(paddr >> 1) & (DCACHE_LEN - 1)`, shifted by one because RVC allows
/// 2-byte alignment, so bit 0 of the address carries no information but bit 1
/// does. 8192 entries covers a 16KB span of hot code.
const DCACHE_LEN: usize = 32768;
use crate::sbi;
use crate::types::AccessType;

// DEBUG: record unique PCs in the intc/timer kernel-text windows (written from step())
pub static mut DBG_PC_SET: [u64; 64] = [0u64; 64];
pub static mut DBG_PC_SET_N: usize = 0;

pub struct Supervisor {
    pub cpu: Cpu,
    pub mmu: Mmu,
    // Privilege & CSRs
    pub priv_level: Privilege,
    pub mstatus: MStatus,
    pub mie: u64,         // interrupt enable
    pub mip: u64,         // interrupt pending
    pub mtvec: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub mscratch: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    // S-mode CSRs
    pub stvec: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub sscratch: u64,
    pub satp: Satp,
    pub stimecmp: u64,
    // Counter
    pub mcycle: u64,
    pub minstret: u64,
    /// Set when this instruction wrote the counter, so its own increment is
    /// suppressed. Cleared at the end of every step.
    /// PMP configuration and address registers.
    ///
    /// STORAGE ONLY — nothing here enforces PMP. That is defensible for this
    /// machine and nowhere near correct in general: PMP restricts what M-mode
    /// grants to S/U, and we boot S-mode directly with no M-mode firmware to
    /// program it, so no guest here depends on enforcement. Software that
    /// probes the registers (Linux, OpenSBI, the ISA tests) needs them to read
    /// back sanely, which is what this provides.
    pub pmpcfg: [u64; 16],
    pub pmpaddr: [u64; 64],
    pub wrote_mcycle: bool,
    pub wrote_minstret: bool,
    // Hart ID
    pub mhartid: u64,
    // WFI state
    pub wfi: bool,
    /// Instructions remaining before the interpreter re-polls the CLINT/PLIC
    /// into `mip`. See `sync_device_interrupts`. Not serialised: a restore
    /// starts it at 0, forcing a fresh poll on the first step.
    int_sync_countdown: u32,
    /// Single-entry instruction-fetch translation cache: the last-fetched
    /// virtual page, the privilege it was validated at, and its physical page.
    /// The interpreter fetches sequentially within a page, so this turns almost
    /// every fetch's full `translate` (satp/TLB/permission checks) into a page
    /// compare. Keyed by privilege so a U/S switch misses rather than serving a
    /// wrong permission; invalidated on every mapping change (sfence/satp/
    /// fence.i/restore), exactly where the software TLB is. `u64::MAX` = empty.
    fetch_vpn: u64,
    fetch_priv: Privilege,
    fetch_ppn: u64,
    /// Set by `take_trap_with_vaddr`, cleared at the top of each `step()`.
    /// Guards against delivering the same trap twice in one instruction.
    trap_delivered: bool,
    // Reservation for LR/SC (single-hart, simple)
    pub reservation_addr: Option<u64>,
    // Console output buffer (for SBI debug_console writes)
    pub console_buf: Vec<u8>,
    pub console_len: usize,
    pub last_trap_cause: u64,
    pub last_trap_epc: u64,
    pub last_ecall_a7: u64,
    pub last_ecall_a6: u64,
    pub last_ecall_a0: u64,
    pub ecall_count: u64,
    // Diagnostic: last compressed instruction that failed to decode
    pub last_decode_fail_raw: u32,
    pub last_decode_fail_pc: u64,
    pub last_fetch_paddr: u64,
    pub last_fetch_half: u16,
    pub last_fetched_raw: u32,
    pub dbg_last_time: u64,
    pub dbg_time_reads: u64,
    /// Enables the per-instruction diagnostic recording below: the pc/raw ring
    /// buffer and the unique-PC set. Both run on *every* retired instruction —
    /// the ring is a store per instruction, and the PC set does a linear scan of
    /// up to 64 entries whenever the guest is inside a kernel window, which is
    /// most of the time. That cost is worth paying while chasing a fault and not
    /// otherwise, so it is off unless a diagnostic test asks for it.
    /// Did the previous instruction land the PC somewhere other than the next
    /// instruction? True after a taken branch, jump, trap or return -- which
    /// is exactly where a basic block can begin.
    /// Bumped whenever a cached virtual-to-physical mapping could have gone
    /// stale: `satp` writes, `sfence.vma`, snapshot restore, and `fence.i`.
    ///
    /// Lets the JIT keep a virtually-keyed lookup cache without re-translating
    /// every block entry. Same contract as the TLB, which is flushed at the
    /// same points.
    pub trans_gen: u32,
    /// Diagnostic: what moved `trans_gen`, by cause. Order is
    /// `GEN_BUMP_CAUSES`. Every bump invalidates the inlined TLB, the chain
    /// table and the virtual-PC cache wholesale, so knowing which cause
    /// dominates decides whether the fix is ASIDs (satp) or honouring
    /// sfence.vma's operands (a single-page flush should not be global).
    pub gen_bump: [u64; 4],
    /// Generation for cached DATA translations only.
    ///
    /// Split from `trans_gen` because the two have different invalidation
    /// rules and the difference is most of the JIT's TLB misses. `trans_gen`
    /// guards caches keyed by VIRTUAL PC -- the chain table and the virtual-PC
    /// cache -- which must be discarded whenever any mapping changes, since a
    /// stale entry would run code compiled for a page that has since moved.
    /// This one guards the inlined data TLB, whose entries are per-page and can
    /// therefore be invalidated individually.
    ///
    /// So a single-page `sfence.vma` bumps `trans_gen` and queues the one page
    /// in `pending_flush`, instead of voiding every cached translation the
    /// guest had. Measured: 96% of bumps were single-page flushes, and they
    /// caused 93% of inlined-TLB misses.
    pub data_trans_gen: u32,
    /// ASID-aware generation cache, keyed by the full satp value (mode|asid|ppn,
    /// NOT the asid alone — asid 0 aliases swapper and every early-boot root, so
    /// keying on it corrupts across a switch). A satp write saves the current
    /// `(trans_gen, data_trans_gen)` under the outgoing space and restores the
    /// incoming space's, so returning to a process revalidates its cached
    /// chain/vcache/inline-TLB entries for free instead of re-translating. A
    /// global `sfence.vma`, `fence.i`, and snapshot restore still invalidate
    /// (they drop the whole table). Direct-mapped; a collision just evicts, and
    /// the evicted space gets a fresh unique generation next time — safe,
    /// because generations are drawn from `asid_gen_next` and never reused. Not
    /// serialised: rebuilt empty on restore.
    asid_tbl: [AsidSlot; ASID_SLOTS],
    /// Monotonic generation allocator. Every `trans_gen`/`data_trans_gen` value
    /// ever assigned is drawn from here, so two live address spaces can never
    /// share a generation and a stale entry can never alias a live one.
    asid_gen_next: u32,
    /// Pages a single-page `sfence.vma` asked to invalidate, for the host to
    /// drain into the inlined TLB. Overflowing is safe: the fallback is to bump
    /// `data_trans_gen`, which is exactly the old behaviour of throwing
    /// everything away.
    pub pending_flush: [u64; 8],
    pub pending_flush_n: u8,
    /// Bumped only by `fence.i`, i.e. only when instruction *memory* changed.
    ///
    /// Deliberately separate from `trans_gen`. That one moves on every satp
    /// write and sfence.vma, and compiled blocks are keyed on physical address
    /// precisely so those cost nothing — flushing them on a context switch
    /// would throw the whole design away. This is the one event that really
    /// does invalidate compiled code, and it is rare.
    pub icache_gen: u32,
    pub block_start: bool,
    pub trace_enabled: bool,
    /// Decoded-instruction cache, direct-mapped, keyed on PHYSICAL address.
    ///
    /// Fetch-and-decode was repeating itself for every instruction of every
    /// loop: read the halfword, test for a compressed encoding, and pull the
    /// operands back out of the bits. The result depends only on the bytes at
    /// the physical address, so it can be remembered.
    ///
    /// Keying on the physical address rather than the virtual one is what makes
    /// this cheap. A virtual key would have to be thrown away on every
    /// `sfence.vma` and every `satp` write -- both of which Linux does on every
    /// context switch -- and the flushing would have eaten the win. Physical
    /// mappings only change when memory itself changes, so the only thing that
    /// invalidates an entry is `fence.i`, which is rare and is precisely the
    /// guest's promise that it has rewritten instruction memory. This is the
    /// same contract a real icache runs under.
    ///
    /// Entry: (physical address, decoded instruction, width, raw encoding).
    dcache: Vec<Option<(u64, Instr, u8, u32)>>,
    // PC/raw ring buffer: last executed instructions (for diagnosing the path into a fault)
    pub pc_trace: [(u64, u32); 1024],
    pub pc_trace_idx: usize,
    // Ring of (jal_pc, jal_raw, phys_pc) for every instruction that jumps from a *virtual*
    // address (>= 0xffffffff80000000) to a *physical* kernel-image address (0x80000000..0x82000000)
    // while the MMU is ON. That transition is the relocation / return-address bug: a jal/jr
    // computes a 32-bit (physical) target instead of a 64-bit VA, so the next fetch faults.
    pub phys_transitions: [(u64, u32, u64); 64],
    pub phys_trans_idx: usize,
    // Snapshot of the pc_trace ring taken at the exact moment an instruction fetch
    // faults on a *physical* kernel-image pc (0x80000000..0x82000000) with the MMU on.
    // This captures the last <=64 instructions before the fault -- including the
    // virtual->physical jump that caused it (which the post-storm dump would have lost).
    pub phys_fault_pc: u64,
    pub phys_fault_ring: [(u64, u32); 1024],
    pub phys_fault_captured: bool,
    // One-shot: the first time the hart executes an instruction inside the
    // secondary-hart trampoline region (0x80201000..0x80201200) we snapshot the pc_trace
    // ring. That snapshot holds the SMP-bringup caller X that wrongly launched the boot
    // hart down the secondary-start path. All pc values are identity-mapped (physical).
    pub tramp_entry_captured: bool,
    pub tramp_entry_ring: [(u64, u32); 1024],
    // Rolling snapshot of the pc_trace ring taken at the *last* transition from a
    // high (>=0xffffffff80000000) virtual pc to a low (<0xc0000000) physical/identity pc
    // with the MMU on. That transition is the entry into the secondary-hart trampoline
    // path; capturing the last one shows the high-level caller that launched it.
    pub low_trans_ring: [(u64, u32); 64],

    /// Optional store trace: (pc, vaddr, value), used to attribute memory
    /// corruption to the instruction that caused it. Empty = disabled.
    pub store_ring: Vec<(u64, u64, u64)>,
    pub store_head: usize,

    /// Interrupt-return register check (see `do_sret`). Snapshot is
    /// (sepc, satp.ppn, sp, x[0..32]); mismatch is (sepc, reg, before, after).
    /// Answer S-mode ecalls as SBI calls in-place. True for booting Linux
    /// (we provide no M-mode firmware); false for bare-metal tests that need
    /// ecall to trap to mtvec like real hardware.
    /// Value watch: log 64-bit stores whose value, masked by `watch_mask`,
    /// equals `watch_expect`.
    /// Disabled when watch_mask is 0. Used to catch a specific packed field
    /// being written without knowing its address in advance.
    pub watch_mask: u64,
    pub watch_expect: u64,
    /// Also require at least one bit set OUTSIDE watch_mask.
    pub watch_outside_nonzero: bool,
    /// Optional pc range the store must come from (0..u64::MAX = any).
    /// Match on the low 12 bits of pc instead of the full address. Module
    /// load addresses move between boots but page offsets do not, so this is
    /// how you pin a specific instruction inside a module. hi = 0 disables.
    pub watch_pcoff: [u64; 8],
    /// Latched page of the anchor offset; 0 until the anchor first runs.
    pub watch_page: u64,
    pub watch_pc_lo: u64,
    pub watch_pc_hi: u64,
    pub watch_hits: Vec<(u64, u64, u64)>,
    /// Loads matching the same filter, kept apart from stores.
    pub watch_loads: Vec<(u64, u64, u64)>,
    pub sbi_enabled: bool,
    pub check_irq_regs: bool,
    pub irq_snapshot: Option<(u64, u64, u64, [u64; 32])>,
    pub irq_mismatch: Option<(u64, u8, u64, u64)>,
    pub irq_snaps: u64,
    pub irq_compares: u64,
}

/// Wrapper that feeds SBI console output into Supervisor's fixed buffer.
struct ConsoleToBuf<'a>(&'a mut Supervisor);

impl<'a> sbi::HostConsole for ConsoleToBuf<'a> {
    fn write(&mut self, buf: &[u8]) {
        let s = &mut self.0;
        // Grow dynamically so console output is never truncated (loglevel=7 floods).
        s.console_buf.extend_from_slice(buf);
        s.console_len = s.console_buf.len();
    }
}

/// What moved `trans_gen`. See `Supervisor::gen_bump`.
/// How many interpreted instructions may pass between CLINT/PLIC polls. The
/// interpreter runs a small fraction of instructions and only in short bursts
/// between block boundaries (where the run loop re-polls), so this bounds
/// worst-case interrupt latency on straight-line interpreted stretches to 64
/// instructions — microseconds, and far under the compiled path's 8192-insn
/// chain budget.
const INT_SYNC_INTERVAL: u32 = 64;

pub const GEN_BUMP_CAUSES: [&str; 4] = [
    "satp write (changed)",
    "sfence.vma global",
    "sfence.vma one page",
    "other (restore, fence.i)",
];

/// One remembered address space's generations, keyed by its full satp value.
/// See `Supervisor::asid_tbl`.
#[derive(Clone, Copy)]
struct AsidSlot {
    /// The satp value (mode|asid|ppn) this slot describes; `u64::MAX` = empty.
    satp: u64,
    trans_gen: u32,
    data_trans_gen: u32,
}

impl AsidSlot {
    const EMPTY: AsidSlot = AsidSlot { satp: u64::MAX, trans_gen: 0, data_trans_gen: 0 };
}

/// How many address spaces the generation cache remembers. The hot working set
/// (the shell, the piped commands, the kernel) is a handful; this is sized for
/// headroom. Direct-mapped, power of two.
const ASID_SLOTS: usize = 128;

/// ASID-keyed generations: a satp write restores that address space's cached
/// generation instead of voiding every virtual-keyed cache. Measured +14.3%
/// on a `yes | cat` pipe (context-switch-bound: chain misses halved), neutral
/// on compute-bound loads, correct on every workload tried. Off = the old
/// invalidate-on-switch behaviour, kept as a one-line knockout for this
/// correctness-sensitive change.
const ASID_KEYED: bool = true;

impl Supervisor {
    pub fn new(pc: u64, hartid: u64) -> Self {
        let mut cpu = Cpu::new(pc);
        cpu.x[0] = 0; // x0 hardwired
        Self {
            cpu,
            mmu: Mmu::new(),
            priv_level: Privilege::Machine,
            mstatus: MStatus::default(),
            mie: 1 << 7, // MTIE: enable machine timer interrupt initially
            mip: 0,
            mtvec: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            mscratch: 0,
            medeleg: 0xFFFF, // Delegate common exceptions to S-mode
            mideleg: 0x2A2, // Delegate SSIP(1)/STIP(5)/MTIP->STIP(7)/SEIP(9) to S-mode (OpenSBI behavior)
            stvec: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            sscratch: 0,
            satp: Satp { mode: 0, asid: 0, ppn: 0 },
            stimecmp: u64::MAX,
            mcycle: 0,
            minstret: 0,
            pmpcfg: [0; 16],
            pmpaddr: [0; 64],
            wrote_mcycle: false,
            wrote_minstret: false,
            mhartid: hartid,
            wfi: false,
            int_sync_countdown: 0,
            fetch_vpn: u64::MAX,
            fetch_priv: Privilege::Machine,
            fetch_ppn: 0,
            trap_delivered: false,
            reservation_addr: None,
            console_buf: vec![0u8; 1048576],
            console_len: 0,
            last_trap_cause: 0,
            last_trap_epc: 0,
            last_ecall_a7: 0,
            last_ecall_a6: 0,
            last_ecall_a0: 0,
            ecall_count: 0,
            last_decode_fail_raw: 0,
            last_decode_fail_pc: 0,
            last_fetch_paddr: 0,
            last_fetch_half: 0,
            last_fetched_raw: 0,
            dbg_last_time: 0,
            dbg_time_reads: 0,
            trans_gen: 0,
            gen_bump: [0; 4],
            data_trans_gen: 0,
            asid_tbl: [AsidSlot::EMPTY; ASID_SLOTS],
            asid_gen_next: 0,
            pending_flush: [0; 8],
            pending_flush_n: 0,
            icache_gen: 0,
            block_start: true,
            trace_enabled: false,
            dcache: vec![None; DCACHE_LEN],
            pc_trace: [(0u64, 0u32); 1024],
            pc_trace_idx: 0,
            phys_transitions: [(0u64, 0u32, 0u64); 64],
            phys_trans_idx: 0,
            phys_fault_pc: 0,
            phys_fault_ring: [(0u64, 0u32); 1024],
            phys_fault_captured: false,
            low_trans_ring: [(0u64, 0u32); 64],
            tramp_entry_captured: false,
            tramp_entry_ring: [(0u64, 0u32); 1024],
            store_ring: Vec::new(),
            store_head: 0,
            watch_mask: 0,
            watch_expect: 0,
            watch_outside_nonzero: false,
            watch_pcoff: [u64::MAX; 8],
            watch_page: 0,
            watch_pc_lo: 0,
            watch_pc_hi: u64::MAX,
            watch_hits: Vec::new(),
            watch_loads: Vec::new(),
            sbi_enabled: true,
            check_irq_regs: false,
            irq_snapshot: None,
            irq_mismatch: None,
            irq_snaps: 0,
            irq_compares: 0,
        }
    }

    /// Instructions that are simple loads/stores (need address translation in S/U mode)
    fn is_simple_load_store(instr: &Instr) -> bool {
        use Instr::*;
        matches!(instr,
            Lb{..}|Lh{..}|Lw{..}|Ld{..}|Lbu{..}|Lhu{..}|Lwu{..}|Fld{..}|Flw{..}|
            Sb{..}|Sh{..}|Sw{..}|Sd{..}|Fsd{..}|Fsw{..}
        )
    }
    /// Instructions that belong to the F/D extension and are therefore gated by
    /// mstatus.FS. Note the loads and stores count: `fsw` with FS off must not
    /// reach memory at all.
    fn is_fp(instr: &Instr) -> bool {
        use Instr::*;
        matches!(instr, Fp { .. } | Flw { .. } | Fsw { .. } | Fld { .. } | Fsd { .. })
    }

    fn is_atomic(instr: &Instr) -> bool {
        use Instr::*;
        matches!(instr,
            Amoswapw{..}|Amoaddw{..}|Amoxorw{..}|Amoandw{..}|Amoorw{..}|
            Amominw{..}|Amomaxw{..}|Amominuw{..}|Amomaxuw{..}|
            Amoswapd{..}|Amoaddd{..}|Amoxord{..}|Amoandd{..}|Amoord{..}|
            Amomind{..}|Amomaxd{..}|Amominud{..}|Amomaxud{..}
        )
    }

    /// Temporary debug method to expose translation result
    pub fn debug_translate(&mut self, bus: &mut dyn Bus, access: AccessType, vaddr: u64) -> Result<u64, u64> {
        match self.translate(bus, access, vaddr) {
            Ok(paddr) => Ok(paddr),
            Err(_) => Err(0xDEAD),
        }
    }

    /// Temporary debug method to fetch and decode instruction
    /// Instruction-fetch translation with a single-entry page cache. Sequential
    /// fetches inside a page (the common case) skip the full `translate` walk
    /// (satp mode, software TLB, permission/privilege checks) and return the
    /// cached physical page + offset. The cache is populated only after a
    /// successful fetch-translate, so the page is guaranteed executable, A-bit
    /// set, and privilege-legal at `fetch_priv`; keying by privilege makes a
    /// U/S switch miss instead of serving a wrong-permission page. Invalidated
    /// on every mapping change (fence.i via dcache_flush, sfence.vma, satp
    /// write, restore), exactly where the software TLB is dropped.
    #[inline]
    fn fetch_translate(&mut self, bus: &mut dyn Bus, vaddr: u64) -> Result<u64, Trap> {
        let vpn = vaddr >> 12;
        if vpn == self.fetch_vpn && self.priv_level == self.fetch_priv {
            return Ok((self.fetch_ppn << 12) | (vaddr & 0xFFF));
        }
        let paddr = self.translate(bus, AccessType::Instruction, vaddr)?;
        self.fetch_vpn = vpn;
        self.fetch_priv = self.priv_level;
        self.fetch_ppn = paddr >> 12;
        Ok(paddr)
    }

    pub fn debug_fetch(&mut self, bus: &mut dyn Bus, vaddr: u64) -> Result<(u64, u16, u8, u32), u64> {
        match self.translate(bus, AccessType::Instruction, vaddr) {
            Ok(paddr) => {
                let half = bus.read_u16(paddr);
                if (half & 0b11) != 0b11 {
                    match riscv_core::compressed::decompress(half) {
                        Some(_) => Ok((paddr, half, 2, 0)),
                        None => Ok((paddr, half, 0, ((bus.read_u16(paddr + 2) as u32) << 16) | (half as u32))),
                    }
                } else {
                    let hi = bus.read_u16(paddr + 2);
                    let raw = ((hi as u32) << 16) | (half as u32);
                    Ok((paddr, half, 4, raw))
                }
            }
            Err(_) => Err(0xDEAD),
        }
    }

    /// Drop every cached decode. Called on `fence.i` -- and on snapshot restore,
    /// where the guest's memory is replaced wholesale under us.
    pub fn dcache_flush(&mut self) {
        // fence.i means instruction memory changed, so anything decoded or
        // compiled from it is stale too -- across every address space, so drop
        // the whole ASID table, not just the current space's generation.
        self.invalidate_all_spaces();
        self.icache_gen = self.icache_gen.wrapping_add(1);
        for e in self.dcache.iter_mut() {
            *e = None;
        }
        self.fetch_vpn = u64::MAX; // fetch-page cache: instruction memory changed
    }

    /// Hash a satp value to its `asid_tbl` slot. The ppn (page-table root)
    /// distinguishes address spaces; mixing a few high bits keeps nearby roots
    /// from clustering. Collisions are safe — they evict.
    fn asid_index(satp: u64) -> usize {
        let ppn = satp & 0xFFFF_FFFF_FFF;
        ((ppn ^ (ppn >> 10) ^ (ppn >> 21)) as usize) & (ASID_SLOTS - 1)
    }

    /// A satp write: stash the outgoing space's generations and select the
    /// incoming space's, WITHOUT invalidating either. This is what lets a
    /// context switch keep the chain table, vcache and inlined data TLB — the
    /// one change this whole feature is for. See `asid_tbl`.
    fn switch_address_space(&mut self, old: u64, new: u64) {
        if !ASID_KEYED {
            // Knockout: treat every switch as a full invalidation, the
            // pre-ASID behaviour.
            self.invalidate_all_spaces();
            return;
        }
        // Save the outgoing space's current generations under its satp.
        let oi = Self::asid_index(old);
        self.asid_tbl[oi] = AsidSlot {
            satp: old,
            trans_gen: self.trans_gen,
            data_trans_gen: self.data_trans_gen,
        };
        // Restore the incoming space's generations, or mint a fresh unique pair
        // for a space we have not seen (or have evicted).
        let ni = Self::asid_index(new);
        let slot = self.asid_tbl[ni];
        if slot.satp == new {
            self.trans_gen = slot.trans_gen;
            self.data_trans_gen = slot.data_trans_gen;
        } else {
            self.asid_gen_next = self.asid_gen_next.wrapping_add(1);
            self.trans_gen = self.asid_gen_next;
            self.data_trans_gen = self.asid_gen_next;
        }
    }

    /// Drop every remembered space and move the current generations to a fresh
    /// value. For the events that can invalidate ANY space's cached
    /// translations or the code itself: a global `sfence.vma`, `fence.i`,
    /// snapshot restore, and the single-page flush queue overflowing. After
    /// this each space re-translates on its next entry — the pre-ASID behaviour
    /// for these (rare, ~4x less frequent than satp switches) events.
    fn invalidate_all_spaces(&mut self) {
        for s in self.asid_tbl.iter_mut() {
            *s = AsidSlot::EMPTY;
        }
        self.asid_gen_next = self.asid_gen_next.wrapping_add(1);
        self.trans_gen = self.asid_gen_next;
        self.data_trans_gen = self.asid_gen_next;
    }

    /// Move only the CURRENT space's chain/vcache generation to a fresh unique
    /// value. The single-page-sfence drain calls this when the flushed page
    /// might hold a chain key: the current space's chain entries must go, but no
    /// other space's, and the new value must not collide with any live space.
    /// data-TLB stays (that cache is invalidated per page).
    pub fn advance_chain_gen(&mut self) {
        self.asid_gen_next = self.asid_gen_next.wrapping_add(1);
        self.trans_gen = self.asid_gen_next;
    }

    /// Record (pc, raw) into the ring buffer so a fault's preceding path can be dumped.
    fn record_trace(&mut self, pc: u64, raw: u32) {
        let i = self.pc_trace_idx % 1024;
        self.pc_trace[i] = (pc, raw);
        self.pc_trace_idx = self.pc_trace_idx.wrapping_add(1);
    }

    /// True if `pc` is a *physical* kernel-image address (high 32 bits clear, in
    /// the loaded-image range). A correctly-relocated kernel only ever uses 64-bit
    /// virtual addresses (>= 0xffffffff80000000), so a physical pc here is a bug.
    fn is_phys_kernel(pc: u64) -> bool {
        pc >= 0x8000_0000 && pc < 0x8200_0000
    }

    /// Record a virtual->physical transition (jal/jr/sret/mret/csr) for later dump.
    fn record_phys_transition(&mut self, src_pc: u64, phys_pc: u64) {
        let i = self.phys_trans_idx % 64;
        self.phys_transitions[i] = (src_pc, 0, phys_pc);
        self.phys_trans_idx = self.phys_trans_idx.wrapping_add(1);
    }

    /// Single step: fetch, decode, translate, execute
    pub fn step(&mut self, bus: &mut dyn Bus) -> Status {
        self.trap_delivered = false;
        // 0. Sync S-mode timer interrupt (STIP, bit 5). The kernel runs in S-mode and
        // uses SBI set_timer (which writes the CLINT mtimecmp MMIO at 0x0200_4000) or,
        // if Sstc is built in, the stimecmp CSR (0x14D). There is no M-mode OpenSBI to
        // forward the CLINT MTIP, so we raise STIP directly whenever the CLINT mtimecmp
        // or the Sstc stimecmp is reached. (MTIP itself is not delegated to S-mode, so
        // raising it would never reach the kernel.)
        // 0/0b. Sync STIP (CLINT timer) and SEIP (PLIC) into mip. BATCHED: the
        // three device reads are the interpreter's most-repeated cost, so poll
        // them at most every INT_SYNC_INTERVAL instructions instead of every
        // one. Freshness where it matters is preserved elsewhere: the run loop
        // calls interrupt_pending (a full sync) at every block boundary, and
        // WFI forces a sync before parking. See sync_device_interrupts.
        if self.int_sync_countdown == 0 {
            self.int_sync_countdown = INT_SYNC_INTERVAL;
            self.sync_device_interrupts(bus);
        } else {
            self.int_sync_countdown -= 1;
        }

        // 1. Check interrupts before instruction (cheap: reads mip)
        if let Some(trap) = self.check_interrupts() {
            return self.take_trap(trap);
        }

        // DEBUG: record unique PCs entering the intc window [0x80a26000,0x80a29000) or
        // timer window [0x8101b000,0x81020000) so we can see which of these functions run.
        if self.trace_enabled {
            let pc = self.cpu.pc;
            let in_win = (pc >= 0xFFFF_FFFF_8020_0000u64 && pc < 0xFFFF_FFFF_8024_0000u64)  // kernel entry/head.S (VALIDATE)
                      || (pc >= 0xFFFF_FFFF_80A0_0000u64 && pc < 0xFFFF_FFFF_80B0_0000u64)  // intc region (1MB)
                      || (pc >= 0xFFFF_FFFF_80F0_0000u64 && pc < 0xFFFF_FFFF_8120_0000u64); // timer region (3MB)
            if in_win {
                unsafe {
                    let mut found = false;
                    let n = DBG_PC_SET_N;
                    for k in 0..n {
                        if DBG_PC_SET[k] == pc { found = true; break; }
                    }
                    if !found && DBG_PC_SET_N < 64 {
                        DBG_PC_SET[DBG_PC_SET_N] = pc;
                        DBG_PC_SET_N += 1;
                    }
                }
            }
        }
        // 2. Fetch instruction (translation via the single-entry fetch-page cache)
        let paddr = match self.fetch_translate(bus, self.cpu.pc) {
            Ok(addr) => addr,
            Err(trap) => {
                // Snapshot the pc_trace ring at the exact moment we fault on a physical
                // kernel-image pc (0x80000000..0x82000000) with the MMU on. This captures
                // the last <=64 instructions before the fault -- including the virtual->
                // physical jump that caused it (the post-storm dump would have lost it).
                if self.satp.mode == 8
                    && self.cpu.pc >= 0x8000_0000
                    && self.cpu.pc < 0x8200_0000
                {
                    self.phys_fault_pc = self.cpu.pc;
                    // Copy the entire 256-entry pc_trace ring in chronological order
                    // (oldest -> newest) so the virtual->physical entry into the trampoline
                    // is captured even though it is far from the final faulting fetch.
                    for k in 0..1024usize {
                        let idx = (self.pc_trace_idx.wrapping_add(k)) % 1024;
                        self.phys_fault_ring[k] = self.pc_trace[idx];
                    }
                    self.phys_fault_captured = true;
                }
                return self.take_trap_with_vaddr(trap, Some(self.cpu.pc));
            }
        };

        self.last_fetch_paddr = paddr;
        let dc_idx = ((paddr >> 1) as usize) & (DCACHE_LEN - 1);
        if let Some((tag, cached, width, raw)) = self.dcache[dc_idx] {
            if tag == paddr {
                self.last_fetched_raw = raw;
                self.last_fetch_half = raw as u16;
                return self.finish_step(cached, width, bus);
            }
        }
        let half = bus.read_u16(paddr);
        self.last_fetch_half = half;
        let (instr, width) = if (half & 0b11) != 0b11 {
            match riscv_core::compressed::decompress(half) {
                Some(ins) => {
                    self.last_fetched_raw = half as u32;
                    (ins, 2u8)
                }
                None => {
                    let hi = self.fetch_upper_half(bus, paddr).unwrap_or(0);
                    let raw = ((hi as u32) << 16) | (half as u32);
                    self.last_decode_fail_raw = raw;
                    self.last_fetched_raw = raw;
                    self.last_decode_fail_pc = self.cpu.pc;
                    return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
                }
            }
        } else {
            // The upper half of a 32-bit instruction can sit on the NEXT page.
            // With the C extension an instruction only needs 2-byte alignment,
            // so one at `...ffe` straddles a page boundary — and consecutive
            // virtual pages are not physically contiguous. Reading `paddr + 2`
            // then fetches whatever physically follows the first page, giving a
            // corrupt upper half: correct rd/funct3/rs1 out of the low half, and
            // a garbage immediate out of the high one.
            let hi = match self.fetch_upper_half(bus, paddr) {
                Ok(h) => h,
                Err(trap) => return self.take_trap_with_vaddr(trap, Some(self.cpu.pc.wrapping_add(2))),
            };
            let raw = ((hi as u32) << 16) | (half as u32);
            self.last_fetched_raw = raw;
            (decode(raw), 4u8)
        };
        self.dcache[dc_idx] = Some((paddr, instr, width, self.last_fetched_raw));
        self.finish_step(instr, width, bus)
    }

    /// The half of `step` from "we have a decoded instruction" onwards, shared
    /// by the decode-cache hit and miss paths.
    fn finish_step(&mut self, instr: Instr, width: u8, bus: &mut dyn Bus) -> Status {
        // 3. Execute (handle CSRs and privileged ops here)
        let prev_pc = self.cpu.pc;
        let prev_raw = self.last_fetched_raw;
        // One-shot capture of the pc_trace ring the first time we execute an instruction
        // inside the secondary-hart trampoline region (0x80201000..0x80201200). This is the
        // boot hart wrongly taking the secondary-start path; the snapshot shows the SMP
        // bring-up caller X that launched it.
        if self.trace_enabled && !self.tramp_entry_captured && prev_pc >= 0x8020_1000 && prev_pc < 0x8020_1200 {
            let base = self.pc_trace_idx.wrapping_sub(1024) % 1024;
            for k in 0..1024usize {
                let idx = (base + k) % 1024;
                self.tramp_entry_ring[k] = self.pc_trace[idx];
            }
            self.tramp_entry_captured = true;
        }

        if instr == Instr::FenceI {
            self.dcache_flush();
        }
        let status = self.execute_supervisor(instr, width, bus);
        // A trap moves the PC without falling through, and take_trap can also
        // fire inside execute_supervisor, so check the flag as well as the
        // arithmetic.
        self.block_start =
            self.trap_delivered || self.cpu.pc != prev_pc.wrapping_add(width as u64);

        // Record every virtual->physical jump (the relocation / return-address bug).
        // prev_pc is the jumping instruction (virtual, high bits set); the new pc is a
        // physical kernel-image address (high bits clear). Only the *transition* is logged.
        // No MMU-mode guard: the transition can happen during the early relocate trampoline
        // (MMU still off) and only fault later once the MMU is enabled.
        if self.trace_enabled
            && prev_pc >= 0xffff_ffff_8000_0000u64
            && self.cpu.pc >= 0x8000_0000
            && self.cpu.pc < 0x8200_0000
        {
            let i = self.phys_trans_idx % 64;
            self.phys_transitions[i] = (prev_pc, prev_raw, self.cpu.pc);
            self.phys_trans_idx = self.phys_trans_idx.wrapping_add(1);
        }

        // Rolling snapshot of the LAST high->low (virtual -> physical/identity) transition.
        // This captures the entry into the secondary-hart trampoline path.
        if self.trace_enabled && prev_pc >= 0xffff_ffff_8000_0000u64 && self.cpu.pc < 0xc000_0000u64 {
            let base = self.pc_trace_idx.wrapping_sub(64) % 1024;
            for k in 0..64usize {
                let idx = (base + k) % 1024;
                self.low_trans_ring[k] = self.pc_trace[idx];
            }
        }

        // A CSR write to a counter takes precedence over that instruction's own
        // increment, so `csrw minstret, 0` followed by a read must yield 0 and
        // not 1. Without this the counter can never be set to a chosen value.
        if !self.wrote_mcycle {
            self.mcycle = self.mcycle.wrapping_add(1);
        }
        if !self.wrote_minstret {
            self.minstret = self.minstret.wrapping_add(1);
        }
        self.wrote_mcycle = false;
        self.wrote_minstret = false;

        // A *bare* Trap coming back from `cpu.execute_width` (e.g. a decode-fail
        // IllegalInstruction) has not been delivered yet: sepc/scause are still stale
        // and pc still points at the faulting instruction, so deliver it here.
        // Traps raised inside `execute_supervisor` itself already went through
        // `take_trap_with_vaddr`; re-delivering those would overwrite sepc with stvec
        // and stval with 0.
        if let Status::Trap(trap) = status {
            if !self.trap_delivered {
                return self.take_trap(trap);
            }
        }
        status
    }

    fn translate(&mut self, bus: &mut dyn Bus, access: AccessType, vaddr: u64) -> Result<u64, Trap> {
        // M-mode always bypasses MMU for instruction fetches and for data
        // accesses unless MPRV=1 (in which case MPP determines privilege).
        if self.priv_level == Privilege::Machine {
            if access == AccessType::Instruction {
                return Ok(vaddr);
            }
            if !self.mstatus.mprv {
                return Ok(vaddr);
            }
            // MPRV=1 in M-mode: use MPP as the effective privilege.
            let eff_priv = self.priv_level_from_mpp();
            // ...but when MPP is M the effective privilege IS machine, and
            // machine-mode data accesses are untranslated. Walking the page
            // tables here breaks any trap handler running with MPRV still set:
            // taking a trap into M-mode leaves MPRV alone and sets MPP = M, so
            // the handler's own loads were being translated when they must not
            // be.
            if eff_priv == Privilege::Machine {
                return Ok(vaddr);
            }
            return self.mmu.translate(
                bus, &self.satp, eff_priv,
                self.mstatus.sum, self.mstatus.mxr,
                access, vaddr,
            );
        }
        // S-mode / U-mode always use translation
        self.mmu.translate(
            bus, &self.satp, self.priv_level,
            self.mstatus.sum, self.mstatus.mxr,
            access, vaddr,
        )
    }

    fn priv_level_from_mpp(&self) -> Privilege {
        match self.mstatus.mpp {
            0 => Privilege::User,
            1 => Privilege::Supervisor,
            3 => Privilege::Machine,
            _ => Privilege::User, // Invalid, treat as U
        }
    }

    /// Execute instruction with supervisor/privileged handling
    fn execute_supervisor(&mut self, instr: Instr, width: u8, bus: &mut dyn Bus) -> Status {
        use Instr::*;

        // Handle CSRs
        match instr {
            Csrrw { rd, rs1, csr } => return self.csr_op(bus, csr, rd, rs1, self.read_reg(rs1), false, false, width),
            Csrrs { rd, rs1, csr } => return self.csr_op(bus, csr, rd, rs1, self.read_reg(rs1), true, false, width),
            Csrrc { rd, rs1, csr } => return self.csr_op(bus, csr, rd, rs1, self.read_reg(rs1), true, true, width),
            // clear=false: CSRRWI *writes* the immediate, it does not clear
            // bits. With clear=true this computed `old & !imm`, i.e. it behaved
            // as CSRRCI, so every `csrwi X, n` silently preserved the old value
            // instead of replacing it — `csrwi medeleg, 0` left medeleg alone.
            Csrrwi { rd, zimm, csr } => return self.csr_op(bus, csr, rd, zimm, zimm as u64, false, false, width),
            Csrrsi { rd, zimm, csr } => return self.csr_op(bus, csr, rd, zimm, zimm as u64, true, false, width),
            Csrrci { rd, zimm, csr } => return self.csr_op(bus, csr, rd, zimm, zimm as u64, true, true, width),
            Ecall => {
                // We have no M-mode firmware, so an S-mode ecall is answered as
                // an SBI call right here. That is right for booting Linux and
                // wrong for anything that expects a real trap to mtvec — the
                // rv64si ISA tests run in S-mode and signal their result with
                // ecall, so with this always on they could never report at all.
                if self.sbi_enabled && self.priv_level == Privilege::Supervisor {
                    self.ecall_count += 1;
                    self.last_ecall_a7 = self.cpu.read_reg(17);
                    self.last_ecall_a6 = self.cpu.read_reg(16);
                    self.last_ecall_a0 = self.cpu.read_reg(10);
                    let a0 = self.cpu.read_reg(10);
                    let a1 = self.cpu.read_reg(11);
                    let a2 = self.cpu.read_reg(12);
                    let a3 = self.cpu.read_reg(13);
                    let a4 = self.cpu.read_reg(14);
                    let a5 = self.cpu.read_reg(15);
                    let a6 = self.cpu.read_reg(16);
                    let a7 = self.cpu.read_reg(17);
                    let mut sink = ConsoleToBuf(self);
                    let ret = sbi::handle_ecall(bus, a0, a1, a2, a3, a4, a5, a6, a7, &mut sink);
                    if ret.ext == 0x54494D45 {
                        // Kernel issued SBI set_timer. Our model raises STIP (mip bit 5)
                        // directly in step(), and STIP is delegated to S-mode via mideleg.
                        // The enable bit that gates a delegated STIP is STIE (sie bit 5),
                        // NOT MTIE (bit 7) -- MTIP is never raised by our timer model, so
                        // setting MTIE here did nothing and the timer interrupt was never
                        // delivered, leaving jiffies frozen. Enable STIE so the pending
                        // STIP actually becomes takeable.
                        self.mie |= 1 << 5; // STIE
                    }
                    self.cpu.write_reg(10, ret.error as u64);
                    self.cpu.write_reg(11, ret.value as u64);
                    self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                    return Status::Running;
                }
                let cause = match self.priv_level {
                    Privilege::User => Exception::EnvironmentCallFromU,
                    Privilege::Supervisor => Exception::EnvironmentCallFromS,
                    Privilege::Machine => Exception::EnvironmentCallFromM,
                };
                return self.take_trap(Trap::Exception(cause));
            }
            Ebreak => {
                // Take a real Breakpoint trap so the kernel's do_trap_breakpoint
                // distinguishes WARN_ON (warning, continues) from BUG() (panic).
                // Skipping the ebreak (pc += 2) masked fatal BUG() panics and turned
                // them into trap storms. (setup_smp BUG is patched to c.nop in the test.)
                return self.take_trap_with_vaddr(Trap::Exception(Exception::Breakpoint), None);
            }
            Mret => return self.do_mret(),
            Sret => {
                // mstatus.TSR traps SRET taken in S-mode.
                if self.priv_level == Privilege::Supervisor && self.mstatus.tsr {
                    return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
                }
                return self.do_sret();
            }
            Wfi => {
                // mstatus.TW traps WFI taken in S-mode.
                if self.priv_level == Privilege::Supervisor && self.mstatus.tw {
                    return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
                }
                // The batched device poll may have skipped a just-arrived
                // interrupt; a sleeping guest must see current state, so force a
                // fresh sync and take the trap if one is now deliverable rather
                // than parking on it.
                self.sync_device_interrupts(bus);
                self.int_sync_countdown = INT_SYNC_INTERVAL;
                if let Some(trap) = self.check_interrupts() {
                    return self.take_trap(trap);
                }
                self.wfi = true;
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Wfi;
            }
            SfenceVma { rs1: sf_rs1, rs2: sf_rs2 } => {
                // mstatus.TVM traps SFENCE.VMA taken in S-mode. Along with TSR
                // and TW below these are the privileged-virtualisation traps: a
                // hypervisor running the OS in S-mode sets them so it can
                // intercept memory-management and sleep operations. They were
                // decoded into MStatus and then never consulted.
                if self.priv_level == Privilege::Supervisor && self.mstatus.tvm {
                    return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
                }
                // rs1 names a virtual address and rs2 an ASID, with x0
                // meaning "all". Ignoring rs1 and flushing globally is correct
                // -- flushing more than asked never breaks a guest -- but it
                // cost the guest every cached translation it had, 139.6k times
                // per 400M instructions, which was 93% of all inlined-TLB
                // misses. rs2 is still ignored: without ASIDs there is only one
                // address space to flush.
                self.gen_bump[if sf_rs1 != 0 { 2 } else { 1 }] += 1;
                let _ = sf_rs2;
                // fetch-page cache: a mapping just changed (single entry, drop
                // unconditionally rather than compare against the flushed page).
                self.fetch_vpn = u64::MAX;
                // The software TLB is 256 entries and cheap to refill, so it
                // is still flushed wholesale -- the expensive cache is the
                // inlined one the compiled code probes, and that is the one
                // worth being precise about.
                self.mmu.flush_tlb();
                if sf_rs1 != 0 {
                    // rs1 names one virtual address. Queue just that page;
                    // every other cached translation stays live — and the
                    // chain/vcache generation does NOT move here. The host
                    // drains this queue before any compiled code can run, and
                    // bumps `trans_gen` only when the page might actually hold
                    // a chain key (Jit::page_may_have_keys). Nearly all of
                    // these name DATA pages — allocator munmaps, 147k per real
                    // Python session — and each one used to wipe all block
                    // chaining (~760 chain stops per flush, measured).
                    let va = self.read_reg(sf_rs1);
                    let n = self.pending_flush_n as usize;
                    if n < self.pending_flush.len() {
                        self.pending_flush[n] = va;
                        self.pending_flush_n += 1;
                    } else {
                        // Queue full: fall back to invalidating everything,
                        // which is what this code did unconditionally before.
                        // That must now include every remembered space, because
                        // the dropped page never reaches the host's filter.
                        self.pending_flush_n = 0;
                        self.invalidate_all_spaces();
                    }
                } else {
                    // Global sfence.vma. Conservatively drops every remembered
                    // space (rs2/asid is not honoured individually): this is the
                    // event Linux issues on ASID-generation rollover, when a
                    // hardware ASID + page-table root can start naming a
                    // different address space, so a stale saved generation must
                    // not survive it. ~4x rarer than satp switches.
                    self.invalidate_all_spaces();
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Running;
            }
            // LR/SC reservation tracking
            Lrw { rd, rs1, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                let paddr = match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(p) => p,
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                };
                let val = bus.read_u32(paddr) as i32 as u64;
                self.cpu.write_reg(rd, val);
                self.reservation_addr = Some(vaddr & !0x3);
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Running;
            }
            Scw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                let paddr = match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(p) => p,
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                };
                if self.reservation_addr == Some(vaddr & !0x3) {
                    bus.write_u32(paddr, self.read_reg(rs2) as u32);
                    self.cpu.write_reg(rd, 0);
                } else {
                    self.cpu.write_reg(rd, 1);
                }
                self.reservation_addr = None;
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Running;
            }
            Lrd { rd, rs1, .. } => {
                let vaddr = self.read_reg(rs1);
                let paddr = match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(p) => p,
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                };
                let val = bus.read_u64(paddr);
                self.cpu.write_reg(rd, val);
                self.reservation_addr = Some(vaddr & !0x7);
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Running;
            }
            Scd { rd, rs1, rs2, .. } => {
                let vaddr = self.read_reg(rs1);
                let paddr = match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(p) => p,
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                };
                if self.reservation_addr == Some(vaddr & !0x7) {
                    bus.write_u64(paddr, self.read_reg(rs2));
                    self.cpu.write_reg(rd, 0);
                } else {
                    self.cpu.write_reg(rd, 1);
                }
                self.reservation_addr = None;
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
                return Status::Running;
            }
            // Atomics: pass through to base CPU after translation
            // All other instructions (ALU/branch/jump/load/store) fall through below.
            _ => {}
        }

        // Dispatch: CSR/system already handled above.
        // Memory ops (load/store/atomic) need translation.
        // ALU/branch/jump don't touch memory.
        // mstatus.FS == Off disables the whole F/D extension: every FP
        // instruction, loads and stores included, raises IllegalInstruction.
        // Linux relies on this for lazy FP context switching — it leaves FS Off
        // until a task actually touches FP, and only then saves and restores the
        // registers. Honouring FP unconditionally would let one task read
        // another's FP state.
        if self.mstatus.fs == 0 && Self::is_fp(&instr) {
            return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
        }
        if self.trace_enabled {
            self.record_trace(self.cpu.pc, self.last_fetched_raw);
        }
        let status = if Self::is_simple_load_store(&instr) || Self::is_atomic(&instr) {
            self.execute_translated(instr, width, bus)
        } else {
            // Non-memory instruction: ALU, branch, jump, fences, etc.
            self.cpu.execute_width(instr, width, bus)
        };
        // An instruction that wrote an f register moves mstatus.FS to Dirty.
        // Linux checks FS on context switch to decide whether the outgoing
        // task's FP registers need saving; if it never goes Dirty the state is
        // silently dropped and floating point results become nondeterministic
        // across a reschedule.
        if self.cpu.fs_dirty {
            self.cpu.fs_dirty = false;
            self.mstatus.fs = 3;
        }
        status
    }

    fn read_reg(&self, i: u8) -> u64 {
        self.cpu.read_reg(i)
    }

    /// The CSR op as compiled code needs it: perform the read/modify/write and
    /// the rd write, but do NOT advance the PC — a compiled block owns the PC
    /// and sets it once at the end.
    ///
    /// Returns true if the instruction would trap. On that path NOTHING is
    /// changed: the checks all precede any mutation, exactly as in `csr_op`, so
    /// the block can bail and let the interpreter re-execute the CSR and take
    /// the trap through `take_trap` in the ordinary way. Calling `take_trap`
    /// here instead would apply the trap twice — once compiled, once
    /// interpreted.
    ///
    /// `kind`: 0 = CSRRW(I) (write, no read-set), 1 = CSRRS(I) (set), 2 =
    /// CSRRC(I) (clear). `src` is the register number or the zero-extended
    /// immediate; `val` is its value. This mirrors `csr_op` and the two must
    /// stay in step.
    pub fn csr_jit(&mut self, bus: &mut dyn Bus, csr: u16, rd: u8, src: u8, val: u64, kind: u8) -> bool {
        let read = kind != 0;
        let clear = kind == 2;

        let csr_priv = ((csr >> 8) & 0x3) as u8;
        if (self.priv_level as u8) < csr_priv {
            return true;
        }
        if csr == 0x180 && self.priv_level == Privilege::Supervisor && self.mstatus.tvm {
            return true;
        }

        let will_write = !read || src != 0;
        if will_write && (csr >> 10) & 0x3 == 0b11 {
            return true;
        }

        let old = self.csr_read(bus, csr);
        let new = if clear {
            old & !val
        } else if read {
            old | val
        } else {
            val
        };
        if will_write {
            self.csr_write(csr, new);
        }
        self.cpu.write_reg(rd, old);
        false
    }

    /// Execute one FP instruction for compiled code: arithmetic via the FPU,
    /// or an FP load/store. Reuses the interpreter's own paths, so the math
    /// cannot diverge from a `step()` of the same instruction. Returns true to
    /// bail: the interpreter re-runs the instruction at its own PC and takes
    /// the trap the ordinary way, so nothing here calls take_trap and nothing
    /// commits on the bail path.
    ///
    /// `kind`: 0 = fld, 1 = flw, 2 = fsd, 3 = fsw, 4 = arithmetic (`arg` is
    /// the raw encoding). For loads r1 = rd and r2 = rs1; for stores r1 = rs2
    /// (the data f-register) and r2 = rs1 (the base).
    /// Is the task's FP state already Dirty? Compiled FP fast paths are only
    /// valid in that state; see `Jit::fs_word`.
    pub fn fs_is_dirty(&self) -> bool {
        self.mstatus.fs == 3
    }

    pub fn fp_jit(&mut self, bus: &mut dyn Bus, kind: u8, r1: u8, r2: u8, arg: u64) -> bool {
        // mstatus.FS == Off: every FP instruction, loads and stores included,
        // must raise IllegalInstruction. Linux's lazy FP context switching
        // depends on that trap; honouring FP here regardless would let one
        // task read another's registers.
        if self.mstatus.fs == 0 {
            return true;
        }
        match kind {
            0 => {
                let vaddr = (self.read_reg(r2) as i64).wrapping_add(arg as i64) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => {
                        let v = bus.read_u64(paddr);
                        self.cpu.write_freg(r1, v);
                        self.cpu.fs_dirty = true;
                    }
                    Err(_) => return true,
                }
            }
            1 => {
                let vaddr = (self.read_reg(r2) as i64).wrapping_add(arg as i64) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => {
                        // NaN-boxed, as everywhere else a single lands in an
                        // f-register.
                        self.cpu.write_freg(r1, 0xFFFF_FFFF_0000_0000 | bus.read_u32(paddr) as u64);
                        self.cpu.fs_dirty = true;
                    }
                    Err(_) => return true,
                }
            }
            2 => {
                let vaddr = (self.read_reg(r2) as i64).wrapping_add(arg as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => bus.write_u64(paddr, self.cpu.read_freg(r1)),
                    Err(_) => return true,
                }
            }
            3 => {
                let vaddr = (self.read_reg(r2) as i64).wrapping_add(arg as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => bus.write_u32(paddr, self.cpu.read_freg(r1) as u32),
                    Err(_) => return true,
                }
            }
            _ => {
                let r = riscv_core::fpu::execute(&mut self.cpu, arg as u32);
                if !r.ok {
                    return true;
                }
                self.cpu.fcsr |= r.flags;
                self.cpu.fs_dirty |= r.dirty;
            }
        }
        // step()'s epilogue: an f-register write moves mstatus.FS to Dirty, so
        // the kernel knows the outgoing task's FP state needs saving.
        if self.cpu.fs_dirty {
            self.cpu.fs_dirty = false;
            self.mstatus.fs = 3;
        }
        false
    }

    fn csr_op(&mut self, bus: &mut dyn Bus, csr: u16, rd: u8, _rs1: u8, val: u64, read: bool, clear: bool, width: u8) -> Status {
        // Check CSR access permissions
        let csr_priv = ((csr >> 8) & 0x3) as u8;
        let priv_lvl = self.priv_level as u8;
        if priv_lvl < csr_priv {
            return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
        }

        // mstatus.TVM also traps satp accesses made from S-mode, reads
        // included, so a hypervisor sees every attempt to inspect or change the
        // guest page table root.
        if csr == 0x180 && self.priv_level == Privilege::Supervisor && self.mstatus.tvm {
            return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
        }

        // Does this instruction actually write? CSRRW/CSRRWI always do; CSRRS,
        // CSRRC and their immediate forms only when the source is not x0/zero.
        // `_rs1` carries zimm for the immediate variants, so one test covers
        // both. Using the VALUE instead of the register number was wrong: a
        // csrrs whose register happens to hold 0 still performs a write.
        let will_write = !read || _rs1 != 0;

        // csr[11:10] == 0b11 marks a read-only CSR. Writing one raises
        // IllegalInstruction — that is how `csrrw a0, cycle, x0` is meant to
        // fail, and the check has to happen BEFORE the read so a bad write does
        // not return a value.
        if will_write && (csr >> 10) & 0x3 == 0b11 {
            return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
        }

        let old = self.csr_read(bus, csr);
        let new = if clear {
            old & !val          // CSRRC
        } else if read {
            old | val            // CSRRS
        } else {
            val                   // CSRRW
        };

        if will_write {
            self.csr_write(csr, new);
        }

        self.cpu.write_reg(rd, old);
        self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
        Status::Running
    }

    fn csr_read(&mut self, bus: &mut dyn Bus, csr: u16) -> u64 {
        match csr {
            // Floating-point CSRs. fcsr packs fflags in [4:0] and the rounding
            // mode in [7:5]; fflags and frm are windows onto the same register.
            0x001 => self.cpu.fcsr & 0x1f,
            0x002 => (self.cpu.fcsr >> 5) & 0x7,
            0x003 => self.cpu.fcsr & 0xff,

            // M-mode CSRs
            0x300 => self.mstatus.to_bits(),
            0x301 => 0x800000000094112D, // misa: RV64IMAFDC
            0x302 => self.medeleg,
            0x303 => self.mideleg,
            0x304 => self.mie,
            0x305 => self.mtvec,
            0x306 => 0, // mcounteren
            0x340 => self.mscratch,
            0x341 => self.mepc,
            0x342 => self.mcause,
            0x343 => self.mtval,
            0x344 => self.mip,
            0x3A0..=0x3AF => self.pmpcfg[(csr - 0x3A0) as usize],
            0x3B0..=0x3EF => self.pmpaddr[(csr - 0x3B0) as usize],
            0xB00 => self.mcycle,
            0xB02 => self.minstret,
            0xF11 => 0, // mvendorid
            0xF12 => 0, // marchid
            0xF13 => 0, // mimpid
            0xF14 => self.mhartid,

            // S-mode CSRs
            0x100 => self.mstatus.to_bits(),
            0x104 => self.mie & 0x222, // sie: S-mode view of mie (SSIE/STIE/SEIE)
            0x105 => self.stvec,
            0x106 => 0, // scounteren
            0x140 => self.sscratch,
            0x141 => self.sepc,
            0x142 => self.scause,
            0x143 => self.stval,
            0x144 => self.mip & 0x222, // sip: S-mode view of mip
            0x14D => self.stimecmp,
            0x180 => self.satp.to_bits(),

            // User-visible counter CSRs (read-only aliases of M-mode counters)
            0xC01 => { let v = bus.read_mtime(); self.dbg_last_time = v; self.dbg_time_reads += 1; v },  // time
            0xC00 => self.mcycle,       // cycle
            0xC02 => self.minstret,     // instret
            0xC80 => 0,                 // cycleh (RV32 only)
            0xC81 => 0,                 // timeh (RV32 only)
            0xC82 => 0,                 // instreth (RV32 only)

            _ => {
                // Unknown CSR — read as 0 for forward compatibility
                0
            }
        }
    }

    fn csr_write(&mut self, csr: u16, val: u64) {
        match csr {
            // Writing any FP CSR counts as dirtying FP state.
            0x001 => {
                self.cpu.fcsr = (self.cpu.fcsr & !0x1f) | (val & 0x1f);
                self.mstatus.fs = 3;
            }
            0x002 => {
                self.cpu.fcsr = (self.cpu.fcsr & !0xe0) | ((val & 0x7) << 5);
                self.mstatus.fs = 3;
            }
            0x003 => {
                self.cpu.fcsr = val & 0xff;
                self.mstatus.fs = 3;
            }
            0x300 => {
                self.mstatus.from_bits(val);
                // UXL/SXL are WARL, hardcode to 64-bit
                self.mstatus.uxl = 2;
                self.mstatus.sxl = 2;
            }
            0x302 => self.medeleg = val,
            0x303 => self.mideleg = val,
            0x304 => self.mie = val,
            0x305 => self.mtvec = val,
            0x340 => self.mscratch = val,
            0x341 => { self.mepc = val; if self.satp.mode == 8 && Self::is_phys_kernel(val) { self.record_phys_transition(self.cpu.pc, val); } }
            0x342 => self.mcause = val,
            0x343 => self.mtval = val,
            0x344 => self.mip = val,
            // On RV64 a pmpaddr holds a 54-bit address field; the top ten
            // bits are hardwired zero and reads must show that.
            0x3A0..=0x3AF => self.pmpcfg[(csr - 0x3A0) as usize] = val,
            0x3B0..=0x3EF => self.pmpaddr[(csr - 0x3B0) as usize] = val & 0x003F_FFFF_FFFF_FFFF,
            0xB00 => { self.mcycle = val; self.wrote_mcycle = true; }
            0xB02 => { self.minstret = val; self.wrote_minstret = true; }
            0x100 => {
                // sstatus is a RESTRICTED VIEW of mstatus, not an alias. Only
                // SIE, SPIE, SPP, VS, FS, XS, SUM, MXR, UXL and SD are writable
                // through it; MIE/MPIE/MPP/MPRV/TW/TVM/TSR must survive
                // untouched. Passing the raw value to from_bits let every
                // `csrw sstatus` in the kernel's irq save/restore path quietly
                // rewrite M-mode state.
                const SSTATUS_MASK: u64 = (1 << 63)   // SD
                    | (0b11 << 32)                    // UXL
                    | (1 << 19) | (1 << 18)           // MXR, SUM
                    | (0b11 << 15) | (0b11 << 13)     // XS, FS
                    | (0b11 << 9)                     // VS
                    | (1 << 8)                        // SPP
                    | (1 << 5)                        // SPIE
                    | (1 << 1); // SIE
                let merged = (self.mstatus.to_bits() & !SSTATUS_MASK) | (val & SSTATUS_MASK);
                self.mstatus.from_bits(merged);
            }
            0x104 => self.mie = (self.mie & !0x222) | (val & 0x222), // sie: only S-mode bits
            0x105 => self.stvec = val,
            0x140 => self.sscratch = val,
            0x141 => { self.sepc = val; if self.satp.mode == 8 && Self::is_phys_kernel(val) { self.record_phys_transition(self.cpu.pc, val); } }
            0x142 => self.scause = val,
            0x143 => self.stval = val,
            0x144 => self.mip = (self.mip & !0x222) | (val & 0x222), // sip: only S-mode bits
            0x14D => { self.stimecmp = val; }
            0x180 => {
                let old = self.satp.to_bits();
                self.satp.from_bits(val);
                // Any satp write installs a new page-table root. Even when mode and
                // ASID are unchanged (e.g. early_pg_dir -> swapper_pg_dir switch with
                // Sv39 + asid 0), prior translations are invalid: flush the TLB.
                // Without this, stale entries from the early tables survive and the
                // boot hart fetches garbage -> trap storm. (sfence.vma also flushes.)
                if old != val {
                    self.gen_bump[0] += 1;
                    // The software TLB is vpn-keyed, not ASID-keyed, and cheap
                    // (256 entries), so it is still flushed wholesale.
                    self.mmu.flush_tlb();
                    self.fetch_vpn = u64::MAX; // fetch-page cache: new page-table root
                    // But the expensive caches the compiled code probes — chain
                    // table, vcache, inlined data TLB — are keyed by the
                    // generation, so instead of bumping (which would void them
                    // all), select this address space's generation. Switching
                    // back to a process revalidates its chain for free.
                    self.switch_address_space(old, val);
                }
            }
            _ => {} // Ignore unknown CSRs
        }
    }

    /// Is an interrupt pending right now?
    ///
    /// Syncs the timer and external interrupt bits into `mip` — the same first
    /// steps `step` takes — then reports whether one would be taken, WITHOUT
    /// taking it. The JIT run loop calls this at chain boundaries: compiled
    /// code never runs `step`, so without this an interrupt is delivered only
    /// when some uncompilable instruction happens to force an interpreter step.
    /// That accident held until CSRs became compilable, at which point a purely
    /// compiled `csrr sstatus`/branch wait-loop spun forever waiting for a
    /// timer tick that the run loop never let in.
    pub fn interrupt_pending(&mut self, bus: &mut dyn Bus) -> bool {
        self.sync_device_interrupts(bus);
        self.check_interrupts().is_some()
    }

    /// Poll the CLINT timer and PLIC and reflect them into `mip` (STIP bit 5,
    /// SEIP bit 9). This is the expensive half of an interrupt check — three
    /// device reads — and it is the interpreter's single most-repeated cost.
    ///
    /// `step` batches it behind `int_sync_countdown` rather than doing it every
    /// instruction; the run loop's `interrupt_pending` refreshes it fresh at
    /// every block boundary, and WFI forces it before parking, so delivery
    /// always sees current state. Between those points a stale window is pure
    /// interrupt latency — bounded by `INT_SYNC_INTERVAL`, far under the
    /// compiled path's chain budget, and de-assertion (a handler writing
    /// mtimecmp or claiming the PLIC) happens with interrupts masked and is
    /// re-evaluated at the `sret` boundary before delivery resumes.
    pub fn sync_device_interrupts(&mut self, bus: &mut dyn Bus) {
        let timer_fired = bus.check_timer_interrupt() || (bus.read_mtime() >= self.stimecmp);
        if timer_fired {
            self.mip |= 1 << 5;
        } else {
            self.mip &= !(1 << 5);
        }
        if bus.check_external_interrupt() {
            self.mip |= 1 << 9;
        } else {
            self.mip &= !(1 << 9);
        }
    }

    fn check_interrupts(&self) -> Option<Trap> {
        if self.wfi {
            // WFI wakes on any enabled pending interrupt; fall through to check
        }

        // Per RISC-V priv spec 3.1.9:
        // - Globally enabled interrupts see individual enable bits (mie/sie)
        // - Delegated interrupts are suppressed in M-mode and appear in the
        //   delegated privilege (S/U)
        let m_enabled = self.priv_level != Privilege::Machine || self.mstatus.mie;
        let s_enabled = self.priv_level == Privilege::User
                     || (self.priv_level == Privilege::Supervisor && self.mstatus.sie);

        let pending = self.mip & self.mie;
        if pending == 0 {
            return None;
        }

        // Compute delegated and non-delegated pending bits
        let delegated = pending & self.mideleg;
        let m_pending = pending & !self.mideleg; // non-delegated only

        // S-mode delegated interrupts
        if s_enabled {
            if delegated != 0 && self.priv_level != Privilege::Machine {
                if (delegated & (1 << 7)) != 0 {
                    // Delegated MTIP → SupervisorTimer
                    return Some(Trap::Interrupt(Interrupt::SupervisorTimer));
                }
                if (delegated & (1 << 5)) != 0 { // STIP
                    return Some(Trap::Interrupt(Interrupt::SupervisorTimer));
                }
                if (delegated & (1 << 1)) != 0 { // SSIP
                    return Some(Trap::Interrupt(Interrupt::SupervisorSoftware));
                }
                if (delegated & (1 << 9)) != 0 { // SEIP
                    return Some(Trap::Interrupt(Interrupt::SupervisorExternal));
                }
            }
        }

        // M-mode interrupts. This must cover the non-delegated SUPERVISOR bits
        // too: when a mideleg bit is clear, that S-mode interrupt is taken in
        // M-mode rather than dropped. Only the three machine bits were handled,
        // so an undelegated SSIP never fired at all — which is what left
        // rv64mi-p-illegal spinning in test_vectored_interrupts.
        //
        // Order is the priority the spec defines: MEI, MSI, MTI, SEI, SSI, STI.
        if m_enabled {
            if (m_pending & (1 << 11)) != 0 {
                return Some(Trap::Interrupt(Interrupt::MachineExternal));
            }
            if (m_pending & (1 << 3)) != 0 {
                return Some(Trap::Interrupt(Interrupt::MachineSoftware));
            }
            if (m_pending & (1 << 7)) != 0 {
                return Some(Trap::Interrupt(Interrupt::MachineTimer));
            }
            if (m_pending & (1 << 9)) != 0 {
                return Some(Trap::Interrupt(Interrupt::SupervisorExternal));
            }
            if (m_pending & (1 << 1)) != 0 {
                return Some(Trap::Interrupt(Interrupt::SupervisorSoftware));
            }
            if (m_pending & (1 << 5)) != 0 {
                return Some(Trap::Interrupt(Interrupt::SupervisorTimer));
            }
        }

        None
    }

    /// Read the upper half of a 32-bit instruction whose lower half is at
    /// `lo_paddr`, translating separately when the instruction crosses a page
    /// boundary. Only pc & 0xFFF == 0xFFE can straddle, since instructions are
    /// 2-byte aligned.
    fn fetch_upper_half(&mut self, bus: &mut dyn Bus, lo_paddr: u64) -> Result<u16, Trap> {
        if self.cpu.pc & 0xFFF != 0xFFE {
            return Ok(bus.read_u16(lo_paddr + 2));
        }
        let hi_vaddr = self.cpu.pc.wrapping_add(2);
        let hi_paddr = self.translate(bus, AccessType::Instruction, hi_vaddr)?;
        Ok(bus.read_u16(hi_paddr))
    }

    fn take_trap(&mut self, trap: Trap) -> Status {
        self.take_trap_with_vaddr(trap, None)
    }

    fn take_trap_with_vaddr(&mut self, trap: Trap, fault_vaddr: Option<u64>) -> Status {
        // Delivering a trap overwrites sepc/scause/stval and redirects pc to stvec.
        // Doing it twice for one instruction therefore records sepc = stvec and
        // stval = 0, which the kernel reads back as "fault at handle_exception+0x0,
        // badaddr 0" — a NULL-deref Oops for what was really an ordinary demand-paging
        // fault. `step()` consults this flag so it only delivers the *bare* traps that
        // come back from `cpu.execute_width`.
        self.trap_delivered = true;
        let cause = match trap {
            Trap::Exception(e) => e as u64,
            Trap::Interrupt(i) => 0x80000000_00000000 | (i as u64),
        };

        // Check delegation to S-mode
        let delegate = match trap {
            Trap::Exception(e) => match e {
                Exception::InstructionAddressMisaligned => (self.medeleg >> 0) & 1 != 0,
                Exception::InstructionAccessFault       => (self.medeleg >> 1) & 1 != 0,
                Exception::IllegalInstruction           => (self.medeleg >> 2) & 1 != 0,
                Exception::Breakpoint                   => (self.medeleg >> 3) & 1 != 0,
                Exception::LoadAddressMisaligned        => (self.medeleg >> 4) & 1 != 0,
                Exception::LoadAccessFault              => (self.medeleg >> 5) & 1 != 0,
                Exception::StoreAddressMisaligned       => (self.medeleg >> 6) & 1 != 0,
                Exception::StoreAccessFault             => (self.medeleg >> 7) & 1 != 0,
                Exception::EnvironmentCallFromU         => (self.medeleg >> 8) & 1 != 0,
                Exception::EnvironmentCallFromS         => (self.medeleg >> 9) & 1 != 0,
                Exception::InstructionPageFault         => (self.medeleg >> 12) & 1 != 0,
                Exception::LoadPageFault                => (self.medeleg >> 13) & 1 != 0,
                Exception::StorePageFault               => (self.medeleg >> 15) & 1 != 0,
                _ => false,
            },
            Trap::Interrupt(i) => {
                let bit = match i {
                    Interrupt::SupervisorSoftware => 1,
                    Interrupt::SupervisorTimer => 5,
                    Interrupt::SupervisorExternal => 9,
                    _ => 64, // Not delegated
                };
                // SupervisorTimer may also come from delegated MTIP (bit 7)
                if bit < 64 {
                    let base_deleg = (self.mideleg >> bit) & 1 != 0;
                    let timer_deleg = matches!(i, Interrupt::SupervisorTimer) && (self.mideleg >> 7) & 1 != 0;
                    base_deleg || timer_deleg
                } else { false }
            }
        };

        if delegate && (self.priv_level == Privilege::Supervisor || self.priv_level == Privilege::User) {
            // Snapshot the register file so the matching sret can verify the
            // kernel handed everything back untouched. Only for interrupts:
            // exceptions legitimately alter registers (syscall returns, signal
            // delivery, page-fault fixups).
            if self.check_irq_regs && matches!(trap, Trap::Interrupt(_)) {
                self.irq_snaps += 1;
                self.irq_snapshot =
                    Some((self.cpu.pc, self.satp.ppn, self.cpu.x[2], self.cpu.x));
            }
            // Trap to S-mode
            self.sepc = self.cpu.pc;
            self.scause = cause;
            let is_page_fault = matches!(trap, Trap::Exception(Exception::InstructionPageFault | Exception::LoadPageFault | Exception::StorePageFault));
            self.stval = if is_page_fault { fault_vaddr.unwrap_or(0) } else { 0 };
            self.mstatus.spp = self.priv_level == Privilege::Supervisor;
            self.mstatus.spie = self.mstatus.sie;
            self.mstatus.sie = false;
            self.priv_level = Privilege::Supervisor;
            self.last_trap_cause = cause;
            self.last_trap_epc = self.cpu.pc; // capture original epc BEFORE overwriting pc
            self.cpu.pc = self.stvec & !1; // strip the MODE bit
            // Clear pending interrupt bit
            if let Trap::Interrupt(i) = trap {
                match i {
                    Interrupt::SupervisorSoftware => self.mip &= !(1 << 1),
                    Interrupt::SupervisorTimer    => {
                        self.mip &= !(1 << 5);
                        self.mip &= !(1 << 7); // Also clear delegated MTIP
                    }
                    Interrupt::SupervisorExternal => self.mip &= !(1 << 9),
                    _ => {}
                }
            }
        } else {
            // Trap to M-mode
            self.mepc = self.cpu.pc;
            self.mcause = cause;
            // Mirror into the S-mode CSRs (sepc/scause) as well. When a trap is
            // NOT delegated to S-mode it still lands at mtvec, but the Linux kernel
            // (and our debug logs) read sepc/scause; leaving them zero causes a
            // spurious re-entry loop (sepc=0 scause=0) instead of the real fault.
            self.sepc = self.cpu.pc;
            self.scause = cause;
            let is_page_fault = matches!(trap, Trap::Exception(Exception::InstructionPageFault | Exception::LoadPageFault | Exception::StorePageFault));
            self.mtval = if is_page_fault { fault_vaddr.unwrap_or(0) } else { 0 };
            self.mstatus.mpp = self.priv_level as u8;
            self.mstatus.mpie = self.mstatus.mie;
            self.mstatus.mie = false;
            self.priv_level = Privilege::Machine;
            self.last_trap_cause = cause;
            self.last_trap_epc = self.cpu.pc; // capture original epc
            self.cpu.pc = self.mtvec & !1; // strip the MODE bit
        }

        // tvec MODE = 1 is vectored, and it applies to INTERRUPTS ONLY:
        // interrupts go to BASE + 4 * cause, exceptions always go to BASE. This
        // was inverted, vectoring exceptions and sending interrupts to BASE,
        // which is what test_vectored_interrupts in rv64mi-p-illegal catches.
        let is_interrupt = (cause >> 63) != 0;
        if is_interrupt {
            let vec = if self.priv_level == Privilege::Machine { self.mtvec } else { self.stvec };
            if vec & 1 != 0 {
                let code = cause & 0x7FFF_FFFF_FFFF_FFFF;
                self.cpu.pc = (vec & !1).wrapping_add(4 * code);
            }
        }

        self.wfi = false;
        Status::Trap(trap)
    }

    fn do_mret(&mut self) -> Status {
        let mpie = self.mstatus.mpie;
        let mpp = self.mstatus.mpp;
        self.mstatus.mie = mpie;
        self.mstatus.mpie = true;
        self.mstatus.mpp = 0; // U-mode
        self.priv_level = match mpp {
            0 => Privilege::User,
            1 => Privilege::Supervisor,
            3 => Privilege::Machine,
            _ => Privilege::User,
        };
        let target = self.mepc & !1;
        if Self::is_phys_kernel(target) && self.satp.mode == 8 {
            self.record_phys_transition(self.cpu.pc, target);
        }
        self.cpu.pc = target;
        Status::Running
    }

    fn do_sret(&mut self) -> Status {
        // Interrupt-return invariant check.
        //
        // A plain interrupt return must restore the full register file: same
        // sepc, same satp and same sp means the same task resuming at the same
        // instruction, so every other register has to match what it was when
        // the interrupt was taken. If one does not, either the kernel's
        // save/restore ran wrong (which means we mis-executed part of it) or
        // we clobbered a register delivering the trap.
        //
        // Context switches are excluded by the sepc/satp/sp triple: a switch
        // resumes a different task, at a different pc, on a different stack.
        if self.check_irq_regs && self.irq_mismatch.is_none() {
            if let Some((sepc, satp_ppn, sp, regs)) = self.irq_snapshot {
                if (self.sepc & !1) == sepc && self.satp.ppn == satp_ppn && self.cpu.x[2] == sp {
                    self.irq_compares += 1;
                    for i in 1..32usize {
                        if self.cpu.x[i] != regs[i] {
                            self.irq_mismatch = Some((sepc, i as u8, regs[i], self.cpu.x[i]));
                            break;
                        }
                    }
                }
                self.irq_snapshot = None;
            }
        }

        let spie = self.mstatus.spie;
        let spp = self.mstatus.spp;
        self.mstatus.sie = spie;
        self.mstatus.spie = true;
        self.mstatus.spp = false;
        self.priv_level = if spp { Privilege::Supervisor } else { Privilege::User };
        let target = self.sepc & !1;
        if Self::is_phys_kernel(target) && self.satp.mode == 8 {
            self.record_phys_transition(self.cpu.pc, target);
        }
        self.cpu.pc = target;
        Status::Running
    }

    // Translated memory helpers (for use within supervisor execute)

}


// ---- Translated memory instruction execution ----
// These replace the base Cpu::execute for load/store operations when MMU is active.
// All other instructions (ALU, branch, etc.) go through cpu.execute normally.

impl Supervisor {
    /// Execute with full address translation.
    /// This is the main dispatch that handles both privileged and memory ops.
    /// Remember a 64-bit store, so that when the kernel hits a BUG() we can ask
    /// "who last wrote the pointer it choked on?".
    ///
    /// Off unless `enable_store_trace` was called; the ring is a plain Vec index
    /// so the cost when disabled is one predictable branch.
    /// Does this pc match the watched instruction set?
    ///
    /// `watch_pcoff` holds page offsets, because a module's load address moves
    /// between boots but its page offsets do not. Offsets alone are ambiguous
    /// though — a 1 MB module has 256 pages and some far hotter function will
    /// share the same offset and drown the buffer. So the FIRST entry is an
    /// anchor: nothing is recorded until an instruction at that offset runs,
    /// and its page is then latched. Afterwards only that page counts.
    fn pcoff_admits(&mut self, pc: u64) -> bool {
        // The pc range gates the anchor too, otherwise the first matching
        // offset anywhere in vmlinux latches the wrong page.
        if pc < self.watch_pc_lo || pc > self.watch_pc_hi {
            return false;
        }
        let off = pc & 0xFFF;
        if !self.watch_pcoff.iter().any(|&o| o == off) {
            return false;
        }
        match self.watch_page {
            0 => {
                if off != self.watch_pcoff[0] {
                    return false;
                }
                self.watch_page = pc & !0xFFF;
                true
            }
            page => pc & !0xFFF == page,
        }
    }

    /// Same filter as record_store, but for values coming OUT of memory.
    /// A load that disagrees with the last store to the same address is the
    /// emulator returning wrong data, which is the one thing that would explain
    /// correct data plus correct code producing a wrong answer.
    #[inline]
    fn record_load(&mut self, vaddr: u64, val: u64) {
        if self.watch_pcoff[0] == u64::MAX || !self.pcoff_admits(self.cpu.pc) {
            return;
        }
        if self.watch_loads.len() < 65536 {
            self.watch_loads.push((self.cpu.pc, vaddr, val));
        }
    }

    #[inline]
    fn record_store(&mut self, vaddr: u64, val: u64) {
        let pcoff_on = self.watch_pcoff[0] != u64::MAX;
        if pcoff_on && !self.pcoff_admits(self.cpu.pc) {
            return;
        }
        if (self.watch_mask != 0 || pcoff_on)
            && (self.watch_mask == 0 || val & self.watch_mask == self.watch_expect)
            && (!self.watch_outside_nonzero || val & !self.watch_mask != 0)
            && self.cpu.pc >= self.watch_pc_lo
            && self.cpu.pc <= self.watch_pc_hi
            && self.watch_hits.len() < 65536
        {
            self.watch_hits.push((self.cpu.pc, vaddr, val));
        }
        if self.store_ring.is_empty() {
            return;
        }
        let i = self.store_head % self.store_ring.len();
        self.store_ring[i] = (self.cpu.pc, vaddr, val);
        self.store_head = self.store_head.wrapping_add(1);
    }

    /// Turn on store tracing with `capacity` entries.
    pub fn enable_store_trace(&mut self, capacity: usize) {
        self.store_ring = alloc::vec![(0u64, 0u64, 0u64); capacity];
        self.store_head = 0;
    }

    /// Every recorded store to `vaddr`, oldest first, as (pc, value).
    pub fn stores_to(&self, vaddr: u64) -> Vec<(u64, u64)> {
        let n = self.store_ring.len();
        if n == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Walk from oldest to newest.
        for k in 0..n {
            let i = (self.store_head + k) % n;
            let (pc, a, v) = self.store_ring[i];
            if a == vaddr && pc != 0 {
                out.push((pc, v));
            }
        }
        out
    }

    pub fn execute_translated(&mut self, instr: Instr, width: u8, bus: &mut dyn Bus) -> Status {
        use Instr::*;

        match instr {
            // I-Type Load
            Lb { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_reg(rd, bus.read_u8(paddr) as i8 as i64 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Lh { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_reg(rd, bus.read_u16(paddr) as i16 as i64 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Lw { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_reg(rd, bus.read_u32(paddr) as i32 as i64 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Ld { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { let v = bus.read_u64(paddr); self.record_load(vaddr, v); self.cpu.write_reg(rd, v); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Lbu { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_reg(rd, bus.read_u8(paddr) as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Lhu { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_reg(rd, bus.read_u16(paddr) as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Lwu { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { let v = bus.read_u32(paddr) as u64; self.record_load(vaddr, v); self.cpu.write_reg(rd, v); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            // S-Type Store
            Sb { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { bus.write_u8(paddr, self.read_reg(rs2) as u8); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Sh { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { bus.write_u16(paddr, self.read_reg(rs2) as u16); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Sw { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => {
                        let val = self.read_reg(rs2) as u32 as u64;
                        // Traced too, so a 32-bit struct field (an rbtree node's
                        // count, say) can be watched the same way as a 64-bit one.
                        self.record_store(vaddr, val);
                        bus.write_u32(paddr, val as u32);
                    }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Sd { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => {
                        let val = self.read_reg(rs2);
                        self.record_store(vaddr, val);
                        bus.write_u64(paddr, val);
                    }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Fld { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => { self.cpu.write_freg(rd, bus.read_u64(paddr)); self.cpu.fs_dirty = true; }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            // flw/fsw used to skip this path entirely, so with the MMU on they
            // used the virtual address as a physical one.
            Flw { rd, rs1, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Load, vaddr) {
                    Ok(paddr) => {
                        // NaN-boxed, as in Cpu::execute_width.
                        self.cpu.write_freg(rd, 0xFFFF_FFFF_0000_0000 | bus.read_u32(paddr) as u64);
                        self.cpu.fs_dirty = true;
                    }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Fsw { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { bus.write_u32(paddr, self.cpu.read_freg(rs2) as u32); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Fsd { rs1, rs2, imm } => {
                let vaddr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { bus.write_u64(paddr, self.cpu.read_freg(rs2)); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            // Atomic RMW operations: translate vaddr -> paddr, then read-modify-write.
            // (Formerly fell through to cpu.execute_width which did NOT translate,
            //  causing atomics on kernel VAs to hit DRAM out-of-range -> NO-OPs -> slab corruption.)
            Amoswapw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr) as i32 as u64; bus.write_u32(paddr, self.read_reg(rs2) as u32); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoaddw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr) as i32; let new = old.wrapping_add(self.read_reg(rs2) as i32); bus.write_u32(paddr, new as u32); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoxorw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr); bus.write_u32(paddr, old ^ self.read_reg(rs2) as u32); self.cpu.write_reg(rd, old as i32 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoandw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr); bus.write_u32(paddr, old & self.read_reg(rs2) as u32); self.cpu.write_reg(rd, old as i32 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoorw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr); bus.write_u32(paddr, old | self.read_reg(rs2) as u32); self.cpu.write_reg(rd, old as i32 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amominw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr) as i32; let new = self.read_reg(rs2) as i32; let m = if old < new { old } else { new }; bus.write_u32(paddr, m as u32); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amomaxw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u32(paddr) as i32; let new = self.read_reg(rs2) as i32; let m = if old > new { old } else { new }; bus.write_u32(paddr, m as u32); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amominuw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    // The comparison is unsigned, but rd still gets the loaded
                    // word SIGN-extended — that is true of every .W AMO, the
                    // `u` only selects the comparison. Zero-extending here made
                    // amominu.w return the wrong value for words >= 0x80000000.
                    Ok(paddr) => { let old = bus.read_u32(paddr); let new = self.read_reg(rs2) as u32; let m = if old < new { old } else { new }; bus.write_u32(paddr, m); self.cpu.write_reg(rd, old as i32 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amomaxuw { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    // Unsigned comparison, sign-extended result — see amominu.w.
                    Ok(paddr) => { let old = bus.read_u32(paddr); let new = self.read_reg(rs2) as u32; let m = if old > new { old } else { new }; bus.write_u32(paddr, m); self.cpu.write_reg(rd, old as i32 as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoswapd { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); bus.write_u64(paddr, self.read_reg(rs2)); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoaddd { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr) as i64; let new = old.wrapping_add(self.read_reg(rs2) as i64); bus.write_u64(paddr, new as u64); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoxord { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); bus.write_u64(paddr, old ^ self.read_reg(rs2)); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoandd { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); bus.write_u64(paddr, old & self.read_reg(rs2)); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amoord { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); bus.write_u64(paddr, old | self.read_reg(rs2)); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amomind { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr) as i64; let new = self.read_reg(rs2) as i64; let m = if old < new { old } else { new }; bus.write_u64(paddr, m as u64); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amomaxd { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr) as i64; let new = self.read_reg(rs2) as i64; let m = if old > new { old } else { new }; bus.write_u64(paddr, m as u64); self.cpu.write_reg(rd, old as u64); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amominud { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); let new = self.read_reg(rs2); let m = if old < new { old } else { new }; bus.write_u64(paddr, m); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            Amomaxud { rd, rs1, rs2, .. } => {
                let vaddr = (self.read_reg(rs1) as i64) as u64;
                match self.translate(bus, AccessType::Store, vaddr) {
                    Ok(paddr) => { let old = bus.read_u64(paddr); let new = self.read_reg(rs2); let m = if old > new { old } else { new }; bus.write_u64(paddr, m); self.cpu.write_reg(rd, old); }
                    Err(t) => return self.take_trap_with_vaddr(t, Some(vaddr)),
                }
                self.cpu.pc = self.cpu.pc.wrapping_add(width as u64);
            }
            _ => {
                // This should never be reached - is_load_store guards the call.
                // If we get here, atomics were handled in execute_supervisor already.
                // Simpler: just return IllegalInstruction as safety fallback.
                return self.take_trap(Trap::Exception(Exception::IllegalInstruction));
            }
        }
        Status::Running
    }
}

// ---------------------------------------------------------------------------
// Snapshots.
//
// Architectural state only. Every diagnostic ring, watch, and one-shot capture
// flag restores to its default: they are instruments bolted on for specific
// investigations, and a resumed machine that diverges because a debug ring was
// missing would be absurd. The TLB is a cache and restores cold.
// ---------------------------------------------------------------------------

use riscv_core::state::{Reader, Writer};

impl Supervisor {
    pub fn save_state(&self, w: &mut Writer) {
        for r in self.cpu.x {
            w.u64(r);
        }
        for r in self.cpu.f {
            w.u64(r);
        }
        w.u64(self.cpu.pc);
        w.u64(self.cpu.fcsr);
        w.bool(self.cpu.fs_dirty);

        w.u8(self.priv_level as u8);
        w.u64(self.mstatus.to_bits());
        w.u64(self.mie);
        w.u64(self.mip);
        w.u64(self.mtvec);
        w.u64(self.mepc);
        w.u64(self.mcause);
        w.u64(self.mtval);
        w.u64(self.mscratch);
        w.u64(self.medeleg);
        w.u64(self.mideleg);
        w.u64(self.stvec);
        w.u64(self.sepc);
        w.u64(self.scause);
        w.u64(self.stval);
        w.u64(self.sscratch);
        w.u64(self.satp.to_bits());
        w.u64(self.stimecmp);
        w.u64(self.mcycle);
        w.u64(self.minstret);
        for v in self.pmpcfg {
            w.u64(v);
        }
        for v in self.pmpaddr {
            w.u64(v);
        }
        w.u64(self.mhartid);
        w.bool(self.wfi);
        w.bool(self.sbi_enabled);
        w.u64(self.reservation_addr.map_or(u64::MAX, |a| a));
        w.bool(self.reservation_addr.is_some());
    }

    pub fn load_state(&mut self, r: &mut Reader) -> Option<()> {
        for i in 0..32 {
            self.cpu.x[i] = r.u64()?;
        }
        for i in 0..32 {
            self.cpu.f[i] = r.u64()?;
        }
        self.cpu.pc = r.u64()?;
        self.cpu.fcsr = r.u64()?;
        self.cpu.fs_dirty = r.bool()?;

        self.priv_level = match r.u8()? {
            0 => Privilege::User,
            1 => Privilege::Supervisor,
            _ => Privilege::Machine,
        };
        self.mstatus.from_bits(r.u64()?);
        self.mie = r.u64()?;
        self.mip = r.u64()?;
        self.mtvec = r.u64()?;
        self.mepc = r.u64()?;
        self.mcause = r.u64()?;
        self.mtval = r.u64()?;
        self.mscratch = r.u64()?;
        self.medeleg = r.u64()?;
        self.mideleg = r.u64()?;
        self.stvec = r.u64()?;
        self.sepc = r.u64()?;
        self.scause = r.u64()?;
        self.stval = r.u64()?;
        self.sscratch = r.u64()?;
        self.satp.from_bits(r.u64()?);
        self.stimecmp = r.u64()?;
        self.mcycle = r.u64()?;
        self.minstret = r.u64()?;
        for i in 0..16 {
            self.pmpcfg[i] = r.u64()?;
        }
        for i in 0..64 {
            self.pmpaddr[i] = r.u64()?;
        }
        self.mhartid = r.u64()?;
        self.wfi = r.bool()?;
        self.sbi_enabled = r.bool()?;
        let addr = r.u64()?;
        self.reservation_addr = if r.bool()? { Some(addr) } else { None };

        self.mmu.flush_tlb();
        // The restored image is a different machine's memory; anything decoded
        // or cached from the old one is meaningless. dcache_flush drops the
        // whole ASID table and moves the generation, so no separate bump here.
        self.dcache_flush();
        Some(())
    }
}
