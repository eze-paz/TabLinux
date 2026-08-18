//! Device bus: maps physical addresses to DRAM, CLINT, PLIC, VirtIO MMIO

use riscv_core::execute::Bus;


/// Guest physical memory layout
const MROM_BASE: u64 = 0x0000_0000;
const MROM_SIZE: u64 = 0x0001_0000; // 64KB

const CLINT_BASE: u64 = 0x0200_0000;
const CLINT_SIZE: u64 = 0x0001_0000; // 64KB

const PLIC_BASE: u64 = 0x0C00_0000;
const PLIC_SIZE: u64 = 0x0060_0000; // 6MB — must cover the context blocks at +0x200000

/// Number of interrupt sources the PLIC models. Source 0 is reserved ("no
/// interrupt"), so this matches the DTB's `riscv,ndev = <0x5f>` plus one.
const PLIC_NSRC: usize = 96;
/// PLIC contexts. The DTB wires `interrupts-extended = <&intc0 11 &intc0 9>`,
/// i.e. context 0 = hart0 M-external, context 1 = hart0 S-external. A couple of
/// spares cost nothing and keep out-of-range writes from panicking.
const PLIC_NCTX: usize = 8;
const PLIC_WORDS: usize = PLIC_NSRC.div_ceil(32);

/// hwirq of `serial@10000000` in the QEMU virt devicetree we clone.
const UART_IRQ: usize = 10;

/// SiFive-compatible PLIC.
///
/// Only what Linux actually drives: per-source priority, per-context enable
/// bitmaps and threshold, and the claim/complete register. Sources are treated
/// as level-triggered (which is what the 8250 is): `level[]` is the raw device
/// line and a source stops being claimable while it is in service, exactly as a
/// real PLIC gates re-assertion between claim and complete.
struct Plic {
    priority: [u32; PLIC_NSRC],
    level: [bool; PLIC_NSRC],
    claimed: [bool; PLIC_NSRC],
    enable: [[u32; PLIC_WORDS]; PLIC_NCTX],
    threshold: [u32; PLIC_NCTX],
    /// Bitmap of sources that are asserted and not in service, i.e. the
    /// candidates a claim could return.
    ///
    /// `any_pending()` is consulted once per retired instruction to drive SEIP.
    /// Scanning all NSRC x NCTX combinations there made the emulator ~50x
    /// slower than the guest work it was emulating, so the hot path walks only
    /// this bitmap — normally empty, at most a couple of bits set.
    claimable: [u32; PLIC_WORDS],
}

impl Plic {
    fn new() -> Self {
        Self {
            priority: [0; PLIC_NSRC],
            level: [false; PLIC_NSRC],
            claimed: [false; PLIC_NSRC],
            enable: [[0; PLIC_WORDS]; PLIC_NCTX],
            threshold: [0; PLIC_NCTX],
            claimable: [0; PLIC_WORDS],
        }
    }

    fn set_level(&mut self, irq: usize, level: bool) {
        if self.level[irq] != level {
            self.level[irq] = level;
            self.recalc_claimable(irq);
        }
    }

    fn recalc_claimable(&mut self, irq: usize) {
        let bit = 1u32 << (irq % 32);
        if self.level[irq] && !self.claimed[irq] {
            self.claimable[irq / 32] |= bit;
        } else {
            self.claimable[irq / 32] &= !bit;
        }
    }

    fn enabled(&self, ctx: usize, irq: usize) -> bool {
        ctx < PLIC_NCTX && (self.enable[ctx][irq / 32] >> (irq % 32)) & 1 != 0
    }

    /// Highest-priority claimable source for `ctx`, or 0 for "none".
    fn best(&self, ctx: usize) -> usize {
        let mut best = 0usize;
        let mut best_prio = 0u32;
        for (w, &word) in self.claimable.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let irq = w * 32 + b;
                if irq == 0 || !self.enabled(ctx, irq) {
                    continue;
                }
                let p = self.priority[irq];
                if p > self.threshold[ctx] && p > best_prio {
                    best_prio = p;
                    best = irq;
                }
            }
        }
        best
    }

    fn any_pending(&self) -> bool {
        if self.claimable.iter().all(|&w| w == 0) {
            return false;
        }
        (0..PLIC_NCTX).any(|ctx| self.best(ctx) != 0)
    }

    fn claim(&mut self, ctx: usize) -> u32 {
        let irq = self.best(ctx);
        if irq != 0 {
            self.claimed[irq] = true;
            self.recalc_claimable(irq);
        }
        irq as u32
    }

    fn complete(&mut self, irq: usize) {
        if irq < PLIC_NSRC {
            self.claimed[irq] = false;
            self.recalc_claimable(irq);
        }
    }
}

const UART_BASE: u64 = 0x1000_0000;
const UART_SIZE: u64 = 0x0000_0100;

use crate::virtio::{GuestMem, VirtioMmio, VIRTIO_MMIO_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE};

const DRAM_BASE: u64 = 0x8000_0000;

/// Device bus implementation matching riscv_core::execute::Bus
pub struct DeviceBus {
    dram: alloc::vec::Vec<u8>,
    dram_base: u64,
    dram_size: u64,

    // CLINT
    mtime: u64,
    mtimecmp: u64,
    msoft: u32,
    /// Sub-tick accumulator: `tick()` is called once per retired instruction, but
    /// mtime must advance far more slowly than that (see `MTIME_STEPS_PER_TICK`).
    tick_accum: u64,

    // Console capture
    pub console_output: alloc::string::String,

    // UART console capture
    pub uart_console: alloc::vec::Vec<u8>,

    // UART 16550A register state (for 8250 autoconfig: scratch + loopback)
    uart_mcr: u8,
    uart_scr: u8,
    uart_ier: u8,
    /// Bytes waiting to be read out of the RBR (console input). RefCell because
    /// `Bus::read_u8` takes `&self` but reading the RBR consumes a byte.
    uart_rx: core::cell::RefCell<alloc::collections::VecDeque<u8>>,
    plic: core::cell::RefCell<Plic>,
    uart_fcr: u8,
    uart_lcr: u8,
    uart_dll: u8,
    uart_dlm: u8,

