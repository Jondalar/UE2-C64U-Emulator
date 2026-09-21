# Menu UI output (overlay chargen, HDMI palette) and input paths — U64-II

Scope: how the unmodified `ultimate` app (target/u64ii/riscv/ultimate) draws the Ultimate menu and gets keystrokes. Defines in effect: `RISCV U64=2 USB2513 OS IOBASE=0x10000000 U2P_IO_BASE=0x10100000`.
All paths below are relative to `firmware/1541ultimate/`. `sw/` = `software/`.

## Sources read  (files + key functions)

Built (listed in `target/u64ii/riscv/ultimate/Makefile`): `screen.cc` (:79), `keyboard_c64.cc` (:80), `userinterface.cc` (:107), `ui_elements.cc` (:109), `tree_browser.cc` (:114), `host_stream.cc`/`keyboard_vt100.cc`/`screen_vt100.cc` (:136-138), `keyboard_usb.cc` (:146), `u64_config.cc` (:165), `socket_gui.cc` (:175), `u64ii_init.cc` (:44), `hdmi_scan.cc` (:46), `chars.bin`/`default_chars.bin` (SRCS_BIN :238). NOT built: `vt100_parser.cc`, `u64/u64ii_test.cc` (used only as corroboration).

| File | Key items |
|---|---|
| sw/io/overlay/overlay.h | `class Overlay : GenericHost` ctor, `take_ownership`, `release_ownership`, `checkButton`, `update_settings`, `OVERLAY_CHARHEIGHT_*` |
| sw/io/overlay/chargen.h | `t_chargen_registers` |
| sw/system/u64.h, u2p.h, iomap.h, itu.h, itu.c | address macros, `ITU_BUTTON_REG`, capabilities |
| sw/application/ultimate/ultimate.cc | `ultimate_main` (host creation, menu open loop), `push_active_menu_button` |
| sw/io/c64/screen.h / screen.cc | `Screen_MemMappedCharMatrix`, `Window` |
| sw/userinterface/userinterface.cc/.h | `run_once`, `buttonDownFor`, `appear`, `set_screen_title`, colour `schemes[]`, `keymapper`, cfg `CFG_USERIF_ITYPE` |
| sw/userinterface/tree_browser.cc, ui_elements.cc | key → MENU_HIDE/EXIT, `wait_free`, `backup/restore` |
| sw/io/c64/keyboard_c64.cc/.h, keyboard.h | matrix scan protocol, keymaps, repeat, `wait_free` |
| sw/io/usb/keyboard_usb.cc/.h | `system_usb_keyboard`, `usb2matrix`, `applyMatrixState`, `getch`, `push_head`, `enableMatrix` |
| sw/io/c64/c64.cc/.h | C64 host (`freeze`, `stop`, `checkButton`, `exists`, `available`) |
| sw/u64/u64_config.cc, hdmi_scan.cc | `DetermineOverlaySettings`, `program_palette_rgb`, `default_colors`, HPD IRQ/task, boot hotkey |
| sw/components/config.cc, sw/filesystem/blockdev_flash.cc | safe-mode read of `U64_RESTORE_REG` |
| sw/api/route_machine.cc, route_input.cc; sw/io/c64/c64_subsys.cc; sw/io/command_interface/control_target.cc | REST / command-interface menu button & key injection |
| sw/network/socket_gui.cc; sw/io/stream/host_stream.*, screen_vt100.*, keyboard_vt100.* | Telnet UI |
| sw/portable/riscv/riscv_main.c | high-IRQ dispatch |
| fpga/ip/video/vhdl_source/char_generator_{regs,pkg,peripheral_12,slave12,rom,rom_pkg}.vhd, fpga/ip/video/vhdl_gen/font_pkg.vhd | open chargen IP (register decode, rendering, fonts) |

## Address map   (table: absolute addr | width | R/W | name | meaning)

Chargen base: `VID_IO_BASE = U2P_IO_BASE+0x40000 = 0x10140000` (u64.h:21); `U64II_OVERLAY_BASE = VID_IO_BASE+0` (u64.h:25). `Overlay(false, 12, U64II_OVERLAY_BASE, …)` (ultimate.cc:125) → regs at `base`, screen at `base + (1<<12)`, colour at `base + (2<<12)` (overlay.h:51-53), same as `U64II_CHARGEN_*` (u64.h:30-32).

