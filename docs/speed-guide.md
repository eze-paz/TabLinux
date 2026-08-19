# Speed levers: implementation and test guide

Companion to speed-checklist.md. That file says *what* and *why*; this one says
*where*, *how*, and *how to know you're done* — written so a development agent
can pick up any single lever cold. Read this preamble fully before any lever;
it encodes mistakes this project has already paid for.

## 0. How to work on this codebase

### Layout

| path | contents |
|---|---|
| `crates/riscv-core` | decode, ALU execute, types. No MMU, no devices. |
| `crates/riscv-supervisor` | `supervisor.rs` = CSRs, traps, privilege, interrupts; `mmu.rs` = Sv39 walker + 256-entry 4-way software TLB; `sbi.rs` = **native** SBI (there is no emulated M-mode firmware — supervisor.rs:385 explains). |
| `crates/riscv-machine` | `lib.rs` = `Machine::run` (the JIT/interpreter run loop, chain execution, interrupt polling, WFI idle skip); `jit.rs` = block cache, hotness, chain table on the Rust side (`HOT=16` jit.rs:34, `chain_gen` jit.rs:226). |
| `crates/riscv-jit` | the wasm code generator. Module doc at the top of lib.rs is the design record — read it before touching codegen. Emits via `wasm-encoder`. |
| `crates/riscv-wasm` | the wasm-bindgen host: imports (`load*/store*/csr`), inline-TLB config (`TlbCfg`), `ChainCfg` wiring (lib.rs:212). |
| `crates/riscv-devices` | `device_bus.rs` = CLINT/mtime (`idle_skip_mtime`:339, `MAX_IDLE_SKIP`:322), PLIC, virtio slots. |
| `jit-*.js`, `*-ab.py` | Node harnesses and A/B drivers (see below). |
| `web/` | the deployed page. `web/pkg` is built locally, untracked. |

### Build

```bash
# Native tests build themselves. The JIT harnesses need the wasm host:
./build-jitnode.sh            # -> /tmp/jitnode  (refuses stale artifacts)
./build-wasm.sh               # -> web/pkg       (the BROWSER build — not what bench/jit-vm-test.js loads!)
```

**`bench/jit-vm-test.js` loads `/tmp/jitnode`.** Building `web/pkg` and then
benchmarking measures the *previous* build and it will look plausible. This
has happened; `build-jitnode.sh` exists because of it.

### The test battery (run all of it before claiming a lever done)

```bash
cargo test --release -p riscv-harness --test isa_suite          # must be 134/134
cargo test --release -p riscv-harness --test boot_to_userspace  # userspace at step 567,856,066 EXACTLY
cargo test --release -p riscv-machine --test snapshot           # determinism
node bench/jit-difftest.js          # 400 randomized cases vs interpreter: 400/400
node bench/jit-vm-test.js           # snapshot boot, console diffed vs interpreter
node bench/jit-coldboot-test.js     # COLD boot, byte-identical console vs interpreter
node bench/jit-restore-repro.js     # the rw-mount interrupt-delivery repro: all steps responsive
```

Notes that have each independently saved or cost a day:
- A different `boot_to_userspace` step count means **behaviour changed**, not
  speed. It is a failure until explained.
- The snapshot harnesses resume a booted guest. Three real bugs (fence.i
  flush, missing interrupt delivery, atomics sign-extension) were invisible to
  them and caught only by `bench/jit-coldboot-test.js` or the difftest. Never skip
  the cold boot.
- If you add a new instruction to codegen, extend the difftest *generator* to
  emit it, and confirm via generated-module byte size that it actually does.
  The AMOMINU/AMOMAXU bug sat green until the generator emitted atomics.

## MEASUREMENT POSTMORTEM (2026-08-10) — read before trusting anything below

**The measurement methodology recorded in these documents is severely
deficient, and conclusions drawn from it are withdrawn.** On the same day the
A/B instrument reported two levers as "+2-3%, verified," the deployed page
regressed from ~5s to 40-50s on its main path (snapshot restore) — reported by
the user, never predicted or caught by any instrument here. The deploy was
reverted (sandpie-server 804a5db) on that report.

What was wrong, specifically:

1. **Every instrument measured a proxy** — steady-state MIPS of a synthetic
   shell loop under Node — and none measured what a user experiences:
   wall-clock from page load to a usable prompt, in a browser, on the restore
   path. The suite could report "faster" while the product got an order of
   magnitude slower, and it did.
2. **The instruments were validated against themselves** (null tests, bootstrap
   CIs). A null test proves the instrument is self-consistent on its own
   metric; it proves nothing about whether that metric predicts the outcome
   that matters. That validation was circular and was treated as if it were
   external.
