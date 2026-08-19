# Making the interpreter faster

## Measure first — and measure the right workload

```bash
cargo run --release -p riscv-machine --example bench user   # userspace throughput
cargo run --release -p riscv-machine --example bench boot   # kernel-heavy, paired
```

Current: **~24 MIPS** userspace. Started this round at ~16 MIPS; boot went from
~133s to ~97s over the same period, though see the warning about boot timings
below — absolute wall times on this box drift enough that only the paired
comparison means anything.

Two numbers, because they disagree. `bench` runs `yes > /dev/null`, so the guest
sits in a tight userspace loop: it measures decode and dispatch and almost
nothing else. Anything on a kernel path — traps, MMU, device MMIO — barely moves
it. Gating the debug tracing measured as *nothing* on `bench` and ~9% on boot,
and boot is the workload that resembles `apk`.

If a change targets kernel or device code, `bench` will not see it. Say which
workload a number came from.

## The noise problem — read this before believing any result

This box is a 12th-gen hybrid part under WSL, which reports a flat 6x2 topology.
P-core/E-core placement is therefore invisible and unfixable from here;
`taskset` made things worse. CPU time from `/proc/self/schedstat` removes the
largest error term but not this one: an SMT sibling or an E-core still bills
full CPU time for reduced throughput, which is the bimodality in the worst
passes.

What works:

- **Short slices, many of them.** 200 x 2M (~150ms), headline = mean of the
  fastest 5%. At 100M per slice every pass caught interference and the spread
  was 43%; at 2M it is 4-8%. Nanosecond timing from `schedstat` is what makes
  slices that short measurable — `/proc/self/stat`'s 10ms USER_HZ ticks would
  quantise them to 7%.
- **If `top-spread` exceeds ~5%, discard the run.** That is what it is for.
- **Interleave A/B. Never compare two builds in sequence.** Host load drifts
  over the minutes a run takes. Sequential comparison showed the debug-gating
  change *losing* when it was actually neutral on that workload:

```bash
cargo build --release -p riscv-machine --example bench
cp target/release/examples/bench /tmp/bench_new
git stash && cargo build --release -p riscv-machine --example bench \
  && cp target/release/examples/bench /tmp/bench_old && git stash pop
./ab.sh 4
```

### The boot workload is scored differently, and must be

`bench user` cannot see kernel-path changes at all — it runs `yes(1)`, so the
guest never leaves userspace. Everything left to optimise lives on kernel paths,
which is what `bench boot` and `bench/boot-ab.py` are for.

A boot is **not homogeneous** — decompression, device probe and page-fault
storms run at genuinely different speeds — so the top-5% estimator is invalid on
it and reported a 17.9% spread when first tried. A boot is instead
**deterministic**: slice `i` executes the identical instruction sequence in both
builds. So `bench boot` writes one row per slice and `bench/boot-ab.py` compares slice
`i` to slice `i`, taking the median of ~563 paired ratios. Interference inflates
individual slices in one run or the other; the median does not care.

```bash
cargo build --release -p riscv-machine --example bench
cp target/release/examples/bench /tmp/bench_new
git stash && cargo build --release -p riscv-machine --example bench \
  && cp target/release/examples/bench /tmp/bench_old && git stash pop
./boot-ab.py 1
```

**It fails its own null test, and that bounds everything below.** Running the
*same binary* against itself has returned 1.000x, 0.904x, 1.086x, and 1.080x
with a 95% CI of [1.000, 1.160] whose four blocks were all above 1.0 — a
systematic bias, not scatter. Ruled out as causes: run order (it uses ABBA and
the effect survives swapping), slice misalignment (all slices pair up on
identical step counts), thermal soak (boot capped, 25s cooldowns), and a P/E
core split (all twelve WSL vCPUs benchmark at 17–19 MIPS; `taskset` lowers
throughput rather than stabilising it). The residue is the Windows host
scheduling the VM's vCPUs, which is not observable from inside the VM.

So `bench/boot-ab.py` refuses to call anything inside ±12% a win, however tight the
interval looks. It can **confirm a large change** — it reads the decode cache at
1.29x — and **cannot adjudicate a small one**.

Do not use whole-run totals as the metric either: on a null run they differed by
3.9%.

I got this wrong once already. 8d85183 claimed ~3% resolution on the strength of
a single null pair that happened to come out at 1.000x. One sample is not a
validation.

A boot is ~1.2G instructions and takes minutes, so this is the confirmation
instrument. Iterate on `bench user`, confirm on `bench/boot-ab.py`.

## What has been done

### Decoded-instruction cache — done, +27% userspace / -22% boot

32768-entry direct-mapped (see the size sweep below), indexed by `(paddr >> 1)`, holding
`(paddr, Instr, width, raw)`. Keying on the **physical** address is the trick: a
virtual key would be discarded on every `sfence.vma` and `satp` write — both per
context switch — and the flushing would have eaten the win. Physical mappings
only change when memory changes, so the only invalidator is `fence.i`, which is
the guest's promise that it rewrote instruction memory. Snapshot restore flushes
too.

