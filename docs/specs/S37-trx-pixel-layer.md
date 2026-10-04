# S37 — The TRX64-Ultimate pixel layer (LVGL UI)

**Status:** phase A built (2026-10-04), phases B1 and B2 (the freeze, §5) built (2026-10-04), B3 (`overlay.v`'s
box, §5.10) built (2026-10-04), local branch `s37-trx-pixel-layer`, not pushed. See "As built" at the end.

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
- Phase B: `designs/c64_basic/trx_sys.v` (page 0 from 0x100, page 5), `freeze_resp.v` (the session),
  `c64_text.v` (the menu text), `overlay.v`, `top.v` (the pause wiring); app `software/io/c64/trx_video.cc`,
  `c64.cc` (`freeze()`, `unfreeze()`, `stop()`, `init_io()`).

**Owns:**
- `crates/ue2-core/src/devices/trx.rs` (new): the IO block and the layer's registers.
- `crates/ue2-core/src/render.rs`: the composite.
- `crates/ue2-core/src/devices/c64.rs`, `drives.rs`: drive B with `--trx-io`.
- `crates/ue2emu`: `--trx-io`.
- Phase B: `crates/c64-bridge` (the machine pause, the chips' side-effect-free view), `devices/trx_session.rs` (new:
  the session), `devices/c64.rs` (the pause, the session's window, the still).

## 1. What the device has

**IO block at 0x101A0000**, sixteen 4 KB pages, byte-wide (`trx_io.v`). UE2 builds page 0 and page 6; every other
page reads 0 and ignores writes.

| Page 0 | Access | Contents |
|---|---|---|
| 000-003 `ID` | R | `"TRX2"` |
| 004 `CAPS` | R | the bitstream reads `$FF`. Phase A: `$C0`, b6 page 6 video, b7 the layer is RGB565. Everything else is off, so the app's paths for the state bus, engine, freeze session and cartridge stay off. Phase B: `$E1` (§5.1) |
| 005 `CAPS2` | R | the bitstream reads `$3F`. Phase A `$00`; phase B: `$08` (§5.1, §5.8) |
| 008-00B `BUILD` | R | 0 |
| 010 `IRQ_EN` | RW | cause mask |
| 014 `IRQ_CAUSE` | R/W1C | b0 the machine pause came into effect (phase B, §5.2), b6 frame start (vsync), b7 flip done; the other bits stay 0 |

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
- **Composite:** with the block on, the window, `png` and screenshots always show the 1920 × 1080 composite of §1,
  scaled to the window as today: the bitstream has no other output, so the picture keeps one shape whether the layer
  is on or off (with it off: the C64 at 4× between the bars). Without the block the picture path is unchanged.
  - When S = 2, x0 and y0 are even (LVGL's case) and `overlay.v`'s box is off, the composite is built at
    960 × 540. Each 50 Hz C64 pixel is then exactly 2 × 2. The box's scale is 1 to 4 per axis and its corner is
    centred to the pixel (§5.10), so with the box on the composite stays at 1920 × 1080.
  - Otherwise it is built at 1920 × 1080.
- **Not changed:** the UDP video stream (S24) carries the VIC picture, as before.
- **Monitor:** `monitor trx` shows ID, CAPS, IRQ_EN/CAUSE and the PX_* registers.

## 3. Phases

| Phase | Content |
|---|---|
| A | Page 0 and page 6 registers, the IRQ, the flip, the composite. Unit tests. A smoke with a `trx64/main` ELF: the splash shows "TRX64-Ultimate", fades, and the log says `[TRX UI] splash done; UI parked` |
| B1 | The freeze looks like the device (§5.1-§5.7): CAPS `$E1`, GATE_CTRL and the machine pause, the session flag, the still capture, the C64 text layer, the window black while paused. Firmware smoke §5.9 |
| B2 | The freeze behaves like the device (§5.8): shadow I/O, commit list, PROTO, CAPS2 b3 |
| B3 | `overlay.v`'s box placement for "Overlay on HDMI" (§5.10) |
| C | Picture-in-picture (T24 §14) once the FPGA has a register map for it |
| S38 | Pages 1, 2, 3, 4, 7, the lens unit, the frame ring and Timewarp (§5.11) |

## 4. Checks (phase A; phase B: §5.9)

- **Unit:** byte staging and commit; reading through the byte-0 latch; a flip lands only at the frame start and
  raises b7; W1C on `IRQ_CAUSE`; the IRQ line follows `IRQ_EN & IRQ_CAUSE`.
- **Composite:** the RGB565 widening; the key; the blend at alpha 0, 128 and 255 against values from the Verilog's
  arithmetic; scale 1, 2 and 3; the layer clipped at 1920 × 1080; the C64 at 4× with its bars.
- **Auto:** the TRX ELF turns it on, the stock ELF off; an explicit on or off wins.
- **Firmware** (`scripts/smoke-trx.sh`): the `trx64/main` app boots under `--trx-io --board c64u`, and the splash is in a `png` 1 s after boot
  and gone at 5 s. A run without `--trx-io` boots the same ELF with no splash, the way it does on Gideon's bitstream.

## 5. Phase B: the freeze

**The setting.** "Interface Type" = "Freeze" (value 0, `[User Interface Settings]`, userinterface.cc:136, :151, :400-403). "Overlay on
HDMI" takes the menu to `overlay.v` instead (ultimate.cc:205-209). UE2 seeds "Overlay on HDMI" into a blank flash
unless `--no-overlay-ui` (flash.rs `seed_overlay_ui`), which is why it has to be switched. The menu button then calls
`C64::freeze()`.

**What the app does** (trx64/main 029da8ef):
1. `trx_video_freeze_enter()` (trx_video.cc:850-893), before the stop:
   - leaves at once without HDMI hot plug (:856; UE2's U64_HDMI_REG reads `$04`, so it goes on);
   - copies the last complete still while the machine runs (`grab_still`, :327-341);
   - draws it into a buffer, every colour mixed with the menu background: t/4 still, t = "Overlay Transparency"
     (default 25 %, t = 1; :307-321, :873);
   - waits for a frame start, switches the layer on (800 × 540, scale 2, at (160, 0), key off, alpha 255;
     :470-481) and flips onto it;
   - writes `GATE_CTRL` = FRZ (:890).
2. `stop(true)` (c64.cc:512-560): C64_STOP = 1, which now pauses the whole machine; then `goUltimax()`, which starts
   the session.
3. `backup_io()`, `init_io()` (c64.cc:1106-1145): screen `$0400`, charset `$0800`, `$D011` = `$1B`, `$D016` = `$C8`,
   VIC bank 0.
4. `trx_video_menu_on()` (:895-905): `TX_CTRL` = show. The hardware draws the text from now on.

**Leaving** (`unfreeze()`, c64.cc:1436-1494): `restore_io()` (c64.cc:1329) first clears `TX_CTRL`
(`trx_video_menu_off()`, trx_video.cc:907-919), `resume()` drops C64_STOP (the session ends, the machine runs),
`trx_video_freeze_leave()` waits for `GATE_CTRL` b8 to clear and two frame starts, then turns the layer off and writes
`GATE_CTRL` = 0 (trx_video.cc:921-942, :682-694).

### 5.1 CAPS

**`CAPS` = `$E1`**: b0 page 0 gate, b5 page 5 session, b6 page 6 video, b7 RGB565. trx_video.cc:801-803 needs exactly
these four; with fewer `init()` returns and the freeze stays Gideon's CPU-only pause with the menu on the VIC (phase
A). The other bits stay off, and with them every path that would need pages 1-4:

| Bit | What reads it in the app |
|---|---|
| b1 state bus | trx_ui.cc:219 (PC display), trx_lens_fw.cc:100 |
| b2 engine | trx_ring.cc:1994, trx_timewarp_ui.cc:410 |
| b3 debug | trx_io.cc:69 (`trx_dbg_yield`), trx_ring.cc:410, app_api_net.cc:354 |
| b4 cartridge | trx_cart.cc:81 |

**`CAPS2` = `$08`.** The bitstream returns the constant `$3F` (trx_io.v:231); what b3 means is what the app does with
it: `has_list`, which gates `set_proto()` (trx_video.cc:673-679, :825), i.e. PROTO for the commit list. B1 and B2 are
built together, so UE2 claims it (the console then says `, commit list`). The text layer is found by `TX_CTRL` b31:24
= 1, not by CAPS2 (trx_video.cc:823).

### 5.2 Page 0 from 0x100

Page 0 from 0x100 and pages 1, 2, 3, 5, 7 are `trx_sys.v`'s (trx_io.v:147-148): 32-bit registers, bytes 0-2 collect,
byte 3 commits, reading byte 0 latches, bytes 1-3 come from the latch (trx_sys.v:278-289, :379-387). These pages share
one collect buffer and one latch (trx_sys.v:191-192), apart from page 6's. A commit answers 0, which the latch then
holds (trx_sys.v:344, :384).

| Off | Name | B1 |
|---|---|---|
| 100 | `GATE_CTRL` | W b0 FRZ (trx_sys.v:313). R b0 FRZ, b8 halted, b9 gate shut (trx_sys.v:215). UE2: b8 = b9 = the pause of §5.3 is in effect (b9 is the state bus shadows following the machine; accepted as b8) |
| 104-13F | `CAP_CTRL`, `CAP_STAT`, `JTAG_PAGE`, `RESUME_PHASE`, `STOP_POS`, debug | read 0, writes ignored (S38). The RTL reads `CAP_CTRL`, `JTAG_PAGE` and the rest back; nothing the app runs with these CAPS uses them |
| 140-FFF | | read 0 (trx_sys.v:212) |

`IRQ_CAUSE` b0 is raised when the pause comes into effect (top.v:1314-1317, trx_io.v:161). Phase A kept it 0.

### 5.3 The machine pause

**Device.** `app_pause = gate_frz` (top.v:1068; the JTAG bit is not in UE2), `c64_halt_req = app_stop & app_pause`
(top.v:1071). That halt stops c64_core as a whole (top.v:1216): CPU, VIC, CIAs, SID; drive A with it (top.v:503-505);
the sound is muted (top.v:1059). Without FRZ, C64_STOP is the CPU-only pause as before (top.v:1229). `app_stop` is
C64_STOP (the debugger owner of top.v:1070 is page 3, not in UE2).

| FRZ | C64_STOP | UE2 |
|---|---|---|
| 0 | 1 | `Hold::Cpu`, as today: VIC, CIAs, SID, drive run |
| 1 | 1 | the pause |
| any | 0 | runs |

**In UE2:**
- TRX64 v0.12.3 has no hold for the whole machine (`Hold::Cpu` keeps the chips running, expansion.rs:68-75). The
  bridge gets `C64Backend::set_paused(bool)`: while paused `advance_to` runs no cycle, so the C64's cycle count,
  VIC, CIAs, SID and the drives stand. At the release the clock is re-anchored at the current time, as after a reset.
  No SID samples while paused: silence.
- The DMA window, the keys and the ROM apertures work while paused, as under `Hold::Cpu`.
- The reset line wins: reset held is `Hold::Reset`, as today.
- The device resumes the VIC only after whole frames, to keep its output lock (c64_core.v:358-365). UE2 has no lock
  and resumes at once, an accepted approximation. Nobody sees it: the app keeps the layer for two frame starts after
  the resume.
- **Frame starts go on while paused**: on the device only the VIC is gated, the output timing runs on. UE2: while
  the VIC frame count stands, a frame start every VIC frame of the current System Mode (PAL 19.95 ms), from the last
  one. The phase A poll stays for the running machine.
- **The C64 window is black while paused** (vic_scaler.v `blank` = `c64_halted`, top.v:1375). With `--trx-io` the
  composite is used while the layer is on or the pause is in effect.

### 5.4 Page 5 and the session

| Off | Name | B1 |
|---|---|---|
| 000 | `SH_CTRL` | W b1:0 `SH_MODE` (reset 1), b2 SIDEFX, b3 PROTO, b8 abort, b9 discard (trx_sys.v:335-341). R {b3 PROTO, b2 SIDEFX, b1:0 mode} (trx_sys.v:229). PROTO clears when a session ends (trx_sys.v:270). SIDEFX and PROTO are kept but act only in B2 |
| 004 | `SH_STATUS` | R b0 session, b1 commit (always 0 in UE2: the list runs within one access), b31:16 sessions started (trx_sys.v:230) |
| 008-01C | `SH_ACCESSES` … `SH_COMMIT` | §5.8; `SH_GCLK` reads 0 (UE2 has no gated clock to count) |
| 020, 024 | `LK_REQ`, `LK_RES` | ignored, 0 (CAPS2 b4 off; S38) |
| 080-10F | overlay bytes, valid bits | §5.8 |

**The session** (freeze_resp.v):
- **Starts** when none runs, it is not blocked, no reset is held (:198) and (:194-195):
  - mode 1: FRZ, C64_STOP and C64_MODE Ultimax;
  - mode 2: the pause (in UE2 the same as FRZ and C64_STOP);
  - mode 0: never.

  Each start counts in `SH_STATUS` b31:16 (:513-519).
- **Runs** while (:196-197): mode 1 FRZ and C64_STOP (Ultimax may go); mode 2 the pause.
- **Ends** when that falls, or on abort (:493): session off, PROTO off. An abort blocks new sessions until C64_STOP
  is off and the machine runs (:521, :859).
- **A reset in a session** (C64_MODE reset) ends it and blocks (:570-574, :772-783). The device first replays the
  VIC registers written in the session (§5.8). The reset then goes on.
- With the firmware's sequence the session runs from `goUltimax()` in `stop(true)` (c64.cc:544) to `resume()`.

**The DMA window in a session** is answered as §5.8 says (B1 and B2 are built together). Without B2, RAM, ROM and
I/O would go to TRX64 with their side effects, and the writes of Gideon's protocol that the device does not commit
(`$D019` = `$81` and `$D012` in `determine_d012()` and `resume()`, c64.cc:462-490, :649-672; every VIC register in
`restore_io()`, :1371; the SID dummies, :1377) would reach the chips, as on a stock U64.

**`vic_view`**, the text layer's registers (freeze_resp.v:460-474), taken from the session's overlay (§5.8), which only
the window's writes in a session fill (not mem-only):

| Register | Address | Kept |
|---|---|---|
| `$D011` | `$D000-$D3FF`, A5:0 = `$11` | b6:5 (ECM, BMM) |
| `$D016` | A5:0 = `$16` | b4 (MCM) |
| `$D018` | A5:0 = `$18` | b7:1 |
| `$D021` | A5:0 = `$21` | b3:0 |
| CIA 2 PRA | `$DD00-$DDFF`, A3:0 = 0 | b1:0 |
| CIA 2 DDRA | A3:0 = 2 | b1:0 |

- A register not written in this session counts as 0; PRA as %11 and DDRA as %00, which is bank 0.
- Pins = PRA | ¬DDRA, VIC bank = 3 − pins.
- All are dropped when the session ends.
- In a session the app has Ultimax on, so `$D000-$DFFF` is I/O whatever `$01` holds (freeze_resp.v `pla`, the Ultimax
  row).

### 5.5 Still capture

Confirmed against pix_layer.v:
- **Window:** VIC raster columns 96-495, lines 16-285 (pix_layer.v:65-66, :504-505): the pixels the composite's C64
  window shows (phase A's mapping through `text_x` and `first_line`).
- **Packing:** pixel k = 400 y + x is nibble k mod 2 of byte k / 2, the low nibble first (pix_layer.v:524-530).
  200 bytes a line, 54,000 a frame.
- **Slot:** a frame goes into the slot that is not the last one. The last starts as 1, so the first frame goes to
  slot 0 (pix_layer.v:494, :518-519). Slot n is at `STILL_BASE` + n × 0x10000.
- **Complete:** last = that slot; `STILL_STATUS` b0 = 1 (it stays 1), b1 = the slot, b31:8 counts (pix_layer.v:535-541,
  :164-168).
- **`STILL_CTRL` b0 and `STILL_BASE`** are taken at the window's first pixel (pix_layer.v:513, :518), so a frame is
  captured whole or not at all. UE2 takes them at the VIC frame end.
- **Torn frames:** the device drops a frame that lost a DDR2 write and uses its slot again. UE2 loses none.
- **Paused:** the VIC is gated, its pixel position stands, so nothing is emitted (pix_layer.v:503) and nothing
  captured; `STILL_STATUS` keeps the last.
- **60 Hz:** the window is 270 lines from line 16 whatever the System Mode; a 263-line VIC never reaches line 285, so
  no frame completes (pix_layer.v:505, :535) and `STILL_STATUS` stays as it was. UE2 does the same: the lines the VIC
  reaches go into the slot after the last, nothing completes. A suspected RTL issue (§6).
- **In UE2:**
  - At each VIC frame end the C64 port, which has the frame and the guest DDR, writes the window into the slot when
    `STILL_CTRL` b0 is set. Address bits 26:0; bytes past UE2's 64 MB are dropped. The default base `0x05050000` is
    past it, so nothing is written until the app sets its own (trx_video.cc:821).
  - When several VIC frames ended since the last sync (`--speed max`), only the newest is written, into the slot the
    device would have used for it, and the count goes up by the frames that passed.
  - The still of a frame lands before that frame's frame start, as on the device (line 285 against line 9 of the next
    frame).

### 5.6 The C64 text layer

**Registers** (page 6, the 32-bit rule; c64_text.v keeps its own stage of byte 0 and its own latch, top.v ORs its read
data with pix_layer's):

| Off | Name | |
|---|---|---|
| 0C0 | `TX_CTRL` | RW b0 show, b1 opaque (c64_text.v:94-95). R `0x0100_0000` \| ctrl (c64_text.v:89): b31:24 = 1 tells the app the layer is there (trx_video.cc:823) |
| 0C4 | `TX_STATUS` | R b0 shown now, b15:8 late lines (c64_text.v:88); late lines stay 0 in UE2 |

**Shown** while `TX_CTRL` b0, a session runs and `vic_view` has ECM = BMM = MCM = 0 (c64_text.v:135, :188). `$D011`
DEN is not looked at.

**What it draws** (c64_text.v:106-173, :231-297):
- 40 × 25 cells of 8 × 8, 200 lines. No scroll, 38 columns, 24 rows, sprites, multicolour, ECM or bitmap.
- Screen code: RAM at bank × `$4000` + `$D018` b7:4 × `$400` + 40 row + column (c64_text.v:125, :165). The RAM itself,
  whatever the PLA maps there.
- Glyph byte, 8-bit code, no reverse logic (c64_text.v:124-126):
  - char ROM at `$D018` b1 × `$800` + 8 code + line, when the bank is 0 or 2 and `$D018` b3:2 = %01;
  - else RAM at bank × `$4000` + `$D018` b3:1 × `$800` + 8 code + line.
- Colour: the colour RAM nibble at 40 row + column.
- Pixel (c64_text.v:289-295): glyph bit 7 − i set gives palette[colour]; clear gives palette[`$D021`] with opaque, else
  the picture below. `$D021` is taken at the frame start (c64_text.v:198-199).
- Palette: vic_scaler.v's (c64_text.v:251-271), the composite's TRX palette.
- **Geometry:** each C64 pixel 4 × 4 output pixels, the text area's top left at output (320, 140), 1280 × 800
  (c64_text.v:46-47: cx 1088, cy 185, in counters whose active area starts at cx 768, cy 45, overlay.v:142, :152). That is
  window (40, 35), where the still's text area is.

**In UE2:** drawn at render time from the current RAM, colour RAM, char ROM and `vic_view`; the device fetches every
frame, so the picture is the same. (320, 140) and 4 × 4 halve exactly, so phase A's 960 × 540 build stays valid with
the text on. While the text is shown, `c64screen` dumps its screen with its character set (the paused VIC still
points at the program's).

**Stacking:** C64 (black while paused), then `overlay.v`, then the layer, then the text (top.v:1385, :1404, :1417:
each takes the one before as `rgb_in`).

### 5.7 The picture while frozen

1920 × 1080:
- the bars, x 0-159 and 1760-1919, black;
- x 160-1759: the layer, the still at 4 × 4 per VIC pixel, mixed with the menu background (t/4 still);
- on it the menu text, 1280 × 800 from (320, 140), glyph pixels in their colour RAM colours, the rest the still.

With S = 2, x0 = 160 and y0 = 0 the composite is built at 960 × 540.

### 5.8 Phase B2: shadow I/O and the commit list

B1 makes the freeze look right. B2 makes the machine come back from it as on the device: the board's
`freeze_raster` test runs 10 of 10 freezes with no missed or moved IRQ (T12 §14.6.1). With B2, CAPS2 = `$08`; both are
built.

In a session no access of the window becomes a bus cycle (freeze_resp.v):
- **Decode:** the PLA with the CPU port frozen at the session's start and C64_MODE Ultimax forced (:274-316).
- **RAM and ROM:** real (the second port), as in B1.
- **VIC, CIA 1/2, SID writes** go into a 128-byte overlay with valid bits (VIC 0-63, CIA 1 64-79, CIA 2 80-95, SID
  96-127), never into the chips.
- **Reads** come from the overlay where written, else from the frozen chip without side effects (UE2: TRX64's peek):
  `$D011` b7, `$D012`-`$D014`, `$D01E`/`$D01F` and `$D019` from the chip, `$D019` less the flags the list
  acknowledges, unused VIC bits 1 (:411-420); CIA 1 ports from the overlay's PRA/DDRA/PRB/DDRB and the keys and
  joysticks (`kbd`, :337-355); CIA 2 ports (PR & DDR) | (pins & ¬DDR); timers, TOD and ICR from the chip
  (:419-436).
- **Commit list** (:478-488, :801-850), 128 entries, more are dropped and counted:
  - plain registers (VIC but `$D011`/`$D012`/`$D019` and those a write does nothing to, :328-335; CIA
    PRA/PRB/DDRA/DDRB/CRA/CRB without LOAD) take one entry at their first write and are written at the end with their
    last value, only where it differs from the chip;
  - the others (`$D011`, `$D012`, `$D019`, CIA timers, TOD, SDR, ICR, CRA/CRB with LOAD, SID, IO1/IO2) take one entry
    per write, written as written (`$D011` bit 7 as written, bits 6-0 the last value);
  - nothing is listed while PROTO is set;
  - at the end the list runs as real bus cycles before the CPU runs on (`hold`, :497); abort, mode 0 or discard drop
    it.
- **Reset in a session:** the overlay's VIC registers but `$13`, `$14`, `$1E`, `$1F` are written for real, the rest
  of the list is dropped, then the reset (:772-783).
- **SIDEFX:** I/O and cartridge accesses are real cycles (:596).
- **Cartridge:** ROML/ROMH/IO1/IO2 reads from the cartridge model without side effects; IO1/IO2 writes go to the list,
  ROML/ROMH writes are dropped and counted (`SH_UNMAPPED`, :658-667).
- **Registers:** `SH_ACCESSES`, `SH_REAL`, `SH_GCLK`, `SH_UNMAPPED`, `SH_LIST`,
  `SH_COMMIT`, the overlay bytes 080-0FF and valid bits 100-10F (trx_sys.v:231-241).
- **Check:** a raster program frozen and resumed ten times keeps every IRQ's line and cycle and misses none (the
  bridge's unit test drives the session with Gideon's protocol under PROTO and a menu between).
- **In UE2:** the decode is freeze_resp.v's PLA, ported; the frozen chips are TRX64's I/O peek; CIA 1's ports are
  TRX64's own port function (VICE `read_ciapa`/`read_ciapb`) over the overlay's port registers, where freeze_resp.v
  has its simpler `kbd()`; the commit is TRX64's banked write with the Ultimax decode put in when I/O is not mapped.

### 5.9 Checks (B1, B2)

**Unit** (`devices/trx.rs`, `devices/c64.rs`, `c64-bridge`, `render.rs`):
- CAPS `$E1`, CAPS2 `$08`; page 0 from 0x100 and page 5 follow the far rule with one shared stage and latch;
  `GATE_CTRL` b0 RW, b8 and b9 follow the pause; 104-FFF read 0.
- C64_STOP without FRZ: `Hold::Cpu`, the VIC frame count goes on. With FRZ: the cycle count, the VIC frame count, a
  CIA timer and drive 8 stand; `IRQ_CAUSE` b0 at the start; after the release the machine runs on from the same
  cycle.
- Frame starts while paused, one per frame period; none lost at the pause's start or end.
- The session: start needs mode 1, FRZ, STOP and Ultimax; it counts; it ends when STOP falls, with PROTO cleared;
  mode 0 none, mode 2 without Ultimax; an abort blocks until STOP is off; a reset ends it and blocks.
- `vic_view`: only writes in a session count; mirrors (`$D3D8` is `$D018`, `$DDF2` is DDRA); unwritten is 0 and bank
  0; cleared at the end; mem-only writes do not count.
- Still: the packing of pixels 0, 1, 399, 400 and 107,999; slots 0, 1, 0; `STILL_STATUS` valid, slot and count;
  `STILL_CTRL` 0 writes nothing and keeps the status; a base past 64 MB writes nothing; paused, no capture; a
  263-line frame never completes.
- Text: `TX_CTRL` reads `0x0100_0000` \| ctrl; a known screen at `$0400`, charset at `$0800` lands at (320, 140) at
  4 × 4 in the colour RAM's colours; background transparent and opaque; the char ROM case (bank 0, `$D018` = `$14`);
  hidden with ECM, BMM or MCM, without a session or without show; `TX_STATUS` b0 follows.
- Composite: the C64 window black while paused; the text over the layer; the 960 × 540 build equals the 1920 × 1080
  one scaled down.

**Monitor:** `monitor trx` adds `GATE_CTRL`, the pause, `SH_CTRL`, `SH_STATUS`, `TX_CTRL`, `TX_STATUS`, the `STILL_*`
registers and the VIC frame count.

**Firmware smoke** (`scripts/smoke-trx-freeze.sh`, the trx64/main ELF, `--board c64u --flash` a fresh copy of
`run/flash.bin`, `--c64-roms`, `--settings scripts/smoke-trx-freeze.cfg`: `Interface Type=Freeze`, `System Mode=PAL`):

| Step | Expected |
|---|---|
| boot | stderr `trx-io: on (TRX firmware: trx_lv_open)`; console `[TRX] IO block present, CAPS E1`, `[TRX] freeze menu on the pixel layer: still at …, text in hardware, commit list` |
| `wait 6000`, `c64screen`, `png run/before.png` | READY.; LVGL parked |
| `button`, `wait 1500` | console `[TRX] pixel layer: 2 buffer(s) of 864000 bytes`, `[C64] goUltimax() — C64_MODE = ULTIMAX`, `Init IO.`; no `[TRX] pixel layer: no flip` |
| `monitor trx` | `GATE_CTRL` `0x301`, `SH_STATUS` `0x0001_0001`, `TX_CTRL` `0x0100_0001`, `TX_STATUS` b0 1, `PX_CTRL` `0x23`, `PX_SIZE` `540 << 16 \| 800`, `PX_STRIDE` 1600, `PX_POS` 160, `STILL_STATUS` b0 1, the VIC frame count |
| `c64screen`, `png run/frozen.png` | the freeze menu, no `READY.` (`--board c64u` gives the Commodore interface); a 960 × 540 png, columns 0-79 and 880-959 black |
| `wait 1000`, `monitor trx`, `png run/frozen2.png` | the same VIC frame count; frozen2 equals frozen |
| `key down`, `wait 300`, `button`, `wait 1500` | the menu closes |
| `monitor trx`, `c64screen`, `png run/after.png` | `GATE_CTRL` 0, `SH_STATUS` `0x0001_0000`, `TX_CTRL` `0x0100_0000`, `PX_CTRL` `0x22`; the screen equals the first dump; after.png is not 960 × 540; the VIC frame count moves again |
| `type print 6*7`, `key return`, `wait 500`, `c64screen` | ` 42` |

The smoke also writes `frozen.png` for a look by eye against a photo of the device (Alex).

### 5.10 Phase B3: `overlay.v` in TRX mode

Only for "Overlay on HDMI"; the freeze path does not touch it. With `--trx-io` UE2 draws Gideon's chargen as
`overlay.v` does, between the C64 and the layer (top.v:1383-1388). Without the block nothing changes.
- **Registers** (overlay.v:84-93, taken at the frame start :119-123): CHAR_WIDTH b3:0, CHAR_HEIGHT b4:0 with the big
  font b6 and stretch b7, CHARS_PER_LINE, ACTIVE_LINES b5:0, the pointer's low 12 bits, TRANSPARENCY (index b3:0,
  on b7). X_ON/Y_ON, the line clocks and PERFORM_SYNC are not read.
- **Scale:** w = cpl · cw, h = nl · lpr with lpr = ch, for the big font ch − ch / 4 (overlay.v:135-137); sx, sy each
  the largest of 4, 3, 2, 1 with 4·w ≤ 1280 resp. 4·h ≤ 800 and so on (:138-139). The firmware's SD settings,
  40 × 25 of 8 × 9: 4 × 3, 1280 × 675. 1080p settings, 40 × 25 of 12 × 23 with the big font: 18 lines per row,
  2 × 1, 960 × 450 (overlay.v:17-18 says 960 × 575; the code gives 450).
- **Place:** centred in 1920 × 1080: first pixel x = (1920 − w·sx) / 2, first line y = (1080 − h·sy) / 2, rounded
  down, 0 when it does not fit (:142-143; cx xs_m1 + 1, cx 768 being x 0). SD settings: (320, 202), the box ends at
  x 1599 and line 876.
- **Counters** (:153-201): each character line repeats sy times and each pixel sx times; the big font steps from
  line 4n + 2 to 4n + 4; a row ends at line ch − 1 (5 bits) and moves the pointer on by cpl (12 bits); nl rows.
  Nothing is drawn while on is 0, cw · cpl is 0 or nl is 0.
- **Pixel** (:221-263): the glyph rules are Gideon's: the 8-pixel ROM font (`ovl_font8.mem`, the first 1 K of
  `roms/chars.bin`), stretch, the 9th line blank unless line 7 is 0x18, the 12 × 24 font for the big one, bit 7 of
  the screen code reverses. The colour index is the colour RAM's low nibble for a set bit, else its high nibble.
- **Blend** (:255-277): an index other than the transparent one is the overlay palette's colour, opaque; the
  transparent index inside the box is the C64 at half brightness, each channel shifted right by 1; outside the box
  the C64 is unchanged. UE2's 50 % mix (`blend_half`) is Gideon's look and stays for the path without the block.
- With the box on, the composite is built at 1920 × 1080.

### 5.11 Deferred to S38

Pages 1 (state bus), 2 (engine, frame ring, APPLY), 3 (debug), 4 (cartridge write-back), 7 (expansion port);
`CAP_CTRL`, `RESUME_PHASE`, `STOP_POS`, the restore path; the lens unit (`LK_REQ`/`LK_RES`, CAPS2 b4); Timewarp
(`trx_video_tw_*`, needs CAPS b2); `BUILD` and CAPS2 b1. Each of them turns on app paths that B1 keeps off by its CAPS.

## 6. Open

- Where the resident UI's buffers live (T24 open question 2). This does not matter to UE2: it reads whatever
  `PX_FRONT` says.
- Picture-in-picture: waits for the RTL (Alex, 2026-10-04): built once the FPGA side defines its registers (T24 §14).
- The `trx64/main` app on UE2 beyond the layer:
  - With CAPS = `$C0` it touches pages 0 and 6 only (As built).
  - The boot ROM and flash layout (T15) do not matter: UE2 loads the ELF.
- The fade: the smoke sees the `PX_GALPHA` writes but checks no intermediate picture.
- Phase B, for the FPGA side:
  - **RTL bug, confirmed by the FPGA side (2026-10-04), fix in progress:** the still at 60 Hz. pix_layer.v's window is 270 lines from line 16 in every System Mode
    (`Y0S`, :505), so a 263-line VIC never reaches line 285, no frame completes (:535), and the freeze shows only the
    menu background behind the text. UE2 models it as it is.
  - top.v:1059 mutes the sound with C64_STOP alone (`c64_mute <= c64_halted | app_s2[1]`), also without FRZ. UE2's
    CPU-only stop (`Hold::Cpu`) keeps the SID playing, as before.
- Phase B, decided (Alex, 2026-10-04): B1 and B2 together; `GATE_CTRL` b9 read as b8; the resume at once; B3 (overlay.v
  placement for "Overlay on HDMI") is built next (Alex, 2026-10-04).

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
- **Gideon's overlay** follows `overlay.v` since B3 (see "As built (phase B3)").
- **Drive B:** the firmware probes DRIVETYPE 2 at boot (c1541.cc:119-123). With `--trx-io` the write keeps the
  1541, so the firmware offers drive B as a 1541 only, and the log says so once.
- **Firmware smoke** (`scripts/smoke-trx.sh`, trx64/main 029da8ef): "LVGL 9.6.0 on the pixel layer" at about
  1.5 s, the splash in a 960 × 540 png, `PX_GALPHA` written 17 times (the fade), "splash done; UI parked" at
  about 4.5 s with the layer off. In the first 6 s the app touches page 0 (ID, CAPS, IRQ_EN = b7) and page 6
  (`PX_CTRL`, `PX_FRONT`, `PX_SIZE`, `PX_STRIDE`, `PX_POS`, `PX_GALPHA`, `PX_KEY`); no other page, no unmapped
  access, no hang. It never writes `PX_BACK`, so no flip. With `--trx-io=off` it reads ID byte 0 once, gets 0 and
  boots to READY with no splash.

## As built (phase B1, B2)

- **The pause** is the bridge's: TRX64 v0.12.3 has no hold for the whole machine, so `Trx64Backend::advance_to` runs
  no cycle while it stands (the sampler, the FPGA's, runs on) and the release re-anchors the clock at the cycle it
  stood at. `GATE_CTRL` writes reach the C64 port with their clock: the C64 runs up to it, then the pause changes.
- **Frame starts while paused** come every frame period of the C64's row from the last one; the window, `png` and
  `c64screen` follow the TRX output's frame starts with `--trx-io` (`Machine::picture_counter`), so the frozen picture
  is published although the VIC frame count stands.
- **The session** (`devices/trx_session.rs`) answers each window access at once; its list runs inside the access that
  drops C64_STOP, before the C64 runs on. The commit writes at the address as written (a mirror stays a mirror) and
  compares a plain CIA port register with the chip's register file.
- **The still** is written when the access hands DDR back. The first VIC frame after the C64 is attached is not
  captured. With the default base, past UE2's 64 MB, frames count but nothing is built or written.
- **TRX64 gaps**, met on the UE2 side: no whole-machine hold (`Hold::Machine`: CPU, VIC, CIAs, SID and drives stand,
  the DMA window and keys still served), and no host write to I/O whatever the PLA maps (the bridge puts the Ultimax
  decode in for the commit and takes it out again).
- **Firmware smoke** (`scripts/smoke-trx-freeze.sh`, trx64/main 029da8ef): the console says `CAPS E1` and `, commit
  list`; frozen, `monitor trx` reads `GATE_CTRL` `0x301`, `SH_STATUS` `0x0001_0001`, `TX_CTRL` `0x0100_0001`, the
  layer 800 × 540 at (160, 0), the still valid; frozen.png is the menu text over the dimmed still, 960 × 540 with black
  bars, and frozen2.png 1 s later is the same file with the same VIC frame count. After the unfreeze the screen is the
  one before, the list wrote nothing (every listed register was put back by `restore_io()`), and `print 6*7` gives 42.

## As built (phase B3)

- **`render.rs`:** `TrxBox` takes the registers and runs overlay.v's line counters over the 1080 lines;
  `draw_overlay_trx` runs its pixel counters across the box and replaces the 50 % window in `render_trx`. The path
  without the block (`blend_overlay`) is unchanged.
- **The frame start:** UE2 renders from one snapshot of registers, screen and colour RAM and palette. overlay.v takes
  the registers at the frame start, but reads the RAMs and the palette live (:208-209, :275); a write within a frame
  shows from the next picture in UE2.
- **Not modelled:**
  - the big font: UE2 draws the 8 × 8 font widened to 12 (`Renderer::new`), the RTL its `ovl_font12.mem`. The TRX
    firmware writes the SD settings (CHAR_WIDTH 8, CHAR_HEIGHT 9, no big font) in the smoke, so it does not show;
  - the bus side: overlay.v decodes the registers at 0x000-0x00F only (Gideon's every 16 bytes), reads the palette as
    0, and powers up with the Colodore palette (UE2: 0). The firmware writes its own palette before the menu opens.
- **Firmware smoke** (`scripts/smoke-trx-overlay.sh`, trx64/main 029da8ef): the TRX app writes 40 × 25 of 8 × 9,
  pointer 0, TRANSPARENCY 0xC0 (index 0). menu.png is 1920 × 1080 with the box at (320, 202)-(1599, 876): outside it
  the picture before the menu, inside it the C64 at half brightness wherever the cells are index 0, and the rest in
  at most 16 opaque colours (6 or 7 per run). After the second `button` the picture is 960 × 540 again.
