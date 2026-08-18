use riscv_core::compressed::decompress;

#[test]
fn probe_e426_now() {
    eprintln!("e422 -> {:?}", decompress(0xe422).unwrap());
    eprintln!("e426 -> {:?}", decompress(0xe426).unwrap());
    eprintln!("ec06 -> {:?}", decompress(0xec06).unwrap());
    eprintln!("e04a -> {:?}", decompress(0xe04a).unwrap());
}
