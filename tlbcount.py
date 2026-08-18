"""Count TLB misses, to see whether the inlined probe is actually hitting.

Inlining the probe moved compiled code from 15.3 to 13.0 ns per instruction --
real, but far less than a direct load (1.45 ns) replacing a host call should
give. Either the probe costs more than expected, or it is missing.

Every miss lands in the host's load/store path, so counting calls there gives
the miss count directly, and accesses are roughly 30% of compiled instructions.

The suspicion worth testing: the generation word folds in the privilege level,
so every trap and return invalidates the whole TLB. The kernel traps constantly.
If that is it, the fix is separate tables per privilege rather than one table
thrown away on each switch.
"""
p = "crates/riscv-machine/src/lib.rs"
s = open(p).read()


def once(old, new):
    global s
    assert s.count(old) == 1, (s.count(old), old[:70])
    s = s.replace(old, new, 1)


once("""    /// Times the host entered compiled code, i.e. chains rather than blocks.
    pub jit_chains: u64,""",
     """    /// Times the host entered compiled code, i.e. chains rather than blocks.
    pub jit_chains: u64,
    /// Compiled memory accesses that missed the inlined TLB and had to call
    /// the host. Accesses that hit never touch Rust, so this is the miss count.
    pub jit_mem_slow: u64,""")

once("""            jit_chains: 0,""", """            jit_chains: 0,
            jit_mem_slow: 0,""")
open(p, "w").write(s)

p2 = "crates/riscv-wasm/src/lib.rs"
t = open(p2).read()
n = t.count("                let m = match JIT_VM.as_mut() {")
assert n == 2, n
t = t.replace("""                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return 0,
                };""",
"""                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return 0,
                };
                m.jit_mem_slow += 1;""", 1)
t = t.replace("""                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return,
                };""",
"""                let m = match JIT_VM.as_mut() {
                    Some(m) => m,
                    None => return,
                };
                m.jit_mem_slow += 1;""", 1)

old = """            self.m.jit.rejected as f64,
        ]"""
new = """            self.m.jit_mem_slow as f64,
        ]"""
assert t.count(old) == 1
t = t.replace(old, new, 1)
open(p2, "w").write(t)

p3 = "jit-vm-test.js"
u = open(p3).read()
u = u.replace("const [entries, chains, chainInsns, rejected] = b.stats;",
              "const [entries, chains, chainInsns, memSlow] = b.stats;", 1)
u = u.replace("""    console.log(`rejected blocks    ${rejected.toFixed(0)}`);""",
"""    // Accesses are ~30% of instructions, so this is the miss rate of the
    // inlined TLB: every hit stays inside wasm and never reaches Rust.
    const accesses = chainInsns * 0.3;
    console.log(`tlb misses         ${memSlow.toFixed(0)} of ~${accesses.toFixed(0)} accesses (${(100 * memSlow / accesses).toFixed(1)}% miss)`);""", 1)
open(p3, "w").write(u)
print("ok")
