# riscv-jit optimization ledger

Every performance lever attempted on the block JIT, with its measured outcome, so
none of them get re-proposed as "new ideas." Read this before suggesting or
building a JIT speedup. Dates are 2026; the box is a noisy Windows/WSL laptop, so
only within-session A/B ratios are trustworthy (never cross-session absolute MIPS).

Status key: **SHIPPED** (on in prod) · **NULL** (correct, no resolvable win, flag-gated off) ·
**REVERTED** (net-negative, removed) · **ANSWERED** (measured question, no code) · **UNBUILT** (not yet tried).

## WHERE THE TIME ACTUALLY GOES (measured, V8 --prof, 2026-08-17)
Wall-time attribution by function (`node --prof` on a single-VM run, `--prof-process`, then bucket the
`[JavaScript]` ticks: `wasm-function[N]` = JIT block bodies; named `riscv_*` = host machinery). This is
the map every future lever should be aimed at.

- **CPython (pybench):** block bodies **23%**, interpreter fallback **27%** (mostly *cold* = not-yet-hot
  blocks), MMU-translate/block-entry **~14%**, run loop **8%**, device/interrupt poll **12%**,
  compilation 6%. Only ~23% is TF-compiled guest code; ~77% is machinery around it, because CPython's
  calls/returns/branches make **short chains → constant block transitions**, so every per-boundary cost
  explodes.
- **md5sum (compute):** block bodies **58%**, seams ~22%, interp 9%. Long compute loops → long chains →
  bodies dominate.

Consequences: (1) TurboFan is NOT the bottleneck — it does a good job on the 23–58% it compiles; that's
why the five bytecode levers were null. (2) The wins are in the machinery: the **cold interpreter tail**
(block cache / faster warmup), **fewer block boundaries** (region formation), and **per-boundary work**
(device-poll cadence, cheaper block entry). Aim here, and re-run `--prof` after each change to watch the
bucket move. Harnesses: `mips.js` / `pybench/pybench-vm.js` under `node --prof`.

## The things that actually moved the needle
- **Selective `fence.i` flush (spurious-flush elimination)** — SHIPPED, the **biggest single cut to the
  cold-interpreter tail** on CPython. The guest issued `fence.i` **~799×/run**; each one bumped
  `icache_gen`, and the run loop responded with a full `Jit::flush()` (discard **every** compiled block).
  Those flushes were **spurious** — the guest was not modifying live code (`fence.i` from `ld.so`/dlopen
  and glibc's over-eager i-cache sync), so re-warming ~800× rebuilt the exact same blocks. Knockout
  (never flush) → cold interp **51.3M → 15.4M** with **byte-identical** output, proving the flushes lost
  nothing. Fix: track which physical pages actually hold compiled code (`code_pages: BTreeSet<ppn>`, set
  in `Jit::installed`, cleared in `flush`); a store's shim (riscv-wasm `jit_store`) calls
  `note_code_write()` (sets `smc_dirty`) only when it targets a code page, and the write-TLB is dropped
  when a page *newly* becomes code so subsequent stores re-resolve through the host. The run loop
  (`SELECTIVE_FENCEI=true`, riscv-machine/lib.rs) then flushes on `icache_gen` change **only if
  `smc_dirty`**. Result: **flushes 799 → 0**, cold **15.4M**, CHECK byte-identical. Wall-time profile
  moved decisively: **block bodies 23% → 57%, interp fallback 27% → 12%** (compiled code is now the
  majority of wall). Same "remove real redundant WORK" category as ASID/batched-poll. LIMITATION:
  interpreter-executed SMC stores are not flagged (only the JIT store shim is); harmless for
  CPython/normal software, and `SELECTIVE_FENCEI=false` is the exact-old-behaviour fallback. Knob:
  `SELECTIVE_FENCEI`.
- **Batched interpreter interrupt/device poll** — SHIPPED, worth **~1.5–2× on CPython** (the campaign's
  biggest win after the inline TLB, and it came straight out of the `--prof` wall-time map above). The
  interpreter's `step()` polled the CLINT timer + PLIC (`check_timer_interrupt`/`read_mtime`/
  `check_external_interrupt`, incl. `virtio_tick` work) on **every single interpreted instruction** —
  the dominant reason each interpreted op cost ~8× a compiled one. Now `sync_device_interrupts` is
  batched behind `int_sync_countdown` (`INT_SYNC_INTERVAL=64`); freshness is preserved where delivery
  can actually happen — the run loop's `interrupt_pending` at every block boundary, and a forced sync
  before any WFI park — so it's pure interrupt-latency batching (≤64 interpreted insns, far under the
  compiled path's 8192 chain budget). Correct: jit-vm-test interp-vs-JIT identical, pybench CHECK
  byte-identical, **cold boot completes** (the timer/WFI/device stress). Profile after: parse_day
  interpreter 30%→6%, device 13%→4%, translate 17%→4%; block bodies became the majority. This is the
  "remove real per-instruction work" category, same as ASID keying but far bigger. Knob:
  `INT_SYNC_INTERVAL`. (supervisor.rs `sync_device_interrupts`)
