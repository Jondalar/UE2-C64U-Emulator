//! S40 §5.1-§5.2: the board's flip-flop images of the 6502N (chip 1 words 8-14, 209 bits) and of each reDIP-CIA
//! (chips 3, 4 words 8-19, 363 bits), encoded from TRX64's state (docs/specs/S40-trx-engine.md). The images are the
//! words of the chip block the app reads back and stages for a load; UE2 loads its own blocks from the side table
//! (engine_tap.rs), so only the encoder is here. Decoding images UE2 did not make is M2/M3 (S40 §0).
//!
//! Layouts are the RTL's: `cpu6510.v:163-164` (`sb_cur`), `proc_registers.vhd:347-368, :472`,
//! `proc_control.vhd:552-587`, `proc_interrupt.vhd:60`; `cia_core.sv:140-352`, `c64_redip_cia.v:108-115` and the
//! `reDIP-CIA/gateware/CIA/cia_*.sv` blocks. Bit 0 is bit 0 of the first word.

use trx64_core::c64re_snapshot::CiaSnapshot;

/// Bits `lo..lo+n` of `words` (little endian across words) set to `v`.
pub(crate) fn put(words: &mut [u32], lo: usize, n: usize, v: u64) {
    for b in 0..n {
        let (w, bit) = ((lo + b) / 32, (lo + b) % 32);
        if v >> b & 1 != 0 {
            words[w] |= 1 << bit;
        } else {
            words[w] &= !(1 << bit);
        }
    }
}

/// Bits `lo..lo+n` of `words`.
#[cfg(test)]
pub(crate) fn get(words: &[u32], lo: usize, n: usize) -> u64 {
    (0..n).fold(0, |v, b| v | u64::from(words[(lo + b) / 32] >> ((lo + b) % 32) & 1) << b)
}

/// The 6502N's flip-flops (209 bits).
pub(crate) mod cpu {
    pub const A: usize = 0;
    pub const X: usize = 8;
    pub const Y: usize = 16;
    pub const S: usize = 24;
    pub const P: usize = 32;
    pub const PC: usize = 40;
    pub const ADL: usize = 56;
    pub const ADH: usize = 64;
    #[allow(dead_code)]
    pub const DREG: usize = 72;
    pub const IR: usize = 80;
    pub const H: usize = 88;
    /// ready_d1, adh_clash, so_d, pc_carry_i, pc_carry_d, index_carry.
    pub const MISC: usize = 96;
    /// proc_control's 22 bits: state 4:0, a_mux 6:5, dout_mux 9:7, rwn 10, latch_dreg 11, copy_d2p 12, dummy 13,
    /// irq_inhibit 14, flags_update 15, trigger_upd 16, vectoring 17, intg 18, a16 19, vect_reg 21:20.
    pub const CTL: usize = 102;
    pub const CTL_VECTORING: usize = CTL + 17;
    pub const CTL_INTG: usize = CTL + 18;
    /// irq_c, nmi_c, nmi_d1, nmi_d2, nmi_act, resetting.
    pub const INT: usize = 124;
    pub const DIN: usize = 130;
    pub const PIN: usize = 138;
    pub const PDIR: usize = 144;
    pub const POUT: usize = 152;
    #[allow(dead_code)]
    pub const PFLOAT: usize = 160;
    #[allow(dead_code)]
    pub const FALL6: usize = 168;
    #[allow(dead_code)]
    pub const FALL7: usize = 187;
    pub const RDY: usize = 206;
    pub const IRQ: usize = 207;
    pub const NMI: usize = 208;
    /// The control state after an opcode fetch with nothing left to write back: the board's `$8D` block
    /// (`ctl` = `11 1 0 0 1 0 0 0 0 1 1 000 11 00000`, fixture board_2026-10-04) with `trigger_upd` cleared (S40 answer
    /// 2): vect_reg %11, a16, latch_dreg, rwn, a_mux 3, state fetch.
    pub const CTL_FETCHED: u64 = 0x38_0C60;
    /// ready_d1, so_d and pc_carry_d as in the board's blocks.
    pub const MISC_IDLE: u64 = 0x15;
}

/// What the encoder takes of the CPU at a boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CpuState {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub s: u8,
    pub p: u8,
    pub pc: u16,
    /// The previous instruction: its opcode and operand bytes.
    pub ir: u8,
    pub op1: u8,
    pub op2: u8,
    /// The opcode at PC as the bus returns it.
    pub din: u8,
    pub pdir: u8,
    pub pout: u8,
    /// An interrupt is decided at this fetch (F1 b4).
    pub intg: bool,
    /// The IRQ and NMI lines asserted; an NMI edge not yet taken.
    pub irq: bool,
    pub nmi: bool,
    pub nmi_edge: bool,
}

