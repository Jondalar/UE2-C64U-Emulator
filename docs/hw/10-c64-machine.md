# C64 machine interface: control, DMA, cartridge, SID/audio, U64 config

Scope: U64-II RISC-V `ultimate` build (`-DRISCV -DU64=2 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000`,
target/u64ii/riscv/ultimate/Makefile:246). Paths are relative to `firmware/1541ultimate/`.
The U64-II top level (C64 core, SID mapper, mixer, stream generators) is closed. The cartridge-slot register
block and the DMA/stop state machine exist as open U2 IP (`fpga/cart_slot`) and are used here to confirm the
C usage. Where only the C code defines the semantics, the doc says so. Unconfirmed points are marked OPEN QUESTION.

## Sources read

| File | Key content |
|---|---|
| software/system/u64.h, u2p.h, iomap.h, itu.h, itu.c | All base/offset macros, video format bits, ITU timer helpers `wait_ms`/`wait_10us`, capability bits |
| software/io/c64/c64.h / c64.cc | `C64` ctor, `init`, `stop`, `hard_stop`, `resume`, `freeze`/`backup_io`/`init_io`/`restore_io`/`unfreeze`, `reset`, `peek`/`poke`, `dma_transfer_frozen`, `init_system_roms`, `start_cartridge`, `set_cartridge`, `set_emulation_flags`, `init_cartridge`, `ConfigureU64SystemBus`, EEPROM helpers, `measure_timing` |
| software/u64/u64_machine.h / .cc | `U64Machine` peek/poke variants, `before/after_memory_access`, `clear_ram`, `get_cpu_port` |
| software/monitor/u64_memory_backend.cc | Monitor ROM cache (reads U64 ROM windows), `C64_VIDEOFORMAT` read |
| software/io/c64/c64_subsys.cc | Menu/REST commands, `dma_load` boot-cart handshake, `restoreCart`, `load_file_dma`, `dma_load_raw_buffer` |
| software/io/c64/c64_crt.cc / .h | CRT parser, ROM placement in DDR, mirroring, EasyFlash EAPI patch, EEPROM chunk |
| software/u64/u64_config.cc / .h | `U64Config` ctor (boot), SID detection, `effectuate_settings`, mixers, resampler, palette, SID addressing, turbo, reset task, high IRQs |
| software/u64/sid_device*.cc, sid_editor.cc | FPGASID/SwinSID/ARMSID/PDsid/SIDKick config-mode dialogs over DMA |
| software/io/c64/joystick_output.cc, io/usb/usb_hid.cc | Joystick/paddle/mouse override registers |
| software/io/c64/reu_preloader.cc | REU image load into DDR |
| software/io/network/data_streamer.cc | `U64_UDP_BASE` header templates, `U64_ETHSTREAM_ENA` |
| software/io/c64/keyboard_c64.cc | `scan_keyboard`/`scan`/`wait_free` (CIA1 matrix via DMA) |
| software/application/ultimate/ultimate.cc, portable/riscv/riscv_main.c | Boot order, main loop, IRQ dispatcher |
| software/system/product.cc, components/config.cc, filesystem/blockdev_flash.cc | `isEliteBoard`, core version, RESTORE safe mode |
| software/u64/led_strip.cc, api/route_machine.cc, network/socket_dma.cc, io/command_interface/{command_intf,control_target,softiec_target}.cc, filetypes/{filetype_sid,filetype_crt}.cc, monitor/monitor_file_io.cc, network/assembly.cc, userinterface/{userinterface,system_info}.cc | Other users of the registers |
| software/6502/bootcrt.tas | Boot cart side of the `$0002` handshake and `$DFFF` self-kill |
| target/u64ii/riscv/ultimate/linker.x:269-287 | DDR placement of cart RAM, REU, cart ROM |
| fpga/cart_slot/vhdl_source/{cart_slot_pkg,cart_slot_registers,slot_master_v4,slot_server_v4}.vhd, fpga/ip/busses/vhdl_source/io_to_dma_bridge.vhd, fpga/cpu_unit/rvlite/vhdl_source/bus_converter.vhd | Register semantics, stop conditions, DMA bridge, multi-byte I/O decomposition |

## Address map

All I/O is 8-bit. A 16/32-bit CPU load/store on the I/O space becomes 2/4 consecutive byte accesses at
addr+0..addr+3, little endian (bus_converter.vhd:56, 82-93, 159-185). The firmware relies on this. It does
32-bit reads/writes on the DMA window (c64.cc:195-197, 911-919, 1036-1044, 1794-1849), a 32-bit FIFO write on the resampler
(u64_config.cc:2226), and `load_file` straight into the ROM windows (c64.cc:1103-1104).

### Cartridge / machine control — `C64_CARTREGS_BASE` = IOBASE+0x40000 = 0x10040000 (c64.h:54-68, iomap.h:14)

| Addr | W | R/W | Name | Meaning (cart_slot_registers.vhd:44-138 unless noted) |
|---|---|---|---|---|
| 0x10040000 | 8 | RW | C64_MODE | W: bit2=1 assert C64 reset. Else bit3=1 release reset. Else bit1=ULTIMAX, bit4=NMI (level). R: bit1 ultimax, bit2 reset, bit4 nmi. FW values: 0x00 normal, 0x02 ultimax, 0x04 RESET, 0x08 UNRESET, 0x10 NMI (c64.h:70-73). Readback used: c64.cc:627, 656-657, 1002; monitor_file_io.cc:257-259 |
| 0x10040001 | 8 | RW | C64_STOP | W bit0: stop request. R: bit0 request, **bit1 HAS_STOPPED** (c64.h:84-85). `C64_STOP==0` test at c64.cc:1449 |
| 0x10040002 | 8 | RW | C64_STOP_MODE | bits1:0: 0 = stop in bad line (BA low ≥14 CPU cycles), 1 = after an R/W "write then read" pattern, 2 = force/immediate (c64.h:77-79; slot_master_v4.vhd:100-104) |
| 0x10040003 | 8 | R | C64_CLOCK_DETECT | bit0 PHI2 present, bit1 VCC, bit2 EXROM sense, bit3 GAME sense, bit4 RESET sense, bit5 NMI sense (c64.h:88-93) |
| 0x10040004 | 8 | R | — | only read by the `dump_hex` of 16 bytes (c64.cc:349) |
| 0x10040005 | 8 | RW | C64_CARTRIDGE_TYPE | bits4:0 type, bits7:5 variant (c64.h:115-163). Boot cart = 0x41 (`CART_TYPE_8K`) |
| 0x10040006 | 8 | W/R | C64_CARTRIDGE_KILL / _ACTIVE | W: bit0 kill, bit1 force-update strobe. FW writes 0x02 twice (c64.cc:1471-1474, c64_subsys.cc:189-190). R: bit0 cart_active (c64_subsys.cc:180-183) |
| 0x10040007 | 8 | RW | C64_KERNAL_ENABLE | U64 writes 0 only (c64.cc:1079, 1437, 1452); R bit0 (system_info.cc:104) |
| 0x10040008 | 8 | RW | C64_REU_ENABLE | bit0 (c64.cc:309, 317, 1296; read 1348) |
| 0x10040009 | 8 | RW | C64_REU_SIZE | bits2:0, 0=128K … 7=16M (c64.cc:315, 1299; c64.cc:60) |
| 0x1004000A | 8 | W | C64_SWAP_CART_BUTTONS | c64.cc:274. On U64 the config item does not exist → `get_value` returns -1 (config.cc:514-520) → writes 0xFF |
| 0x1004000B | 8 | W | C64_TIMING_ADDR_VALID | U64 writes 0xBB (c64.cc:336) |
| 0x1004000C | 8 | W | C64_PHI2_EDGE_RECOVER | U64 writes 0 (c64.cc:335); 0x08-0x0B = measurement trigger (c64.cc:1797-1849, developer) |
| 0x1004000D | 8 | RW | C64_SERVE_CONTROL | bit0 SERVE_WHILE_STOPPED: internal cart answers DMA cycles while stopped (slot_server_v4.vhd:526). Read-modify-write (u64_machine.cc:204-210, 225-239, 289-299, 326-329) |
| 0x1004000E | 8 | RW | C64_SAMPLER_ENABLE | bit0 (c64.cc:310, 323, 1303; read 1362) |

