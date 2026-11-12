# S45 — The MultiCore host's video: every HDMI mode, the mode request, the scaled pixel layer, a chosen screen

**Status:** DONE (2026-11-12). T1-T7 and T9 pass in `cargo test`. The MultiCore session ran T8 with its UI harness
(`tests/ui`, its stick and SD fixtures), and it passed; see T8 below.
**Repo:** UE2 (u64-emulator). Hardware and software reference: TRX64U-MultiCore `main` at bdf659ee8. The host/ tree is
frozen (`contract/host_pin.txt`).
- `host/hdmi_modes.vh`: the rows.
- `host/hdmi_timing.v`: the counters and the active area's origin.
- `host/mode_req.v`: the mode request.
- `host/pix_layer.v`: the pixel layer, including `pix_wrec`.
- `host/overlay.v` and `host/c64_text.v`: the box and the text layer in every mode.
- `host/vic_scaler.v`: the C64 on the 1080p rows only.
- `app/video/edid.cc` and `videomode.cc`: what the apps read.
- `host-app/software/hostapp/hostvideo.cc` and `hostvideo_board.cc`: the host's choice of mode.
**Origin:** the MultiCore session, 2026-11-11. ue2emu always showed a 1920 × 1080 frame with the 960 × 540 menu
canvas unscaled in its top-left quarter. `--handover video=480p` changed nothing. The session needs the host app and
the core app at 480p and 720p as on the board, so its UX reviews take their pictures from the emulator. Owner, the
same day: "Wenn MultiCore Fehler meldet -> abklären mit der Session, umsetzen." The plan was agreed with the session.
**Depends on:** S37 (the pixel layer) and S44 (`--core`, `--icap`, the hand-over page).

---

## §1 The problem

ue2emu modelled TRX64-Ultimate's host only (`designs/c64_basic`, S37):
- a fixed 1080p frame;
- a pixel layer scaled by S from PX_CTRL b5:4.

MultiCore's host differs (T164):
- It runs one of nine HDMI modes.
- An app asks for the mode in the overlay window at 0x10144F00.
- The pixel layer is shown at an output size of its own (PX_OSIZE), nearest or bilinear.
- The box, the text layer and the C64 follow the mode's active area.

The apps write PX_OSIZE and leave b5:4 at 0, so S was 1, and nothing followed the mode.

## §2 Which host

The MultiCore host's video is modelled when the run is a MultiCore one: `--icap` (the host app) or `--core` (a core
app). Both exist only there. Any other `--trx-io` run keeps TRX64-Ultimate's host, unchanged.

## §3 The rows (hdmi_modes.vh)

Row m = family × 6 + hdmi, where family 0 is 50 Hz and family 1 is 60 Hz.

| Row | Mode | Active | Raster | Pixel clock (Hz) | Locked |
|---|---|---|---|---|---|
| 0 | 576p 50 | 720 × 576 | 864 × 625 | 27,067,117 | no |
| 1 | 720p 50 | 1280 × 720 | 1980 × 750 | 74,434,572 | no |
| 2 | 1080p 50 | 1920 × 1080 | 2640 × 1125 | 148,869,143 | yes |
| 6 | 480p 60 | 640 × 480 | 800 × 525 | 25,125,345 | no |
| 7 | 720p 60 | 1280 × 720 | 1650 × 750 | 74,035,605 | no |
| 8 | 1080p 60 | 1920 × 1080 | 2200 × 1125 | 148,071,211 | yes |
| 9 | 800 × 600 | 800 × 600 | 1052 × 650 | 41,051,794 | no |
| 10 | 1024 × 768 | 1024 × 768 | 1344 × 806 | 64,995,343 | no |
| 11 | 1280 × 1024 | 1280 × 1024 | 1688 × 1052 | 106,548,910 | no |

Rows 3, 4 and 5 do not exist.

The active area starts B = hfp + hsync + hbp pixels into a line. It starts Bv = vfp + vsync + vbp lines into a frame
(hdmi_timing.v `xact0`, `yact0`). A frame starts at cy 0, the first of the Bv blank lines.

## §4 The mode request (mode_req.v), 0x10144F00-0x10144F09

