# JIT status

**Working, deployed, ~7x.** Enabled with ; off by default.

| | MIPS |
|---|---|
| interpreter (wasm) | ~12 |
| JIT | ~85 (72.6 / 85.5 / 96.6 across runs) |

Verified by booting the real guest under Node -- the same V8 the browser
runs -- with the JIT on and off and diffing the console byte for byte.
isa_suite 134/134; 400 randomised differential cases match the interpreter
on registers and memory through every call path.

## How it got there

Each step came from measuring where the time actually was, and several
contradicted the obvious guess.

| change | effect |
|---|---|
| ALU-only blocks | 1.64x |
| compile branches | 2.48x |
| chain blocks in Rust | 3.30x |
| tail-call chaining in wasm | 45.8 MIPS |
| inline the TLB probe | ~57 MIPS |
| retry after an obstacle | ~85 MIPS |

The last one was the largest and the least obvious. Coverage was stuck at
87.6% because a compiled run stops before the first instruction the
compiler cannot handle, and the interpreter then ran the whole *remainder*
of that basic block --  is only set by a control transfer.
Offering the JIT the instruction after the obstacle took coverage to 96.2%.

## Measurements that redirected the design

- **One module per block does not work.** Rotating 400 blocks cost 293 ns
  per entry against 21 ns for one repeated: the host call site goes
  megamorphic. One module with an internal dispatcher: 38 ns.
- **Imports must be wasm-to-wasm.** A JS closure costs ~32 ns per call, a
  wasm function ~6 ns. Measuring the JS version argues for an inlined TLB
  for the wrong reason.
- **...but the inlined TLB was needed anyway**, for the opposite reason.
  The 6 ns was an empty stub; the real host call does an MMU translation
  and a bus dispatch, and that only became visible once chaining removed
  everything else.
- **Hotness was never the problem.** Lowering the threshold 50 -> 8 -> 4
  each produced far more compiled blocks and slightly worse throughput.

## What did not pay, so it is not retried

- A JIT-side data-translation cache in Rust (38.9/40.1/41.6 vs 43.6/45.6):
  a cache in front of the MMU's own TLB, costing more than the call it
  skipped.
- Moving the fault check to the slow path only. Strictly less work on the
  hot path, but measured lower (median 65.8 vs 88) and the box was too
  noisy to resolve it. Reverted rather than banked.

## A note on reading the browser console

The browser console tool returns a tab's accumulated log, not the messages since
the last navigation. A tab that has already run a broken build keeps showing
those errors on every subsequent reload, which is how a wall of "recursive use
of an object detected which would lead to unsafe aliasing in rust" got
attributed to a pre-existing re-entrancy in the worker. It was not pre-existing:
it was the null-slot trap from the code-cache discard bug, and it disappeared
when that was fixed.

Checked the way it should have been the first time -- an error listener in the
worker caught nothing, one in the page caught nothing, and a **fresh tab** shows
no such error at all, before or after a cache reset under load.

When a browser symptom needs attributing, use a new tab.

## Lever 1 was measured and is much smaller than estimated

The plan said "M extension, so those stop splitting blocks" and guessed the
interpreted 3.8% could largely go away, worth ~39%. Counted over a boot plus a
userspace workload -- 2.0G instructions -- the instructions the compiler
actually refuses are:

| kind | share of all executed |
|---|---|
| system (ecall, sret, fence, sfence) | 0.504% |
| CSR access | 0.398% |
| other | 0.286% |
| **mul family** | **0.189%** |
| atomics (lr/sc) | 0.053% |
| **div / rem** | **0.033%** |
| **total refused** | **1.46%** |

So the M extension is 0.22% of executed instructions. Compiling it perfectly
saves ~0.18 ns of an 11 ns average: under 2%. Not worth building.

