# Python-in-guest optimization campaign — measurements

Goal: speed up CPython running inside the emulated RISC-V Linux guest.
Workload: `pybench/pybench.py` (pure-Python phases: calls, arith, dicts,
lists, strings, attrs), run via `pybench/pybench-vm.js` under node, timed
between console markers so mount + interpreter startup are excluded. Guest
work is fully deterministic; every run cross-checks the Python results
(`CHECK` line) and byte-determinism of guest-side counters.

## Methodology

Wall MIPS on this box swings ~±30% between IDENTICAL runs (same binary, same
guest work: observed 40–88 MIPS an hour apart, 92–160 within one batch).
Sequential comparison is therefore unsound. Every claim below is from
`pybench/pybench-ab.js`: interleaved ABBA pairs, fresh node process per run,
median of per-pair ratios, 8 pairs, SCALE=3 (~600M-step window).

## Baseline profile (JIT on, window-delta stats)

- JIT 8.0× interpreter on Python (vs 11.2× on a shell workload)
- Coverage 99.0% — interpreted instructions are a non-issue
- insns/host-entry 188; insns/chain 484
- Chain-probe misses: 60% conflict evictions (8192-entry table),
  17% budget, 17% chain-cap, 8% empty
- Inline-TLB hit rate ~99.97% — probe COST is the lever, not miss rate

## Lever results (median B/A, pairs favoring B)

| Lever | Result | Verdict |
|---|---|---|
| TLB-probe group CSE, v1 (full fallback per member) | 0.884, 0/8 | REGRESSION — code bloat |
| Register store-to-load forwarding (`REG_FWD_ON`) | 0.853, 2/8 | REGRESSION — Liftoff locals aren't registers; gated off |
| TLB-probe group CSE, v2 (slim host-call fallback) | 1.041, 6/8 | WIN (small) |
| Chain table 8192 → 65536 entries | 1.084, 7/8 | WIN |
| chain_max 1024 → 16384 (runtime knob) | 0.742 @ SCALE=1, noisy 2/6 | inconclusive |
| chain_max 1024 → 8192, retest on combo @ SCALE=3 | 1.053, 5/8 | CONTRADICTS the above — unresolved, left at 1024 (sweep via `set_chain_max`) |
| COMBO: slim CSE + chain 65536 | **1.124, 7/8** | ADOPTED — wins compose |
| combo on the SHELL workload (bench/jit-ab.py) | 1.037, "no resolvable difference" | no regression outside Python |
| TurboFan ceiling (`--no-liftoff`, same binary both sides) | 1.048, 4/6 | dynamic tiering already works; codegen-quality headroom ≈ 5% |

Meta-lesson (twice-measured): on V8's baseline wasm tier, ADDING generated
code to save memory traffic loses; SHRINKING code or removing host round
trips wins. Optimize for code size and exit count, not instruction count.

## Levers resolved WITHOUT building, by measured bound

- **Per-site jalr inline caches**: their target (conflict evictions, 60% of
  chain misses) fell to the 64k chain table instead. Remaining per-hop hash
  cost ≈ 10 wasm ops ≈ <1% of executed ops. Dominated.
- **Superblock fusion of hot chains**: interior hops ≈ entries − chains ≈
  693k/window; each hop tail ≈ 35 wasm ops against 188 insns × ~40 ops per
  entry → mechanical win 0.5–1.5%. Indirect win (wider CSE scope) bounded by
  the whole CSE lever (+4%). High cost, low ceiling — not built.

## Top remaining lever (untested, rig is ready for it)

Rebuild the GUEST CPython instead of the emulator: the guest runs Alpine's
CPython 3.14 (gcc build → computed-goto dispatch, one indirect jump per
bytecode = one chain-table probe each). A `--without-computed-gotos` (or
clang tail-call-interpreter) riscv64-musl cross build swaps in via
`mkpydisk.py` + `LD_LIBRARY_PATH`/`PYTHONHOME` with zero engine changes, and
the A/B rig measures it end to end. Also unmeasured: block-formation
hotness threshold (304k/window "noblock" chain exits).

## AOT / jit-cache / eviction session (2026-08-13, later)

- Session profiler shipped (commit 1dc48f3) and deployed; forced-reset
  experiment (cap 12000): 4 resets in 98s, MIPS held ~175, only the ~2.4k-block
  hot set re-formed after each discard. Discard-all eviction is fine because
  re-formation is lazy and hot-only.
- IndexedDB module persistence had been silently dead for the whole project:
  modern Chrome cannot structured-clone WebAssembly.Module into IDB; every put
  threw into a swallowed catch ("cache init: entries:0" on every load). Fixed
  by storing bytes + background-compiling at init — then measured USELESS:
  0/46 cross-session boot hits, because batch composition is timing-dependent.
- Deterministic batching (sort by paddr, content-defined cuts, min 8 max 96):
  fixed the keys (9/44 hits) but −32% steady-state (6/6 pairs, bench/jit-ab.py) —
  more, smaller, sorted modules multiply cross-INSTANCE tail-calls (V8
  instance-switch cost). Reverted. The chain wants flow-order big modules.
