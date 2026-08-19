/* @ts-self-types="./riscv_wasm.d.ts" */

export class Vm {
    static __wrap(ptr) {
        const obj = Object.create(Vm.prototype);
        obj.__wbg_ptr = ptr;
        VmFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        VmFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_vm_free(ptr, 0);
    }
    /**
     * Attach a virtio-9p shared folder with the given mount tag. Cold boot
     * only: on restore the device comes back from the snapshot at its slot, so
     * the page seeds it via `p9_put` instead of attaching a second one.
     * Returns false if no virtio slot was free.
     * @param {string} tag
     * @returns {boolean}
     */
    attach_9p(tag) {
        const ptr0 = passStringToWasm0(tag, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.vm_attach_9p(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * Attach a virtio-9p share served lazily from the host. Nothing is seeded;
     * the guest's listings and reads fault in via `p9_take_reqs`/`p9_supply`,
     * which the page backs with `fetch('/files/...')` and an OPFS walk. Cold
     * boot only, like `attach_9p`.
     * @param {string} tag
     * @returns {boolean}
     */
    attach_9p_lazy(tag) {
        const ptr0 = passStringToWasm0(tag, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.vm_attach_9p_lazy(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * Give the restored virtio-blk device its backing store.
     *
     * Must be called before the guest touches /dev/vda. virtio-mmio has no
     * hotplug — Linux probes the slots once at boot — so the disk has to have
     * been present when the snapshot was taken; this only re-binds the bytes.
     * Returns false if there is no such device or the size disagrees with the
     * snapshot.
     * @param {number} sectors
     * @param {Function} read_fn
     * @param {Function} write_fn
     * @returns {boolean}
     */
    attach_disk(sectors, read_fn, write_fn) {
        const ret = wasm.vm_attach_disk(this.__wbg_ptr, sectors, read_fn, write_fn);
        return ret !== 0;
    }
    /**
     * Attach a virtio-net device and return its MAC. Frames move through
     * `net_take` / `net_inject`; the host stack is JavaScript.
     * @param {Uint8Array} mac
     * @returns {boolean}
     */
    attach_net(mac) {
        const ptr0 = passArray8ToWasm0(mac, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.vm_attach_net(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * Diagnostic: why the compiled tail-call probe missed and why chains
     * ended. Order is `riscv_machine::CHAIN_MISS_BINS`; note that its bins are
     * not all one denominator, which the names spell out. Shares the
     * `interp_hist_enable` switch.
     * @returns {Float64Array}
     */
    chain_miss() {
        const ret = wasm.vm_chain_miss(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Console bytes produced since the last call. Raw, not UTF-8 decoded —
     * the kernel emits partial sequences and box-drawing characters, so
     * decoding belongs in the terminal on the JS side.
     * @returns {Uint8Array}
     */
    console() {
        const ret = wasm.vm_console(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * @returns {Float64Array}
     */
    diag() {
        const ret = wasm.vm_diag(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Guest RAM in MiB. Read it rather than assuming the value passed to the
     * constructor: a restored machine has the snapshot's size.
     * @returns {number}
     */
    dram_mb() {
        const ret = wasm.vm_dram_mb(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Diagnostic: what moved the translation generation, by cause. Order is
     * `riscv_supervisor::GEN_BUMP_CAUSES`. Always counted; these are rare
     * events on cold paths.
     * @returns {Float64Array}
     */
    gen_bump() {
        const ret = wasm.vm_gen_bump(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * How long the caller may sleep, in milliseconds, or 0 if the guest has
     * work to do. Valid immediately after `run` returns.
     * @returns {number}
     */
    idle_ms() {
        const ret = wasm.vm_idle_ms(this.__wbg_ptr);
        return ret;
    }
    /**
     * Diagnostic: how long the guest asked to wait each time it idled, binned.
     * Order is `riscv_machine::IDLE_WAIT_BINS`; last element is total guest
     * time skipped while idle, in seconds.
     * @returns {Float64Array}
     */
    idle_waits() {
        const ret = wasm.vm_idle_waits(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * @param {Uint8Array} bytes
     */
    input(bytes) {
        const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.vm_input(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * Diagnostic: interpreted instructions binned by why they landed in the
     * interpreter. Order is `riscv_machine::INTERP_BINS`. Empty unless
     * `interp_hist_enable(true)` was called.
     * @returns {Float64Array}
     */
    interp_hist() {
        const ret = wasm.vm_interp_hist(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Start binning interpreted instructions for `interp_hist`.
     *
     * Not free, which is why it is opt-in: each one is translated, fetched and
     * decoded a second time purely to be counted. Fine for a measurement run,
     * wasteful on every other one.
     * @param {boolean} on
     */
    interp_hist_enable(on) {
        wasm.vm_interp_hist_enable(this.__wbg_ptr, on);
    }
    /**
     * Build a wasm module for the pending blocks, installing itself into the
     * host's function table starting at `table_base`.
     *
     * Returns an empty vector if there was nothing to build. The blocks stay
     * queued until `jit_installed` confirms linking worked.
     * @param {number} table_base
     * @returns {Uint8Array}
     */
    jit_build(table_base) {
        const ret = wasm.vm_jit_build(this.__wbg_ptr, table_base);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Turn block compilation on. Off by default: the interpreter is the
     * known-good path, and the JIT should be opt-in until it has run in
     * anger.
     * @param {boolean} on
     */
    jit_enable(on) {
        wasm.vm_jit_enable(this.__wbg_ptr, on);
    }
    /**
     * Discard every compiled block.
     *
     * The host must also drop its module instances and clear the function
     * table slots, or the modules stay alive and the memory is not actually
     * reclaimed -- a table entry is a live reference.
     *
     * Cheap to do: blocks are re-formed from the guest's instruction stream
     * on demand, so this costs recompiling whatever is still hot. It is the
     * same thing `fence.i` already triggers.
     */
    jit_flush() {
        wasm.vm_jit_flush(this.__wbg_ptr);
    }
    /**
     * Diagnostic: whole-cache flushes (fence.i / restore) since start.
     * @returns {number}
     */
    jit_flushes() {
        const ret = wasm.vm_jit_flushes(this.__wbg_ptr);
        return ret;
    }
    /**
     * @returns {number}
     */
    jit_formed() {
        const ret = wasm.vm_jit_formed(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Diagnostic: [instructions compiled, instructions folded by macro-op
     * fusion] since process start. Coarse firing-rate probe for A/B analysis.
     * @returns {Uint32Array}
     */
    jit_fuse_stats() {
        const ret = wasm.vm_jit_fuse_stats(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * Diagnostic: [multi-access groups formed, of those strided (hoisted)].
     * Reach probe for TLB-probe hoisting.
     * @returns {Uint32Array}
     */
    jit_hoist_stats() {
        const ret = wasm.vm_jit_hoist_stats(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * Confirm the module built by `jit_build` was instantiated. Only now are
     * the blocks recorded as callable.
     * @param {number} table_base
     */
    jit_installed(table_base) {
        wasm.vm_jit_installed(this.__wbg_ptr, table_base);
    }
    /**
     * How many blocks have been installed, i.e. the next free table slot
     * relative to `jit_table_base`.
     * @returns {number}
     */
    jit_installed_count() {
        const ret = wasm.vm_jit_installed_count(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Diagnostic: [clean fall-through blocks, of those directly tail-call
     * linked to an in-batch same-page successor] since process start. Reach
     * probe for the TAILLINK lever.
     * @returns {Uint32Array}
     */
    jit_link_stats() {
        const ret = wasm.vm_jit_link_stats(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * How many blocks are decoded and waiting to be compiled.
     * @returns {number}
     */
    jit_pending() {
        const ret = wasm.vm_jit_pending(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Diagnostic: [reg-file loads emitted, reg-file stores emitted, guest
     * instructions compiled] since process start. Sizes register residency:
     * reg-file memory ops per compiled instruction.
     * @returns {Uint32Array}
     */
    jit_reg_stats() {
        const ret = wasm.vm_jit_reg_stats(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * Diagnostic: [total plain stores, memset-run stores, memcpy copy-pairs]
     * compiled. SIMD-reach probe.
     * @returns {Uint32Array}
     */
    jit_simd_stats() {
        const ret = wasm.vm_jit_simd_stats(this.__wbg_ptr);
        var v1 = getArrayU32FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * Block entries seen, entries that hit a compiled block, and the guest
     * instructions those hits covered. Enough to work out what fraction of
     * execution the JIT is actually carrying, and how many instructions it
     * gets per host round trip.
     * @returns {Float64Array}
     */
    jit_stats() {
        const ret = wasm.vm_jit_stats(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Emulated time, in the 10 MHz mtime ticks the devicetree declares.
     *
     * For measuring how much guest time a second of host time buys, which is
     * the only way to see whether an idle guest is being fast-forwarded or
     * emulated in real time.
     * @returns {number}
     */
    mtime() {
        const ret = wasm.vm_mtime(this.__wbg_ptr);
        return ret;
    }
    /**
     * @param {Uint8Array} frame
     */
    net_inject(frame) {
        const ptr0 = passArray8ToWasm0(frame, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.vm_net_inject(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * Frames the guest transmitted, concatenated with a 2-byte big-endian
     * length prefix each. One copy across the boundary beats one call per
     * frame, and a burst of small packets is the common case.
     * @returns {Uint8Array}
     */
    net_take() {
        const ret = wasm.vm_net_take(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Build a machine from images the page has already fetched.
     *
     * `dtb` is passed in rather than embedded because it encodes the memory
     * size and kernel command line, which the page chooses at boot. Its initrd
     * addresses are patched to wherever the initramfs actually lands.
     * @param {Uint8Array} kernel
     * @param {Uint8Array} initrd
     * @param {Uint8Array} dtb
     * @param {number} dram_mb
     */
    constructor(kernel, initrd, dtb, dram_mb) {
        const ptr0 = passArray8ToWasm0(kernel, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(initrd, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passArray8ToWasm0(dtb, wasm.__wbindgen_malloc);
        const len2 = WASM_VECTOR_LEN;
        const ret = wasm.vm_new(ptr0, len0, ptr1, len1, ptr2, len2, dram_mb);
        this.__wbg_ptr = ret;
        VmFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * Fine opcode-category histogram of interpreted instructions (all of them
     * when the JIT is off). See `riscv_machine::OP_HIST_BINS`.
     * @returns {Float64Array}
     */
    op_hist() {
        const ret = wasm.vm_op_hist(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * The share's mutation counter. The page polls it and flushes to OPFS when
     * it moves, so guest writes become real OPFS files.
     * @returns {number}
     */
    p9_dirty() {
        const ret = wasm.vm_p9_dirty(this.__wbg_ptr);
        return ret;
    }
    /**
     * Every file in the share, flattened for JS:
     *   [count u32]( [pathLen u32][path][dataLen u32][data] )*
     * The page slices this apart and writes each file to OPFS.
     * @returns {Uint8Array}
     */
    p9_list() {
        const ret = wasm.vm_p9_list(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Make a directory in the 9p share.
     * @param {string} path
     */
    p9_mkdir(path) {
        const ptr0 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.vm_p9_mkdir(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * Seed or overwrite a file in the 9p share from OPFS.
     * @param {string} path
     * @param {Uint8Array} data
     */
    p9_put(path, data) {
        const ptr0 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        wasm.vm_p9_put(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * Convert a restored 9p device to lazy on-demand mode. The cold path uses
     * `attach_9p_lazy`; a restored machine gets the device back from its
     * snapshot in seeded mode, so the page flips it here before the mount.
     */
    p9_set_lazy() {
        wasm.vm_p9_set_lazy(this.__wbg_ptr);
    }
    /**
     * Supply a fetched payload for a fault `p9_take_reqs` handed out: file bytes
     * for kind 0, a serialized listing for kind 1 (see `Virtio9p::apply_listing`:
     * `namelen[u16 LE] | name | flags[u8] bit0=dir | size[u64 LE]`, repeated).
     * Returns whether a blocked guest request was completed.
     * @param {number} id
     * @param {Uint8Array} payload
     * @returns {boolean}
     */
    p9_supply(id, payload) {
        const ptr0 = passArray8ToWasm0(payload, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.vm_p9_supply(this.__wbg_ptr, id, ptr0, len0);
        return ret !== 0;
    }
    /**
     * Drain guest mutations for write-back, flattened for JS:
     *   [count u32]( [op u8][pathLen u32][path][dataLen u32][data] )*
     * op 0 = write/create `path` with `data`; 1 = delete `path` (no data);
     * 2 = create directory `path` (no data). The page applies each to OPFS and
     * emits the matching file:changed / file:deleted so sync-core propagates it.
     * @returns {Uint8Array}
     */
    p9_take_changes() {
        const ret = wasm.vm_p9_take_changes(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Drain the pending lazy-9p faults, flattened for JS:
     *   [count u32]( [id u32][kind u8][off u64][len u32][pathLen u32][path] )*
     * kind 0 = read the file at `path` (whole file; off/len are the guest's
     * request, advisory); kind 1 = list directory `path` (empty = root).
     * @returns {Uint8Array}
     */
    p9_take_reqs() {
        const ret = wasm.vm_p9_take_reqs(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Chronological pc_trace ring (interpreter fetches only): flat pairs
     * [pc0, raw0, pc1, raw1, ...], oldest first.
     * @returns {Float64Array}
     */
    pc_trace() {
        const ret = wasm.vm_pc_trace(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Instructions attributed to the virtual-clock idle cycle (short spans
     * between WFI parks). Subtracting these from the step counter gives a
     * throughput number that does not read "idle" as "fast".
     * @returns {number}
     */
    prof_idle_insns() {
        const ret = wasm.vm_prof_idle_insns(this.__wbg_ptr);
        return ret;
    }
    /**
     * WFI parks by wake source: [timer-or-nothing, device]. Diagnostic for
     * the idle classifier.
     * @returns {Float64Array}
     */
    prof_parks() {
        const ret = wasm.vm_prof_parks(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Retired instructions by privilege level: [user, supervisor, _, machine].
     * Always counted (one add per chain or interpreted step); cumulative
     * since the machine was created — snapshots do not carry it.
     * @returns {Float64Array}
     */
    prof_priv() {
        const ret = wasm.vm_prof_priv(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Resume a machine from `Machine::save` output (kernels/shell.snap).
     * RAM already holds the booted system, so no images are needed — the
     * page skips minutes of interpreted boot. Returns undefined on a
     * version/format mismatch; regenerate with the make_snapshot example.
     * @param {Uint8Array} snapshot
     * @returns {Vm | undefined}
     */
    static restore(snapshot) {
        const ptr0 = passArray8ToWasm0(snapshot, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.vm_restore(ptr0, len0);
        return ret === 0 ? undefined : Vm.__wrap(ret);
    }
    /**
     * Execute up to `budget` instructions. Keep this at a few million: long
     * enough to amortise the call, short enough that the Worker stays
     * responsive to input and inbound frames.
     * @param {number} budget
     * @returns {number}
     */
    run(budget) {
        const ret = wasm.vm_run(this.__wbg_ptr, budget);
        return ret >>> 0;
    }
    /**
     * Serialise the running machine, for `Vm::restore`.
     *
     * The page uses this to cache a booted machine per RAM size: a cold boot
     * is minutes, and without somewhere to put the result every reload paid
     * that again. Returns undefined if the machine cannot be serialised.
     *
     * Only non-zero DRAM pages are stored, so a mostly-idle guest is far
     * smaller than its configured RAM.
     * @returns {Uint8Array | undefined}
     */
    save() {
        const ret = wasm.vm_save(this.__wbg_ptr);
        let v1;
        if (ret[0] !== 0) {
            v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v1;
    }
    /**
     * Instructions a compiled chain may retire before returning to the run
     * loop, which is the only place interrupts are checked. Raising it cuts
     * boundary crossings and raises worst-case interrupt latency.
     * @param {number} n
     */
    set_chain_max(n) {
        wasm.vm_set_chain_max(this.__wbg_ptr, n);
    }
    /**
     * Diagnostic snapshot of interrupt/CPU state, for repro harnesses only.
     * Layout: [pc, priv_level, mip, mie, sie(0/1), ext_pending(0/1),
     *          any_virtio_pending(0/1), mtime, stimecmp].
     * Tell the machine what time it is on the host, in nanoseconds, before
     * calling `run`. Any monotonic source will do; only differences are used.
     *
     * Supplying it holds the guest clock to real time and lets `run` return as
     * soon as the guest is genuinely waiting. Not supplying it keeps the old
     * behaviour, where an idle guest races its own clock and never yields.
     * @param {number} ns
     */
    set_host_ns(ns) {
        wasm.vm_set_host_ns(this.__wbg_ptr, ns);
    }
    /**
     * Toggle the deep-idle handback. The worker turns it off while network
     * traffic flows — see Machine::set_idle_handback for the reasoning.
     * @param {boolean} on
     */
    set_idle_handback(on) {
        wasm.vm_set_idle_handback(this.__wbg_ptr, on);
    }
    /**
     * Single-page sfence filter effectiveness: [skips, conservative hits].
     * A skip preserved the whole chain table; a hit took the old wholesale
     * invalidation because the flushed page might hold a chain key.
     * @returns {Float64Array}
     */
    sfence_filter() {
        const ret = wasm.vm_sfence_filter(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Diagnostic: what the block-cache slot held on a tag mismatch. Order is
     * `riscv_machine::jit::SLOT_BINS`.
     * @returns {Float64Array}
     */
    slot_state() {
        const ret = wasm.vm_slot_state(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * @returns {bigint}
     */
    steps() {
        const ret = wasm.vm_steps(this.__wbg_ptr);
        return BigInt.asUintN(64, ret);
    }
    /**
     * Diagnostic: why inlined-TLB probes missed. Order is
     * `riscv_machine::TLB_MISS_BINS`. Shares the `interp_hist_enable` switch.
     * @returns {Float64Array}
     */
    tlb_miss() {
        const ret = wasm.vm_tlb_miss(this.__wbg_ptr);
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
}
if (Symbol.dispose) Vm.prototype[Symbol.dispose] = Vm.prototype.free;
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_throw_344f42d3211c4765: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg_call_e3b662382210db98: function() { return handleError(function (arg0, arg1, arg2, arg3) {
            const ret = arg0.call(arg1, arg2, arg3);
            return ret;
        }, arguments); },
        __wbg_length_1f0964f4a5e2c6d8: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_new_with_length_e6785c33c8e4cce8: function(arg0) {
            const ret = new Uint8Array(arg0 >>> 0);
            return ret;
        },
        __wbg_prototypesetcall_4770620bbe4688a0: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
        },
        __wbg_set_4d7dd76f3dae2926: function(arg0, arg1, arg2) {
            arg0.set(getArrayU8FromWasm0(arg1, arg2));
        },
        __wbindgen_cast_0000000000000001: function(arg0) {
            // Cast intrinsic for `F64 -> Externref`.
            const ret = arg0;
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./riscv_wasm_bg.js": import0,
    };
}

const VmFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_vm_free(ptr, 1));

function addToExternrefTable0(obj) {
    const idx = wasm.__externref_table_alloc();
    wasm.__wbindgen_externrefs.set(idx, obj);
    return idx;
}

function getArrayF64FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getFloat64ArrayMemory0().subarray(ptr / 8, ptr / 8 + len);
}

function getArrayU32FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint32ArrayMemory0().subarray(ptr / 4, ptr / 4 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedFloat64ArrayMemory0 = null;
function getFloat64ArrayMemory0() {
    if (cachedFloat64ArrayMemory0 === null || cachedFloat64ArrayMemory0.byteLength === 0) {
        cachedFloat64ArrayMemory0 = new Float64Array(wasm.memory.buffer);
    }
    return cachedFloat64ArrayMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint32ArrayMemory0 = null;
function getUint32ArrayMemory0() {
    if (cachedUint32ArrayMemory0 === null || cachedUint32ArrayMemory0.byteLength === 0) {
        cachedUint32ArrayMemory0 = new Uint32Array(wasm.memory.buffer);
    }
    return cachedUint32ArrayMemory0;
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        const idx = addToExternrefTable0(e);
        wasm.__wbindgen_exn_store(idx);
    }
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedFloat64ArrayMemory0 = null;
    cachedUint32ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('riscv_wasm_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
// Appended by the publish script. The JIT links generated blocks against the
// host's own memory and function table, and those are not otherwise reachable
// from outside this module.
export { wasm as __wasm };