Two further things this settles. The system instructions -- 0.5%, the largest
group -- are traps and returns, which *must* leave compiled code; there is
nothing to win there. And the refused 1.46% does not account for the interpreted
3.8%: the other ~2.3% is compilable code that simply has no block yet, which
aggressive retrying was already shown not to fix (96.2% -> 97.0%, with blocks
going 12k -> 56k and throughput falling).

Recoverable by instruction coverage: CSR plus mul/div/atomics/other, about
0.96%, worth ~7%. Real, but not the lever.

## Lever 2 is dead: registers in locals do not pay on this target

Tried three times, twice after superblocks made traces longer -- which was the
condition the first attempt was said to need.

| variant | code for 400 blocks | median of paired runs |
|---|---|---|
| registers in memory (shipped) | 106,683 | — |
| promote all, write back at each fault site | 144,334 | — |
| promote all, one write-back path | 119,231 | 0.911 |
| promote only registers used 4+ times | 114,460 | 0.915 |

Every variant measured slower, by about the same amount, including the one that
promotes only registers a trace uses heavily and leaves the rest in memory. That
consistency is the result: it is not a tuning problem.

The premise appears to be wrong for wasm on V8. The reasoning was that every
operand being a load from the register file must cost more than a local read.
But V8 keeps the register-file base in a host register, and a wasm linear-memory
load with guard pages carries no bounds check, so `i64.load` from the register
file is about as cheap as `local.get` -- both L1 hits, and V8 is free to keep hot
values in host registers either way. What promotion definitely adds is a
prologue and a write-back.

**Do not try this again without first showing that a register-file load is
actually more expensive than a local read on the target engine.** That
measurement, not the intuition from native JITs, is the thing to check.

## Superseded: the earlier reading of lever 2

Guest registers were moved into wasm locals: every register a block touches
loaded once at entry, the body working on locals, destinations written back at
exit. It was correct -- 400 differential cases and a byte-identical guest boot
-- and it was slower.

Wall clock could not resolve it: twelve interleaved ABBA pairs gave a median
ratio of 0.911 with individual pairs from 0.66 to 1.23, a mean of 0.93 +/- 0.12.
The interval includes 1.0, so on its own that says nothing.

Generated code size does resolve it, being deterministic:

| build | code for the same 400 blocks |
|---|---|
| registers in memory | 106,683 bytes |
| registers in locals, write-back at each fault site | 144,334 (+35%) |
| registers in locals, single write-back path | 119,231 (+12%) |

The first version emitted the whole register write-back inline at every memory
access, because a faulting access must commit before returning. Restructuring so
the body sits in a `block` and fault sites `br_if` out of it -- one write-back on
the normal path, one at the bail label -- recovered most of that. It is still
12% larger than not doing it at all.

The arithmetic says why, and it is structural rather than a coding problem. For
a seven-instruction block touching six registers, the prologue and epilogue cost
about 42 instructions while the body saves about 14. **Blocks are too short to
amortise it.** Mean block length is ~7 instructions.

So this lever is not independent of lever 3: it needs superblocks first. With
blocks several times longer the prologue and epilogue are paid once over far
more instructions, and the same change should pay. Attempting it again before
that would repeat this result.

## Remaining levers toward 10x

At ~11 ns per instruction: ~8 ns is compiled code over 96.2% of
instructions, ~3 ns is the interpreter over the remaining 3.8%.

1. **Guest registers in wasm locals.** Every operand is currently a load
   from linear memory. Loading each register used by a block once, and
   writing back at the exit, removes most of that traffic. Needs care at
   bail points, which must write back before returning.
2. **M extension** (mul/div/rem), so those stop splitting blocks. Division
   needs explicit guards -- RISC-V defines results for divide-by-zero and
   overflow where wasm traps.
3. **Superblocks**, extending compilation past a branch along the hot path,
   which lengthens blocks and amortises the per-block chain probe.

## Measurement note

The dev box is too noisy to resolve changes under ~15%: repeated runs of
identical code have spanned 72.6 to 96.6 MIPS. Anything smaller needs a
quieter machine, and should not be believed from a single pair of runs.
