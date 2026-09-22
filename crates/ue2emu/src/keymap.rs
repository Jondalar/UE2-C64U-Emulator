//! Host keys and control-language key names → C64 matrix. Spec: docs/specs/S08-frontend-control.md
//!
//! Matrix convention (docs/hw/05-ui-overlay-input.md §C): `row` is the bit the firmware drives low in
//! U64II_KEYB_COL 0x1010040A, `col` the bit that answers low in U64II_KEYB_ROW 0x1010040B. The
//! firmware keymaps are indexed `row * 8 + col`, the order `Keyboard_C64::matrixToKeyCode(row, col)`
//! uses (firmware software/io/c64/keyboard_c64.cc:90-96) and the order its scan loop fills
//! (keyboard_c64.cc:231-254: outer loop over COL bits, inner loop over ROW bits). This is the
//! `HostInput::Key { row, col }` convention S07 documents on `U64Io::set_key`.

use winit::keyboard::KeyCode;

/// A C64 matrix key, optionally requiring SHIFT (e.g. CRSR UP = SHIFT + CRSR DOWN).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixKey {
    pub row: u8,
    pub col: u8,
    pub shift: bool,
}

impl MatrixKey {
    const fn at(index: u8, shift: bool) -> Self {
        MatrixKey { row: index >> 3, col: index & 7, shift }
    }
}

/// Left SHIFT, the modifier pressed for `MatrixKey::shift` (keyboard_c64.cc:18 `modifier_map[15]`).
pub const LSHIFT: MatrixKey = MatrixKey::at(15, false);

/// Printable legend without modifiers, `keymap_normal` (keyboard_c64.cc:27-36). 0 = not a character.
const NORMAL: [u8; 64] = [
    0, 0, 0, 0, 0, 0, 0, 0, // DEL RETURN CRSR→ F7 F1 F3 F5 CRSR↓
    b'3', b'w', b'a', b'4', b'z', b's', b'e', 0, // … LSHIFT
    b'5', b'r', b'd', b'6', b'c', b'f', b't', b'x',
    b'7', b'y', b'g', b'8', b'b', b'h', b'u', b'v',
    b'9', b'i', b'j', b'0', b'm', b'k', b'o', b'n',
    b'+', b'p', b'l', b'-', b'.', b':', b'@', b',',
    b'\\', b'*', b';', 0, 0, b'=', b'|', b'/', // £ * ; HOME RSHIFT = ↑ /
    b'1', b'`', 0, b'2', b' ', 0, b'q', 0, // 1 ← CTRL 2 SPACE C= Q RUN/STOP
];

/// Printable legend with SHIFT, `keymap_shifted` (keyboard_c64.cc:49-58). 0 = not a character.
const SHIFTED: [u8; 64] = [
    0, 0, 0, 0, 0, 0, 0, 0, // INST (0x8D) CRSR← F8 F2 F4 F6 CRSR↑
    b'#', b'W', b'A', b'$', b'Z', b'S', b'E', 0,
    b'%', b'R', b'D', b'&', b'C', b'F', b'T', b'X',
    b'\'', b'Y', b'G', b'(', b'B', b'H', b'U', b'V',
    b')', b'I', b'J', b'0', b'M', b'K', b'O', b'N',
    b'{', b'P', b'L', b'}', b'>', b'[', b'@', b'<',
    b'_', b'*', b']', 0, 0, b'=', b'^', b'?', // … CLR …
    b'!', b'~', 0, b'"', 0, 0, b'Q', 0, // … SHIFT-SPACE … RUN/STOP
];

/// Control-language names for keys without a single-character legend, as (name, index, shift).
/// Indices from `keymap_normal`/`keymap_shifted` (keyboard_c64.cc:27-36, :49-58) and
/// `modifier_map` (keyboard_c64.cc:16-25). Any other key is named by its character, see
/// [`key_for_char`].
const NAMES: &[(&str, u8, bool)] = &[
    ("del", 0, false),
    ("inst", 0, true),
    ("return", 1, false),
    ("right", 2, false),
    ("left", 2, true),
    ("f7", 3, false),
    ("f8", 3, true),
    ("f1", 4, false),
    ("f2", 4, true),
    ("f3", 5, false),
    ("f4", 5, true),
    ("f5", 6, false),
    ("f6", 6, true),
    ("down", 7, false),
    ("up", 7, true),
    ("lshift", 15, false),
    ("pound", 48, false),
    ("home", 51, false),
    ("clr", 51, true),
    ("rshift", 52, false),
    ("uparrow", 54, false),
    ("larrow", 57, false),
    ("ctrl", 58, false),
    ("space", 60, false),
    ("cbm", 61, false),
    ("runstop", 63, false),
];

