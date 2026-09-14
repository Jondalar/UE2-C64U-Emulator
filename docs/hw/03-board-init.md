# U64-II Board Init: I2C, Audio Codec, HDMI/PLLs, Power, LEDs, Blingboard, Product Detection

Scope: everything `custom_hardware_init()` and `U64Config` do to board-level peripherals on the
U64-II build (`-DRISCV -DU64=2 -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000`), plus product /
board-revision / FPGA-type detection. C64 core, ITU, USB, RMII, overlay and ESP32 internals are only
covered where board init touches them; they have their own docs.

Base macros used throughout: `IOBASE = 0x10000000`, `U2P_IO_BASE = 0x10100000`
(target Makefile `OPTIONS`), `U64_IO_BASE = 0x10100400` (u64.h:15), `VID_IO_BASE = 0x10140000`
(u64.h:21), `C64_IO_BASE = 0x10180000` (u64.h:49), `C64_CARTREGS_BASE = 0x10040000` (iomap.h:14),
`C64_MEMORY_BASE = 0x10050000` (iomap.h:22), `USB_BASE = 0x10080000` (iomap.h:33).

---

## Sources read

| File | Key functions / content |
|---|---|
| software/portable/riscv/riscv_main.c | `main` (152-167), `vPortSetupTimerInterrupt` (169-187), IRQ dispatcher incl. high IRQs (82-134), `install_high_irq` (38-45) |
| software/portable/riscv/crt0.S | static constructors, then `main` (197-218) |
| software/system/u64ii_init.cc | `custom_hardware_init` (114-138), `calc_pll` (28-98), `ResetHdmiPll` (105-109), `SetVideoPll` (141-160), `SetExternalPLL` (162-171), `SetInternalPLL` MMCM (173-268) |
| software/io/i2c/i2c_drv.h / i2c_drv.cc | generic I2C transaction layer: probe/read/write/block/nau (i2c_drv.cc:124-399), lock/unlock (i2c_drv.h:46-63), channel ids (i2c_drv.h:78-80) |
| software/io/i2c/hw_i2c.h / hw_i2c_drv.h / hw_i2c_drv.cc | HW I2C master register struct + status bits, byte-level driver |
| fpga/io/i2c/vhdl_source/i2c_master.vhd | open VHDL of the HW I2C master (register semantics confirmed) |
| software/io/audio/nau8822.cc | `nau8822_init` (10-45) |
| software/io/usb/usb_hwinit.cc, usb_nano.h | `initialize_usb_hub`, `USB2513Init`, `USB2503Init` |
| software/u64/hdmi_scan.cc/.h | `SetScanModeRegisters`, `SetVicCrop`, `SetVideoMode1080p`, PLL blobs |
| software/u64/color_timings.cc/.h | `color_timings[]` table |
| software/u64/u64_config.cc/.h | `U64Config` ctor (892-992), `effectuate_settings` (1054-1162), SID socket detect + I2C (655-819), mixers (1342-1402), LED select (1413-1422), HPD/EDID (994-1010, 2577-2691), resampler (2215-2230), palette (2720-2764), overlay geometry (2974-3007), ESP32 power settings (1462-1600) |
| software/u64/led_strip.cc/.h | `LedStrip` task, map / data / intensity protocol |
| software/io/c64/keyboard_c64.cc | U64-II keyboard/joystick matrix scan, `BLING_RX_FLAGS` |
| software/system/product.cc/.h | `getBoardRevision`, `isEliteBoard`, `getProductId`, version strings |
| software/system/itu.c/.h | `getFpgaCapabilities`, `getFpgaVersion`, `getFpgaType`, `wait_ms`, CAPAB bits, high IRQ numbers |
| software/io/flash/w25q_flash.cc | 50T vs 100T flash layout selection |
| software/filetypes/filetype_u2p.cc | update-file acceptance by FPGA type |
| software/io/mdio/mdio.c, software/io/network/rmii_interface.cc | MDIO bit-bang, PHY probe at boot |
| software/io/c64/c64.cc/.h | `ConfigureU64SystemBus` (cart detect), `hard_stop`, `c64_reset_detect` |
| software/components/config.cc/.h, software/filesystem/blockdev_flash.cc | `U64_RESTORE_REG` safe mode |
| software/application/ultimate/ultimate.cc | `ultimate_main` ordering, keyboard/overlay wiring, HPD gate for UI |
| software/components/init_function.cc | init ordering |
| software/userinterface/system_info.cc | version display (reads only) |
| software/io/c64/c64_subsys.cc, software/io/wifi/wifi_cmd.cc/.h | U64-II power off / reboot via ESP32 |
| software/network/assembly.cc | `U64II_BLACKBOARD` gate |
| software/portable/riscv/bootloader_u64ii.c | (not in app ELF) PLL pre-init, FPGA-type app address, for reference |

Not compiled in this target (checked against Makefile SRCS_CC): `codec.cc` (SGTL5000), `rtc_i2c.cc`
(RTC at I2C 0xA2; build uses `rtc_dummy.cc`), `fpll.cc` (U64 mk1 PLL code; `SetHdmiPll` is only declared at u64ii_init.cc:110 and called in the `U64 != 2` branch, u64_config.cc:1149),
`u64ii_test.cc`, `audio_select.cc`, `update_common.h`. Their I2C/register accesses do not occur.

---

## Address map

### U2P misc GPIO page (0x10100000)

| Addr | W | R/W | Name | Meaning (source) |
|---|---|---|---|---|
| 0x10100006 | 8 | R | `U2PIO_GET_MDIO` | MDIO input; nonzero = 1 (u2p.h:68, mdio.c:109) |
| 0x10100008 | 8 | R/W | `U2PIO_SCL` | bit-bang SCL; **unused on U64-II** (Hw_I2C_Driver overrides all bit ops; i2c_drv.h:23-28 are virtual) |
| 0x10100009 | 8 | R/W | `U2PIO_SDA` | bit-bang SDA; unused on U64-II |
| 0x1010000A | 8 | W | `U2PIO_SET_MDC` | MDC level 0/1 (u2p.h:66, mdio.c:13-14) |
| 0x1010000B | 8 | W | `U2PIO_SET_MDIO` | MDIO drive 0/1 (1 = release/high) (u2p.h:67) |
| 0x1010000C | 8 | **R** | `U2PIO_BOARDREV` | board revision in bits 7:3 (`>>3`) (u2p.h:73, product.cc:44). On U2+ RTL bit0 reads back `speaker_en` (u2p_io.vhd:75-77); the firmware shifts it out |
| 0x1010000C | 8 | **W** | `U2PIO_SPEAKER_EN` | same address, write = speaker enable/volume (u2p.h:69). U64-II writes **0xFF** (see hazard H18) |
| 0x1010000D | 8 | W (R as delay) | `U2PIO_HUB_RESET` | 1 = assert USB hub reset, 0 = release; read only as delay (usb_hwinit.cc:76-91) |
| 0x1010000F | 8 | W | `U2PIO_ULPI_RESET` | 1 = ULPI reset, 0 = release, 0x80 = buffer enable (u2p.h:100, usb_hwinit.cc:147,165,168) |

### Matrix keyboard page (0x10100300) — only the part board code touches

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x1010030B | **32** | W | `MATRIX_WASD_TO_JOY` | `(volatile uint32_t*)(MATRIX_KEYB+0x0B)` — 32-bit store at a non-aligned address (u2p.h:98). Values 0x00/0x01/0x03 (u64_config.cc:1065-1066); 0 forced during C64 matrix scan (keyboard_c64.cc:197) |

