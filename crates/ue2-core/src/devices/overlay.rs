//! Overlay chargen, HDMI timing/palette, VIC cropper.
//! Window 0x10140000-0x1014FFFF. Spec: docs/specs/S07-overlay-u64io-render.md
//!
//! Layout (u64.h:21-32; `Overlay(false, 12, 0x10140000, …)` ultimate.cc:125 → overlay.h:51-53):
//!
//! | Offset | Block | Model |
//! |---|---|---|
//! | 0x0000-0x0FFF | chargen registers, decoded on addr(3:0) | write latches 0x0-0xD, reads 0 (char_generator_regs.vhd:43-80) |
//! | 0x1000-0x1FFF | screen RAM | 4 K RAM, power-up 0x20 (char_generator_peripheral_12.vhd:163-176); 05 #12, 00 §2 C33 |
//! | 0x2000-0x2FFF | colour RAM | 4 K RAM, power-up 0x0F (char_generator_peripheral_12.vhd:178-193); 05 #12, 00 §2 C33 |
//! | 0x4000-0x401D | HDMI timing `t_video_timing_regs` (u64.h:172-203) | stored, reads 0 (write-only in firmware, hdmi_scan.cc:6-30) |
//! | 0x5000-0x503F | HDMI palette 16 × {R,G,B,pad} | RAM (u64_config.cc:2733-2739, :2752-2755) |
//! | 0x8000-0x8003 | VIC cropper offset_x, offset_y, size_x/2, size_y/2 | stored, reads 0 (hdmi_scan.cc:45-60) |
//!
//! Everything else in the window reads 0 and ignores writes. No interrupts (05 §Interrupts).

use crate::host::DisplaySnapshot;
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

pub const BASE: u32 = 0x1014_0000;
pub const SIZE: u32 = 0x1_0000;

/// Chargen register indices (`t_chargen_registers`, chargen.h:13-28) used by `render`.
pub const REG_CHAR_WIDTH: usize = 0x2;
pub const REG_CHAR_HEIGHT: usize = 0x3;
pub const REG_CHARS_PER_LINE: usize = 0x4;
pub const REG_ACTIVE_LINES: usize = 0x5;
pub const REG_X_ON_HI: usize = 0x6;
pub const REG_X_ON_LO: usize = 0x7;
pub const REG_Y_ON_HI: usize = 0x8;
pub const REG_Y_ON_LO: usize = 0x9;
pub const REG_POINTER_HI: usize = 0xA;
pub const REG_POINTER_LO: usize = 0xB;
pub const REG_TRANSPARENCY: usize = 0xD;

/// Screen and colour RAM size: `g_screen_size` = addrbits = 12 (ultimate.cc:125; 05 OQ 1).
pub const TEXT_RAM_SIZE: usize = 0x1000;
/// `t_video_timing_regs` +00 HSYNCPOL .. +1D VID_YQ (u64.h:172-203).
pub const HDMI_REGS: usize = 0x1E;
/// 16 × {R,G,B,pad} (u64_config.cc:2733-2739).
pub const PALETTE_SIZE: usize = 64;
/// `t_vic_crop_regs` (hdmi_scan.cc:45-50).
pub const CROPPER_REGS: usize = 4;

const REGS_END: u32 = 0x0FFF;
const SCREEN: u32 = 0x1000;
const SCREEN_END: u32 = SCREEN + TEXT_RAM_SIZE as u32 - 1;
const COLOR: u32 = 0x2000;
const COLOR_END: u32 = COLOR + TEXT_RAM_SIZE as u32 - 1;
const HDMI: u32 = 0x4000;
const HDMI_END: u32 = HDMI + HDMI_REGS as u32 - 1;
const PALETTE: u32 = 0x5000;
const PALETTE_END: u32 = PALETTE + PALETTE_SIZE as u32 - 1;
const CROPPER: u32 = 0x8000;
const CROPPER_END: u32 = CROPPER + CROPPER_REGS as u32 - 1;

