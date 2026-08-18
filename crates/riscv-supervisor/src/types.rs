#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Privilege {
    User = 0,
    Supervisor = 1,
    Machine = 3,
}

#[derive(Debug, Clone, Copy)]
pub struct MStatus {
    pub sd: bool,
    pub mbe: bool,
    pub sbe: bool,
    pub sxl: u8,
    pub uxl: u8,
    pub tsr: bool,
    pub tw: bool,
    pub tvm: bool,
    pub mxr: bool,
    pub sum: bool,
    pub mprv: bool,
    pub xs: u8,
    pub fs: u8,
    pub mpp: u8,
    pub vs: u8,
    pub spp: bool,
    pub mpie: bool,
    pub spie: bool,
    pub mie: bool,
    pub sie: bool,
}

impl MStatus {
    pub fn to_bits(&self) -> u64 {
        let mut val = 0u64;
        if self.sd { val |= 1 << 63; }
        if self.mbe { val |= 1 << 37; }
        if self.sbe { val |= 1 << 36; }
        val |= (self.sxl as u64 & 0x3) << 34;
        val |= (self.uxl as u64 & 0x3) << 32;
        if self.tsr { val |= 1 << 22; }
        if self.tw { val |= 1 << 21; }
        if self.tvm { val |= 1 << 20; }
        if self.mxr { val |= 1 << 19; }
        if self.sum { val |= 1 << 18; }
        if self.mprv { val |= 1 << 17; }
        val |= (self.xs as u64 & 0x3) << 15;
        val |= (self.fs as u64 & 0x3) << 13;
        val |= (self.mpp as u64 & 0x3) << 11;
        val |= (self.vs as u64 & 0x3) << 9;
        if self.spp { val |= 1 << 8; }
        if self.mpie { val |= 1 << 7; }
        if self.spie { val |= 1 << 5; }
        if self.mie { val |= 1 << 3; }
        if self.sie { val |= 1 << 1; }
        val
    }

    pub fn from_bits(&mut self, val: u64) {
        self.sd = (val >> 63) & 1 != 0;
        self.mbe = (val >> 37) & 1 != 0;
        self.sbe = (val >> 36) & 1 != 0;
        self.sxl = ((val >> 34) & 0x3) as u8;
        self.uxl = ((val >> 32) & 0x3) as u8;
        self.tsr = (val >> 22) & 1 != 0;
        self.tw = (val >> 21) & 1 != 0;
        self.tvm = (val >> 20) & 1 != 0;
        self.mxr = (val >> 19) & 1 != 0;
        self.sum = (val >> 18) & 1 != 0;
        self.mprv = (val >> 17) & 1 != 0;
        self.xs = ((val >> 15) & 0x3) as u8;
        self.fs = ((val >> 13) & 0x3) as u8;
        self.mpp = ((val >> 11) & 0x3) as u8;
        self.vs = ((val >> 9) & 0x3) as u8;
        self.spp = (val >> 8) & 1 != 0;
        self.mpie = (val >> 7) & 1 != 0;
        self.spie = (val >> 5) & 1 != 0;
        self.mie = (val >> 3) & 1 != 0;
        self.sie = (val >> 1) & 1 != 0;
    }
}

impl Default for MStatus {
    fn default() -> Self { Self {
        sd: false, mbe: false, sbe: false,
        sxl: 2, uxl: 2, tsr: false, tw: false, tvm: false,
        mxr: false, sum: false, mprv: false,
        xs: 0, fs: 0, mpp: 0, vs: 0, spp: false,
        mpie: false, spie: false, mie: false, sie: false,
    }}
}

#[derive(Debug, Clone, Copy)]
pub struct Satp {
    pub mode: u8,
    pub asid: u16,
    pub ppn: u64,
}

impl Satp {
    pub fn to_bits(&self) -> u64 {
        ((self.mode as u64 & 0xF) << 60) |
        ((self.asid as u64 & 0xFFFF) << 44) |
        (self.ppn & 0xFFFFFFFFFFF)
    }

    pub fn from_bits(&mut self, val: u64) {
        let mode = ((val >> 60) & 0xF) as u8;
        // WARL: only modes 0 (Bare) and 8 (Sv39) are supported.
        // Unsupported modes read back as 0 so probing software falls back.
        self.mode = if mode == 0 || mode == 8 { mode } else { 0 };
        self.asid = ((val >> 44) & 0xFFFF) as u16;
        self.ppn = val & 0xFFFFFFFFFFF;
    }
}

impl Default for Satp {
    fn default() -> Self { Satp { mode: 0, asid: 0, ppn: 0 } }
}

impl core::fmt::LowerHex for Satp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:x}", self.to_bits())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessType {
    Instruction,
    Load,
    Store,
}