### U64 board control, `U64_IO_BASE` = 0x10100400 (u64.h:65-80)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100400 | 8 | W | `U64_HDMI_REG` | 0x20 DDC enable, 0x10 DDC disable, 0x08 HPD reset (= HPD IRQ ack) (u64.h:93-95) |
| 0x10100400 | 8 | R | `U64_HDMI_REG` | bit2 `HPD_CURRENT`, bit3 `HPD_WASLOW` (latched; not read by this build) (u64.h:96-97) |
| 0x10100401 | 8 | W | `U64_POWER_REG` | 0x2B,0xB2 sequence = power off. **U64=1 only** (c64_subsys.cc:255-259); U64-II uses ESP32 |
| 0x10100402 | 8 | R | `U64_RESTORE_REG` | ==1 → safe mode, no flash disk (config.cc:48, blockdev_flash.cc:161) |
| 0x10100403 | 8 | R | `U64_CART_DETECT` | bit0 GAME, bit1 EXROM, active low; `(v&3)!=3` → external cart present (u64.h:68, c64.cc:1514) |
| 0x10100404 | 8 | W | `U64_HDMI_PLL_RESET` | write 3 then 0 = pulse (u64ii_init.cc:107-108) |
| 0x10100405 | 8 | W/R | `U64_USERPORT_EN` | 3 = user port power on, 0 = off; read back only for printf (u64_config.cc:1071-1074) |
| 0x10100406 | 8 | W | `U64II_KEYB_JOY` | joystick swap select `swap & 1` (u64_config.cc:1064, 2396) |
| 0x10100406 | 8 | R | `U64II_KEYB_JOY` | joystick lines bits4:0 active low (up,down,left,right,fire) (keyboard_c64.cc:208-219) |
| 0x10100407 | 8 | R | `U64II_BLACKBOARD` | bit0 must be 1 for Assembly64 client (assembly.cc:36) |
| 0x10100408 | 8 | W | `U64_HDMI_ENABLE` | 1 = HDMI mode, 0 = DVI (u64_config.cc:1091-1095, 2681-2685) |
| 0x10100409 | 8 | W | `U64_INT_CONNECTORS` | bit0 parallel cable, bits1-2 IEC burst mode (0..2), bits6:4 IEC wiring: 0x70 all, 0x60, 0x30, 0x50 (u64_config.cc:1098-1100) |
| 0x1010040A | 8 | W | `U64II_KEYB_COL` | keyboard column select, active low (like CIA1 $DC00) (keyboard_c64.cc:231-239) |
| 0x1010040B | 8 | R (+W 0xFF) | `U64II_KEYB_ROW` | keyboard row sense, active low (like CIA1 $DC01); firmware also writes 0xFF twice (keyboard_c64.cc:201-202) |
| 0x1010040C | 8 | W | `U64_LEDSTRIP_EN` | 1 = CIA_PWM pins become LED strip pins (u64.h:77, led_strip.cc:337-380) |
| 0x1010040D | 8 | W | `U64_PWM_DUTY` | 0xD8 when user port enabled, else 0x00 (u64_config.cc:1073) |
| 0x1010040E | 8 | W | `U64_CASELED_SELECT` | `(sel1<<4)|sel0`, sources index `ledselects[]` (u64_config.cc:286-288,1419); default 0x40 (u64_config.cc:376-377) |
| 0x1010040F | 8 | W | `U64_ETHSTREAM_ENA` | stream generators; written 0 in `U64Config` ctor (u64_config.cc:897) |

### Audio mixers / resampler (0x10100500..) — written by board init

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100500-0x10100513 | 8 | W | `U64_AUDIO_MIXER` | 10 sources × {R, L} gain; order UltiSID1, UltiSID2, Socket1, Socket2, SamplerL, SamplerR, Drive1, Drive2, TapeRead, TapeWrite (u64_config.cc:1349-1358, 437-446). Write-only, reads 0 (u64_config.cc:1333-1334) |
| 0x10100540-0x10100553 | 8 | W | `U64_SPEAKER_MIXER` | same layout, identical L/R value; all 0 if speaker disabled (u64_config.cc:1383-1402) |
| 0x10100580 | 32 | W | `U64_RESAMPLE_DATA` | coefficient push, 675 (PAL) or 512 (NTSC) words (u64_config.cc:2220-2227) |
| 0x10100584 | 8 | W | `U64_RESAMPLE_RESET` | write 1 |
| 0x10100588 | 8 | W | `U64_RESAMPLE_LABOR` | 45 (PAL) / 169 (NTSC) |
| 0x10100589 | 8 | W | `U64_RESAMPLE_FLUSH` | 1 during load, 0 to finish |

### LED strip controller, `C64_IO_LED` = 0x10100600 (u64.h:19, led_strip.h:25-28, led_strip.cc:150-154)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100600-0x101006FB | 8 | W | `LEDSTRIP_DATA[0..251]` | 84 LEDs × 3 (R,G,B). When MAP=1, writes go to the address map instead of colour data |
| 0x101006FC | 8 | W | `LED_MAP` | 1 = map-write mode, 0 = data mode; after map setup written 0x40 (WS2812) or 0x80 (APA102) (led_strip.cc:447-451) |
| 0x101006FD | 8 | W | `LED_STARTADDR` | data offset of newest colour ("FROM") |
| 0x101006FE | 8 | W | `LED_INTENSITY` | `intensity<<2`, intensity 0..31 (default 8) |
| 0x101006FF | 8 | W | `LED_START` | any write = start transfer |

### HW I2C master, `U64II_HW_I2C_BASE` = 0x10100700 (u64.h:34, hw_i2c.h:5-15, i2c_master.vhd)

| Addr | W | R/W | Name | Meaning (VHDL line) |
|---|---|---|---|---|
| 0x10100700 | 8 | W | `data_out` | if not started: START + transmit byte; else transmit byte (vhd:163-174) |
| 0x10100700 | 8 | R | `data_out` | shift register: received byte after RX; 0xFF after TX (TX shifts in '1's, vhd:216) (vhd:300-301) |
| 0x10100701 | 8 | R | `status` | bit7 BUSY (state≠idle), bit0 STARTED, bit2 ERROR (NACK), bit3 MISSED_WRITE, bit4 TEMP_ERROR; reading clears bit2 and bit3 (vhd:302-313) |
| 0x10100702 | 8 | W | `repeated_start` | any value → repeated-start sequence; STARTED stays 1 (vhd:176-177, 267-274) |
| 0x10100703 | 8 | W | `stop` | any value → STOP, STARTED=0 (vhd:179-180, 257-265) |
| 0x10100704 | 8 | W/R | `receive` | receive 8 bits then NACK; read = data_out (vhd:182-185) |
| 0x10100705 | 8 | W/R | `receive_ack` | receive 8 bits then ACK; read = data_out (vhd:187-190) |
| 0x10100706 | 8 | W/R | `channel` | bits1:0 bus select (0 HDMI, 1 1V8, 2 3V3; 4 buses in VHDL); only accepted when idle (vhd:192-193, 314-315) |
| 0x10100707 | 8 | W | `soft_reset` | bit0 = reset state machine (vhd:282-283); read returns 0 (commented X"5B", vhd:316-317) |
| 0x10100708 | 8 | W | `scan_enable` | bit0 = hand bus to FPGA keyboard scanner (`scan_en` output, vhd:285-286); reset → 0 |

Commands written while not idle are dropped and set MISSED_WRITE (vhd:288-291). Bit timing: tick =
clock/400 kHz, 4 ticks per bit → ~100 kHz SCL (vhd:28, 50-59); clock stretching honoured (vhd:88-90).

### Blingboard (u64.h:35-43)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100800 | 8 | R | `BLING_RX_DATA` | not used in this build |
| 0x10100801 | 8 | W | `BLING_RX_GET` | not used in this build |
| 0x10100802 | 8 | R | `BLING_RX_FLAGS` | bit2 = Blingboard installed (u64.h:43); only affects a menu heading (led_strip.cc:497-501) |
| 0x10100802 | 8 | W | `BLING_RX_FLAGS` | 0x01 = disable shift-lock during matrix scan, 0x00 = re-enable (keyboard_c64.cc:198, 261, 273, 367, 378) |
| 0x10100803 | 8 | W | `BLING_RX_IRQEN` | not used in this build |
| 0x10100900 | ? | – | `U64II_BLINGBOARD_LEDS` | defined (u64.h:36), never accessed |

The RX_DATA/GET/FLAGS/IRQEN layout mirrors the ITU UART (itu.h `UART_DATA/GET/FLAGS/ICTRL`) → a UART-style
receiver is likely, but unused here (OPEN QUESTION Q4).

