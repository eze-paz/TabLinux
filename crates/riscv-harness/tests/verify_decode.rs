use riscv_core::compressed::decompress;
use riscv_core::types::Instr;

fn decode_one(raw: u16, desc: &str) {
    match decompress(raw) {
        Some(instr) => {
            let s = format!("{:?}", instr);
            eprintln!("{}: raw={:#06x} -> {}", desc, raw, s);
        }
        None => eprintln!("{}: raw={:#06x} -> None", desc, raw),
    }
}

#[test]
fn verify_decodes() {
    decode_one(0x8d91, "0x8d91 step190840");
    decode_one(0x90ae, "0x90ae step190841");
    decode_one(0x8e4d, "0x8e4d step190850");
    decode_one(0x962e, "0x962e step190844");
    decode_one(0x8d4d, "0x8d4d step190854");
    decode_one(0x8131, "0x8131 step190853");
}
