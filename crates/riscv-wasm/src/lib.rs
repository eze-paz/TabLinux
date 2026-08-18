//! Browser entry point.
//!
//! Deliberately thin: everything interesting lives in `riscv-machine`, which is
//! plain `no_std` Rust and testable natively. This file only moves bytes across
//! the wasm boundary.
//!
//! The caller drives the machine in slices — `run(budget)` returns after a fixed
//! number of instructions so the JS event loop keeps breathing. Nothing here
//! blocks, because a blocking call in a Worker would deadlock the very
//! `postMessage` traffic that feeds the console and the network.

extern crate alloc;
use riscv_devices::BlockBackend;
use riscv_machine::{BootImages, Machine};
use wasm_bindgen::prelude::*;

/// A disk whose bytes live in JavaScript — in practice an OPFS
/// `FileSystemSyncAccessHandle`.
///
/// This works only because that handle's `read`/`write` are genuinely
/// synchronous. Everything else in the OPFS API is promise-based, and a
/// promise cannot be awaited from inside a virtio request without unwinding
/// the whole emulator into an async state machine. The sync handle is also
/// Worker-only, which is a second reason the VM lives in a Worker.
struct JsDisk {
    sectors: u64,
    /// `(sector, count) -> Uint8Array`
    read_fn: js_sys::Function,
    /// `(sector, Uint8Array) -> void`
    write_fn: js_sys::Function,
}

const SECTOR: usize = 512;

impl BlockBackend for JsDisk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool {
        let count = (buf.len() / SECTOR) as u32;
        let got = match self.read_fn.call2(
            &JsValue::NULL,
            &JsValue::from_f64(sector as f64),
            &JsValue::from_f64(count as f64),
        ) {
            Ok(v) => js_sys::Uint8Array::from(v),
            Err(_) => return false,
        };
        if got.length() as usize != buf.len() {
            return false;
        }
        got.copy_to(buf);
        true
    }

    fn write(&mut self, sector: u64, buf: &[u8]) -> bool {
        let view = js_sys::Uint8Array::new_with_length(buf.len() as u32);
        view.copy_from(buf);
        self.write_fn
            .call2(&JsValue::NULL, &JsValue::from_f64(sector as f64), &view)
            .is_ok()
    }
}

#[wasm_bindgen]
pub struct Vm {
    m: Machine,
    /// Blocks handed to JS by jit_build, awaiting confirmation that the
    /// module linked. Kept separate from the queue so a failed instantiation
    /// does not leave blocks recorded at table slots that hold nothing.
    pending_install: Vec<(u64, u32, u32, bool)>,
}

#[wasm_bindgen]
impl Vm {
    /// Build a machine from images the page has already fetched.
    ///
    /// `dtb` is passed in rather than embedded because it encodes the memory
    /// size and kernel command line, which the page chooses at boot. Its initrd
    /// addresses are patched to wherever the initramfs actually lands.
    #[wasm_bindgen(constructor)]
    pub fn new(kernel: &[u8], initrd: &[u8], dtb: &[u8], dram_mb: u32) -> Vm {
        let mut m = Machine::new(BootImages {
            kernel,
            initrd,
            dtb,
            dram_bytes: dram_mb as usize * 1024 * 1024,
        });
        // The worker sleeps on deep-idle parks instead of spinning the WFI
        // cycle; native callers keep run-to-budget behaviour.
        m.idle_handback = true;
        Vm { m, pending_install: Vec::new() }
    }

    /// Resume a machine from `Machine::save` output (kernels/shell.snap).
    /// RAM already holds the booted system, so no images are needed — the
    /// page skips minutes of interpreted boot. Returns undefined on a
    /// version/format mismatch; regenerate with the make_snapshot example.
    pub fn restore(snapshot: &[u8]) -> Option<Vm> {
        let mut m = Machine::restore(snapshot)?;
        m.idle_handback = true;
        Some(Vm { m, pending_install: Vec::new() })
    }

    /// Serialise the running machine, for `Vm::restore`.
    ///
    /// The page uses this to cache a booted machine per RAM size: a cold boot
    /// is minutes, and without somewhere to put the result every reload paid
    /// that again. Returns undefined if the machine cannot be serialised.
    ///
    /// Only non-zero DRAM pages are stored, so a mostly-idle guest is far
    /// smaller than its configured RAM.
    pub fn save(&mut self) -> Option<Vec<u8>> {
        self.m.save().ok()
    }

