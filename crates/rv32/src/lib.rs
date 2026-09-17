//! RV32IM interpreter with rvlite semantics. Spec: docs/specs/S01-cpu-rv32.md
//!
//! The U64-II CPU is Gideon's rvlite core (docs/hw/01-cpu-boot-memory.md "Which CPU core"): RV32I, Zicsr and the
//! multiply half of M, M-mode only, direct-mode traps, one level-sensitive external interrupt. VHDL citations are
//! relative to `fpga/cpu_unit/rvlite/vhdl_source/` in the firmware tree.
//!
//! Where this model knowingly differs from the RTL (none of it is observable by the firmware):
//! - DIV/DIVU/REM/REMU follow the M extension. rvlite decodes them as MUL/MULH/MULHSU/MULHU
//!   (decode_comb.vhd:149-155; multiply.vhd:39-50); the firmware is built with `-mno-div` (doc 01).
//! - Interrupts are sampled before every fetch, without rvlite's `inhibit_irq` delay while a CSR instruction is in
//!   execute (execute.vhd:101; csr.vhd:93). Doc 01 "Trap semantics" states the two models are equivalent.
//! - 16/32-bit loads and stores are little-endian byte sequences at any address ([`Bus`]). rvlite loads 0 from, and
//!   does not store to, a 16-bit access at an odd address (core_pkg.vhd:235-243,273-277).

/// Memory bus seen by the CPU. Multi-byte accesses default to little-endian byte sequences.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, val: u8);

    fn read16(&mut self, addr: u32) -> u16 {
        self.read8(addr) as u16 | (self.read8(addr.wrapping_add(1)) as u16) << 8
    }
    fn read32(&mut self, addr: u32) -> u32 {
        self.read16(addr) as u32 | (self.read16(addr.wrapping_add(2)) as u32) << 16
    }
    fn write16(&mut self, addr: u32, val: u16) {
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }
    fn write32(&mut self, addr: u32, val: u32) {
        self.write16(addr, val as u16);
        self.write16(addr.wrapping_add(2), (val >> 16) as u16);
    }
    /// Instruction fetch.
    fn fetch(&mut self, addr: u32) -> u32 {
        self.read32(addr)
    }
}

/// What a single [`Cpu::step`] did, beyond plain execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// One instruction executed (ECALL/MRET included; their trap is taken internally).
    Stepped,
    /// A pending interrupt was taken instead of executing an instruction.
    Interrupt,
    /// WFI executed (NOP on rvlite).
    Wfi,
    /// EBREAK executed; breakpoint trap taken.
    Ebreak,
    /// Illegal instruction word; illegal-instruction trap taken.
    Illegal(u32),
}

pub mod csr {
    pub const MSTATUS: u16 = 0x300;
    pub const MISA: u16 = 0x301;
    pub const MIE: u16 = 0x304;
    pub const MTVEC: u16 = 0x305;
    pub const MSCRATCH: u16 = 0x340;
    pub const MEPC: u16 = 0x341;
    pub const MCAUSE: u16 = 0x342;
    pub const MTVAL: u16 = 0x343;
    pub const MIP: u16 = 0x344;
    pub const MARCHID: u16 = 0xF12;
    pub const MIMPID: u16 = 0xF13;

    pub const MSTATUS_MIE: u32 = 1 << 3;
    pub const MSTATUS_MPIE: u32 = 1 << 7;
    pub const MIE_MEIE: u32 = 1 << 11;
    pub const MIP_MEIP: u32 = 1 << 11;

    /// Read-only `misa`: RV32I only, the M bit is not set (csr.vhd:38).
    pub const MISA_VALUE: u32 = 0x4000_0100;
    /// Read-only `marchid` (csr.vhd:35).
    pub const MARCHID_VALUE: u32 = 0x13;
    /// Read-only `mimpid`: the csr entity's `g_version` default (csr.vhd:13,36).
    pub const MIMPID_VALUE: u32 = 0x475A_0001;

    pub const MCAUSE_MEI: u32 = 0x8000_000B;
    pub const MCAUSE_ECALL_M: u32 = 11;
    pub const MCAUSE_BREAKPOINT: u32 = 3;
    pub const MCAUSE_ILLEGAL: u32 = 2;
}