/// Control-language key name (`return`, `down`, `f1`, `a`, …) → matrix key.
///
/// Multi-character names are case-insensitive ([`NAMES`]); a single character is looked up as typed,
/// so `a` is the A key and `A` is SHIFT + A.
pub fn key_by_name(name: &str) -> Option<MatrixKey> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return key_for_char(c);
    }
    let lower = name.to_ascii_lowercase();
    NAMES
        .iter()
        .find(|(n, _, _)| *n == lower)
        .map(|&(_, index, shift)| MatrixKey::at(index, shift))
}

/// Matrix key that makes the Ultimate menu receive `c` (the firmware keymaps decide the character,
/// keyboard_c64.cc:277-278). Unshifted legends win, so `0`, `*`, `=`, `@` need no SHIFT.
pub fn key_for_char(c: char) -> Option<MatrixKey> {
    if !c.is_ascii() || c == '\0' {
        return None;
    }
    let b = c as u8;
    let find = |table: &[u8; 64]| table.iter().position(|&x| x == b).map(|i| i as u8);
    find(&NORMAL)
        .map(|i| MatrixKey::at(i, false))
        .or_else(|| find(&SHIFTED).map(|i| MatrixKey::at(i, true)))
}

/// Host key for the C64 RESTORE key, Page Up as in VICE. RESTORE is not in the matrix but on the NMI line
/// (`HostInput::Restore`, docs/specs/S14-c64-trx64.md §6), so the window handles it.
pub const RESTORE_KEY: KeyCode = KeyCode::PageUp;

