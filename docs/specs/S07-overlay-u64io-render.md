# S07 — Overlay device, U64 IO page (keyboard matrix), renderer

**Owns:**
- `crates/ue2-core/src/devices/{overlay.rs,u64io.rs}`
- `crates/ue2-core/src/render.rs`
- `crates/ue2-core/src/host.rs` (additive fields only)

**Reads:** `docs/hw/05-ui-overlay-input.md` (all: renderer B, keyboard C, T0/T1), `docs/hw/03-board-init.md` (U64 IO page), `docs/hw/00-memory-map.md` §1b rows 0x10100400-0F and 0x10140000-0x10148003, §2 C18-C20, C32-C36; firmware `software/io/overlay/*`, `software/io/c64/keyboard_c64.cc`, `software/userinterface/screen.cc`, `roms/chars.bin`, `fpga/.../font_pkg.vhd` if referenced

## Scope

- **`Overlay`** device on `0x10140000-0x1014FFFF`:
  - chargen register latches (read 0 unless doc 05 says otherwise);
  - 4 K screen RAM `0x10141000` and 4 K colour RAM `0x10142000` with read-back (C33);
  - HDMI timing `0x10144000` stored;
  - palette RAM `0x10145000` (16 × RGBx);
  - cropper `0x10148000` stored;
  - `snapshot(now_ms) -> DisplaySnapshot`.
- **`U64Io`** device on `0x10100400-0x101004FF`, T0 constants and T1 input:
  - HDMI_REG read 0x04 (HPD); RESTORE 0x00; CART_DETECT 0x03; BLACKBOARD 0x01.
  - LED/PWM/ETHSTREAM latches with read-back.
  - Keyboard scan: COL latch at `0x1010040A`; ROW `0x1010040B` is a pure function of the COL latch and the
    pressed-key set, idle 0xFF, stable between reads (C36, C18).
  - JOY `0x10100406` = joystick lines (idle 0xFF).
  - Host API `set_key(row, col, down)`, `set_joystick(lines)`.
  - The row/col convention must match `keyboard_c64.cc` (keymap index = row*8+col or as the scanner really
    works). Document it in a doc comment; S08's keymap depends on it.
- **`render.rs`:**
  - `Renderer::new(font)`: font bytes from `roms/chars.bin` as doc 05 specifies.
  - `render(&snap, &mut Vec<u32>) -> (w, h)`, 0x00RRGGBB:
    - geometry and char size from the latched registers;
    - colours from the palette;
    - transparency and visibility (reg 0x0D bit 7) as doc 05;
    - when invisible, a dark background so the window is not empty.
  - `text_dump(&snap) -> String`: rows of the active text area, screen codes mapped to printable ASCII,
    trailing spaces trimmed. Used by headless tests to read menus.
- Add fields to `DisplaySnapshot` if the renderer needs them (e.g. timing regs); report additions.

## Tests

- Screen/colour RAM read-back; palette write/read.
- Snapshot content.
- Keyboard: press (row, col) → the scan pattern shows the bit only for the matching COL; release → idle 0xFF.
- Renderer: a synthetic snapshot with a known char and colour produces the expected pixels at the expected
  cell.
- `text_dump` of a synthetic "HELLO" row.

## Acceptance

`cargo test -p ue2-core overlay u64io render` passes.