### HDMI video pipeline, `VID_IO_BASE` = 0x10140000

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10140000.. | – | – | `U64II_OVERLAY_BASE` | overlay char generator (separate doc) |
| 0x10144000-0x1014401D | 8 | W | `U64II_HDMI_REGS` (`t_video_timing_regs`, u64.h:172-203) | +00 HSYNCPOL, +01 HSYNCTIME, +02 HBACKPORCH, +03 HACTIVE, +04 HFRONTPORCH, +05 HREPETITION, +06 resync, +07 VID_Y, +08 VSYNCPOL, +09 VSYNCTIME, +0A VBACKPORCH, +0B VACTIVE, +0C VFRONTPORCH, +0D VIC, +0E REPCN, +0F IRCYQ, +10 x_offset, +11 tx_swing, +12 gearbox, +13 hscaler, +14 vscaler, +15 VID_A, +16 VID_B, +17 VID_S, +18 VID_C, +19 VID_M, +1A VID_R, +1B VID_EC, +1C VID_SC, +1D VID_YQ |
| 0x10145000-0x1014503F | 8 | W | `U64II_HDMI_PALETTE` | 16 × {R,G,B,pad} (u64_config.cc:2733-2739) |
| 0x10148000-0x10148003 | 8 | W | cropper `t_vic_crop_regs` | offset_x, offset_y, size_x = x_size>>1, size_y = y_size>>1 (hdmi_scan.cc:45-60) |

### Internal PLL (Xilinx MMCM DRP) — `IOBASE`+0x200000 (u64ii_init.cc:173-174)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10200000 + 2·idx | 16 | W | `MMCM[idx]` | `uint16_t*` array, so DRP index idx lands at byte address 2·idx. Indices used: 0x08,0x09,0x13,0x14,0x15,0x16,0x18,0x19,0x1A,0x28,0x4E,0x4F → 0x10200010, …12, …26, …28, …2A, …2C, …30, …32, …34, …50, …9C, …9E |
| 0x102000FF | 8 | W | `MMCM_RESET` | 0xB3 then 0x3B (u64ii_init.cc:266-267); no lock poll |

### C64-core / ITU registers read or written by board init (owned by other docs)

| Addr | W | R/W | Name | Use here |
|---|---|---|---|---|
| 0x10000006 | 8 | R/W | `ITU_TIMER` | `wait_ms` polls to 0 (itu.c:63-71) |
| 0x1000000B | 8 | R | `ITU_FPGA_VERSION` | display only (system_info.cc:174) |
| 0x1000000C-0x1000000F | 8 | R | capabilities bytes 31:24 … 7:0 | `getFpgaCapabilities` (itu.c:19-29): FPGA type = bits 29:28 = byte 0x1000000C bits 5:4; `CAPAB_ULTIMATE64` bit26 = 0x1000000C bit2; `CAPAB_ETH_RMII` bit24 = 0x1000000C bit0; `CAPAB_CARTRIDGE` bit9 = 0x1000000E bit1 |
| 0x10000027 / 0x10000028 | 8 | R/W | `ITU_IRQ_HIGH_EN` / `_ACT` | high IRQ enable/active (itu.h:29-30) |
| 0x10040000 | 8 | W | `C64_MODE` | 0x08 UNRESET before SID detect (u64_config.cc:661) |
| 0x10040001 | 8 | R/W | `C64_STOP` | `hard_stop` waits for bit1 (c64.cc:399-409) |
| 0x10040003 | 8 | R | `C64_CLOCK_DETECT` | bit4 RESET_SENSE polled (c64.h:57, 92, 332-334) |
| 0x1005D012 | 8 | R | C64 `$D012` via DMA | raster wait in SID detect (u64_config.cc:2239) |
| 0x1005D400.. / 0x1005D500.. | 8 | R/W | SID sockets via DMA | FPGASID/ARMSID/SwinSID/PDsid/SIDKick probes (u64_config.cc:572-653) |
| 0x1005DC00-0x1005DC03 | 8 | R/W | CIA1 via DMA | boot hotkey scan (u64_config.cc:950-963) |
| 0x10180000-0x1018002F | 8 | W | C64 core cfg | SCANLINES +00, VIDEOFORMAT +01, TURBOREGS_EN +02, PHASE_INCR +05, SID bases/masks +08..+0F, CORE_VERSION +10 (R), SID_EN +11/+12, PADDLE_EN +13, PADDLE_SWAP +1A, SPEED_PREFER +2D, SPEED_UPDATE +2E, VIC_SPLIT +2F (u64.h:104-144) |
| 0x10180080-0x10180087 | 8 | R | `C64_VOICE_ADSR(x)` | LED strip SID-music mode input (led_strip.cc:323-325) |
| 0x10180800-0x1018083F / 0x10180C00-… | 8 | W | `C64_PALETTE` RGB / YUV | (u64_config.cc:2724-2731, 2760) |
| 0x1018101E | 8 | R | `C64_PLD_JOYCTRL` | only if board rev == 0x14 (product.cc:73-78) |
| 0x10080000-0x100807FF / 0x10080800 | 16 / 8 | W | USB nano RAM / `NANO_START` | cleared / stopped before hub init (usb_hwinit.cc:146-151; usb_nano.h:15,54) |

---

## I2C device inventory (all addresses 8-bit write form as used by the code)

| Ch | 8-bit addr (7-bit) | Device (per source) | Access | What firmware needs back |
|---|---|---|---|---|
| 1 (1V8) | 0x34 (0x1A) | NAU8822 audio codec (nau8822.cc:5) | 2-byte 9-bit writes only | ACK (NACK → only printf, i2c_drv.cc:266-288) |
| 2 (3V3) | 0x58 (0x2C) | SMSC/Microchip USB2513 hub (usb_hwinit.cc:7) | probe, SMBus block writes | ACK on probe selects USB2513 path (usb_hwinit.cc:157-158) |
| 2 (3V3) | 0x5A (0x2D) | USB2503 hub, fallback (usb_hwinit.cc:8) | probe, byte writes | probed only if 0x58 NACKs |
| 1 (1V8) | 0x40 (0x20) | 8-bit I/O expander: reg1 output, reg2 polarity, reg3 config (u64ii_init.cc:128-130). Drives SID socket regulator/shunt/caps (u64_config.cc:770-811) | byte writes | ACK |
| 1 (1V8) | 0x42 (0x21) | 16-bit I/O expander: reg2 out0, reg6 cfg0, reg7 cfg1 (u64ii_init.cc:132-136); keyboard matrix "Column is A (drive), Row is B (read)" | byte writes by CPU; FPGA scanner when `scan_enable`=1 | ACK; for real keys see Q2 |
| 1 (1V8)* | 0xC8 (0x64) | video PLL (C64 clock from 24 MHz ref) (u64ii_init.cc:12, 155-158) | SMBus block write reg 0x14 len 8 | ACK |
| 0 (HDMI)* | 0xC8 (0x64) | external HDMI PLL (u64ii_init.cc:162-170) | byte 0x81, block 0x14 len 8, byte 0x81 | ACK |
| 0 (HDMI) | 0xA0/0xA1 (0x50) | monitor EDID (u64_config.cc:2594, 2608) | read block 128 bytes ×N | only when HPD=1; bytes 0-7 = EDID header, byte 126 = ext count |
| 0 (HDMI) | 0x60 (0x30) | E-DDC segment pointer (i2c_drv.cc:362) | write page byte | only for EDID ext blocks ≥2 |

\* On board revision 0x15 the two PLLs swap channels: video PLL on ch0, HDMI PLL on ch1
(u64ii_init.cc:157, 166; bootloader_u64ii.c:103, 117).

Register semantics of the PLL (P/N/R/Q/VCO-range layout, byte-count block write, reg 0x01 via command
0x81 = "LVCMOS input, powerdown/powerup") match a TI CDCE913-family part; this is an inference from the
code, not stated in source (Q5). Same for the PCA9554/PCA9535-style expanders.

---

## Init / boot sequence as seen from the bus

Only the app ELF (`ultimate`) is considered. The separate bootloader (bootloader_u64ii.c) also programs
both PLLs and turns scan off (bootloader_u64ii.c:87-131, 162-166). The app re-programs everything, so
skipping the bootloader loses nothing board-level.