    /// VirtIO MMIO slots, matching the eight `virtio_mmio@1000N000` nodes in the
    /// devicetree. Slot i lives at `VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_STRIDE`
    /// and owns PLIC hwirq `i + 1`.
    virtio: [Option<VirtioMmio>; VIRTIO_MMIO_SLOTS],
    /// One past the highest occupied slot, so the per-instruction scans walk
    /// two entries instead of eight. Slots are filled from the bottom by
    /// `attach_virtio` and never freed, so this is exact rather than a
    /// high-water mark that drifts.
    virtio_n: usize,
    net_poll_accum: u64,
    /// Emit emulated-time stamps for device events (see `trace_ms`).
    pub net_trace: bool,
    /// Consecutive idle parks that declined to move the clock.
    idle_stall: u32,
    /// Frame queues shared with the attached virtio-net device, if any.
    pub net: Option<crate::virtio_net::SharedNet>,

    // Timer debug logging
    pub timer_debug_log: alloc::vec::Vec<u8>,
    pub timer_debug_enabled: bool,

    // CLINT access counters (Cell for interior mutability in &self reads)
    clint_mtime_reads: core::cell::Cell<u64>,
    clint_mtimecmp_writes: core::cell::Cell<u64>,
}

extern crate alloc;

impl DeviceBus {
    /// Guest RAM, in bytes.
    ///
    /// Worth reading rather than assuming: a machine restored from a snapshot
    /// has whatever size the snapshot was taken at, which need not be the size
    /// a cold boot would have chosen.
    pub fn dram_size(&self) -> u64 {
        self.dram_size
    }

    /// Allocate guest RAM. On wasm, lazily: reserve without writing, so
    /// untouched guest pages never become resident and the tab pays only for
    /// the RAM the guest actually uses. Linear memory is zero on grow and a
    /// large first allocation grows fresh pages rather than reusing a freed
    /// block, so the bytes are defined zeros. On every other target malloc
    /// does not zero, so fall back to eager `vec![0u8; n]` there — the native
    /// test suite boots the guest and would read garbage otherwise.
    #[cfg(target_arch = "wasm32")]
    fn alloc_dram(dram_size: usize) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec::Vec::<u8>::with_capacity(dram_size);
        // SAFETY: wasm grow semantics define these bytes as zero; the boot and
        // difftest battery reads guest RAM the guest never wrote and would
        // diverge instantly if they came back non-zero.
        unsafe { v.set_len(dram_size); }
        v
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn alloc_dram(dram_size: usize) -> alloc::vec::Vec<u8> {
        alloc::vec![0u8; dram_size]
    }

    pub fn new(dram_size: usize) -> Self {
        Self {
            dram: Self::alloc_dram(dram_size),
            dram_base: DRAM_BASE,
            dram_size: dram_size as u64,
            mtime: 0,
            mtimecmp: !0u64, // no timer armed until the kernel writes mtimecmp/stimecmp
            tick_accum: 0,
            msoft: 0,
            console_output: alloc::string::String::new(),
            uart_console: alloc::vec::Vec::new(),
            uart_mcr: 0,
            uart_scr: 0,
            uart_ier: 0,
            uart_rx: core::cell::RefCell::new(alloc::collections::VecDeque::new()),
            plic: core::cell::RefCell::new(Plic::new()),
            uart_fcr: 0,
            uart_lcr: 0,
            uart_dll: 0,
            uart_dlm: 0,
            virtio: [const { None }; VIRTIO_MMIO_SLOTS],
            virtio_n: 0,
            net_poll_accum: 0,
            net_trace: false,
            idle_stall: 0,
            net: None,
            timer_debug_log: alloc::vec::Vec::new(),
            timer_debug_enabled: false,
            clint_mtime_reads: core::cell::Cell::new(0),
            clint_mtimecmp_writes: core::cell::Cell::new(0),
        }
    }

    pub fn load_blob(&mut self, addr: u64, data: &[u8]) {
        let offset = (addr - self.dram_base) as usize;
        self.dram[offset..offset + data.len()].copy_from_slice(data);
    }

    pub fn load_dtb(&mut self, dtb: &[u8]) {
        let offset = MROM_BASE as usize;
        if offset + dtb.len() <= MROM_SIZE as usize {
            // Need to handle MROM... for now just skip dtb
            // In practice, bbl embeds its own dtb
        }
    }

    fn dram_offset(&self, addr: u64) -> Option<usize> {
        if addr >= self.dram_base && addr < self.dram_base + self.dram_size {
            Some((addr - self.dram_base) as usize)
        } else {
            None
        }
    }

    fn in_clint(&self, addr: u64) -> bool {
        addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE
    }

    fn in_plic(&self, addr: u64) -> bool {
        addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE
    }

    /// Map a physical address to a (slot, register offset) pair, or None if it
    /// is outside the virtio-mmio window.
    fn virtio_slot(addr: u64) -> Option<(usize, usize)> {
        let end = VIRTIO_MMIO_BASE + VIRTIO_MMIO_STRIDE * VIRTIO_MMIO_SLOTS as u64;
        if addr < VIRTIO_MMIO_BASE || addr >= end {
            return None;
        }
        let rel = addr - VIRTIO_MMIO_BASE;
        Some(((rel / VIRTIO_MMIO_STRIDE) as usize, (rel % VIRTIO_MMIO_STRIDE) as usize))
    }

    /// Attach a device to the first free virtio-mmio slot. Returns its hwirq.
    ///
    /// Slot order matters to the guest only in that it determines the device
    /// probe order (and therefore /dev/vda vs /dev/vdb), not correctness.
    pub fn attach_virtio(&mut self, dev: alloc::boxed::Box<dyn crate::virtio::VirtioDevice>) -> Option<usize> {
        let slot = self.virtio.iter().position(|s| s.is_none())?;
        let irq = slot + 1;
        self.virtio[slot] = Some(VirtioMmio::new(dev, irq));
        self.virtio_n = self.virtio_n.max(slot + 1);
        Some(irq)
    }

    /// Attach a virtio-net device, returning its hwirq and the frame queues.
    ///
    /// The caller drains to_host and fills to_guest. Natively that is a test or
    /// a small host stack; in the browser it is v86 fake_network.js on the far
    /// end, with wisp_network.js carrying the terminated TCP streams out over a
    /// WebSocket.
    pub fn attach_virtio_net(
        &mut self,
        mac: [u8; 6],
    ) -> Option<(usize, crate::virtio_net::SharedNet)> {
        use crate::virtio_net::{NetQueues, SharedBackend, VirtioNet};
        let shared = alloc::rc::Rc::new(core::cell::RefCell::new(NetQueues::default()));
        let dev = VirtioNet::new(alloc::boxed::Box::new(SharedBackend(shared.clone())), mac);
        let irq = self.attach_virtio(alloc::boxed::Box::new(dev))?;
        self.net = Some(shared.clone());
        Some((irq, shared))
    }

