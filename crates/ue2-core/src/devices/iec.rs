//! IEC processor, ACIA, C2N tape (T0).
//! Spec: docs/specs/S04-board-t0.md. Registers: docs/hw/11-drives-iec-periph.md.
//!
//! The UltiCommand interface 0x10044000 used to be a table here; it is a `devices::c64::C64Port` window now, so
//! the C64's UCI block can serve it (docs/specs/S15-uci.md).

use crate::devices::board::{add_table, at, span, Reg, Span, RAM};
use crate::io::IoMap;
use crate::machine::MachineConfig;

/// IEC processor 0x10028000 (iec_processor_io.vhd). Registers decode `address(3:0)`, CODE RAM is bit 11.
const IEC: &[Span] = &[
    // VERSION, only printed (iec_interface.cc:73).
    at(0x00, Reg::Const(0x25)),
    // 00 §2 C22, 11 H11/H12: idle FIFOs. TX_FIFO_STATUS 0x01 = down FIFO empty, not full; RX_FIFO_STATUS
    // 0x01 = up FIFO empty, so the "IEC Server" poll every 2 ticks (iec_interface.cc:182-189) reads nothing.
    at(0x01, Reg::Const(0x01)),
    at(0x02, Reg::Const(0x01)),
    // 00 §1c M7, 11 H10: the slot[3] `dst[-1]` write to 0x100287FF (iec_interface.cc:121-126,141) is a no-op.
    // CODE RAM: 0x768-byte microcode plus the patched device address bytes (iec_interface.cc:71-81,128-145).
    span(0x800, 0x1000, RAM),
];

/// ACIA 6551 0x1004A000 (acia6551.vhd). No C64 side, so its registers stay at reset and irq_source is 0:
/// high IRQ 0 is never raised.
const ACIA: &[Span] = &[
    // rx_head / tx_tail: the app-owned ring indices.
    at(0x00, RAM),
    at(0x03, RAM),
    // 0x01 rx_tail, 0x02 tx_head, 0x04 control, 0x06 status read 0. command resets to 0x02.
    at(0x05, Reg::Const(0x02)),
    // enable + IRQ enables (acia.cc:18-20,72-84).
    at(0x07, Reg::Latch { mask: 0x1F, init: 0 }),
    // handsh: CTS 0, DSR 2, DCD 4, RTS disable 5, RX pushback 6 (reset 1); RTS/DTR come from the C64.
    at(0x08, Reg::Latch { mask: 0x75, init: 0x40 }),
    // TX ring 0x800 and RX ring 0xA00. The +0x100 mirrors of the 512-byte BRAM are not modelled; the
    // firmware never uses them.
    span(0x800, 0x900, RAM),
    span(0xA00, 0xB00, RAM),
];

/// C2N playback 0x100A0000: every read returns PLAYBACK_STATUS (c2n_playback_io.vhd). 00 §2 C26, 11 H15:
/// idle = FIFO empty (bit7), not enabled.
const TAPE_PLAY: &[Span] = &[span(0x000, 0x1000, Reg::Const(0x80))];

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    add_table(map, 0x1002_8000, 0x1000, "iec", IEC);
    add_table(map, 0x1004_A000, 0x1000, "acia", ACIA);
    add_table(map, 0x100A_0000, 0x1000, "tape-play", TAPE_PLAY);
    // C2N record 0x100C0000: RECORD_STATUS 0 (bit7 = FIFO non-empty) and FIFO reads 0, so `flush()`
    // (tape_recorder.cc:250-262) exits and ITU bit 3 stays low (00 §2 C27, 11 H8/H16).
    add_table(map, 0x100C_0000, 0x1000, "tape-record", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::board::rig::Rig;

    #[test]
    fn c22_iec_registers() {
        let mut rig = Rig::new(install);
        assert_eq!([0, 1, 2].map(|o| rig.r8(0x1002_8000 + o)), [0x25, 0x01, 0x01]);
        // IecInterface ctor: reset, code load (iec_interface.cc:71-81).
        rig.w8(0x1002_8003, 0x00);
        for i in 0..0x768 {
            rig.w8(0x1002_8800 + i, i as u8);
        }
        // configure(): slot 0 listener/talker, then the stray slot 3 write.
        rig.w8(0x1002_8844, 0x3F);
        rig.w8(0x1002_8828, 0x5F);
        rig.w8(0x1002_87FF, 0x3F);
        rig.w8(0x1002_87FF, 0x5F);
        assert_eq!(rig.r8(0x1002_87FF), 0);
        assert_eq!((rig.r8(0x1002_8844), rig.r8(0x1002_8828), rig.r8(0x1002_8F67)), (0x3F, 0x5F, 0x67));
        assert_eq!([1, 2].map(|o| rig.r8(0x1002_8000 + o)), [0x01, 0x01], "still idle");
    }

    #[test]
    fn a6_acia_idle() {
        let mut rig = Rig::new(install);
        // Acia ctor (acia.cc:18-20).
        rig.w8(0x1004_A00A, 0x00);
        rig.w8(0x1004_A007, 0x00);
        assert_eq!(rig.r8(0x1004_A009), 0, "irq_source");
        assert_eq!(rig.r8(0x1004_A008), 0x40);
        rig.w8(0x1004_A007, 0x07);
        rig.w8(0x1004_A003, 0x10);
        rig.w8(0x1004_AA00, 0x41);
        assert_eq!((rig.r8(0x1004_A007), rig.r8(0x1004_A003), rig.r8(0x1004_AA00)), (0x07, 0x10, 0x41));
        assert_eq!((rig.r8(0x1004_A002), rig.r8(0x1004_A00A)), (0, 0));
    }

    #[test]
    fn c26_tape_idle() {
        let mut rig = Rig::new(install);
        // TapeController::stop (tape_controller.cc:111-115), TapeRecorder::stop (tape_recorder.cc:138-158).
        rig.w8(0x100A_0000, 0x06);
        rig.w8(0x100A_0000, 0x00);
        rig.w8(0x100C_0000, 0x00);
        rig.w8(0x100C_0000, 0x06);
        assert_eq!(rig.r8(0x100A_0000), 0x80);
        assert_eq!(rig.r8(0x100A_0800), 0x80);
        assert_eq!(rig.r8(0x100C_0000), 0x00);
        assert_eq!(rig.r32(0x100C_0800), 0);
    }
}