- **Single-entry instruction-fetch page cache** — SHIPPED, worth **~+9% on interpreter-only
  execution** (parse_day interpreter window, ABBA +7.3% / +10.6%, 9.67→10.6 MIPS; byte-identical
  CHECK). Also came out of the `--prof` map: the interpreter re-ran the full `translate` (satp mode
  → software TLB → permission/privilege checks) on **every** fetch, but fetches are sequential within
  a page. Now `step()` calls `fetch_translate`, which compares `pc>>12` against a cached
  `{fetch_vpn, fetch_priv, fetch_ppn}` and returns `ppn|offset` on a hit, doing the full walk only on
  a page/privilege change. Keyed by privilege so a U/S switch misses (never serves a wrong-permission
  page); populated only after a successful fetch-translate (so the page is executable + A-set);
  invalidated at exactly the software-TLB flush points (fence.i via `dcache_flush`, sfence.vma, satp
  write, restore). Zero cost on the compiled path (compiled blocks never call `step()`; mapping
  changes are all uncompiled block-enders, so the cache can't go stale mid-block). Correct:
  jit-vm-test identical, pybench + parse_day CHECK byte-identical, **cold boot interp-vs-JIT
  byte-identical (11974 B, both reach the shell)** — the strongest gate, exercising satp/sfence/priv/
  WFI through a full kernel boot. BLAST RADIUS = the interpreter's share of wall: meaningful during
  cold start / interpreter-heavy phases, **below run-to-run noise on JIT-saturated steady state**
  (full pybench base 3279–3968ms ±21% swamps the ~1%-of-wall interpreter). Never hurts (a miss is one
  extra branch before the same translate), so shipped ON without a flag. (supervisor.rs
  `fetch_translate` / `fetch_vpn`)
- **Inline software TLB** — SHIPPED, worth **~2.3× overall** (awk/sh 3×, gzip 2.5×). Every guest
  load/store probes an in-linear-memory direct-mapped TLB and hits without a host call.
  This is the big win and it is spent — knockout confirms no remaining headroom in it.
  See `TlbCfg`, `tlb_probe`. (reference_riscv-jit-inline-tlb-shipped)
- **chain_max = 8192** — SHIPPED, **1.2–1.5× across all workloads**. Raising the max blocks-per-host-entry.
  Runtime knob, no rebuild; trades interrupt latency. Coverage was NOT the lever here;
  real causes were gen-trans churn, budget, cache thrash. (reference_riscv-jit-chainmax-win)
- **Group-CSE of same-base load/store probes** (`GROUP_CSE_ON`) — SHIPPED (on). Shares one TLB
  probe across a run of accesses with the same base register.

## V8 codegen tier — ANSWERED, no free MIPS there
- Hot JIT blocks **DO reach TurboFan** under default settings (trace: TurboFan compilations in 28
  of ~60 modules; timing: hot-block tier-up worth **~+18%**, host tier-up ~+23%).
- Tiering is already well-tuned: lowering `--wasm-tiering-budget` (200k/20k) does NOT help (compile
  churn on short-lived block modules); `--no-liftoff` (eager TurboFan) is ~3× SLOWER / OOMs.
- **Consequence:** the hot code is optimized machine code, so hand-optimizing the wasm *bytecode*
  for things TurboFan already does (constant folding, its own regalloc, dead-store elimination) is
  redundant. The 10× is NOT in the codegen tiers. (reference_riscv-jit-v8-tiering)

## Per-instruction / per-block micro-opts — NULL (redundant with TurboFan)
- **Macro-op fusion** `lui/auipc + addi → one store` (`FUSE_ON`) — NULL. Correct (difftest 412/412),
  fires on 4.16% of insns, A/B −2.6% ±3.2% vs 4.9% null. TurboFan already constant-folds these.
  (commit 4eeb416, reference_riscv-jit-macroop-fusion)
- **Register residency into wasm locals** (`REG_RESIDENT_ON`) — NULL, and this is the decisive one.
  Write-through read-cache: guest reg reads served from per-register wasm locals instead of the
  linear-memory reg file. Correct (difftest 412/412; interp-vs-JIT identical over 400M insns). It cut
  EMITTED reg loads from 1.60 to 0.70 per compiled instruction (−56%), yet A/B was +0.5% ±3.6% vs a
  3.8% null. Proof that **bytecode load count is decoupled from wall clock at the TurboFan tier** — TF
  already eliminates redundant constant-offset reg-file loads and keeps the values in machine
  registers, so the bytecode reduction produced identical machine code. This was the "one micro-lever
  not redundant with TF"; it turned out to be redundant too. Caveat: caches only WRITTEN regs (safe
  across runtime-conditional arms); the read-only-reg-across-a-guest-store sub-case is untested, but a
  56%-load-cut-for-zero result makes it very unlikely to pay. (reference_riscv-jit-v8-tiering)