**Write 0x4F00:** the request. A value is refused when bits 7:4 are set, when it is above 11, or when it is not a
row. A refused request changes nothing.
- A C64-type core (c64_type 1) is held to its family: the family part is replaced by the System Mode's family
  (`fam_v60`). Only a locked row is accepted.
- A frame-buffer core takes the request as it is.

**Reads:**
- 0x4F04: 0x48.
- 0x4F05: 1.
- 0x4F06: flags, from bit 7 down: {refused, 0, 0, c64_type, row in use is locked, running, pix_locked, gt_ready}.
- 0x4F07: the running row.
- 0x4F08: requests written.
- 0x4F09: requests processed.

**Model:** a request is taken at once. 0x4F09 equals 0x4F08 after it. The new row runs with running, pix_locked and
gt_ready set (the device sets them about 8 clk50 cycles later, plus the relock). The rest of 0x4F00-0x4FFF reads 0
and ignores writes. The overlay's other ranges are unchanged.

**c64_type is 0:** top.v:1398-1402 ties it to 0, because no DFX core ships on the VIC path (T164 Q4). Every request
is therefore taken as it is, and flag b4 reads 0. The lock rule above is not modelled; it would only be reached with
c64_type 1, which the host's top level never drives.

**The row at power on** is 2 (mode_req's reset value). With `--handover video=`, the row is the hand-over word's:
`video_mode_index(hdmi, sys)`, which is 6 + hdmi for NTSC60 and hdmi otherwise. The host left that mode running
when it started the core. A word with no mode, or one naming no row, keeps 2.

## §5 The frame

**Size:** the output picture is the running row's active area, hact × vact.

**Frame starts and PX_STATUS vertical blank (b1):**
- On a locked row (1080p) while a VIC counts frames (the C64 core, no `--core` profile), the frame follows the VIC
  (the `align` pulse), as in S37.
- Otherwise the frame runs free at the row's rate: htot × vtot pixel clocks per frame.
- Vertical blank holds for the first Bv lines.

**The C64:** vic_scaler draws it only on a 1080p row (`en`), as in S37. On every other row it is black.

**The box (overlay.v):** centred on the active area.
- SY is the largest of 4, 3, 2, 1 with h · SY ≤ vact.
- SX is the largest with w · SX ≤ hact, at most 2 · SY.
- If the box is wider or taller than the active area, it starts at the area's left or top edge and is cut off on
  the far side.

**The text layer (c64_text.v):**
- ST = min(st_h, st_v):
  - st_h is 4 for hact ≥ 1280, 3 for ≥ 960, 2 for ≥ 640, else 1;
  - st_v is 4 for vact ≥ 800, 3 for ≥ 600, 2 for ≥ 400, else 1.
- The text area is 320 ST × 200 ST, centred in the active area.

## §6 The pixel layer (pix_layer.v)

**Registers:** as in S37, plus the following.
- **PX_OSIZE 0x0A8:** {height 16, width 16} output pixels. Default 1080, 1920.
- **PX_CTRL:**
  - b5:4 is not read.
  - b6 turns smoothing on.
  - b3:1 = 2 selects the 32-bit format: B 7:0, G 15:8, R 23:16. This format has no colour key.

**Frame start:** the layer is on when all of the following hold:
- b0 is set, and the format is 1 or 2;
- width, height, OW and OH are each non-zero;
- OW and OH are each below 2048;
- x0 < hact and y0 < vact.

Width is clamped to 960 and height to 1080. The picture starts at (x0, y0) of the active area.

**Nearest sampling:** output pixel x of the layer shows source pixel ⌊x · W / OW⌋. Output line y shows source line
⌊y · H / OH⌋. There are OW × OH output pixels, clipped at the active area.

**Smoothing (b6):**
- Horizontal smoothing applies when W ≤ OW. Vertical smoothing applies when H ≤ OH and, in format 2, W ≤ 512.
- At position p = x · W / OW, let i = ⌊p⌋ and q = ⌊1024 · frac(p)⌋.
- The vertical pass runs first, on lines m and m + 1. The last line pairs with itself.
- Per channel: V = (T·2048 + (B − T)(2qv + 1)) >> 5, which is in 1/64 steps.
- Then horizontally on pixels i and i + 1. The last pixel pairs with itself.
- Per channel: out = (A·2048 + (B − A)(2qh + 1) + 65536) >> 17, kept to 8 bits.
- An axis that is not smoothed takes its nearest sample.

**Colour key (format 1):** the key decides on the nearest pixel, ⌊p⌋ of line m. A keyed pixel is transparent.

**Blend:** with the global alpha, as in S37.

TRX64's host (S37) is the same sampler with OW = S · W and OH = S · H, nearest only: ⌊x · W / (S · W)⌋ = ⌊x / S⌋.

## §7 The screen (`--screen`)

The emulated monitor's EDID decides what the host app puts up, because `hv_start` reads it (`video_read_edid`, I2C
channel HDMI, 0xA0).

