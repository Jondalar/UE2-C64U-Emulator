# S07 — Overlay device, U64 IO page (keyboard matrix), renderer

**Status:** built.

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
  - JOY `0x10100406` = the lines of the control port bit 0 of the last write selects (idle 0xFF); S36.
  - Host API `set_key(row, col, down)`; the joystick lines come from `C64Port` (S36).
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

### The output mode (issue #3, 2026-09-20)

`DisplaySnapshot` carries the HDMI timing registers, and the renderer composes what the device puts out rather
than a picture of its own:

- Canvas = the active area the firmware programmed (`VID_HACTIVE`/`VID_VACTIVE` with the low bits from
  `VID_HREPETITION`, hdmi_scan.cc:6-27).
- The C64 frame is stretched over it, as the device's scaler does.
- The overlay is a 40×25 window drawn 1:1 at `X_ON - (hsync + hbackporch)`, `Y_ON - (vsync + vbackporch)` and
  clipped at the edges — right of centre and below the middle in every mode (docs/hw/05, the table of
  `DetermineOverlaySettings`). It is never a full screen.
- Transparent cells show the C64 through, which is what the hardware does (docs/hw/05 OQ 3, answered by a photo).
- Until the firmware programs the timing, and without a C64, the old behaviour stands: canvas covers frame and
  grid, both centred.

`render::looks_like_c64_char_rom` recognises a roms directory whose `chars.bin` is the 4 KB C64 character ROM
instead of the firmware's 2 KB overlay font ('A' at glyph 1 rather than 0x41); `ue2emu` warns instead of drawing a
menu in graphics symbols.

## Tests

- Screen/colour RAM read-back; palette write/read.
- Snapshot content.
- Keyboard: press (row, col) → the scan pattern shows the bit only for the matching COL; release → idle 0xFF.
- Renderer: a synthetic snapshot with a known char and colour produces the expected pixels at the expected
  cell.
- `text_dump` of a synthetic "HELLO" row.
- With PAL SD timing and X_ON 386 / Y_ON 307 the window starts at (254, 263) of a 720×576 canvas, a transparent
  cell shows the C64, and a window placed past the right edge is clipped.
- The firmware's `chars.bin` is not taken for a C64 character ROM, and `characters.901225-01.bin` is.

## Acceptance

`cargo test -p ue2-core overlay u64io render` passes.

## The C64 picture is cropped, scaled and placed — not stretched

The device does not put the VIC picture across the whole screen. `SetVicCrop` takes a window out of the frame, two
fixed-ratio scalers blow it up, and `x_offset` places the result (hdmi_scan.cc:45-163):

| 1080p | value |
|---|---|
| `SetVicCrop(8, 9, 384, 270)` | crop at (8, 9), 384x270 — the registers hold the size halved |
| `hscaler = 0x0C` | 15/4, so 384 becomes 1440 |
| `vscaler = 0x08` | 4/1, so 270 becomes 1080 |
| `x_offset = 240` | 240 + 1440 + 240 = 1920: pillarboxed in the active area |

The scaler codes are ratios, not sizes: the firmware's own table gives each code's output for a 384-pixel and a
400-pixel crop, and for 240 and 270 lines, and every pair is the same factor. Four vertical entries are rounded in
the 240 column (533, 686, 1067, 1371) and exact in the 270 one, which is where 20/9, 20/7, 40/9 and 40/7 come
from.

Until this was read out of the firmware the renderer stretched the frame over the whole canvas, which at 1080p made
the picture 1920 wide instead of 1440 — a third too wide, and the reason a capture put beside a photo of real
hardware did not line up.

Only the crop's size is taken over. Its origin counts in the FPGA's video stream, which starts elsewhere than the
TRX64 canvas (384x272 from raster line 16, 32-pixel borders). Laid on the canvas as is, the 480p crop (8, 0) left 35
border lines above the text and 5 below it (issue #3). The crop is centred on the canvas instead: 19 lines above and
21 below at 240 lines, 34 and 36 at 270, 32 pixels left and right.