Adjacent blocks used by this subsystem:

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10044000 / 0x10044001 | 8 | W(R) | CMD_IF_SLOT_BASE / _ENABLE | UCI placement: base 0x47 → $DF1C, 0x7F → $DFFC, 0x07 → $DE1C (c64.cc:43-44, 330-331, 1305-1321). Enable is read at c64.cc:1355. See the UCI doc |
| 0x10046000..0x100467FF | 8 | R | CART_TIMING_BASE | Bus measurement samples, 2048 bytes per trigger (c64.cc:1793-1852). Developer / REST `MENU_MEASURE_TIMING_API` only (route_machine.cc:604) |
| 0x1004C000 | 8 | RW | EEPROM dirty | R≠0 = dirty; W 1 clears (c64.cc:1640-1660). Only if CAPAB_EEPROM (c64_crt.cc:214, 272) |
| 0x1004C800..0x1004CFFF | 8 | RW | EEPROM data | 2048 bytes GMOD2 serial EEPROM image (c64.cc:1644, 1650) |

### DMA window — `C64_MEMORY_BASE` = IOBASE+0x50000 = 0x10050000 (iomap.h:22)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10050000..0x1005FFFF | 8 (16/32 decomposed) | RW | C64 bus | Each byte access = one C64 bus cycle at C64 address (addr−0x10050000), done by DMA through the current PLA map (C64_MODE ultimax, cart lines, `$01`), or RAM-only when C64_DMA_MEMONLY=1. Reads stall until the cycle is served. Writes do not stall, so the FW adds a "flush" read after writes that must land before a mode change (c64.cc:660-666, 710-716, 754-776; u64_machine.cc:341-344). Named aliases: VIC `$D000+x` (c64.h:167), CIA1/2, SID volume, screen `$0400`, color `$D800` (c64.h:167-203) |

### U64 board I/O — `U64_IO_BASE` = U2P_IO_BASE+0x400 = 0x10100400 (u64.h:15, 65-80)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100400 | 8 | RW | U64_HDMI_REG | W: 0x20 DDC enable, 0x10 DDC disable, 0x08 HPD reset/ack. R: bit2 HPD current, bit3 HPD was low (u64.h:93-97). Video/HDMI doc owns it. Read at ultimate.cc:183 picks overlay vs C64-screen menu |
| 0x10100401 | 8 | W | U64_POWER_REG | U64-I only (c64_subsys.cc:255-259 is `#elif U64 == 1`). Not used on U64-II |
| 0x10100402 | 8 | R | U64_RESTORE_REG | ==1 → safe mode, config defaults (config.cc:47-50), flash disk not mounted (blockdev_flash.cc:160-163) |
| 0x10100403 | 8 | R | U64_CART_DETECT | bit0 GAME, bit1 EXROM of external port, active low. `(v&3)!=3` → external cart present (c64.cc:1514) |
| 0x10100404 | 8 | W | U64_HDMI_PLL_RESET | 3 then 0 (u64ii_init.cc:107-108) |
| 0x10100405 | 8 | RW | U64_USERPORT_EN | 3/0 (u64_config.cc:1071-1072); read for printf (1074) |
| 0x10100406 | 8 | RW | U64II_KEYB_JOY | W swap bit (u64_config.cc:1064, 2396). R = joystick port-2 lines for the overlay keyboard (ultimate.cc:126 → keyboard_c64.cc:208-209) |
| 0x10100407 | 8 | R | U64II_BLACKBOARD | bit0=0 → Assembly64 client refuses (assembly.cc:35-37) |
| 0x10100408 | 8 | W | U64_HDMI_ENABLE | 1 = HDMI, 0 = DVI (u64_config.cc:1091-1095, 2681-2685) |
| 0x10100409 | 8 | W | U64_INT_CONNECTORS | bit0 parallel cable, bits2:1 burst patch, bits6:4 IEC connections 0x70/0x60/0x30/0x50 (u64_config.cc:1098-1100) |
| 0x1010040A | 8 | W | U64II_KEYB_COL | overlay keyboard column drive (ultimate.cc:126) |
| 0x1010040B | 8 | R | U64II_KEYB_ROW | overlay keyboard row sense (ultimate.cc:126) |
| 0x1010040C | 8 | W | U64_LEDSTRIP_EN | 1 = CIA PWM pins drive LED strip (u64.h:77; led_strip.cc:337-380) |
| 0x1010040D | 8 | W | U64_PWM_DUTY | 0xD8 / 0x00 (u64_config.cc:1073) |
| 0x1010040E | 8 | W | U64_CASELED_SELECT | (sel1<<4)\|sel0 (u64_config.cc:1419) |
| 0x1010040F | 8 | RW | U64_ETHSTREAM_ENA | bits3:0 enable stream 0 VIC, 1 audio, 2 debug, 3 IEC; bits7:4 debug mode byte. Read-modify-write (data_streamer.cc:335, 410-414); 0 at boot (u64_config.cc:897) |