**`--screen MODE`:**
- MODE is one of `576p50`, `480p60`, `720p50`, `720p60`, `1080p50`, `1080p60`, `800x600`, `1024x768`, `1280x1024`.
- It gives an HDMI monitor that offers that mode only.
- The base block holds the mode's detailed timing (the row's raster and pixel clock) and no established or standard
  timings.
- The CEA block names the mode's VIC as native (none for the PC rows) and holds the HDMI VSDB.
- `edid.cc` reads that mode, and `video_modes_default` picks it.

**`--screen none`:** no monitor. The hot-plug line is low and nobody answers at 0xA0.

**Without the flag:** the monitor stays as it was. That is the 1920 × 1080@60 HDMI monitor of S04, which Gideon's
firmware's Auto mode needs.

This deviates from what was agreed with MultiCore ("no EDID without the flag"). Without the flag, MultiCore's host
reads the base block of that monitor, which gives rows 8, 9 and 10, and `video_modes_default` picks 1080p60.

## §7a Pictures and the monitor (after T8)

**`png`:** a `png` has the output's size, hact × vact of the running mode, or 1920 × 1080 on TRX64's host. The
composite may still be built at half size where every picture halves exactly (S37). The `png` then doubles it, which
is exact. Before, such a `png` came out at half size (MultiCore, T8 item 5).

**The monitor's device views:** `itu`, `cart`, `joy`, `flash`, `sd`, `usb`, `net`, `audio`, `trx` and `dir` read
the machine only. They answer without a C64, as in a MultiCore run with `--c64 none` (T8 item 6). Every other verb
still needs `--c64 trx64`.

## §8 Tests

- T1: mode_req register reads, refusal, counters (unit).
- T2: the row at power on from the hand-over word (unit).
- T3: the frame period and vertical blank per row; the C64 on a locked row follows the VIC (unit).
- T4: the layer's nearest sampling equals ⌊x · W / OW⌋. TRX64's S path gives the same pictures as before (render
  unit; the S37 tests unchanged).
- T5: smoothing against the formulas of §6, including the edge pixel and edge line (unit).
- T6: the box, the text area and the C64 at 480p, 720p and 1080p (render unit).
- T7: the `--screen` EDIDs: checksums, a detailed timing that `row_of_timing`'s rules map to the row, the CEA native
  VIC (unit).
- T8: MultiCore's kit: coreapp with `--handover video=ntsc60:480p` shows a 640 × 480 picture with the compact canvas.
  The host app with `--screen 720p60` comes up at 1280 × 720. Run with the kit, as S44's smokes are.
  - Passed (MultiCore, 2026-11-12). The core app (A1200 and VC-20) at `ntsc60:480p` gives 640 × 480 with the compact
    canvas, nothing cut. At `pal50:720p` it gives 1280 × 720, with PX_POS 0x005A00A0 and PX_OSIZE 0x021C03C0 (960 × 540
    centred). The host app at `--screen 720p60` reads "1280 x 720 (from the screen, EDID read)"; at `480p60` it gives
    640 × 480.
  - The start-up splash keeps PX_OSIZE 1920 × 1080 for about 1.9 s before the menu is placed. That is MultiCore's bug
    (run.cc, their T210), not the emulator's.
  - WiFi "No module" on F8 is MultiCore's too: only the C64 core registers the wifi states. The ESP32 stub answers.
- T9: a `png` of a half-size composite is the output's size (render unit; `smoke-trx.sh` and
  `smoke-trx-freeze.sh` check 1920 × 1080); the monitor's `trx` answers without a C64 (monitor unit).