3. **The regime that regressed had no harness at all.** The restore+setup path
   (the page's common path) was never timed by anything. Changes shipped with
   "battery green" while their effect on that path was unknown.
4. **The cause of the 40-50s regression is an UNVERIFIED HYPOTHESIS** (realtime
   clock not gated during restore setup — plausible from code inspection of
   vm-worker.js:685, but the timing repro was never run). The revert commit
   states it as fact; it is not established fact.

Consequences for the claims in these files:

- **The "10x is not reachable" conclusion is WITHDRAWN as unsupported.** It was
  derived from ns/instruction analysis using this instrument suite, and it
  bounds only instruction throughput. User-experienced speed demonstrably has
  order-of-magnitude levers that live entirely outside ns/instruction — this
  very regression was one of them, firing in the wrong direction. The 150-300
  MIPS "ceiling" carries the same caveat: even if roughly right about MIPS, it
  says nothing about wall-clock experience, which is the goal metric.
- Small A/B verdicts (+2-3% "wins") should be treated as claims about the
  synthetic-loop metric only, not as shipped improvements, until an end-to-end
  harness exists.
- Deterministic event counters (chain/TLB miss taxonomies, gen-bump causes) are
  raw tool outputs and remain reproducible facts — but which of them MATTERS
  was ranked using the deficient throughput lens, so the rankings are suspect
  too.

The missing instrument, which must exist before any further perf claim: a
browser-context, end-to-end timing of the deployed page's real paths — snapshot
restore to interactive prompt, cold boot to prompt, and a scripted interactive
workload — run before and after every change that ships.

### Performance measurement — use bench/jit-fast-ab.js

**(Deficient — see the postmortem above. Kept for the mechanics of running it,
not as a shipping gate. Its verdicts describe a synthetic Node workload only.)**

```bash
./build-jitnode.sh /tmp/jitnode_new
git stash && ./build-jitnode.sh /tmp/jitnode_old && git stash pop
node bench/jit-fast-ab.js --null      # validate the instrument FIRST
node bench/jit-fast-ab.js             # the measurement
```

**Resolution ~2.8%, runtime ~100 seconds, null bias 0.31%.** Run it twice
and check the two medians agree; two independent runs beat one long one,
because they resample the host's mood as well as the guest's.

`bench/jit-ab.py` is superseded. It took 40 minutes to return a number that
disagreed with itself -- the first two pairs of one run were 0.838 and
1.091 -- because it booted the interpreter for 78s per run to produce a
correctness reference it then used to measure 6s of JIT, and compared
across processes minutes apart. Correctness is the battery's job, asked
once, not sixteen times per verdict.

Things that instrument taught, which apply to any measurement here:

- **Do not raise SLICES past ~480.** V8 dies in `WasmTableObject::Grow`:
  blocks accumulate for the whole run with no equivalent of the engine's
  `JIT_CACHE_MAX` discard.
- **/proc/self/schedstat is the wrong clock**, despite excluding
  descheduled time, which is what you want. Measured, it only advances
  every ~4ms. Against a 33ms slice that quantisation makes many A and B
  slices tie at exactly 1.0, and the bootstrap then reports an impossible
  0.0% resolution off a spike of exact ones. Use `hrtime` and reject
  interference with an estimator (fastest slice per block) instead.
- **An instrument claiming impossible precision is worse than a noisy
  one.** Sanity-check the interval against the spread: with n samples the
  CI of the median cannot be much tighter than sigma/sqrt(n).
- **Deterministic counters beat timing whenever they can answer the
  question.** `bench/chain-sweep.js` and `bench/tlbprobe.js` decide whether a lever
  is worth an A/B at all, in one run, with no statistics.
- **Suspiciously unchanged numbers are a bug signal.** A real no-op moves
  counters a little; *identical* counts meant a patch had silently not
  applied.

### The instrumentation you get for free

`Machine::interp_hist` (`riscv-machine/src/lib.rs:53`, gated by
`interp_hist_on`) bins every interpreted instruction by *why* it was
interpreted; `bench/jit-vm-test.js` prints share-of-wall per bin. The split that
matters is **uncompilable vs merely cold** — codegen fixes the first, nothing
fixes the second. When a lever claims to move a bin, print the histogram
before and after; that is deterministic and immune to the noise problem.

### Definition of done, per lever

1. Test battery green, including cold boot.
2. `bench/jit-fast-ab.js` median clearing its own reported CI (typically ~3%), or
   a wall-clock win for Tier 3, or an explicit "not proven" record. The old
   >=1.15 bar was a property of the old instrument and no longer applies.
3. A short section appended to this file recording the result —
   *especially* negatives. The closed-levers list is the most valuable part of
   this project's docs.
4. Committed locally. **Do not push** — the remote embeds a credential; the
   user pushes.

---

## Stage-2 result (2026-08-10): compiled cost per instruction class

Measured in-situ: pure-class guest loops (tools/classbench/gen.py, hand-wrapped
static ELFs, no relocations) run inside the restored VM via bench/classbench.js, timed
between shell-computed markers, instruction counts exact from the step counter.
Two runs; absolutes swing ~25% with the box, the ranking does not.

| class | ns/instr (2 runs) | vs alu | what it says |
|---|---|---|---|
| alu   | 11.1 / 15.5  | 1.0x | the FLOOR: even pure addi pays this |
| load  | 15.7 / 19.5  | ~1.3x | inline TLB fast path is fine |
| store | 18.1 / 21.1  | ~1.5x | fine too |
| br    | 16.2 / 22.1  | ~1.4x | not-taken branches are cheap |
| jmp   | 27.2 / 26.8  | ~2x  | a direct jump ends the block: ~16-18ns per hop |
| ind   | 44.6 / 51.7  | ~3.3x | jalr: chain probe through the vcache |
| fp    | 121 / 131    | ~9x  | the stage-1 host call, whole-interpreter-path per op |

Decisions this settles:

- **Stage 1.5 (inline FP in pure wasm) is the largest measured per-class win
  left**: ~120ns -> plausibly ~15-20ns for fadd/fmul/fcvt, on top of stage 1's
  4.4x. Gated on extending the difftest generator to FP with f-register
  comparison — the host-call design made "same math by construction" true, and
  inlining forfeits that property, so the generator must cover it first.
- **The memory path is NOT the lever.** Loads run ~1.3x alu; the same-page
  probe reuse idea (§1.4) and memory64/multi-memory plans are LOW VALUE —
  closed unless something reopens them with data.
- **The alu floor is the 700-MIPS question.** ~11-15ns for a pure addi means
  integer code as currently compiled tops out around 65-90 MIPS regardless of
  everything else. That cost is per-instruction register-file traffic plus
  per-block bookkeeping, and the lever with headroom is a trace-level
  optimizer (value forwarding, dead regfile-write elimination across the
  484-instruction traces) — NOT registers-in-locals, which failed 4x as a
  1:1 mapping with spills.
- Block-end hops price at ~16-18ns (direct) / ~120ns-per-unit (indirect);
  loops-as-wasm-`loop` (§1.3) is real but second to the two above.

Caveat carried from the postmortem: these are synthetic single-class loops in
Node — honest for RANKING codegen costs, and the ranking is what stage 3 needs;
they are not end-to-end user numbers and claim nothing about wall-clock UX.

## Stage-3 result (2026-08-10): register forwarding is DEAD — the fifth confirmation

Implemented and measured: a forwarding pass shadowing recently-touched guest
registers in six wasm locals. Deliberately NOT the four-times-failed
registers-in-locals design — no prologue, no epilogue, no write-back
obligation; every regfile store still happened, locals only shadowed values
already in hand, repeat reads became local.get. Correct (difftest 400/400 with
FP and atomics in the stream), and REVERTED:

  A/B vs stage-1.5 baseline:  +0.1% ±4.3%  and  −2.1% ±2.5%
  generated code:             +2.3% bytes

The explanation consistent with all five negatives: **TurboFan already
performs redundancy elimination on loads from a constant base at constant
offsets** — the regfile loads the pass "saved" were already being folded by
the engine, so the tees added code and local pressure for nothing. Hand
register-allocation on top of V8 loses in every form tried: prologue/epilogue
promotion (4x), and now zero-overhead shadowing.

Dead-store elimination (v2) is closed by analysis without building: it can
only fire between two writes to the same register with NO intervening read,
fault site, or side exit — and fault sites (every load/store/csr/fp) punctuate
real traces every few instructions. The window population is a few percent of
stores at ~1ns each: under 0.5% ceiling. Not worth its bail-semantics risk.

What this settles about the ALU floor (~8-15ns for pure addi): it is NOT
register-file traffic. What remains in a compiled addi after V8's own
optimization is the mandatory store (architecturally required per
instruction), wasm dispatch overhead, and the Liftoff-tier share of execution.
None of those yield to a trace optimizer emitting better memory patterns. The
honest levers left for the floor are structural: fewer, longer functions
(loops as wasm `loop`s — removes per-block bookkeeping V8 cannot), or fewer
guest instructions (Tier 3 guest-side work). The 700-MIPS question stays open,
but wasm-level peephole work is now a measured dead end — do not retry a
sixth time without evidence from a different engine.

## Stage-3b result (2026-08-10): guest loops as wasm `loop`s — built, correct, sub-resolution, reverted

The last structural card. Traces that close on themselves (a conditional
backward branch to their own first instruction — what decode_run produces for
every guest loop) were emitted as real wasm `loop`s: back edge = a branch
instead of return + chain probe + re-entry, with the chain's own
insns < budget check once per iteration so worst-case interrupt latency was
unchanged. Side exits got a depth-parametrised branch target; CSRs cannot
occur inside a loop body (decode_run ends traces at them), so translation
cannot change mid-loop.

Correct — difftest 400/400, console byte-identical, awk sum exact — and
measured on the real workload:

  A/B vs stage-1.5:  +0.7% ±1.5%  and  +0.8% ±1.8%   (not resolvable)
  jump-dense microbench: 27 -> 16 ns/instr           (the mechanism is real)

Why the microbench win vanishes in real code: chains already amortise block
hops across ~484 instructions, so hop cost is ~2-3% of the whole machine —
consistent with the chain-cap result (halving boundary crossings bought 2%).
A wasm loop removes at most half of what remains. Reverted per the standard
applied to forwarding: sub-resolution complexity is not banked.

**This closes the compiled-path optimization space on V8 as measured.** The
scorecard: memory path fine (1.3x alu), FP host calls fixed (stage 1/1.5),
register traffic already optimal (five negatives), block structure worth <2%
(this). The ALU floor ~8-15ns is the cost of executing ~5 wasm ops per guest
instruction under V8's tiering, and no emitter-level transformation measured
so far moves it. What remains for large factors: fewer guest instructions
(Tier 3), warm-up elimination (Tier 4.1 persistent cache), parallelism (SMP),
or a different execution substrate entirely.

## jalr / indirect-branch measurement (2026-08-10): a real ~4% lever, unbuilt

QEMU caches indirect-branch targets; we pay a full chain probe on every jalr
(the priciest class, ~3.3x alu in the microbench). Measured on a call-heavy
workload (fork/exec + ls + md5sum + grep, 26M instructions, interpreter so the
count is exact; instruction MIX is identical JIT-on-or-off). bench/jalrbench.js;
engine scaffolding was reverted, so re-apply it to reproduce.

  jalr executed        2.84% of all instructions
    returns (ret)      69% of jalr
    indirect (non-ret) 31% of jalr
  predictability:
    shadow stack       99.4% of RETURNS   (the natural structure for returns)
    last-target cache  73.3% of ALL jalr  (the natural structure for the rest)

A combined predictor — shadow stack for returns, last-target for the indirect
31% — would cover ~90% of jalr. Crude ceiling: jalr is ~9% of wall at 3.3x, and
cutting a predicted hit from ~3.3x toward ~1.5x recovers ~3.5-4.5% of wall on
call-heavy work, ~0 on tight compute loops (awk has almost no jalr).

Verdict: the ONE genuine unbuilt TCG-style micro-lever, ceiling ~4% — above the
instrument's ~2.8% floor, same class as the sfence win that shipped. Worth a
prototype IF pursued, with two honest caveats: (1) the 3.3x->1.5x recovery is a
guess — the current chain probe already RESOLVES the target, so a cached-target
check only saves the probe overhead (hash + gen check + budget test), not the
whole jalr cost, and the real saving per hit needs a prototype to measure; (2)
it is workload-dependent and modest. Ranked below the higher-level levers
(persistent cache, fork/exec) for "fast real Linux". A return-address shadow
stack is the higher-value half (returns are 69% of jalr at 99.4% predictable)
and is the piece to prototype first if this is taken up.

## Tier 1 — JIT levers

### 1.1 Chain-survival instrumentation (do this before 1.2)

**Why first:** the record claims "every privilege transition invalidates the
chain table", but the `chain_gen` formula (`1 + ((trans_gen << 2) | priv)`,
jit.rs:226) is *derived*, not bumped — entries written under (trans_gen=T,
priv=S) match again the next time the machine is in that state. So syscalls
alone should not permanently kill entries. What actually kills them is
`trans_gen` moving (satp write on every context switch, supervisor.rs:1051 and
:723) or slot eviction. Which one dominates decides whether to build 1.2 or
Tier 2's ASID work. Do not build either on the current guess.