Generic byte-level expansion used below (hw_i2c_drv.cc):
- `start(ch)`: W 0x10100706=ch; R 0x10100701 until bit7=0 (hw_i2c_drv.cc:12-16)
- `tx(b)`: W 0x10100700=b; R 0x10100701 until bit7=0; bit2 → NACK (hw_i2c_drv.cc:31-37)
- `rs`: W 0x10100702=1; poll (18-22) · `stop`: W 0x10100703=1; poll (25-29)
- `rx(ack)`: W 0x10100705=1 (ack) / 0x10100704=1 (nack); poll; R 0x10100700 (39-48)
- `write_byte(A,r,v)` = start,tx(A),tx(r),tx(v),stop · `write_nau(A,r,v9)` = start,tx(A),tx((r<<1)|(v9>>8)),tx(v9&FF),stop ·
  `write_block(A,r,buf,n)` = start,tx(A),tx(r),**tx(n)**,tx(buf…),stop · `probe(A)` = start,tx(A),stop ·
  `read_block(A,r,n)` = start,tx(A),tx(r),rs,tx(A|1),rx(ack)…rx(nack),stop (i2c_drv.cc:124-354).
  Each of these aborts with a stop on the first NACK.

### Phase A — `main()` before the scheduler (riscv_main.c:152-156)

1. crt0 runs static constructors, then `main` (crt0.S:197-218). `puts("-- Custom Hardware Init --")`.
2. `new Hw_I2C_Driver(0x10100700)` (u64ii_init.cc:116). The base ctor creates a FreeRTOS mutex, no I/O
   (i2c_drv.h:31-36). **W 0x10100708 = 0** (scan off) (hw_i2c_drv.h:17).
3. `nau8822_init(ch 1)` (u64ii_init.cc:120 → nau8822.cc:10-45), address 0x34 (bytes sent after the address):
   | reg | 9-bit value | bytes |
   |---|---|---|
   | 0x00 reset | 0x000 | 00 00 |
   | *`wait_ms(2)`*: W 0x10000006=200, poll until 0, ×2 (nau8822.cc:18, itu.c:63-71) | | |
   | 0x01 | 0x0CF | 02 CF |
   | 0x02 | 0x03F | 04 3F |
   | 0x03 | 0x18F | 07 8F |
   | 0x04 | 0x050 | 08 50 |
   | 0x06 | 0x000 | 0C 00 |
   | 0x07 | 0x000 | 0E 00 |
   | 0x0A | 0x008 | 14 08 |
   | 0x0E | 0x108 | 1D 08 |
   | 0x2F | 0x005 | 5E 05 |
   | 0x30 | 0x005 | 60 05 |
4. `initialize_usb_hub()` (u64ii_init.cc:123 → usb_hwinit.cc:144-169):
   - W 0x10080800 (`NANO_START`) = 0; W 0x1010000F = 1; 1024 × W16 0 to 0x10080000..0x100807FE.
   - ch 2: `probe(0x58)`. If ACK → `USB2513Init` (79-115): W 0x1010000D=1, 50×R 0x1010000D, W 0x1010000D=0,
     50×R; `write_block(0x58,0xFF,[02],1)` (reset); `write_block(0x58,0x00,24 bytes)` with
     `24 04 13 25 A0 0B 9A 20 02 00 00 00 01 32 01 32 32 00 00 00 00 00 00 00` (71-74);
     `write_block(0x58,0xFF,[01],1)` (attach). Each failure → puts + return.
   - else `probe(0x5A)` → `USB2503Init` (same reset pulses; 17 `write_byte(0x5A, reg, val)` from table 51-69).
   - else `puts("No USB hub found.")` (162). None of these branches affects later control flow.
   - W 0x1010000F = 0, then W 0x1010000F = 0x80 (165-168).
5. **W 0x10100400 = 0x20** (DDC enable) (u64ii_init.cc:126). Not switched off again until an EDID read.
6. ch 1: `write_byte(0x40,0x01,0x00)`, `(0x40,0x03,0x00)`, `(0x40,0x02,0x00)` (u64ii_init.cc:127-130).
7. ch 1: `write_byte(0x42,0x06,0x00)`, `(0x42,0x07,0xFF)`, `(0x42,0x02,0x00)` (u64ii_init.cc:134-136).
8. Return; `xTaskCreate(ultimate_main)`, `vTaskStartScheduler` → `vPortSetupTimerInterrupt` programs ITU (riscv_main.c:159-187).

### Phase B — `ultimate_main` (ultimate.cc:79-)

9. R 0x1000000C-F (capabilities) (ultimate.cc:87); `getProductVersionString` R 0x10180010 `C64_CORE_VERSION`
   (product.cc:138-140). Print only.
10. `InitFunction::executeAll()` sorted ascending by ordering (init_function.cc:29-50). Board-relevant:
    ordering 1 "U64 Config" (u64_config.cc:93), 9 "U64 Palette" (2880), 51 "RMII Interface"
    (rmii_interface.cc:28), 61 "LED Strip" (led_strip.cc:50). ConfigManager is a function-static singleton
    built on the first `register_store` (config.h:291-294, 331). Its ctor reads **0x10100402** (config.cc:48).

### Phase C — `U64Config::U64Config()` (u64_config.cc:892-992), ordering 1

11. W 0x1010040F = 0 (897).
12. Gate: `getFpgaCapabilities() & CAPAB_ULTIMATE64` (904). **If 0, nothing below runs.**
13. `C64::getMachine()` (C64 ctor, C64 doc) → `ConfigureU64SystemBus()` **R 0x10100403** (c64.cc:1514);
    maybe `hard_stop` (u64_config.cc:912-915).
14. `isEliteBoard()` **R 0x1010000C** (u64_config.cc:932 → product.cc:69); rev 0x14 → R 0x1018101E.
    (Also at SID socket store ctor, u64_config.cc:531.)
15. `i2c->enable_scan(true,false)` → **W 0x10100708 = 1**, `scanning=true` (u64_config.cc:938, hw_i2c_drv.cc:59-62).
    From now on every `i2c_lock` writes 0x10100708=0 and `vTaskDelay(2)`, every `i2c_unlock` writes 1 (i2c_drv.h:46-63).
16. `sockets.detect()` (655-751): W 0x10040000=0x08; **poll R 0x10040003 while bit4** (662-663); SID address
    setup writes C64 IO + `wait_ms(1)` (2303-2319); FPGASID probe (`hard_stop` poll 0x10040001 bit1, W/R
    0x1005D419/41A/400/401); remakes probes (`wait_10us`, `wait_ms(10)`, R 0x1005D41B/41C/41E, SIDKick
    string); if a socket is still empty: `S_SidDetector` → up to 3× `DetectSidImpl` inside
    `portENTER_CRITICAL`: **poll R 0x1005D012 until 0xFF** (2239-2240, 2346-2351).
17. `clear_ram` (C64 doc); menu setup.
18. Boot hotkey: W 0x1005DC02=0xFF, W 0x1005DC03=0x00, `scan_keyboard(&DC01,&DC00)`: W 0x1005DC00=0, R 0x1005DC01;
    if ≠0xFF → 8 column scans with stable-read loop (keyboard_c64.cc:118-155). Key 0x10 → System Mode PAL,
    0x0E → NTSC (u64_config.cc:949-964).