    /// Give the restored virtio-blk device its backing store.
    ///
    /// Must be called before the guest touches /dev/vda. virtio-mmio has no
    /// hotplug — Linux probes the slots once at boot — so the disk has to have
    /// been present when the snapshot was taken; this only re-binds the bytes.
    /// Returns false if there is no such device or the size disagrees with the
    /// snapshot.
    pub fn attach_disk(&mut self, sectors: f64, read_fn: js_sys::Function, write_fn: js_sys::Function) -> bool {
        self.m.bus.attach_blk_backend(Box::new(JsDisk {
            sectors: sectors as u64,
            read_fn,
            write_fn,
        }))
    }

    /// Attach a virtio-9p shared folder with the given mount tag. Cold boot
    /// only: on restore the device comes back from the snapshot at its slot, so
    /// the page seeds it via `p9_put` instead of attaching a second one.
    /// Returns false if no virtio slot was free.
    pub fn attach_9p(&mut self, tag: &str) -> bool {
        self.m.bus.attach_9p(tag).is_some()
    }

    /// Seed or overwrite a file in the 9p share from OPFS.
    pub fn p9_put(&mut self, path: &str, data: &[u8]) {
        self.m.bus.p9_put(path, data);
    }

    /// Make a directory in the 9p share.
    pub fn p9_mkdir(&mut self, path: &str) {
        self.m.bus.p9_mkdir(path);
    }

    /// The share's mutation counter. The page polls it and flushes to OPFS when
    /// it moves, so guest writes become real OPFS files.
    pub fn p9_dirty(&self) -> f64 {
        self.m.bus.p9_dirty() as f64
    }

