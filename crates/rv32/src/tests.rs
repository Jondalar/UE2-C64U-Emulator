//! Unit tests for the rvlite CSR and trap model (docs/hw/01-cpu-boot-memory.md H2-H7; docs/hw/00-memory-map.md A1,
//! B7-B10), plus the ignored MIPS timing probe. Instruction semantics at large are covered by
//! `tests/riscv_tests.rs`.

use super::csr::*;
use super::{mul_div, Bus, Cpu, Exit};
use std::time::Instant;

const RAM_LEN: usize = 1 << 16;

/// 64 KiB of RAM at address 0. Accesses that do not fit read 0 and ignore writes (doc 00 H16: no bus faults).
struct Ram(Vec<u8>);

impl Ram {
    fn with_program(words: &[u32]) -> Ram {
        let mut ram = Ram(vec![0; RAM_LEN]);
        for (i, &w) in words.iter().enumerate() {
            ram.write32(4 * i as u32, w);
        }
        ram
    }
}

impl Bus for Ram {
    fn read8(&mut self, addr: u32) -> u8 {
        self.0.get(addr as usize).copied().unwrap_or(0)
    }
    fn write8(&mut self, addr: u32, val: u8) {
        if let Some(b) = self.0.get_mut(addr as usize) {
            *b = val;
        }
    }
    fn read32(&mut self, addr: u32) -> u32 {
        let a = addr as usize;
        self.0.get(a..a + 4).map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap()))
    }
    fn write32(&mut self, addr: u32, val: u32) {
        let a = addr as usize;
        if let Some(w) = self.0.get_mut(a..a + 4) {
            w.copy_from_slice(&val.to_le_bytes());
        }
    }
}

const RA: u32 = 1;
const T0: u32 = 5;
const T1: u32 = 6;
const T2: u32 = 7;
const S0: u32 = 8;
const S1: u32 = 9;
const A0: u32 = 10;
const T3: u32 = 28;
const T4: u32 = 29;

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const MRET: u32 = 0x3020_0073;
const WFI: u32 = 0x1050_0073;
const NOP: u32 = 0x0000_0013;

const CSRRW: u32 = 1;
const CSRRS: u32 = 2;
const CSRRWI: u32 = 5;
const CSRRSI: u32 = 6;
const CSRRCI: u32 = 7;

fn i_type(op: u32, f3: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    ((imm as u32) & 0xfff) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}

fn r_type(f7: u32, f3: u32, rd: u32, rs1: u32, rs2: u32) -> u32 {
    f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | 0x33
}

fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(0x13, 0, rd, rs1, imm)
}

fn lui(rd: u32, imm20: u32) -> u32 {
    imm20 << 12 | rd << 7 | 0x37
}

fn jalr(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(0x67, 0, rd, rs1, imm)
}

fn jal(rd: u32, off: i32) -> u32 {
    let m = off as u32;
    (m >> 20 & 1) << 31 | (m >> 1 & 0x3ff) << 21 | (m >> 11 & 1) << 20 | (m >> 12 & 0xff) << 12 | rd << 7 | 0x6f
}