**Do:** add counters to the chain-insert and chain-probe paths in
`riscv-machine/src/jit.rs` (Rust side) — probes, hits, misses split by
"key mismatch (evicted)" vs "gen mismatch"; for gen mismatches, record whether
the stored gen differs in the priv bits, the trans_gen bits, or both. Gate
behind the existing `interp_hist_on` style flag; print from `bench/jit-vm-test.js`
next to the histogram.

**Test:** counters change nothing architectural — battery must be green and
boot step count identical. Run one boot + one apk workload, read the split.

**Accept:** you can now say "chain misses are X% eviction / Y% trans_gen /
Z% priv" with numbers. That's the deliverable; it selects the next lever.

### 1.2 Per-privilege chain partitioning (only if 1.1 blames priv/eviction)

**Where:** slot index is computed in the compiled probe
(`chain_to_successor`, riscv-jit/src/lib.rs — `(next_pc >> 1) & (entries-1)`)
and must match the Rust insert path in jit.rs.

**Do:** fold the privilege bit into the *slot index* (not the key): the guest
PC's bit 0 is always 0 (IALIGN≥2), so index on
`((next_pc >> 1) ^ (priv << k)) & (entries-1)` where priv is read from a fixed
wasm-memory address the host keeps current (same mechanism as the gen word,
`gen_addr` in `ChainCfg`). Kernel and user blocks then stop evicting each
other. Keep the gen check exactly as is — it is what stops a user block
chaining into a kernel page, and the record already establishes you cannot
remove privilege from the validity check.