### Audio — U2P_IO_BASE+0x500.. (u64.h:16-18, 82-85)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10100500..0x10100513 | 8 | W (reads 0) | U64_AUDIO_MIXER | 10 channels × 2 bytes. Channel order 0 UltiSID1, 1 UltiSID2, 2 Socket1, 3 Socket2, 4 Sampler L, 5 Sampler R, 6 Drive1, 7 Drive2, 8 Tape read, 9 Tape write (u64_config.cc:436-446). Byte 2i = (pan_ctrl[10−pan]·vol)>>8, byte 2i+1 = (pan_ctrl[pan]·vol)>>8 (1349-1358), commented "right and left" (1315-1317). Write-only, reads 0 (1333-1334). Mute = bytes 0..7 ← 0 (1318-1331) |
| 0x10100540..0x10100553 | 8 | W | U64_SPEAKER_MIXER | same 10 channels, both bytes = volume_ctrl[vol] (u64_config.cc:1396-1400); all 20 ← 0 when disabled (1389-1394) |
| 0x10100580..0x10100583 | 32 | W | U64_RESAMPLE_DATA | FIR coefficient FIFO: 675 (PAL, `audio_div==77`) or 512 (NTSC) int32 (u64_config.cc:2215-2227) |
| 0x10100584 | 8 | W | U64_RESAMPLE_RESET | 1 (2223) |
| 0x10100588 | 8 | W | U64_RESAMPLE_LABOR | 45 (PAL) / 169 (NTSC) (2228) |
| 0x10100589 | 8 | W | U64_RESAMPLE_FLUSH | 1 before loading, 0 after (2224, 2229) |
| 0x10100600..0x101006FF | 8 | W | C64_IO_LED | LED strip RGB data 0x00-0xFB; +0xFC MAP enable, +0xFD start addr, +0xFE intensity, +0xFF START strobe (led_strip.h:25-28; led_strip.cc:150-154, 330-392) |
| 0x10100200 | 32 | R | U64_CLOCKMEAS / U64_CLOCK_FREQ | defined (u64.h:14, 87), no compiled user |
| 0x1010000C | 8 | W | U2PIO_SPEAKER_EN | written at u64_config.cc:1082-1087. CFG_SPEAKER_EN/VOL are not in the U64-II store (378-381) → `get_value`=-1 → writes 0xFF. Same address as U2PIO_BOARDREV (u2p.h:69, 73) |

### C64 core I/O — `C64_IO_BASE` = U2P_IO_BASE+0x80000 = 0x10180000 (u64.h:49, 104-154)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10180000 | 8 | W | C64_SCANLINES | cfg 0/1 (u64_config.cc:1088) |
| 0x10180001 | 8 | RW | C64_VIDEOFORMAT | bit0 NTSC encoding, bit1 60 Hz, bit2 RGB, bit3 NTSC clock ref, bits5:4 cycles/line 0=63,1=64,2=65, bit6 reset burst (u64.h:105, 163-170). Value = `color_timings[mode].mode_bits \| format` (u64_config.cc:1134, 1143; color_timings.cc:6-11). Read bit1 → monitor poll Hz (u64_memory_backend.cc:241) |
| 0x10180002 | 8 | W | C64_TURBOREGS_EN | 0x00 / 0x01 / 0x05, +0x02 SuperCPU detect (u64_config.cc:326, 1626-1634) |
| 0x10180003 | 8 | RW | C64_DMA_MEMONLY | 1 = DMA sees RAM only. Saved/restored (c64.cc:728, 828; u64_machine.cc:61-76, 218, 240) |
| 0x10180005 | 8 | W | C64_PHASE_INCR | `ct->phase_inc` (u64_config.cc:1121) |
| 0x10180007 | 8 | W | C64_VIC_TEST | DEVELOPER only (u64_config.cc:1156-1158) |
| 0x10180008 / 09 | 8 | W | C64_SID1_BASE / SID2_BASE | socket decode, value = C64 address>>4 (table u64_config.cc:218-235). 0x01 = unmapped |
| 0x1018000A / 0B | 8 | W | C64_EMUSID1/2_BASE | UltiSID decode, same encoding |
| 0x1018000C..0F | 8 | W | C64_SID1/SID2/EMUSID1/EMUSID2_MASK | compare mask for A11..A4; 0xFE = 32-byte window, 0xF0 = 256 bytes (u64_config.cc:1238-1241, 2309-2314); unmapped mask 0xFE (202-203) |
| 0x10180010 | 8 | R | C64_CORE_VERSION | displayed as "1.%02x" (product.cc:139, routes.cc:202, socket_dma.cc:598, system_info.cc:176) |
| 0x10180011 / 12 | 8 | W | C64_SID1_EN / SID2_EN | 0/1 (u64_config.cc:817-818, 2315-2316, 2331-2332) |
| 0x10180013 | 8 | W | C64_PADDLE_EN | (u64_config.cc:1060) |
| 0x10180014 | 8 | W | C64_STEREO_ADDRSEL | 0..5 → split bit A5..A9 (u64_config.cc:265, 319, 880, 2317) |
| 0x1018001A | 8 | W | C64_PADDLE_SWAP | (u64_config.cc:1061, 2398) |
| 0x10180020 / 21 | 8 | W | C64_EMUSID1/2_WAVES | 0 = 6581, 1 = 8580 combined waveforms (u64_config.cc:1651-1656) |
| 0x10180022 / 23 | 8 | W | C64_EMUSID1/2_RES | resonance 0/1 (1645-1650) |
| 0x10180027 / 28 | 8 | W | C64_EMUSID1/2_DIGI | digi level 0..3 (1657-1662) |
| 0x10180029 | 8 | W | C64_EMUSID_SPLIT | 0..7 → split_bits (u64_config.cc:266, 320, 881) |
| 0x1018002A | 8 | W | C64_BUS_BRIDGE | {0,1,2,3,5}[cfg] (c64.cc:68, 1511); bit0 = write mirroring (c64.cc:1598-1601) |
| 0x1018002B | 8 | RW | C64_BUS_INTERNAL | bit0 IO1, bit1 IO2, bit2 ROM, bit3 IRQ served by internal cart (c64.cc:1539-1592). Read-modify-write in ISR (u64_config.cc:1016) |
| 0x1018002C | 8 | W | C64_BUS_EXTERNAL | same bits for the external port (c64.cc:1593) |
| 0x1018002D | 8 | W | C64_SPEED_PREFER | speed idx (0..15) \| badlines<<7; 0x80 = off (u64_config.cc:1626-1635) |
| 0x1018002E | 8 | W | C64_SPEED_UPDATE | strobe 1 (1636) |
| 0x1018002F | 8 | W | C64_VIC_SPLIT | 1 = C64 writes to $D200-$D2FF drive the LED strip (led_strip.cc:413, 422) |
| 0x10180030 / 31 | 8 | W | C64_JOY1/2_SWOUT | injected joystick, active-low bits4:0 \| 0xE0 (joystick_output.cc:81-82) |
| 0x10180032..35 | 8 | W | C64_PADDLE_1_X/Y, 2_X/Y | 0x80 released / 0x00 pressed for fire2/fire3 (joystick_output.cc:26-37, 83-86), or 7-bit mouse (usb_hid.cc:1476-1477) |
| 0x10180036 / 37 | 8 | W | C64_MOUSE_EN_1/2 | (joystick_output.cc:90-91; usb_hid.cc:308) |
| 0x10180080.. | 8 | R | C64_VOICE_ADSR(x) | envelope level, x = sidsel·4 + voice(0..2) (u64.h:154; led_strip.cc:323-325) |

### Other C64-side windows (u64.h:50-61, 156-161)