fn branch(f3: u32, rs1: u32, rs2: u32, off: i32) -> u32 {
    let m = off as u32;
    (m >> 12 & 1) << 31 | (m >> 5 & 0x3f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (m >> 1 & 0xf) << 8
        | (m >> 11 & 1) << 7
        | 0x63
}

fn sw(rs2: u32, rs1: u32, off: i32) -> u32 {
    let m = off as u32;
    (m >> 5 & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (m & 0x1f) << 7 | 0x23
}

/// CSR instruction; `src` is rs1 for the register forms and the 5-bit immediate for the `i` forms.
fn csr_op(f3: u32, rd: u32, csr: u16, src: u32) -> u32 {
    i_type(0x73, f3, rd, src, csr as i32)
}

/// Step `n` times, requiring plain execution each time.
fn run(cpu: &mut Cpu, ram: &mut Ram, n: usize) {
    for _ in 0..n {
        let pc = cpu.pc;
        assert_eq!(cpu.step(ram), Exit::Stepped, "pc {pc:#x}");
    }
}

#[test]
fn ecall_mret_round_trip() {
    // H5 + H7: portYIELD's ecall, the handler adds 4 to mepc and returns with mret (port_asm.S:229-238,297).
    let mut prog = vec![NOP; 0x44];
    prog[0] = ECALL;
    prog[1] = addi(T1, 0, 7);
    prog[0x40] = csr_op(CSRRS, T0, MEPC, 0);
    prog[0x41] = addi(T0, T0, 4);
    prog[0x42] = csr_op(CSRRW, 0, MEPC, T0);
    prog[0x43] = MRET;
    let mut ram = Ram::with_program(&prog);
    let mut cpu = Cpu::new(0);
    cpu.csr_write(MTVEC, 0x100);
    cpu.csr_write(MSTATUS, MSTATUS_MIE);

    assert_eq!(cpu.step(&mut ram), Exit::Stepped);
    assert_eq!(cpu.pc, 0x100);
    assert_eq!(cpu.csr.mepc, 0, "mepc is the PC of the ecall, not +4");
    assert_eq!(cpu.csr_read(MCAUSE), MCAUSE_ECALL_M);
    assert_eq!(cpu.csr_read(MSTATUS), MSTATUS_MPIE);

    run(&mut cpu, &mut ram, 4);
    assert_eq!(cpu.pc, 4);
    assert_eq!(cpu.csr_read(MSTATUS), MSTATUS_MIE | MSTATUS_MPIE);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.x[T1 as usize], 7);
    assert_eq!(cpu.insns, 6);
}

#[test]
fn interrupt_needs_meip_meie_and_mie() {
    // H6 and doc 01 "Interrupt": taken before fetch only when meip & mie.MEIE & mstatus.MIE.
    let mut prog = vec![NOP; 0x81];
    prog[0] = addi(T0, 0, 1);
    prog[1] = addi(T0, T0, 1);
    let mut ram = Ram::with_program(&prog);

    for (meip, mie, mstatus) in [(false, MIE_MEIE, MSTATUS_MIE), (true, 0, MSTATUS_MIE), (true, MIE_MEIE, 0)] {
        let mut cpu = Cpu::new(0);
        cpu.meip = meip;
        cpu.csr_write(MIE, mie);
        cpu.csr_write(MSTATUS, mstatus);
        run(&mut cpu, &mut ram, 2);
        assert_eq!(cpu.x[T0 as usize], 2);
    }

    let mut cpu = Cpu::new(0);
    cpu.csr_write(MTVEC, 0x200);
    cpu.csr_write(MIE, MIE_MEIE);
    cpu.csr_write(MSTATUS, MSTATUS_MIE);
    run(&mut cpu, &mut ram, 1);
    cpu.meip = true;
    assert_eq!(cpu.step(&mut ram), Exit::Interrupt);
    assert_eq!(cpu.csr.mepc, 4, "mepc is the PC of the instruction not yet executed");
    assert_eq!(cpu.csr_read(MCAUSE), MCAUSE_MEI);
    assert_eq!(cpu.csr_read(MSTATUS), MSTATUS_MPIE);
    assert_eq!((cpu.pc, cpu.insns, cpu.x[T0 as usize]), (0x200, 1, 1));
    // The line is still up but MIE is now 0: the handler's first instruction runs.
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.pc, 0x204);
}

#[test]
fn mtvec_is_direct_mode_only() {
    // H3/B8: riscv_main.c:171 stores the handler's first instruction word; H4/B7 then assert (mtvec & 3) == 0.
    let mut ram = Ram::with_program(&[
        lui(T0, 0xF8810),
        addi(T0, T0, 0x113),
        csr_op(CSRRW, 0, MTVEC, T0),
        csr_op(CSRRS, T1, MTVEC, 0),
        EBREAK,
    ]);
    let mut cpu = Cpu::new(0);
    run(&mut cpu, &mut ram, 4);
    assert_eq!(cpu.x[T1 as usize], 0xF881_0110);
    assert_eq!(cpu.step(&mut ram), Exit::Ebreak);
    assert_eq!(cpu.pc, 0xF881_0110);
}