| Abs addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10140000 | 8 | (W, never written) | LINE_CLOCKS_HI | VHDL: clocks_per_line(10:8) (regs.vhd:44-45). Firmware never writes. |
| 0x10140001 | 8 | (W, never written) | LINE_CLOCKS_LO | — |
| 0x10140002 | 8 | W | CHAR_WIDTH | pixels per glyph column, bits 3:0 (regs.vhd:48-49). FW writes 8 or 12 (overlay.h:160) |
| 0x10140003 | 8 | W | CHAR_HEIGHT | bits 4:0 = scanlines/cell, bit6 = big_font (12×24 font), bit7 = stretch_y (regs.vhd:50-53). FW writes 0x09 / 0x90 / 0x5E (overlay.h:19-23, :161) |
| 0x10140004 | 8 | W | CHARS_PER_LINE | text columns (40) (overlay.h:154) |
| 0x10140005 | 8 | W | ACTIVE_LINES | text rows, bits 5:0 (25) (overlay.h:155; regs.vhd:56-57) |
| 0x10140006/07 | 8+8 | W | X_ON_HI/LO | 12-bit raster x start (overlay.h:156-157) |
| 0x10140008/09 | 8+8 | W | Y_ON_HI/LO | 12-bit raster y start (overlay.h:158-159) |
| 0x1014000A/0B | 8+8 | W | POINTER_HI/LO | 15-bit start index into screen RAM; FW writes 0 (overlay.h:162-163) |
| 0x1014000C | 8 | W | PERFORM_SYNC | FW writes 0 (overlay.h:164) |
| 0x1014000D | 8 | W | TRANSPARENCY | bits 3:0 transparent colour index, bit6 own_keyboard, bit7 overlay_on (regs.vhd:72-75). FW writes 0xC0 = show (overlay.h:92) / 0x00 = hide (overlay.h:98) |
| 0x10141000..0x10141FFF | 8 | R/W | Screen RAM | cell glyph code (bit7 = reverse); FW uses cells 0..cols*rows-1 (40×25 = 0x000..0x3E7). VHDL dpram default 0x20 (peripheral_12.vhd:163-176) |
| 0x10142000..0x10142FFF | 8 | R/W | Colour RAM | low nibble fg, high nibble bg (screen.cc:296-297). VHDL default 0x0F (peripheral_12.vhd:178-193) |
| 0x10144000..0x1014401D | 8 | W | U64II_HDMI_REGS (`t_video_timing_regs`) | HDMI timing/scaler (u64.h:26, :172-203; hdmi_scan.cc:112-305). Owned by HDMI doc; listed because `resync=2` is written on HPD reconfigure (u64_config.cc:2688-2689) |
| 0x10145000..0x1014503F | 8 | W | U64II_HDMI_PALETTE | 16 × {R,G,B,pad} (u64_config.cc:2733-2739, :2752-2755) |
| 0x10148000..0x10148003 | 8 | W | U64II_CROPPER_BASE | VIC crop offset_x, offset_y, size_x/2, size_y/2 (hdmi_scan.cc:45-60) |
| 0x10180800..0x1018083F | 8 | W | C64_PALETTE RGB | 16 × {R,G,B,pad} (u64.h:50; u64_config.cc:2724-2731) |
| 0x10180C00..0x10180C3F | 8 | W | C64_PALETTE YUV | 16 × {Y,U,V,pad} (u64_config.cc:2793-2801) |
| 0x1000000A | 8 | R | ITU_BUTTON_REG | bits 7:5 buttons; bit6 (ITU_BUTTON1=0x40) = menu button (itu.h:21, :93-96; c64.cc:1501-1503; overlay.h:114-115) |
| 0x10100300..0x1010030A | 8 | W | MATRIX_KEYB | USB/REST → C64 matrix feed: [0..7] active-high row bitmaps, [8] reset, [9] restore, [10] freeze (u2p.h:19; keyboard_usb.cc:221-228) |
| 0x1010030B | **32** | W | MATRIX_WASD_TO_JOY | `volatile uint32_t*` at non-aligned address (u2p.h:98); written 0 / `wasd_to_joy` every scan (keyboard_c64.cc:197,260,272; keyboard_usb.cc:252; u64_config.cc:1066) |
| 0x10100400 | 8 | R/W | U64_HDMI_REG | W: 0x20 DDC_ENABLE, 0x10 DDC_DISABLE, 0x08 HPD_RESET; R: bit2 HPD_CURRENT, bit3 HPD_WASLOW (u64.h:65, :93-97) |
| 0x10100402 | 8 | R | U64_RESTORE_REG | ==1 at boot → config safe mode (config.cc:48-50; blockdev_flash.cc:161-163) |
| 0x10100406 | 8 | R/W | U64II_KEYB_JOY | R: joystick lines active-low, bits 4:0 (keyboard_c64.cc:208-219). W: joy swap bit (u64_config.cc:1064, :2396) |
| 0x10100407 | 8 | R | U64II_BLACKBOARD | bit0 (network/assembly.cc:36) — not UI |
| 0x1010040A | 8 | W | U64II_KEYB_COL | matrix select, active-low (C64 CIA1 PA equivalent) (u64.h:75; ultimate.cc:126) |
| 0x1010040B | 8 | R (also written 0xFF) | U64II_KEYB_ROW | matrix return, active-low (CIA1 PB equivalent) (u64.h:76; keyboard_c64.cc:201-202, :232, :241-243) |
| 0x10100800..0x10100803 | 8 | R/W | Blingboard keyb: RX_DATA, RX_GET, RX_FLAGS, RX_IRQEN | FW only writes RX_FLAGS 0x01/0x00 (shift-lock disable/enable) and reads RX_FLAGS bit2 = installed (u64.h:38-43; keyboard_c64.cc:198,261,273,367,378; led_strip.cc:497). RX_DATA never read in this build |
| 0x10181000/01 | 8 | R | C64_PLD_STATE0/1 | read and copied into PORTA/B on menu close (overlay.h:101-102) |
| 0x10181010/11 | 8 | W | C64_PLD_PORTA/B | ← STATE0/STATE1 (u64.h:146-149) |

Not read by firmware: any chargen register (VHDL acks reads with default data, regs.vhd:79-80).

## Init / boot sequence as seen from the bus  (ordered, with file:line)