**Pitfalls:** the Rust-side insert (jit.rs) and the wasm-side probe must
compute identical indices or chains silently never hit (symptom: MIPS drops to
interpreter-with-blocks levels, ~40). Assert hit-rate in `bench/jit-vm-test.js`
output before benchmarking.

**Test:** battery; then ABBA. **Accept ≥1.15; expect the win on the apk
workload, not `yes`.**

### 1.3 Guest loops as wasm `loop` / megablocks

**Where:** trace formation is in jit.rs (`decode_run` region — where a block's
instruction list is built and terminated); codegen consumes it in
riscv-jit/src/lib.rs.

**Do, in two stages:**
1. *Megablocks:* when block A's chain successor has been B for N consecutive
   entries (track in the Rust chain table), recompile A+B as one function and
   replace A's cache entry. Per-hop probe/gen/count overhead disappears for
   that edge. Cap merged length (start: 1024 insns) and merge count so
   pathological growth can't happen. Discard merged blocks on the same flushes
   as normal blocks (they are normal blocks, just longer).
2. *Loops:* when the successor of a block is *itself* (or a merged block's own
   entry), emit a wasm `loop` whose back-edge condition is the existing chain
   budget check (`INSNS_OFF` counter vs `jit_budget`) — the interrupt window
   at chain boundaries is preserved because falling out of the loop returns to
   `Machine::run_chain` (lib.rs:383), which is where `interrupt_pending` runs.