#[test]
fn mip_is_the_read_only_meip_pin() {
    // A1/H2: crt0.S:81 writes mip = 0, which must neither trap nor stick (csr.vhd:96).
    let mut ram = Ram::with_program(&[
        addi(T2, 0, -1),
        csr_op(CSRRW, 0, MIP, T2),
        csr_op(CSRRS, T0, MIP, 0),
        csr_op(CSRRS, T1, MIP, 0),
    ]);
    let mut cpu = Cpu::new(0);
    cpu.meip = true;
    run(&mut cpu, &mut ram, 3);
    cpu.meip = false;
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.x[T0 as usize], cpu.x[T1 as usize]), (MIP_MEIP, 0));
    cpu.meip = true;
    cpu.csr_write(MIP, 0);
    assert_eq!(cpu.csr_read(MIP), MIP_MEIP);
}

#[test]
fn unknown_csrs_read_zero_and_ignore_writes() {
    // A1/H2: crt0.S:83-89 writes mcountinhibit, mcounteren, mcycle(h), minstret(h); none may trap.
    for addr in [0x320, 0x306, 0xB00, 0xB80, 0xB02, 0xB82, 0x7C0, 0xF14, MTVAL] {
        let mut ram = Ram::with_program(&[addi(T0, 0, -1), csr_op(CSRRW, T1, addr, T0), csr_op(CSRRS, T2, addr, 0)]);
        let mut cpu = Cpu::new(0);
        run(&mut cpu, &mut ram, 3);
        assert_eq!((cpu.x[T1 as usize], cpu.x[T2 as usize]), (0, 0), "csr {addr:#x}");
    }
}

#[test]
fn read_only_and_trap_only_csrs() {
    // csr.vhd:35-38 ID values; mcause is written only by traps (csr.vhd:101-122); mstatus keeps MIE/MPIE only.
    let mut cpu = Cpu::new(0);
    for addr in [MISA, MARCHID, MIMPID, MCAUSE] {
        cpu.csr_write(addr, 0x5555_5555);
    }
    cpu.csr_write(MSTATUS, u32::MAX);
    cpu.csr_write(MIE, u32::MAX);
    assert_eq!(cpu.csr_read(MISA), MISA_VALUE);
    assert_eq!(cpu.csr_read(MARCHID), MARCHID_VALUE);
    assert_eq!(cpu.csr_read(MIMPID), MIMPID_VALUE);
    assert_eq!(cpu.csr_read(MCAUSE), 0);
    assert_eq!(cpu.csr_read(MSTATUS), MSTATUS_MIE | MSTATUS_MPIE);
    assert_eq!(cpu.csr_read(MIE), MIE_MEIE);
}

#[test]
fn x0_writes_are_ignored() {
    let mut ram = Ram::with_program(&[
        addi(T0, 0, 3),
        addi(0, 0, 5),
        lui(0, 0x12345),
        r_type(0, 0, 0, T0, T0),
        csr_op(CSRRS, 0, MISA, 0),
        jalr(0, 0, 24),
        NOP,
    ]);
    let mut cpu = Cpu::new(0);
    for _ in 0..7 {
        assert_eq!(cpu.step(&mut ram), Exit::Stepped);
        assert_eq!(cpu.x[0], 0, "pc {:#x}", cpu.pc);
    }
    assert_eq!(cpu.pc, 28);
}