    /// Every file in the share, flattened for JS:
    ///   [count u32]( [pathLen u32][path][dataLen u32][data] )*
    /// The page slices this apart and writes each file to OPFS.
    pub fn p9_list(&self) -> Vec<u8> {
        let files = self.m.bus.p9_list();
        let mut out = Vec::new();
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, data) in files {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
        }
        out
    }

    /// Attach a virtio-9p share served lazily from the host. Nothing is seeded;
    /// the guest's listings and reads fault in via `p9_take_reqs`/`p9_supply`,
    /// which the page backs with `fetch('/files/...')` and an OPFS walk. Cold
    /// boot only, like `attach_9p`.
    pub fn attach_9p_lazy(&mut self, tag: &str) -> bool {
        self.m.bus.attach_9p_lazy(tag).is_some()
    }

    /// Convert a restored 9p device to lazy on-demand mode. The cold path uses
    /// `attach_9p_lazy`; a restored machine gets the device back from its
    /// snapshot in seeded mode, so the page flips it here before the mount.
    pub fn p9_set_lazy(&mut self) {
        self.m.bus.p9_set_lazy();
    }

    /// Drain the pending lazy-9p faults, flattened for JS:
    ///   [count u32]( [id u32][kind u8][off u64][len u32][pathLen u32][path] )*
    /// kind 0 = read the file at `path` (whole file; off/len are the guest's
    /// request, advisory); kind 1 = list directory `path` (empty = root).
    pub fn p9_take_reqs(&mut self) -> Vec<u8> {
        let reqs = self.m.bus.p9_take_reqs();
        let mut out = Vec::new();
        out.extend_from_slice(&(reqs.len() as u32).to_le_bytes());
        for r in reqs {
            out.extend_from_slice(&r.id.to_le_bytes());
            out.push(r.kind);
            out.extend_from_slice(&r.off.to_le_bytes());
            out.extend_from_slice(&r.len.to_le_bytes());
            out.extend_from_slice(&(r.path.len() as u32).to_le_bytes());
            out.extend_from_slice(r.path.as_bytes());
        }
        out
    }

    /// Drain guest mutations for write-back, flattened for JS:
    ///   [count u32]( [op u8][pathLen u32][path][dataLen u32][data] )*
    /// op 0 = write/create `path` with `data`; 1 = delete `path` (no data);
    /// 2 = create directory `path` (no data). The page applies each to OPFS and
    /// emits the matching file:changed / file:deleted so sync-core propagates it.
    pub fn p9_take_changes(&mut self) -> Vec<u8> {
        let changes = self.m.bus.p9_take_changes();
        let mut out = Vec::new();
        out.extend_from_slice(&(changes.len() as u32).to_le_bytes());
        for ch in changes {
            out.push(ch.op);
            out.extend_from_slice(&(ch.path.len() as u32).to_le_bytes());
            out.extend_from_slice(ch.path.as_bytes());
            out.extend_from_slice(&(ch.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&ch.data);
        }
        out
    }

    /// Supply a fetched payload for a fault `p9_take_reqs` handed out: file bytes
    /// for kind 0, a serialized listing for kind 1 (see `Virtio9p::apply_listing`:
    /// `namelen[u16 LE] | name | flags[u8] bit0=dir | size[u64 LE]`, repeated).
    /// Returns whether a blocked guest request was completed.
    pub fn p9_supply(&mut self, id: u32, payload: &[u8]) -> bool {
        self.m.bus.p9_supply(id, payload)
    }

    /// Execute up to `budget` instructions. Keep this at a few million: long
    /// enough to amortise the call, short enough that the Worker stays
    /// responsive to input and inbound frames.
    pub fn run(&mut self, budget: u32) -> u32 {
        // Compiled blocks call back into the memory path below, which has no
        // way to reach the machine through arguments. Park it here for the
        // duration; single-threaded, and a block only runs from inside this
        // call.
        unsafe { JIT_VM = &mut self.m as *mut Machine };
        let n = self.m.run(budget as u64) as u32;
        unsafe { JIT_VM = core::ptr::null_mut() };
        n
    }

    /// Turn block compilation on. Off by default: the interpreter is the
    /// known-good path, and the JIT should be opt-in until it has run in
    /// anger.
    pub fn jit_enable(&mut self, on: bool) {
        self.m.jit_enabled = on;
    }

    /// How many blocks are decoded and waiting to be compiled.
    pub fn jit_pending(&self) -> u32 {
        self.m.jit.pending_len() as u32
    }

    /// Diagnostic: [instructions compiled, instructions folded by macro-op
    /// fusion] since process start. Coarse firing-rate probe for A/B analysis.
    pub fn jit_fuse_stats(&self) -> Vec<u32> {
        let (t, h) = riscv_jit::fuse_stats();
        alloc::vec![t as u32, h as u32]
    }

    /// Diagnostic: [clean fall-through blocks, of those directly tail-call
    /// linked to an in-batch same-page successor] since process start. Reach
    /// probe for the TAILLINK lever.
    pub fn jit_link_stats(&self) -> Vec<u32> {
        let (t, h) = riscv_jit::link_stats();
        alloc::vec![t as u32, h as u32]
    }

    /// Diagnostic: [reg-file loads emitted, reg-file stores emitted, guest
    /// instructions compiled] since process start. Sizes register residency:
    /// reg-file memory ops per compiled instruction.
    pub fn jit_reg_stats(&self) -> Vec<u32> {
        let (l, s) = riscv_jit::reg_stats();
        let (compiled, _) = riscv_jit::fuse_stats();
        alloc::vec![l as u32, s as u32, compiled as u32]
    }

    /// Diagnostic: [multi-access groups formed, of those strided (hoisted)].
    /// Reach probe for TLB-probe hoisting.
    pub fn jit_hoist_stats(&self) -> Vec<u32> {
        let (t, s) = riscv_jit::hoist_stats();
        alloc::vec![t as u32, s as u32]
    }

    /// Diagnostic: whole-cache flushes (fence.i / restore) since start.
    pub fn jit_flushes(&self) -> f64 {
        self.m.jit.flushes as f64
    }

    /// Diagnostic: [total plain stores, memset-run stores, memcpy copy-pairs]
    /// compiled. SIMD-reach probe.
    pub fn jit_simd_stats(&self) -> Vec<u32> {
        let (t, m, c) = riscv_jit::simd_stats();
        alloc::vec![t as u32, m as u32, c as u32, riscv_jit::simd_vec_stores() as u32]
    }

    /// Build a wasm module for the pending blocks, installing itself into the
    /// host's function table starting at `table_base`.
    ///
    /// Returns an empty vector if there was nothing to build. The blocks stay
    /// queued until `jit_installed` confirms linking worked.
    pub fn jit_build(&mut self, table_base: u32) -> Vec<u8> {
        let pending = self.m.jit.take_pending();
        if pending.is_empty() {
            return Vec::new();
        }
        let runs: Vec<&[riscv_jit::Src]> = pending.iter().map(|(_, v)| &v[..]).collect();
        // Physical start of each pending run, so the compiler can find same-page
        // fall-through successors in this batch and link them directly.
        let paddrs: Vec<u64> = pending.iter().map(|(pa, _)| *pa).collect();
        // Baked in as constants. The table is allocated once and never
        // resized, so the addresses stay valid for the life of every block
        // compiled against them.
        let chain = riscv_jit::ChainCfg {
            base: self.m.jit.chain_base(),
            gen_addr: self.m.jit.chain_gen_addr(),
            entries: riscv_machine::jit::CHAIN_ENTRIES as u32,
            // The inlined TLB has its own generation word, so a single-page
            // sfence can void block chaining without voiding every cached
            // data translation with it.
            tlb: Some(riscv_jit::TlbCfg {
                read_base: self.m.jit.tlb_r_base(),
                write_base: self.m.jit.tlb_w_base(),
                entries: riscv_machine::jit::TLB_ENTRIES as u32,
                gen_addr: self.m.jit.data_gen_addr(),
            }),
            ras_base: self.m.jit.ras_base(),
            ras_sp_addr: self.m.jit.ras_sp_addr(),
            ras_entries: riscv_machine::jit::RAS_ENTRIES as u32,
        };
        // The f-register file and the FS word live at stable addresses for the
        // life of this machine (the Vm is boxed), same as every other baked
        // address above.
        let fp_cfg = riscv_jit::FpCfg {
            fregs_base: core::ptr::addr_of!(self.m.cpu.cpu.f) as u32,
            fs_word: self.m.jit.fs_word_addr(),
        };
        match riscv_jit::compile_many_into_table(&runs, &paddrs, table_base, Some(chain), Some(fp_cfg)) {
            Some((bytes, _covered)) => {
                self.pending_install = pending
                    .iter()
                    .map(|(pa, run)| {
                        let len: u32 = run.iter().map(|(_, w, _)| *w as u32).sum();
                        let branchy = run
                            .last()
                            .map(|(i, _, _)| riscv_jit::is_terminator(i))
                            .unwrap_or(false);
                        (*pa, len, run.len() as u32, branchy)
                    })
                    .collect();
                bytes
            }
            None => Vec::new(),
        }
    }

    /// Confirm the module built by `jit_build` was instantiated. Only now are
    /// the blocks recorded as callable.
    pub fn jit_installed(&mut self, table_base: u32) {
        if self.pending_install.is_empty() {
            return;
        }
        let paddrs: Vec<u64> = self.pending_install.iter().map(|(p, ..)| *p).collect();
        let bytes: Vec<u32> = self.pending_install.iter().map(|(_, b, ..)| *b).collect();
        let insns: Vec<u32> = self.pending_install.iter().map(|(_, _, i, _)| *i).collect();
        let branchy: Vec<bool> = self.pending_install.iter().map(|(.., b)| *b).collect();
        self.m.jit.installed(&paddrs, &bytes, &insns, &branchy, table_base);
        self.pending_install.clear();
    }

    /// How many blocks have been installed, i.e. the next free table slot
    /// relative to `jit_table_base`.
    pub fn jit_installed_count(&self) -> u32 {
        self.m.jit.installed_count()
    }

    /// Discard every compiled block.
    ///
    /// The host must also drop its module instances and clear the function
    /// table slots, or the modules stay alive and the memory is not actually
    /// reclaimed -- a table entry is a live reference.
    ///
    /// Cheap to do: blocks are re-formed from the guest's instruction stream
    /// on demand, so this costs recompiling whatever is still hot. It is the
    /// same thing `fence.i` already triggers.
    pub fn jit_flush(&mut self) {
        self.m.jit.flush();
        self.pending_install.clear();
    }

    pub fn jit_formed(&self) -> u32 {
        self.m.jit.formed as u32
    }

    /// Block entries seen, entries that hit a compiled block, and the guest
    /// instructions those hits covered. Enough to work out what fraction of
    /// execution the JIT is actually carrying, and how many instructions it
    /// gets per host round trip.
    pub fn jit_stats(&self) -> Vec<f64> {
        alloc::vec![
            self.m.jit.entries as f64,
            self.m.jit_chains as f64,
            self.m.jit_chain_insns as f64,
            self.m.jit.rejected as f64,
        ]
    }

    /// Guest RAM in MiB. Read it rather than assuming the value passed to the
    /// constructor: a restored machine has the snapshot's size.
    pub fn dram_mb(&self) -> u32 {
        (self.m.bus.dram_size() / (1024 * 1024)) as u32
    }

    /// Start binning interpreted instructions for `interp_hist`.
    ///
    /// Not free, which is why it is opt-in: each one is translated, fetched and
    /// decoded a second time purely to be counted. Fine for a measurement run,
    /// wasteful on every other one.
    pub fn interp_hist_enable(&mut self, on: bool) {
        self.m.interp_hist_on = on;
    }

    /// Diagnostic: interpreted instructions binned by why they landed in the
    /// interpreter. Order is `riscv_machine::INTERP_BINS`. Empty unless
    /// `interp_hist_enable(true)` was called.
    pub fn interp_hist(&self) -> Vec<f64> {
        self.m.interp_hist.iter().map(|&n| n as f64).collect()
    }

    /// Fine opcode-category histogram of interpreted instructions (all of them
    /// when the JIT is off). See `riscv_machine::OP_HIST_BINS`.
    pub fn op_hist(&self) -> Vec<f64> {
        self.m.op_hist.iter().map(|&n| n as f64).collect()
    }

    /// Instructions a compiled chain may retire before returning to the run
    /// loop, which is the only place interrupts are checked. Raising it cuts
    /// boundary crossings and raises worst-case interrupt latency.
    pub fn set_chain_max(&mut self, n: f64) {
        self.m.chain_max = (n as u64).max(1);
    }

    /// Diagnostic: why the compiled tail-call probe missed and why chains
    /// ended. Order is `riscv_machine::CHAIN_MISS_BINS`; note that its bins are
    /// not all one denominator, which the names spell out. Shares the
    /// `interp_hist_enable` switch.
    pub fn chain_miss(&self) -> Vec<f64> {
        self.m.chain_miss.iter().map(|&n| n as f64).collect()
    }

    /// Diagnostic: what the block-cache slot held on a tag mismatch. Order is
    /// `riscv_machine::jit::SLOT_BINS`.
    pub fn slot_state(&self) -> Vec<f64> {
        self.m.jit.slot_state.iter().map(|&n| n as f64).collect()
    }

    /// Diagnostic: what moved the translation generation, by cause. Order is
    /// `riscv_supervisor::GEN_BUMP_CAUSES`. Always counted; these are rare
    /// events on cold paths.
    pub fn gen_bump(&self) -> Vec<f64> {
        self.m.cpu.gen_bump.iter().map(|&n| n as f64).collect()
    }

    /// Diagnostic: why inlined-TLB probes missed. Order is
    /// `riscv_machine::TLB_MISS_BINS`. Shares the `interp_hist_enable` switch.
    pub fn tlb_miss(&self) -> Vec<f64> {
        self.m.tlb_miss.iter().map(|&n| n as f64).collect()
    }

    /// Single-page sfence filter effectiveness: [skips, conservative hits].
    /// A skip preserved the whole chain table; a hit took the old wholesale
    /// invalidation because the flushed page might hold a chain key.
    pub fn sfence_filter(&self) -> Vec<f64> {
        alloc::vec![self.m.jit.sfence_skips as f64, self.m.jit.sfence_hits as f64]
    }

    /// Retired instructions by privilege level: [user, supervisor, _, machine].
    /// Always counted (one add per chain or interpreted step); cumulative
    /// since the machine was created — snapshots do not carry it.
    pub fn prof_priv(&self) -> Vec<f64> {
        self.m.prof_insns.iter().map(|&n| n as f64).collect()
    }

    /// Instructions attributed to the virtual-clock idle cycle (short spans
    /// between WFI parks). Subtracting these from the step counter gives a
    /// throughput number that does not read "idle" as "fast".
    pub fn prof_idle_insns(&self) -> f64 {
        self.m.prof_idle_insns as f64
    }

    /// WFI parks by wake source: [timer-or-nothing, device]. Diagnostic for
    /// the idle classifier.
    pub fn prof_parks(&self) -> Vec<f64> {
        self.m.prof_parks.iter().map(|&n| n as f64).collect()
    }

    /// Toggle the deep-idle handback. The worker turns it off while network
    /// traffic flows — see Machine::set_idle_handback for the reasoning.
    pub fn set_idle_handback(&mut self, on: bool) {
        self.m.set_idle_handback(on);
    }

    /// Console bytes produced since the last call. Raw, not UTF-8 decoded —
    /// the kernel emits partial sequences and box-drawing characters, so
    /// decoding belongs in the terminal on the JS side.
    pub fn console(&mut self) -> Vec<u8> {
        self.m.take_console()
    }

    pub fn input(&mut self, bytes: &[u8]) {
        self.m.console_input(bytes);
    }

    pub fn steps(&self) -> u64 {
        self.m.steps
    }

    /// Diagnostic snapshot of interrupt/CPU state, for repro harnesses only.
    /// Layout: [pc, priv_level, mip, mie, sie(0/1), ext_pending(0/1),
    ///          any_virtio_pending(0/1), mtime, stimecmp].
    /// Tell the machine what time it is on the host, in nanoseconds, before
    /// calling `run`. Any monotonic source will do; only differences are used.
    ///
    /// Supplying it holds the guest clock to real time and lets `run` return as
    /// soon as the guest is genuinely waiting. Not supplying it keeps the old
    /// behaviour, where an idle guest races its own clock and never yields.
    pub fn set_host_ns(&mut self, ns: f64) {
        self.m.host_ns = ns as u64;
    }

    /// How long the caller may sleep, in milliseconds, or 0 if the guest has
    /// work to do. Valid immediately after `run` returns.
    pub fn idle_ms(&self) -> f64 {
        if self.m.idle_until == 0 {
            return 0.0;
        }
        let now = self.m.bus.diag_mtime();
        if self.m.idle_until <= now {
            return 0.0;
        }
        // 10 MHz: 10_000 ticks per millisecond.
        (self.m.idle_until - now) as f64 / 10_000.0
    }

    /// Diagnostic: how long the guest asked to wait each time it idled, binned.
    /// Order is `riscv_machine::IDLE_WAIT_BINS`; last element is total guest
    /// time skipped while idle, in seconds.
    pub fn idle_waits(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.m.idle_waits.iter().map(|&n| n as f64).collect();
        v.push(self.m.idle_skipped as f64 / 10_000_000.0);
        v
    }

    /// Emulated time, in the 10 MHz mtime ticks the devicetree declares.
    ///
    /// For measuring how much guest time a second of host time buys, which is
    /// the only way to see whether an idle guest is being fast-forwarded or
    /// emulated in real time.
    pub fn mtime(&self) -> f64 {
        self.m.bus.diag_mtime() as f64
    }

    pub fn diag(&mut self) -> Vec<f64> {
        let s = &self.m.cpu;
        let pc = s.cpu.pc as f64;
        let prv = s.priv_level as u8 as f64;
        let mip = s.mip as f64;
        let mie = s.mie as f64;
        let sie = if s.mstatus.sie { 1.0 } else { 0.0 };
        let mtime = self.m.bus.diag_mtime() as f64;
        let stimecmp = s.stimecmp as f64;
        let ext = if self.m.bus.external_interrupt_pending() { 1.0 } else { 0.0 };
        let vpend = if self.m.bus.any_virtio_pending() { 1.0 } else { 0.0 };
        vec![pc, prv, mip, mie, sie, ext, vpend, mtime, stimecmp]
    }

    /// Chronological pc_trace ring (interpreter fetches only): flat pairs
    /// [pc0, raw0, pc1, raw1, ...], oldest first.
    pub fn pc_trace(&self) -> Vec<f64> {
        let s = &self.m.cpu;
        let mut out = Vec::with_capacity(2048);
        for k in 0..1024usize {
            let idx = s.pc_trace_idx.wrapping_add(k) % 1024;
            let (pc, raw) = s.pc_trace[idx];
            out.push(pc as f64);
            out.push(raw as f64);
        }
        out
    }

    /// Attach a virtio-net device and return its MAC. Frames move through
    /// `net_take` / `net_inject`; the host stack is JavaScript.
    pub fn attach_net(&mut self, mac: &[u8]) -> bool {
        if mac.len() != 6 {
            return false;
        }
        let mut m = [0u8; 6];
        m.copy_from_slice(mac);
        self.m.bus.attach_virtio_net(m).is_some()
    }

    /// Frames the guest transmitted, concatenated with a 2-byte big-endian
    /// length prefix each. One copy across the boundary beats one call per
    /// frame, and a burst of small packets is the common case.
    pub fn net_take(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(q) = self.m.bus.net.clone() {
            let mut q = q.borrow_mut();
            while let Some(f) = q.to_host.pop_front() {
                if f.len() > u16::MAX as usize {
                    continue;
                }
                out.extend_from_slice(&(f.len() as u16).to_be_bytes());
                out.extend_from_slice(&f);
            }
        }
        out
    }

    pub fn net_inject(&mut self, frame: &[u8]) {
        self.m.bus.net_inject_frame(frame);
    }
}