**Pitfalls (each has bitten a variant of this already):**
- The interrupt-unmask bail (1ebbf13) must still work from inside a merged
  block: the `csr` host import sets `jit_fault`, and every fault check must
  break out of the *merged* function, not just the sub-block. Reuse
  `bail_if_faulted` unchanged and the fault plumbing carries over.
- `fence.i`/uncompilable instructions still terminate traces — merging never
  crosses them.
- Self-modifying code: merged blocks are still keyed and flushed by
  `icache_gen`; verify with `bench/jit-coldboot-test.js`, which exists precisely
  because alternatives-patching rewrites kernel text.
- Watch `JIT_CACHE_MAX` pressure: merged functions are bigger; check the
  discard counter doesn't start cycling (the 45608ed collapse was a discard
  bug — reread that commit before touching cache lifecycle).

**Test:** battery, then ABBA on both boot and apk. Also print insns/chain and
block-entry counts (already in bench/jit-vm-test.js output): entries should drop
sharply; if they don't, merging isn't firing — fix that before benchmarking.

**Accept ≥1.15.** This is the biggest item in the file; budget accordingly,
and land stage 1 alone if it clears the bar by itself.

### 1.4 Same-page TLB probe reuse

**Where:** `load`/`store` codegen in riscv-jit/src/lib.rs (load at :402,
`tlb_probe` just below). The probe re-derives the TLB entry address from the
full virtual address every time.

**Do (static version only):** within one block, when two accesses use the
same base register, no intervening write to that register, and static offsets
whose difference keeps them in one 4K page (`(off1 & !0xFFF) == (off2 & !0xFFF)`
is NOT sufficient — compare `(base+off)` page-stability: offsets must satisfy
`off1 >> 12 == off2 >> 12` *and* you must know base is unchanged; when the low
12 bits of the offsets could carry across a page boundary at runtime, skip the
optimization for that pair), reuse the translated host page address kept in a
scratch local from the first probe. Invalidate the cached local at: any write
to the base register, any host call (csr/load/store slow path — a satp write
inside the block can change translation), and any fault-check branch target.

**Pitfalls:** permissions — a load probe and a store probe are different
checks (`tlb_probe(..., is_store)`); only reuse across accesses of the *same*
kind, or store the stricter (store) translation and let loads reuse it, never
the reverse. MMIO: the fast path only covers RAM pages (that's what the inline
TLB holds), and reuse inherits that property — an address that fell to the
slow path caches nothing.

**Test:** battery — the difftest generator must emit paired same-page accesses
(add a stack-push/pop pattern to the generator; verify module size moves).
Then ABBA. **Accept ≥1.15**; if it lands 1.0–1.15, revert and record — the
probe may simply be cheaper than modeled, which is itself a finding (it's
happened: the Rust-side translation cache lost to the call it skipped).

### 1.5 Value forwarding (regfile store→load elision)

**Where:** codegen main loop in riscv-jit/src/lib.rs — every instruction
currently loads operands from the register file and stores results back.