#[test]
fn csrsi_csrci_toggle_mstatus_mie() {
    // crt0.S:53 `csrrci mstatus,8`; portmacro.h:110-111 `csrs/csrc mstatus,8`.
    let mut ram = Ram::with_program(&[
        csr_op(CSRRSI, T0, MSTATUS, 8),
        csr_op(CSRRCI, T1, MSTATUS, 8),
        csr_op(CSRRCI, T2, MSTATUS, 8),
    ]);
    let mut cpu = Cpu::new(0);
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.x[T0 as usize], cpu.csr_read(MSTATUS)), (0, MSTATUS_MIE));
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.x[T1 as usize], cpu.csr_read(MSTATUS)), (MSTATUS_MIE, 0));
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.x[T2 as usize], cpu.csr_read(MSTATUS)), (0, 0));
}

#[test]
fn csr_immediate_is_sign_extended_from_bit_4() {
    // execute.vhd:98; doc 01 "SYSTEM" quirk.
    let mut ram = Ram::with_program(&[csr_op(CSRRWI, 0, MSCRATCH, 0x10), csr_op(CSRRWI, T0, MSCRATCH, 0x0f)]);
    let mut cpu = Cpu::new(0);
    run(&mut cpu, &mut ram, 2);
    assert_eq!((cpu.x[T0 as usize], cpu.csr_read(MSCRATCH)), (0xFFFF_FFF0, 0x0f));
}

#[test]
fn ebreak_illegal_and_wfi() {
    // Doc 01 "Trap semantics": causes 3 and 2 with mepc = PC; compressed words and the A/F opcodes are illegal.
    for (word, exit, cause) in [
        (EBREAK, Exit::Ebreak, MCAUSE_BREAKPOINT),
        (0x0000_0000, Exit::Illegal(0), MCAUSE_ILLEGAL),
        (0x0000_4501, Exit::Illegal(0x4501), MCAUSE_ILLEGAL),
        (0x0000_002F, Exit::Illegal(0x2F), MCAUSE_ILLEGAL),
        (0x0000_0007, Exit::Illegal(0x07), MCAUSE_ILLEGAL),
        (u32::MAX, Exit::Illegal(u32::MAX), MCAUSE_ILLEGAL),
    ] {
        let mut ram = Ram::with_program(&[NOP, word]);
        let mut cpu = Cpu::new(0);
        cpu.csr_write(MTVEC, 0x40);
        cpu.csr_write(MSTATUS, MSTATUS_MIE);
        run(&mut cpu, &mut ram, 1);
        assert_eq!(cpu.step(&mut ram), exit);
        assert_eq!((cpu.pc, cpu.csr.mepc, cpu.csr_read(MCAUSE)), (0x40, 4, cause), "{word:#010x}");
        assert_eq!(cpu.csr_read(MSTATUS), MSTATUS_MPIE);
        assert_eq!(cpu.insns, 2);
    }

    // WFI and the other funct3=000/100 SYSTEM words are NOPs (decode_comb.vhd:207-211,252-253).
    let mut ram = Ram::with_program(&[WFI, 0x1020_0073, 0x0000_4073]);
    let mut cpu = Cpu::new(0);
    assert_eq!(cpu.step(&mut ram), Exit::Wfi);
    run(&mut cpu, &mut ram, 2);
    assert_eq!((cpu.pc, cpu.csr.mcause), (12, 0));
}

#[test]
fn jump_targets_clear_bits_1_0() {
    // fetch.vhd:58: no misaligned-fetch trap, the target's low bits are dropped. mepc itself keeps them.
    let mut ram = Ram::with_program(&[addi(T0, 0, 0x13), jalr(RA, T0, 0), NOP, NOP, MRET]);
    let mut cpu = Cpu::new(0);
    run(&mut cpu, &mut ram, 2);
    assert_eq!((cpu.pc, cpu.x[RA as usize]), (0x10, 8));
    cpu.csr_write(MEPC, 0x0B);
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.pc, cpu.csr_read(MEPC)), (0x08, 0x0B));
}

