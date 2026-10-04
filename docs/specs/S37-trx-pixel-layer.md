# S37 — The TRX64-Ultimate pixel layer (LVGL UI)

**Status:** phase A built (2026-10-04), local branch `s37-trx-pixel-layer`, not pushed. See "As built" at the end.

**Why:** TRX64-Ultimate (Alex's own FPGA for the C64U, `/Users/alex/Development/TRX64-Ultimate`, Gitea
`TREX/TRX64-Ultimate`) adds an RGB565 pixel layer over the C64 picture. Its app fork (`software/ultimate-app`,
branch `trx64/main`) runs LVGL v9.6.0 on it (TRX spec T24). UE2 models the layer, so that UI can be built and tested
on the Mac before it reaches a board.

**Reads** (TRX64-Ultimate, the hardware is the truth):
- `designs/c64_basic/trx_io.v`: page 0, the interrupt.
- `designs/c64_basic/pix_layer.v`: page 6.
- `designs/c64_basic/vic_scaler.v`: where the C64 sits in 1920 × 1080.
- `software/ultimate-app/software/io/c64/trx_io.h`, `software/lvgl_port/trx_lv_port.cc`, `software/io/c64/trx_ui.cc`:
  the app side.
- `docs/specs/T24-lvgl-ui.md`, `T12-freeze-overlay.md`: what the app does with it.

**Owns:**
- `crates/ue2-core/src/devices/trx.rs` (new): the IO block and the layer's registers.
- `crates/ue2-core/src/render.rs`: the composite.
- `crates/ue2-core/src/devices/c64.rs`, `drives.rs`: drive B with `--trx-io`.
- `crates/ue2emu`: `--trx-io`.

## 1. What the device has

**IO block at 0x101A0000**, sixteen 4 KB pages, byte-wide (`trx_io.v`). UE2 builds page 0 and page 6; every other
page reads 0 and ignores writes.

| Page 0 | Access | Contents |
|---|---|---|
| 000-003 `ID` | R | `"TRX2"` |
| 004 `CAPS` | R | the bitstream reads `$FF`. UE2 reads `$C0`: b6 page 6 video, b7 the layer is RGB565. Everything else is off, so the app's paths for the state bus, engine, freeze session and cartridge stay off |
| 005 `CAPS2` | R | `$00` (the bitstream reads `$3F`) |
| 008-00B `BUILD` | R | 0 |
| 010 `IRQ_EN` | RW | cause mask |
| 014 `IRQ_CAUSE` | R/W1C | b6 frame start (vsync), b7 flip done; the other bits stay 0 |

The interrupt is `(IRQ_EN & IRQ_CAUSE) != 0` on **ITU high IRQ 2**.

**Page 6 at 0x101A6000** (`pix_layer.v`). 32-bit registers, little endian:
- Writing byte 0-2 stages them; writing byte 3 commits all four.
- Reading byte 0 latches the register, and bytes 1-3 come from the latch.

| Off | Name | Default | Meaning |
|---|---|---|---|
| 060 | `STILL_CTRL` | 1 | b0 capture (§5) |
| 064 | `STILL_BASE` | `0x05050000` | slot 0; slot 1 at +64 KB |
| 068 | `STILL_STATUS` | 0 | b0 valid, b1 last slot, b31:8 frames |
| 080 | `PX_CTRL` | `0x22` | b0 on, b3:1 format (1 = RGB565, anything else = off), b5:4 scale (0 and 1 = 1, then 2, 3) |
| 084 | `PX_FRONT` | 0 | DDR byte address of the shown buffer, 16-byte aligned |
| 088 | `PX_BACK` | 0 | write: show this buffer from the next frame start (the flip) |
| 090 | `PX_SIZE` | 540 / 960 | {h 16, w 16}, w ≤ 960, h ≤ 1080 |
| 094 | `PX_STRIDE` | 1920 | bytes per line, multiple of 16 |
| 098 | `PX_POS` | 0 | {y0 16, x0 16} in output pixels |
| 09C | `PX_GALPHA` | 255 | global alpha |
| 0A0 | `PX_KEY` | 0 | b15:0 RGB565 key, b16 key on |
| 0A4 | `PX_STATUS` | | b0 flip pending, b1 vertical blank, b15:8 underruns, b31:16 frames |

