//! What the loaded firmware image says about the DDR layout the FPGA has to match (docs/status/carts.md, "Cartridge ROM
//! in DDR"). Firmware paths are under firmware/1541ultimate/software.
//!
//! The cartridge ROM moved with 3.15 ("Large cart support", 2e5c9e05): `__cart_rom_start` went from 0x00F00000 (1 MB)
//! to 0x03C00000 (4 MB), together with the FPGA's `g_rom_base_cart`. The C64 Ultimate 1.x firmware is built on 3.14 and
//! keeps the old place. The address is a linker constant, so a stripped image (`.app`, `.ue2`) only holds it as code:
//! `C64::set_cartridge` (c64.cc:1240-1290) loads `get_cartridge_rom_addr()` with a `lui` shortly before it prints
//! "Copying %d bytes from array %p to mem addr %p".

use crate::bus::RAM_MASK;
use crate::c64host::CartRom;

/// The format string `set_cartridge` passes the cartridge ROM address to (c64.cc:1285).
const COPY_MESSAGE: &[u8] = b"Copying %d bytes from array %p to mem addr %p";
/// How far before the message's address load the `lui` of the ROM address may be. 3.14, 3.15 and C64U 1.1.0 load it
/// 0xB8-0xC0 and 0x0C bytes before.
const WINDOW: u32 = 0x200;
/// `lui` and `addi` of one address are at most this far apart.
const PAIR: u32 = 64;

/// The cartridge ROM layout of the firmware in `ram` (loaded `segments`), or None when the image does not show it.
pub fn cart_rom(ram: &[u8], segments: &[(u32, u32)]) -> Option<CartRom> {
    let text = segments.iter().find_map(|&(base, len)| {
        let at = (base & RAM_MASK) as usize;
        let bytes = ram.get(at..at + len as usize)?;
        let i = bytes.windows(COPY_MESSAGE.len()).position(|w| w == COPY_MESSAGE)?;
        Some(base + i as u32)
    })?;
    let mut found = None;
    for &(base, len) in segments {
        let at = (base & RAM_MASK) as usize;
        let Some(code) = ram.get(at..at + len as usize) else { continue };
        let words = code.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")));
        // Per register: the last `lui` value and its address.
        let mut lui = [(0u32, 0u32); 32];
        let mut luis: Vec<(u32, u32)> = Vec::new();
        for (i, w) in words.enumerate() {
            let pc = base + 4 * i as u32;
            let rd = ((w >> 7) & 31) as usize;
            match w & 0x7F {
                0x37 => {
                    lui[rd] = (w & 0xFFFF_F000, pc);
                    luis.push((pc, w & 0xFFFF_F000));
                }
                // `addi rd, rs, imm` completing the message's address.
                0x13 if (w >> 12) & 7 == 0 => {
                    let rs = ((w >> 15) & 31) as usize;
                    let (hi, at) = lui[rs];
                    let value = hi.wrapping_add(((w as i32) >> 20) as u32);
                    if value == text && at != 0 && pc - at < PAIR {
                        // The layouts' bases among the `lui`s before it; two different ones leave it open.
                        let near = luis.iter().rev().take_while(|&&(p, _)| pc - p <= WINDOW);
                        for &(_, v) in near {
                            let Some(rom) = [CartRom::LARGE, CartRom::SMALL].into_iter().find(|r| r.base == v as usize)
                            else {
                                continue;
                            };
                            if found.is_some_and(|f| f != rom) {
                                return None;
                            }
                            found = Some(rom);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x0003_0000;

    fn lui(rd: u32, value: u32) -> u32 {
        (value & 0xFFFF_F000) | (rd << 7) | 0x37
    }

    fn addi(rd: u32, rs: u32, imm: i32) -> u32 {
        ((imm as u32) << 20) | (rs << 15) | (rd << 7) | 0x13
    }

    /// Code at BASE: `words`, then the message at BASE + 0x1000 (its address split as the compiler splits it).
    fn image(words: &[u32]) -> (Vec<u8>, Vec<(u32, u32)>) {
        let mut ram = vec![0u8; 0x0010_0000];
        for (i, w) in words.iter().enumerate() {
            let at = BASE as usize + 4 * i;
            ram[at..at + 4].copy_from_slice(&w.to_le_bytes());
        }
        let text = BASE as usize + 0x1000;
        ram[text..text + COPY_MESSAGE.len()].copy_from_slice(COPY_MESSAGE);
        (ram, vec![(BASE, 0x2000)])
    }

    /// `lui a5, rom` (get_cartridge_rom_addr), some code, then `lui a0 / addi a0` of the message.
    fn set_cartridge(rom: u32) -> Vec<u32> {
        let text = BASE + 0x1000;
        let hi = text.wrapping_add(0x800) & 0xFFFF_F000;
        let mut words = vec![lui(15, rom)];
        words.extend([addi(0, 0, 0); 10]);
        words.extend([lui(10, hi), addi(10, 10, (text.wrapping_sub(hi)) as i32)]);
        words
    }

    #[test]
    fn both_layouts_are_read_from_the_code() {
        let (ram, segs) = image(&set_cartridge(0x03C0_0000));
        assert_eq!(cart_rom(&ram, &segs), Some(CartRom::LARGE));
        let (ram, segs) = image(&set_cartridge(0x00F0_0000));
        assert_eq!(cart_rom(&ram, &segs), Some(CartRom::SMALL));
    }

    #[test]
    fn unknown_conflicting_or_missing_gives_none() {
        let (ram, segs) = image(&set_cartridge(0x0200_0000));
        assert_eq!(cart_rom(&ram, &segs), None, "not an FPGA layout");
        let mut words = set_cartridge(0x03C0_0000);
        words.insert(1, lui(14, 0x00F0_0000));
        let (ram, segs) = image(&words);
        assert_eq!(cart_rom(&ram, &segs), None, "two layouts");
        let mut far = vec![lui(15, 0x03C0_0000)];
        far.extend(vec![addi(0, 0, 0); 200]);
        far.extend_from_slice(&set_cartridge(0x03C0_0000)[11..]);
        let (ram, segs) = image(&far);
        assert_eq!(cart_rom(&ram, &segs), None, "too far before the message");
        let (mut ram, segs) = image(&set_cartridge(0x03C0_0000));
        ram[BASE as usize + 0x1000] = b'X';
        assert_eq!(cart_rom(&ram, &segs), None, "no message");
    }

    /// The upstream ELF (skipped without a firmware tree) is 3.15: the large layout, as its linker symbol says.
    #[test]
    fn upstream_firmware_has_the_large_layout() {
        let Some(root) = crate::loader::tests::firmware_root() else { return };
        let elf = root.join(crate::loader::tests::FIRMWARE_ELF);
        let mut ram = vec![0; crate::bus::RAM_SIZE];
        let fw = crate::loader::load_firmware(&elf, &mut ram).unwrap();
        let rom = cart_rom(&ram, &fw.segments).expect("found");
        let symbols = crate::symbols::Symbols::from_elf(&elf).unwrap();
        if let Some(sym) = symbols.addr_of("__cart_rom_start") {
            assert_eq!(rom.base, sym as usize);
        }
        assert_eq!(rom, CartRom::LARGE);
    }
}