#[test]
fn undefined_encodings_follow_rvlite_decode() {
    let mut ram = Ram::with_program(&[
        addi(T0, 0, 1),
        // SLLI with funct7 bit 5 set: rvlite ignores funct7 here (decode_comb.vhd:119-121).
        i_type(0x13, 1, T1, T0, 0x404),
        // Branch funct3 010 is never taken, 011 always (alu_branch.vhd:119-120).
        branch(2, 0, 0, 16),
        branch(3, T0, 0, 8),
        NOP,
        // LOAD funct3 011 loads a word (core_pkg.vhd:245-246): t2 = the branch word above.
        i_type(0x03, 3, T2, 0, 12),
    ]);
    let mut cpu = Cpu::new(0);
    run(&mut cpu, &mut ram, 5);
    assert_eq!(cpu.x[T1 as usize], 16);
    assert_eq!(cpu.pc, 24);
    assert_eq!(cpu.x[T2 as usize], branch(3, T0, 0, 8));
}

#[test]
fn m_extension_results() {
    let min = 0x8000_0000;
    for (f3, a, b, want) in [
        (0, 0x8000_0000, 2, 0),
        (1, min, min, 0x4000_0000),
        (1, u32::MAX, u32::MAX, 0),
        (2, u32::MAX, u32::MAX, u32::MAX),
        (3, u32::MAX, u32::MAX, 0xFFFF_FFFE),
        (4, 7, 0, u32::MAX),
        (4, min, u32::MAX, min),
        (4, -7i32 as u32, 2, -3i32 as u32),
        (5, 7, 0, u32::MAX),
        (5, 7, 2, 3),
        (6, 7, 0, 7),
        (6, min, u32::MAX, 0),
        (6, -7i32 as u32, 2, u32::MAX),
        (7, 7, 0, 7),
        (7, 7, 2, 1),
    ] {
        assert_eq!(mul_div(f3, a, b), want, "funct3 {f3} a {a:#x} b {b:#x}");
    }
}

/// Reports interpreter speed. Target: >= 150 MIPS in a release build on Apple Silicon (spec S01).
#[test]
#[ignore = "timing probe: cargo test --release -p rv32 -- --ignored --nocapture mips"]
fn mips() {
    // Checksum loop over a 256-word buffer at 0x1000: load, add, shift, xor, mul, store, call/return, branch.
    let prog = [
        lui(S0, 1),
        addi(S1, 0, 256),
        addi(A0, 0, 1),
        addi(T0, S0, 0), // 0x0c outer
        addi(T1, S1, 0),
        i_type(0x03, 2, T2, T0, 0), // 0x14 inner: lw t2, 0(t0)
        r_type(0, 0, T2, T2, A0),
        i_type(0x13, 1, T3, T2, 5),
        r_type(0, 4, T2, T2, T3),
        r_type(1, 0, T3, T2, S1),
        sw(T2, T0, 0),
        jal(RA, 20),
        addi(T0, T0, 4),
        addi(T1, T1, -1),
        branch(1, T1, 0, -36),
        jal(0, -48),
        i_type(0x13, 5, T4, T2, 3), // 0x40 fn: srli t4, t2, 3
        r_type(0, 0, A0, A0, T4),
        jalr(0, RA, 0),
    ];
    const INSNS: u64 = 200_000_000;
    let mut ram = Ram::with_program(&prog);
    let mut cpu = Cpu::new(0);
    let mut other = 0u64;
    let start = Instant::now();
    for _ in 0..INSNS {
        other += (cpu.step(&mut ram) != Exit::Stepped) as u64;
    }
    let secs = start.elapsed().as_secs_f64();
    assert_eq!((other, cpu.insns), (0, INSNS));
    assert!(cpu.pc < 0x4c && cpu.x[A0 as usize] != 0);
    let mips = INSNS as f64 / secs / 1e6;
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!("rv32 interpreter: {mips:.1} MIPS ({INSNS} instructions in {secs:.2} s, {profile} build)");
}
