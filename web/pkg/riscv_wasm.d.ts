/* tslint:disable */
/* eslint-disable */

export class Vm {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Attach a virtio-9p shared folder with the given mount tag. Cold boot
     * only: on restore the device comes back from the snapshot at its slot, so
     * the page seeds it via `p9_put` instead of attaching a second one.
     * Returns false if no virtio slot was free.
     */
    attach_9p(tag: string): boolean;
    /**
     * Attach a virtio-9p share served lazily from the host. Nothing is seeded;
     * the guest's listings and reads fault in via `p9_take_reqs`/`p9_supply`,
     * which the page backs with `fetch('/files/...')` and an OPFS walk. Cold
     * boot only, like `attach_9p`.
     */
    attach_9p_lazy(tag: string): boolean;
    /**
     * Give the restored virtio-blk device its backing store.
     *
     * Must be called before the guest touches /dev/vda. virtio-mmio has no
     * hotplug — Linux probes the slots once at boot — so the disk has to have
     * been present when the snapshot was taken; this only re-binds the bytes.
     * Returns false if there is no such device or the size disagrees with the
     * snapshot.
     */
    attach_disk(sectors: number, read_fn: Function, write_fn: Function): boolean;
    /**
     * Attach a virtio-net device and return its MAC. Frames move through
     * `net_take` / `net_inject`; the host stack is JavaScript.
     */
    attach_net(mac: Uint8Array): boolean;
    /**
     * Diagnostic: why the compiled tail-call probe missed and why chains
     * ended. Order is `riscv_machine::CHAIN_MISS_BINS`; note that its bins are
     * not all one denominator, which the names spell out. Shares the
     * `interp_hist_enable` switch.
     */
    chain_miss(): Float64Array;
    /**
     * Console bytes produced since the last call. Raw, not UTF-8 decoded —
     * the kernel emits partial sequences and box-drawing characters, so
     * decoding belongs in the terminal on the JS side.
     */
    console(): Uint8Array;
    diag(): Float64Array;
    /**
     * Guest RAM in MiB. Read it rather than assuming the value passed to the
     * constructor: a restored machine has the snapshot's size.
     */
    dram_mb(): number;
    /**
     * Diagnostic: what moved the translation generation, by cause. Order is
     * `riscv_supervisor::GEN_BUMP_CAUSES`. Always counted; these are rare
     * events on cold paths.
     */
    gen_bump(): Float64Array;
    /**
     * How long the caller may sleep, in milliseconds, or 0 if the guest has
     * work to do. Valid immediately after `run` returns.
     */
    idle_ms(): number;
    /**
     * Diagnostic: how long the guest asked to wait each time it idled, binned.
     * Order is `riscv_machine::IDLE_WAIT_BINS`; last element is total guest
     * time skipped while idle, in seconds.
     */
    idle_waits(): Float64Array;
    input(bytes: Uint8Array): void;
    /**
     * Diagnostic: interpreted instructions binned by why they landed in the
     * interpreter. Order is `riscv_machine::INTERP_BINS`. Empty unless
     * `interp_hist_enable(true)` was called.
     */
    interp_hist(): Float64Array;
    /**
     * Start binning interpreted instructions for `interp_hist`.
     *
     * Not free, which is why it is opt-in: each one is translated, fetched and
     * decoded a second time purely to be counted. Fine for a measurement run,
     * wasteful on every other one.
     */
    interp_hist_enable(on: boolean): void;
    /**
     * Build a wasm module for the pending blocks, installing itself into the
     * host's function table starting at `table_base`.
     *
     * Returns an empty vector if there was nothing to build. The blocks stay
     * queued until `jit_installed` confirms linking worked.
     */
    jit_build(table_base: number): Uint8Array;
    /**
     * Turn block compilation on. Off by default: the interpreter is the
     * known-good path, and the JIT should be opt-in until it has run in
     * anger.
     */
    jit_enable(on: boolean): void;
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
    jit_flush(): void;
    /**
     * Diagnostic: whole-cache flushes (fence.i / restore) since start.
     */
    jit_flushes(): number;
    jit_formed(): number;
    /**
     * Diagnostic: [instructions compiled, instructions folded by macro-op
     * fusion] since process start. Coarse firing-rate probe for A/B analysis.
     */
    jit_fuse_stats(): Uint32Array;
    /**
     * Diagnostic: [multi-access groups formed, of those strided (hoisted)].
     * Reach probe for TLB-probe hoisting.
     */
    jit_hoist_stats(): Uint32Array;
    /**
     * Confirm the module built by `jit_build` was instantiated. Only now are
     * the blocks recorded as callable.
     */
    jit_installed(table_base: number): void;
    /**
     * How many blocks have been installed, i.e. the next free table slot
     * relative to `jit_table_base`.
     */
    jit_installed_count(): number;
    /**
     * Diagnostic: [clean fall-through blocks, of those directly tail-call
     * linked to an in-batch same-page successor] since process start. Reach
     * probe for the TAILLINK lever.
     */
    jit_link_stats(): Uint32Array;
    /**
     * How many blocks are decoded and waiting to be compiled.
     */
    jit_pending(): number;
    /**
     * Diagnostic: [reg-file loads emitted, reg-file stores emitted, guest
     * instructions compiled] since process start. Sizes register residency:
     * reg-file memory ops per compiled instruction.
     */
    jit_reg_stats(): Uint32Array;
    /**
     * Diagnostic: [total plain stores, memset-run stores, memcpy copy-pairs]
     * compiled. SIMD-reach probe.
     */
    jit_simd_stats(): Uint32Array;
    /**
     * Block entries seen, entries that hit a compiled block, and the guest
     * instructions those hits covered. Enough to work out what fraction of
     * execution the JIT is actually carrying, and how many instructions it
     * gets per host round trip.
     */
    jit_stats(): Float64Array;
    /**
     * Emulated time, in the 10 MHz mtime ticks the devicetree declares.
     *
     * For measuring how much guest time a second of host time buys, which is
     * the only way to see whether an idle guest is being fast-forwarded or
     * emulated in real time.
     */
    mtime(): number;
    net_inject(frame: Uint8Array): void;
    /**
     * Frames the guest transmitted, concatenated with a 2-byte big-endian
     * length prefix each. One copy across the boundary beats one call per
     * frame, and a burst of small packets is the common case.
     */
    net_take(): Uint8Array;
    /**
     * Build a machine from images the page has already fetched.
     *
     * `dtb` is passed in rather than embedded because it encodes the memory
     * size and kernel command line, which the page chooses at boot. Its initrd
     * addresses are patched to wherever the initramfs actually lands.
     */
    constructor(kernel: Uint8Array, initrd: Uint8Array, dtb: Uint8Array, dram_mb: number);
    /**
     * Fine opcode-category histogram of interpreted instructions (all of them
     * when the JIT is off). See `riscv_machine::OP_HIST_BINS`.
     */
    op_hist(): Float64Array;
    /**
     * The share's mutation counter. The page polls it and flushes to OPFS when
     * it moves, so guest writes become real OPFS files.
     */
    p9_dirty(): number;
    /**
     * Every file in the share, flattened for JS:
     *   [count u32]( [pathLen u32][path][dataLen u32][data] )*
     * The page slices this apart and writes each file to OPFS.
     */
    p9_list(): Uint8Array;
    /**
     * Make a directory in the 9p share.
     */
    p9_mkdir(path: string): void;
    /**
     * Seed or overwrite a file in the 9p share from OPFS.
     */
    p9_put(path: string, data: Uint8Array): void;
    /**
     * Convert a restored 9p device to lazy on-demand mode. The cold path uses
     * `attach_9p_lazy`; a restored machine gets the device back from its
     * snapshot in seeded mode, so the page flips it here before the mount.
     */
    p9_set_lazy(): void;
    /**
     * Supply a fetched payload for a fault `p9_take_reqs` handed out: file bytes
     * for kind 0, a serialized listing for kind 1 (see `Virtio9p::apply_listing`:
     * `namelen[u16 LE] | name | flags[u8] bit0=dir | size[u64 LE]`, repeated).
     * Returns whether a blocked guest request was completed.
     */
    p9_supply(id: number, payload: Uint8Array): boolean;
    /**
     * Drain guest mutations for write-back, flattened for JS:
     *   [count u32]( [op u8][pathLen u32][path][dataLen u32][data] )*
     * op 0 = write/create `path` with `data`; 1 = delete `path` (no data);
     * 2 = create directory `path` (no data). The page applies each to OPFS and
     * emits the matching file:changed / file:deleted so sync-core propagates it.
     */
    p9_take_changes(): Uint8Array;
    /**
     * Drain the pending lazy-9p faults, flattened for JS:
     *   [count u32]( [id u32][kind u8][off u64][len u32][pathLen u32][path] )*
     * kind 0 = read the file at `path` (whole file; off/len are the guest's
     * request, advisory); kind 1 = list directory `path` (empty = root).
     */
    p9_take_reqs(): Uint8Array;
    /**
     * Chronological pc_trace ring (interpreter fetches only): flat pairs
     * [pc0, raw0, pc1, raw1, ...], oldest first.
     */
    pc_trace(): Float64Array;
    /**
     * Instructions attributed to the virtual-clock idle cycle (short spans
     * between WFI parks). Subtracting these from the step counter gives a
     * throughput number that does not read "idle" as "fast".
     */
    prof_idle_insns(): number;
    /**
     * WFI parks by wake source: [timer-or-nothing, device]. Diagnostic for
     * the idle classifier.
     */
    prof_parks(): Float64Array;
    /**
     * Retired instructions by privilege level: [user, supervisor, _, machine].
     * Always counted (one add per chain or interpreted step); cumulative
     * since the machine was created — snapshots do not carry it.
     */
    prof_priv(): Float64Array;
    /**
     * Resume a machine from `Machine::save` output (kernels/shell.snap).
     * RAM already holds the booted system, so no images are needed — the
     * page skips minutes of interpreted boot. Returns undefined on a
     * version/format mismatch; regenerate with the make_snapshot example.
     */
    static restore(snapshot: Uint8Array): Vm | undefined;
    /**
     * Execute up to `budget` instructions. Keep this at a few million: long
     * enough to amortise the call, short enough that the Worker stays
     * responsive to input and inbound frames.
     */
    run(budget: number): number;
    /**
     * Serialise the running machine, for `Vm::restore`.
     *
     * The page uses this to cache a booted machine per RAM size: a cold boot
     * is minutes, and without somewhere to put the result every reload paid
     * that again. Returns undefined if the machine cannot be serialised.
     *
     * Only non-zero DRAM pages are stored, so a mostly-idle guest is far
     * smaller than its configured RAM.
     */
    save(): Uint8Array | undefined;
    /**
     * Instructions a compiled chain may retire before returning to the run
     * loop, which is the only place interrupts are checked. Raising it cuts
     * boundary crossings and raises worst-case interrupt latency.
     */
    set_chain_max(n: number): void;
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
     */
    set_host_ns(ns: number): void;
    /**
     * Toggle the deep-idle handback. The worker turns it off while network
     * traffic flows — see Machine::set_idle_handback for the reasoning.
     */
    set_idle_handback(on: boolean): void;
    /**
     * Single-page sfence filter effectiveness: [skips, conservative hits].
     * A skip preserved the whole chain table; a hit took the old wholesale
     * invalidation because the flushed page might hold a chain key.
     */
    sfence_filter(): Float64Array;
    /**
     * Diagnostic: what the block-cache slot held on a tag mismatch. Order is
     * `riscv_machine::jit::SLOT_BINS`.
     */
    slot_state(): Float64Array;
    steps(): bigint;
    /**
     * Diagnostic: why inlined-TLB probes missed. Order is
     * `riscv_machine::TLB_MISS_BINS`. Shares the `interp_hist_enable` switch.
     */
    tlb_miss(): Float64Array;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __indirect_function_table: WebAssembly.Table;
    readonly __wbg_vm_free: (a: number, b: number) => void;
    readonly csr: (a: number, b: number, c: number, d: bigint, e: number, f: bigint) => void;
    readonly fp: (a: bigint, b: bigint, c: bigint) => void;
    readonly load16u: (a: bigint, b: bigint) => bigint;
    readonly load32u: (a: bigint, b: bigint) => bigint;
    readonly load64: (a: bigint, b: bigint) => bigint;
    readonly load8u: (a: bigint, b: bigint) => bigint;
    readonly store16: (a: bigint, b: bigint, c: bigint) => void;
    readonly store32: (a: bigint, b: bigint, c: bigint) => void;
    readonly store64: (a: bigint, b: bigint, c: bigint) => void;
    readonly store8: (a: bigint, b: bigint, c: bigint) => void;
    readonly vm_attach_9p: (a: number, b: number, c: number) => number;
    readonly vm_attach_9p_lazy: (a: number, b: number, c: number) => number;
    readonly vm_attach_disk: (a: number, b: number, c: any, d: any) => number;
    readonly vm_attach_net: (a: number, b: number, c: number) => number;
    readonly vm_chain_miss: (a: number) => [number, number];
    readonly vm_console: (a: number) => [number, number];
    readonly vm_diag: (a: number) => [number, number];
    readonly vm_dram_mb: (a: number) => number;
    readonly vm_gen_bump: (a: number) => [number, number];
    readonly vm_idle_ms: (a: number) => number;
    readonly vm_idle_waits: (a: number) => [number, number];
    readonly vm_input: (a: number, b: number, c: number) => void;
    readonly vm_interp_hist: (a: number) => [number, number];
    readonly vm_interp_hist_enable: (a: number, b: number) => void;
    readonly vm_jit_build: (a: number, b: number) => [number, number];
    readonly vm_jit_enable: (a: number, b: number) => void;
    readonly vm_jit_flush: (a: number) => void;
    readonly vm_jit_flushes: (a: number) => number;
    readonly vm_jit_formed: (a: number) => number;
    readonly vm_jit_fuse_stats: (a: number) => [number, number];
    readonly vm_jit_hoist_stats: (a: number) => [number, number];
    readonly vm_jit_installed: (a: number, b: number) => void;
    readonly vm_jit_installed_count: (a: number) => number;
    readonly vm_jit_link_stats: (a: number) => [number, number];
    readonly vm_jit_pending: (a: number) => number;
    readonly vm_jit_reg_stats: (a: number) => [number, number];
    readonly vm_jit_simd_stats: (a: number) => [number, number];
    readonly vm_jit_stats: (a: number) => [number, number];
    readonly vm_mtime: (a: number) => number;
    readonly vm_net_inject: (a: number, b: number, c: number) => void;
    readonly vm_net_take: (a: number) => [number, number];
    readonly vm_new: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => number;
    readonly vm_op_hist: (a: number) => [number, number];
    readonly vm_p9_dirty: (a: number) => number;
    readonly vm_p9_list: (a: number) => [number, number];
    readonly vm_p9_mkdir: (a: number, b: number, c: number) => void;
    readonly vm_p9_put: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly vm_p9_set_lazy: (a: number) => void;
    readonly vm_p9_supply: (a: number, b: number, c: number, d: number) => number;
    readonly vm_p9_take_changes: (a: number) => [number, number];
    readonly vm_p9_take_reqs: (a: number) => [number, number];
    readonly vm_pc_trace: (a: number) => [number, number];
    readonly vm_prof_idle_insns: (a: number) => number;
    readonly vm_prof_parks: (a: number) => [number, number];
    readonly vm_prof_priv: (a: number) => [number, number];
    readonly vm_restore: (a: number, b: number) => number;
    readonly vm_run: (a: number, b: number) => number;
    readonly vm_save: (a: number) => [number, number];
    readonly vm_set_chain_max: (a: number, b: number) => void;
    readonly vm_set_host_ns: (a: number, b: number) => void;
    readonly vm_set_idle_handback: (a: number, b: number) => void;
    readonly vm_sfence_filter: (a: number) => [number, number];
    readonly vm_slot_state: (a: number) => [number, number];
    readonly vm_steps: (a: number) => bigint;
    readonly vm_tlb_miss: (a: number) => [number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
