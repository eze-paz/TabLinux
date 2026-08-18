# Speed checklist — everything left to try

State when written (post-909f43c/1ebbf13): ~70 MIPS in-browser, 87% of
instructions compiled at ~6.5 ns, interpreter = 13.7% of wall.

**WITHDRAWN (2026-08-10): the "10x is not reachable / ceiling 150-300 MIPS /
claw toward 2-4x" framing that used to stand here.** It rested on a measurement
methodology now marked severely deficient (see the postmortem in
speed-guide.md): instruments that validated themselves against themselves,
measured a synthetic proxy, and failed to predict a ~10x user-facing regression
that shipped the same day they reported "+3%, verified." The ns/instruction
analysis bounds instruction throughput at best; the goal metric is wall-clock
user experience, where order-of-magnitude swings demonstrably live (the 5s ->
40-50s restore regression was one). No ceiling claim survives until an
end-to-end browser-path harness exists.

Rules of evidence are in speed.md and jit-plan.md and still apply: the box has
1.75x run-to-run spread, so nothing counts except `jit-ab.py` /
`boot-ab.py` interleaved medians, and anything under ~15% is unresolvable here.
For guest-side items the metric is wall clock to a marker (shell prompt, `apk
add` completion), not MIPS — a change that halves retired instructions halves
MIPS at constant wall time and is still a win.

## Measured so far (2026-08-09/10)

Baseline ~70 MIPS. Shipped, each verified by the full battery including a
byte-identical cold boot:

| change | effect | commit |
|---|---|---|
| chain cap 256 -> 1024 | +2.0% / +2.3% | 4719f0a |
| one-page sfence stops voiding the data TLB | +2.8% / +3.2% | a9b596b |

Instrumentation added: `chain_miss` (why chains stop), `tlb_miss` (why the
inlined TLB probe misses), `gen_bump` (what moved the generation word),
`chain-sweep.js`, `tlbprobe.js`, and `jit-fast-ab.js` -- a 100-second A/B
resolving ~2.8%, replacing the 40-minute one that could not resolve 15%.

**What measurement killed, before any of it was built:**

- **Per-privilege chain partitioning: dead.** Privilege causes exactly
  0.0% of chain probe misses. `chain_gen` is derived rather than bumped,
  so returning to a privilege level revalidates entries stamped under it,
  and a privilege transition traps out through uncompilable code, so no
  compiled block ever probes across the boundary.
- **Privilege killing the inlined TLB: dead** (0.3% of misses, now 1.0%).
  `tlbcount.py` carried this suspicion; it can be retired.
- **Chain-boundary overhead: closed.** Halving boundary crossings bought
  2%, so removing them entirely could not reach 4%. Megablocks are worth
  less than the plan assumed -- they now have to justify themselves on
  trace length alone, not on boundary cost.
- **ASIDs: downgraded, then partly obsoleted.** They address satp writes,
  which are 3.0% of generation bumps; the satp path already skips the
  flush when the written value is unchanged. The 96% case was one-page
  sfence, now fixed without ASIDs. What is left for them is the ~0.84M
  misses after real context switches -- roughly 1% of wall.

**Where the inlined TLB stands now:** 1.12M misses per 400M instructions,
about 1.0% of compiled accesses. Remaining causes: real address-space
switches 74.9% (ASIDs), capacity eviction 20.4% (`TLB_ENTRIES` is 1024 --
a one-constant experiment), everything else under 1.5%.

## Tier 1 — JIT levers with a measured target

- [ ] **Chain-survival instrumentation, then per-privilege partitioning.**
  REFRAMED after re-reading jit.rs:226 — `chain_gen` is *derived*
  (`1 + ((trans_gen << 2) | priv)`), so same-priv entries revalidate on
  return from a syscall; what actually kills entries is trans_gen bumps
  (satp per context switch) or slot eviction. Instrument the miss split
  first (guide §1.1); build the priv-in-slot-index fix (§1.2) only if
  eviction/priv is what the numbers blame — otherwise the fix is ASIDs.
- [ ] **Guest loops as wasm `loop` (megablocks v2).** A hot guest loop pays a
  chain probe + gen check + insn-count write per iteration. Detect back-edges
  to the block's own start (or within a superblock) and emit a real wasm
  `loop` with the interrupt budget check as the loop condition. Removes all
  per-iteration chain overhead. Biggest structural item; subsumes plain
  chain-merging.
- [ ] **Merge hot chains into single functions (megablocks v1).** Cheaper
  variant of the above without loop re-entry: a chain that always runs
  A→B→C becomes one function, killing per-hop probe/count/PC writes. Also
  amortizes TurboFan tier-up (module churn note in jit-plan).