1. `custom_hardware_init`: W 0x10100400 = 0x20 (u64ii_init.cc:126).
2. ConfigManager ctor: R 0x10100402; `==1` → safeMode (config.cc:48-50). Flash disk init: same read, `==1` → skip flash FS (blockdev_flash.cc:161-163).
3. `ultimate_main`: R 0x1000000C..0F capabilities (ultimate.cc:87; itu.c:19-29).
4. InitFunction "U64 Config" (prio 1, u64_config.cc:89-93) → `U64Config()` only if `CAPAB_ULTIMATE64` (u64_config.cc:904):
   - boot hotkey: W 0x1005DC02=0xFF, 0x1005DC03=0x00, then `scan_keyboard(&CIA1_DPB,&CIA1_DPA)` on the C64 CIA1 window 0x1005DC00/01 (u64_config.cc:948-964; c64.h:186-188,202; keyboard_c64.cc:118-155). Key 0x10 → PAL, 0x0E → NTSC.
   - `effectuate_settings`: W 0x10100406 = swap&1, W32 0x1010030B = wasd (u64_config.cc:1062-1066); palette 16×4 bytes to 0x10180800, 0x10145000, YUV to 0x10180C00 (u64_config.cc:1113-1118, 2720-2743); first call has `systemMode=e_NOT_SET` (u64_config.cc:894) → `doPll` → HDMI/PLL setup and `DetermineOverlaySettings` (u64_config.cc:1108-1136). `overlay` is still NULL, so no chargen writes (u64_config.cc:1137).
   - HPD: create task, `install_high_irq(ITU_IRQHIGH_HDMI=5, hpd_monitor_irq)`, give semaphore → task reads U64_HDMI_REG bit2 and EDID (u64_config.cc:969-972, 1002-1010, 2581-2584).
   - InitFunction "U64 Palette" (prio 9) programs the palette again (u64_config.cc:2880-2889).
5. `C64::getMachine/init/start` if `CAPAB_CARTRIDGE` (ultimate.cc:100-104); `C64::init` sets `available=true` (c64.cc:211).
6. `system_usb_keyboard.setMatrix(0x10100300)`: W [0..7]=0, then `applyMatrixState` W [0..10] and W32 0x1010030B; `enableMatrix(true)` repeats it (ultimate.cc:114-117; keyboard_usb.cc:976-1011, 214-229).
7. `new Overlay(false, 12, 0x10140000, overlay_default)` with `overlay_default = u64_configurator->overlaySettings` (ultimate.cc:123-125):
   W +4 cols, +5 rows, +6/+7 X_ON, +8/+9 Y_ON, +2 CHAR_WIDTH, +3 CHAR_HEIGHT, +A/+B = 0, +C = 0 (overlay.h:154-164); then `release_ownership`: W 0x1014000D = 0x00; R 0x10181000 → W 0x10181010; R 0x10181001 → W 0x10181011 (overlay.h:98-102).
   Default config (CFG_SYSTEM_MODE default 1 = NTSC, u64_config.cc:348,275; CFG_HDMI_RESOLUTION default 0 = SD, u64_config.cc:350,256-263) → `{40,25, X_on=300, Y_on=235, 8, 0x09}` (u64_config.cc:2984-2986): +4=0x28 +5=0x19 +6=0x01 +7=0x2C +8=0x00 +9=0xEB +2=0x08 +3=0x09.
8. `new Keyboard_C64(overlay, ROW=0x1010040B, COL=0x1010040A, JOY=0x10100406)` — no bus access (ultimate.cc:126; keyboard_c64.cc:98-112).
9. Overlay UI: `UserInterface::init(overlay)` → `is_permanent()` true → `appear()` (userinterface.cc:244-253, 503-512; overlay.h:87-89) → `set_screen_title`: `clear()` = memset 0x10141000[0..999]=0x20, 0x10142000[0..999]=0x0F; title at row 0 centred with `\eA`…`\eO` (colour 1 → 15); rows 1 and 24 filled with char 0x02 (userinterface.cc:625-643; screen.cc:347-356). Then `TreeBrowser::init/redraw` draws the file browser into the same RAM. The overlay RAM is fully populated while TRANSPARENCY = 0 (hidden).
10. C64 UI: `init(c64)` — C64 not permanent → no drawing (userinterface.cc:250; c64.cc host has no `is_permanent`).
11. Main loop, every `vTaskDelay(3)` = 15 ms at `configTICK_RATE_HZ 200` (ultimate.cc:206; sw/FreeRTOS/Source/FreeRTOSConfig.h:21):
    R 0x1000000A (`c64->checkButton`, c64.cc:1493-1505); USB key queue (no HW); R 0x10100400 every iteration (`&&` left operand, ultimate.cc:183); `pollInactive()` on both UIs (ultimate.cc:192-195; tree_browser.cc:161-165).

## Boot hazards  (poll loops, probes, required responses)

