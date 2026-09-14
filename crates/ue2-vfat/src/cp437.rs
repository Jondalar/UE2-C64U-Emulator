//! Code page 437 for short (8.3) names. The firmware's FatFs uses code page 437 (`FF_CODE_PAGE`), so a name the
//! firmware stores without a long name can hold bytes 0x80-0xFF. fatfs's default converter turns every such byte into
//! U+FFFD, which the snapshot reader must refuse (it cannot tell one such name from another); decoded as code page
//! 437 they are ordinary characters the host can take.

use fatfs::OemCpConverter;

/// Unicode of the bytes 0x80-0xFF.
const HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{A0}', //
];

#[derive(Debug)]
pub struct Cp437;

pub static CP437: Cp437 = Cp437;

impl OemCpConverter for Cp437 {
    fn decode(&self, oem_char: u8) -> char {
        match oem_char {
            0..=0x7F => char::from(oem_char),
            _ => HIGH[usize::from(oem_char - 0x80)],
        }
    }

    fn encode(&self, uni_char: char) -> Option<u8> {
        if uni_char.is_ascii() {
            return Some(uni_char as u8);
        }
        HIGH.iter().position(|&c| c == uni_char).map(|i| 0x80 + i as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_bytes_round_trip() {
        assert_eq!(CP437.decode(b'A'), 'A');
        assert_eq!(CP437.decode(0x81), 'ü');
        assert_eq!(CP437.decode(0x9A), 'Ü');
        assert_eq!(CP437.decode(0xE1), 'ß');
        assert_eq!(CP437.decode(0xFF), '\u{A0}');
        for b in 0..=255u8 {
            assert_eq!(CP437.encode(CP437.decode(b)), Some(b), "{b:#04x}");
            assert_ne!(CP437.decode(b), '\u{FFFD}');
        }
        assert_eq!(CP437.encode('€'), None);
    }
}