- [ ] **Same-page TLB probe reuse.** Every load/store emits a full probe
  (~12–15 wasm ops + 2 loads). Stack traffic hits one page repeatedly; within
  a block, cache the last (page, host_base) per base register and skip the
  probe when the static offset stays in-page. Memory ops are ~25–30% of the
  mix. Must invalidate the cached pair at fault sites and gen checks.
- [ ] **Value forwarding.** If insn N writes rd and N+1 reads it, keep the
  value in a scratch local instead of regfile store+load. NOT the dead
  registers-in-locals lever — no prologue/epilogue, no allocator, regfile
  store still happens (or is sunk only when the next write provably kills
  it before any fault site). ABBA before believing anything.
- [ ] **Cheap interrupt-deliverability pre-check.** The 1ebbf13 unmask check
  costs ~10–15% because each *enable* runs `interrupt_pending`'s full PLIC
  refresh. Pre-check timer via `mtime >= stimecmp` (2 loads) and external via
  a cached `plic.any_pending()` maintained on `set_level`, only then do the
  full scan. Recovers most of that 10–15%.
- [ ] **Cold-block threshold, fresh A/B.** Measured negative pre-CSR, but
  traces went 46→217 since; dynamics changed. Low expectation (the cold bin
  was shown to be genuinely cold), cheap to rerun.
- [ ] **sfence.vma compiled** (no-op + gen bump, like fence but bumps
  trans_gen) and **mulh** (128-bit synthesis). ~1% each, ceiling ~2%. Do only
  after the big items, or as warm-up.

## Tier 2 — MMU architecture (helps JIT and interpreter alike)

- [ ] **ASIDs in the emulated MMU.** Advertise ASID bits in satp; Linux then
  stops flushing on context switch, so satp writes with an unchanged mapping
  stop bumping trans_gen — inline TLB entries and chain links survive
  context switches instead of dying on every one. Pairs with per-privilege
  chain tables; together they make gen bumps *rare* instead of per-switch.
  Needs: TLB entries keyed by (asid, vpage), sfence.vma honoring the asid
  argument. Verify with boot_to_userspace step count — behaviour must not
  change, only speed.
- [ ] **Superpage TLB entries.** CONFIRMED real: `TlbEntry` stores `size`
  (mmu.rs:18) but `lookup()` exact-matches `va >> 12` (mmu.rs:49), so a 2M
  mapping misses for its other 511 pages and the walker re-walks each. Fix
  the Rust TLB's lookup masking first, then the inline wasm TLB (guide §2.2).