/// The 6502N image at a snap (S40 §5.1): settled registers and nothing pending.
pub(crate) fn encode_cpu(c: &CpuState) -> [u32; 7] {
    use cpu::*;
    let mut w = [0u32; 7];
    for (lo, n, v) in [
        (A, 8, u64::from(c.a)),
        (X, 8, u64::from(c.x)),
        (Y, 8, u64::from(c.y)),
        (S, 8, u64::from(c.s)),
        (P, 8, u64::from(c.p)),
        (PC, 16, u64::from(c.pc)),
        (ADL, 8, u64::from(c.op1)),
        (ADH, 8, u64::from(c.op2)),
        (IR, 8, u64::from(c.ir)),
        (H, 8, 0xFF),
        (MISC, 6, MISC_IDLE),
        (CTL, 22, CTL_FETCHED),
        (DIN, 8, u64::from(c.din)),
        // Cassette sense and the three pulled-up lines high (proc pin_r as in the board's blocks).
        (PIN, 6, 0x3F),
        (PDIR, 8, u64::from(c.pdir)),
        (POUT, 8, u64::from(c.pout)),
        (RDY, 1, 1),
        (IRQ, 1, u64::from(c.irq)),
        (NMI, 1, u64::from(c.nmi)),
    ] {
        put(&mut w, lo, n, v);
    }
    put(&mut w, CTL_INTG, 1, u64::from(c.intg));
    put(&mut w, CTL_VECTORING, 1, u64::from(c.intg));
    // proc_interrupt: irq_c is ~IRQP (1 = no IRQ), nmi_c/nmi_d1 the line, nmi_d2 the line before, nmi_act an edge.
    let int = u64::from(!c.irq) | u64::from(c.nmi) << 1 | u64::from(c.nmi) << 2 | u64::from(c.nmi && !c.nmi_edge) << 3
        | u64::from(c.nmi_edge) << 4;
    put(&mut w, INT, 6, int);
    w
}

/// Each reDIP-CIA's flip-flops (363 bits).
pub(crate) mod cia {
    pub const DDRB: usize = 8;
    pub const DDRA: usize = 16;
    pub const PRB: usize = 24;
    pub const PRA: usize = 32;
    /// Timer A, timer B: prescaler (the latch) 15:0, counter 31:16, then hi_w_prev, start_prev, reload_prev,
    /// count_prev, intr, intr_up, toggle_prev, os_loaded_prev (cia_timer.sv:131).
    pub const TA: usize = 43;
    pub const TB: usize = 83;
    pub const T_COUNTER: usize = 16;
    pub const T_START_PREV: usize = 33;
    pub const T_COUNT_PREV: usize = 35;
    pub const T_OS_LOADED_PREV: usize = 39;
    /// TOD (cia_tod.sv:284): alarm 31:0, clock 63:32 as `{10ths, sec, min, hr}` with hr in the low byte, h12 64,
    /// we_10ths_prev 65, rd_10ths_prev 66, tod_start_state 67, tod_sample_state 68, clatch 100:69.
    pub const TOD: usize = 123;
    pub const ALARM: usize = TOD;
    pub const CLOCK: usize = TOD + 32;
    pub const TOD_START: usize = TOD + 67;
    pub const TOD_SAMPLE: usize = TOD + 68;
    pub const CLATCH: usize = TOD + 69;
    /// Serial: SDR 7:0 (cia_serial.sv:206).
    pub const SDR: usize = 244;
    /// Interrupt (cia_interrupt.sv:165): sources_prev 4:0, ir_set_prev 5, icr_prev 13:6, icr7_prev 14, irq_prev 15,
    /// flags_prev 20:16, ir_clr_prev 21, ir_set_phi2 22, ir_clr 23, mask 28:24, w_mask_prev 29.
    pub const ICR: usize = 287;
    pub const ICR7_PREV: usize = ICR + 14;
    pub const IRQ_PREV: usize = ICR + 15;
    pub const FLAGS_PREV: usize = ICR + 16;
    pub const MASK: usize = ICR + 24;
    /// Control A, B: the register 7:0, cr_w_prev 8, cnt_prev 9 (cia_control.sv:121).
    pub const CRA: usize = 317;
    pub const CRB: usize = 327;
}

/// A TOD register file `{10ths, sec, min, hr}` (VICE's order from `$xC08`) as cia_tod.sv's 32-bit `tod_t`.
fn tod_word(r: [u8; 4]) -> u64 {
    u64::from(r[3]) | u64::from(r[2]) << 8 | u64::from(r[1]) << 16 | u64::from(r[0]) << 24
}