/// The machine a running compiled block belongs to.
///
/// Null except inside `Vm::run`. Compiled blocks are reached only from that
/// call, and wasm is single-threaded, so no block can observe it null or stale.
static mut JIT_VM: *mut Machine = core::ptr::null_mut();

use riscv_core::execute::Bus as _;

/// Why the inlined TLB probe missed, for the access the host is now servicing.
///
/// Reads the same entry generated code just rejected. Must be called before
/// `fill_tlb` refills it. See `TLB_MISS_BINS`.
fn classify_tlb_miss(
    m: &Machine,
    addr: u64,
    store: bool,
    size: u64,
    mmio: bool,
    faulted: bool,
) -> usize {
    if faulted {
        return 8;
    }
    if mmio {
        return 7;
    }
    let tlb = if store { &m.jit.tlb_w } else { &m.jit.tlb_r };
    let e = tlb[((addr >> 12) as usize) & (riscv_machine::jit::TLB_ENTRIES - 1)];
    // Generation 0 is the zeroed-table marker; live generations start at 1.
    if e.gen == 0 {
        return 0;
    }
    if e.vpn != addr >> 12 {
        return 1;
    }
    // Against the TLB's own generation word, not the chain table's. They hold
    // different values now that a single-page sfence moves only one of them,
    // and comparing with the wrong one reports every entry as stale.
    if e.gen != m.jit.data_gen {
        // Undo `refresh_gen`'s packing to see which half moved.
        let (was_t, was_p) = ((e.gen - 1) >> 2, (e.gen - 1) & 3);
        let (now_t, now_p) = ((m.jit.data_gen - 1) >> 2, (m.jit.data_gen - 1) & 3);
        return match (was_t != now_t, was_p != now_p) {
            (false, true) => 2,
            (true, false) => 3,
            _ => 4,
        };
    }
    // The entry was live, so the probe's third test is what rejected it: a
    // multi-byte access running off the end of the page, which the host must
    // service because the next page is elsewhere in linear memory.
    if (addr & 0xFFF) + size > 4096 {
        5
    } else {
        6
    }
}