    /// Frames the guest has transmitted so far.
    pub fn net_sent_frames(&self) -> Option<alloc::vec::Vec<alloc::vec::Vec<u8>>> {
        Some(self.net.as_ref()?.borrow().to_host.iter().cloned().collect())
    }

    /// Hand a frame to the guest.
    pub fn net_inject_frame(&self, frame: &[u8]) -> Option<()> {
        self.net.as_ref()?.borrow_mut().to_guest.push_back(frame.to_vec());
        Some(())
    }

    /// Longest single jump an idle hart may take, in mtime ticks — 1 ms at the
    /// 10 MHz timebase the devicetree declares.
    ///
    /// Skipping straight to the timer deadline is what makes idle cheap, but a
    /// NOHZ deadline can be hundreds of milliseconds out, and anything arriving
    /// inside the skipped interval is delivered as though it had waited that
    /// long. That is what put ping RTTs at 992 ms.
    pub const MAX_IDLE_SKIP: u64 = 10_000;

    /// Skip idle time toward `target`, never past a frame already queued for
    /// the guest and never by more than `MAX_IDLE_SKIP`.
    ///
    /// Two narrower variants were tried against the ping round trip and both
    /// measured *worse* than this, so neither is here: crawling the clock while
    /// a device had work in flight (1448 ms), and stopping it dead (2277 ms),
    /// against 343 ms for this. See `docs/networking.md` — the remaining
    /// inflation is not in the idle path at all.
    /// Skip idle time all the way to `target`, same queued-frame guard but no
    /// per-call cap. For the deep-idle host-sleep path: the run loop calls the
    /// capped variant hundreds of times per real millisecond while spinning,
    /// so the cap paces without stalling — but a host that SLEEPS between
    /// parks pays one capped hop (1ms guest) per ~50ms nap, which ran the
    /// guest clock at ~2% of real time (`sleep 5` measured 470ms of guest
    /// progress in 21.5s). After a slept-on park the wait is simply over;
    /// jump to it, exactly the state the old spin reached via many hops.
    pub fn idle_skip_mtime_to(&mut self, target: u64) {
        if self.net.as_ref().is_some_and(|n| !n.borrow().to_guest.is_empty())
            && self.idle_stall < 4096
        {
            self.idle_stall += 1;
            return;
        }
        self.idle_stall = 0;
        self.fast_forward_mtime(target);
    }

    pub fn idle_skip_mtime(&mut self, target: u64) {
        // Already deliverable: moving at all just backdates it.
        //
        // Bounded, because a frame only leaves the queue once the guest posts a
        // receive buffer, and an interface that is down never does. Unbounded,
        // one undeliverable frame stops the clock for the rest of the run and
        // every subsequent idle is paid at full interpreter price — which is
        // exactly what burned three billion instructions before the guest could
        // finish a single ping.
        if self.net.as_ref().is_some_and(|n| !n.borrow().to_guest.is_empty())
            && self.idle_stall < 4096
        {
            self.idle_stall += 1;
            return;
        }
        self.idle_stall = 0;
        self.fast_forward_mtime(target.min(self.mtime + Self::MAX_IDLE_SKIP));
    }

    /// Skip idle time toward `target`, bounded by how much REAL time the host
    /// says has passed rather than by `MAX_IDLE_SKIP`.
    ///
    /// Same queued-frame guard as `idle_skip_mtime`, for the same reason. What
    /// it drops is the fixed cap, and it can, because the cap was only ever a
    /// proxy for "do not run the guest clock far ahead of reality". Given the
    /// host clock, that can be enforced directly, and the proxy stops mattering.
    ///
    /// Returns true if the guest clock reached `target`, i.e. its wait is over.
    pub fn idle_skip_mtime_realtime(&mut self, target: u64, allowance: u64) -> bool {
        if self.net.as_ref().is_some_and(|n| !n.borrow().to_guest.is_empty())
            && self.idle_stall < 4096
        {
            self.idle_stall += 1;
            return false;
        }
        self.idle_stall = 0;
        self.fast_forward_mtime(target.min(self.mtime.saturating_add(allowance)));
        self.mtime >= target
    }

    /// Retire every in-flight virtio completion immediately and resync the PLIC
    /// lines. Returns true if any device had work outstanding.
    ///
    /// Called when the guest is about to idle: a completion it is waiting on
    /// must be delivered rather than skipped over by the timer fast-forward.
    /// See `VirtioMmio::flush_pending`.
    pub fn flush_virtio_completions(&mut self) -> bool {
        let mut any = false;
        for slot in 0..self.virtio_n {
            let Some(dev) = self.virtio[slot].as_mut() else { continue };
            if !dev.has_pending() {
                continue;
            }
            let mut mem = GuestMem { dram: &mut self.dram, base: self.dram_base };
            if dev.flush_pending(&mut mem) {
                any = true;
            }
            let (irq, level) = (dev.irq, dev.irq_level());
            self.plic.borrow_mut().set_level(irq, level);
        }
        any
    }

    /// Recompute every virtio slot's PLIC line from its InterruptStatus.
    fn refresh_virtio_irqs(&self) {
        let mut plic = self.plic.borrow_mut();
        for slot in self.virtio[..self.virtio_n].iter().flatten() {
            plic.set_level(slot.irq, slot.irq_level());
        }
    }

    /// Service an MMIO store to a virtio slot, then resync its interrupt line.
    fn virtio_write(&mut self, slot: usize, off: usize, val: u32) {
        // Split the borrow: the device DMAs into `dram`, which is a sibling
        // field, so both can be borrowed mutably at once.
        let Some(dev) = self.virtio[slot].as_mut() else {
            return;
        };
        let mut mem = GuestMem {
            dram: &mut self.dram,
            base: self.dram_base,
        };
        let changed = dev.write(off, val, &mut mem);
        // A queue notify is handled synchronously inside `write`, so this is the
        // moment the guest's frame becomes visible to the host.
        if self.net_trace && off == 0x050 {
            let mt = self.mtime;
            crate::trace_ms("kick", mt);
        }
        if changed {
            let level = dev.irq_level();
            let irq = dev.irq;
            self.plic.borrow_mut().set_level(irq, level);
        }
    }

    fn in_uart(&self, addr: u64) -> bool {
        addr >= UART_BASE && addr < UART_BASE + UART_SIZE
    }