19. `effectuate_registered_settings()` → `U64Config::effectuate_settings` (1054-1162). With defaults:
    - W 0x10180013 PADDLE_EN, W 0x1018001A PADDLE_SWAP, **W 0x10100406 = swap&1**, **W32 0x1010030B = 0**.
    - W 0x10100405 = 3 (default enabled, 365), W 0x1010040D = 0xD8, R 0x10100405 (printf).
    - `setCpuSpeed`: W 0x10180002, 0x1018002D, 0x1018002E=1 (1603-1639).
    - **W 0x1010000C = 0xFF** (speaker; `get_value` of absent item returns -1, config.cc:514-520; items
      excluded at u64_config.cc:378-381; write at 1082-1084).
    - W 0x10180000 (scanlines, default 1); W 0x10100408 = `hdmiMonitor?1:0` (Auto; `hdmiMonitor` not yet
      initialised, u64_config.h:104 — Q11).
    - W 0x10100409 = 0x70 (defaults: all connected, no burst, no parallel cable).
    - `systemMode` e_NOT_SET(6) ≠ cfg (default **1 = NTSC**, u64_config.cc:348 with `color_sel` 275) → doPll.
    - Palette: W 0x10180800.. RGB, 0x10145000.. HDMI RGB, YUV (2720-2743). W 0x10180005 = `phase_inc` (0x87 NTSC).
    - hdmiMode default 0 = e_480p_576p (350, ctor 895).
    - `SetVideoPll(mode, ppm)` (u64ii_init.cc:141-160): R 0x1010000C; `calc_pll(24.0, f)` with
      f = (m + frac/2³²)·50/30 (+ppm) MHz (NTSC ≈ 32.727 MHz, PAL ≈ 31.528 MHz); lock; ch = rev==0x15 ? 0 : 1;
      `write_block(0xC8, 0x14, 8 bytes)`; unlock.
    - W 0x10180001 = `mode_bits|format` (0x2B for NTSC default).
    - `SetVideoMode1080p(NTSC, 480p)` (hdmi_scan.cc:156-179): AVI 4:3 (W 0x10144015=1, 0x1014401A=9,
      0x10144019=1, 0x10144007=0); `SetInternalPLL(e_INTPLL_25_13)`: MMCM[0x14]=0x130D, [0x15]=0x0080,
      [0x13]=0, [0x16]=0x1041, [0x4F]=0x1800, [0x4E]=0x0800, [0x28]=0xFFFF, [0x18]=0x0090, [0x19]=0x7C01,
      [0x1A]=0x7DE9, [0x08]=0x1187, [0x09]=0x0080, W 0x102000FF=0xB3,0x3B (u64ii_init.cc:213-228, 266-267);
      `SetExternalPLL(ext_pll_640x480)`: lock, ch = rev==0x15 ? 1 : 0, `write_byte(0xC8,0x81,0x18)`,
      `write_block(0xC8,0x14,{57 00 01 01 20 D7 BB F1},8)`, `write_byte(0xC8,0x81,0x08)`, unlock
      (u64ii_init.cc:162-171, hdmi_scan.cc:81); W 0x10144012 gearbox=1; `SetScanModeRegisters(sm_480p60)`
      → 0x10144000-05 = 00 30 0C 50 04 00, 0x10144007 = 00, 0x10144008-0E = 00 02 21 3C 0A 01 00
      (offset 06 `resync` is not touched here) (hdmi_scan.cc:6-28, 68);
      cropper W 0x10148000..03 = 08 00 BF 78; W 0x10144013=3, 0x10144014=0, 0x10144010=0, 0x10144006=2.
      PAL default path uses `ext_pll_720x576`, `sm_576p50`, crop (9,0,383,288), hscaler 4 (hdmi_scan.cc:159-168).
    - `DetermineOverlaySettings` (RAM only). `overlay` is still NULL (ultimate.cc:47, created at 125).
    - `ResetHdmiPll`: W 0x10100404=3, =0 (u64ii_init.cc:105-109).
    - `SetResampleFilter`: W 0x10100584=1, 0x10100589=1, 512 × W32 0x10100580 (NTSC), W 0x10100588=169, W 0x10100589=0.
    - `setLedSelector`: W 0x1010040E = 0x40.
20. Create HPD monitor task; `install_high_irq(5)` → ITU_IRQ_HIGH_EN |= 0x20; give semaphore
    (u64_config.cc:969-972). ~200 ms later the task runs `read_edid` (R 0x10100400 bit2; if set:
    W 0x10100400=0x20, lock, ch0 `read_block(0xA0,0x00,128)`, `(0xA0,0x80,128)` if byte126≠0, `read_block_ext`
    for ext ≥2, W 0x10100400=0x10, unlock) then `configure_hdmi_output`: W 0x10100408, W 0x10144006=2
    (1002-1010, 2577-2691).
21. `install_high_irq(6)` (UNLOCK) (974).
22. `sockets.effectuate`: lock, ch1 `write_byte(0x40,0x01,sid_ctrl)`, `write_byte(0x40,0x03,0x00)`, unlock;
    C64_SID1_EN/SID2_EN (753-819). Default with empty sockets: socket enable 0 → reg=0, shunt 0, caps cfg 0 →
    cap bit = 1 → value 0x08 per socket → **sid_ctrl = 0x88**.
23. `mixercfg` → W 0x10100500-13, then speaker mixer W 0x10100540-53 (1342-1365); `ultisids`,
    `sidaddressing` → C64 IO; `speakercfg` → W 0x10100540-53 again (981-983).
24. If config stale → flash write; start reset task (985-989).

### Phase D — later init functions / main loop

25. "U64 Palette" (9): palette rewrite (2882-2890).
26. "RMII Interface" (51), if `CAPAB_ETH_RMII`: `mdio_read(2, 0)`; if ≠0x0022 → `mdio_read(2, 3)`;
    `mdio_write` 0x04=0x01E1, 0x1B=0x0500, 0x16=0x0002, 0x00=0x1200 to that address (rmii_interface.cc:59-74);
    RMII task polls `mdio_read(1)` bit2 (link) (rmii_interface.cc:155, 175).
27. "LED Strip" (61): task `run()`: R 0x10100802 (menu heading), `effectuate_settings`: W 0x1018002F=0,
    `MapSingleColor` (W 0x101006FC=1, 252 map bytes 0,1,2,…, W 0x101006FC=0), W 0x101006FC=0x80 (default
    type APA102, led_strip.cc:60). Default mode 1 "Fixed": every 50 ticks W 0x1010040C=1, 0x101006FE=0x20,
    0x10100600-02 = RGB, 0x101006FD=0, 0x101006FF=0; R 0x10180080-82 each loop (led_strip.cc:309-400).
28. `ultimate_main` continues: `Overlay` on 0x10140000, `Keyboard_C64(row=0x1010040B, col=0x1010040A,
    joy=0x10100406)` (ultimate.cc:122-127). Main loop: on menu request, R 0x10100400 bit2 decides between
    overlay UI and C64 UI (ultimate.cc:181-188).

---

## Boot hazards