- **Register store-to-load forwarding** within a block (`REG_FWD_ON`) — off. −15% median at Liftoff.
  Subsumed by the residency result above.
- **TLB-probe hoisting across an induction variable** (`TLB_HOIST_ON`) — NULL. Lets a group survive a
  constant-stride `addi base,base,K` so a striding loop (memcpy, sha256, string scan) re-uses one TLB
  probe for the whole strided run instead of re-probing each iteration; the leader's span covers the
  range and `group_host_addr` reconstructs each member's host address from the cached page base.
  Correct (interp-vs-JIT identical incl. a page-crossing memcpy+sha256 stress) and FIRES on ~30% of all
  groups (sha256 31% / md5sum 29% / memcpy 32% / gzip 29%). But A/B is **0.0% ±1.1% on memcpy** (tight
  CI, the most probe-dense workload) and −0.5% on sha256. Reason: a strided run hits one page, so every
  member's probe reads the SAME TLB entry with the SAME vpn, and TurboFan already CSEs those repeated
  loads/compares. Shipped OFF. (reference_riscv-jit-v8-tiering)

- **libc-idiom SIMD: memset run → `v128.store`** (`SIMD_ON`) — NULL, for a DIFFERENT reason than the
  scalar levers. A strictly-consecutive same-value contiguous `sd` run (musl/kernel memset,
  page-zeroing) is emitted as v128 stores — one 16-byte machine store per two guest `sd`, genuinely
  fewer ops that TF can't redo. Correct (interp-vs-JIT identical incl. a memset-heavy page-crossing
  stress) and FIRES on the hot clear_user/memset loops. But A/B is −1.5% ±3.8% on a memset-bound `dd
  bs=1M` (clean 2.7% null). **Data movement is memory-bandwidth / per-iteration-overhead bound, not
  instruction-ISSUE bound**: a v128.store moves the same bytes at the same bandwidth, and the loop's
  base-bump/branch/instret work is untouched, so halving the store *count* buys nothing. SIMD pays only
  when issue width is the bottleneck (vectorizable compute), which the guest's scalar stream doesn't
  hand us. Shipped OFF. (reference_riscv-jit-v8-tiering)
- **RAS for `jalr` returns** (`RAS_ON`) — NULL (commit 0a0293f). A real return-address stack: `jal ra`
  pushes the resolved return block, `jalr ra` pops and tail-calls it directly, gen- and PC-validated,
  falling back to the chain probe on any mismatch (so correct-by-construction; CPython deep-fib CHECK
  byte-identical). Null because the re-profile showed buckets unmoved — its ceiling is the chain-lookup
  seam, only **~3% of wall**: the 65536-entry chain table already tail-calls returns on a hit, and
  "returns are 25–45% of chain STOPS" misled — those stops mostly HIT cheaply. Kept flag-gated off.
  (reference_riscv-jit-v8-tiering)

## Takeaway: stop optimizing per-block wasm bytecode
FIVE independent per-block levers all measured NULL. Four (fusion, tail-call linking, register
residency, TLB-probe hoisting) because the hot blocks reach TurboFan (~+18%, confirmed) which already
does that class of optimization. The fifth (memset SIMD) because data movement is bandwidth/overhead-
bound, so fewer/wider store ops move the same bytes at the same speed.
The hot path is TurboFan machine code; TF already does constant folding, its own register allocation,
and redundant-load elimination. **There is no 10× — nor any resolvable win — in making the per-block
wasm bytecode "better" for things TF already does.** The wins that landed (inline TLB 2.3×, chain_max
1.2–1.5×) all reduced actual WORK or host round-trips, not bytecode quality. Direct future effort at
the UNBUILT structural levers below, not at more peephole/regalloc work.

