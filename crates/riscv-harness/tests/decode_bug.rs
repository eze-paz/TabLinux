use riscv_core::decode::decode;
use riscv_core::types::Instr;

#[test]
fn test_decode_auipc_bug() {
    let raw = 0x00bf2717u32;
    let instr = decode(raw);
    eprintln!("decode({:#010x}) = {:?}", raw, instr);
    match instr {
        Instr::Auipc { rd, imm } => {
            eprintln!("  auipc rd={} imm={}", rd, imm);
            assert_eq!(rd, 7, "auipc rd should be t2 (x7), got x{}", rd);
        }
        other => panic!("expected Auipc, got {:?}", other),
    }
}