| # | Where | Read | Bad response → effect | Required response |
|---|---|---|---|---|
| H1 | hw_i2c_drv.cc:4-5 (used 15, 21, 28, 35, 46) | 0x10100701 bit7 | stuck 1 (e.g. 0xFF) → **infinite busy loop, no timeout**, first hit in Phase A step 3 | bit7 = 0 (immediately or after a few reads) |
| H2 | hw_i2c_drv.cc:36; i2c_drv.cc:124-354 | 0x10100701 bit2 on the final non-busy read | 1 → NACK: codec/PLL/expander writes only print; hub probe fails (`No USB hub found`); EDID reads fail | 0 (ACK) for 0x34, 0x40, 0x42, 0xC8 (ch0+ch1), 0x58 (ch2); 0xA0/0x60 only if HPD=1 and an EDID exists |
| H3 | nau8822.cc:18 → itu.c:63-71; also u64_config.cc:600-626, 2318 | 0x10000006 | never reaching 0 → hang in Phase A before scheduler | ITU timer counts down to 0 (ITU doc) |
| H4 | u64_config.cc:904 | CAPAB_ULTIMATE64 (0x1000000C bit2) | 0 → no HDMI/PLL/mixer/SID config at all; ultimate.cc:124 copies uninitialised overlay settings | 1 |
| H5 | ultimate.cc:100 | CAPAB_CARTRIDGE (0x1000000E bit1) | 0 → no C64 object, main loop never runs (ultimate.cc:166) | 1 |
| H6 | product.cc:67-81; users u64_config.cc:531-534, 932-934, 2388-2390 | 0x1010000C >>3 | not in {0x13, 0x15, 0x16, 0x17} → non-Elite: Joystick Swapper and SID shunt disabled, swap hotkey no-op | 0xB8 (rev 0x17 "U64E V2.2 (Mass Prod)") |
| H7 | u64ii_init.cc:157, 166 | 0x1010000C >>3 == 0x15 | 0x15 → video/HDMI PLL channels swapped | not 0x15 (unless the I2C model mirrors the swap) |
| H8 | itu.c:40-48 → w25q_flash.cc:81-83; filetype_u2p.cc:82-84 | caps bits 29:28 (0x1000000C bits 5:4) | mismatch with flash image → APPL/FLASHDRIVE at wrong offsets (50T: FLASHDRIVE 0x400000; 100T: 0x580000; CONFIG 0xFE8000 on both) | value that matches the flash image: 3 = 100T layout, 0..2 = 50T layout |
| H9 | c64.cc:1514-1524 | 0x10100403 bits1:0 | ≠3 → "external cartridge present"; default Automatic moves the whole bus to external (internal=0) | 0x03 |
| H10 | config.cc:48; blockdev_flash.cc:161 | 0x10100402 | ==1 → SAFE MODE (defaults, settings not loaded) and no flash disk | 0x00 |
| H11 | u64_config.cc:662-663 (`c64_reset_detect`, c64.h:332-334) | 0x10040003 bit4 | stuck 1 → **infinite loop** inside U64 Config init | bit4 = 0 |
| H12 | c64.cc:405-406 (from u64_config.cc:576, 2340, 914) | 0x10040001 bit1 after W 1 | never 1 → **infinite loop** | bit1 = 1 once a stop is requested (C64 doc) |
| H13 | u64_config.cc:2239-2240 (under `portENTER_CRITICAL`, 2349) | 0x1005D012 | never 0xFF → **hard freeze with IRQs masked**. Reached whenever a socket is not identified as FPGASID/remake, i.e. always with empty sockets | returns 0xFF at least once (constant 0xFF is fine) |
| H14 | u64_config.cc:582-587, 608-636; sid_device_pdsid.cc:113; sid_device_sidkick.cc:175-183 | 0x1005D400/401, D41B/D41C, D41E, D41D | matching IDs (0x1D/0xF5, 'S''W', 'N''O', 'S', SIDKick string) → phantom SID device objects | anything else; 0xFF or 0x00 both give "none" (S_SidDetector needs `buf[17]==2`, 2354-2363) |
| H15 | keyboard_c64.cc:135-138, 240-243 | 0x1010040B (and 0x1005DC01 at boot hotkey) | value changing between the two back-to-back reads → loop spins until stable | consecutive reads identical; idle = 0xFF |
| H16 | keyboard_c64.cc:208-226 | 0x10100406 bits4:0 | ≠0x1F → phantom joystick → endless cursor/RETURN keys in overlay menu | 0x1F (idle; 0xFF ok) |
| H17 | u64_config.cc:950-963 | 0x1005DC01 | a key giving code 0x10 or 0x0E → System Mode forced to PAL/NTSC | 0xFF |
| H18 | u64_config.cc:1082-1084 vs product.cc:44 | – | write of 0xFF to 0x1010000C (speaker) stored as RAM → later BOARDREV reads 0xFF → rev 0x1F → non-Elite (H6) | bits 7:3 of 0x1010000C must be independent of writes (on U2+ RTL only bit0 follows the written `speaker_en`, u2p_io.vhd:75-77,122-124) |
| H19 | ultimate.cc:183-186; u64_config.cc:2581-2584 | 0x10100400 bit2 | 0 → menu button / F10 always opens the **C64-side UI** (needs C64 core video); no EDID read | 1 for the HDMI overlay UI, plus config `CFG_USERIF_ITYPE`=1 ("Overlay on HDMI"; default 0 "Freeze", userinterface.cc:117, 123) |
| H20 | u64_config.cc:2594-2605, 2629-2676 | EDID bytes (if HPD=1) | invalid header/no CEA ext → `hdmiMonitor=false` → Auto writes 0x10100408=0 (DVI) | valid EDID + CEA ext with HDMI VSDB OUI 03 0C 00 for HDMI mode; NACK is harmless |
| H21 | i2c_drv.h:49, hw_i2c_drv.cc:55-57 | – | `i2c_lock` uses `xSemaphoreTake(…,5000)` and `vTaskDelay(2)` → with no RTOS tick all I2C after scan-enable blocks | working ITU timer IRQ from Phase B (ITU doc) |
| H22 | rmii_interface.cc:60-66, 155, 175 | 0x10100006 during MDIO reads | not a PHY → "could not find Ethernet PHY", continues at addr 3; link bit never 1 → no network link | PHY model: reg2 = 0x0022 at addr 0 (or 3); reg1 bit2 = 1 for link up |
| H23 | network/assembly.cc:36 | 0x10100407 bit0 | 0 → Assembly64 search returns -1 | 0x01 |

---

## Interrupts

Dispatcher (riscv_main.c:82-134): reads `ITU_IRQ_ACTIVE`, acks it via `ITU_IRQ_CLEAR`, then **always** reads
`ITU_IRQ_HIGH_ACT` (0x10000028) and calls `high_irqs[i].handler` for each set bit i<7. A set bit with
no handler clears its enable bit in `ITU_IRQ_HIGH_EN` (0x10000027) (riscv_main.c:118-129).
`install_high_irq(n)` sets `ITU_IRQ_HIGH_EN` bit n (riscv_main.c:38-45). How a high IRQ reaches the CPU
interrupt line is in the ITU doc.

| High IRQ | Name (itu.h) | Installed by | Handler / ack | Notes |
|---|---|---|---|---|
| 4 | `ITU_IRQHIGH_BLING` | nobody | – | if raised, dispatcher disables bit 4 |
| 5 | `ITU_IRQHIGH_HDMI` | u64_config.cc:971 | `hpd_monitor_irq`: **W 0x10100400 = 0x08** (HPD_RESET = ack), give semaphore, return 1 (u64_config.cc:994-1000). Task waits 200 ms, reads EDID, reprograms 0x10100408 and resync (1002-1010) | raise condition (edge/level, polarity) is closed-source (Q3). Emulator: latch on HPD change, keep HIGH_ACT bit5 set until 0x08 is written |
| 6 | `ITU_IRQHIGH_UNLOCK` | u64_config.cc:974 | `unlock_irq`: POKE $D038=0, C64_BUS_INTERNAL |= 2, CMD_IF slot enable/base 0x47 (1012-1020) | C64/UCI doc |

The HW I2C master, LED strip, mixers, PLLs and Blingboard have no interrupt use in this build.

---

## Functional model

### HW I2C master (T1)
- Per-channel bus (0..3) with attached device models. State: `started`, `busy`, `error`, `missed_write`,
  `data_out`, `channel`, `scan_en`.
- W 0x700: if `!started` → START, `started=1`; send byte to the addressed device (1st byte after START or
  repeated start = address byte; bit0 = R/W). ACK/NACK → `error`. Then `data_out = 0xFF`.
- W 0x702: repeated start (address phase next). W 0x703: STOP, `started=0`, device transaction ends.
- W 0x704/0x705: device supplies next byte → `data_out`; master NACK/ACK.
- R 0x701: `{busy<<7 | temp_err<<4 | missed<<3 | error<<2 | started}`; clear `error` and `missed` after
  the read (vhd:307-308). Completing instantly (busy never set) is allowed: the driver only loops while bit7=1.
- W 0x706 only when idle; W 0x707 bit0 → reset (channel 0, scan off, started 0). W 0x708 bit0 → scan enable.
- Transaction formats the firmware produces are listed in the Phase B preamble. SMBus block writes put a
  **length byte** after the register byte (i2c_drv.cc:306). Device models must expect it (USB2513 and
  CDCE-style PLL both use this format).

### NAU8822 (0x34)
9-bit registers: byte1 = `reg<<1 | d8`, byte2 = `d7..0` (i2c_drv.cc:276-282). Reg 0 write = reset. Final
state after boot: R1=0x0CF, R2=0x03F, R3=0x18F, R4=0x050 (left-justified, 24-bit), R6=0 (MCLK direct,
256 fs), R7=0 (48 kHz), R10=0x008 (DAC 128× OSR), R14=0x108 (ADC 128× OSR, HPF), R47/R48=0x005 (AUX-in
→ ADC 0 dB). Write-only (no reads in build). Emulator: store only.

### USB hub (0x58 on ch2)
Accept `STCD(0xFF)=0x02` (reset), a 24-byte config block at reg 0 (VID 0x0424, PID 0x2513, DID 0x0BA0,
CFG1 0x9A, CFG2 0x20, CFG3 0x02, MAXPS/B 1/0x32, HCMCS/B 1/0x32, PWRT 0x32), then `STCD=0x01` = attach.
T1: the attach can be the event that makes downstream USB ports visible in the USB model.