pub struct Overlay {
    /// Chargen register latches. 0xE/0xF are not decoded and stay 0 (char_generator_regs.vhd:76-77).
    pub regs: [u8; 16],
    pub screen: Vec<u8>,
    pub color: Vec<u8>,
    pub palette: Vec<u8>,
    /// HDMI timing registers 0x10144000.. (latched only; the frontend does its own scaling).
    pub hdmi: [u8; HDMI_REGS],
    /// VIC cropper 0x10148000..
    pub cropper: [u8; CROPPER_REGS],
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Overlay {
            regs: [0; 16],
            screen: vec![0x20; TEXT_RAM_SIZE],
            color: vec![0x0F; TEXT_RAM_SIZE],
            palette: vec![0; PALETTE_SIZE],
            hdmi: [0; HDMI_REGS],
            cropper: [0; CROPPER_REGS],
        }
    }

    pub fn snapshot(&self, now_ms: u64) -> DisplaySnapshot {
        DisplaySnapshot {
            regs: self.regs,
            screen: self.screen.clone(),
            color: self.color.clone(),
            palette: self.palette.clone(),
            hdmi: self.hdmi,
            now_ms,
            c64: None,
        }
    }
}

impl IoDevice for Overlay {
    fn name(&self) -> &'static str {
        "overlay"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        match off {
            0..=REGS_END => {
                let reg = (off & 0xF) as usize;
                if reg <= REG_TRANSPARENCY {
                    self.regs[reg] = val;
                }
            }
            SCREEN..=SCREEN_END => self.screen[(off - SCREEN) as usize] = val,
            COLOR..=COLOR_END => self.color[(off - COLOR) as usize] = val,
            HDMI..=HDMI_END => self.hdmi[(off - HDMI) as usize] = val,
            PALETTE..=PALETTE_END => self.palette[(off - PALETTE) as usize] = val,
            CROPPER..=CROPPER_END => self.cropper[(off - CROPPER) as usize] = val,
            _ => {}
        }
    }

    /// Reads have no side effects; chargen registers read 0 (char_generator_regs.vhd:79-80).
    fn peek8(&self, off: u32) -> u8 {
        match off {
            SCREEN..=SCREEN_END => self.screen[(off - SCREEN) as usize],
            COLOR..=COLOR_END => self.color[(off - COLOR) as usize],
            PALETTE..=PALETTE_END => self.palette[(off - PALETTE) as usize],
            _ => 0,
        }
    }

    crate::impl_as_any!();
}