**Do:** peephole only, no allocator (the allocator variant is closed, 4
negatives): when instruction N writes rd and N+1 reads the same register,
`local.tee` the value into one scratch local at N and read the local at N+1.
The regfile store at N still happens (fault semantics stay identical — a
fault at N+1 must observe N's committed write). This saves only the *reload*,
which caps the win: measure the dynamic frequency of write-then-read-next
pairs first (add a counter in trace build; one boot's histogram answers it).
If pairs are <10% of instructions, the ceiling is ~2-3% — record and skip.

**Test:** battery; ABBA only if the frequency measurement justifies it.

### 1.6 Cheap interrupt-deliverability pre-check

**Where:** the `csr` host shim in riscv-wasm/src/lib.rs (the 1ebbf13 unmask
check) calls `Supervisor::interrupt_pending`, whose cost is
`refresh_virtio_irqs` (PLIC scan) on every interrupt-*enable* write.

**Do:** before the full scan: timer deliverability is
`bus.read_mtime() >= min(stimecmp, mtimecmp)` (two reads — see
supervisor.rs:1069 for the exact existing predicate); external is a cached
`plic.any_pending()` bit maintained where `set_level` is called (device_bus).
Only when either pre-check fires do the full `interrupt_pending` scan. The
pre-check may be conservatively *true* (falls through to the real check) but
must never be false when the real check would deliver — write that as a
comment and a debug assertion (`debug_assert!(pre || !full)`) and run a boot
with debug assertions on.

**Test:** battery — bench/jit-restore-repro.js is the one that matters (it is the
regression test for exactly this path). ABBA target: the 1ebbf13 record says
the check costs ~10-15%, so recovering it should just clear the bar.

### 1.7 Small codegen coverage: sfence.vma, mulh; cold-threshold re-check

- `sfence.vma`: compile as no-op + bump-gen host call (it must invalidate the
  inline TLB and end the trace like a satp write — reuse the csr import's
  `refresh_gen` path). ~1%.
- `mulh/mulhu/mulhsu`: 64x64→high-64 synthesis from four 32x32 products, pure
  wasm, no host call. Extend the difftest generator with edge operands
  (0x8000…, -1, mixed signs). ~1%.
- Cold threshold (`HOT`, jit.rs:34): rerun the 16→8→4 sweep post-CSR. One ABBA
  per value; expectation low, stop at the first non-win.

These are warm-up tasks — do one before attempting 1.3 to learn the
build/test loop cheaply. Individually below the noise floor, so land them
batched, or accept "not proven" and keep them only if diff-neutral in size.

---

## Tier 2 — MMU architecture

### 2.1 ASIDs (the likely real fix for chain/TLB death)

**Where:** `Satp` already parses an asid field (supervisor.rs:232). satp
writes bump `trans_gen` unconditionally (supervisor.rs:1051 region; sfence at
:723). The software TLB (mmu.rs) flushes via the same gen mechanics; the
inline wasm TLB and chain table validate against gen words.

**Do — per-ASID generations, not TLB re-keying:** keep a small map
`asid -> gen` in `Supervisor` (Sv39 ASIDs are 16-bit; a 64-entry direct-mapped
array with the asid as key beats a HashMap and bounds memory). On satp write:
if mode+ppn+asid all unchanged, do nothing; if asid changes, *load* that
asid's stored gen into the live gen (allocating a fresh gen for a new asid)
instead of bumping. `sfence.vma` with rs2=asid bumps only that asid's gen;
global sfence (rs2=x0) bumps all (bump a global epoch folded into every gen).
Advertise ASID support: Linux probes by writing all-ones to satp.asid and
reading back — the CSR write path must preserve the field (check
supervisor.rs:962/1036 region for the satp read/write) or Linux silently runs
asid-less and nothing changes.

**Pitfalls:**
- The inline wasm TLB and the chain table share the generation word
  (riscv-wasm lib.rs:481 comment). Loading an *older* gen value on switch-back
  revalidates every entry stamped under that gen — which is exactly the point,
  but it means a gen value must never be reused for a different address space:
  allocate gens from a monotonic counter, never recycle, and on counter wrap
  (u32) flush everything once.
- Kernel-global mappings (PTE G bit) are valid across ASIDs; ignoring that is
  *correct but slow* (kernel entries die per switch). First version: ignore G.
  Note it as follow-up.
- Verify Linux actually enables ASIDs: boot with a debug print on the probe
  write. If the kernel config doesn't use them, this lever is dead on arrival
  — check before implementing the rest.

**Test:** battery — `boot_to_userspace` step count WILL change if the kernel
takes a different path (ASID probe succeeds where it failed before). That is
the one sanctioned case of a step-count change: re-baseline it deliberately,
record old and new numbers in the commit, and confirm the cold-boot console
diff is clean. Then 1.1's instrumentation shows gen-miss share collapsing, and
ABBA on the apk workload (context-switch dense) decides. **This is the Tier-2
item most likely to clear 1.15.**

### 2.2 Superpage TLB entries

**Status: confirmed real gap.** `TlbEntry` stores `size` (mmu.rs:18) but
`lookup()` matches `va >> 12` exactly (mmu.rs:49) — a 2MB mapping inserted at
one vaddr misses for its other 511 pages, so the walker re-walks per 4K page.

**Do:** in `lookup`, match superpages by masking: entry hits if
`va >> (12 + 9*size) == va_key >> (9*size)` (store va_key already shifted per
size, or store size and mask at probe). `insert` from `apply_tlb` (mmu.rs:201)
— confirm the walker passes the real level as `size`. Keep the 4-way set
structure; index superpages by `va >> 12` still (they'll occupy one set —
acceptable; alternatives complicate `set_index`).

**Then the same for the inline wasm TLB** (TLB_ENTRY_BYTES=32 layout,
riscv-jit lib.rs:179): the entry has spare bytes — store a page-size mask and
apply it in `tlb_probe`. This is the half that pays at 6.5 ns/insn; the Rust
half only pays on inline-TLB misses. Do the Rust half first (simpler,
validates the masking logic), the wasm half second.

**Test:** battery. Add a walk counter (walks per 1M insns) printed by
bench/jit-vm-test.js — it should drop hard on kernel-heavy phases even if MIPS moves
less than 15%. Accept on ABBA, or on walk-count + not-proven-but-simplifying
grounds if it's a wash.

### 2.3 Kernel linear-map fast path — parked

Only attempt if 2.2's walk counter still shows the linear range dominating
misses afterward. The honest version must re-validate the linear-map bounds
against the live page table on every trans_gen change, and the risk of a
silent wrong-translation bug is the highest of anything in this file. Do not
start here.

---

## Tier 3 — guest OS work (score by wall clock, never MIPS)

### 3.1 Idle fast-forward — mostly EXISTS; finish it

`Machine::run` already warps on WFI: `Status::Wfi` → `idle_skip_mtime(next)`
with `next = stimecmp.min(mtimecmp)` (lib.rs:245-269), capped by
`MAX_IDLE_SKIP` = 1ms of mtime per jump (device_bus.rs:322).

Remaining, in order:
1. **Verify it fires under the JIT** — WFI is uncompilable so control does
   return to `run`, but confirm with a counter (idle skips per second at an
   idle shell prompt; should be ~1000/s with the 1ms cap, and host CPU near
   zero). If host CPU is high at an idle prompt, find what's spinning first —
   that's a bug, not a tuning knob.
2. **Raise the cap adaptively:** consecutive skips with no intervening
   interrupt can double the jump (1ms → 2 → 4 … cap 100ms). The cap exists
   for RX timing constants (networking.md — timing constants have produced
   misleading ping RTTs before); keep any virtio RX pending check as an
   immediate-wake condition and test ping RTT + apk fetch after.
3. **Spin-loop warp is a research add-on:** a guest polling `time` without
   WFI (idle=poll) can be detected via the existing `dbg_time_reads` counter
   heuristic — park it unless (1) shows the guest actually does this; Linux
   defaults to WFI.

**Test:** interactive: idle shell → host CPU ~0%, then keystroke latency
unchanged (type at the prompt). Timing: ping RTT through the relay unchanged;
`apk add` wall clock unchanged or better. Guest `sleep 5` still takes ~5s of
*guest* time.

### 3.2 Kernel config diet

**Where:** `kernels/` holds the kernel; find the .config used to build it (if
the config isn't in-tree, extract with `scripts/extract-ikconfig` or rebuild
from the recipe in ALPINE_BOOT.md).

**Do, one change per boot measurement:** candidates in expected order:
`CONFIG_HZ=100` (from 250/1000 — check current), `CONFIG_NO_HZ_IDLE=y`,
`CONFIG_PREEMPT_NONE=y`, disable `CONFIG_DEBUG_*` (list what's on first),
`CONFIG_RISCV_ALTERNATIVE` off (this also removes early-boot text patching —
which interacts with the fence.i flush path; keep the flush regardless), LTO
if the toolchain supports it, and strip modules/drivers with no matching
virtio device.

**Metric:** retired instructions to the shell marker (the harness counts
steps — deterministic, noise-free) AND wall clock via ABBA cold boots. A
config change that cuts steps 20% is a 20% wall win at fixed MIPS and is
immune to the measurement floor. **The old boot-to-userspace step baseline no
longer applies to a new kernel** — re-baseline per config, keep the old kernel
binary around, and keep the snapshot compatibility story straight: a new
kernel invalidates existing snapshots; the web page caches boots per RAM size
(7f3813d), so bump whatever cache key covers the kernel image.

### 3.3 Console batching

**First measure:** count UART MMIO stores during a cold boot (one counter in
device_bus). Boot output is ~tens of KB → order 10^4-10^5 trapped stores; if
that's <0.1% of 1.2G boot instructions, only the *trace truncation* around
each MMIO matters, and the cheap fix is `quiet loglevel=4` on the kernel
cmdline (find where the DTB/cmdline is built — fdt.rs in riscv-machine).
virtio-console is a real device implementation project; demand evidence from
the counter before starting it.

### 3.4 PV bulk-memory SBI extension

**Where:** sbi.rs handles SBI calls natively already — adding a vendor
extension is small. The kernel side is a patch to the RISC-V
`clear_page`/`copy_page` (arch/riscv/lib/), probing the extension once at
boot via `sbi_probe_extension`.

**Do:** extension id in the vendor space (0x09000000+): `ZERO_PFN(gpa)`,
`COPY_PFN(dst_gpa, src_gpa)`. Host validates both addresses are guest RAM
(reuse the bus's RAM-range check), does the memset/memcpy on the backing
store, returns. Guest patch: if probe succeeded, `clear_page` becomes one
ecall. Keep the C fallback — the same kernel must still boot on the
interpreter build and on real hardware.

**Pitfalls:** the JIT's inline TLB caches *host addresses of guest pages* —
host memset through the backing store is coherent with that (same memory),
but the *decoded-instruction cache and compiled blocks* are keyed on physical
addresses: zeroing a page that contained code must invalidate those. Cheapest
correct rule: have the hypercall bump `icache_gen` only when the target page
range overlaps any compiled block's page (the block cache can answer that; if
plumbing is awkward, v1 = always treat like fence.i and measure — page-zero
storms during boot may make the always-flush version a net loss, which the
step counter will show immediately).

**Test:** battery on BOTH kernels (patched, unpatched). Steps-to-shell and
apk wall clock. Expect the win concentrated in fork/exec-heavy work
(`apk add`, shell pipelines).

### 3.5 SBI audit — CLOSED

Already native; supervisor.rs:385-391. Nothing to do. Kept here so nobody
re-opens it.

---

## Tier 4 — research grade

Ordering rule: nothing in this tier starts while a Tier 1-3 item with a
measured target sits unattempted, except 4.1 which is independent of MIPS
(it attacks warm-up, which the Node benchmark cannot even see).

### 4.1 Persistent JIT cache (IndexedDB)

**Where:** browser side only — web/ worker; the wasm module cache lives in
the JS host (bench/jit-dispatch.js / the worker's module registry). V8 allows
structured-cloning `WebAssembly.Module` into IndexedDB.

**Do:** key = hash of (guest physical code bytes the block was compiled from,
codegen version constant — bump it on every riscv-jit change). On block
compile, persist async; on snapshot restore, bulk-load and *revalidate each
entry by re-reading and hashing those guest bytes* before insertion (the
snapshot guarantees the bytes, but a persisted cache can outlive the snapshot
it was built against — never trust the key alone). Invalidate wholesale on
`icache_gen` bumps as usual once live.

**Test:** the metric is time-to-full-speed after restore in the *browser*:
instrument MIPS-per-second for the first 60s after restore, warm vs cold
cache (the stats panel from 61afb00 already samples throughput). Correctness:
poison one persisted entry's stored bytes in devtools and confirm
revalidation rejects it. No ABBA needed — the effect is qualitative or it
isn't there.

### 4.2 AOT-translate the kernel from the snapshot image

Only after 4.1, which builds the same persistence machinery. AOT = run the
existing trace compiler offline over the snapshot's kernel text (post
alternatives-patching, which sidesteps the self-modification window), emit
the same modules 4.1 would have cached, ship as a file. If 4.1's revalidation
story is solid this is mostly a build step, not new runtime code.

### 4.3 Idiom recognition → `memory.copy`

Recognize musl/kernel memcpy inner loops at trace-build time and emit bulk
wasm ops when src/dst/len translate contiguously *within single pages* (cross
page = fall back; do not build multi-page contiguity proofs). Overlaps 3.4 —
do 3.4 first; it catches the kernel half with far less machinery, and its
counters will say how much userspace copying remains.

### 4.4 Wasm branch hinting

The proposal is shipped in current V8. Emit "unlikely" on every
`bail_if_faulted` branch and the TLB-miss `else` arm. Small, self-contained,
good first research task; needs the `wasm-encoder` crate to support the
custom section (check its changelog; hand-roll the section if not). ABBA in
Node first, then confirm in-browser via the stats panel, since engine
versions differ.

### 4.5 Macro-op fusion

Count first (trace-build histogram of adjacent pairs: `lui+addi`,
`auipc+ld`, `slli+srli`); the pairs share the value-forwarding plumbing from
1.5, so do them together or not at all. Ceiling is a few %; fine as a
batched-with-1.5 experiment, not alone.

### 4.6 SMP second hart — PARKED

Recorded as likely net-negative for the terminal workload (atomics/fence
codegen assumptions die, kernel gains SMP overhead). Do not start without a
workload that demonstrably needs it. If started: it is a project on the scale
of the original JIT, plan accordingly.

---

## Appendix: pre-flight checklist for any lever

```
[ ] Read the relevant "closed" entries in speed-checklist.md
[ ] Baseline: build-jitnode.sh, one bench/jit-vm-test.js run, save the histogram
[ ] Branch from master; one lever per branch
[ ] Implement behind a revert flag when cheap (pattern: __noXyz on the wasm host)
[ ] Battery green, cold boot included; difftest generator extended if codegen changed
[ ] ABBA (bench/jit-ab.py 4) or wall-clock ABBA for Tier 3 — nothing else counts
[ ] Record the result in docs, wins AND losses, with the numbers
[ ] Commit locally; do not push; do not touch web/pkg unless shipping to the page
```