## Block chaining / dispatch — mostly spent
- The chain **already tail-calls** its successor via `ReturnCallIndirect` through a hash chain-table.
  There is NO host round-trip per block in steady state; the host is entered only on a probe miss.
- **Static same-page fall-through direct linking** (`TAILLINK_ON`) — NULL. Direct `return_call` to an
  in-batch same-page successor, skipping the probe. Correct (interp-vs-JIT identical over 400M insns),
  links 21.5% of fall-through blocks, A/B +0.3% ±4.2% vs 2.8% null. Cross-module direct calls are
  impossible in wasm, so reach is batch-locality-bound. (commit b21cd48, reference_riscv-jit-tailcall-linking)
- **Per-block runtime inline cache** (cache last successor + guard) — REVERTED. Correct, cut evicted
  misses 26%→6%, but net neutral/negative (sh +10%, gzip −6%); code bloat > collision savings; 73% of
  stops are coverage, not linkable. (reference_riscv-jit-inline-cache-ab)
- Chain-stop composition measured: FWD-DIRECT 37–78%, INDIRECT jalr 25–45%, backedge loops only 10–25%
  (the trace already unrolls short loops). (reference_riscv-jit-chain-terminator-split)
- **`MAX_RUN` sweep (longer traces → fewer boundaries)** — NULL, and an INFORMATIVE null (2026-08-18).
  Swept `MAX_RUN` 64→128→256, rebuilt each, ran the interp-vs-JIT gate. CPython chain-probe misses
  moved **1,332,736 → 1,331,661 (−0.08%)**; correctness byte-identical. The decisive metric is
  **cap-ends: only 47 (MR=64) → 84 (MR=256) traces out of 1.33M boundaries hit the length cap** — i.e.
  traces already **close on their own** (taken branch / jump / jalr / uncompilable instr) far below 64
  insns, so the cap is almost never what ends them. Lengthening traces therefore cannot remove
  boundaries: the 1.33M boundaries are inherent to CPython's control-flow density, not to truncation.
  No wall measurement taken — cap-ends IS the direct measurement of what MAX_RUN changes (~60/1.33M),
  and a MIPS number would be pure noise on a 0.08% mechanism change. **Kills the "region formation via
  longer linear traces" thesis.** REDIRECTS to coverage: 64.4% of misses are "no block" (successor
  never compiled) — the boundaries exist because successors are UNCOMPILED, not because traces are
  short. Next lever = prefetch/background-compile of successors, not trace extension. MAX_RUN restored
  to 64.
- **Successor-prefetch coverage (path 3) — DIAGNOSED DEAD before building (2026-08-18).** Followed the
  MAX_RUN redirect: instrumented the 858k "no block" misses by the predecessor block's terminator
  (temporary `blk_ends_jalr` idx-table; reverted after measuring). Split: **direct edge
  (jal/branch/fall-through, prefetch-predictable) 41.5% | indirect `jalr` (register target,
  unpredictable) 58.5%.** So a static successor-prefetch reaches at most 41.5% of coverage misses — the
  majority are CPython's ceval computed-goto dispatch + returns, which a static prefetch cannot predict.
  AND the ceiling is tiny: the whole cold-compilable interpreter bucket these misses feed is only
  **~3.1% of wall** now (batched-poll + selective-flush already shrank it). 41.5% of ~3% ≈ **~1.3% of
  wall, below the noise floor.** Not worth building. **Coverage is not the lever.** What remains: block
  bodies are 57% of wall (TF-optimized machine code, no headroom), and the largest non-body cost is the
  **per-boundary seam** (translate ~8% + run-loop ~9% + device ~8%) — every one of the 1.33M chain
  misses pays a full return-to-run-loop round trip. The open direction is making the boundary cheaper /
  returning to the host less often (e.g. servicing "no block → interpret one instr → retry" without
  fully unwinding the run loop), NOT compiling more successors.

