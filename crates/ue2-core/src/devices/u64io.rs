//! U64 IO page: HDMI/HPD, restore, cart detect, joystick, keyboard matrix scan, LED latches.
//! Window 0x10100400-0x101004FF. Spec: docs/specs/S07-overlay-u64io-render.md
//!
//! Register view (u64.h:65-80; docs/hw/03-board-init.md "U64 board control"; 00-memory-map §1b).
//! Every write is latched per offset; reads come from a separate view (00 §1c M5, M6):
//!
//! | Off | Name | Read | Write |
//! |---|---|---|---|
//! | 0x00 | U64_HDMI_REG | 0x04 = HPD_CURRENT (05 #3; 00 §2 C20, C35; §3 C10) | 0x20/0x10/0x08 DDC/HPD reset, no effect (no hot-plug IRQ modelled) |
//! | 0x01 | U64_POWER_REG | 0 | ignored (U64 mk1 only) |
//! | 0x02 | U64_RESTORE_REG | 0x00, no safe mode (05 #7; 00 §2 C8) | — |
//! | 0x03 | U64_CART_DETECT | GAME (bit 0) / EXROM (bit 1) of the physical expansion port: [`U64Io::cart_detect`], 0x03 = no external cart (c64.cc:1514) | — |
//! | 0x04 | U64_HDMI_PLL_RESET | 0 | latched |
//! | 0x05 | U64_USERPORT_EN | latch (u64_config.cc:1071-1074) | latched |
//! | 0x06 | U64II_KEYB_JOY | joystick lines, idle 0xFF (05 #8; 00 §2 C36) | swap bit, latched (u64_config.cc:1064) |
//! | 0x07 | U64II_BLACKBOARD | 0x01 (assembly.cc:36; 00 §3 C14) | — |
//! | 0x08-0x09 | HDMI_ENABLE / INT_CONNECTORS | 0 | latched |
//! | 0x0A | U64II_KEYB_COL | 0 | matrix line select, active low |
//! | 0x0B | U64II_KEYB_ROW | matrix return, see `set_key` (05 #9, #10; 00 §2 C18, C36) | 0xFF, no effect (keyboard_c64.cc:201-202) |
//! | 0x0C-0x0F | LEDSTRIP_EN, PWM_DUTY, CASELED_SELECT, ETHSTREAM_ENA | latch (read-modify-write, data_streamer.cc:335, :410-414) | latched |
//!
//! Offsets 0x10-0xFF read 0 and ignore writes. No interrupts (05 §Interrupts).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::c64host::CART_DETECT_NONE;
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

pub const BASE: u32 = 0x1010_0400;
pub const SIZE: u32 = 0x100;

const HDMI_REG: u32 = 0x00;
const RESTORE_REG: u32 = 0x02;
const CART_DETECT: u32 = 0x03;
const USERPORT_EN: u32 = 0x05;
const KEYB_JOY: u32 = 0x06;
const BLACKBOARD: u32 = 0x07;
const KEYB_COL: usize = 0x0A;
const KEYB_ROW: u32 = 0x0B;
const LEDSTRIP_EN: u32 = 0x0C;
const ETHSTREAM_ENA: u32 = 0x0F;

/// HPD_CURRENT set, HPD_WASLOW clear (u64.h:96-97): the overlay UI needs bit 2 (ultimate.cc:183-187).
const HDMI_REG_VALUE: u8 = 0x04;
const RESTORE_VALUE: u8 = 0x00;
const BLACKBOARD_VALUE: u8 = 0x01;

pub struct U64Io {
    /// Joystick lines (active low, idle 0xFF).
    pub joystick: u8,
    /// Last value written to each offset 0x00-0x0F.
    pub latch: [u8; 16],
    /// Pressed keys: bit `col` of `matrix[row]`, convention on `set_key`.
    pub matrix: [u8; 8],
    /// U64_CART_DETECT: GAME (bit 0) and EXROM (bit 1) of the physical expansion port. `Machine::attach_c64` shares
    /// it with the C64 port, which stores the backend's `cart_detect` after every access; 0x03 without a cartridge.
    pub cart_detect: Arc<AtomicU8>,
}