| # | Where | What | Required emulator response |
|---|---|---|---|
| 1 | ultimate.cc:100-107, 166, 209-215 | Menu loop only exists if `CAPAB_CARTRIDGE` (0x00000200). Otherwise task prints "GUI … terminated" and suspends: no menu can ever open. | Capabilities (0x1000000C..0F, big-endian byte order itu.c:23-26) must include 0x00000200. |
| 2 | u64_config.cc:892-904; ultimate.cc:123-125; screen.cc:51-56, 296-298 | Overlay geometry comes from `U64Config::overlaySettings`, only initialised when `CAPAB_ULTIMATE64` (0x04000000). Without it the heap object's `overlaySettings` is not set by the ctor; a 0×0 size leaves `cell_colour_codes` NULL and `output_raw` writes through it. Also USB matrix is not attached (ultimate.cc:114). | Capabilities must include 0x04000000. |
| 3 | ultimate.cc:183-187 | Overlay UI chosen only if R 0x10100400 bit2 (HPD_CURRENT) = 1 **and** `CFG_USERIF_ITYPE == 1`. Else the C64 "Freeze" UI is used. | Return bit2 = 1 (e.g. 0x04). Writes 0x20/0x10/0x08 accepted, no effect needed. |
| 4 | userinterface.cc:123; config.cc:102-111 | `CFG_USERIF_ITYPE` default **0 = "Freeze"**. Store "User Interface Settings", page id 0x47454E2E (userinterface.h:28), shared by both UIs. | Pre-seed the flash config page with ITYPE=1, or accept one Freeze-path entry and toggle with C=+I / USB TAB (`KEY_CTRL_I`=0x09, tree_browser.cc:465-468; keyboard_c64.cc:65; keyboard.h:39). |
| 5 | c64.cc:985-1008, 411-497; userinterface.cc:308 | Freeze path: `run_once` returns at once if `C64::exists()` = PHI2 bit0 of R 0x10040003 is 0 (c64.cc:352-360), so nothing happens. If bit0=1: `stop(true)` has bounded loops (25 ms, 10 ms via ITU_TIMER, c64.cc:437-483), then **unbounded** `while(!(C64_STOP & 0x02));` on R 0x10040001 (c64.cc:493-494). | Avoid (hazards 3+4). If entered: 0x10040001 must read bit1=1 after `C64_STOP=1`. |
| 6 | ultimate.cc:168-199; userinterface.cc:311-314, 362-381; c64.cc:1497-1505 | Button edge detection: `button_prev` starts 0, so a constant 0x40 in ITU_BUTTON_REG yields one false push at the first loop, then `buttonDownFor(1000)` polls for 1 s → `swapDisk()` instead of the menu, and no edge is ever seen again. | 0x1000000A bits 7:5 = 0 when idle. Menu press = bit6 high ≥ 1 loop (≥15 ms, use 50-200 ms), released < 1000 ms. |
| 7 | config.cc:48-50; blockdev_flash.cc:161-163 | R 0x10100402 == 1 → safe mode (config defaults, including ITYPE=0) and no flash filesystem. | Return 0. |
| 8 | keyboard_c64.cc:208-226 | R 0x10100406 bits 4:0 ≠ 0x1F → phantom joystick. Value 0 → bit0 "up" → `KEY_UP` auto-repeat, and `joystick_blocks_keyboard` blocks the matrix. | Return 0xFF (bits 4:0 = 1) when idle. |
| 9 | keyboard_c64.cc:240-243 | `do { row=*ROW; *COL=col; } while(row != *ROW);` — spins until two reads agree. | ROW reads must be stable for a fixed COL latch (a pure function of latch + key state). |
| 10 | keyboard_c64.cc:231-232, 256-264, 369-375 | R 0x1010040B ≠ 0xFF with COL=0 means "key down". Non-0xFF idle: garbage keys, and every `wait_free()` (after each menu action with ret<0, tree_browser.cc:232-234; popups userinterface.cc:662-664) burns its 2000 ms timeout (keyboard.h:6). | Idle ROW = 0xFF. |
| 11 | u2p.h:98 | 32-bit store to 0x1010030B (unaligned), on every scan and matrix update. | Bus must accept it without an alignment/access trap. Latch or ignore. |
| 12 | screen.cc:90, 130-136, 138-157, 170-179, 189-201 | Screen RAM is read back (`^= 0x80` cursor, `backup()` memcpy, scroll copies). Colour RAM is read back in scroll_up/down. Popups call `backup/restore` (ui_elements.cc:77,172,247,475,507,540,603,615). | 0x10141000 and 0x10142000 = plain 4096-byte RAM with read-back. Not a hang, but colours and text corrupt otherwise. |
| 13 | u64_config.cc:948-964; keyboard_c64.cc:127-138 | Boot hotkey scans C64 CIA1 PB 0x1005DC01 (C64 memory window) with the same stable-read loop. 0x10/0x0E switch system mode. | Return 0xFF, stable. (C64-core doc.) |
| 14 | keyboard_c64.cc:189-193 | If REST matrix is active, scan returns early. | Nothing at register level. |
| 15 | itu.c:83-86 | `getMsTimer` double-read loop, used by the VT100 ESC timeout (keyboard_vt100.cc:40,74) and USB repeat (keyboard_usb.cc:521). | Stable 0x10000022/23 between reads (ITU doc). |

Values that change control flow but are harmless: BLING_RX_FLAGS bit2 only changes LED menu headings (led_strip.cc:497). C64_PLD_STATE0/1 values are only copied (overlay.h:101-102).

## Interrupts    (ITU irq bit / high irq number, raise + ack protocol)