| Addr | W | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10180800..0x1018083F | 8 | W | C64_PALETTE RGB | 16 × {R,G,B,pad} (u64_config.cc:2724-2731, 2747-2750) |
| 0x10180C00..0x10180C3F | 8 | W | C64_PALETTE YUV | 16 × {Y,U,V,pad}, U/V signed (u64_config.cc:2760-2763, 2795-2801, 2804-2847). HDMI copy at U64II_HDMI_PALETTE 0x10145000 → video doc |
| 0x10181000..0x1018101F | 8 | RW | C64_PLD_ACC | U64-I PLD. On U64-II only JOYCTRL 0x1018101E: R bit7 in `isEliteBoard` when board rev 0x14 (product.cc:73-78), W in `swap_joystick` (u64_config.cc:2397) |
| 0x10181800 | 8 | RW | U64_DEBUG_REGISTER | FPGA debug; REST/socket only (route_machine.cc:462-490, socket_dma.cc:299-302) |
| 0x10182000..0x101821FF | 8/16 | — | C64_GLYPH | defined, no compiled user |
| 0x10185000..0x101857FF | 8 | W | UltiSID1 filter curve | C64_SID_BASE+0x1000, 1024 × uint16 LE (u64_config.cc:1259-1298; sid_coeff.c:314-323) |
| 0x10185800..0x10185FFF | 8 | W | UltiSID2 filter curve | +0x1800 (u64_config.cc:1260-1262) |
| 0x10188000..0x10189FFF | 8 | W(R?) | U64_BASIC_BASE | BASIC ROM image (c64.cc:1103) |
| 0x1018A000..0x1018BFFF | 8 | W(R?) | U64_KERNAL_BASE | KERNAL ROM image. "write only" per c64.cc:1085, yet read by u64_machine.cc:107 and u64_memory_backend.cc:75-81 |
| 0x1018C000..0x1018CFFF | 8 | W(R?) | U64_CHARROM_BASE | char ROM (c64.cc:1104-1108; read u64_machine.cc:103) |
| 0x10190000..0x101900FF | 8 | W | U64_UDP_BASE | 4 × 64-byte slots, slot i at +64·i; bytes 0..41 = Ethernet/IPv4/UDP header template (data_streamer.cc:311-404). Source ports 0xD000/0xD400/0x196E/0x0605 (345-354) |

### DDR regions shared with the FPGA cartridge logic (linker.x:269-287)

| Addr | Size | Name | Meaning |
|---|---|---|---|
| 0x00EF0000 | 0x10000 | __cart_ram_start | cart function RAM, zeroed on every cart set (c64.cc:437-440, 1384) |
| 0x01000000 | 0x1000000 | REU_MEMORY_BASE | REU/GeoRAM contents (c64.h:14-15; reu_preloader.cc:104-108; filetype_reu.cc:117-127) |
| 0x03C00000 | 0x400000 | __cart_rom_start | CRT ROM, bank·16K (+0x2000 for $A000 chips); C128 32K banks (c64_crt.cc:251-253, 291-299). Custom carts copied to offset 0 (c64.cc:1285-1288). `cart_mem[5]=0` kills a previous CBM80 image (c64.cc:1251) |

## Init / boot sequence as seen from the bus

1. `custom_hardware_init`: U64_HDMI_REG ← 0x20 (u64ii_init.cc:126). The rest is I2C → other docs.
2. `ultimate_main` reads capabilities 0x1000000C-F (ultimate.cc:87, itu.c:18-28) and runs InitFunctions sorted by ordering (ultimate.cc:94; init_function.cc:29-38).
3. The ConfigManager is created at the first `register_store`. It reads U64_RESTORE_REG 0x10100402 (config.cc:47-50).
4. InitFunction "U64 Config" (ordering 1, u64_config.cc:93). The member ctors run first: `U64SidSockets` calls `isEliteBoard` → U2PIO_BOARDREV 0x1010000C, and C64_PLD_JOYCTRL 0x1018101E if rev 0x14 (u64_config.cc:531; product.cc:67-81). Then the body (u64_config.cc:892-992):
   1. U64_ETHSTREAM_ENA ← 0 (897). Continues only if CAPAB_ULTIMATE64 (904).
   2. `C64::getMachine()` → `U64Machine` → C64 ctor: C64_STOP_MODE ← 2, C64_MODE ← 0 (c64.cc:147-148), read CLOCK_DETECT bit0 (log only, c64.cc:166-168).
   3. `ConfigureU64SystemBus` (911): BUS_BRIDGE ←, read U64_CART_DETECT, BUS_INTERNAL/EXTERNAL ← (c64.cc:1509-1596).
   4. `hard_stop` (914): read C64_STOP; if bit1 clear: STOP_MODE ← 2, STOP ← 1, **poll bit1 forever**; `wait_10us(2)` (c64.cc:399-409).
   5. `sockets.detect()` (940 → 655-751):
      - C64_MODE ← 0x08 (661), then **poll CLOCK_DETECT bit4 until 0** (662-663).
      - `S_SetupDetectionAddresses`: SID1_BASE ← 0x40, SID2_BASE ← 0x50, masks 0xF0, EMUSID bases 0x60, masks 0xFE, SID1/2_EN ← 1, ADDRSEL ← 0, wait 1 ms (2303-2319).
      - `detectFPGASID(0/1)`: hard_stop; DMA W $D419/$D519 ← 0xEE, $D41A ← 0xAB; R $D400,$D401 (576-592).
      - wait 100 ms; `detectRemakes(0/1)`: W $D41D='S', $D41E='I', $D41F='D' (10 µs gaps), wait 10 ms, R $D41B,$D41C (599-613).
      - PDsid probe: W $D41D='P', R $0002, W $D41E='D', R $0002, R $D41E (sid_device_pdsid.cc:107-114).
      - SIDKick probe: W $D41F ← 0xFF, 32× {W $D41E ← 224+i, R $D41D}, W $D41F ← 0xFD (sid_device_sidkick.cc:154-184).
      - If a socket is still NONE, `S_SidDetector` (686 → 2336-2384): hard_stop, then up to 3× `DetectSidImpl` inside `portENTER_CRITICAL`. Each run **polls $D012 until 0xFF** (2239-2240), then 16× {W $D412=0x48, $D40F=0x48, $D412=0x24, $D41F×3, R $D41B, $D41F×4, R $D41B; same at $D5xx} (2242-2274).
      - `detectDukestahAdapter` only if sid1 = FPGASID and sid2 = none (695-700).
   6. `clear_ram` (941 → u64_machine.cc:349-373): `stop(false)` (STOP_MODE ← 1, STOP ← 1, poll bit1 ≤10 ms via ITU_TIMER, else STOP_MODE ← 2 and poll forever, c64.cc:462-497). Then critical section, DMA_MEMONLY ← 1, 64 KB `raminit.bin` → 0x10050000, DMA_MEMONLY ← 0, `resume()` (stop_mode 2: STOP_MODE ← 2, MODE ← 0, STOP ← 0, c64.cc:589-596). **From here the C64 is running.**
   7. Boot hotkey: DMA W $DC02 ← 0xFF, $DC03 ← 0x00 without stopping the machine (950-951). `scan_keyboard`: W $DC00 ← 0, R $DC01; if ≠0xFF, per column `do {R $DC01; W $DC00} while (R $DC01 differs)` (keyboard_c64.cc:118-155). Key 0x10 → System Mode PAL, 0x0E → NTSC (954-963).
   8. `effectuate_registered_settings` (967 → 1054-1162): PADDLE_EN, PADDLE_SWAP, U64II_KEYB_JOY, MATRIX_WASD_TO_JOY, USERPORT_EN, PWM_DUTY, speed regs (1603-1639), U2PIO_SPEAKER_EN ← 0xFF, SCANLINES, HDMI_ENABLE, INT_CONNECTORS, palette 0x10180800/0x10180C00/0x10145000 (2720-2743), PHASE_INCR. If mode changed: SetVideoPll (I2C), VIDEOFORMAT, SetVideoMode1080p, ResetHdmiPll (0x10100404 ← 3,0), SetResampleFilter (0x10100584 ← 1, 0x10100589 ← 1, 675/512 × DATA, 0x10100588 ←, 0x10100589 ← 0). CASELED_SELECT.
   9. HPD task; `install_high_irq(5)` and `(6)` → ITU_IRQ_HIGH_EN r-m-w (969-974; riscv_main.c:38-45).
   10. `sockets.effectuate` (753-819): I2C expander 0x40 ← sid_ctrl, SID1_EN/SID2_EN. `mixercfg` → 20 bytes 0x10100500 plus speaker mixer (1342-1365). `ultisids` → 2×2048 bytes filter RAM, RES/WAVES/DIGI (836-848, 1641-1665). `sidaddressing` → bases/masks/ADDRSEL/SPLIT (857-888). `speakercfg` (510-514).
   11. Reset task created (989).