    /// Instructions retired per mtime tick.
    ///
    /// `tick()` runs once per instruction. The DTB declares
    /// `timebase-frequency = 10 MHz`, so one jiffy (HZ=100, 10 ms) is 100_000
    /// mtime ticks; at this divisor that is 1M instructions of real work per
    /// jiffy — roughly a 100 MIPS machine, and comfortably more than the
    /// interrupt entry/exit path costs.
    ///
    /// This used to be `mtime += 10000` PER INSTRUCTION, i.e. 1 ms of virtual
    /// time per instruction. Once the kernel armed its sstc tick, every ~2000
    /// instructions took a SupervisorTimer interrupt and the boot livelocked in
    /// `handle_softirqs` — hundreds of millions of steps with no forward
    /// progress. Time must run slower than the code that observes it.
    const MTIME_STEPS_PER_TICK: u64 = 10;

    /// Advance the clock as if `n` instructions retired.
    ///
    /// For compiled blocks, which retire several instructions per run-loop
    /// iteration. Ticking once per block instead would run emulated time slow
    /// in proportion to block length, starving the guest of timer interrupts.
    ///
    /// The per-device poll runs once rather than `n` times: it is driven by
    /// divisors on these accumulators, so advancing them by `n` keeps the
    /// polling rate per retired instruction the same as the interpreter's.
    /// Address, in this module's linear memory, of the 4 KiB page containing
    /// `paddr` -- but only if that page is plain DRAM.
    ///
    /// `None` for MMIO, and that is the whole point: a device register read has
    /// side effects and its bytes are not in linear memory, so it must never be
    /// reachable by a direct load from compiled code.
    ///
    /// Sound only because `dram` is allocated once and never resized. If it
    /// were, every address handed out here would dangle.
    pub fn dram_page_host_addr(&self, paddr: u64) -> Option<u32> {
        let page = paddr & !0xFFF;
        let off = page.checked_sub(self.dram_base)?;
        // The whole page has to be inside DRAM, not just its first byte.
        if off.checked_add(4096)? > self.dram_size {
            return None;
        }
        Some((self.dram.as_ptr() as usize as u64 + off) as u32)
    }

    pub fn tick_n(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        if n == 1 {
            self.tick();
            return;
        }
        self.tick_accum += n;
        let ticks = self.tick_accum / Self::MTIME_STEPS_PER_TICK;
        self.tick_accum %= Self::MTIME_STEPS_PER_TICK;
        self.mtime = self.mtime.wrapping_add(ticks);
        // virtio_tick bumps this by one itself; credit the rest here.
        self.net_poll_accum = self.net_poll_accum.wrapping_add(n - 1);
        // Advance the virtio completion clock by the whole chain, not by one:
        // otherwise a completion latency measured in ticks takes as many chains
        // to retire, and a driver polling for it spins. See VirtioMmio::tick.
        self.virtio_tick(n);
    }

    pub fn tick(&mut self) {
        self.tick_accum += 1;
        if self.tick_accum >= Self::MTIME_STEPS_PER_TICK {
            self.tick_accum = 0;
            self.mtime = self.mtime.wrapping_add(1);
        }
        self.virtio_tick(1);
    }

    /// Retire any virtio completions whose latency has elapsed, and give
    /// devices a chance to push inbound data. `advance` is retired instructions
    /// since the last call — the amount to move each device's completion clock.
    fn virtio_tick(&mut self, advance: u64) {
        // Receive polling is far cheaper than it looks but still not free, so
        // it runs on a divisor rather than every retired instruction. At the
        // emulated clock this is still thousands of polls a second.
        self.net_poll_accum = self.net_poll_accum.wrapping_add(1);
        let poll_rx = self.net_poll_accum % 512 == 0;

        let mt = self.mtime;
        let trace = self.net_trace;
        for slot in 0..self.virtio_n {
            let Some(dev) = self.virtio[slot].as_mut() else { continue };
            if poll_rx {
                let mut mem = GuestMem { dram: &mut self.dram, base: self.dram_base };
                if dev.poll(&mut mem) {
                    if trace {
                        crate::trace_ms("rx->guest", mt);
                    }
                    let (irq, level) = (dev.irq, dev.irq_level());
                    self.plic.borrow_mut().set_level(irq, level);
                }
            }
            let Some(dev) = self.virtio[slot].as_mut() else { continue };
            // Cheap early-out: only devices with work outstanding need the
            // borrow dance, and this runs once per retired instruction.
            if !dev.has_pending() {
                continue;
            }
            let mut mem = GuestMem { dram: &mut self.dram, base: self.dram_base };
            if dev.tick(advance, &mut mem) {
                // A completion retired, so the guest is only now told its
                // buffer is done — this is the deferred half of every transfer.
                if trace {
                    crate::trace_ms("completion", mt);
                }
                let (irq, level) = (dev.irq, dev.irq_level());
                self.plic.borrow_mut().set_level(irq, level);
            }
        }
    }

    /// Jump mtime forward to `target` (never backwards).
    ///
    /// Used when the hart parks in WFI: on a single-hart machine nothing but the
    /// timer can wake it, so spinning the idle loop just burns emulated
    /// instructions. Skipping straight to the next deadline is what a real
    /// emulator does and turns a multi-second NOHZ idle from billions of steps
    /// into one.
    pub fn fast_forward_mtime(&mut self, target: u64) {
        if target > self.mtime {
            self.mtime = target;
            self.tick_accum = 0;
        }
    }

    /// Queue console input for the guest to read out of the UART RBR.
    pub fn uart_push_input(&mut self, bytes: &[u8]) {
        self.uart_rx.borrow_mut().extend(bytes.iter().copied());
        self.refresh_uart_irq();
    }

    fn pop_rx(&self) -> u8 {
        self.uart_rx.borrow_mut().pop_front().unwrap_or(0)
    }

    /// Is the 8250's interrupt line asserted?
    ///
    /// TX completes instantly here, so THR is always empty: enabling IER.THRI is
    /// enough to assert. Without this, `serial8250_start_tx` armed an interrupt
    /// that never arrived, the tty ring never drained, and every byte written by
    /// a *userspace* process was lost — kernel printk still worked because it
    /// uses the polled `console_write` path.
    fn uart_irq_level(&self) -> bool {
        let thri = self.uart_ier & 0x02 != 0;
        let rdi = self.uart_ier & 0x01 != 0 && !self.uart_rx.borrow().is_empty();
        thri || rdi
    }

    fn refresh_uart_irq(&self) {
        let level = self.uart_irq_level();
        self.plic.borrow_mut().set_level(UART_IRQ, level);
    }