- Chargen/overlay, keyboard matrix (COL/ROW/JOY), MATRIX_KEYB, palettes: **no interrupts**. Everything is polled.
- HDMI hot-plug: ITU high IRQ **5** (`ITU_IRQHIGH_HDMI`, itu.h:45), installed at u64_config.cc:971. Dispatch reads ITU_IRQ_HIGH_ACT 0x10000028 and calls each set bit's handler; with no handler it clears the bit in ITU_IRQ_HIGH_EN 0x10000027 (riscv_main.c:118-129). No generic ack: `hpd_monitor_irq` acks by writing U64_HDMI_REG = 0x08 (HPD_RESET) and wakes the HPD task (u64_config.cc:994-1000). The task waits 200 ms, reads EDID over I2C and reconfigures HDMI (u64_config.cc:1002-1010). For the emulator: raise bit 5 only on a simulated hot-plug and drop it when 0x08 is written. T0/T1 need not raise it.
- Blingboard high IRQ 4 (itu.h:44) is defined but not installed in this build (no `install_high_irq(ITU_IRQHIGH_BLING…)`).
- USB keyboard data arrives via ITU irq bit 0x04 → `usb_irq` (riscv_main.c:106-108) → HID driver → `Keyboard_USB::process_data`. See the USB doc.

## Functional model  (protocol/state machine the emulator needs for real function)