5. InitFunction "U64 Palette" (ordering 9): palette written again (u64_config.cc:2880-2890). "LED Strip" (61): task loops over C64_VOICE_ADSR reads and C64_IO_LED writes (led_strip.cc:322-392). "Data Streamer" (70): config only. "REU Preloader" (98): may load the image into DDR 0x01000000 after the file appears (reu_preloader.cc:13-29, 81-120).
6. ultimate.cc:100-104 (needs CAPAB_CARTRIDGE): `c64->init()` (c64.cc:201-212):
   1. `effectuate_settings`: SWAP_CART_BUTTONS ← 0xFF, `ConfigureU64SystemBus`, `set_emulation_flags` (REU_ENABLE/SAMPLER_ENABLE ← 0, CMD_IF enable ← 0, REU_SIZE, REU_ENABLE, SAMPLER_ENABLE if CAPAB_SAMPLER, CMD_IF 0x10044001/0x10044000 ← en/0x47 if CAPAB_COMMAND_INTF, PHI2_EDGE_RECOVER ← 0, TIMING_ADDR_VALID ← 0xBB, reads 0x10040000..0F) (c64.cc:270-350).
   2. `init_system_roms`: KERNAL_ENABLE ← 0; 8192 bytes → 0x1018A000; BASIC file → 0x10188000; char file or default → 0x1018C000 (c64.cc:1077-1109).
   3. `init_cartridge` (c64.cc:1444-1478): R C64_STOP; if 0: MODE ← 0x04. KERNAL_ENABLE ← 0, CARTRIDGE_TYPE ← 0, `init_system_roms` again. If `ConfigureU64SystemBus` reports an external cart: wait 100 ms, MODE ← 0x08, STOP ← 0, return. Else `set_cartridge(NULL)` (CRT → DDR 0x03C00000, TYPE ←, flags, cart RAM clear; c64.cc:1242-1391), KILL ← 2, KILL ← 2, MODE ← 0x08, STOP ← 0.
   4. `c64->start()`: STOP ← 0 (c64.cc:219-222).
7. Main loop (ultimate.cc:166-207): ITU_BUTTON_REG via `checkButton` (c64.cc:1493-1506). On a menu request, U64_HDMI_REG bit2 picks the HDMI overlay UI over the C64-screen UI (183-186).
8. C64-screen menu open (`take_ownership` → `freeze`, c64.h:355-358; c64.cc:985-1008):
   1. `frozen_mode` ← R C64_MODE.
   2. `stop(true)`: STOP_MODE ← 0, STOP ← 1, poll bit1 ≤25 ms. On success MODE ← 0x02 and R $D012/$D011/$D01A/$D019. If $D01A bit0: `determine_d012` polls $D019&0x81 ≤40 ms.
   3. `backup_io`: R 49 VIC regs, W $D011/$D020/$D021 ← 0, mixer bytes 0..7 ← 0, R CIA1 regs 0..12 except 8 and 11, 32-bit R $0800-$0FFF/$D800-$DBFF/$0400-$07FF, W charset → $0800, CIA DDR/port juggling (c64.cc:880-942).
   4. `init_io` (944-983).
   - Unfreeze = `restore_io` + `resume` (1024-1075, 524-597).

## Boot hazards

