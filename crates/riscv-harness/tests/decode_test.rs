use riscv_core::compressed::decompress;

#[test]
fn decode_crash_site() {
    let instrs = [0x8a2a, 0x84ae, 0x3c23, 0xfac4, 0xf0ef, 0x820f, 0x892a, 0x4763, 0x0005, 0x3603];
    for (i, raw) in instrs.iter().enumerate() {
        let off = 0xc1e458 + i * 2;
        let va = 0xffffffff80200000u64 + off as u64;
        match decompress(*raw) {
            Some(instr) => println!("off={:06x} va={:018x} raw={:04x} -> {:?}", off, va, raw, instr),
            None => println!("off={:06x} va={:018x} raw={:04x} -> decode failed", off, va, raw),
        }
    }
}