### A. Overlay show/hide state machine
- Hidden: TRANSPARENCY = 0x00 (overlay.h:98). Visible: 0xC0 = overlay_on, own_keyboard, transparent index 0 (overlay.h:92; regs.vhd:72-75).
- Open (ultimate.cc:166-201): a push (button edge / USB F10 or ScrollLock / REST / command interface) plus HPD bit2 plus ITYPE==1 → `enableMatrix(false)` (USB/REST keys stop reaching the C64, keyboard_usb.cc:997-1011) → `run_once`: `buttonDownFor(1000)` → `take_ownership` (W 0xC0) → poll loop every 15 ms (userinterface.cc:304-358).
- Close: button edge while open (userinterface.cc:326-332), or TreeBrowser root returns MENU_HIDE/EXIT: RUN/STOP, `←`(`` ` ``), KEY_MENU → HIDE (tree_browser.cc:427-434); F8 → EXIT (:435-437); ESC / F10 / ScrollLock → HIDE (:520-524). `allow_exit=false` for the root browser (tree_browser.cc:39). Then `release_ownership` (W 0x00 plus PLD copy), `wait_free()`, `enableMatrix(true)` (ultimate.cc:200).
- `own_keyboard` (bit6) is a chargen output (peripheral_12.vhd:70). Presumably it detaches the physical keyboard from the C64 core while the menu is open → OPEN QUESTION 6. For a later C64 emulator: while bit6=1, do not feed host keys to the C64 CIA.

### B. Rendering the overlay into RGBA (open-IP semantics, char_generator_slave12.vhd)
Register latches: `cols=R[4]`, `rows=R[5]&0x3F`, `cw=R[2]&0x0F`, `h=R[3]&0x1F`, `big=R[3]>>6&1`, `stretch=R[3]>>7`, `ptr=((R[0xA]&0x7F)<<8)|R[0xB]`, `transp=R[0xD]&0x0F`, `visible=R[0xD]>>7`.

```
if !visible: draw nothing (C64 video only)
lines = big ? [y for y in 0..h-1 if (y&3)!=3] : [0..h-1]          # slave12.vhd:110-119
for r in 0..rows-1, c in 0..cols-1:
  i    = ptr + r*cols + c;  code = SCR[i];  attr = COL[i]           # slave12.vhd:111,186
  g    = code & 0x7F;  rev = code >> 7                              # :188-192
  for (ly, y) in enumerate(lines):
    if big:     bits = (FONT12[g*8 + (y>>2)] >> (12*(y&3))) & 0xFFF    # :141-148, :191
    elif stretch: bits = ROM8[g*8 + ((y>>1)&7)]                       # :188
    elif (y&8)==0: bits = ROM8[g*8 + (y&7)]                           # :189
    else:       b = ROM8[g*8+7]; bits = (b==0x18) ? b : 0             # :190, :151-153 (only line 8 of 0x09)
    for x in 0..cw-1:
      on  = ((bits >> (cw-1-x)) & 1) != rev                           # MSB-left; :131, :159
      idx = on ? (attr & 0xF) : (attr >> 4)                           # :160, :167
      if idx == transp: transparent pixel (show C64 video / black)    # :161-172
      else: rgb = HDMI_PAL[idx]  (0x10145000 + idx*4 → R,G,B)
      put(c*cw + x, r*len(lines) + ly)
```

Cell geometry written by `DetermineOverlaySettings` (u64_config.cc:2974-3007), always 40×25:

| HDMI mode (CFG_HDMI_RESOLUTION) | X_ON,Y_ON | CHAR_WIDTH | CHAR_HEIGHT | cell px | grid px |
|---|---|---|---|---|---|
| 0 SD, PAL/NTSC-50 | 386,307 | 8 | 0x09 | 8×9 | 320×225 |
| 0 SD, 60 Hz (**default**) | 300,235 | 8 | 0x09 | 8×9 | 320×225 |
| 1 720p | 958,160 | 8 | 0x90 (stretch) | 8×16 | 320×400 |
| 2 1080p | 1438,240 | 12 | 0x5E (big, h=30) | 12×23 (see OQ 5) | 480×575 |
| 3 800×600 | 450,355 | 8 | 0x09 | 8×9 | 320×225 |
| 4 1024×768 | 674,325 | 8 | 0x90 | 8×16 | 320×400 |
| 5 1280×1024 | 745,330 | 12 | 0x5E | 12×23 | 480×575 |

X_ON/Y_ON count in output pixels from the sync pulse: the chargen's counters restart on the rising edge of h_sync
and v_sync (`char_generator_timing.vhd:108-113`), so the active area starts `sync + back porch` in. With the timing
of `SetVideoMode`/`SetVideoMode1080p` (hdmi_scan.cc:34-35, :62-67) every mode above puts the 40×25 window right of
centre and below the middle — SD PAL 386-(64+68) = 254 of 720 across, 307-(5+39) = 263 of 576 down; 1080p
1438-(44+148) = 1246 of 1920 across. A photo of a U64 with a game running and the menu open (2026-09-20) shows
exactly that: a window in the lower right, the game visible through it. The emulator renders the output mode as
its canvas and places the window there (OQ 4 answered).

Fonts (not in the firmware ELF; they are FPGA ROMs):
- 8×8, 128 glyphs: `roms/chars.bin` bytes 0..1023. Verified byte-identical to `char_generator_rom_pkg.vhd` bytes 0..1023. The two files differ only at 1176-1182, which the 11-bit address `'0' & code(6:0) & row(2:0)` never reaches (slave12.vhd:188-190). Bit 7 = leftmost pixel. The same `chars.bin` is linked into the app as `_chars_bin_start`, used only by the C64 Freeze host (c64.cc:123,143; Makefile:238).
- 12×24, 128 glyphs: `fpga/ip/video/vhdl_gen/font_pkg.vhd` `c_font(0 to 128*8-1)` of 36-bit words (font_pkg.vhd:8). Row r of glyph g = word g*8+r. Sub-row s∈{0,1,2} = `(word >> 12*s) & 0xFFF`, bit 11 leftmost (checked against the ASCII-art comments: 'A' renders correctly). Generated from a PNG plus glyphs 0x0B,0x12,0x14-0x1F copied from chars.bin (vhdl_gen/png2vhd.py).

Glyph codes written: ASCII 0x20-0x7E, plus line graphics 0x01-0x13 (`CHR_*`, screen.h:6-24). Bit 7 = reverse (screen.cc:291-294) and cursor (XOR 0x80, screen.cc:130-157).

Colours: `default_colors[16][3]` (u64_config.cc:54-70): 0 000000, 1 F7F7F7, 2 8D2F34, 3 6AD4CD, 4 9835A4, 5 4CB442, 6 2C29B1, 7 EFEF5D, 8 984E20, 9 5B3800, A D1676D, B 4A4A4A, C 7B7B7B, D 9FEF93, E 6D6AEF, F B2B2B2. Always use the last 16×3 written to 0x10145000 (.VPL palettes are loaded via `load_palette_vpl`, u64_config.cc:1113-1118).

Colour schemes `schemes[]` {border,bg,fg,sel,sel_bg,sel_rev,status,inactive,config} (userinterface.cc:203-208). CFG_USERIF_COLORSCHEME default **1** (userinterface.cc:126):
0 "Standard Blue" {14,6,14,1,6,0,12,12,7}; 1 "Ultimate Black" {0,0,12,1,6,0,6,6,7}; 2 "C128" {13,11,15,13,0,0,15,12,7}; 3 Telnet {0,0,15,13,0,0,15,12,7}. `Overlay` does not override `set_colors`, so border/bg are not hardware (host.h:24). Cell bg is 0 by default (`Window` ctor screen.cc:374, `clear` screen.cc:353). With TRANSPARENCY=0xC0 all bg-0 pixels are transparent (OQ 3, answered: the photo above shows the game through the menu).

Escape protocol inside the byte stream (not stored): `ESC R` reverse on, `ESC r` reverse off, `ESC B` → bg 2 (bug noted in source), `ESC x` → fg = x&15 (screen.cc:227-248).

Text-only alternative: read 0x10141000[0..999] and 0x10142000[0..999] directly. The REST endpoint `GET /v1/machine:menu_screen` returns the same 2000 bytes from a shadow copy (route_machine.cc:430-445; userinterface.cc:550-582).

### C. Keyboard matrix (U64II_KEYB_COL / ROW / JOY) — primary register-level input
Polled by `Keyboard_C64::scan()` from `getch()` every 4 ticks = 20 ms (`vTaskDelayUntil(…,4)`, keyboard_c64.cc:316-319), **only while the overlay is owned** (`is_accessible()` = `enabled`, keyboard_c64.cc:185-186; overlay.h:83-85, 93, 103). The C64 matrix cannot open the menu.

Per-scan bus pattern (keyboard_c64.cc:196-273): W32 0x1010030B=0; W 0x10100802=0x01; W ROW=0xFF ×2; W COL=0xFF ×2; R JOY ×2; if joystick idle: W COL=0, R ROW; if ≠0xFF, for y=0..7: W COL=0xFF, W COL=~(1<<y) ×2, then the stable-read loop; finally W32 0x1010030B=wasd, W 0x10100802=0x00.

Emulator model:
```
state[8]  : bit b of state[a] = key at (a,b) pressed        (a = COL bit, b = ROW bit)
on W COL  : col = value
on R ROW  : v = 0xFF; for a in 0..7: if !(col>>a & 1): v &= ~state[a]; return v
on R JOY  : 0xFF & ~joy_port2_bits (bit0 up, 1 down, 2 left, 3 right, 4 fire)
```
Matrix index a*8+b → key (keymap_normal, keyboard_c64.cc:27-36; modifiers keyboard_c64.cc:16-25):

| a\b | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|---|
| 0 | INST/DEL | RETURN | CRSR→ | F7 | F1 | F3 | F5 | CRSR↓ |
| 1 | 3 | W | A | 4 | Z | S | E | LSHIFT(mod1) |
| 2 | 5 | R | D | 6 | C | F | T | X |
| 3 | 7 | Y | G | 8 | B | H | U | V |
| 4 | 9 | I | J | 0 | M | K | O | N |
| 5 | + | P | L | − | . | : | @ | , |
| 6 | £(`\`) | * | ; | HOME | RSHIFT(mod1) | = | ↑(`\|`) | / |
| 7 | 1 | ←(`` ` ``) | CTRL(mod4) | 2 | SPACE | C=(mod2) | Q | RUN/STOP |

