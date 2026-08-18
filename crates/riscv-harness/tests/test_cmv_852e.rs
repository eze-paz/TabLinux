use riscv_core::compressed::decompress;
use riscv_core::types::Instr;

#[test]
fn test_cmv_852e() {
    let result = decompress(0x852e);
    eprintln!("decompress(0x852e) = {:?}", result);
    
    let r = 0x852eu32;
    let rd = ((r >> 7) & 0b11111) as u8;
    let rs2 = ((r >> 2) & 0b11111) as u8;
    eprintln!("rd = {} (a0={})", rd, rd == 10);
    eprintln!("rs2 = {} (a1={})", rs2, rs2 == 11);
    
    match result {
        Some(Instr::Add { rd, rs1, rs2 }) => {
            eprintln!("Decoded as Add rd={} rs1={} rs2={}", rd, rs1, rs2);
            assert_eq!(rd, 10, "rd should be a0");
            assert_eq!(rs1, 0, "rs1 should be x0 for c.mv");
            assert_eq!(rs2, 11, "rs2 should be a1 (register 11)");
        }
        _ => panic!("Expected Add instruction, got {:?}", result),
    }
}