| # | Where | Poll / probe | Required response |
|---|---|---|---|
| H1 | c64.cc:405 (`hard_stop`, reached from u64_config.cc:914, 542, 576, 2340 and c64.cc:1160) | `while(!(C64_STOP & 0x02));` no timeout | After W C64_STOP bit0=1, R bit1 = 1 (immediately is fine). R bit0 = last written. W 0 → bit1 = 0 |
| H2 | c64.cc:493 (`stop`, FORCE fallback; boot via u64_machine.cc:358) | `while(!(C64_STOP & 0x02));` | same as H1 |
| H3 | c64.cc:437-456, 473-483 | bit1 polled against ITU_TIMER budgets (25 ms / 10 ms) | bit1 set; ITU_TIMER at 0x10000006 must count down to 0 (ITU doc) or these loops never end |
| H4 | u64_config.cc:662 | `while (C64_CLOCK_DETECT & 0x10);` after C64_MODE ← 0x08 | bit4 = 0 once reset is released (not held) |
| H5 | u64_config.cc:2239 (`DetectSidImpl`, critical section, up to 3× per boot, u64_config.cc:2346-2352) | `while (C64_PEEK(0xD012) != 0xFF);` no timeout, interrupts off | DMA read of $D012 must reach 0xFF by reads alone. A raster counter advanced per read or per emulated time independent of IRQs, or constant 0xFF |
| H6 | c64.cc:1514, 1458-1464 | external cart detect | U64_CART_DETECT = 0x03. 0 → bus switched to external, internal cart/REU/UCI I/O not served, `init_cartridge` skips cart load |
| H7 | config.cc:48, blockdev_flash.cc:161 | RESTORE key at boot | U64_RESTORE_REG ≠ 1 (0x00). 1 → config defaults and no /flash (ROMs, CRTs, palettes) |
| H8 | c64.cc:354, 987, 1143; userinterface.cc:308, 324 | `phi2_present()` | CLOCK_DETECT bit0 = 1. 0 → C64-screen menu never opens (`exists()` false, freeze/unfreeze are no-ops) |
| H9 | ultimate.cc:100-107; u64_config.cc:904; c64.cc:205, 311, 319, 1306 | capabilities | CAPAB_CARTRIDGE 0x00000200 and CAPAB_ULTIMATE64 0x04000000 set. Without them: no `c64` object, no U64Config. CAPAB_COMMAND_INTF 0x00040000 gates UCI, CAPAB_SAMPLER 0x00200000 gates the sampler, CAPAB_EEPROM 0x00400000 gates GMOD2 |
| H10 | keyboard_c64.cc:135-138 (boot via u64_config.cc:952), 240-243 (menu scan) | `do{row=$DC01; W $DC00} while(row != $DC01);` no timeout | Consecutive DMA reads of $DC01 must be equal. Use 0xFF (no key). All-zero is harmless: shift_flag=7, mtrx=63 → key 0 (keyboard_c64.cc:16-25, 68). Values that decode to 0x10/0x0E change System Mode |
| H11 | keyboard_c64.cc:208-213 | port-2 joystick via $DC00 read after W $DC00 ← 0xFF | $DC00 reads back 0xFF when 0xFF was written, else a phantom joystick press becomes menu keys |
| H12 | u64_config.cc:582-590, 608-637; sid_device_pdsid.cc:113; sid_device_sidkick.cc:172-183 | SID signature probes | Reads of $D400-$D5FF must not produce $D400/$D401 = 1D F5, $D41B/$D41C = 'S','W' or 'N','O', $D41E='S' after writing 'D', or the SIDKick strings, unless that device is emulated. 0x00 or RAM-like readback yields "none" |
| H13 | u64_config.cc:2354-2377 | 6581/8580 recognition | With $D41B/$D51B reading 0 both sockets = none. For a detected socket, result2==2 and result1∈{0 = 8580, 1 = 6581}, stable over 16 iterations |
| H14 | c64.cc:620 (`C64::reset`, REST/menu reset) | `while(ioRead8(ITU_TIMER));` no timeout | ITU_TIMER counts down (ITU doc) |
| H15 | c64.cc:378-385, 548-566 | $D019&0x81 and $D012 compare, 40 ms timeouts | No hang. Give $D012 a moving value to avoid 40 ms stalls |
| H16 | readback-dependent registers: c64.cc:627, 656-657, 703, 749, 1002 (C64_MODE); c64.cc:1449 (C64_STOP); c64.cc:728/828, u64_machine.cc:61-76 (DMA_MEMONLY); u64_machine.cc:204-210 (SERVE_CONTROL); u64_config.cc:1016 (BUS_INTERNAL); data_streamer.cc:335, 410-414 (ETHSTREAM_ENA); u64_memory_backend.cc:241 (VIDEOFORMAT); c64.cc:1348, 1355, 1362 (REU/CMD_IF/SAMPLER enables) | read-modify-write / save-restore | Store and return the written value (C64_MODE per the VHDL bit rules in the table) |
| H17 | c64.cc:911-919, 1036-1044; c64.cc:1103-1104 | 32-bit loads/stores into DMA and ROM windows | Decompose into 4 byte accesses, LSB first (bus_converter.vhd:159-185) |
| H18 | c64_subsys.cc:180-191 (after any DMA load/run, not boot) | `C64_CARTRIDGE_ACTIVE` bit0 polled 500×vTaskDelay(2) | bit0 → 0 after the 6502 boot cart writes $40 to $DFFF (bootcrt.tas:86-89, 591, 609). Else ~5 s wait then `init_cartridge` |
| H19 | c64_subsys.cc:635-645, filetype_sid.cc:749-763 (runtime) | `$0002 == 0x01` handshake, 60×/30×25 ms timeout | Needs the boot/SID cart running on a 6502 (T1). T0 fails gracefully (timeout → `init_cartridge`) |
| H20 | product.cc:67-81 | `isEliteBoard` | BOARDREV>>3 ∈ {0x13, 0x15..0x17}, or 0x14 with C64_PLD_JOYCTRL bit7. Else joystick-swap setting and SID shunt options are disabled (u64_config.cc:531-534, 932-934, 2388-2390) |
| H21 | assembly.cc:36 | U64II_BLACKBOARD bit0 | 1 to enable the Assembly64 client; 0 returns -1 |

## Interrupts

- **ITU IRQ bit 7 (0x80, ITU_INTERRUPT_RESET)** = C64 reset event.
  - Enabled by `CommandInterface::run_reset_task` (command_intf.cc:100-102), which exists only with CAPAB_COMMAND_INTF & CAPAB_CARTRIDGE (command_intf.cc:44).
  - Dispatcher reads ITU_IRQ_ACTIVE 0x10000005 and writes the same bits to ITU_IRQ_CLEAR 0x10000004 (riscv_main.c:86-87), then calls `ResetInterruptHandlerCmdIf` (command_intf.cc:29-36) and `ResetInterruptHandlerU64` (u64_config.cc:2114-2117).
  - The latter wakes `run_reset_task`, which re-applies U64 settings, sockets, mixer, UltiSIDs and SID addressing unless `skipReset` was set by SID autoconfig (u64_config.cc:1034-1051, 2046).
  - c64.cc:1188 relies on this to restore BUS_INTERNAL/EXTERNAL after `start_cartridge`.
- **High IRQ 6 (ITU_IRQHIGH_UNLOCK)**: `unlock_irq` (u64_config.cc:1012-1020, installed 974) runs in the ISR.
  - DMA W $D038 ← 0 without stopping ("disable the IRQ once again").
  - BUS_INTERNAL |= 0x02, CMD_IF enable ← 1, base ← 0x47. Returns 1.
- **High IRQ 5 (ITU_IRQHIGH_HDMI)**: `hpd_monitor_irq` acks with U64_HDMI_REG ← 0x08 (u64_config.cc:994-1000) → video doc.
- A high IRQ with no handler is disabled by the dispatcher (riscv_main.c:118-129).
- No other interrupt comes from this block. Stop/DMA completion is polled (H1-H3).

## Functional model

### Stop / DMA state machine (slot_master_v4.vhd:100-152, io_to_dma_bridge.vhd:29-60)
- States: `RUNNING` → (STOP bit0=1 ∧ condition) → `STOPPED` (bit1=1) → (STOP bit0=0 ∧ condition) → `RUNNING`.
- Condition per STOP_MODE: 0 = CPU sees BA low for 14 cycles (in a bad line); 1 = R/W history "write then read"; 2 = immediately. The same condition is applied on release. The FW always sets 2 before a plain release (c64.cc:590) and 0 when it resumes from a bad-line stop (577).
- Only the 6510 is held. VIC, CIAs and SID keep running (the FW reads a live raster during stop, c64.cc:548-566; H5).
- A DMA byte = one bus cycle through the PLA using the current map. C64_MODE bit1 forces ULTIMAX with the freezer cart's decode: RAM $0000-$0FFF, I/O, cart ROM windows, and **no RAM at $1000-$7FFF/$A000-$CFFF**, so the FW temporarily restores `frozen_mode` (c64.cc:650-667, 737-780).
- C64_DMA_MEMONLY=1 bypasses I/O and ROM and hits RAM (used for bulk RAM copy and while frozen, u64_machine.cc:152-165, 191, 263; control_target.cc:533-535). SERVE_WHILE_STOPPED lets the internal cart answer.
- FW pattern for every live access (u64_machine.cc:152-177): if not frozen and not stopped → `stop(false)`; critical section; DMA_MEMONLY ← memOnly; access; DMA_MEMONLY ← 0; `resume()` if it stopped.
- DMA reads of $0000/$0001: see OPEN QUESTION 2. The FW writes both and then does 2 dummy reads of $0001 to apply the port (u64_machine.cc:37-56).