/// Maps the whole 64 K video window (VID_IO_BASE, u64.h:21-28).
pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(BASE, SIZE, Box::new(Overlay::new()));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::irq::IrqState;

    fn with_ctx<R>(f: impl FnOnce(&mut IoCtx) -> R) -> R {
        let mut ram = [0u8; 0];
        let mut irq = IrqState::new();
        let mut console = Vec::new();
        let mut ctx = IoCtx { now: 0, pc: 0, ram: &mut ram, irq: &mut irq, console: &mut console };
        f(&mut ctx)
    }

    /// 05 #12, 00 §2 C33: cursor XOR, backup/restore and scrolling read the text RAM back.
    #[test]
    fn c33_screen_and_color_ram_read_back() {
        let mut ov = Overlay::new();
        with_ctx(|ctx| {
            assert_eq!(ov.read8(0x1000, ctx), 0x20, "screen RAM power-up value");
            assert_eq!(ov.read8(0x2FFF, ctx), 0x0F, "colour RAM power-up value");
            ov.write8(0x13E7, 0xC1, ctx);
            ov.write8(0x23E7, 0x6C, ctx);
            let cell = ov.read8(0x13E7, ctx) ^ 0x80;
            ov.write8(0x13E7, cell, ctx);
            assert_eq!(ov.read8(0x13E7, ctx), 0x41);
            assert_eq!(ov.read8(0x23E7, ctx), 0x6C);
        });
        assert_eq!(ov.screen[0x3E7], 0x41);
        assert_eq!(ov.color[0x3E7], 0x6C);
    }

    #[test]
    fn palette_write_read() {
        let mut ov = Overlay::new();
        with_ctx(|ctx| {
            for i in 0..PALETTE_SIZE as u32 {
                ov.write8(0x5000 + i, (i * 3) as u8, ctx);
            }
            for i in 0..PALETTE_SIZE as u32 {
                assert_eq!(ov.read8(0x5000 + i, ctx), (i * 3) as u8);
            }
            ov.write8(0x5040, 0xAA, ctx);
            assert_eq!(ov.read8(0x5040, ctx), 0, "past the palette is RAZ/WI");
        });
    }

    #[test]
    fn chargen_registers_latch_and_read_zero() {
        let mut ov = Overlay::new();
        with_ctx(|ctx| {
            // overlay.h:154-164 with the default SD geometry (05 Init step 7).
            for (off, val) in [(4, 0x28), (5, 0x19), (6, 0x01), (7, 0x2C), (8, 0x00), (9, 0xEB), (2, 0x08), (3, 0x09)] {
                ov.write8(off, val, ctx);
            }
            ov.write8(0x0D, 0xC0, ctx);
            assert_eq!(ov.read8(0x0D, ctx), 0, "chargen registers are never readable");
            // addr(3:0) decode: 0x1C aliases PERFORM_SYNC, 0x0E/0x0F are not registers.
            ov.write8(0x1C, 0x01, ctx);
            ov.write8(0x0E, 0x55, ctx);
            ov.write8(0xFFF, 0x66, ctx);
        });
        assert_eq!(ov.regs, [0, 0, 0x08, 0x09, 0x28, 0x19, 0x01, 0x2C, 0x00, 0xEB, 0, 0, 0x01, 0xC0, 0, 0]);
    }

    #[test]
    fn hdmi_timing_and_cropper_stored() {
        let mut ov = Overlay::new();
        with_ctx(|ctx| {
            ov.write8(0x4006, 2, ctx); // resync (u64_config.cc:2688-2689)
            ov.write8(0x401D, 0x77, ctx);
            ov.write8(0x401E, 0x99, ctx);
            ov.write8(0x8000, 0x20, ctx);
            ov.write8(0x8003, 0x88, ctx);
            assert_eq!(ov.read8(0x4006, ctx), 0);
            assert_eq!(ov.read8(0x8003, ctx), 0);
        });
        assert_eq!((ov.hdmi[6], ov.hdmi[0x1D]), (2, 0x77));
        assert_eq!(ov.cropper, [0x20, 0, 0, 0x88]);
    }

    #[test]
    fn snapshot_content() {
        let mut ov = Overlay::new();
        with_ctx(|ctx| {
            ov.write8(0x0D, 0xC0, ctx);
            ov.write8(0x1005, b'H', ctx);
            ov.write8(0x2005, 0x0C, ctx);
            ov.write8(0x5004, 0xF7, ctx);
        });
        let snap = ov.snapshot(1234);
        assert_eq!(snap.now_ms, 1234);
        assert_eq!(snap.regs[REG_TRANSPARENCY], 0xC0);
        assert_eq!((snap.screen.len(), snap.color.len(), snap.palette.len()), (4096, 4096, 64));
        assert_eq!((snap.screen[5], snap.color[5], snap.palette[4]), (b'H', 0x0C, 0xF7));
    }

    #[test]
    fn install_maps_video_window() {
        let mut map = IoMap::new();
        install(&mut map, &MachineConfig::new(PathBuf::new(), PathBuf::new()));
        let (dev, off) = map.resolve(0x1014_2010).expect("mapped");
        assert_eq!(off, 0x2010);
        assert_eq!(map.devices[dev].name(), "overlay");
        assert_eq!(map.resolve(0x1014_FFFF).map(|(_, o)| o), Some(0xFFFF));
        assert!(map.resolve(0x1015_0000).is_none());
        assert!(map.get::<Overlay>().is_some());
    }
}