### I/O expanders
- 0x40 (ch1), 8-bit: output reg1 = `sid_ctrl`, config reg3 = 0 (all outputs). `sid_ctrl` nibble per
  socket (low nibble socket 1, high nibble socket 2): bits1:0 regulator (3 = 6581 12 V, 2 = 8580/other 9 V,
  0 = off), bit2 shunt (1k), bit3 caps (1 = 470 pF, 0 = 22 nF, from `1-CFG_SIDx_CAPS` with
  `sid_caps[]={"470 pF","22 nF"}`) (u64_config.cc:755-811, 282). Informational for the emulator.
- 0x42 (ch1), 16-bit: port 0 all outputs (columns), port 1 all inputs (rows), out0 = 0x00 (all columns
  selected) (u64ii_init.cc:132-136). The CPU then enables the FPGA scanner (0x708=1). The CPU reads the matrix
  through 0x1010040A/0x1010040B instead. Assumed: the FPGA scanner drives out0 and reads in1 and
  presents the result at `U64II_KEYB_ROW` (Q2).

### Keyboard / joystick registers (T1)
- `U64II_KEYB_COL` (W 0x1010040A): active-low column select, same role as CIA1 $DC00. 0 = all selected;
  scan uses 0xFE,0xFD,…,0x7F (keyboard_c64.cc:235-254).
- `U64II_KEYB_ROW` (R 0x1010040B): active-low rows, same role as $DC01. Matrix index = 8·(selected column
  bit) + row bit, identical to the C64 keyboard matrix (keymap_normal row 1 = 3 W A 4 Z S E LSHIFT,
  keyboard_c64.cc:27-36). Must be stable across consecutive reads.
- `U64II_KEYB_JOY` (R 0x10100406): bit0 up, bit1 down, bit2 left, bit3 right, bit4 fire, active low;
  scanned with columns deselected (0xFF) (keyboard_c64.cc:204-219). W = joystick swap select.

### External PLL programming (0xC8)
8 bytes to regs 0x14..0x1B: `57 00 P P N[11:4] {N[3:0],R[8:5]} {R[4:0],Q[5:3]} {Q[2:0],np[2:0],range[1:0]}`
(u64ii_init.cc:89-95). Decode: `M = ((N << np) − R) / Q`, `f_out = f_in · N / M / P`
(check: ext_pll_640x480 → N=525, M=263, P=1, matches comment hdmi_scan.cc:81). Video PLL f_in = 24 MHz
(u64ii_init.cc:155). HDMI PLL f_in comes from the FPGA (LVCMOS in, reg 0x01 via command 0x81: 0x18 = power
down, 0x08 = power up, u64ii_init.cc:167-169). T1 emulator: derive the C64 master clock / HDMI pixel
clock from these writes, or just from `CFG_SYSTEM_MODE`/`CFG_HDMI_RESOLUTION`.

`color_timings[]` (color_timings.cc:20-27), indexed by `t_video_mode` (u64.h:205-213):
0 PAL-50 (m=18, audio_div 77, phase_inc 0x37, mode_bits 0x00), 1 NTSC-60 (19, 80, 0x87, 0x2B),
2 PAL-60 (19, 80, 0x22, 0x2A), 3 NTSC-50 (18, 77, 0x98, 0x01), 4 PAL-60/L (19, 80, 0xB3, 0x2A),
5 NTSC-50/L (18, 77, 0x09, 0x01). `audio_div == 77` ⇒ "PAL" 50 Hz family everywhere
(hdmi_scan.cc:117, u64_config.cc:2218).

### HDMI timing registers (T1: size the HDMI framebuffer)
Encoding (hdmi_scan.cc:6-28), decode with HREP = 0x10144005:
`hactive = HACTIVE<<3 | (HREP>>1 & 6)`, `hsync = HSYNCTIME<<1 | (HREP>>1 & 1)`,
`hbp = HBACKPORCH<<2 | (HREP>>4 & 3)`, `hfp = HFRONTPORCH<<2 | (HREP>>6 & 3)`,
`vactive = VACTIVE<<3`, `vsync/vbp/vfp` direct, pixel repetition = HREP bit0 / REPCN+1.
Modes written (hdmi_scan.cc:65-79, 156-304):

| `CFG_HDMI_RESOLUTION` | PAL family | NTSC family | hscaler / vscaler / x_offset / gearbox |
|---|---|---|---|
| 0 SD | 720×576p50 VIC17, crop (9,0,383,288) | 640×480p60 VIC1, crop (8,0,383,240) | 04/00/0/1 · 03/00/0/1 |
| 1 HD | 1280×720p50 VIC19, crop (8,9,384,270) | 720p60 VIC4, crop (8,0,384,240) | 09/13/160/2 · 09/15/160/2 |
| 2 FullHD | 1080p50 VIC31 | 1080p60 VIC16 | 0C/08/240/0 · 0C/19/240/0 |
| 3 800×600 | @50 | @60 | 08/10 · 08/12, gearbox 2 |
| 4 1024×768 | @50 | @60 | 0A/14 gb2 · 0A/16 gb0 |
| 5 1280×1024 | @50 | @60 | 0B/17 · 0B/08, gb0 |

Overlay placement per mode is in `DetermineOverlaySettings` (u64_config.cc:2974-3007).
`resync = 2` is written after every mode change and HPD event (u64_config.cc:2689).

### HPD / EDID
- R 0x10100400 bit2 = monitor connected. W 0x20/0x10 gate the DDC buffer around EDID reads. W 0x08 acks the
  HPD event.
- EDID EEPROM model at 0xA0 on ch0: sequential read from pointer set by the register byte; extension blocks
  ≥2 use segment pointer 0x60 (i2c_drv.cc:356-399; u64_config.cc:2616-2617).

### LED strip (T1)
Protocol from led_strip.cc:
1. Map setup: W `LED_MAP`=1, write 252 map bytes, W `LED_MAP`=0, then W `LED_MAP`=0x40/0x80 for protocol
   (157-175, 447-451).
2. Frame: W `LED_INTENSITY`, write colour bytes at `offset`, W `LED_STARTADDR`=offset, W `LED_START` (any).
3. Derived from the code comments (`the led controller reads the address where to find the colors`,
   led_strip.cc:186-187, 240-241): channel c of LED k = `data[(map[3k+c] + FROM) mod 252]` scaled by
   intensity (Q12). LED order: case bottom 0-29, case top 30-59, keyboard strip 60-83 (led_strip.cc:179-182).
- `U64_LEDSTRIP_EN` (0x1010040C) must be 1 for output. `C64_VIC_SPLIT`=1 lets the C64 write $D200-$D2FF
  into the LED data in "Programmatic" mode (led_strip.cc:413-423).
- Power LEDs: `U64_CASELED_SELECT` nibbles select from `ledselects[]` (u64_config.cc:286-288).

### Power, user port, cartridge, restore
- Power off / power cycle on U64-II go to the ESP32 (`CMD_MACHINE_OFF`, `CMD_MACHINE_REBOOT`,
  wifi_cmd.cc:259-273, called from c64_subsys.cc:245-255). "Power On After Power Loss" and "Wake On Wi-Fi" live
  in ESP32 NVS. They are only pushed on change or when the module reports in (u64_config.cc:1462-1600),
  so there is no ESP32 dependency during U64Config boot. Rail voltages also come from the ESP32
  (`voltages_t`, wifi_cmd.h:21-30).
- `U64_USERPORT_EN` 3/0 and `U64_PWM_DUTY` 0xD8/0 follow `CFG_USERPORT_EN` (default enabled).
- `U64_CART_DETECT`: GAME/EXROM sense of the external port. Idle = 0x03.
- `U64_RESTORE_REG`: 1 = RESTORE held at power-on → safe mode.

### MDIO (bit-bang, mdio.c)
- Frame per `mdio_bit`: W SET_MDIO=v, W MDC=1, W MDC=0 (mdio.c:28-37).
- Read: 33×'1', ST `01`, OP `10`, PHYAD `0 0 0 a a` (bits 1:0 = 1 if addr≠0, so only addr 0 or 3 work),
  REGAD 5 bits MSB first, **one** TA bit '1', then 16× (`mdio_bit(1)`, sample R 0x10100006) (mdio.c:81-114).
- Emulator: count MDC rising edges from ST. D15 must be on GET_MDIO after the 49th rising edge of the frame
  (33 preamble + 2 + 2 + 5 + 5 + 1 TA = 48, +1 for PHY TA), D0 after the 64th.