### Reset
- `C64::reset` = MODE ← 0 (drops ultimax/NMI), MODE ← 0x04, ITU_TIMER 20 ticks, MODE ← 0x08 (c64.cc:615-623). `is_in_reset` = MODE bit2 (625-628).
- CLOCK_DETECT bit4 = reset line sensed (H4).
- NMI pulse: MODE ← 0x10, wait ~1 ms, MODE ← 0 (monitor_file_io.cc:283-286).

### Freeze (menu on C64 screen)
See boot step 8. `restore_io` writes CIA2 port A only if the menu changed it (c64.cc:1047-1049) and pulses SID $D41F/$D43F/$D51F ← 0 (1070-1072). While frozen, peek/poke of $0400-$07FF, $0800-$0FFF and $D800-$DBFF go to the backup buffers (c64.cc:643-648, 693-698; u64_machine.cc:8-35).

### Cartridge start (`start_cartridge`, c64.cc:1154-1224)
1. `hard_stop`; MODE ← 0x02; $D020 ← 0; $D011 ← 0.
2. MODE ← 0; W $8005 ← 0 (kills CBM80); MODE ← 0x04; TYPE ← 0; wait 50 ms.
3. For custom carts: BUS_INTERNAL ← 15, BUS_EXTERNAL ← 0.
4. STOP_MODE ← 2; STOP ← 0 (still in reset).
5. TYPE/REU/SAMPLER/CMD_IF ← 0; `init_system_roms`; `set_cartridge` (TYPE ← type|variant; ROM copy; require/prohibit flags; c64.cc:1242-1391).
6. Mixer restore; MODE ← 0x08.

### DMA load via boot cart (c64_subsys.cc:591-671; bootcrt.tas:14, 72-89, 450-451, 486-512)
1. Stop. DMA W $0172 ← runcode, $014F ← sync flag, $0175.. ← 16-char name, $0174 ← len, **$0002 ← 0x80**.
2. `start_cartridge(boot_cart)`: TYPE 0x41, ROM = bootcrt at 0x03C00000.
3. Wait 100 ms, stop. W $0173 ← drive, **$0002 ← 0x40** ("cart ready"; 6502 waits `bit $02 / bvc`).
4. Unless RUNCODE_REAL: loop resume / wait 25 ms / stop until **R $0002 == 0x01** (6502 `dma_loader` writes 1), at most 60 times. Then the file is DMA-written at its load address; $2D-$32, $AE-$AF, $90, $35-$36 are updated (673-739); **$0002 ← 0**; $0172 ← runcode; $00BA ← drv.
5. Resume. The 6502 finishes and writes $40 to $DFFF (cart off, bootcrt.tas:591, 609). `restoreCart` waits for CARTRIDGE_ACTIVE=0, then re-installs the configured cart and writes KILL ← 2 ×2 (c64_subsys.cc:178-192).

Raw DMA (REST `machine:writemem/readmem`, socket 0xFF06): stop → memcpy at 0x10050000+offset → resume, or `dma_transfer_frozen` when frozen (c64_subsys.cc:558-589).

### ROM upload
- KERNAL (with optional fast-reset patch at offset 0x1D6C), BASIC and char ROM are written to their windows on every `init_system_roms` (c64.cc:1077-1109): boot ×2, every cart start, and `set_rom_config`.
- `enable_kernal` (CRT with CART_KERNAL, socket 0xFF08, UCI `C64_SET_KERNAL`) writes 8192 bytes to 0x1018A000 while the C64 may be running (c64.cc:1399-1407; socket_dma.cc:172-173; c64_subsys.cc:466-469).

### Cartridge ROM placement (c64_crt.cc)
- Area cleared to 0xFF (334-337). CHIP packets placed at bank·16K + (load & 0x2000) (291-299). Chips <8K are mirrored to 8K (313-327).
- `auto_mirror` fills up to 64 banks (339-358).
- EasyFlash: EAPI at +0x3800 replaced with the internal driver (393-404).
- EEPROM chunk ($DE00) goes to EEPROM_BASE+0x800 (256-280, 743-785).
- Type/prohibit/require mapping at 466-651. `C64_CRT::check_header` rejects unimplemented types (204-223).

### SID addressing / audio
- Decode (derived from u64_config.cc:218-235, 1231-1255, 2303-2314): a SID answers when `((A11..A4) & MASK) == BASE`.
- STEREO_ADDRSEL / EMUSID_SPLIT pick one of A5..A9 as the A/B selector for dual devices (stereo_bits/split_bits u64_config.cc:319-320).
- `auto_mirror` clears mask bits for A5..A9 when all SIDs in $D400-$D7FF agree (2406-2465).
- SID player autoconfig rewrites bases/masks and mixer channels, then sets `skipReset` (2043-2105).
- External SID devices (FPGASID etc.) are configured by DMA writes to $D419/$D41A or $D41D-$D41F while stopped with MODE ← 0x02 (sid_device_*.cc; u64_config.cc:2536-2569).
- Audio path (closed): per-channel gains in the mixers, resampler FIR loaded per video mode.

### Joystick / paddles / mouse
`JoystickOutput::apply` ANDs USB port-1, REST persistent and REST timed overlays, active low (joystick_output.cc:75-93, 275-286). A 20 ms timer releases overlays (19, 45-49, 231-244).

### Streams
U64_UDP_BASE slot i holds a ready 42-byte header (checksum filled, data_streamer.cc:386-404). Setting ETHSTREAM_ENA bit i starts FPGA-generated UDP frames. Payload format is not in this tree (OPEN QUESTION 5).

## Emulator model tiers

### T0 — dummy C64, boot without hang
- **Cart regs 0x10040000-0F**: per-register latch following the cart_slot_registers.vhd bit rules. C64_STOP R = `req | (req<<1)`, i.e. the stop is instant. CLOCK_DETECT R = 0x01 (PHI2 present; bit4 0 whenever MODE reset is released). 0x01 leaves bits 2/3 (EXROM/GAME sense) at 0. Their only reader, `C64::get_exrom_game` (c64.h:323-325), has no caller, so the value does not matter. CARTRIDGE_ACTIVE R = 0.
- **DMA window 0x10050000**: 64 KB byte array plus minimal I/O overlay. Byte access, with 16/32-bit decomposition.
  - $D012 = free-running counter incremented per read (wraps through 0xFF); $D011 bit7 = counter bit8.
  - $D019 = 0; $DC01 = 0xFF; $DC00 = last written value.
  - $D400-$D7FF reads 0 (or RAM-like); other I/O RAM-like.
  - Ignore C64_MODE/DMA_MEMONLY mapping (always RAM).