impl Default for U64Io {
    fn default() -> Self {
        Self::new()
    }
}

impl U64Io {
    pub fn new() -> Self {
        let mut latch = [0; 16];
        // No matrix line selected until the first scan writes the latch.
        latch[KEYB_COL] = 0xFF;
        U64Io { joystick: 0xFF, latch, matrix: [0; 8], cart_detect: Arc::new(AtomicU8::new(CART_DETECT_NONE)) }
    }

    /// Press/release a C64 matrix key. Positions outside 0..=7 are ignored.
    ///
    /// Convention = the firmware scanner's (keyboard_c64.cc:90-96, :236-255) and docs/hw/05 §C:
    /// - `row` is the matrix line selected by driving bit `row` of U64II_KEYB_COL (0x1010040A) low
    ///   (CIA1 port A equivalent).
    /// - `col` is the bit that then reads low on U64II_KEYB_ROW (0x1010040B) (CIA1 port B equivalent).
    /// - The keymap index is `row * 8 + col` (`keymap_normal`, keyboard_c64.cc:27-36), e.g. (0,1) RETURN,
    ///   (0,7) CRSR DOWN, (1,2) A, (1,7) LEFT SHIFT, (6,4) RIGHT SHIFT, (7,2) CTRL, (7,4) SPACE,
    ///   (7,5) C=, (7,7) RUN/STOP. MATRIX_KEYB uses the same layout (keyboard_usb.cc:111-124).
    ///
    /// The U64 register names are the other way round: the COL register selects a `row`, and the ROW
    /// register returns `col` bits.
    /// `ETHSTREAM_ENA`: one enable bit per UDP stream generator in the low nibble, the bus stream's mode in the
    /// high one (u64.h:80, data_streamer.cc:410-414).
    pub fn ethstream_ena(&self) -> u8 {
        self.latch[ETHSTREAM_ENA as usize]
    }

    pub fn set_key(&mut self, row: u8, col: u8, down: bool) {
        if row >= 8 || col >= 8 {
            return;
        }
        if down {
            self.matrix[row as usize] |= 1 << col;
        } else {
            self.matrix[row as usize] &= !(1 << col);
        }
    }

    pub fn set_joystick(&mut self, lines: u8) {
        self.joystick = lines;
    }

    /// U64II_KEYB_ROW: a pure function of the COL latch and the pressed keys, so the firmware's
    /// `do { row = *ROW; *COL = col; } while (row != *ROW)` loop terminates (keyboard_c64.cc:240-243).
    fn row_value(&self) -> u8 {
        let select = self.latch[KEYB_COL];
        (0..8).filter(|&row| select & (1 << row) == 0).fold(0xFF, |v, row| v & !self.matrix[row])
    }
}

