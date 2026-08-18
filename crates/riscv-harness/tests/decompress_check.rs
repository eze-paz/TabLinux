#[test]
fn check_1afd() {
    let ins = riscv_core::compressed::decompress(0x1afd);
    eprintln!("decompress(0x1afd) = {:?}", ins);
}
