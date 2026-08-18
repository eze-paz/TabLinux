#[test]
fn test_decompress_0000() {
    let result = riscv_core::compressed::decompress(0x0000);
    println!("decompress(0x0000) = {:?}", result);
    match result {
        Some(riscv_core::types::Instr::Addi { rd, rs1, imm }) => {
            println!("rd={} rs1={} imm={}", rd, rs1, imm);
            assert!(rd == 0 && rs1 == 0 && imm == 0, "Expected NOP, got rd={} rs1={} imm={}", rd, rs1, imm);
        }
        Some(other) => panic!("Expected Addi, got {:?}", other),
        None => panic!("decompress(0x0000) returned None"),
    }
}