impl IoDevice for U64Io {
    fn name(&self) -> &'static str {
        "u64io"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        if let Some(latch) = self.latch.get_mut(off as usize) {
            *latch = val;
        }
    }

    /// Reads have no side effects.
    fn peek8(&self, off: u32) -> u8 {
        match off {
            HDMI_REG => HDMI_REG_VALUE,
            RESTORE_REG => RESTORE_VALUE,
            CART_DETECT => self.cart_detect.load(Ordering::Relaxed),
            KEYB_JOY => self.joystick,
            BLACKBOARD => BLACKBOARD_VALUE,
            KEYB_ROW => self.row_value(),
            USERPORT_EN | LEDSTRIP_EN..=ETHSTREAM_ENA => self.latch[off as usize],
            _ => 0,
        }
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(BASE, SIZE, Box::new(U64Io::new()));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::irq::IrqState;

    const COL: u32 = KEYB_COL as u32;

    fn with_ctx<R>(f: impl FnOnce(&mut IoCtx) -> R) -> R {
        let mut ram = [0u8; 0];
        let mut irq = IrqState::new();
        let mut console = Vec::new();
        let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut ram, irq: &mut irq, console: &mut console };
        f(&mut ctx)
    }

    /// 00 §3 T0 constant reads; writes to the same offsets do not change them (M5, M6).
    #[test]
    fn t0_constants() {
        let mut io = U64Io::new();
        with_ctx(|ctx| {
            for val in [0x20, 0x10, 0x08] {
                io.write8(HDMI_REG, val, ctx);
            }
            io.write8(KEYB_JOY, 0x01, ctx);
            io.write8(KEYB_ROW, 0xFF, ctx);
            let read: Vec<u8> = [0x00, 0x02, 0x03, 0x06, 0x07, 0x0B].iter().map(|&o| io.read8(o, ctx)).collect();
            assert_eq!(read, [0x04, 0x00, 0x03, 0xFF, 0x01, 0xFF]);
            io.write8(0x10, 0x5A, ctx);
            assert_eq!(io.read8(0x10, ctx), 0);
            assert_eq!(io.read8(0xFF, ctx), 0);
        });
        assert_eq!(io.latch[KEYB_JOY as usize], 0x01, "joystick swap bit is latched");
    }

    #[test]
    fn led_pwm_ethstream_latches_read_back() {
        let mut io = U64Io::new();
        with_ctx(|ctx| {
            io.write8(USERPORT_EN, 3, ctx);
            for (off, val) in [(0x0C, 1), (0x0D, 0xD8), (0x0E, 0x40), (0x0F, 0x00)] {
                io.write8(off, val, ctx);
            }
            // data_streamer.cc:410-414: ETHSTREAM_ENA &= 0x0F; |= mode << 4; |= 1 << id.
            let v = io.read8(ETHSTREAM_ENA, ctx) & 0x0F;
            io.write8(ETHSTREAM_ENA, v | 0x20, ctx);
            let v = io.read8(ETHSTREAM_ENA, ctx) | 0x02;
            io.write8(ETHSTREAM_ENA, v, ctx);
            let read: Vec<u8> = (0x0C..=0x0F).map(|o| io.read8(o, ctx)).collect();
            assert_eq!(read, [1, 0xD8, 0x40, 0x22]);
            assert_eq!(io.read8(USERPORT_EN, ctx), 3);
            io.write8(0x09, 0x70, ctx);
            assert_eq!(io.read8(0x09, ctx), 0, "INT_CONNECTORS is write-only");
        });
    }

    /// 05 #9, #10; 00 §2 C36: idle ROW is 0xFF and stable for any COL latch.
    #[test]
    fn c36_row_idle_and_stable() {
        let mut io = U64Io::new();
        with_ctx(|ctx| {
            for col in [0x00, 0xFE, 0x7F, 0xFF] {
                io.write8(COL, col, ctx);
                assert_eq!(io.read8(KEYB_ROW, ctx), 0xFF);
                assert_eq!(io.read8(KEYB_ROW, ctx), 0xFF);
            }
        });
    }

    #[test]
    fn keyboard_press_shows_only_on_matching_col_select() {
        let mut io = U64Io::new();
        io.set_key(3, 5, true);
        with_ctx(|ctx| {
            io.write8(COL, 0x00, ctx);
            assert_eq!(io.read8(KEYB_ROW, ctx), !(1 << 5), "all lines selected");
            for line in 0..8 {
                io.write8(COL, !(1u8 << line), ctx);
                let expect = if line == 3 { !(1u8 << 5) } else { 0xFF };
                assert_eq!(io.read8(KEYB_ROW, ctx), expect, "COL line {line}");
                assert_eq!(io.read8(KEYB_ROW, ctx), expect, "stable between reads");
            }
            io.write8(COL, 0xFF, ctx);
            assert_eq!(io.read8(KEYB_ROW, ctx), 0xFF, "nothing selected");
        });
        io.set_key(3, 5, false);
        io.set_key(8, 0, true);
        io.set_key(0, 8, true);
        with_ctx(|ctx| {
            io.write8(COL, 0x00, ctx);
            assert_eq!(io.read8(KEYB_ROW, ctx), 0xFF, "released; out-of-range ignored");
        });
    }

    /// Runs the firmware's scan loop (keyboard_c64.cc:231-255) against the model: the decoded keymap
    /// index must be `row * 8 + col`, with modifiers from `modifier_map` (keyboard_c64.cc:16-25).
    #[test]
    fn firmware_scan_decodes_row_col_index() {
        fn scan(io: &mut U64Io, ctx: &mut IoCtx) -> Option<(usize, u8)> {
            const MODIFIERS: [(usize, u8); 4] = [(15, 0x01), (52, 0x01), (58, 0x04), (61, 0x02)];
            io.write8(COL, 0, ctx);
            if io.read8(KEYB_ROW, ctx) == 0xFF {
                return None;
            }
            let (mut mtrx, mut shift, mut col, mut idx) = (0x40, 0u8, 0xFEu8, 0);
            for _ in 0..8 {
                io.write8(COL, 0xFF, ctx);
                io.write8(COL, col, ctx);
                io.write8(COL, col, ctx);
                let mut row = loop {
                    let row = io.read8(KEYB_ROW, ctx);
                    io.write8(COL, col, ctx);
                    if row == io.read8(KEYB_ROW, ctx) {
                        break row;
                    }
                };
                for _ in 0..8 {
                    if row & 1 == 0 {
                        match MODIFIERS.iter().find(|m| m.0 == idx) {
                            Some(&(_, flag)) => shift |= flag,
                            None => mtrx = idx,
                        }
                    }
                    row >>= 1;
                    idx += 1;
                }
                col = (col << 1) | 1;
            }
            Some((mtrx, shift))
        }

        let mut io = U64Io::new();
        with_ctx(|ctx| {
            assert_eq!(scan(&mut io, ctx), None);
            io.set_key(1, 2, true); // 'a' = keymap_normal[10]
            assert_eq!(scan(&mut io, ctx), Some((10, 0)));
            io.set_key(1, 7, true); // LEFT SHIFT = modifier_map[15]
            assert_eq!(scan(&mut io, ctx), Some((10, 0x01)));
            io.set_key(1, 2, false);
            io.set_key(1, 7, false);
            io.set_key(7, 7, true); // RUN/STOP = keymap_normal[63]
            assert_eq!(scan(&mut io, ctx), Some((63, 0)));
        });
    }

    /// CARTSLOT: U64_CART_DETECT reads the shared physical-port lines; writes do not change them.
    #[test]
    fn cart_detect_follows_the_shared_lines() {
        let mut io = U64Io::new();
        let cell = io.cart_detect.clone();
        with_ctx(|ctx| {
            assert_eq!(io.read8(CART_DETECT, ctx), 0x03, "empty port");
            cell.store(0x02, Ordering::Relaxed);
            io.write8(CART_DETECT, 0x03, ctx);
            assert_eq!(io.read8(CART_DETECT, ctx), 0x02, "EXROM high, GAME low: an ULTIMAX cartridge");
        });
    }

    #[test]
    fn joystick_lines() {
        let mut io = U64Io::new();
        with_ctx(|ctx| {
            assert_eq!(io.read8(KEYB_JOY, ctx), 0xFF);
            io.set_joystick(0xEF); // fire (keyboard_c64.cc:218)
            io.write8(KEYB_JOY, 0x00, ctx);
            assert_eq!(io.read8(KEYB_JOY, ctx), 0xEF, "write is the swap bit, not the lines");
        });
    }

    #[test]
    fn install_maps_io_page() {
        let mut map = IoMap::new();
        install(&mut map, &MachineConfig::new(PathBuf::new(), PathBuf::new()));
        let (dev, off) = map.resolve(0x1010_040B).expect("mapped");
        assert_eq!((map.devices[dev].name(), off), ("u64io", 0x0B));
        assert!(map.resolve(0x1010_0500).is_none());
        assert!(map.get_mut::<U64Io>().is_some());
    }
}