Keymap selection: no modifier → normal; shift → `keymap_shifted` (CRSR←/↑, F2/F4/F6/F8, INST); C= or CTRL → `keymap_control` (keyboard_c64.cc:49-80). UI keycodes: keyboard.h:22-98. `keymapper` turns F1→PGUP, F3→HELP, F5→TASKS, F7→PGDN, F2→CONFIG, F4→SYSINFO, F6→SEARCH (userinterface.cc:853-861). With navmode 1, W/A/S/D become cursors (userinterface.cc:841-851).
Joystick (keyboard_c64.cc:213-219): up → KEY_UP, down → KEY_DOWN, left → KEY_LEFT, right → KEY_RIGHT, fire → RETURN.

Timing (keyboard_c64.cc:105-111, 286-309): the first scan that sees a key queues it (buffer 16). A held key repeats on the 17th following scan (~340 ms), then every 5th scan (~100 ms). The same key is accepted again only after a scan with no key (mtrx_prev reset, :256-258). A modifier change on the same key is ignored and restarts the delay (:294-297). Injection recipe: hold 40-200 ms, release ≥ 40 ms. Shifted keys: press the modifier in the same or an earlier scan.
`getch` falls back to `system_usb_keyboard.getch()` when its buffer is empty (keyboard_c64.cc:325-327).

### D. MATRIX_KEYB 0x10100300 (output toward C64 core)
`applyMatrixState` writes [i] = local (USB, only while matrixEnabled and no REST control) | REST state, [8] = reset (USB modifiers == 0x0F), [9] = restore (F12 / REST), [10] = freeze (F11) (keyboard_usb.cc:214-229, 394-435). Row/bit layout is the same a*8+b as table C, active-high (`keymap_usb2matrix`, keyboard_usb.cc:111-124: USB 'a' 0x04 → 0x0A). While the menu is open, the local part is forced to 0 (ultimate.cc:198). T1: latch only; later feed the C64 emulator's CIA.