    /// Does any PLIC context have a claimable interrupt? Drives SEIP.
    /// Diagnostic: mtime field, without going through the Bus trait.
    pub fn diag_mtime(&self) -> u64 {
        self.mtime
    }

    /// Diagnostic: does any virtio device have a completion still in flight?
    pub fn any_virtio_pending(&self) -> bool {
        self.virtio[..self.virtio_n]
            .iter()
            .flatten()
            .any(|d| d.has_pending())
    }

    pub fn external_interrupt_pending(&self) -> bool {
        let level = self.uart_irq_level();
        // Occupied virtio slots only — normally one or two, so this stays off
        // the PLIC's expensive full-source scan.
        self.refresh_virtio_irqs();
        let mut plic = self.plic.borrow_mut();
        plic.set_level(UART_IRQ, level);
        plic.any_pending()
    }

    fn plic_read(&self, addr: u64) -> u32 {
        let off = (addr - PLIC_BASE) as usize;
        let mut plic = self.plic.borrow_mut();
        match off {
            0x0000_0000..=0x0000_0FFF => {
                let irq = off / 4;
                if irq < PLIC_NSRC { plic.priority[irq] } else { 0 }
            }
            0x0000_1000..=0x0000_1FFF => {
                // pending bitmap (read-only)
                let word = (off - 0x1000) / 4;
                if word >= PLIC_WORDS { return 0; }
                let mut bits = 0u32;
                for b in 0..32 {
                    let irq = word * 32 + b;
                    if irq < PLIC_NSRC && plic.level[irq] && !plic.claimed[irq] {
                        bits |= 1 << b;
                    }
                }
                bits
            }
            0x0000_2000..=0x001F_FFFF => {
                let ctx = (off - 0x2000) / 0x80;
                let word = ((off - 0x2000) % 0x80) / 4;
                if ctx < PLIC_NCTX && word < PLIC_WORDS { plic.enable[ctx][word] } else { 0 }
            }
            0x0020_0000..=0x03FF_FFFF => {
                let ctx = (off - 0x200000) / 0x1000;
                match (off - 0x200000) % 0x1000 {
                    0x0 => if ctx < PLIC_NCTX { plic.threshold[ctx] } else { 0 },
                    // Reading claim/complete claims the highest-priority source.
                    0x4 => if ctx < PLIC_NCTX { plic.claim(ctx) } else { 0 },
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn plic_write(&mut self, addr: u64, val: u32) {
        let off = (addr - PLIC_BASE) as usize;
        let mut plic = self.plic.borrow_mut();
        match off {
            0x0000_0000..=0x0000_0FFF => {
                let irq = off / 4;
                if irq < PLIC_NSRC { plic.priority[irq] = val; }
            }
            0x0000_1000..=0x0000_1FFF => {} // pending is read-only
            0x0000_2000..=0x001F_FFFF => {
                let ctx = (off - 0x2000) / 0x80;
                let word = ((off - 0x2000) % 0x80) / 4;
                if ctx < PLIC_NCTX && word < PLIC_WORDS { plic.enable[ctx][word] = val; }
            }
            0x0020_0000..=0x03FF_FFFF => {
                let ctx = (off - 0x200000) / 0x1000;
                match (off - 0x200000) % 0x1000 {
                    0x0 => if ctx < PLIC_NCTX { plic.threshold[ctx] = val; },
                    // Writing the claim register completes that source.
                    0x4 => plic.complete(val as usize),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn check_timer_interrupt(&self) -> bool {
        self.mtime >= self.mtimecmp
    }

    pub fn get_mtime(&self) -> u64 {
        self.mtime
    }

    pub fn get_mtimecmp(&self) -> u64 {
        self.mtimecmp
    }

    pub fn get_clint_mtime_reads(&self) -> u64 {
        self.clint_mtime_reads.get()
    }

    pub fn get_clint_mtimecmp_writes(&self) -> u64 {
        self.clint_mtimecmp_writes.get()
    }

    pub fn get_dram(&self) -> &[u8] {
        &self.dram
    }

    pub fn dram_base(&self) -> u64 { self.dram_base }

    pub fn get_dram_mut(&mut self) -> &mut [u8] {
        &mut self.dram
    }

    pub fn get_uart_console(&self) -> &[u8] {
        &self.uart_console
    }
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    // One bounds check on a subslice, then a single 4-byte load. Indexing four
    // times instead costs four checks and four one-byte loads, on every guest
    // `lw` and every 32-bit instruction fetch.
    match bytes[offset..offset + 4].try_into() {
        Ok(b) => u32::from_le_bytes(b),
        // Unreachable: the slice is exactly 4 bytes. Written without `unwrap`
        // so this stays panic-free in the no_std build.
        Err(_) => 0,
    }
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    match bytes[offset..offset + 8].try_into() {
        Ok(b) => u64::from_le_bytes(b),
        Err(_) => 0,
    }
}

impl Bus for DeviceBus {
    fn read_mtime(&self) -> u64 {
        self.mtime
    }
    fn check_timer_interrupt(&self) -> bool {
        self.mtime >= self.mtimecmp
    }
    fn check_external_interrupt(&self) -> bool {
        DeviceBus::external_interrupt_pending(self)
    }

    fn read_u8(&self, addr: u64) -> u8 {
        if let Some(off) = self.dram_offset(addr) {
            self.dram[off]
        } else if self.in_clint(addr) {
            match addr & 0xFFFF {
                0x0000 => 3, // MSIP hart 0
                _ => 0,
            }
        } else if let Some((slot, off)) = Self::virtio_slot(addr) {
            match self.virtio[slot].as_ref() {
                Some(d) => (d.read(off & !3) >> ((off % 4) * 8)) as u8,
                None => 0,
            }
        } else if self.in_uart(addr) {
            let off = (addr - UART_BASE) as u8;
            let dlab = self.uart_lcr & 0x80 != 0;
            match off {
                // DLL (DLAB=1) or RBR: pop one queued input byte.
                0x00 => {
                    if dlab {
                        self.uart_dll
                    } else {
                        self.pop_rx()
                    }
                }
                0x01 => { if dlab { self.uart_dlm } else { self.uart_ier } }  // DLM (DLAB=1) or IER
                // IIR, highest-priority cause first. Bits 7:6 = FIFOs enabled.
                //   0x4 = received data available, 0x2 = THR empty, 0x1 = none.
                0x02 => {
                    if self.uart_ier & 0x01 != 0 && !self.uart_rx.borrow().is_empty() {
                        0xC4
                    } else if self.uart_ier & 0x02 != 0 {
                        0xC2
                    } else {
                        0xC1
                    }
                }
                0x03 => self.uart_lcr,      // LCR
                0x04 => self.uart_mcr,      // MCR
                // LSR: THRE|TEMT always (TX is instantaneous), plus DR when input
                // is queued. Reported at the reg-shift 0/1/2/3 aliases.
                0x05 | 0x0A | 0x14 | 0x1C => {
                    0x60 | if self.uart_rx.borrow().is_empty() { 0x00 } else { 0x01 }
                }
                0x06 => {                    // MSR: reflect MCR in loopback mode
                    if self.uart_mcr & 0x10 != 0 {
                        ((self.uart_mcr & 1) << 5)
                            | ((self.uart_mcr & 2) << 3)
                            | ((self.uart_mcr & 4) << 4)
                            | ((self.uart_mcr & 8) << 4)
                    } else {
                        0x00
                    }
                }
                0x07 => self.uart_scr,      // SCR: scratch read-back
                _ => 0,
            }
        } else {
            0
        }
    }

    fn read_u16(&self, addr: u64) -> u16 {
        u16::from_le_bytes([self.read_u8(addr), self.read_u8(addr + 1)])
    }

    fn read_u32(&self, addr: u64) -> u32 {
        if let Some(off) = self.dram_offset(addr) {
            read_u32_le(&self.dram, off)
        } else if self.in_clint(addr) {
            if addr == 0x0200_BFF8 || addr == 0x0200_BFFC {
                self.clint_mtime_reads.set(self.clint_mtime_reads.get() + 1);
            }
            let val = match addr {
                0x0200_0000 => self.msoft, // msip is a 32-bit register (single hart)
                0x0200_0004 => 0, // hart-1 msip / reserved (not modeled)
                0x0200_4000 => (self.mtimecmp >> 0) as u32,
                0x0200_4004 => (self.mtimecmp >> 32) as u32,
                0x0200_BFF8 => (self.mtime >> 0) as u32,
                0x0200_BFFC => (self.mtime >> 32) as u32,
                _ => 0,
            };
            if self.timer_debug_enabled {
                let _ = alloc::format!("[TIMER] read_u32 addr={:#x} -> mtime={} mtimecmp={} val={} timer_int={}\n",
                    addr, self.mtime, self.mtimecmp, val, self.check_timer_interrupt());
            }
            val
        } else if let Some((slot, off)) = Self::virtio_slot(addr) {
            match self.virtio[slot].as_ref() {
                Some(d) => d.read(off),
                None => 0,
            }
        } else if self.in_plic(addr) {
            self.refresh_uart_irq();
            self.plic_read(addr)
        } else if self.in_uart(addr) {
            self.read_u8(addr) as u32
        } else {
            0
        }
    }

    fn read_u64(&self, addr: u64) -> u64 {
        if let Some(off) = self.dram_offset(addr) {
            read_u64_le(&self.dram, off)
        } else if self.in_clint(addr) {
            if addr == 0x0200_BFF8 {
                self.clint_mtime_reads.set(self.clint_mtime_reads.get() + 1);
            }
            let val = match addr {
                0x0200_0000 => self.msoft as u64,
                0x0200_4000 => self.mtimecmp,
                0x0200_BFF8 => self.mtime,
                _ => 0,
            };
            if self.timer_debug_enabled {
                let _ = alloc::format!("[TIMER] read_u64 addr={:#x} -> mtime={} mtimecmp={} val={} timer_int={}\n",
                    addr, self.mtime, self.mtimecmp, val, self.check_timer_interrupt());
            }
            val
        } else {
            (self.read_u32(addr) as u64) | ((self.read_u32(addr + 4) as u64) << 32)
        }
    }

    fn write_u8(&mut self, addr: u64, val: u8) {
        if let Some(off) = self.dram_offset(addr) {
            self.dram[off] = val;
        } else if let Some((slot, off)) = Self::virtio_slot(addr) {
            // Only device config space (>= 0x100) is byte addressable; the
            // transport registers are 32-bit only per the spec.
            if off >= 0x100 {
                self.virtio_write(slot, off, val as u32);
            }
        } else if self.in_uart(addr) {
            let off = (addr - UART_BASE) as u8;
            let dlab = self.uart_lcr & 0x80 != 0;
            match off {
                0x00 => { // THR (DLAB=0) or DLL (DLAB=1)
                    if dlab { self.uart_dll = val; } else { self.uart_console.push(val); }
                }
                0x01 => { // IER (DLAB=0) or DLM (DLAB=1)
                    if dlab { self.uart_dlm = val; } else { self.uart_ier = val; }
                }
                0x02 => self.uart_fcr = val,          // FCR
                0x03 => self.uart_lcr = val,          // LCR (controls DLAB)
                0x04 => self.uart_mcr = val,          // MCR (loopback bit 4)
                0x07 => self.uart_scr = val,          // SCR: scratch
                _ => {}
            }
        }
    }

    fn write_u16(&mut self, addr: u64, val: u16) {
        if let Some(off) = self.dram_offset(addr) {
            self.dram[off..off + 2].copy_from_slice(&val.to_le_bytes());
        } else if self.in_uart(addr) {
            self.write_u8(addr, (val & 0xFF) as u8);
        } else {
            self.write_u8(addr, (val & 0xFF) as u8);
            self.write_u8(addr + 1, ((val >> 8) & 0xFF) as u8);
        }
    }

    fn write_u32(&mut self, addr: u64, val: u32) {
        if let Some(off) = self.dram_offset(addr) {
            self.dram[off..off + 4].copy_from_slice(&val.to_le_bytes());
        } else if self.in_uart(addr) {
            self.write_u8(addr, (val & 0xFF) as u8);
        } else if self.in_clint(addr) {
            match addr {
                0x0200_0000 => self.msoft = val,
                0x0200_4000 => self.mtimecmp = (self.mtimecmp & !0xFFFFFFFF) | (val as u64),
                0x0200_4004 => self.mtimecmp = (self.mtimecmp & 0xFFFFFFFF) | ((val as u64) << 32),
                _ => {}
            }
        } else if let Some((slot, off)) = Self::virtio_slot(addr) {
            self.virtio_write(slot, off, val);
        } else if self.in_plic(addr) {
            self.plic_write(addr, val);
        }
    }

    fn write_u64(&mut self, addr: u64, val: u64) {
        if let Some(off) = self.dram_offset(addr) {
            self.dram[off..off + 8].copy_from_slice(&val.to_le_bytes());
        } else if self.in_uart(addr) {
            self.write_u8(addr, (val & 0xFF) as u8);
        } else if self.in_clint(addr) {
            match addr {
                0x0200_0000 => self.msoft = val as u32,
                0x0200_4000 => { self.mtimecmp = val; self.clint_mtimecmp_writes.set(self.clint_mtimecmp_writes.get() + 1); },
                _ => {}
            }
        } else {
            self.write_u32(addr, (val & 0xFFFFFFFF) as u32);
            self.write_u32(addr + 4, ((val >> 32) & 0xFFFFFFFF) as u32);
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots.
// ---------------------------------------------------------------------------

use riscv_core::state::{Reader, Writer};

impl DeviceBus {
    /// Serialize the whole bus. DRAM is written sparsely: 4 KiB pages that are
    /// all zero are skipped, which is most of a 1 GiB machine after boot.
    ///
    /// Errs when a device that cannot snapshot itself (virtio-blk today) is
    /// attached; a snapshot that silently dropped the disk would restore to a
    /// machine whose mounted filesystem vanished.
    pub fn save_state(&self, w: &mut Writer) -> Result<(), &'static str> {
        w.u64(self.dram_size);
        const PAGE: usize = 4096;
        let n_pages = self.dram.len() / PAGE;
        let mut used: u64 = 0;
        for i in 0..n_pages {
            if self.dram[i * PAGE..(i + 1) * PAGE].iter().any(|&b| b != 0) {
                used += 1;
            }
        }
        w.u64(used);
        for i in 0..n_pages {
            let page = &self.dram[i * PAGE..(i + 1) * PAGE];
            if page.iter().any(|&b| b != 0) {
                w.u64(i as u64);
                w.buf.extend_from_slice(page);
            }
        }

        w.u64(self.mtime);
        w.u64(self.mtimecmp);
        w.u32(self.msoft);
        w.u64(self.tick_accum);

        w.u8(self.uart_mcr);
        w.u8(self.uart_scr);
        w.u8(self.uart_ier);
        w.u8(self.uart_fcr);
        w.u8(self.uart_lcr);
        w.u8(self.uart_dll);
        w.u8(self.uart_dlm);
        let rx: alloc::vec::Vec<u8> = self.uart_rx.borrow().iter().copied().collect();
        w.bytes(&rx);

        {
            let p = self.plic.borrow();
            for v in p.priority {
                w.u32(v);
            }
            for v in p.level {
                w.bool(v);
            }
            for v in p.claimed {
                w.bool(v);
            }
            for ctx in p.enable {
                for v in ctx {
                    w.u32(v);
                }
            }
            for v in p.threshold {
                w.u32(v);
            }
        }

        for slot in 0..VIRTIO_MMIO_SLOTS {
            match &self.virtio[slot] {
                None => w.bool(false),
                Some(dev) => {
                    w.bool(true);
                    let blob = dev.dev.dev_state().ok_or("attached device cannot snapshot")?;
                    w.u32(dev.dev.device_id());
                    w.bytes(&blob);
                    dev.save_state(w);
                }
            }
        }
        w.u64(self.net_poll_accum);
        Ok(())
    }

    /// Restore into a fresh bus. Devices are re-created by kind before their
    /// transport state is loaded, so the negotiated rings stay valid.
    pub fn load_state(r: &mut Reader) -> Option<Self> {
        let dram_size = r.u64()? as usize;
        let mut bus = DeviceBus::new(dram_size);
        const PAGE: usize = 4096;
        let used = r.u64()?;
        for _ in 0..used {
            let i = r.u64()? as usize;
            let at = i.checked_mul(PAGE)?;
            let page = r.buf.get(r.pos..r.pos + PAGE)?;
            bus.dram.get_mut(at..at + PAGE)?.copy_from_slice(page);
            r.pos += PAGE;
        }

        bus.mtime = r.u64()?;
        bus.mtimecmp = r.u64()?;
        bus.msoft = r.u32()?;
        bus.tick_accum = r.u64()?;

        bus.uart_mcr = r.u8()?;
        bus.uart_scr = r.u8()?;
        bus.uart_ier = r.u8()?;
        bus.uart_fcr = r.u8()?;
        bus.uart_lcr = r.u8()?;
        bus.uart_dll = r.u8()?;
        bus.uart_dlm = r.u8()?;
        bus.uart_rx.borrow_mut().extend(r.bytes()?.iter().copied());

        {
            let mut p = bus.plic.borrow_mut();
            for i in 0..PLIC_NSRC {
                p.priority[i] = r.u32()?;
            }
            for i in 0..PLIC_NSRC {
                p.level[i] = r.bool()?;
            }
            for i in 0..PLIC_NSRC {
                p.claimed[i] = r.bool()?;
            }
            for c in 0..PLIC_NCTX {
                for i in 0..PLIC_WORDS {
                    p.enable[c][i] = r.u32()?;
                }
            }
            for c in 0..PLIC_NCTX {
                p.threshold[c] = r.u32()?;
            }
            // claimable is derived; recompute rather than trust the file.
            for i in 0..PLIC_NSRC {
                p.recalc_claimable(i);
            }
        }

        for _slot in 0..VIRTIO_MMIO_SLOTS {
            if !r.bool()? {
                continue;
            }
            let kind = r.u32()?;
            let blob = r.bytes()?.to_vec();
            match kind {
                1 => {
                    // net: re-create through the normal attach path so the
                    // SharedNet queues exist, then overwrite negotiated state.
                    let mut r2 = Reader::new(&blob);
                    let mac_b = r2.bytes()?;
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(mac_b.get(..6)?);
                    bus.attach_virtio_net(mac)?;
                    let slot = (0..VIRTIO_MMIO_SLOTS)
                        .rev()
                        .find(|&i| bus.virtio[i].is_some())?;
                    let dev = bus.virtio[slot].as_mut()?;
                    dev.dev.load_dev_state(&blob)?;
                    dev.load_state(r)?;
                }
                2 => {
                    // Storage. The snapshot carries the disk's size, never its
                    // bytes — those live in a file or in OPFS and outlive any
                    // snapshot. The device comes back detached, so the guest
                    // still sees the /dev/vda it probed at boot, and the host
                    // must call `attach_blk_backend` before the guest reads.
                    let mut sect = [0u8; 8];
                    sect.copy_from_slice(blob.get(..8)?);
                    let sectors = u64::from_le_bytes(sect);
                    let dev = crate::virtio_blk::VirtioBlk::new(alloc::boxed::Box::new(
                        crate::virtio_blk::DetachedBackend { sectors },
                    ));
                    // attach_virtio hands back the PLIC hwirq, not the slot
                    // index (irq = slot + 1). Using it as an index reads the
                    // next slot along, which is empty, and the restore fails
                    // with no clue as to why.
                    let irq = bus.attach_virtio(alloc::boxed::Box::new(dev))?;
                    bus.virtio[irq - 1].as_mut()?.load_state(r)?;
                }
                9 => {
                    // 9p share. Like storage, the tree is host-side: the blob is
                    // only the mount tag. It comes back empty, at the same slot
                    // the guest probed, and the host re-seeds it from OPFS
                    // before the setup script re-mounts.
                    let tag = alloc::string::String::from_utf8_lossy(&blob).into_owned();
                    let dev = crate::virtio_9p::Virtio9p::new(&tag);
                    let irq = bus.attach_virtio(alloc::boxed::Box::new(dev))?;
                    bus.virtio[irq - 1].as_mut()?.load_state(r)?;
                }
                _ => return None,
            }
        }

        bus.net_poll_accum = r.u64()?;
        Some(bus)
    }

    /// virtio device id occupying `slot`, if any. For diagnostics.
    pub fn virtio_device_id(&self, slot: usize) -> Option<u32> {
        Some(self.virtio.get(slot)?.as_ref()?.dev.device_id())
    }

    /// Attach a virtio-9p share with the given mount tag. Must run before the
    /// guest boots (or, on restore, before it resumes) — virtio-mmio has no
    /// hotplug. Returns the PLIC hwirq, or None if no slot is free.
    pub fn attach_9p(&mut self, tag: &str) -> Option<usize> {
        self.attach_virtio(alloc::boxed::Box::new(crate::virtio_9p::Virtio9p::new(tag)))
    }

    /// Attach a virtio-9p share served lazily from the host: nothing is seeded,
    /// and the guest's reads and directory listings fault in through
    /// `p9_take_reqs`/`p9_supply`. See `Virtio9p::new_lazy`.
    pub fn attach_9p_lazy(&mut self, tag: &str) -> Option<usize> {
        self.attach_virtio(alloc::boxed::Box::new(crate::virtio_9p::Virtio9p::new_lazy(tag)))
    }

    fn p9_slot(&mut self) -> Option<&mut crate::virtio::VirtioMmio> {
        self.virtio.iter_mut().flatten().find(|s| s.dev.device_id() == 9)
    }

    /// Convert the 9p device to lazy on-demand mode (restore path).
    pub fn p9_set_lazy(&mut self) {
        if let Some(s) = self.p9_slot() {
            s.dev.p9_set_lazy();
        }
    }

    /// Drain guest mutations the lazy 9p device recorded, for write-back to
    /// OPFS/Dropbox.
    pub fn p9_take_changes(&mut self) -> alloc::vec::Vec<crate::virtio::FileChange> {
        self.p9_slot().map(|s| s.dev.p9_take_changes()).unwrap_or_default()
    }

    /// Drain the file/listing requests the lazy 9p device is waiting on, for the
    /// host to fetch from OPFS/Dropbox.
    pub fn p9_take_reqs(&mut self) -> alloc::vec::Vec<crate::virtio::HostReq> {
        self.p9_slot().map(|s| s.take_host_reqs()).unwrap_or_default()
    }

    /// Feed a fetched payload back to the lazy 9p device, completing the guest
    /// read/readdir that was blocked on it. Returns whether a chain was
    /// completed (false if the id was unknown or already re-deferred).
    pub fn p9_supply(&mut self, id: u32, payload: &[u8]) -> bool {
        let slot = match self.virtio[..self.virtio_n]
            .iter()
            .position(|s| s.as_ref().is_some_and(|d| d.dev.device_id() == 9))
        {
            Some(i) => i,
            None => return false,
        };
        let Some(dev) = self.virtio[slot].as_mut() else { return false };
        let mut mem = GuestMem { dram: &mut self.dram, base: self.dram_base };
        let ok = dev.supply(id, payload, &mut mem);
        let (irq, level) = (dev.irq, dev.irq_level());
        self.plic.borrow_mut().set_level(irq, level);
        ok
    }

    /// Seed or overwrite a file in the 9p share (host side).
    pub fn p9_put(&mut self, path: &str, data: &[u8]) -> bool {
        self.p9_slot().map(|s| s.dev.p9_put(path, data)).unwrap_or(false)
    }
    /// Make a directory in the 9p share (host side).
    pub fn p9_mkdir(&mut self, path: &str) {
        if let Some(s) = self.p9_slot() {
            s.dev.p9_mkdir(path);
        }
    }
    /// Every file in the share, for flushing to OPFS.
    pub fn p9_list(&self) -> alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<u8>)> {
        self.virtio
            .iter()
            .flatten()
            .find(|s| s.dev.device_id() == 9)
            .map(|s| s.dev.p9_list())
            .unwrap_or_default()
    }
    /// The share's mutation counter, or 0 if there is no share.
    pub fn p9_dirty(&self) -> u64 {
        self.virtio
            .iter()
            .flatten()
            .find(|s| s.dev.device_id() == 9)
            .map(|s| s.dev.p9_dirty())
            .unwrap_or(0)
    }

    /// Hand a restored virtio-blk device its backing store. Fails if no such
    /// device exists or the capacity disagrees with what the snapshot recorded
    /// — a size mismatch would surface much later as filesystem damage.
    pub fn attach_blk_backend(
        &mut self,
        backend: alloc::boxed::Box<dyn crate::virtio_blk::BlockBackend>,
    ) -> bool {
        for slot in self.virtio.iter_mut().flatten() {
            if slot.dev.device_id() == 2 {
                return slot.dev.attach_backend(backend);
            }
        }
        // No block device in any slot, so make one.
        //
        // Only `load_state` created one before, which meant a restored machine
        // had a disk and a cold-booted one silently did not: this returned
        // false, the caller had nothing to do about it, and the guest came up
        // with no /dev/vda at all. Creating it here has to happen before the
        // guest runs — virtio-mmio has no hotplug and Linux probes the slots
        // exactly once — which is the caller's job, and is why this is not a
        // lazy attach.
        self.attach_virtio(alloc::boxed::Box::new(crate::virtio_blk::VirtioBlk::new(
            backend,
        )))
        .is_some()
    }
}