/// The only `mstatus` bits rvlite stores; everything else reads 0 (csr.vhd:104-106; doc 01 CSR table).
const MSTATUS_STORED: u32 = csr::MSTATUS_MIE | csr::MSTATUS_MPIE;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Csrs {
    pub mstatus: u32,
    pub mie: u32,
    pub mtvec: u32,
    pub mscratch: u32,
    pub mepc: u32,
    pub mcause: u32,
    /// rvlite has no `mtval` (csr.vhd:32,74): never set by the CPU, and `csr_read(MTVAL)` returns 0.
    pub mtval: u32,
}

#[derive(Clone, Debug)]
pub struct Cpu {
    /// x0..x31; `x[0]` always reads 0.
    pub x: [u32; 32],
    pub pc: u32,
    pub csr: Csrs,
    /// External interrupt line (mip.MEIP), driven by the machine before each step.
    pub meip: bool,
    /// Instructions executed (interrupt entries not counted).
    pub insns: u64,
}

impl Cpu {
    pub fn new(entry: u32) -> Self {
        Cpu { x: [0; 32], pc: entry, csr: Csrs::default(), meip: false, insns: 0 }
    }

    pub fn reset(&mut self, entry: u32) {
        *self = Cpu::new(entry);
    }

    /// Execute one instruction, or take a pending interrupt.
    ///
    /// - Interrupt: level-sensitive, taken before fetch when `meip & mie.MEIE & mstatus.MIE` (csr.vhd:93; doc 01
    ///   H6). No instruction executes and `insns` is not counted.
    /// - Illegal (cause 2): every major opcode rvlite does not decode, which includes all words whose low two bits
    ///   are not `11` (decode_comb.vhd:256-266). There are no access-fault or misaligned-fetch exceptions
    ///   (doc 00 H16): every jump target has bits 1:0 cleared (fetch.vhd:58).
    /// - Undefined encodings inside a decoded major opcode never trap and follow rvlite's datapath:
    ///   - OP decodes only funct7 bit 0 (MUL group) and bit 5 (SUB/SRA) (decode_comb.vhd:146-155).
    ///   - OP-IMM uses funct7 bit 5 only for SRLI/SRAI (decode_comb.vhd:119-121).
    ///   - BRANCH funct3 010 is never taken and 011 always (alu_branch.vhd:119-120).
    ///   - LOAD funct3 011/110/111 load the full word (core_pkg.vhd:245-246).
    ///   - STORE sizes come from funct3 bits 1:0 (decode_comb.vhd:86-92).
    ///   - JALR ignores funct3.
    ///   - SYSTEM funct3 000 words other than ECALL/EBREAK/MRET/WFI, and funct3 100, are NOPs
    ///     (decode_comb.vhd:209-211,252-253).
    #[inline]
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> Exit {
        if self.meip & (self.csr.mie & csr::MIE_MEIE != 0) & (self.csr.mstatus & csr::MSTATUS_MIE != 0) {
            self.trap(csr::MCAUSE_MEI);
            return Exit::Interrupt;
        }

        let pc = self.pc;
        let insn = bus.fetch(pc);
        self.insns += 1;

        let rd = ((insn >> 7) & 31) as usize;
        let f3 = (insn >> 12) & 7;
        let rs1 = self.x[((insn >> 15) & 31) as usize];
        let rs2 = self.x[((insn >> 20) & 31) as usize];
        let imm_i = ((insn as i32) >> 20) as u32;
        let mut next = pc.wrapping_add(4);

        match insn & 0x7f {
            // LOAD
            0x03 => {
                let addr = rs1.wrapping_add(imm_i);
                self.x[rd] = match f3 {
                    0 => bus.read8(addr) as i8 as u32,
                    1 => bus.read16(addr) as i16 as u32,
                    4 => bus.read8(addr) as u32,
                    5 => bus.read16(addr) as u32,
                    _ => bus.read32(addr),
                };
            }
            // MISC-MEM (FENCE.I): NOP (decode_comb.vhd:112-113).
            0x0f => {}
            // OP-IMM
            0x13 => {
                let shamt = (insn >> 20) & 31;
                self.x[rd] = match f3 {
                    0 => rs1.wrapping_add(imm_i),
                    1 => rs1 << shamt,
                    2 => ((rs1 as i32) < (imm_i as i32)) as u32,
                    3 => (rs1 < imm_i) as u32,
                    4 => rs1 ^ imm_i,
                    5 if insn & (1 << 30) != 0 => ((rs1 as i32) >> shamt) as u32,
                    5 => rs1 >> shamt,
                    6 => rs1 | imm_i,
                    _ => rs1 & imm_i,
                };
            }
            // AUIPC
            0x17 => self.x[rd] = pc.wrapping_add(insn & 0xffff_f000),
            // STORE
            0x23 => {
                let imm_s = (((insn as i32) >> 20) as u32 & !0x1f) | ((insn >> 7) & 0x1f);
                let addr = rs1.wrapping_add(imm_s);
                match f3 & 3 {
                    0 => bus.write8(addr, rs2 as u8),
                    1 => bus.write16(addr, rs2 as u16),
                    _ => bus.write32(addr, rs2),
                }
            }
            // OP
            0x33 => {
                self.x[rd] = if insn & (1 << 25) != 0 {
                    mul_div(f3, rs1, rs2)
                } else {
                    match f3 {
                        0 if insn & (1 << 30) != 0 => rs1.wrapping_sub(rs2),
                        0 => rs1.wrapping_add(rs2),
                        1 => rs1 << (rs2 & 31),
                        2 => ((rs1 as i32) < (rs2 as i32)) as u32,
                        3 => (rs1 < rs2) as u32,
                        4 => rs1 ^ rs2,
                        5 if insn & (1 << 30) != 0 => ((rs1 as i32) >> (rs2 & 31)) as u32,
                        5 => rs1 >> (rs2 & 31),
                        6 => rs1 | rs2,
                        _ => rs1 & rs2,
                    }
                };
            }
            // LUI
            0x37 => self.x[rd] = insn & 0xffff_f000,
            // BRANCH
            0x63 => {
                let taken = match f3 {
                    0 => rs1 == rs2,
                    1 => rs1 != rs2,
                    2 => false,
                    3 => true,
                    4 => (rs1 as i32) < (rs2 as i32),
                    5 => (rs1 as i32) >= (rs2 as i32),
                    6 => rs1 < rs2,
                    _ => rs1 >= rs2,
                };
                if taken {
                    let imm_b = (((insn as i32) >> 19) as u32 & !0xfff)
                        | ((insn << 4) & 0x800)
                        | ((insn >> 20) & 0x7e0)
                        | ((insn >> 7) & 0x1e);
                    next = pc.wrapping_add(imm_b) & !3;
                }
            }
            // JALR
            0x67 => {
                self.x[rd] = next;
                next = rs1.wrapping_add(imm_i) & !3;
            }
            // JAL
            0x6f => {
                let imm_j = (((insn as i32) >> 11) as u32 & !0xf_ffff)
                    | (insn & 0xf_f000)
                    | ((insn >> 9) & 0x800)
                    | ((insn >> 20) & 0x7fe);
                self.x[rd] = next;
                next = pc.wrapping_add(imm_j) & !3;
            }
            // SYSTEM
            0x73 => return self.system(insn, rd, rs1),
            _ => {
                self.trap(csr::MCAUSE_ILLEGAL);
                return Exit::Illegal(insn);
            }
        }

        self.x[0] = 0;
        self.pc = next;
        Exit::Stepped
    }