/// The reDIP-CIA image from TRX64's CIA (S40 §5.2): `s` its whole context, `read` its registers 0-15 as a read
/// returns them (the counters and the TOD as they stand).
pub(crate) fn encode_cia(s: &CiaSnapshot, read: &[u8; 16]) -> [u32; 12] {
    use cia::*;
    let reg = |i: usize| s.c_cia.get(i).copied().unwrap_or(0) as u8;
    let mut w = [0u32; 12];
    for (lo, v) in [(PRA, reg(0)), (PRB, reg(1)), (DDRA, reg(2)), (DDRB, reg(3))] {
        put(&mut w, lo, 8, u64::from(v));
    }
    // The timers: latch, the counter as read, and the steady-state pipeline (S40 answer 3): a running phi2-counted
    // timer has start_prev and count_prev, a stopped one neither; os_loaded_prev is 1 on a 6526.
    let (cra, crb) = (reg(0x0E), reg(0x0F));
    for (base, latch, cnt, cr, phi2) in [
        (TA, s.ta_latch, u16::from_le_bytes([read[4], read[5]]), cra, cra & 0x20 == 0),
        (TB, s.tb_latch, u16::from_le_bytes([read[6], read[7]]), crb, crb & 0x60 == 0),
    ] {
        put(&mut w, base, 16, latch as u64 & 0xFFFF);
        put(&mut w, base + T_COUNTER, 16, u64::from(cnt));
        let running = cr & 1 != 0;
        put(&mut w, base + T_START_PREV, 1, u64::from(running));
        put(&mut w, base + T_COUNT_PREV, 1, u64::from(running && phi2));
        put(&mut w, base + T_OS_LOADED_PREV, 1, 1);
    }
    let arr = |v: &Vec<i64>| -> [u8; 4] { std::array::from_fn(|k| v.get(k).copied().unwrap_or(0) as u8) };
    put(&mut w, ALARM, 32, tod_word(arr(&s.todalarm)));
    put(&mut w, CLOCK, 32, tod_word([reg(8), reg(9), reg(10), reg(11)]));
    put(&mut w, CLATCH, 32, tod_word(if s.todlatched != 0 { arr(&s.todlatch) } else { [reg(8), reg(9), reg(10), reg(11)] }));
    put(&mut w, TOD_START, 1, u64::from(s.todstopped == 0));
    put(&mut w, TOD_SAMPLE, 1, u64::from(s.todlatched == 0));
    put(&mut w, SDR, 8, u64::from(reg(0x0C)));
    let ir = s.irqflags & 0x80 != 0;
    put(&mut w, ICR7_PREV, 1, u64::from(ir));
    put(&mut w, IRQ_PREV, 1, u64::from(ir));
    put(&mut w, FLAGS_PREV, 5, s.irqflags as u64 & 0x1F);
    put(&mut w, MASK, 5, u64::from(reg(0x0D)) & 0x1F);
    put(&mut w, CRA, 8, u64::from(cra));
    put(&mut w, CRB, 8, u64::from(crb));
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board's own blocks (fixture `board_2026-10-04.c64rering`): its chip 1 and chip 3 hidden words against the
    /// nodes the board's app wrote from them. Checks the bit positions the encoder and the app use.
    #[test]
    fn the_fixture_has_the_layout() {
        let Some(entries) = crate::board::tests::fixture_blocks() else { return };
        assert_eq!(entries.len(), 9);
        for (w, payload) in entries {
            let h = &w[27..34];
            let cpuj = &payload["cpu"];
            let n = |k: &str| cpuj[k].as_i64().unwrap() as u64;
            assert_eq!(get(h, cpu::PC, 16), n("pc"));
            assert_eq!([get(h, cpu::A, 8), get(h, cpu::X, 8), get(h, cpu::Y, 8), get(h, cpu::S, 8)], [n("a"), n("x"), n("y"), n("sp")]);
            assert_eq!(get(h, cpu::P, 8), n("flags"));
            assert_eq!(get(h, cpu::CTL, 22) & !(1 << 16), cpu::CTL_FETCHED, "the after-fetch control state");
            assert_eq!(get(h, cpu::RDY, 1), 1);
            assert_eq!((get(h, cpu::PDIR, 8), get(h, cpu::POUT, 8)), (0x2F, 0x37));
            for (ci, base, key) in [(0, 34, "cia1"), (1, 46, "cia2")] {
                let c = &w[base..base + 12];
                let node = &payload[key];
                let regs: Vec<u64> = node["c_cia"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
                assert_eq!([get(c, cia::PRA, 8), get(c, cia::PRB, 8), get(c, cia::DDRA, 8), get(c, cia::DDRB, 8)], [regs[0], regs[1], regs[2], regs[3]], "{key} ports");
                assert_eq!(get(c, cia::TA, 16), node["ta_latch"].as_u64().unwrap(), "{key} timer A latch");
                assert_eq!(get(c, cia::TB, 16), node["tb_latch"].as_u64().unwrap(), "{key} timer B latch");
                // The RTL's `cr` keeps the LOAD strobe (bit 4) as written; the app's node and TRX64 do not.
                assert_eq!([get(c, cia::CRA, 8) & !0x10, get(c, cia::CRB, 8) & !0x10], [regs[14], regs[15]], "{key} CRA/CRB");
                let alarm: Vec<u64> = node["todalarm"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
                assert_eq!(get(c, cia::ALARM, 8), alarm[3], "{key} alarm hours in the low byte (replay_cia)");
                let _ = ci;
            }
        }
    }

    /// UE2's own image of a known state reads back where the app and the board read it.
    #[test]
    fn the_encoder_puts_each_field_in_place() {
        let c = CpuState {
            a: 0x11,
            x: 0x22,
            y: 0x33,
            s: 0xF3,
            p: 0x24,
            pc: 0xE5CF,
            ir: 0xA5,
            op1: 0xC6,
            op2: 0,
            din: 0x85,
            pdir: 0x2F,
            pout: 0x37,
            intg: true,
            irq: true,
            nmi: false,
            nmi_edge: false,
        };
        let w = encode_cpu(&c);
        assert_eq!(w[0], 0xF333_2211);
        assert_eq!(get(&w, cpu::PC, 16), 0xE5CF);
        assert_eq!((get(&w, cpu::IR, 8), get(&w, cpu::DIN, 8)), (0xA5, 0x85));
        assert_eq!(get(&w, cpu::CTL, 22), cpu::CTL_FETCHED | 3 << 17, "intg and vectoring, nothing to write back");
        assert_eq!(get(&w, cpu::INT, 6), 0, "the IRQ line low: irq_c 0");
        assert_eq!((get(&w, cpu::IRQ, 1), get(&w, cpu::RDY, 1)), (1, 1));
        let mut s: CiaSnapshot = serde_json::from_str(
            r#"{"v": 3, "c_cia": [127, 255, 255, 0, 37, 64, 255, 255, 5, 48, 18, 129, 90, 129, 17, 8],
            "irqflags": 129, "ack_irqflags": 0, "new_irqflags": 0, "irq_enabled": 1, "rdi": 0, "ifr_clock": 0,
            "ifr_delay": 0, "tat": 0, "tbt": 0, "ta_state": 0, "ta_latch": 16421, "ta_cnt": 0, "ta_clk": 0,
            "tb_state": 0, "tb_latch": 65535, "tb_cnt": 0, "tb_clk": 0, "sr_bits": 0, "sdr_valid": 0,
            "sdr_force_finish": 0, "shifter": 0, "sdr_delay": 0, "sp_in_state": 0, "cnt_in_state": 0,
            "cnt_out_state": 0, "todalarm": [1, 2, 3, 132], "todlatch": [0, 0, 0, 0], "todlatched": 0,
            "todstopped": 0, "todticks": 0, "todclk": 0, "todtickcounter": 0, "power_tickcounter": 0,
            "power_ticks": 0, "old_pa": 0, "old_pb": 0, "read_clk": 0, "read_offset": 0, "last_read": 0, "model": 0}"#,
        )
        .unwrap();
        let mut read = [0u8; 16];
        read[4] = 0x34;
        read[5] = 0x12;
        let w = encode_cia(&s, &read);
        assert_eq!([get(&w, cia::DDRB, 8), get(&w, cia::DDRA, 8), get(&w, cia::PRB, 8), get(&w, cia::PRA, 8)], [0, 0xFF, 0xFF, 0x7F]);
        assert_eq!((get(&w, cia::TA, 16), get(&w, cia::TA + cia::T_COUNTER, 16)), (0x4025, 0x1234));
        assert_eq!((get(&w, cia::TA + cia::T_START_PREV, 1), get(&w, cia::TA + cia::T_COUNT_PREV, 1)), (1, 1), "running");
        assert_eq!(get(&w, cia::TB + cia::T_START_PREV, 1), 0, "timer B stopped");
        assert_eq!((get(&w, cia::MASK, 5), get(&w, cia::FLAGS_PREV, 5), get(&w, cia::ICR7_PREV, 1)), (1, 1, 1));
        // replay_cia's bytes: alarm hours/min/sec/10ths at 123/131/139/147.
        assert_eq!([123, 131, 139, 147].map(|b| get(&w, b, 8)), [0x84, 3, 2, 1]);
        assert_eq!(get(&w, cia::CLOCK, 32), 0x0530_1281, "the clock: 10ths in the high byte, hours low");
        assert_eq!((get(&w, cia::SDR, 8), get(&w, cia::CRA, 8), get(&w, cia::CRB, 8)), (0x5A, 0x11, 0x08));
        s.todlatched = 1;
        s.todlatch = vec![9, 8, 7, 6];
        assert_eq!(get(&encode_cia(&s, &read), cia::CLATCH, 32), 0x0908_0706, "the latch while latched");
    }
}
