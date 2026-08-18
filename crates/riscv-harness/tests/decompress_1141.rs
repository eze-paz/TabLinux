#[test]
fn check_1141() {
    let ins = riscv_core::compressed::decompress(0x1141);
    eprintln!("decompress(0x1141) = {:?}", ins);
}
