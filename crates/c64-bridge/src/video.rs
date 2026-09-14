//! C64 palette and frame capture (docs/specs/S14-c64-trx64.md §5.4, §9).

use trx64_core::render::COLODORE;
use trx64_core::Machine;
use ue2_core::c64host::{C64Charset, C64Frame};

/// VIC register $D018: bits 7:4 give the screen RAM page, `(v >> 4) << 10` in the VIC bank.
const VIC_MEM_PTR: u8 = 0x18;
/// 40 × 25 text cells.
const SCREEN_CELLS: usize = 1000;

/// C64_PALETTE RGB: 16 × {R,G,B,pad} (u64_config.cc:2720-2731). TRX64's COLODORE until the firmware writes it.
pub struct Palette([u8; 64]);

impl Default for Palette {
    fn default() -> Self {
        let mut bytes = [0; 64];
        for (entry, rgb) in bytes.chunks_exact_mut(4).zip(COLODORE) {
            entry[..3].copy_from_slice(&rgb);
        }
        Palette(bytes)
    }
}

impl Palette {
    pub fn set_byte(&mut self, off: u8, val: u8) {
        if let Some(byte) = self.0.get_mut(usize::from(off)) {
            *byte = val;
        }
    }

    /// 0x00RRGGBB per colour index.
    pub fn rgb(&self) -> [u32; 16] {
        std::array::from_fn(|i| u32::from_be_bytes([0, self.0[i * 4], self.0[i * 4 + 1], self.0[i * 4 + 2]]))
    }
}

/// The last complete frame (384×272 PAL canvas, lib.rs:1766) and the text screen the VIC shows.
pub fn frame(m: &Machine, palette: &Palette) -> C64Frame {
    let (width, height, indices) = m.render_canvas_indices();
    let d018 = m.vic.read_reg(VIC_MEM_PTR);
    let bank = m.vic_bank_base();
    let base = usize::from(bank) + (usize::from(d018 >> 4) << 10);
    let screen = (base..base + SCREEN_CELLS).map(|addr| m.ram[addr & 0xFFFF]).collect();
    // Bits 3:1 give the character base, `(v & 0x0E) << 10`; the CHAR ROM shows at $1000-$1FFF of banks 0 and 2
    // (vic.rs:149-157).
    let charset = match bank.wrapping_add(u16::from(d018 & 0x0E) << 10) & 0x7800 {
        0x1000 => C64Charset::Upper,
        0x1800 => C64Charset::Lower,
        _ => C64Charset::Ram,
    };
    C64Frame { width, height, indices, palette: palette.rgb(), screen, charset }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_bytes() {
        let mut palette = Palette::default();
        assert_eq!(palette.rgb()[6], 0x0027_24C4, "COLODORE blue (render.rs:734)");
        // default_colors[6] = 2C 29 B1 (u64_config.cc:54-70) at +24, pad byte and past-the-end writes ignored.
        for (off, val) in [(24, 0x2C), (25, 0x29), (26, 0xB1), (27, 0xFF), (64, 0xFF)] {
            palette.set_byte(off, val);
        }
        assert_eq!((palette.rgb()[6], palette.rgb()[7]), (0x002C_29B1, u32::from_be_bytes([0, COLODORE[7][0], COLODORE[7][1], COLODORE[7][2]])));
    }

    #[test]
    fn frame_reads_the_screen_the_vic_shows() {
        let mut m = Machine::new();
        (m.ram[0x0400], m.ram[0x07E7], m.ram[0x2000]) = (0x08, 0x2E, 0x09);
        m.vic.write_reg(VIC_MEM_PTR, 0x14);
        let shot = frame(&m, &Palette::default());
        assert_eq!((shot.width, shot.height, shot.indices.len()), (384, 272, 384 * 272));
        assert_eq!((shot.screen.len(), shot.screen[0], shot.screen[999]), (SCREEN_CELLS, 0x08, 0x2E), "$D018 $14: $0400");
        assert_eq!(shot.charset, C64Charset::Upper);
        m.vic.write_reg(VIC_MEM_PTR, 0x86);
        let shot = frame(&m, &Palette::default());
        assert_eq!((shot.screen[0], shot.charset), (0x09, C64Charset::Lower), "$D018 $86: $2000, ROM $1800");
        m.vic.write_reg(VIC_MEM_PTR, 0x12);
        assert_eq!(frame(&m, &Palette::default()).charset, C64Charset::Ram, "Freeze UI: font at $0800");
    }
}