- Write: same header with OP `01`, TA `10`, 16 data bits, trailing '1' (mdio.c:49-79).
- PHY registers used: 2 (ID1, expect 0x0022), 1 (status bit2 link), 4, 0x1B, 0x16, 0 (writes).

### Product / board detection (what is read)
- **Product identity is compile-time.** `getProductId()` returns `PRODUCT_ULTIMATE_64_ELITE_II` (0x06) for
  `U64 == 2` without any register read (product.cc:84-98, product.h:10). Strings: product "Ultimate 64-II",
  hostname "Ultimate-64-II-xxyyzz", family "U64MK2", update extension "UE2" (product.cc:18, 28, 38, 200-201).
- **"C64 Ultimate" vs "Ultimate 64 Elite II": no runtime distinction in this source.** The only
  "C64U" text is the unconditional store alt name "C64U Specific Settings" (u64_config.cc:943) (Q7).
- **Board revision**: `U2PIO_BOARDREV >> 3` (0x1010000C). 0x15 "U64E V2.0 (Early Proto)" (PLL channel swap),
  0x16 "V2.1 (Null Series)", 0x17 "V2.2 (Mass Prod)" (product.cc:57-62). `isEliteBoard` true for 0x13,
  0x15-0x17; 0x14 checks 0x1018101E bit7 (product.cc:67-81). `getBoardRevision()` has no caller in the build.
- **50T vs 100T**: `getFpgaType()` = capabilities bits 29:28 (itu.c:40-48, itu.h:77, 81). `>= 3` selects the
  100T flash map (BOOTFPGA 0-0x3C0000, APPL 0x3C0000, FLASHDRIVE 0x580000, CONFIG 0xFE8000) vs the 50T map
  (APPL 0x220000, FLASHDRIVE 0x400000, CONFIG 0xFE8000) (w25q_flash.cc:51-63, 81-83). It also restricts
  updates to `.CFW` files (filetype_u2p.cc:82-84). The bootloader loads the app from 0x3C0000 if type==3,
  else 0x220000 (bootloader_u64ii.c:173-176).
- **System info** (system_info.cc:159-206, on user request only): R `ITU_FPGA_VERSION` shown as `1%02X`;
  if CAPAB_ULTIMATE64, R `C64_CORE_VERSION` shown as `V1.%02X`; flash type string. No control flow.

---

## Emulator model tiers

### T0 — boot to main loop without hang
| Register(s) | Model |
|---|---|
| 0x10100700-0x10100708 | writes ignored; R 0x701 = 0x00 (idle, ACK); R 0x700/0x704/0x705 = 0xFF |
| 0x1010000C | R = 0xB8 constant (rev 0x17); writes ignored for reads (H6, H18). RTL bit0 = speaker_en is unused by the firmware (`>>3`) |
| 0x1000000C-F | CAPAB_ULTIMATE64 = 1, CAPAB_CARTRIDGE = 1, FPGA type consistent with the flash image (ITU doc) |
| 0x10100400 | R 0x00 (no HPD) — boots; T1 needs 0x04 for overlay UI |
| 0x10100402 | R 0x00 |
| 0x10100403 | R 0x03 |
| 0x10100406 | R 0x1F |
| 0x10100407 | R 0x01 |
| 0x1010040B | R 0xFF |
| 0x10100802 | R 0x00 |
| 0x10040001 bit1 / 0x10040003 bit4 | 1 / 0 (C64 doc) |
| 0x1005D012 (and SID/CIA DMA reads) | 0xFF |
| 0x10100006 | any constant |
| sinks | 0x10100000-0F other bits, 0x1010030B (32-bit), 0x10100404-0F other W, 0x10100500-0x10100589, 0x10100600-0x101006FF, 0x10144000-1D, 0x10145000-3F, 0x10148000-03, 0x10200000-0x102000FF, 0x10080000-0x10080800 |
| ITU timer + tick IRQ | working (H3, H21) |

### T1 — functional board
- HW I2C transaction engine (above) with device models: NAU8822 store, USB2513 (attach event), expander
  0x40 (SID socket power state), expander 0x42 + FPGA scan (host keyboard → `U64II_KEYB_ROW`), two PLLs
  (decode frequencies), EDID EEPROM on ch0 incl. segment 0x60.
- HPD: bit2 follows virtual monitor; latch + high IRQ 5, ack on W 0x08. Needed for the HDMI overlay UI (H19).
- HDMI regs / cropper / scaler / palette decode → framebuffer geometry and colours; `U64_HDMI_ENABLE`
  HDMI/DVI flag; `resync` strobe.
- Keyboard/joystick matrix at 0x1010040A/0B/06 from host input, CIA-style active-low.
- LED strip renderer (map + FROM + intensity, protocol byte) and power LED selectors.
- Mixer/speaker gains and resampler coefficient load (audio doc).
- MDIO PHY model at addr 0: reg2=0x0022, reg1 bit2 = link.
- User port power, cart detect (external cart insert → bits cleared), RESTORE-held safe-mode switch.
- Blingboard: RX_FLAGS bit2 = installed (cosmetic).

---

## Open questions

- Q1: Real address decode width of the closed-source U64-II blocks (U64_IO_BASE page, HW I2C mirrors at
  +0x09..+0xFF, Blingboard pages, MMCM page). The firmware only uses the offsets listed.
- Q2: FPGA keyboard scanner behind `scan_enable` (0x10100708): which channel (1V8 inferred from
  u64ii_init.cc:127-136), polling rate, how results reach `U64II_KEYB_ROW`/`JOY`, how CPU writes to
  `U64II_KEYB_COL` are used, and what happens to a scan in progress when the CPU clears scan_enable (driver
  just waits 2 ticks, hw_i2c_drv.cc:55-57).
- Q3: HPD high IRQ 5 raise condition (edge/level, polarity) and the exact meaning of read bit3 `HPD_WASLOW`.
- Q4: Blingboard RX_DATA/RX_GET/RX_IRQEN semantics and `U64II_BLINGBOARD_LEDS` (0x10100900). Unused in this
  build; `ITU_IRQHIGH_BLING` (4) has no handler.
- Q5: Exact part numbers of the PLLs (CDCE913-family inferred) and expanders (PCA9554/PCA9535-style inferred);
  needed only if the emulator wants datasheet-exact read-back.
- Q6: `U64II_BLACKBOARD` (0x10100407): meaning of the name and of bits other than bit0.
- Q7: How a real C64 Ultimate differs from an Ultimate 64 Elite II to this firmware (BOARDREV value?
  capability bits? FPGA type?). Not derivable from source. A dump of 0x1010000C and 0x1000000B-0x1000000F
  from both products is needed.
- Q8: BOARDREV values shipped on C64 Ultimate / later U64-II boards. Anything outside 0x13/0x15-0x17 makes
  `isEliteBoard()` false and disables the joystick swapper.
- Q9: Actual capability bits 29:28 on 50T hardware (0, 1 or 2; code only distinguishes `>= 3`).
- Q10: Sort stability of `IndexedList::sort` for equal orderings ("U64 Config" and "RAM Disk" both 1;
  "Boot Cart"/"SID Cart" 0). This decides which init function first creates ConfigManager (first
  0x10100402 read).
- Q11: `hdmiMonitor` is read in `effectuate_settings` (u64_config.cc:1092) before any `configure_hdmi_output`.
  It is not initialised in the ctor, so the first write to 0x10100408 depends on heap contents.
- Q12: LED controller mapping arithmetic (modulo 252 assumed), intensity scaling, meaning of values 0x40/0x80
  vs 1/0 in `LED_MAP`, and whether `CFG_LED_LENGTH` needs to reach the FPGA (it does not in this code).
- Q13: `U64_INT_CONNECTORS` bit meanings beyond the comment "4: ext, 5: ult, 6: cia" (u64_config.cc:1098),
  and `U64_USERPORT_EN` value 3 (two separate enables?).
- Q14: MMCM DRP addressing: does the FPGA decode byte address 2·idx (as the C pointer arithmetic produces)
  as DRP register idx? Consistent with the 16-bit DRP word width but unverified.
- Q15: Default `CFG_SYSTEM_MODE` is index 1 = NTSC (u64_config.cc:348). Confirm against a real
  unconfigured unit if the emulator ships an empty config page.