/// Guest memory accesses made by compiled code.
///
/// A fault does not trap the wasm: that would unwind out of `Machine::run`
/// entirely rather than just the block. Instead the flag the generated code
/// checks after every access is set, and the faulting guest PC recorded so the
/// interpreter can resume there and take the trap the ordinary way.
macro_rules! jit_load {
    ($name:ident, $read:ident, $ty:ty) => {
        #[no_mangle]
        pub extern "C" fn $name(addr: u64, pc: u64) -> u64 {
            unsafe {
                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return 0,
                };
                match m.cpu.debug_translate(
                    &mut m.bus,
                    riscv_supervisor::AccessType::Load,
                    addr,
                ) {
                    Ok(pa) => {
                        // Cache it, unless it is MMIO -- those must keep
                        // reaching the bus, since reading one has side effects
                        // and its bytes are not in linear memory.
                        let page = m.bus.dram_page_host_addr(pa);
                        if m.interp_hist_on {
                            let b = classify_tlb_miss(
                                m, addr, false,
                                core::mem::size_of::<$ty>() as u64,
                                page.is_none(), false,
                            );
                            m.tlb_miss[b] += 1;
                        }
                        if let Some(page) = page {
                            m.jit.fill_tlb(addr, page, false);
                        }
                        m.bus.$read(pa) as u64
                    }
                    Err(_) => {
                        if m.interp_hist_on {
                            let b = classify_tlb_miss(m, addr, false, 0, false, true);
                            m.tlb_miss[b] += 1;
                        }
                        m.cpu.cpu.jit_fault = 1;
                        m.jit_fault_pc = pc;
                        0
                    }
                }
            }
        }
    };
}