- **C64_IO_BASE 0x10180000-FF**: RAM-like latch. CORE_VERSION a fixed non-zero value (OPEN QUESTION 6); VOICE_ADSR 0.
- **U64_IO_BASE**: CART_DETECT 0x03, RESTORE 0x00, BLACKBOARD 0x00 (or 0x01), KEYB_ROW 0xFF, KEYB_JOY 0xFF, others latch. HDMI_REG per video doc.
- **Write sinks (reads 0)**: mixers, speaker mixer, resampler, LED strip, palette, UltiSID filter RAM, UDP headers, EEPROM, CART_TIMING (reads 0).
- **ROM windows**: RAM-backed (readback of written data serves u64_machine.cc:92-107 and u64_memory_backend.cc:74-83).
- **Capabilities / ITU**: ITU_TIMER countdown and capability bits per ITU doc (H3, H9, H14).
- **Known T0 limits**: DMA load/SID player time out (H19); the C64-screen menu draws into the byte array only (no picture).

### T1 — mapping onto an external cycle-accurate C64 emulator

| FW register / action | Emulator hook |
|---|---|
| C64_MODE bit2/bit3 | assert/release /RESET (CPU, CIAs, VIC, SID; RAM kept). CLOCK_DETECT bit4 = reset line state |
| C64_MODE bit1 | force PLA ULTIMAX (GAME=0, EXROM=1) with freezer decode: only $0000-$0FFF RAM, I/O, cart ROM windows visible |
| C64_MODE bit4 | NMI line level |
| C64_STOP / STOP_MODE | hold 6510 (DMA) at next cycle meeting condition 0/1/2. Keep VIC/CIA/SID clocked. bit1 = held |
| DMA window byte R/W | bus cycle via PLA with current $01, cart lines, ULTIMAX flag. Normal I/O side effects (CIA ICR read clears, filetype_sid.cc:715-716). DMA_MEMONLY=1 → direct RAM. SERVE_WHILE_STOPPED → internal cart ROM/I/O answers |
| CARTRIDGE_TYPE/variant, KILL/force, ACTIVE | cart mapper (types c64.h:125-163) reading ROM from emulated DDR 0x03C00000 and RAM from 0x00EF0000. ACTIVE = mapper enabled. $DFFF←$40 on type 0x41 disables it |
| REU_ENABLE/SIZE | 1764-style REU at $DF00, memory = DDR 0x01000000, size 128K<<n. GeoRAM when TYPE=0x1F |
| BUS_INTERNAL / BUS_EXTERNAL / BUS_BRIDGE | gate internal IO1/IO2/ROM/IRQ; external port absent (CART_DETECT=3) |
| CMD_IF slot / SAMPLER_ENABLE | UCI at $DF1C/$DFFC/$DE1C; sampler at $DF20-$DFFF (UCI/sampler docs) |
| KERNAL/BASIC/CHAR windows | live ROM images, writable at any time |
| SIDx_BASE/MASK/EN, EMUSID*, ADDRSEL, SPLIT | route $D400-$D7FF/$DE00-$DFFF SID reads/writes to UltiSID1/2 instances. Sockets: none, or an emulated 6581/8580 answering H13. RES/WAVES/DIGI/filter curve RAM parametrise UltiSID |
| VOICE_ADSR | expose envelope levels |
| AUDIO_MIXER / SPEAKER_MIXER | per-channel L/R gains when mixing SID/sampler/drive/tape audio. Resampler only affects filter quality (may ignore) |
| VIDEOFORMAT / PHASE_INCR / SCANLINES | PAL (63 cycles, 50 Hz) vs NTSC (65 cycles, 60 Hz) selection per bits; frame renderer options |
| C64_PALETTE | 16-colour RGB for the frame renderer |
| JOY1/2_SWOUT, PADDLE_x, MOUSE_EN | AND into CIA1 $DC01/$DC00 joystick bits (active low); POTX/POTY inputs |
| Keyboard matrix | FW reads the C64 keyboard only through CIA1 over DMA (keyboard_c64.cc:209-243) and the overlay path U64II_KEYB_COL/ROW/JOY. USB→matrix injection is MATRIX_KEYB 0x10100300 (keyboard doc) |
| TURBOREGS_EN / SPEED_PREFER / SPEED_UPDATE | CPU speed multiplier (optional) |
| VIC_SPLIT | $D200-$D2FF writes → LED strip data |
| ETHSTREAM_ENA + UDP headers | emulator builds VIC/audio UDP frames from rendered frame / audio and injects them into the RMII TX path |
| EEPROM_BASE | GMOD2 93C86 model backed by 0x1004C800, dirty flag at 0x1004C000 |
| ITU IRQ 0x80 | raise on C64 reset (see OPEN QUESTION 3) |
| High IRQ 6 | raise on the U64 unlock sequence (OPEN QUESTION 4) |
| Video frame | no FW register reads video. Menu screen reads use DMA RAM $0400/$D800 (userinterface.cc:536-548) |

## Open questions

1. OPEN QUESTION: power-on state of C64_MODE reset and C64_STOP on U64-II. The `g_cartreset_init` / `g_boot_stop` generics of the closed top are unknown (cart_slot_registers.vhd:145-151). FW never assumes it; it sets both (c64.cc:147-148, u64_config.cc:914).
2. OPEN QUESTION: do DMA accesses to $0000/$0001 hit the (FPGA) 6510 port register or the RAM underneath? u64_machine.cc:37-56 and 79-89 suggest the port.
3. OPEN QUESTION: exact trigger of ITU bit 7. Candidates: reset assert, reset release, reset button, FW MODE writes. Level or edge (see ITU doc).
4. OPEN QUESTION: condition that raises high IRQ 6 and how it is cleared. The ISR only pokes $D038 ← 0 (u64_config.cc:1014).
5. OPEN QUESTION: payload/packet format of the FPGA VIC/audio/debug/IEC UDP streams (only the header template is in FW).
6. OPEN QUESTION: C64_CORE_VERSION value on current U64-II cores (display only).
7. OPEN QUESTION: resampler FIFO latching for 32-bit writes that arrive as 4 byte writes; meaning of LABOR (45/169).
8. OPEN QUESTION: CARTRIDGE_KILL bit1 "force" and the double write for "Carts V5" (c64.cc:1469-1474). U64 cart_active semantics.
9. OPEN QUESTION: VOICE_ADSR index layout beyond `sidsel*4 + voice` (led_strip.cc:323-325).
10. OPEN QUESTION: whether the U64 ROM windows are readable. c64.cc:1085 says write-only; u64_machine.cc:92-107 and u64_memory_backend.cc:74-83 read them.
11. OPEN QUESTION: exact SID decode formula (address range gating, split semantics). Derived from config tables only.
12. OPEN QUESTION: DMA behaviour when the C64 is not stopped. The FW pokes CIA1 at boot after `clear_ram` resumed the machine (u64_config.cc:950-952), and the unlock ISR pokes $D038 live. The open U2 bridge answers only stopped DMA (slot_master_v4.vhd:108).
13. OPEN QUESTION: CLOCK_DETECT bits1/2/3/5 on U64-II (no FW user except helpers).
14. OPEN QUESTION: effect of the 0xFF writes to C64_SWAP_CART_BUTTONS (c64.cc:274) and to 0x1010000C U2PIO_SPEAKER_EN/BOARDREV (u64_config.cc:1084) caused by missing config items on U64-II.