### E. Other input paths (non-register or other blocks)
- USB HID keyboard (USB doc): `process_data` → keycodes (keyboard_usb.cc:464-504, maps :65-109). F10 or ScrollLock opens the menu (ultimate.cc:172-176), CTRL-D swaps disk, ESC closes. USB TAB = 0x09 = `KEY_CTRL_I` toggles ITYPE in the browser.
- REST (network doc): `PUT /v1/machine:menu_button` → closes if open (`push_active_menu_button`, ultimate.cc:53-60), else `C64_PUSH_BUTTON` → `c64->setButtonPushed()` → opens on the next loop, no button register needed (route_machine.cc:65-78; c64_subsys.cc:207-208). `POST /v1/machine:input` while the menu is open → `system_usb_keyboard.push_head(key)` (route_input.cc:323-354) → reaches the UI through the fallback in C. Command interface `CTRL_CMD_FREEZE` does the same push (control_target.cc:129-131).
- Telnet (network doc): InitFunction prio 100 creates `SocketGui` (socket_gui.cc:29); listens on TCP 23 when `CFG_NETWORK_TELNET_SERVICE` ≠ 0 (socket_gui.cc:70, 249-252). Max 4 sessions (:41). Optional password (:79-165). Each session gets its own `UserInterface(title,false)` on `HostStream` (socket_gui.cc:184-200), forced scheme 3 (userinterface.cc:212). Screen is VT100 60×24 (screen_vt100.h:25-26). Colours are ANSI sequences (screen_vt100.cc:29-40). Codes <0x20 go through the DEC line-drawing set via `ESC(0` (screen_vt100.cc:74, 91-96). Keys: ESC[A-D cursors, ESC[n~ F-keys/Home/Ins/PgUp, ESC O P-S F1-F4, 0x7F → BACK, 0x12 → KEY_CTRL_R, a lone ESC after 150 ms (keyboard_vt100.cc:13-140; .h:54). No hardware access; independent of overlay/HPD/ITYPE.

## Emulator model tiers  (T0: boot without hang / T1: functional; what each needs)

**T0 — boots, UI task alive, overlay RAM populated**
- Capabilities include 0x00000200 and 0x04000000 (hazards 1-2).
- 0x10140000-0x10140FFF: write-latch 14 regs (addr&0xF), reads 0. 0x10141000/0x10142000: 4 KiB R/W RAM each.
- 0x10145000-0x1014503F, 0x10180800-0x10180C3F, 0x10144000-0x1014401D, 0x10148000-03: write-accepting RAM.
- 0x1000000A → 0x00. 0x10100400 → 0x04 (writes ignored). 0x10100402 → 0x00.
- 0x10100406 → 0xFF; 0x1010040B → 0xFF (stable); 0x1010040A writes latched.
- 0x10100300-0x1010030E: RAM, including the 32-bit write at 0x1010030B.
- 0x10100800-03: RAM, RX_FLAGS read bit2 = 0. 0x10181000-0x10181011: RAM.
- C64 window 0x1005DC01 → 0xFF (boot hotkey).

**T1 — visible, navigable menu**
- Renderer per B. Fonts from roms/chars.bin[0:1024] and font_pkg.vhd. Palette from 0x10145000. Visibility = 0x1014000D bit7. Transparent index = low nibble.
- The headless `text_dump` prints screen RAM whether or not the overlay is visible (the RAM is populated while hidden, step 9 of the boot sequence). Check `regs[0xD]` bit7 for visibility.
- The selection is colour-only in the default scheme: `CFG_USERIF_COLORSCHEME` defaults to 1 (userinterface.cc:126), and `schemes[1]` has selected 1, selected_bg 6, selected_rev 0 (userinterface.cc:203-208,216-224). `draw_item` sets colour, reverse and background from these (tree_browser_state.cc:157-159), so the screen codes do not change. A text dump cannot show the cursor; use colour RAM or a rendered image.
- Menu button: host key → 0x1000000A bit6 pulse (50-200 ms).
- Host keyboard → 8×8 matrix per C (+ joystick via 0x10100406). Hold/release timing as in C.
- `CFG_USERIF_ITYPE=1` in the flash config (hazard 4), or implement C64 STOP/PHI2 enough to survive one Freeze entry (hazard 5).
- HPD bit fixed at 1. Optional: high IRQ 5 hot-plug.

**T2** — USB HID keyboard (USB host doc), REST/telnet (network docs), C64-core Freeze UI (screen 0x10050400 / colour 0x1005D800, c64.h:192-193), MATRIX_KEYB → C64 CIA, `own_keyboard` gating.

## Open questions

1. U64-II top level is closed. The firmware's addrbits=12 (ultimate.cc:125) and 12-px fonts match `char_generator_peripheral_12` with g_screen_size=12 and g_color_ram=true, but the instance and generics cannot be confirmed.
2. Does the overlay's 4-bit pixel index go through U64II_HDMI_PALETTE (0x10145000), or through another/fixed table?
3. **Answered (2026-09-20).** The HDMI mixer honours `pixel_opaque`: a photo of a U64 running Wasteland with the
   menu open shows the game through the menu's background, only the selected row opaque. The renderer's
   transparency is right; issue #3 was a roms directory whose `chars.bin` was the C64 character ROM.
4. **Answered (2026-09-20).** X_ON/Y_ON are output pixels counted from the sync pulse; the active area starts
   `sync + back porch` in (`char_generator_timing.vhd:108-113` with the mode tables of hdmi_scan.cc). The renderer
   composes at the output size and places the window there. Open only: how exactly the scaler maps the VIC picture
   into the active area (we stretch it; the hardware has `hscaler`/`vscaler` and the VIC cropper).
5. **Answered for the open IP (2026-09-21), and the cell is portrait.** `char_generator_slave12.vhd:104-119`: a row
   ends when `char_y = char_height-1`, and with `big_font` the counter goes `+2` where `char_y(1:0) = "10"`, so it
   counts 0,1,2,4,5,6,8,… With h=30 that is 23 scanlines per cell and sub-row 7.2 is never shown, exactly as this
   line suspected — the `_24` in the macro name is the font's row count, not the cell's. Horizontally the `draw`
   state emits `char_width` pixels at one per chargen clock, so 12 means 12 output pixels. A 40×25 window at 1080p
   is therefore **480×575, taller than it is wide**, and that is what the firmware asks for: Gideon's own U64-II
   tester uses the same 12/0x5E cell with 100×39 cells (`u64ii_programmer.cc:61-64`) to fill the screen, 1200×897.
   Open only for the closed U64-II RTL, and only between 23 and 24 scanlines (575 vs 600) — nothing in either
   reading makes the window landscape.
6. Semantics of 0x1010040A/0B/06 come from firmware usage only. Open points: does ROW also reflect keys injected via MATRIX_KEYB or the Blingboard? Does `own_keyboard` detach the C64 CIA from the matrix? Is read 0x10100406 (port-2 lines) really a separate function from write (swap bit)?
7. Can the FPGA raise ITU_BUTTON1 from USB F11 (MATRIX_KEYB[10] "freeze"), a C64-keyboard combo or the case switch? No firmware path exists. Help text mentions only reset by holding the switch up (userinterface.cc:106-110).
8. Blingboard (C64U keyboard, 0x10100800): RX_DATA/RX_GET are never read in this build. How does that keyboard reach the menu, if at all (via KEYB_ROW?).
9. Cleanest way to get CFG_USERIF_ITYPE=1 without the Freeze path: flash config page format (flash doc) or a REST config route (route_configs.cc not read).
10. Whether `TreeBrowser::poll_inactive → checkFileManagerEvent` (tree_browser.cc:161-165) redraws overlay RAM while hidden was not traced. Harmless for the model (RAM is always rendered-ready).