macro_rules! jit_store {
    ($name:ident, $write:ident, $ty:ty) => {
        #[no_mangle]
        pub extern "C" fn $name(addr: u64, val: u64, pc: u64) {
            unsafe {
                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return,
                };
                match m.cpu.debug_translate(
                    &mut m.bus,
                    riscv_supervisor::AccessType::Store,
                    addr,
                ) {
                    Ok(pa) => {
                        let page = m.bus.dram_page_host_addr(pa);
                        if m.interp_hist_on {
                            let b = classify_tlb_miss(
                                m, addr, true,
                                core::mem::size_of::<$ty>() as u64,
                                page.is_none(), false,
                            );
                            m.tlb_miss[b] += 1;
                        }
                        if let Some(page) = page {
                            // A store to a page that holds compiled blocks is
                            // self-modifying code: flag it so the next fence.i
                            // discards, and do NOT cache the page for writes so
                            // every further store to it keeps reaching here.
                            if m.jit.is_code_page(pa >> 12) {
                                m.jit.note_code_write();
                            } else {
                                m.jit.fill_tlb(addr, page, true);
                            }
                        }
                        m.bus.$write(pa, val as $ty)
                    }
                    Err(_) => {
                        if m.interp_hist_on {
                            let b = classify_tlb_miss(m, addr, true, 0, false, true);
                            m.tlb_miss[b] += 1;
                        }
                        m.cpu.cpu.jit_fault = 1;
                        m.jit_fault_pc = pc;
                    }
                }
            }
        }
    };
}