    /// CSR read with rvlite semantics (csr.vhd:52-74; doc 01 CSR table, H2, H4).
    ///
    /// `mstatus` exposes only MIE/MPIE, `mie` only MEIE, `mip` is the live MEIP pin. `mtval`, `mhartid` and every
    /// unimplemented CSR read 0 (doc 00 A1).
    pub fn csr_read(&self, addr: u16) -> u32 {
        match addr {
            csr::MSTATUS => self.csr.mstatus & MSTATUS_STORED,
            csr::MISA => csr::MISA_VALUE,
            csr::MIE => self.csr.mie & csr::MIE_MEIE,
            csr::MTVEC => self.csr.mtvec,
            csr::MSCRATCH => self.csr.mscratch,
            csr::MEPC => self.csr.mepc,
            csr::MCAUSE => self.csr.mcause,
            csr::MIP if self.meip => csr::MIP_MEIP,
            csr::MARCHID => csr::MARCHID_VALUE,
            csr::MIMPID => csr::MIMPID_VALUE,
            _ => 0,
        }
    }

    /// CSR write as a CSR instruction performs it (csr.vhd:101-116); never traps (doc 01 H2).
    ///
    /// - `mtvec` keeps bits 1:0 = 0, so any value is accepted and reads back in direct mode
    ///   (csr.vhd:110; H3: riscv_main.c:171 writes the handler's first instruction word 0xF8810113).
    /// - `mepc` stores all 32 bits (csr.vhd:112); `mret` clears bits 1:0 of the target.
    /// - `mcause` is written only by traps (csr.vhd:117-122), `mip` is the pin, read-only and unimplemented CSRs
    ///   ignore the write.
    pub fn csr_write(&mut self, addr: u16, val: u32) {
        match addr {
            csr::MSTATUS => self.csr.mstatus = val & MSTATUS_STORED,
            csr::MIE => self.csr.mie = val & csr::MIE_MEIE,
            csr::MTVEC => self.csr.mtvec = val & !3,
            csr::MSCRATCH => self.csr.mscratch = val,
            csr::MEPC => self.csr.mepc = val,
            _ => {}
        }
    }