**Timing.** Output frames are locked to the VIC, one output frame per VIC frame.
- At each frame start:
  - the committed settings take effect;
  - a pending flip moves `PX_BACK` into `PX_FRONT`, and cause b7 is raised;
  - cause b6 is raised.
- A write to `PX_FRONT` shows from the next frame start, with no flip and no b7: the fetch takes the front address
  at the frame start (`pix_layer.v`, `la <= c_front`).
- On the device the frame start is 45 output lines before the first visible line. UE2 raises it when the VIC
  finishes a frame.

**Picture.**
- **Output:** 1920 × 1080. The C64 is a 400 × 270 window of the VIC (X0 96, Y0 16) shown 4×, so 1600 × 1080 with
  160-pixel black bars left and right (`vic_scaler.v`). For 60 Hz System Modes the window is 400 × 240 at 4× and 4.5×.
- **Stacking:** C64, then Gideon's overlay, then the layer, then the freeze menu's text.
- **The layer:** w × h source pixels, each S × S output pixels, top left at (x0, y0).
  - Each RGB565 channel widens to 8 bits by repeating its top bits.
  - A pixel equal to the key, with the key on, is transparent.
  - Every other pixel blends per channel: `out = bg + ((fg − bg) · a) >> 8`, with `a = G + G[7]` and an arithmetic
    shift. G = 255 gives fg and G = 0 gives bg, both exactly.
  - No per-pixel alpha.

## 2. In UE2

- **`--trx-io[=auto|on|off]`** (TOML `trx-io = "auto"`, `true` or `false`), default auto. The flag is the board's
  bitstream, which the firmware cannot decide on a real board; auto is a convenience on top.
  - auto is on when the loaded image is TRX firmware: the ELF symbol `trx_lv_open` (trx_lv_port.cc) when the
    image has symbols, else the string `[TRX UI]` (trx_ui.cc) in a loaded segment, which covers `.app` and `.ue2`.
    Stock firmware has neither.
  - One stderr line says which way it went, e.g. `trx-io: on (TRX firmware: trx_lv_open)`, `trx-io: off (stock
    firmware)`.
  - On installs the block and makes high IRQ 2 its line. Off leaves 0x101A0000 unmapped, as on Gideon's
    bitstream, where the app reads ID 0 and leaves the layer alone; `--trx-io=off` runs TRX firmware that way.
- **High IRQ 2:** with `--trx-io`, drive B's WD177x does not drive it. A 1581 at drive B is refused with a message.
  The TRX bitstream has no drive B (`trx_io.v`: "we have no drive B").
- **Buffers:** the layer reads the guest DDR at `PX_FRONT`, the same memory the RISC-V writes. The fetch keeps
  address bits 26:0; a pixel past UE2's 64 MB reads 0 (black, or transparent with key 0 on). There are no
  underruns, so the count stays 0.