## Invalidation / generations
- **ASID-keyed generations** — SHIPPED (`ASID_KEYED`, supervisor.rs), and the FIRST resolvable win of
  this campaign. A satp write now restores that address space's saved `(trans_gen, data_trans_gen)`
  instead of bumping (which voided every virtual-keyed cache). Keyed by the FULL satp value
  (mode|asid|ppn) via a 128-entry direct-mapped table — NOT the asid alone, which was what sank the
  prior attempt (asid 0 aliases swapper + every early root). Generations are drawn from a monotonic
  allocator so two live spaces never share one; a global sfence.vma / fence.i / restore / queue
  overflow drops the whole table; a key-page single-page sfence advances only the current space.
  **Measured +14.3% ±5.5% on `yes | cat` (context-switch-bound: chain misses 1.50M→714k, gen-trans
  the 82%→ portion halved); neutral on compute-bound (ls|md5sum, where satp churn is only 7.7% of
  chain stops); correct on all workloads (interp-vs-JIT identical incl. a high process-churn recycling
  stress).** The category that pays: it removes real re-translation WORK, not bytecode. Knockout:
  `ASID_KEYED=false`. (reference_riscv-jit-asid-confirmed)
  - Remaining churn it does NOT touch: single-page-sfence chain invalidation (~538k gen-trans on the
    gzip pipe) from `page_may_have_keys` firing on data-page munmaps — a separate precision lever.

## Correctness invariants the JIT relies on (don't break these when optimizing)
- Table function indices are **append-only, never reused** (`Jit::installed`), so a baked index always
  means the same block for the life of the process.
- A trace **never crosses a page**; `Src` offsets are relative and can be non-contiguous because a
  trace follows `jal` within the page (so physical fall-through = `paddr + last.off + last.width`,
  NOT `paddr + sum(widths)` — that bug shipped once and the interp-vs-JIT harness caught it).
- `count_insns` advances instret by the full decoded count even when instructions are fused/merged.
- The **interp-vs-JIT harness (`jit-vm-test.js`) is the real correctness gate** for batch/chain changes;
  `jit-difftest.js` only covers single blocks. Run both.

## UNBUILT (still open) — structural only; per-block codegen is a dead end (see Takeaway)
1. **Single-page-sfence chain precision** — a data-page munmap that trips `page_may_have_keys` still
   advances the current space's chain gen, voiding its whole chain (~538k gen-trans misses on the gzip
   pipe, the largest remaining chunk after ASID keying). Make the key-page filter precise (or track
   chain keys per page) so a data-page flush never touches chaining. Reduces real work; same category
   as the ASID win.
2. ~~Return-address stack for `jalr ra`~~ — **BUILT, NULL** (`RAS_ON`). A real stack pushing a call's
   resolved return block and popping+tail-calling it at the return, fall-back to the chain probe on any
   mismatch (so correct-by-construction; verified CHECK-identical on deep-recursion CPython). But
   re-profiling after the build showed the buckets barely moved: its ceiling is the chain-lookup seam it
   targets, which `--prof` says is **only ~3% of wall** — the 65536-entry chain table already tail-calls
   returns on a hit, so returns' host-round-trip fraction is sub-noise, and the per-call resolve-probe
   makes it net null-to-negative. Lesson: "returns are 25-45% of chain STOPS" was misleading — those
   stops mostly HIT cheaply. The profiling loop caught it (predict → build → re-profile → target's
   ceiling too small). Shipped OFF. (RAS_ON, reference_riscv-jit-v8-tiering)
3. **single-page-sfence chain precision** (repeated from above) — the largest remaining real-WORK lever:
   data-page munmaps trip `page_may_have_keys` and void the current space's whole chain (~538k gen-trans
   misses on the gzip pipe). Make the filter precise so a data-page flush never touches chaining.
4. **Cross-session compiled-block cache** (`project_riscv-vm-jit-cache`) — skip recompiling hot blocks a
   returning program already compiled. Removes real compile work, not codegen quality.

DO NOT re-propose: register residency/allocation, macro-op fusion, store-to-load forwarding, static
block linking, per-block inline caches, TLB-probe hoisting/CSE, memset/memcpy SIMD, or any "emit
tighter/fewer/wider wasm ops" idea — ALL measured null. Scalar-op restructuring is redundant with
TurboFan; SIMD data movement is bandwidth-bound. The only wins come from removing real WORK the engine
does redundantly: ASID keying (shipped +14%), and the sfence/chain-cache levers above. That is the
whole remaining direction.

## How to measure (so results are trustworthy on this box)
- Steady-state MIPS: `mips.js` (warmup + median-of-best-of-8 slices). Tier probes: `tiertrace.js` +
  `--trace-wasm-compilation-times`. Reach probes: `fusestat.js`, `linkstat.js` (jit_fuse_stats / jit_link_stats).
- A/B two builds: `jit-fast-ab.js` (interleaved, `--null` first; believe nothing under the reported
  resolution). SLICES>~320 OOMs the table; box shows ~2–5% self-bias.
- Build: `build-jitnode.sh <dir>`; flip a `*_ON` const for A/B and build both to `$HOME/...` (WSL wipes
  /tmp on idle, so keep builds + scripts under $HOME and build+run in one shell).
