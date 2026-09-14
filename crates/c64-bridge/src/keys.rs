//! C64 key matrix → TRX64 held keys (docs/specs/S14-c64-trx64.md §6).
//!
//! Index `row * 8 + col` in the `U64Io::set_key` convention: `row` is the CIA1 port A bit that selects the line,
//! `col` the port B bit that reads low (keyboard_c64.cc:27-36). TRX64's `key_matrix` gives the same pair as
//! `(col, row)` with col = port A and row = port B (keyboard.rs:18-47), and only takes key names.

use trx64_core::keyboard::KeyboardMatrix;

/// TRX64 key name per matrix index.
pub const KEY_NAMES: [&str; 64] = [
    "DEL", "RETURN", "CRSR_RT", "F7", "F1", "F3", "F5", "CRSR_DN", //
    "3", "W", "A", "4", "Z", "S", "E", "L_SHIFT", //
    "5", "R", "D", "6", "C", "F", "T", "X", //
    "7", "Y", "G", "8", "B", "H", "U", "V", //
    "9", "I", "J", "0", "M", "K", "O", "N", //
    "+", "P", "L", "-", ".", ":", "@", ",", //
    "POUND", "*", ";", "HOME", "R_SHIFT", "=", "UP_ARROW", "/", //
    "1", "LARROW", "CTRL", "2", "SPACE", "C_EQ", "Q", "RUN_STOP", //
];

/// Keys pressed by the host and by MATRIX_KEYB, each as `[row] = col bits`, and what TRX64 holds.
#[derive(Debug, Default)]
pub struct Keys {
    host: [u8; 8],
    matrix: [u8; 8],
    held: [u8; 8],
}

impl Keys {
    /// Host key; positions outside the matrix are ignored.
    pub fn set_host(&mut self, row: u8, col: u8, down: bool) {
        if let (Some(line), true) = (self.host.get_mut(usize::from(row)), col < 8) {
            if down {
                *line |= 1 << col;
            } else {
                *line &= !(1 << col);
            }
        }
    }

    /// MATRIX_KEYB rows, active high (keyboard_usb.cc:214-229).
    pub fn set_matrix(&mut self, rows: [u8; 8]) {
        self.matrix = rows;
    }

    /// TRX64 dropped its held keys (`cold_reset`, lib.rs:588).
    pub fn cleared(&mut self) {
        self.held = [0; 8];
    }

    /// Press and release TRX64 keys until it holds the host keys OR MATRIX_KEYB.
    pub fn apply(&mut self, kb: &mut KeyboardMatrix) {
        for row in 0..8 {
            let want = self.host[row] | self.matrix[row];
            let changed = want ^ self.held[row];
            for col in (0..8).filter(|col| changed & (1 << col) != 0) {
                let name = KEY_NAMES[row * 8 + col];
                if want & (1 << col) != 0 {
                    kb.key_down(name);
                } else {
                    kb.key_up(name);
                }
            }
            self.held[row] = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name drives TRX64's matrix at its own position (row = port A line, col = port B bit).
    #[test]
    fn all_64_names_hit_their_matrix_position() {
        for (index, name) in KEY_NAMES.iter().enumerate() {
            let (row, col) = (index / 8, index % 8);
            let mut kb = KeyboardMatrix::new();
            kb.key_down(name);
            for line in 0..8 {
                let expect = if line == row { !(1u8 << col) } else { 0xFF };
                assert_eq!(kb.read_rows_for_pa(0, !(1u8 << line)), expect, "{name} on line {line}");
            }
        }
    }

    #[test]
    fn host_and_matrix_fold_into_held_keys() {
        let mut keys = Keys::default();
        let mut kb = KeyboardMatrix::new();
        keys.set_host(0, 1, true); // RETURN
        keys.set_matrix([0, 0x80, 0, 0, 0, 0, 0, 0x80]); // L_SHIFT, RUN_STOP
        keys.apply(&mut kb);
        assert_eq!(kb.pressed_keys(), ["RETURN", "L_SHIFT", "RUN_STOP"]);
        keys.set_host(1, 7, true);
        keys.set_matrix([0; 8]);
        keys.apply(&mut kb);
        assert_eq!(kb.pressed_keys(), ["RETURN", "L_SHIFT"], "held by the host alone");
        keys.set_host(8, 0, true);
        keys.set_host(0, 8, true);
        keys.set_host(0, 1, false);
        kb.clear();
        keys.cleared();
        keys.apply(&mut kb);
        assert_eq!(kb.pressed_keys(), ["L_SHIFT"], "re-applied after a reset; out-of-range ignored");
    }
}