- **Composite:** while the layer is on (`PX_CTRL` b0, format 1, w, h ≠ 0), the window, `png` and screenshots show
  the 1920 × 1080 composite of §1, scaled to the window as today. While it is off, the picture path is the one we
  have now, with no extra cost.
  - When S = 2, x0 and y0 are even (LVGL's case) and no overlay window is up, the composite is built at
    960 × 540. Each 50 Hz C64 pixel is then exactly 2 × 2. The overlay's text is drawn 1:1 in the output mode, so
    with a window up the composite stays at 1920 × 1080.
  - Otherwise it is built at 1920 × 1080.
- **Not changed:** the UDP video stream (S24) carries the VIC picture, as before.
- **Monitor:** `monitor trx` shows ID, CAPS, IRQ_EN/CAUSE and the PX_* registers.

## 3. Phases

| Phase | Content |
|---|---|
| A | Page 0 and page 6 registers, the IRQ, the flip, the composite. Unit tests. A smoke with a `trx64/main` ELF: the splash shows "TRX64-Ultimate", fades, and the log says `[TRX UI] splash done; UI parked` |
| B | Still capture (§5) for T12's freeze renderer and T24 phase 3 |
| C | Picture-in-picture (T24 §14) once the FPGA has a register map for it |

## 4. Checks

- **Unit:** byte staging and commit; reading through the byte-0 latch; a flip lands only at the frame start and
  raises b7; W1C on `IRQ_CAUSE`; the IRQ line follows `IRQ_EN & IRQ_CAUSE`.
- **Composite:** the RGB565 widening; the key; the blend at alpha 0, 128 and 255 against values from the Verilog's
  arithmetic; scale 1, 2 and 3; the layer clipped at 1920 × 1080; the C64 at 4× with its bars.
- **Auto:** the TRX ELF turns it on, the stock ELF off; an explicit on or off wins.
- **Firmware** (`scripts/smoke-trx.sh`): the `trx64/main` app boots under `--trx-io --board c64u`, and the splash is in a `png` 1 s after boot
  and gone at 5 s. A run without `--trx-io` boots the same ELF with no splash, the way it does on Gideon's bitstream.

## 5. Still capture (phase B)

Every VIC frame, the 400 × 270 window (4-bit colour indices, two pixels per byte, the first in the low nibble, rows of
200 bytes, 54,000 bytes) goes into the slot that does not hold the last complete frame. Slot 0 is at `STILL_BASE`,
slot 1 at +64 KB. `STILL_STATUS` then names that slot and counts the frame. With `STILL_CTRL` b0 = 0 nothing is
written. Phase A keeps `STILL_STATUS` at 0 and writes nothing.

## 6. Open

- Where the resident UI's buffers live (T24 open question 2). This does not matter to UE2: it reads whatever
  `PX_FRONT` says.
- Picture-in-picture: registers not designed yet (T24 §14).
- The `trx64/main` app on UE2 beyond the layer:
  - With CAPS = `$C0` it touches pages 0 and 6 only (As built).
  - The boot ROM and flash layout (T15) do not matter: UE2 loads the ELF.
- The fade: the smoke sees the `PX_GALPHA` writes but checks no intermediate picture.

## As built (phase A)

- **Frame start:** `C64Port` publishes TRX64's VIC frame count after each sync; the block polls it every 1 ms, so
  the frame start comes up to 1 ms after the VIC frame ends. Without a C64 that counts frames it comes every PAL
  frame (19.95 ms).
- **`PX_STATUS` b1** holds for 0.8 ms after the frame start (45 lines of 2688 clocks at 151.58 MHz).
- **C64 picture in the composite:**
  - colours from `vic_scaler.v`'s own palette, not the firmware's `C64_PALETTE`;
  - TRX64's canvas is 384 wide, the window 400: the columns outside it repeat the canvas's edge pixel (the border);
  - the window is mapped through the text area's first pixel (raster x 136, `x_text`) and the canvas's first line;
  - a 60 Hz composite at 960 × 540 takes every second output line of the 5, 4, 5, 4 pattern.
- **Gideon's overlay** is placed as UE2 places it without the layer (X_ON/Y_ON in the output mode, half strength).
  `overlay.v` instead centres the box and scales it; not modelled.
- **Drive B:** the firmware probes DRIVETYPE 2 at boot (c1541.cc:119-123). With `--trx-io` the write keeps the
  1541, so the firmware offers drive B as a 1541 only, and the log says so once.
- **Firmware smoke** (`scripts/smoke-trx.sh`, trx64/main 029da8ef): "LVGL 9.6.0 on the pixel layer" at about
  1.5 s, the splash in a 960 × 540 png, `PX_GALPHA` written 17 times (the fade), "splash done; UI parked" at
  about 4.5 s with the layer off. In the first 6 s the app touches page 0 (ID, CAPS, IRQ_EN = b7) and page 6
  (`PX_CTRL`, `PX_FRONT`, `PX_SIZE`, `PX_STRIDE`, `PX_POS`, `PX_GALPHA`, `PX_KEY`); no other page, no unmapped
  access, no hang. It never writes `PX_BACK`, so no flip. With `--trx-io=off` it reads ID byte 0 once, gets 0 and
  boots to READY with no splash.