Boot reached userspace at the *same step*, 567,856,066, which is the evidence
that the instruction stream is identical rather than merely plausible.

### Virtio slot scan — done, ~12%

`tick()` walked all eight MMIO slots per instruction and the IRQ refresh walked
them again, mostly to find `None`. Both now bounded by `virtio_n`.

### Debug tracing gated off — done, ~9% on boot

`step()` recorded every instruction into a 1024-entry ring, linearly scanned a
64-entry unique-PC set inside three kernel windows, and ran three more
diagnostic blocks after execute whose guards were true for all kernel code. All
behind `trace_enabled`, default false; `oneshot_alpine` turns it on.

### Per-instruction device work — exhausted, do not revisit

After the slot fix, stubbing `virtio_tick()` out entirely is worth **1.5%**.
The old idea of running it every 64 instructions and dividing
`COMPLETION_LATENCY` and the RX divisor is not worth the risk to timing
constants that already produced misleading ping RTTs once (networking.md).

### `codegen-units = 1` — not proven, reverted

Seven interleaved pairs: 17.19 -> 17.85 mean, +3.8%, winning 4 of 7. Below the
noise floor. Retry if the instrument improves.

### Devirtualising the bus — not proven, reverted

`&mut dyn Bus` became `<B: Bus + ?Sized>` across 18 signatures in execute.rs,
sbi.rs, mmu.rs and supervisor.rs — `?Sized` keeps the harness's `dyn Bus`
closures compiling, while `Machine`'s concrete `DeviceBus` monomorphises the hot
path. It builds and it is mechanical.

It measured ~3%: userspace A/B was inconclusive (-5.7%, +9.2%, -19.6%) and three
boot pairs gave +3.2%, -8.3%, -4.2%. That is inside the band where the harness
produces false positives on identical binaries, so there is nothing to bank, and
the diff was reverted rather than carried as unmeasurable complexity.

The patch is small and scripted (see the commit history around bc0bd31 for the
approach). Redo it if a machine that can measure 3% becomes available.

### Cache-size sweep — resolved at 32768, after being wrong once

Now `DCACHE_LEN = 32768`, worth **+10%** on boot over the 8192 it shipped with.
131072 is 2.9% slower than 32768, so the optimum is here. `bench user` sees no
difference between any of them, correctly: `yes` has a tiny working set.

Worth keeping the history, because the first attempt got the sign wrong.
Sequentially I measured 96.6s (8192), 119.1s (32768), 109.6s (2048), concluded
8192 was optimal and that 32768 "blew past L2" — a tidy mechanism for a result
that did not exist. Re-running 8192 then gave **136.8s**. The drift between runs
of identical code was larger than the differences being read off them.

Paired and interleaved, 32768 is 10% *faster* than 8192. The reasoning was not
merely unsupported, it was backwards, and it was reached by exactly the
sequential comparison this file already warned against.

## Next, best payoff first

**The measurement floor now governs what is worth attempting.** The cheap
interpreter wins are taken; what is left below is either sub-10% — unmeasurable
here, so it cannot be validated even if implemented correctly — or large enough
to see through the noise. Prefer the latter, or fix the floor first.

### 0. Fix the floor, if small wins matter

Hardware counters (`perf` is absent from this WSL image; `linux-tools-generic`
would add it) count instructions and cycles directly instead of inferring speed
from wall time on a hypervisor-scheduled vCPU. Failing that, a quiet machine
that is not a thermally-limited laptop. Everything sub-10% is blocked on this.

### 1. A JIT

Compile hot basic blocks to WebAssembly, which is what v86 does and why it is in
a different class. Worth 10x — comfortably above the noise floor, which is now a
point in its favour over the small wins. A project, not a change.

### 2. Threaded dispatch

Jump table or tail calls in place of the execute `match`. With decode cached,
dispatch is a larger share of what remains — but the expected size is single
digits, so it is blocked on item 0.

## Guard rails

```bash
cargo test --release -p riscv-harness --test isa_suite         # 134/134
cargo test --release -p riscv-harness --test boot_to_userspace # step count
cargo test --release -p riscv-machine --test snapshot          # determinism
```

`bench/boot-ab.py` also warns if the two runs produced different slice counts, which
means the instruction stream diverged — the pairing is then meaningless and the
ratio must not be believed.

Userspace is reached at step **567,856,066**. A different number means behaviour
changed, not just speed — a failure until explained. Verify against an
unmodified tree before believing it moved: the 567,614,707 once recorded here
was stale and cost a boot cycle to clear.

## Shipping it to the browser

`web/pkg/` is untracked and built locally, so none of this reaches the deployed
VM until it is rebuilt and synced:

```bash
cargo build --release --target wasm32-unknown-unknown -p riscv-wasm
wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/riscv_wasm.wasm
```