- Bound on the whole lever class from the profiler: V8 compile ≈ 0.3s/session
  (1.3% of boot wall). Warmup cost is block FORMATION, which module caching
  cannot skip. True AOT would persist formed block SETS keyed on guest code
  bytes — worth <2% today; not built.
- Final config: in-memory module cache ON (worth real time during reset storms:
  176/485 hits), IDB persistence OFF (`PERSIST=false` in jit-cache.js with the
  full reasoning), old stores cleared on init.

## Storage-path measurements (2026-08-14, pybench/iobench-vm.js, node, JIT on)

| Phase | Result | Reading |
|---|---|---|
| ext4 bulk write (48M, fsync) | 13.9 MB/s, 2 insns/byte | guest-CPU-bound |
| ext4 bulk read (cold cache) | 6.6 MB/s, 3 insns/byte (~11.7k insns per 4K page) | guest-CPU-bound: block layer + page-cache copy run emulated |
| 9p bulk read (32M, msize 128K) | 104–185 MB/s, 0.06 insns/byte | host-side native copy; NOT a bottleneck |
| per-file open+read+close (shell builtin) | ~5–10 ms/file, ~75–95k insns/file, both mounts | emulated kernel path length, not the backend |
| fork+exec (busybox cat) | ~1.2M insns per spawn | subprocess-heavy patterns crawl; single-process fine |
| 9p cache=loose re-read | no clear win in this pass | noisy; retest if metadata churn shows up in real jobs |

Conclusion: the storage BACKENDS are not the bottleneck — the emulated CPU
cost of kernel I/O paths is. Every MIPS improvement speeds "the disk" too.
COMPLETION_LATENCY=2000 insns is negligible. Deferred: worker-side blk/9p
counters in the session profiler (vm-worker.js was under concurrent edit).

## gentrans churn: CONFIRMED single-page sfences (user session 2026-08-14)

13m50s real Python data job: gen bumps = satp 170 / sfence-all 189 /
**sfence-page 69.4k**, against 52.9M gentrans chain stops (~760 per bump =
full-table wipe then refill). Design sketch for the fix: do NOT bump chain
gen on single-page sfence; keep a page-tag filter (bitmap/bloom over chain
keys' vpns, set on insert) — most flushed pages are data pages with no chain
entries, so the common case skips invalidation entirely; on a filter hit,
scan-and-kill matching keys (65536×16B ≈ 50–100µs, rare). Same treatment for
the 4096-entry vcache. Expected: most of the 52.9M gentrans stops and a chunk
of the 54.4M empty-slot refill misses disappear.

## Single-page sfence key-page filter (2026-08-14) — ADOPTED

Single-page `sfence.vma` (allocator munmaps: 69–147k per real session, vs a
few hundred satp/global) used to bump `trans_gen` unconditionally, wiping all
64k chain entries + vcache each time (~760 chain stops per wipe). Now the
supervisor defers the decision; the host bumps only if the flushed page might
hold a chain/vcache key.

Two iterations, both fully measured:
- v1, 64k-bit hash bitmap: PLACEBO. Per-page false positives compound across
  a multi-page munmap (64 pages × ~5% aliasing ≈ 95% per munmap): storm A/B
  0.950, gentrans only −55%.
- v2, exact generation-stamped vpn set (16k slots, entry = vpn<<12|gen&0xfff,
  no clearing — stale gens self-invalidate; overflow ⇒ conservative for the
  generation): 80,223/80,223 sfences skipped, ZERO conservative hits, ZERO
  in-window gentrans stops.

| Workload | median B/A | pairs |
|---|---|---|
| mmap-churn storm (16K maps, per-page shootdowns — the real session's shape) | **1.479** | 8/8, spread 1.44–1.59 |
| general pybench | **1.044** | 8/8, spread 1.02–1.07 |

Gates: difftest 400/400, jit-vm-test byte-identical, identical CHECK across
all 32 A/B runs. Bench-design lesson: a 256 KiB munmap crosses Linux's
flush-all threshold and goes GLOBAL — the first storm accidentally measured
global sfences, which the filter deliberately does not touch.

Remaining gentrans floor = satp writes (context switches) — the ASID
revalidation lever, unbuilt.

## Correctness gates (all levers pass before A/B)

1. `cargo run --release -p riscv-jit --example difftest` + `node
   bench/jit-difftest.js` — 400/400 register+memory match (no-TLB path)
2. `node bench/jit-vm-test.js` — interpreter-vs-JIT byte-identical console on a
   real Alpine boot + shell workload (exercises TLB + group paths)
3. `pybench` CHECK — identical Python results every run

## Rig

- `kernels/mkpydisk.py` — builds `kernels/disk-python.img` (256 MiB ext4,
  Alpine riscv64 python3 + deps + /bench), no root needed
- `pybench/pybench-vm.js` — restore shell.snap, attach the python disk, run
  bench, report windowed MIPS + windowed JIT stats (JIT=0/1, SCALE, CHAINMAX,
  JITNODE=/path/to/shim)
- `pybench/pybench-ab.js A B pairs [K=V] [A.K=V] [B.K=V]` — interleaved A/B;
  `B.NODE_FLAGS=--no-liftoff` style per-side V8 flags supported
