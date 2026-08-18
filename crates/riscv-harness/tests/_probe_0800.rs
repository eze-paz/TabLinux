use riscv_core::compressed::decompress;

#[test]
fn probe_0800() {
    let ins = decompress(0x0800).unwrap();
    eprintln!("0x0800 -> {:?}", ins);
}