/// Host key (physical position, US layout) → matrix key. Covers letters, digits, space, return,
/// backspace/delete → DEL, cursor keys (CRSR with SHIFT for up/left), F1-F8 (F2/F4/F6/F8 = SHIFT +
/// F1/F3/F5/F7), Home, `.` and `,`, the shift keys, Ctrl → CTRL, Option → C=, ESC → RUN/STOP.
/// F12 (menu button) and [`RESTORE_KEY`] are handled by the window, not here.
pub fn host_key(code: KeyCode) -> Option<MatrixKey> {
    use KeyCode::*;
    let name = match code {
        KeyA => "a",
        KeyB => "b",
        KeyC => "c",
        KeyD => "d",
        KeyE => "e",
        KeyF => "f",
        KeyG => "g",
        KeyH => "h",
        KeyI => "i",
        KeyJ => "j",
        KeyK => "k",
        KeyL => "l",
        KeyM => "m",
        KeyN => "n",
        KeyO => "o",
        KeyP => "p",
        KeyQ => "q",
        KeyR => "r",
        KeyS => "s",
        KeyT => "t",
        KeyU => "u",
        KeyV => "v",
        KeyW => "w",
        KeyX => "x",
        KeyY => "y",
        KeyZ => "z",
        Digit0 => "0",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        Period => ".",
        Comma => ",",
        // The C64 row after "0" is 0,+,-,£,HOME,DEL; a PC's is 0,-,=,Backspace — so PC Minus lines up
        // with C64 "+", not "-". Same logic on the home row (L,:,;,= vs PC's L,;,') and the QWERTY row
        // (P,@,*,↑ vs PC's P,[,],\). The C64 has more dedicated symbol keys in these two rows than a PC
        // keyboard has spare punctuation positions, so "=" and "£" (pound) have no honest positional slot
        // left; F9/F10 are unused otherwise and stand in for them.
        Semicolon => ":",
        Quote => ";",
        Slash => "/",
        Minus => "+",
        Equal => "-",
        BracketLeft => "@",
        BracketRight => "*",
        Backslash => "uparrow",
        Space => "space",
        Enter | NumpadEnter => "return",
        Backspace | Delete => "del",
        ArrowDown => "down",
        ArrowUp => "up",
        ArrowRight => "right",
        ArrowLeft => "left",
        F1 => "f1",
        F2 => "f2",
        F3 => "f3",
        F4 => "f4",
        F5 => "f5",
        F6 => "f6",
        F7 => "f7",
        F8 => "f8",
        F9 => "=",
        F10 => "pound",
        // F11 aliases HOME for keyboards that have no Home key. On macOS, Mission Control owns F11
        // ("Show Desktop") by default, so the key never reaches the application and the alias is inert
        // until that shortcut is turned off in System Settings > Keyboard > Keyboard Shortcuts. Home
        // itself is unaffected either way.
        F11 | Home => "home",
        ShiftLeft => "lshift",
        ShiftRight => "rshift",
        ControlLeft | ControlRight => "ctrl",
        AltLeft | AltRight => "cbm",
        Escape => "runstop",
        Backquote => "larrow",
        _ => return None,
    };
    key_by_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(k: MatrixKey) -> usize {
        k.row as usize * 8 + k.col as usize
    }

    /// Every name the control language documents (docs/specs/S08-frontend-control.md, control.rs).
    #[test]
    fn control_language_names_resolve() {
        let names = [
            "return", "down", "up", "left", "right", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8",
            "space", "runstop", "del", "inst", "home", "clr", "lshift", "rshift", "ctrl", "cbm",
            "larrow", "uparrow", "pound",
        ];
        for n in names {
            assert!(key_by_name(n).is_some(), "name '{n}' must resolve");
            assert_eq!(key_by_name(&n.to_uppercase()), key_by_name(n), "'{n}' is case-insensitive");
        }
        for (n, _, _) in NAMES {
            assert!(names.contains(n), "name '{n}' missing from the documented list");
        }
        for c in ('a'..='z').chain('0'..='9') {
            assert!(key_by_name(&c.to_string()).is_some(), "'{c}' must resolve");
        }
        assert_eq!(key_by_name("nope"), None);
        assert_eq!(key_by_name(""), None);
    }

    #[test]
    fn named_positions_match_the_c64_matrix() {
        assert_eq!(key_by_name("down"), Some(MatrixKey { row: 0, col: 7, shift: false }));
        assert_eq!(key_by_name("up"), Some(MatrixKey { row: 0, col: 7, shift: true }));
        assert_eq!(key_by_name("return"), Some(MatrixKey { row: 0, col: 1, shift: false }));
        assert_eq!(key_by_name("runstop"), Some(MatrixKey { row: 7, col: 7, shift: false }));
        assert_eq!(key_by_name("space"), Some(MatrixKey { row: 7, col: 4, shift: false }));
        assert_eq!(key_by_name("f2"), Some(MatrixKey { row: 0, col: 4, shift: true }));
        assert_eq!(key_by_name("a"), Some(MatrixKey { row: 1, col: 2, shift: false }));
        assert_eq!(key_by_name("A"), Some(MatrixKey { row: 1, col: 2, shift: true }));
        assert_eq!(LSHIFT, MatrixKey { row: 1, col: 7, shift: false });
    }

    #[test]
    fn every_printable_legend_round_trips() {
        for (table, shift) in [(&NORMAL, false), (&SHIFTED, true)] {
            for &b in table.iter().filter(|&&b| b != 0) {
                let k = key_for_char(b as char).unwrap();
                let legend = if k.shift { SHIFTED[idx(k)] } else { NORMAL[idx(k)] };
                assert_eq!(legend, b, "'{}' (shift table {shift})", b as char);
            }
        }
        assert_eq!(key_for_char('0'), Some(MatrixKey::at(35, false)), "unshifted legend wins");
        assert_eq!(key_for_char('"'), Some(MatrixKey::at(59, true)));
        assert_eq!(key_for_char('\t'), None);
        assert_eq!(key_for_char('ä'), None);
    }

    #[test]
    fn host_keys() {
        assert_eq!(host_key(KeyCode::KeyA), key_by_name("a"));
        assert_eq!(host_key(KeyCode::Digit0), key_by_name("0"));
        assert_eq!(host_key(KeyCode::ArrowUp), Some(MatrixKey { row: 0, col: 7, shift: true }));
        assert_eq!(host_key(KeyCode::ArrowLeft), Some(MatrixKey { row: 0, col: 2, shift: true }));
        assert_eq!(host_key(KeyCode::Backspace), key_by_name("del"));
        assert_eq!(host_key(KeyCode::F8), Some(MatrixKey { row: 0, col: 3, shift: true }));
        assert_eq!(host_key(KeyCode::AltLeft), Some(MatrixKey { row: 7, col: 5, shift: false }));
        assert_eq!(host_key(KeyCode::ShiftRight), Some(MatrixKey { row: 6, col: 4, shift: false }));
        assert_eq!(host_key(KeyCode::Escape), key_by_name("runstop"));
        assert_eq!(host_key(KeyCode::F12), None, "menu button belongs to the window");
        assert_eq!(host_key(RESTORE_KEY), None, "RESTORE is not a matrix key");
        assert_eq!(host_key(KeyCode::Semicolon), key_by_name(":"));
        assert_eq!(host_key(KeyCode::Quote), key_by_name(";"));
        assert_eq!(host_key(KeyCode::Slash), key_by_name("/"));
        assert_eq!(host_key(KeyCode::Minus), key_by_name("+"));
        assert_eq!(host_key(KeyCode::Equal), key_by_name("-"));
        assert_eq!(host_key(KeyCode::BracketLeft), key_by_name("@"));
        assert_eq!(host_key(KeyCode::BracketRight), key_by_name("*"));
        assert_eq!(host_key(KeyCode::Backslash), key_by_name("uparrow"));
        assert_eq!(host_key(KeyCode::Backquote), key_by_name("larrow"));
        assert_eq!(host_key(KeyCode::F9), key_by_name("="));
        assert_eq!(host_key(KeyCode::F10), key_by_name("pound"));
    }

    /// Token of a C initializer list: a char literal, or the bare identifier / number.
    fn c_tokens(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut it = body.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\'' => {
                    let mut lit = String::new();
                    while let Some(c) = it.next() {
                        match c {
                            '\\' => lit.push(it.next().unwrap()),
                            '\'' => break,
                            _ => lit.push(c),
                        }
                    }
                    out.push(format!("'{lit}'"));
                }
                c if c.is_ascii_alphanumeric() || c == '_' => {
                    let mut id = c.to_string();
                    while let Some(&n) = it.peek().filter(|n| n.is_ascii_alphanumeric() || **n == '_') {
                        id.push(n);
                        it.next();
                    }
                    out.push(id);
                }
                _ => {}
            }
        }
        out
    }

    /// Cross-check the embedded tables and names against firmware keyboard_c64.cc.
    #[test]
    fn tables_match_firmware_source() {
        let fw = std::env::var_os("UE2_FIRMWARE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| crate::repo_root().join("firmware/1541ultimate"));
        let path = fw.join("software/io/c64/keyboard_c64.cc");
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {} not found (set UE2_FIRMWARE)", path.display());
            return;
        };
        let specials = [
            ("KEY_BACK", "del"),
            ("KEY_INSERT", "inst"),
            ("KEY_RETURN", "return"),
            ("KEY_RIGHT", "right"),
            ("KEY_LEFT", "left"),
            ("KEY_F1", "f1"),
            ("KEY_F2", "f2"),
            ("KEY_F3", "f3"),
            ("KEY_F4", "f4"),
            ("KEY_F5", "f5"),
            ("KEY_F6", "f6"),
            ("KEY_F7", "f7"),
            ("KEY_F8", "f8"),
            ("KEY_DOWN", "down"),
            ("KEY_UP", "up"),
            ("KEY_HOME", "home"),
            ("KEY_CLEAR", "clr"),
            ("KEY_BREAK", "runstop"),
        ];
        let table_tokens = |table: &str| {
            let start = src.find(&format!("{table}[] = {{")).expect(table);
            let body = &src[start..];
            c_tokens(&body[body.find('{').unwrap() + 1..body.find("};").unwrap()])
        };
        let normal = table_tokens("keymap_normal");
        for (table, ours) in [("keymap_normal", &NORMAL), ("keymap_shifted", &SHIFTED)] {
            let toks = table_tokens(table);
            assert_eq!(toks.len(), 65, "{table}: 64 keys + terminator");
            for (i, t) in toks.iter().take(64).enumerate() {
                if let Some(lit) = t.strip_prefix('\'') {
                    assert_eq!(ours[i] as char, lit.chars().next().unwrap(), "{table}[{i}]");
                } else if let Some((_, name)) = specials.iter().find(|(k, _)| k == t) {
                    assert_eq!(ours[i], 0, "{table}[{i}] is {t}");
                    // A key with the same code in both tables (KEY_BREAK) needs no SHIFT.
                    let shift = normal[i] != *t;
                    assert_eq!(key_by_name(name), Some(MatrixKey::at(i as u8, shift)), "{t}");
                } else {
                    assert_eq!(ours[i], 0, "{table}[{i}] = {t} is not a character");
                }
            }
        }
        // modifier_map: left shift 15, right shift 52, control 58, C= 61 (keyboard_c64.cc:16-25).
        let start = src.find("modifier_map[] = {").unwrap();
        let body = &src[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find("};").unwrap()];
        let body: String = body.lines().map(|l| l.split("//").next().unwrap()).collect();
        let mods: Vec<(usize, String)> =
            c_tokens(&body).into_iter().enumerate().filter(|(_, t)| t != "0x00").collect();
        let expect =
            [(15, "0x01", "lshift"), (52, "0x01", "rshift"), (58, "0x04", "ctrl"), (61, "0x02", "cbm")];
        assert_eq!(mods.len(), expect.len());
        for ((i, t), (ei, et, name)) in mods.iter().zip(expect) {
            assert_eq!((*i, t.as_str()), (ei, et));
            assert_eq!(key_by_name(name), Some(MatrixKey::at(ei as u8, false)));
        }
    }
}