- [ ] **Kernel linear-map fast path.** Sv39's linear map is
  virt = phys + constant. A compiled access whose address lands in that range
  could translate by subtraction — no probe at all. Fragile (must track the
  guest's actual mapping and permissions honestly); attempt only if the TLB
  numbers say misses in the linear range matter after superpages.

## Tier 3 — Speed up the OS itself (fewer instructions, same JIT)

- [ ] **Idle fast-forward — finish it, most of it EXISTS.** `Machine::run`
  already warps on WFI via `idle_skip_mtime` (lib.rs:245-269,
  device_bus.rs:339) capped at 1ms per jump. Remaining: verify it fires
  under the JIT (counter + host-CPU-at-idle check), adaptive cap raising
  with virtio-RX as an immediate-wake condition, and (only if measured
  real) spin-loop warp. Guide §3.1.
- [ ] **Kernel config diet.** The kernel is our workload; recompile it
  smaller: HZ=100 (each tick is a trap + timer reprogram + PLIC round),
  NO_HZ idle, PREEMPT_NONE, CONFIG_DEBUG_* off, unused drivers/subsystems
  out, LTO on. Every removed instruction is removed at 6.5 ns. Measure: wall
  clock to shell, retired-instruction count to the boot marker (the harness
  already counts steps — a config that boots in fewer *steps* wins before
  MIPS even enters).
- [ ] **Console batching.** Every UART byte is an MMIO store → bus dispatch →
  block truncation (MMIO is never in the fast path). `loglevel=4`/`quiet`
  during boot, and consider virtio-console with a ring instead of the UART
  for bulk output — printk storms currently serialize through the slowest
  path in the machine.
- [x] **Native SBI audit — CLOSED, already native.** supervisor.rs:385-391:
  there is no emulated M-mode OpenSBI; SBI calls land in sbi.rs host-side.
  Nothing to do.
- [ ] **Paravirtual bulk-memory ops.** A guest `clear_page`/`copy_page` is
  ~4096 emulated loads/stores; a PV SBI extension ("zero this pfn", "copy pfn
  A→B") is one host memset/memcpy — orders of magnitude on page-fault storms
  and fork/exec. Needs a tiny kernel patch (RISC-V has `sbi_ecall` plumbing;
  hook `clear_page`/`copy_page` when the extension probes present). Research
  grade but classic, and we control both sides.
- [ ] **initramfs / userspace trim.** Boot trims measured only ~5% (821aa47),
  so expectations low; musl already. Revisit only if a profile shows
  userspace init burning instructions somewhere specific.

## Tier 4 — Research grade

- [ ] **Persistent JIT cache.** Compiled `WebAssembly.Module`s are
  structured-cloneable to IndexedDB. Key modules by hash of the physical code
  bytes they were compiled from; on snapshot restore, preload — the guest
  wakes up already hot instead of re-warming ~8000 blocks. Kills the
  tier-up/warm-up phase entirely for the deployed page. Validate keys
  carefully: phys-keyed is only safe because fence.i flushes — persisted
  entries must revalidate against current guest memory bytes.
- [ ] **AOT translation of the kernel image.** Offline, translate the hot 90%
  of the kernel binary to wasm and ship it next to the snapshot — the JIT
  becomes a fallback for code the AOT pass didn't cover. Same revalidation
  story as above (alternatives patching rewrites kernel text early; AOT from
  the *post-patch* snapshot image sidesteps that).
- [ ] **Idiom recognition → bulk wasm ops.** Recognize the guest's
  memcpy/memset inner loops (musl + kernel have a handful of forms) and emit
  `memory.copy`/`memory.fill` or SIMD loads/stores for whole pages when the
  translation is contiguous. Overlaps with PV bulk-memory (Tier 3) — PV is
  easier and catches the kernel; this catches userspace without guest
  patches.
- [ ] **Wasm branch hinting.** The branch-hinting proposal (shipped in recent
  V8) marks fault-check branches unlikely; codegen currently leaves layout to
  the engine. Cheap to emit, plausibly a few %, needs ABBA on the real
  browser engine version.
- [ ] **SMP: a second hart.** wasm threads + SharedArrayBuffer (COI is already
  on in prod), one worker per hart. Honest cost: atomics/fences become real
  (the "one hart ⇒ plain load/store" codegen assumption dies), IPIs, and the
  kernel config grows SMP overhead that slows single-threaded work. Only
  pays on parallel workloads — apk/gcc -j2 — and probably nets negative for
  the interactive terminal. Park unless a workload demands it.
- [ ] **Macro-op fusion.** Fuse `lui+addi`, `auipc+ld`, `slli+srli` (
  zext idioms) at trace-build time into single wasm ops. Small per-pair, but
  these pairs are dense in compiler output; measure pair frequency in the
  existing histogram before building.

## Closed — do not retry without new evidence

Carried from jit-plan.md / the levers record so this file is self-contained:

- Registers in wasm locals: **4 attempts, 4 negatives**, TurboFan tier-up
  confirmed reached — the allocator already does this better.
- Forcing V8 tiers (`--no-liftoff` 0.65x) and tiering-budget tuning: natural
  tier-up is the sweet spot, and flags aren't browser-actionable anyway.
- Chain table 8x / TLB 8x: chains entered identical; collisions were never
  the limiter (privilege invalidation was — see Tier 1 item 1).
- Block threshold 16→4 (pre-CSR): cold code is genuinely cold.
- JIT-side data-translation cache in Rust: a cache in front of the TLB cost
  more than the call it skipped.
- div/rem codegen: 0.0% of wall. The fiddliest codegen for nothing.
- Excluding interrupt CSRs from compilation instead of the runtime unmask
  check: −43%.
- GPU offload of scalar emulation: wrong shape for WebGPU; not a candidate.
- **Paravirtual clear_page (2026-08-13): built, browser-measured −15%, reverted.**
  Intercept clear_page at its fixed entry PC (KASLR off) and zero the page with
  one host DRAM fill instead of the guest's ~512-store loop. Correct
  (byte-identical output with/without, 1609 pages zeroed) but a *net loss* on the
  real path: the JIT already **compiles** clear_page's loop, so the host-fill
  saving (~3µs/page × ~1200 pages ≈ 3ms on a 900ms fork workload, below the ~40%
  noise) is swamped by the per-run-loop-iteration PC check. The native analysis
  that ranked it high assumed the *interpreter's* clear_page cost, which the JIT
  had already removed — the exact gap the browser harness (`web/bench.html`,
  added the same day) exists to close. Bulk-memory PV only pays where the guest
  op is NOT compiled and dominates wall clock; find that with the harness first.
