# rusty-riscv

A **RISC-V (RV64GC) Linux virtual machine that runs in the browser**, written in
Rust, with a hand-written **RISC-V → WebAssembly block JIT**. It boots a real
Alpine Linux userspace and runs unmodified `riscv64` binaries — CPython, busybox,
`apk`, and more — at JIT speed. No server does the emulation, no plugins: it's
entirely client-side WebAssembly.

Most in-browser RISC-V emulators are interpreters. This one **compiles hot guest
code to WebAssembly at runtime and tail-calls between the compiled blocks**,
which is where the interesting engineering lives.

---

## Architecture

The guest executes through two tiers. Cold code runs in a portable interpreter;
once a block of guest instructions is hit often enough it is compiled to a
WebAssembly function and installed in a growable indirect function table.

```
            guest RISC-V instructions
                      │
      ┌───────────────┴───────────────┐
      │                               │
  interpreter                    block JIT  (riscv-jit)
  (riscv-core)                   RISC-V → wasm, keyed by
  cold / not-yet-hot             physical address
      │                               │
      └──────────────┬────────────────┘
                     │  compiled blocks tail-call their
                     │  successors through a hash chain table
                     ▼
        linear memory  ── inline software TLB (Sv39)
        virtio-mmio (block, net) · CLINT · PLIC
```

### Crates

| crate | responsibility |
|-------|----------------|
| `riscv-core` | instruction decode + portable interpreter core |
| `riscv-supervisor` | S-mode CSRs, Sv39 MMU (software page-table walker), traps |
| `riscv-machine` | the machine: run loop, block cache, chain table |
| `riscv-jit` | the RISC-V → wasm block JIT (`wasm-encoder` codegen) |
| `riscv-devices` | CLINT, PLIC, virtio-mmio block + net |
| `riscv-hostnet` | in-guest TCP/IP termination + WISP bridge glue |
| `riscv-wasm` | `wasm-bindgen` bindings; the browser entry point |
| `rvdis` | a small RISC-V disassembler used for debugging |
| `web/` | the browser front-end (a Web Worker drives the VM + JIT pump) |

### How the JIT works

Traces of guest instructions are compiled to wasm functions keyed by **physical
address** and installed in an append-only indirect function table (indices are
never reused). Blocks **tail-call their successors** through a hash-keyed chain
table using `return_call_indirect`, so steady-state execution stays inside
WebAssembly with no host round-trip — the host is entered only when a chain
probe misses.

Guest memory accesses are compiled to an **inline software TLB probe**: the
translation is checked directly in the emitted wasm and, on a hit, the access
becomes a direct linear-memory load/store. A host call happens only on a TLB
miss. Address-space switches (`satp` writes) **restore** a cached generation per
address space instead of invalidating it, so returning to a process revalidates
its compiled blocks for free rather than re-translating them.

---

## Benchmarks

Measured on a laptop under Node/V8 (the same wasm engine as the browser). These
are representative steady-state figures — absolute MIPS varies by host and
browser, so treat them as orders of magnitude, not guarantees.

- **CPython workloads run at ~150–180 MIPS** under the JIT. A pure Python
  data-parsing loop stays ~**81 % inside compiled block bodies** (the rest is
  translation, the run loop, and device polling).
- The VM boots Alpine to a shell and runs `python3`, `apk`, `busybox`, etc.
  unmodified.

The single most useful document in this repository is
[`crates/riscv-jit/OPTIMIZATION_LEDGER.md`](crates/riscv-jit/OPTIMIZATION_LEDGER.md)
— a measured record of **every** performance lever tried on the JIT, including
the ones that failed. Highlights of what actually moved the needle:

| lever | effect |
|-------|--------|
| Inline software TLB (vs host-call per access) | **~2.3× overall** |
| Batched interrupt / device poll | **~1.5–2× on CPython** |
| Selective `fence.i` flush (drop spurious cache flushes) | cold-interpreter work **51M → 15M** instructions |
| ASID-keyed generations (restore, don't invalidate, on `satp`) | **+14 %** on pipe-heavy workloads |
| `chain_max` (blocks per host entry) | **1.2–1.5×** across workloads |

And, just as valuable, the levers measured **null** so they don't get retried:
macro-op fusion, register-residency-into-locals, TLB-probe hoisting, memset
SIMD, static tail-call linking, per-block inline caches — each was correct but
gave no resolvable win, because the hot code is already TurboFan-optimized
machine code. If you plan to work on the JIT, **read the ledger first.**

Methodology note: this is a noisy laptop, so only **within-run A/B ratios** are
trustworthy — never compare absolute MIPS across sessions. The benchmark
harnesses (`mips.js`, `pybench/`, `jit-vm-test.js`, and the disassembler
`rvdis`) are included.

---

## Networking (WISP)

A browser cannot open a raw TCP socket, so the VM does networking in two hops:

1. **In-guest TCP/IP termination.** `riscv-hostnet`'s `fake_network` terminates
   the guest's TCP connections *inside* the VM — the guest gets a real `eth0`
   via `virtio-net`, and its SYNs are answered by an in-VM stack rather than by
   a real router.
2. **A WISP bridge to a relay you run.** Each terminated connection is bridged
   to a **WISP** stream over a single WebSocket to a relay; the relay is the
   party that opens the real socket to the internet.

**WISP** is a small WebSocket multiplexing protocol (the v86-compatible v1
shape, BSD-2-Clause). All streams share one WebSocket; frames are
`[type:u8][stream:u32 LE][payload]`:

| frame | meaning |
|-------|---------|
| `CONNECT` | open a TCP stream to `host:port` |
| `DATA` | payload bytes, either direction |
| `CONTINUE` | flow-control credit (the relay opens the window) |
| `CLOSE` | close a stream |

So the path is: **guest → `virtio-net` → in-VM TCP stack → WISP client →
WebSocket → your WISP relay → real TCP.** This is the same "tunnel TCP over a
WebSocket relay" pattern browser SSH clients use.

**You must point it at your own relay.** No relay is bundled and none is
hardcoded. The WISP endpoint is resolved, in order, from a `?wisp=ws://…` URL
parameter, or defaults to a `/wisp` path on whatever host serves the page
(`wss://<your-host>/wisp`). Any standard WISP v1 server works; run one on your
own infrastructure. See [`docs/networking.md`](docs/networking.md).

---

## Build

Native (interpreter, tests, examples):

```bash
cargo build --release
cargo test
```

Browser (the JIT is instantiated by the host, so `riscv-wasm` targets `wasm32`):

```bash
rustup target add wasm32-unknown-unknown
RUSTFLAGS="-C link-arg=--export-table -C link-arg=--growable-table" \
  cargo build --release --target wasm32-unknown-unknown -p riscv-wasm
wasm-bindgen --keep-lld-exports --target web \
  --out-dir web/pkg target/wasm32-unknown-unknown/release/riscv_wasm.wasm
```

Serve `web/` over HTTP with cross-origin isolation headers (COOP + COEP) so
`SharedArrayBuffer` is available, then open it.

---

## Guest OS images

`kernels/` contains a prebuilt Alpine `riscv64` kernel + initramfs and a small
ext4 disk, bundled for out-of-box runnability. **These are not part of this
project's source** and retain their own licenses — see
[`NOTICE`](NOTICE) for the Linux kernel (GPL-2.0), Alpine, and CPython terms and
their upstream sources.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
