#[test]
fn test_decode_b7673703() {
    let raw = 0xb7673703u32;
    let ins = riscv_core::decode::decode(raw);
    eprintln!("decode({:#010x}) = {:?}", raw, ins);
}