/// The whole F/D extension for compiled code, one call per instruction.
///
/// Runs the interpreter's own FP paths (`Supervisor::fp_jit`), so results are
/// identical to stepping by construction. A bail -- FS off, an illegal
/// encoding, a page fault -- commits nothing and rewinds to this instruction's
/// own pc, where the interpreter re-runs it and takes the trap normally.
#[no_mangle]
pub extern "C" fn fp(packed: u64, arg: u64, pc: u64) {
    unsafe {
        let m = match JIT_VM.as_mut() {
            Some(m) => m,
            None => return,
        };
        let kind = (packed & 0xff) as u8;
        let r1 = ((packed >> 8) & 0xff) as u8;
        let r2 = ((packed >> 16) & 0xff) as u8;
        if m.cpu.fp_jit(&mut m.bus, kind, r1, r2, arg) {
            m.cpu.cpu.jit_fault = 1;
            m.jit_fault_pc = pc;
        } else {
            // The op may have moved FS to Dirty; the inline fast paths are
            // gated on the word this refreshes.
            m.jit.refresh_gen(&m.cpu);
        }
    }
}

/// CSR read/modify/write for compiled code.
///
/// A block calls this instead of ending before every csrrw/csrrs/csrrc the way
/// it used to — the interpreter round trip and the trace it broke were ~6% of
/// wall time. Like a load, a trap does not unwind the wasm: the flag is set and
/// the interpreter re-runs the CSR at `pc` to take it.
///
/// `refresh_gen` afterwards is the load-bearing line. A write to satp (or any
/// translation-affecting CSR) bumps `trans_gen`, and the generation word it
/// recomputes is shared by the inline TLB and the chain probe. Recomputing it
/// here means every later access in this block re-validates against the new
/// address space, and the block's chain probe misses and returns to the run
/// loop for a fresh lookup instead of chaining into a block compiled for the
/// old mapping.
#[no_mangle]
pub extern "C" fn csr(csr: u32, rd: u32, src: u32, val: u64, kind: u32, pc: u64) {
    unsafe {
        let m = match JIT_VM.as_mut() {
            Some(m) => m,
            None => return,
        };
        let trapped = m.cpu.csr_jit(&mut m.bus, csr as u16, rd as u8, src as u8, val, kind as u8);
        if trapped {
            m.cpu.cpu.jit_fault = 1;
            m.jit_fault_pc = pc;
            return;
        }
        // A write to an interrupt-control CSR can unmask an already-pending
        // interrupt (setting `sstatus.SIE`, or an enable bit while `mip` is
        // set). Compiled code only checks for interrupts at chain boundaries,
        // so a `local_irq_enable(); …; local_irq_disable()` window entirely
        // inside one block would close before the next boundary and the
        // interrupt pending across it would never be delivered — the guest
        // wedges with SIE=0 and `mip` set, which is exactly what a read-write
        // ext4 mount's writeback triggered. Bail to the interpreter, which
        // takes the trap on its next step, whenever the write left an interrupt
        // deliverable. `interrupt_pending` returns false in the far more common
        // case (a disable, or nothing pending), so the CSR stays fully compiled
        // there — the reason this is a runtime check and not an exclusion from
        // `is_compilable_csr`, which cost ~40% of JIT MIPS.
        // `mstatus.sie` gates every S-mode interrupt, and the guest only runs
        // in S/U mode. When it is clear after the write, nothing is deliverable,
        // so the expensive `interrupt_pending` (a PLIC scan) is skipped — this
        // drops the whole disable half of every `local_irq_save`/`restore`
        // pair, the hottest interrupt-CSR traffic there is. The window the bug
        // needs is precisely an enable, where `sie` is set here.
        if riscv_jit::is_interrupt_csr(csr as u16)
            && m.cpu.mstatus.sie
            && m.cpu.interrupt_pending(&mut m.bus)
        {
            m.cpu.cpu.jit_fault = 1;
            // Resume AFTER the CSR: it already committed inside `csr_jit`, so
            // re-running it would double-apply the write. Every CSR encoding is
            // four bytes (none are in the compressed set).
            m.jit_fault_pc = pc.wrapping_add(4);
            return;
        }
        m.jit.refresh_gen(&m.cpu);
    }
}

jit_load!(load8u, read_u8, u8);
jit_load!(load16u, read_u16, u16);
jit_load!(load32u, read_u32, u32);
jit_load!(load64, read_u64, u64);
jit_store!(store8, write_u8, u8);
jit_store!(store16, write_u16, u16);
jit_store!(store32, write_u32, u32);
jit_store!(store64, write_u64, u64);


/// f64 because wasm-bindgen has no clean u64 vector; these are counters, and
/// f64 is exact to 2^53 which is far past any plausible run.
fn alloc_stats(entries: u64, hits: u64, hit_insns: u64, rejected: u64) -> Vec<f64> {
    alloc::vec![entries as f64, hits as f64, hit_insns as f64, rejected as f64]
}