    /// SYSTEM opcode (decode_comb.vhd:192-255). `self.pc` still holds the PC of `insn`.
    #[inline(never)]
    fn system(&mut self, insn: u32, rd: usize, rs1: u32) -> Exit {
        let f3 = (insn >> 12) & 7;
        match (f3, insn >> 20) {
            // ECALL: mepc = PC of the ecall, not +4 (H5; portYIELD, portmacro.h:97).
            (0, 0x000) => {
                self.trap(csr::MCAUSE_ECALL_M);
                return Exit::Stepped;
            }
            (0, 0x001) => {
                self.trap(csr::MCAUSE_BREAKPOINT);
                return Exit::Ebreak;
            }
            (0, 0x302) => {
                self.mret();
                return Exit::Stepped;
            }
            // WFI is a NOP; the idle task never uses it (doc 01 "Idle").
            (0, 0x105) => {
                self.pc = self.pc.wrapping_add(4);
                return Exit::Wfi;
            }
            (0, _) | (4, _) => {}
            (_, addr) => {
                // rvlite sign-extends the 5-bit immediate from bit 4 (execute.vhd:98; doc 01 "SYSTEM"). The
                // firmware only uses immediates below 16 (`csrrci mstatus,8`, crt0.S:53; portmacro.h:110-111).
                let src = if f3 & 4 != 0 { (((insn as i32) << 12) >> 27) as u32 } else { rs1 };
                let old = self.csr_read(addr as u16);
                let new = match f3 & 3 {
                    1 => src,
                    2 => old | src,
                    _ => old & !src,
                };
                self.csr_write(addr as u16, new);
                self.x[rd] = old;
                self.x[0] = 0;
            }
        }
        self.pc = self.pc.wrapping_add(4);
        Exit::Stepped
    }

    /// Trap entry (csr.vhd:117-126): `mepc` = PC of the trapping or interrupted instruction (H5, H6),
    /// MPIE = MIE, MIE = 0, jump to the direct-mode vector.
    #[cold]
    #[inline(never)]
    fn trap(&mut self, cause: u32) {
        let mpie = if self.csr.mstatus & csr::MSTATUS_MIE != 0 { csr::MSTATUS_MPIE } else { 0 };
        self.csr.mstatus = (self.csr.mstatus & !MSTATUS_STORED) | mpie;
        self.csr.mepc = self.pc;
        self.csr.mcause = cause;
        self.pc = self.csr.mtvec & !3;
    }

    /// `mret` (csr.vhd:127-128; H7): MIE = MPIE, MPIE = 1, stay in M-mode whatever the stacked MPP says.
    fn mret(&mut self) {
        let mie = if self.csr.mstatus & csr::MSTATUS_MPIE != 0 { csr::MSTATUS_MIE } else { 0 };
        self.csr.mstatus = (self.csr.mstatus & !MSTATUS_STORED) | csr::MSTATUS_MPIE | mie;
        self.pc = self.csr.mepc & !3;
    }
}

/// OP with funct7 bit 0: the M extension. MUL/MULH/MULHSU/MULHU match multiply.vhd:39-50; DIV/DIVU/REM/REMU use the
/// M-spec results for division by zero and signed overflow.
#[inline(always)]
fn mul_div(f3: u32, a: u32, b: u32) -> u32 {
    match f3 {
        0 => a.wrapping_mul(b),
        1 => ((a as i32 as i64 * b as i32 as i64) >> 32) as u32,
        2 => ((a as i32 as i64 * b as i64) >> 32) as u32,
        3 => ((a as u64 * b as u64) >> 32) as u32,
        4 if b == 0 => u32::MAX,
        4 => (a as i32).wrapping_div(b as i32) as u32,
        5 if b == 0 => u32::MAX,
        5 => a / b,
        6 if b == 0 => a,
        6 => (a as i32).wrapping_rem(b as i32) as u32,
        _ if b == 0 => a,
        _ => a % b,
    }
}

#[cfg(test)]
mod tests;
