# 00: Consolidated memory map, boot-hazard checklist, cross-doc contradictions (U64-II `ultimate`)

This is a synthesis of docs 01-12. Paths are relative to `firmware/1541ultimate/` unless marked `ELF`.
`ELF` means `target/u64ii/riscv/ultimate/result/ultimate.elf`, inspected with `riscv64-elf-objdump/nm/readelf`; addresses are valid for that build only.
Build defines: `-DRISCV -DU64=2 -DUSB2513 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000 -DCLOCK_FREQ=100000000 -DFP_SUPPORT=1`.
`U2P_IO_BASE` is `#ifndef`-guarded in `software/system/u2p.h:13-15`, so the command-line value 0x10100000 is the one in effect.

## Sources read

- **Docs:** 01-cpu-boot-memory, 02-itu-uart, 03-board-init, 04-esp32-wifi, 05-ui-overlay-input, 06-spi-flash-config, 07-sd-card-filesystems, 08-network-rmii, 09-usb, 10-c64-machine, 11-drives-iec-periph, 12-gaps.
- **Firmware, re-read to resolve conflicts:**

| File | Why |
|---|---|
| software/system/iomap.h:10-35, u64.h:14-87, u2p.h:13-73 | base addresses |
| software/system/u64ii_init.cc:100-171 | `custom_hardware_init` vs `SetVideoPll` / `SetExternalPLL` (BOARDREV read timing) |
| software/io/iec/iec_interface.cc:7-61,110-145; iec_drive.cc:34-38,160-172; printer/iec_printer.cc:108-118,136-146,368-376 | capability-gate crash path |
| software/network/socket_dma.cc:198-208; io/c64/c64_subsys.cc:408-420 | NULL `c1541_A` |
| software/components/init_function.cc:10-51; indexed_list.h:41-43,90,191-216 | InitFunction ordering |
| software/network/httpd.cc:15-26 | HTTP Daemon registration |
| software/u64/u64_config.cc:478-531,892-910,2236-2384,572-640; u64_config.h:94-100 | member ctor order, SID probe outcomes |
| software/io/c64/c64.h:202-203 | `C64_PEEK`/`C64_POKE` = raw DMA byte access |
| fpga/io/itu/vhdl_source/itu.vhd:16-18; fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd:507-508; fpga_top/ecp5_test/vhdl_source/{ecp5_tester.vhd:443-444, basic_io.vhd:225} | ITU edge generics |
| target/u64ii/riscv/bootloader/Makefile:11,21; target/u64ii/riscv/ultimate/Makefile | CPU core; no copper.cc in SRCS |
| README.md:7 (this repo) | project claim "neorv32" |

- **ELF checks:**
  - `objdump -h`, `readelf -l`, `nm -n` for layout symbols.
  - Disassembly of `vPortSetupTimerInterrupt` (0x3DFC4) and `freertos_risc_v_trap_handler` (0x33700).
  - Census of CSR instructions and opcodes.
  - `.init_array` (0x1035BC-0x1036F3, 78 entries) mapped to `_GLOBAL__sub_I_*`.
  - Every `InitFunction::InitFunction` call site with its name string and ordering.
  - Every load/store whose base comes from a `lui` constant outside the listed IO windows: none found. Base+index forms in `U64Machine::read_cpu_block` (ELF 0x52A14-0x52A88) resolve to the ROM windows.

---

## 1. Consolidated address map (address-sorted)

**Bus rules (doc 01)**
- Data access with addr bit 28 = 0 goes to DDR (26-bit, 64 MB). Bit 28 = 1 goes to the 8-bit IO bus (24-bit address).
  - 16/32-bit IO accesses become 2/4 byte strobes at addr+0..+3, LSB first, each with its own side effects (bus_converter.vhd:56,82-93,159-185).
- 0x8000xxxx (bit 28 = 0) is the rvlite boot BRAM, not DDR: the converter matches `addr[31:16] = 0x8000` before it tests the IO bit (bus_converter.vhd:96,118,123). The emulator loads the ELF directly, so this page reads 0 and ignores writes.
- Instruction fetches go through a second `bus_converter` with `g_support_io => false` (rvlite_wrapper.vhd:101-106), so a fetch ignores bit 28. Only the boot BRAM page is special; every other fetch reads DDR (modulo 64 MB).
- There are no bus-error exceptions (doc 01 H16).
- Emulator default for every unlisted address: **read 0, ignore writes, never fault.**

**T0 / T1 columns:** T0 = the model needed to boot to the UI loop without hanging. T1 = the functional model. "RAZ/WI" = read 0, ignore writes.

### 1a. CPU address space: RAM and boot ROM

| Addr range | Block | Doc | T0 model | T1 model |
|---|---|---|---|---|
| 0x00000000-0x0000FFF7 | DDR low RAM. Not used by the app, but `NULL->current_address` is read at 0x0 for a root USB device (usb_base.cc:411) | 01, 09 H9 | RAM | RAM |
| 0x0000FFF8-0x0000FFFF | BOOT_MAGIC jump addr / magic 0x1571BABE (boot ROM only; the app never reads it) | 01 | RAM | RAM |
| 0x00010000-0x0002FFFF | boot ROM `ram_test` scratch | 01 | RAM | RAM |
| 0x00030000-0x00103513 | `.text`. `_start`=0x30000 (entry), `__crt0_dummy_trap_handler` 0x30178, `freertos_risc_v_trap_handler` 0x33700, `vPortSetupTimerInterrupt` 0x3DFC4 (ELF). The first three are `STT_NOTYPE`, size 0 (doc 01 §B) | 01 | load ELF PT_LOAD (single RWE segment 0x30000, filesz 0x126AE8, memsz 0xE19644) | + invalidate any decode cache on writes (`.u2p` jump, filetype_u2p.cc:93-140) |
| 0x00103514-0x001035BB | section `detect_sid` (holds `DetectSidImpl`, ELF) | 10 | part of load | same |
| 0x001035BC-0x0014B603 | `.rodata`. Contains `.init_array` 0x1035BC-0x1036F3, `_iec_code_b_start` 0x120D1A (0x768 B), `_nano_minimal_b_start` 0x121482 (0x58E B) (ELF) | 01, 09, 11 | part of load | same |
| 0x0014B608-0x00156AE7 | `.data` (`xISRStackTop` 0x1566C4) | 01 | part of load | same |
| 0x00156AF0-0x00E49643 | `.bss` (NOLOAD, zeroed by crt0 word by word). Contains `custom_outbyte` 0x156AF0, `xISRStack` 0x1590F0 (256 words) and **`ucHeap` 0x1594F0-0x009594EF** (8 MiB FreeRTOS heap: every `new`/`malloc`). The heap holds DMA targets: RMII pool, WiFi cmd buffers, WD177x buffer, GCR image buffers (doc 04, 08, 11) | 01 | RAM (zero-fill is part of crt0) | same |
| 0x00E49644-0x00E8FFFF | `__heap_start`..`__heap_end`: sbrk heap (effectively unused). Initial `sp` = 0x00E8FFFC (`__crt0_stack_begin`) | 01 | RAM | RAM |
| 0x00E90000-0x00EA7FFF | not referenced by linker.x | — | RAM | RAM |
| 0x00EA8000-0x00EAFFFF | `__kernal_area` (c64.cc:1412-1422) | 01 | RAM | RAM |
| 0x00EB0000-0x00EBFFFF | `__drive_b_sound` (48 K used) | 01, 11 | RAM | shared with the drive-B sound model |
| 0x00EC0000-0x00ECFFFF | `__drive_a_sound` | 01, 11 | RAM | shared with the drive-A sound model |
| 0x00ED0000-0x00EDFFFF | `__drive_b_area` (6502 64 K image, ROM at +0x8000) | 01, 11 | RAM | shared with the external drive-B CPU |
| 0x00EE0000-0x00EEFFFF | `__drive_a_area` (ZP $77/$78 at 0x00EE0077/78) | 01, 11 | RAM | shared with the external drive-A CPU |
| 0x00EF0000-0x00EFFFFF | `__cart_ram_start` (cart function RAM) | 01, 10 | RAM | shared with the cart mapper |
| 0x00F00000-0x00FFFFFF | not referenced | — | RAM | RAM |
| 0x01000000-0x01FFFFFF | REU / GeoRAM memory (c64.h:14). Also the sampler's default START (sampler_pkg.vhd:46) | 01, 10, 12 | RAM | shared with the REU and sampler DMA |
| 0x02000000-0x02FFFFFF | RAM disk "Temp", f_mkfs at every boot (ramdisk.cc:25-41) | 01, 07 H13 | **RAM (must exist)** | RAM |
| 0x03000000-0x03BFFFFF | `__updater_start` (no compiled user) | 01 | RAM | RAM |
| 0x03C00000-0x03FFFFFF | `__cart_rom_start` (CRT banks, boot cart) | 01, 10 | RAM | shared with the cart mapper |
| 0x04000000-0x0FFFFFFF, 0x20000000-0x2FFFFFFF, … (bit 28 = 0) | DDR alias modulo 64 MB per rvlite IP (OPEN, Q-A4). The firmware never accesses these. The `lui 0x20000` hits are constants (ELF) | 01 | RAZ/WI or mirror | per real top |
| 0x80000000-0x80001FFF | rvlite boot BRAM (code 0x1C00 + RAM 0x400), reset PC | 01 | not needed (load the ELF) | optional boot-ROM run |
| (neorv32 internal IO, CLINT/MTIME, SYSINFO) | **Does not exist.** The build targets rvlite: no neorv32 CSRs or MMIO, tick from the ITU (see C1) | 01 | nothing | nothing |

### 1b. IO bus (0x10000000-0x10FFFFFF; higher bit-28-set addresses alias per rvlite IP, OPEN Q-A4)

| Addr range | Block | Doc | T0 model | T1 model |
|---|---|---|---|---|
| 0x10000000-0x10000009 | ITU IRQ controller + timers: GLOBAL, ENABLE, DISABLE, EDGE, CLEAR, ACTIVE, ITU_TIMER (5 µs countdown), IRQ_TIMER_EN/HI/LO | 01, 02 | IRQ core (mask/flag/clear/active), 0x06 countdown or instant 0, tick bit0 every 499 968 clocks | full itu.vhd incl. counter readback |
| 0x1000000A | ITU_BUTTON_REG (bit6 = menu) | 02, 05 | 0x00 | host menu key → bit6 pulse 50-200 ms |
| 0x1000000B | ITU_FPGA_VERSION (cosmetic) | 02 | constant | constant |
| 0x1000000C-0x1000000F | CAPABILITIES, big-endian u32 | 02, 03, 11 | **0x34000222** (§3 C4) | per modelled feature, e.g. 0x3DE40BE7 |
| 0x10000010-0x10000013 | UART DATA / GET / FLAGS / ICTRL | 01, 02 | FLAGS=0x40, DATA W → host stdout | + optional FIFO pacing |
| 0x10000014-0x1000001F | UART aliases (`addr[1:0]`). 0x1000001F ← 0x49 from the early trap handler (crt0.S:307-309) | 02 | RAZ/WI | log "early trap" |
| 0x10000022-0x10000023 | ITU_MS_TIMER HI/LO | 02 | ⌊t_ms⌋ & 0xFFFF, stable between reads | same |
| 0x10000024, 0x25, 0x29 | USB / SD / PRINTER busy LED | 02, 11 | WI | LED indicator |
| 0x10000026 | ITU_MISC_IO (no writer in ELF) | 02 | WI | WI |
| 0x10000027 | ITU_IRQ_HIGH_EN (read-modify-write) | 02, 04, 11 | latch + readback | same |
| 0x10000028 | ITU_IRQ_HIGH_ACT = src_high & HIGH_EN (level) | 02 | 0 (no sources) | per source |
| 0x10000030-0x1000003F | ITU splitter dummy | 02 | RAZ | RAZ |
| 0x10020000-0x1002000E | Drive A registers | 11 | RAZ/WI is valid (IRQ_ACK=0, DIRTY=0) | latches per drive_registers.vhd |
| 0x10020800-0x1002087F | Drive A DIRTY (**not RAM**) | 11 H4 | read 0x00 | floppy.vhd:148-189 semantics |
| 0x10021000-0x100217FF | Drive A param RAM (512 × 32-bit LE; reads 0) | 11 | WI | track table → external drive |
| 0x10021800-0x1002180F | Drive A WD177x | 11 | 0x10021806 = 0x00, 0x1002180E = 0x00 | FIFO + DMA + stepper |
| 0x10024000-0x1002580F | Drive B (same layout +0x4000) | 11 | as drive A | as drive A |
| 0x10028000-0x1002800F | IEC processor registers | 11 | R0=0x25, R1=0x01, R2=0x01, others 0 | HLE or LLE microcode |
| 0x100287FF | stray write target (`dst[-1]`, iec_interface.cc:121-126,141) | 11 H10 | WI | WI |
| 0x10028800-0x10028FFF | IEC CODE RAM (0x768 bytes loaded) | 11 | RAM or WI | RAM |
| 0x10040000-0x1004000E | C64 cart / machine control (MODE, STOP, STOP_MODE, CLOCK_DETECT, …) | 10 | latches. STOP R = req\|req<<1. CLOCK_DETECT = 0x01 | cart_slot_registers.vhd + external C64 |
| 0x10042000-0x1004200B, 0x10042800-0x10042FFF | legacy SID_BASE (dead in this build) | 12 | RAZ/WI | RAZ/WI |
| 0x10044000-0x1004400F | UCI registers | 11, 10 | +4/+5/+6/+7/+8/+9 read 0x00/0x6F/0x70/0xDF/0xE0/0xFF. HANDSHAKE and STATUS read 0. Idle read pointers: +A = 0x00 (status 0x700), +C/+D = 0x80/0x03 (response 0x380). IRQMASK reset 7 | command_protocol.vhd |
| 0x10044800-0x10044FFF | UCI RAM (cmd 0x10044800, resp 0x10044B80, status 0x10044F00) | 11 | RAM | RAM shared with the C64 side |
| 0x10046000-0x100467FF | CART_TIMING_BASE (== COPPER_BASE, iomap.h:17-18; copper.cc not in SRCS) | 10 | RAZ/WI | developer only |
| 0x10048000-0x10049FFF | Sampler (256 B aliased over 8 K) | 12 | modelled since S16: even reads the IRQ status vector, odd the version 0x10 | voice engine (docs/specs/S16-ultimate-audio.md) |
| 0x1004A000-0x1004A00A | ACIA 6551 registers | 11 | latches, irq_source 0 | acia6551.vhd |
| 0x1004A800-0x1004A9FF / 0x1004AA00-0x1004ABFF | ACIA TX ring / RX ring RAM | 11 | RAM | RAM |
| 0x1004C000 / 0x1004C800-0x1004CFFF | EEPROM dirty / data (GMOD2) | 10 | RAZ/WI | 93C86 model |
| 0x10050000-0x1005FFFF | C64 DMA window (one byte = one C64 bus cycle) | 10, 03, 05 | 64 K byte array + I/O overlay: $D012 reaches 0xFF, $DC00/$DC01 = 0xFF, $D019=0, $D400-$D7FF 0x00 or 0xFF | external C64 via PLA |
| 0x10060000-0x100600FF | SD SPI (4 registers of 4 B each, repeating every 16 B) | 07 | RAZ/WI (CD=0) | SPI master + SDHC card |
| 0x10060100 | RTC_BASE (I2C RTC, no user) | 07 | RAZ/WI | RAZ/WI |
| 0x10060200-0x1006020F | SPI flash controller (DATA +0, RATE +4, CTRL +8, CRC +C) | 06 | CS framing + JEDEC `01 60 18`, SR1=0, UID | NOR model 16 MiB, persistent |
| 0x10060304 / 0x10060305 | TRACE_BASE: PROFILER_SUB / PROFILER_TASK (written on every context switch) | 01, 11 | WI | WI |
| 0x10060400-0x1006040F | RTC seconds timer (32-bit LE) | 07 | host UTC epoch | lock/unlock semantics |
| 0x10060500-0x1006050F | GCR codec | 11 | implement or RAZ (not used at boot) | gcr_codec.vhd exact |
| 0x10060604 / 0x10060608 | ICAP PULSE / WRITE | 06 | WI | IPROG → full reset |
| 0x10060700-0x10060701 | Audio select | 12 | WI | store only |
| 0x10060800-0x1006082F | RMII MAC (RX filter / TX / free queue) | 08 | RAZ/WI (TX_BUSY=0, ALLOC_VALID=0) | MAC + host bridge |
| 0x10060900-0x1006090F | WiFi DMA UART | 04 | live status bits, flowctrl b7 reads 0, TX completion, stub u64ctrl | virtual u64ctrl + L2 bridge |
| 0x10080000-0x100807FF | USB nano 2 K BRAM (blob, pipe descriptors 0x10080600, FIFO 0x10080700, status 0x100807CC-0x100807F2) | 09 | RAM (8/16/32-bit); never raise ITU bit 2 | HLE nano |
| 0x10080800 (-0x10080FFF alias) | NANO_START | 09 | WI, read 0 | run control |
| 0x100A0000-0x100A07FF / 0x100A0800-0x100A0FFF | C2N playback control-status / FIFO | 11 | status 0x80 on every read in the window (c2n_playback_io.vhd:87-89) | pulse generator |
| 0x100C0000-0x100C07FF / 0x100C0800-0x100C0FFF | C2N record control-status / FIFO | 11 | status 0x00 | edge timer + FIFO + IRQ |
| 0x10100006 | U2PIO_GET_MDIO | 03, 08 | 0 (link down) | MDIO PHY decoder |
| 0x10100008-0x10100009 | U2PIO SCL/SDA (bit-bang, unused on U64-II) | 03 | RAZ/WI | RAZ/WI |
| 0x1010000A / 0x1010000B | U2PIO SET_MDC / SET_MDIO | 03, 08 | WI | MDIO decoder |
| 0x1010000C | **R** U2PIO_BOARDREV / **W** U2PIO_SPEAKER_EN (same address) | 03, 10 | R = 0xB8 constant, writes do not change reads | U2+ RTL: R bits 7:3 = board_rev, bit0 = speaker_en readback (u2p_io.vhd:75-77,122-124); FW uses only `>>3` |
| 0x1010000D / 0x1010000F | HUB_RESET / ULPI_RESET | 03, 09 | latch bit0 | reset events to USB model |
| 0x10100100-0x1010010E | DDR2 PHY (boot ROM only) | 01 | RAZ/WI | RAZ/WI |
| 0x10100200-0x10100203 | U64_CLOCKMEAS (no user) | 10 | RAZ | RAZ |
| 0x10100300-0x1010030A | MATRIX_KEYB (USB/REST → C64 matrix) | 05, 03 | RAM or WI | latch → C64 CIA |
| 0x1010030B-0x1010030E | MATRIX_WASD_TO_JOY (32-bit store at an unaligned address) | 05 #11, 03 | WI (no alignment trap) | latch |
| 0x10100400 | U64_HDMI_REG (W 0x20/0x10/0x08, R bit2 HPD, bit3 WASLOW) | 03, 05, 10 | R 0x04 | HPD model + high IRQ 5 |
| 0x10100401 | U64_POWER_REG (U64 mk1 only) | 03 | WI | WI |
| 0x10100402 | U64_RESTORE_REG | 03, 06, 07 | 0x00 | host "safe mode" switch |
| 0x10100403 | U64_CART_DETECT | 03, 10 | 0x03 | external cart emulation |
| 0x10100404-0x10100405 | HDMI_PLL_RESET / USERPORT_EN | 03 | WI / latch | same |
| 0x10100406 | U64II_KEYB_JOY (R joystick lines, W swap) | 03, 05 | R 0xFF | host joystick |
| 0x10100407 | U64II_BLACKBOARD | 03, 10 | 0x01 | same |
| 0x10100408-0x10100409 | HDMI_ENABLE / INT_CONNECTORS | 03, 10 | WI | store |
| 0x1010040A / 0x1010040B | KEYB_COL (W) / KEYB_ROW (R, also W 0xFF) | 03, 05 | COL latch, ROW = 0xFF (stable) | 8×8 matrix from host keys |
| 0x1010040C-0x1010040F | LEDSTRIP_EN, PWM_DUTY, CASELED_SELECT, ETHSTREAM_ENA (read-modify-write) | 03, 10 | latch + readback | same |
| 0x10100500-0x10100513 / 0x10100540-0x10100553 | audio mixer / speaker mixer (write-only) | 03, 10 | WI | gains |
| 0x10100580-0x10100589 | resampler DATA(32) / RESET / LABOR / FLUSH | 03, 10 | WI | ignore or store |
| 0x10100600-0x101006FF | LED strip | 03 | WI | renderer |
| 0x10100700-0x10100708 | HW I2C master | 03 | R 0x701 = 0x00, R 0x700/0x704/0x705 = 0xFF, writes ignored | transaction engine + devices |
| 0x10100800-0x10100803 | Blingboard keyboard RX (FW writes FLAGS, reads bit2) | 03, 05 | RAZ/WI | cosmetic |
| 0x10100900 | Blingboard LEDs (never accessed) | 03 | — | — |
| 0x10140000-0x1014000D | overlay chargen registers (never read) | 05 | write latches | renderer inputs |
| 0x10141000-0x10141FFF / 0x10142000-0x10142FFF | overlay screen RAM / colour RAM (read back by the FW) | 05 #12 | **4 K RAM each** | render source |
| 0x10144000-0x1014401D | HDMI timing regs | 03, 05 | WI | framebuffer geometry |
| 0x10145000-0x1014503F | HDMI palette (16 × RGBx) | 03, 05 | RAM | overlay colours |
| 0x10148000-0x10148003 | VIC cropper | 03 | WI | geometry |
| 0x10180000-0x10180037 | C64 core config (VIDEOFORMAT, DMA_MEMONLY, SID bases/masks, CORE_VERSION +0x10 R, BUS_*, JOY/PADDLE, …) | 10 (03 subset) | RAM-like latch. CORE_VERSION constant | external C64 hooks |
| 0x10180080-0x10180087 | C64_VOICE_ADSR (R; index sidsel·4+voice, layout OPEN) | 10, 03 | 0 | envelope levels |
| 0x10180800-0x1018083F / 0x10180C00-0x10180C3F | C64 palette RGB / YUV | 10, 05 | WI | frame renderer palette |
| 0x10181000-0x1018101F | C64_PLD_ACC (STATE0/1 R, PORTA/B W, JOYCTRL 0x1018101E) | 05, 10 | RAM | same |
| 0x10181800 | U64_DEBUG_REGISTER (REST/socket; ELF uses lui 0x10182 − 0x800) | 10 | RAM | RAM |
| 0x10182000-0x101821FF | C64_GLYPH (no user) | 10 | — | — |
| 0x10185000-0x101857FF / 0x10185800-0x10185FFF | UltiSID1 / UltiSID2 filter curve (C64_SID_BASE 0x10184000 + 0x1000/+0x1800) | 10, 12 | WI | UltiSID parameters |
| 0x10188000-0x10189FFF | U64 BASIC ROM window | 10 | **RAM (read back)** | live ROM |
| 0x1018A000-0x1018BFFF | U64 KERNAL ROM window | 10 | **RAM (read back)** | live ROM |
| 0x1018C000-0x1018CFFF | U64 CHAR ROM window | 10 | **RAM (read back)** | live ROM |
| 0x10190000-0x101900FF | U64_UDP_BASE stream header templates | 08, 10 | WI | stream generator |
| 0x10200000-0x102000FE | MMCM DRP (`uint16_t` index i at byte 2·i) | 01, 03 | WI | optional clock decode |
| 0x102000FF | MMCM_RESET (0xB3, 0x3B) | 01, 03 | WI | WI |

### 1c. Address-map overlaps / conflicts and their resolution

| # | Overlap / conflict | Resolution |
|---|---|---|
| M1 | 0x1010000C is BOARDREV on read and SPEAKER_EN on write. The U64-II build writes 0xFF there (u64_config.cc:1082-1084) | On U2+ RTL a read returns `board_rev` in bits 7:3 and the written `speaker_en` in bit0 (u2p_io.vhd:75-77,122-124). The firmware uses only `>> 3` (product.cc:44,69), so bits 7:3 must not follow writes: a RAM model turns BOARDREV into 0x1F and makes `isEliteBoard` false (doc 03 H18, doc 10 H20). The T0 constant 0xB8 is sufficient |
| M2 | 0x10046000 is both CART_TIMING_BASE and COPPER_BASE (iomap.h:17-18) | copper.cc is not in the ultimate Makefile SRCS; only CART_TIMING users (c64.cc:1793-1852) exist. No conflict |
| M3 | Doc 01 lists 0x10060200/08 as "boot ROM only"; doc 06 shows heavy app use | App use confirmed: `get_flash()` from ConfigManager (config.cc:37), ELF `lui 0x10060` × 75. Doc 01 was scoped to the boot ROM |
| M4 | Doc 03 lists C64 core config as 0x10180000-0x1018002F; doc 10 lists up to 0x10180037 | Superset: doc 10 (joystick_output.cc:81-91) is authoritative |
| M5 | 0x10100406 read = joystick lines, write = swap bit; 0x1010040B read = row, write = 0xFF | Separate read and write functions (doc 05 OQ6 still open on hardware semantics) |
| M6 | 0x10100400 bit 0x08 is `HPD_RESET` on write and `HPD_WASLOW` on read (u64.h:95-96) | Separate read and write functions; the FW never reads WASLOW |
| M7 | 0x100287FF lies just below IEC CODE RAM | It is the `slots[3]` stray write, a no-op in the VHDL (doc 11 H10) |
| M8 | Doc 10 OQ10 says the ROM windows are "write-only" (c64.cc:1085), but the FW reads them | ELF: `U64Machine::read_cpu_block` (0x52A14-0x52A88) loads from 0x1017E000+addr ($A000 → 0x10188000), 0x1017C000+addr ($E000 → 0x1018A000) and 0x1017F000+addr ($D000 → 0x1018C000). Emulator: RAM-backed with readback. Hardware readability stays OPEN |
| M9 | 0x01000000 is REU memory and the sampler default START | Intentional sharing (MOD data loaded into REU memory, filetype_reu.cc:117) |
| M10 | DMA masters' address widths: WiFi and RMII 26 bits, USB 26 bits, WD177x **24 bits** (wd177x.vhd:150), drive params 26 bits | All DMA buffers are heap (< 0x00E49644) or fixed DDR pools, so everything fits 24 bits. Mask per block |

---

## 2. Boot hazard checklist, in firmware execution order

Order is derived from: crt0.S:53-218, the `.init_array` order (ELF), riscv_main.c:152-187, port.c:151-192, port_asm.S:309-349, ultimate.cc:79-207, and the InitFunction order resolved in §3 C7.
"Req" = the value or behaviour the emulator must provide.

### Phase A: crt0, before `main` (interrupts off, no tick)

| # | Where | Hazard | Req | Doc |
|---|---|---|---|---|
| A1 | crt0.S:53,75-89 | CSR writes to mip, mcountinhibit, mcounteren, mcycle(h), minstret(h). ELF census: CSRs used = mstatus, mie, mip, mtvec, mepc, mcause, mtval, mscratch, mcounteren, mcountinhibit, mcycle(h), minstret(h) | No trap on any CSR. Unknown CSRs RAZ/WI. mtval reads 0 | 01 H2 |
| A2 | crt0.S:280-315 | Any trap here makes the dummy handler write 0x49 to 0x1000001F | Accept the write; log it | 02 H14 |
| A3 | crt0.S:161-190 | `.bss` zero 0x156AF0-0xE49643 (~3.3 M word stores) | RAM covers 0x30000-0xE8FFFF (and 0-0x3FFFFFF for the pools) | 01 H14 |
| A4 | .init_array #30 `usb2` (usb_base.cc:43) | W 0x10000002 ← 0x04 | Accept | 09 |
| A5 | .init_array #43 `cmd_if` (command_intf.cc:39-66) | **Capabilities read before `main`**. If COMMAND_INTF & CARTRIDGE: W 0x10044000/02, then R 0x10044006/08/04 give the buffer bases | Capabilities valid from reset. If bit 18 is set: 0x70 / 0xE0 / 0x00, otherwise buffers alias or overrun | 11 H1, H6 |
| A6 | .init_array #54 `Acia` (acia.cc:18-20) | Capabilities read. If ACIA: W 0x1004A00A/07 | Accept | 11 |
| A7 | .init_array #72 `Esp32` → `DmaUART` ctor (dma_uart.h:65-81) | R 0x10060900/01 (discarded), W 0x10060903 ← 0x80 ×2, `install_high_irq(3)` read-modify-write on 0x10000027 | HIGH_EN reads back. 0x10060903 read returns b7 = 0 | 04 H2, H3 |
| A8 | any ctor printf | UART TX poll (see B1) | FLAGS bit4 = 0 | 02 H1 |

### Phase B: `main`, before the scheduler (interrupts off, no tick; every poll must resolve without IRQs)

| # | Where | Hazard | Req | Doc |
|---|---|---|---|---|
| B1 | riscv_main.c:154 → itu.c:289 | `while (UART_FLAGS & 0x10)` before every console byte; this is the first app IO read | 0x10000012 = 0x40 (never bit4). DATA writes → host console (strip `\r`) | 01 H1, 02 H1 |
| B2 | u64ii_init.cc:116 | `Hw_I2C_Driver` ctor: W 0x10100708 ← 0 | Accept | 03 |
| B3 | u64ii_init.cc:120 → nau8822.cc; hw_i2c_drv.cc:4-5,12-37 | I2C status 0x10100701 bit7 polled with **no timeout** | bit7 = 0. bit2 = 0 (ACK) for 0x34 | 03 H1, H2 |
| B4 | nau8822.cc:18 → itu.c:63-71 | `wait_ms(2)`: W 0x10000006 ← 200, poll until 0 (×2) | Countdown by emulated time (−1 per 5 µs), or instant 0. Must not need IRQs | 02 H2, 03 H3 |
| B5 | u64ii_init.cc:123 → usb_hwinit.cc:144-169 | W 0x10080800; 1024 × W16 0x10080000..; I2C probe 0x58/0x5A on ch2; 50 × R 0x1010000D | USB window accepts 8/16/32-bit writes. I2C BUSY = 0. ACK or NACK both boot | 09 H1, H2 |
| B6 | u64ii_init.cc:126-136 | W 0x10100400 ← 0x20; I2C writes to 0x40 and 0x42 on ch1 | BUSY = 0 | 03 |
| B7 | tasks.c:2065 → port.c:157-162 | `configASSERT((mtvec&3)==0)` and ISR stack alignment. Failure → `vAssertCalled` loops forever | mtvec reads back with bits 1:0 = 0 | 01 H4 |
| B8 | riscv_main.c:171 (ELF 0x3DFC4: `lw a5,0x33700`, `csrw mtvec,a5`) | mtvec ← **0xF8810113** (the handler's first instruction word, not its address) | Accept; mask to 0xF8810110; no abort | 01 H3, §3 C6 |
| B9 | riscv_main.c:178-186 | ITU program: 0x27←0 (drops the WiFi HIGH_EN set in A7), 0x07←0, 0x02←FF, 0x04←FF, 0x08←07, 0x09←A0, 0x07←1, 0x01←1, 0x00←1 | Reload 0x07A0 → period (0x7A0+1)·256 = 499 968 clocks = 4.99968 ms | 02 |
| B10 | port.c:192; port_asm.S:309-310,347-349 | MEIE set, final mtvec, `csrrw mstatus` sets MIE; first task via `ret` | mret: MIE←MPIE, MPIE←1, stay in M-mode. ECALL: mcause 11, mepc = PC of the ecall. IRQ: mcause 0x8000000B | 01 H5-H7 |

### Phase C: scheduler running (**a working 200 Hz tick is mandatory from here**)

| # | Where | Hazard | Req | Doc |
|---|---|---|---|---|
| C1 | riscv_main.c:86-129 | Tick ISR: R 0x05 (ACTIVE) → W 0x04 (CLEAR) ← same → R 0x28 (HIGH_ACT) | Bit-0 edge flag set every 4.99968 ms. ACTIVE = (flag \| src&~edge) & mask. CLEAR clears flags. Line = GLOBAL & ((ACTIVE≠0) \| (src_high & HIGH_EN ≠ 0)). No stuck sources | 01 H8-H9, 02 H6-H8 |
| C2 | idle task (no WFI, FreeRTOSConfig.h:40) | The CPU spins | Emulated time advances with executed instructions | 01, 02 H6 |
| C3 | tasks created in ctors (UCI tasks if bit 18; "SD Card Manager") | UCI: W 0x10044005←7, 0x10000001←0x10; W 0x10000004←0x80, 0x10000001←0x80 (command_intf.cc:101-102,117-118). SD: R 0x10060008 every 100 ms | 0x10044003 idle 0x00 and the UCI line gated by IRQMASK. ITU bit7 never active unless enabled (doc 12: `xSemaphoreGiveFromISR(NULL)` → assert). 0x10060008 = 0x00 (no card) | 11 H7, 12, 07 H1 |
| C4 | ultimate.cc:87-90 | Capabilities: CARTRIDGE (else no C64 UI loop, main task suspends), ULTIMATE64 (else no U64Config/overlay geometry), FPGA type (flash layout), bits 25/31 = 0 | **0x34000222** (§3 C4) | 01 H12-H13, 02 H9-H11, 05 #1-2 |
| C5 | ultimate.cc:94 → InitFunction [0] SID Cart, Boot Cart | no bus hazards documented | — | — |
| C6 | [1] **U64 Config**. Member `U64Mixer` ctor `register_store` (u64_config.cc:478-480) → `ConfigManager` ctor (config.cc:37-66) | `get_flash()` testers W25Q → S25FL → S25FL-L (`.init_array` #1-#3). JEDEC 00/FF → stub flash, no persistence, list grows | JEDEC `01 60 18`. CS framing: CTRL=0 per-byte frame, 1 joins the frame, 3 drops bytes | 06 H1, H7 |
| C7 | s25fl_l_flash.cc:40,48,53 | S25FL-L probe runs `wait_ms(1)` ×3 | ITU_TIMER countdown (B4) | 06 H2 |
| C8 | config.cc:48; blockdev_flash.cc:161 | U64_RESTORE_REG == 1 → safe mode, no /flash | 0x10100402 = 0x00 | 03 H10, 06 H4 |
| C9 | w25q_flash.cc:81-83; blockdev_flash.cc:148-183 | FPGA type picks the FAT offset (0x400000 / 0x580000). FAT reads use opcode 0x13 plus DATA_32 byte order. Blank → f_mkfs; if writes are lost → no /flash | Type matches the image. 4-byte opcodes. DATA_32 = b0\|b1<<8\|… | 06 H5, H6, H8, H9 |
| C10 | w25q_flash.cc:489-506 (interrupts masked) | `wait_ready`: SR1 BUSY poll against `getMsTimer` | `05` returns SR1 with BUSY=0 on every byte. ms timer free-running and stable across two reads | 06 H3, 02 H4 |
| C11 | u64_config.cc:521-531 `U64SidSockets` ctor | `isEliteBoard` → R 0x1010000C >> 3 ∈ {0x13, 0x15-0x17} | 0xB8 (rev 0x17) | 03 H6, 10 H20 |
| C12 | u64_config.cc:904-914; c64.cc:147-148,1514,399-409 | ULTIMATE64 gate. C64 ctor: STOP_MODE←2, MODE←0. CART_DETECT. `hard_stop`: `while(!(C64_STOP & 2));` **no timeout** | CART_DETECT = 0x03. STOP bit1 = 1 immediately after bit0 is written | 10 H1, H6; 03 H9, H12 |
| C13 | u64_config.cc:938; i2c_drv.h:46-63 | `enable_scan` → every later `i2c_lock` does `xSemaphoreTake(…,5000)` + `vTaskDelay(2)` | Working tick (C1) | 03 H21 |
| C14 | u64_config.cc:661-663 | MODE←0x08 then `while (CLOCK_DETECT & 0x10);` no timeout | 0x10040003 bit4 = 0 | 10 H4, 03 H11 |
| C15 | u64_config.cc:572-640; sid_device_pdsid.cc:113; sid_device_sidkick.cc:154-184 | SID signature probes over DMA ($D400/01 = 1D F5, $D41B/1C = 'S''W'/'N''O', PDsid, SIDKick) | Constant 0x00, constant 0xFF or RAM-like readback all give "none" (re-read, §3 C11) | 10 H12, 03 H14 |
| C16 | u64_config.cc:2236-2240 inside `portENTER_CRITICAL` (2349), up to 3× | `while (C64_PEEK(0xD012) != 0xFF);`: **hard freeze with interrupts masked** | 0x1005D012 reaches 0xFF by reads alone (constant 0xFF, or a per-read counter) | 10 H5, 03 H13 |
| C17 | u64_machine.cc:349-373 → c64.cc:462-497 | `clear_ram`: `stop(false)` polls STOP bit1 against an ITU_TIMER budget, then a forced unbounded poll; 64 K DMA writes | STOP bit1 as C12; ITU_TIMER counts | 10 H2, H3 |
| C18 | u64_config.cc:948-964; keyboard_c64.cc:118-155 | Boot hotkey: `do{R $DC01; W $DC00} while (R $DC01 differs)` **no timeout**; key codes 0x10/0x0E force PAL/NTSC | $DC01 stable 0xFF; $DC00 reads back what was written | 10 H10-H11, 03 H15, H17 |
| C19 | u64_config.cc:1054-1162 | `effectuate_settings`: W32 0x1010030B (unaligned); W 0x1010000C ← 0xFF; `SetVideoPll` / `SetExternalPLL` read BOARDREV (u64ii_init.cc:152,164) → PLL I2C channel; MMCM; resampler 512 × W32 | No alignment trap. BOARDREV reads independent of writes; ≠ 0x15 | 05 #11, 03 H7, H18 |
| C20 | u64_config.cc:969-974,1002-1010,2577-2691 | HPD task + `install_high_irq(5,6)`. About 200 ms later: R 0x10100400 bit2; if set, EDID read on ch0 0xA0 | 0x04 (NACK on EDID is harmless → DVI); HIGH_ACT bits 5/6 = 0 | 03 H19-H20, 05 #3 |
| C21 | [1] RAM Disk (ramdisk.cc:25-41) | f_mkfs + memcpy into 0x02000000-0x02FFFFFF; `get_fattime` reads 0x10060400 | Plain RAM there. RTC = epoch seconds (0 only gives 1970 timestamps) | 07 H12-H13 |
| C22 | [11] SoftIEC Drive → `new IecInterface` (iec_interface.cc:10,20-21) | If HARDWARE_IEC = 0 the ctor returns early and leaves `slaves[]`, `available_slots`, `*_loc` uninitialised; `IecDrive` still calls `register_slave` + `configure` (iec_drive.cc:171-172) → UB (wild virtual call and writes) | Capability bit 5 = 1 (§3 C4). Then R 0x10028000, code load, W 0x100287FF (accept). IEC Server polls 0x10028002 every 2 ticks → 0x01; 0x10028001 → 0x01 | 11 H1, H10-H12 |
| C23 | [20] Network Config (network_config.cc:44-48; product.cc:185-190) | `get_flash()` + `read_serial` (0x4B) → hostname/MAC | 0x4B + 4 dummy + 8 stable UID bytes (00/FF only makes all instances identical, no hang) | 06 H10 |
| C24 | [51] RMII Interface (only with CAPAB_ETH_RMII) | MDIO reg2 == 0x0022 at PHY 0/3 (fallback only prints); link poll reg1 bit2 every 250 ms; RX IRQ bit5 level = `used_valid`; TX_BUSY | T0: bit clear, or GET_MDIO=0. T1: PHY model, level IRQ, TX_BUSY=0 | 08 H1-H10 |
| C25 | [52] WiFi Application (wifi.cc:41-51 → wifi_cmd.cc:32-44) | EnableIRQ 0x85 → buf_irq pending → ISR `while ((status & 7) != 0)` with no ack register; flowctrl read-modify-write; IDENTIFY retried every 0.5 s; blocking RPCs; TX buffer starvation hangs the UI when the WiFi row opens | Live status bits (H1), HIGH_ACT bit3 = line & EN (H2), flowctrl b7 reads 0 (H3), TX completion (H6), stub u64ctrl answers every command except 0x08 within 100 ticks (H4-H9) | 04 H1-H11 |
| C26 | [60] Tape Playback (if C2N_STREAMER) | status poll | 0x100A0000 = 0x80 | 11 H15 |
| C27 | [61] LED Strip, [61] Tape Recording (if C2N_RECORDER, enables ITU 0x08) | record IRQ level | Line low unless enabled and ≥ 512 bytes | 11 H8 |
| C28 | [65] C1541/71/81 Init (if DRIVE_1541_1/2) | W/R 0x1002000D readback (multi-mode). WD177x `while (R 0x10021806 & 0x80) W …=1` → **hang on 0xFF**. HIGH_ACT bits 1/2. POWER readback. DIRTY must not be RAM (`wait_for_writeback` never exits). ROM missing on /flash → drive powered off | 0x10021806 = 0x00. DIRTY reads 0x00. HIGH_ACT bits 1/2 = 0. RAZ/WI satisfies all of these | 11 H2-H5, H9, H19 |
| C29 | [70] Data Streamer, [98] REU Preloader (DDR 0x01000000), [100] Telnet, [101] FTP, [102] Socket 64, [103] HTTP, [105] Modem (ACIA deinit: HIGH_EN RMW) | no poll hazards | HIGH_EN readback | 11, 08 |
| C30 | ultimate.cc:100-107 `C64::init` (CARTRIDGE) | `set_emulation_flags` reads 0x10040000-0F; ROM window writes; `init_cartridge` reads C64_STOP and cart detect; KILL ← 2 ×2; STOP ← 0 | Latches with readback (doc 10 H16) | 10 |
| C31 | ultimate.cc:109 → usb_base.cc:185-208,317-350 | If bit23: blob load 711 × W16, R32 0x10080000, NANO_START=1. The ISR drains HEAD/TAIL until two reads match | T0: never raise ITU bit 2; HEAD/TAIL stable (0/0) | 09 H3-H6 |
| C32 | ultimate.cc:122-127; overlay.h:98-102,154-164 | Overlay ctor writes; `release_ownership` R 0x10181000/01 → W 0x10181010/11 | Accept; PLD RAM | 05 |
| C33 | userinterface.cc:625-643; screen.cc:90-201 | Overlay screen/colour RAM is filled and **read back** (cursor XOR, backup/restore, scroll) | 0x10141000 / 0x10142000 = 4 K R/W RAM | 05 #12 |
| C34 | ultimate.cc:166-207; c64.cc:1497-1505; userinterface.cc:311-314,362-381 | Main loop every 15 ms: button edge detection with `button_prev`=0; a held bit6 at boot → swapDisk and no further edges | 0x1000000A idle 0x00. Press = bit6 high for 50-200 ms | 02 H12, 05 #6 |
| C35 | ultimate.cc:183-187; userinterface.cc:123 | Overlay UI only if HPD bit2 = 1 **and** `CFG_USERIF_ITYPE == 1` (default 0 = Freeze). Freeze path: `C64::exists` (CLOCK_DETECT bit0) → `stop(true)` → unbounded C64_STOP poll | HPD 0x04. Seed ITYPE=1 in the flash config page (id 0x47454E2E), or keep C64_STOP instant | 05 #3-#5 |
| C36 | keyboard_c64.cc:196-273 (overlay open, every 20 ms) | JOY idle; ROW stable-read loop **no timeout**; ROW idle | 0x10100406 = 0xFF. 0x1010040B a pure function of the COL latch + key state, idle 0xFF | 05 #8-#10 |

### Runtime-only hazards (not on the boot path)

| Area | Hazards |
|---|---|
| USB device attach | 09 H7-H14, e.g. `bulk_in` infinite loop while holding the mutex, hub assert on > 4 bytes, root device must be HS, read at 0x0 |
| SD insert | 07 H3-H11 (CMD0/8/41/58 responses; 240 k / 600 k polls with IRQs off) |
| WiFi row / power off | 04 H5-H6 |
| Disk swap | 11 H4 |
| C64 reset | `while(ioRead8(ITU_TIMER))` (10 H14) |
| DMA load / boot cart | 10 H18-H19 |
| Socket / REST drive commands | NULL `c1541_A` when DRIVE_1541_1 = 0 (socket_dma.cc:204, c64_subsys.cc:415) |
| Tape flush | 11 H16 |

---

## Interrupts (consolidated)

**CPU line:** one level input → `mip.MEIP` → trap with mcause 0x8000000B when `mie.MEIE & mstatus.MIE` (csr.vhd:93-96).
- ITU output = `irq_en & ((active & mask) ≠ 0 | (src_high & mask_high) ≠ 0)` (itu.vhd:245-252).
- There is no CLINT and no FIRQ.

**Low byte (EN 0x10000001, DIS 0x10000002, CLEAR 0x10000004, ACTIVE 0x10000005).** Edge mask per §3 C2 = 0x85.

| Bit | Source | Edge / level | Enabled by | Ack |
|---|---|---|---|---|
| 0 | periodic timer, 200 Hz | edge | riscv_main.c:185 | CLEAR |
| 1 | UART | level (never driven) | never | — |
| 2 | USB nano FIFO push | **edge** (1-cycle pulse) | usb_base.cc:344-345 | CLEAR + TAIL write |
| 3 | tape record FIFO ≥ 512 | level | tape_recorder.cc:47 | drain FIFO |
| 4 | UCI handshake & ~mask | level | command_intf.cc:118 | mask set via 0x10044004 |
| 5 | RMII RX `used_valid` | level | rmii_interface.cc:108 | POP 0x1006082F |
| 6 | RMII TX done | level | never (keep masked) | 0x10060819 |
| 7 | C64 reset | edge (OPEN) | command_intf.cc:101-102 | CLEAR |

**High byte (EN 0x10000027, ACT 0x10000028).** Level, no ITU clear register; an active bit with no handler gets its EN bit cleared (riscv_main.c:118-129); bit 7 is never serviced.

| Bit | Source | Installed by | Source-side ack |
|---|---|---|---|
| 0 | ACIA | acia.cc:67 | irq_source / tx_tail |
| 1 | WD177x A | wd177x.cc:63 | 0x10021806 pop |
| 2 | WD177x B | wd177x.cc:63 | 0x10025806 pop |
| 3 | WiFi DMA UART | esp32.cc:73 | rx_pop / tx_push / rx_addr / ictrl |
| 4 | Blingboard | none | — |
| 5 | HDMI HPD | u64_config.cc:971 | W 0x10100400 ← 0x08 |
| 6 | U64 unlock | u64_config.cc:974 | handler pokes $D038 |
| 7 | GURU | never serviced | — |

---

## Emulator model tiers (cross-block summary)

**T0 (boot to the running overlay-UI loop)**
- CPU per doc 01 (RV32I + MUL group + Zicsr, rvlite trap semantics).
- ELF load; 64 MB RAM at 0.
- IO default RAZ/WI with LE byte decomposition, never faulting.
- ITU per doc 02 T0; capabilities 0x34000222; UART FLAGS 0x40.
- Constant reads:

| Register | Value |
|---|---|
| I2C status 0x10100701 | 0x00 |
| BOARDREV 0x1010000C | 0xB8 |
| HDMI_REG 0x10100400 | 0x04 |
| RESTORE 0x10100402 | 0x00 |
| CART_DETECT 0x10100403 | 0x03 |
| KEYB_JOY 0x10100406 | 0xFF |
| KEYB_ROW 0x1010040B | 0xFF |
| BLACKBOARD 0x10100407 | 0x01 |

- C64 cart regs: latches, STOP instant, CLOCK_DETECT 0x01 (bit4 = 0; bits 2/3 = 0, and their only reader `C64::get_exrom_game` has no caller, c64.h:323-325).
- DMA window: 64 K array; $D012 = 0xFF (or a counter), $DC00/$DC01 = 0xFF.
- Overlay regs, 4 K screen RAM, 4 K colour RAM, HDMI palette RAM.
- ROM windows as RAM.
- SPI flash T0: JEDEC 01 60 18, SR1=0, UID.
- WiFi DMA UART with the stub u64ctrl (doc 04 T0).
- IEC R1/R2 = 0x01.
- Everything else RAZ/WI.

**T1** = the T1 columns of §1: I2C devices, HPD/EDID, flash NOR + FAT image, SD card, RMII + host bridge, USB HLE nano, virtual u64ctrl, overlay renderer + matrix keyboard, C64/drive hooks.

---

## 3. Cross-doc contradictions (resolved against firmware / ELF)

| # | Contradiction | Resolution |
|---|---|---|
| C1 | **CPU core.** README.md:7 says "RV32IM, neorv32", and doc 02 defers to it. Doc 01 says rvlite | **rvlite.** Boot ROM generated into `fpga/cpu_unit/rvlite/.../bootrom_u64ii_pkg.vhd` (bootloader/Makefile:11). The ultimate Makefile has no `neorv32` reference; the only neorv32 VPATH is in the bootloader (:21), shadowed by portable crt0. ELF: 0 DIV/REM instructions, 337 MUL-family (rvlite has no divider, Makefile:8-9 `-mno-div`), only standard M-mode CSRs (A1), 1 `wfi` (0x30150, crt0), 2 `mret` (0x301CC, 0x33874), no `ebreak`, no compressed code. The interrupt model (single MEI line from the ITU) is the same either way. README should be corrected |
| C2 | **ITU edge mask, bit 7.** Doc 11 says low bits 0x08/0x10/0x80 are level (`itu.vhd:16` default `"00000001"`). Docs 01/02/08/09 use 0x85 (bits 0, 2, 7 edge) | The generic default is overridden to `"10000101"` / `g_edge_write=false` in every open top: ultimate_logic_32.vhd:507-508, ecp5_tester.vhd:443-444, basic_io.vhd:225. The firmware clears bits 2 and 7 before enabling them (usb_base.cc:344, command_intf.cc:101), which implies latched edges. **Use 0x85.** Bits 3/4/5/6 level. The U64-II value is still OPEN (Q-B2) |
| C3 | **BOARDREV timing.** Doc 01 H17 puts BOARDREV reads (u64ii_init.cc:152,157,164-166) pre-scheduler inside `custom_hardware_init`. Doc 03 places them in U64Config | **Doc 03 is correct.** `custom_hardware_init` (u64ii_init.cc:114-138) has no BOARDREV read. `SetVideoPll` (:141-160) and `SetExternalPLL` (:162-171) read it and take `i2c_lock` (needs RTOS). The first BOARDREV read is `isEliteBoard` in the `U64SidSockets` ctor (u64_config.cc:531), inside InitFunction "U64 Config" |
| C4 | **Minimum capability word.** Doc 01: ≥ 0x04000200. Doc 02 T0: 0x34000200 "only bits 26, 9 + type". Doc 11 H1: must include 0x02\|0x04\|0x20\|0x80\|0x100\|0x200\|0x800\|0x40000\|0x08000000. Doc 12: bit21 = 0 | InitFunctions "SoftIEC Drive" and "Printer" are unconditional (iec_drive.cc:34-38, iec_printer.cc:108-118). `IecInterface` is heap-allocated (iec_interface.cc:10, heap_4 does not zero) and returns early without HARDWARE_IEC (:20-21), yet `register_slave` + `configure` run (iec_drive.cc:171-172; iec_interface.cc:51-61,128-145) → UB. **Bit 5 is required at boot.** Missing DRIVE_1541_1 only crashes runtime paths (socket_dma.cc:204, c64_subsys.cc:415); the other doc-11 bits are optional. **T0 word = 0x34000222** (ULTIMATE64 \| type 3 \| CARTRIDGE \| HARDWARE_IEC \| DRIVE_1541_1). RAZ/WI already satisfies the drive/IEC T0 needs, except IEC R1/R2 = 0x01, which avoids a busy loop. Keep bits 18/21/23/24/27 off until their block is modelled. Type 2 vs 3 must match the flash image (OPEN Q-B1) |
| C5 | Doc 01 H17 / OQ9 list the I2C busy protocol as OPEN | Resolved by doc 03: status 0x10100701 bit7 BUSY, bit2 NACK, read clears bits 2/3 (i2c_master.vhd:302-313) |
| C6 | Doc 01 OQ5: does riscv_main.c:171 load the handler word into mtvec? | **Yes.** ELF 0x3DFC4: `lui a5,0x33; lw a5,1792(a5); csrw mtvec,a5`. The word at 0x33700 is 0xF8810113 (`addi sp,sp,-120`), so mtvec becomes 0xF8810110 under rvlite masking. Overwritten by port_asm.S:309-310 before MIE is set. Harmless if mtvec write is accept-and-mask |
| C7 | InitFunction ties have "no defined order" (doc 01; doc 03 Q10): "U64 Config" vs "RAM Disk" (1), "SID Cart" vs "Boot Cart" (0), "Tape Recording" vs "LED Strip" (61) | The comb sort is unstable but deterministic for a given append order. Append order comes from `.init_array` (ELF call sites; HTTP Daemon via `new` in `httpd` ctor, httpd.cc:15-26). Simulating `IndexedList::sort` (indexed_list.h:191-216) with `compare` (init_function.cc:40-51) gives: **SID Cart, Boot Cart, U64 Config, RAM Disk**, U64 Palette, SoftIEC Drive, Printer, UltiCopy, Network Config, A64 FS, FTP FS, LwIP, RMII, WiFi, Tape Playback, **LED Strip, Tape Recording**, C1541, Data Streamer, REU Preloader, Telnet, FTP Daemon, Raw Socket 64, HTTP Daemon, Modem. The result does not change when HTTP Daemon is appended last. So `ConfigManager` is first created by U64Config's member `U64Mixer` (u64_config.cc:478-480; members declared mixercfg, speakercfg, sockets, … u64_config.h:94-100). Those member ctors, and therefore flash probing, run **even without CAPAB_ULTIMATE64** (the gate is in the body, :904). Valid for this link order only |
| C8 | Doc 06 OQ5: flash tester order is inferred | Confirmed from `.init_array`: #1 `w25q_flash`, #2 `s25fl_flash`, #3 `s25flxxxl_flash` |
| C9 | Doc 11 says the constructor order in `__init_array` is undetermined | Resolved (Phase A): the IO-touching ctors run in the order #30 usb2, #43 cmd_if, #54 Acia, #72 Esp32, #78 softIecTarget (no HW). Capability reads therefore happen before `main`, so the capability value must be valid from reset |
| C10 | 0x10100400 T0 value: doc 03 T0 says 0x00 (boots), doc 05 T0 says 0x04 | Both boot. Choose **0x04** for T0: the overlay UI is the goal, and the EDID NACK is harmless (u64_config.cc:2594-2605) |
| C11 | DMA reads at T0: doc 03 says $D012 and SID/CIA reads = 0xFF. Doc 10 says $D012 per-read counter, $D400-$D7FF = 0 | Re-read u64_config.cc:2236-2240, 2355-2377, 576-640: every SID probe returns "none" for 0x00, 0xFF or RAM-like readback (`buffer[17]==2` needed, IDs 1D F5 / 'S''W' / 'N''O'). $D012 only needs to hit 0xFF. **Both are valid.** CIA $DC01 must be stable 0xFF and $DC00 must return 0xFF after 0xFF is written (constant 0xFF satisfies both) |
| C12 | ITU_TIMER "instant 0 is fine" (doc 01/02 T0) vs "instant 0 exhausts the stop budgets" (doc 02 H3, doc 10 H3) | Consistent if C64_STOP bit1 is instant. The budgets only fall through to the forced-stop poll (c64.cc:490-497), which then succeeds at once. A real countdown is preferred for T1 timing |
| C13 | Unmapped-read value: doc 07 H1 notes 0xFF causes "SD Error!"; doc 01 H16 says reads return 0 | Use **0** as the global default. 0xFF breaks SD (CD=1), I2C (BUSY), UART (TxFull), WD177x, UCI and more |
| C14 | Minor T0 constant differences: 0x10100406 0x1F (doc 03) vs 0xFF (doc 05); BLACKBOARD 0x01 (doc 03) vs 0x00/0x01 (doc 10) | Equivalent. Choose 0xFF and 0x01 |
| C15 | Docs 07 H13 and 09 H1 describe "bus fault → crash" | Those describe a faulting emulator. Hardware has no bus errors (doc 01 H16). Never fault |
| C16 | Doc 04 cites `wishbone2memio.vhd` for multi-byte IO; the others cite `rvlite/bus_converter.vhd` | Same semantics (sequential bytes, ascending address, LSB first). Per C1, the rvlite bridge is the relevant one |
| C17 | Doc 01 notes `recovery/u64ii/ultimate.bin` has almost no M-extension words | Irrelevant: the locally built ELF (above) is authoritative for instruction coverage |

### Firmware defects the emulator must tolerate (not to be "fixed")

| Defect | Where | Doc |
|---|---|---|
| mtvec written with an instruction word | riscv_main.c:171 | C6 |
| NULL `dev->parent` read at 0x0 for a root USB device | usb_base.cc:411 | 09 H9 |
| Uninitialised `IecInterface` without HARDWARE_IEC | C4 | — |
| Uninitialised `get_max_lun` return | usb_scsi.cc:75-88 | 09 H11 |
| `hdmiMonitor` uninitialised at the first 0x10100408 write | u64_config.cc:1092 | 03 Q11 |
| `mdns_resp_add_netif` without `mdns_resp_init`, sharing DHCP client-data slot 0 | network_interface.cc:284-285 | 08 Q1 |
| Stray write 0x100287FF | iec_interface.cc:121-126 | 11 H10 |
| 0xFF written to missing-config registers | 0x1010000C, 0x1004000A | 10 Q14 |
| side-1 DIRTY index quirk | c1541.cc:498 | 11 Q9 |
| `sbrk` double increment | riscv_main.c:224-225 | 01 |

---

## Open questions (deduplicated)

### A. Closed U64-II top level: decode and wiring

1. **Q-A1: CPU instance.** Is the CPU rvlite (`g_mult`, `g_start_addr=0x80000000`) with ITU `irq_out` as the only interrupt? Are ITU `irq_in[7:2]` / `irq_high[7:0]` wired as in ultimate_logic_32.vhd:523-536? (01 Q1; 02 Q3; 04 Q4; 11 Q3)
2. **Q-A2: Open IP identity.** Do the U64-II blocks match the open IP (`spi_peripheral_io` for SD and flash, `uart_dma` + generics `g_events`/`g_divisor`, `ethernet_rmii`/`free_queue`, `usb_host_nano` with the bit-11 split, `gcr_codec`, `iec_processor`, `command_protocol`, `acia6551`, `c2n_*`, sampler `g_sampler`, `audio_select`, `char_generator_peripheral_12`)? (04 Q1; 05 Q1; 06 Q2; 07 Q1; 08 Q2; 09 Q1; 11 Q2; 12 Q1-Q2)
3. **Q-A3: Physical presence.** Does the U64-II have an SD slot, with which CD/WP polarity? Is the RTC timer present, and what is its reset value / battery backing? (07 Q1, Q2, Q4)
4. **Q-A4: Aliasing.** DDR above 0x03FFFFFF, IO beyond 0x10FFFFFF, and mirrors inside each window (ITU every 0x40, flash every 0x10, drive windows, USB 0x10080800-0x10080FFF, U64_IO page, I2C page, MMCM page). Does the U64-II CPU bridge also split 16/32-bit IO LE-sequentially? (01 Q3; 02 Q5; 03 Q1, Q14; 04 Q6; 06 Q2; 07 Q3; 08 Q3; 09 Q1; 11 Q4)
5. **Q-A5: Clock.** Is the sys clock really 100 MHz? `CLOCK_FREQ` only sets the tick reload and ITU units. (01 Q4)
6. **Q-A6: DMA width.** DMA address width and identity mapping for USB, sampler and WiFi (26 bits assumed). (09 Q3; 12 Q3)

### B. Values the firmware reads that are not derivable from source

1. **Q-B1: Capabilities.** The exact capability word (feature bits and FPGA type 2 vs 3) on U64E-II and on C64 Ultimate. The boot log line `*** FPGA Capabilities: %8x ***` (ultimate.cc:90) settles it. (01 Q2; 02 Q1; 03 Q7, Q9; 06 Q3; 09 Q6; 11 Q1; 12 Q1)
2. **Q-B2: ITU generics.** `g_edge_init`/`g_edge_write` (0x85 assumed, §3 C2), `g_version` (0x25 assumed, the ultimate_logic_32.vhd:13 default), UART generics. (01 Q2; 02 Q2, Q4, Q8; 09 Q2; 11 Q3)
3. **Q-B3: Board revision.** BOARDREV values on shipped boards, C64U vs U64E-II. (03 Q7, Q8)
4. **Q-B4: C64 core.** `C64_CORE_VERSION`; power-on state of C64_MODE reset / C64_STOP; CLOCK_DETECT bits 1/2/3/5. (10 Q1, Q6, Q13)
5. **Q-B5: Flash part.** S25FL128L (24 config pages at 0xFE8000) or W25Q128 (16 at 0xFF0000)? This decides which `Flash` subclass answers `get_flash()` and hence the UID/MAC source. (06 Q1; 08 Q6)
6. **Q-B6: PHY.** Part number, reset/clock source. (08 Q4)
7. **Q-B7: USB hardware.** ULPI PHY identity; USB2513 port wiring / on-board devices. (09 Q5, Q7)

### C. Semantics only partly derivable

1. **Q-C1: ITU misc.**
   - 0x1000001F as a real marker register (02 Q6).
   - `misc_io`, `buttons[2:0]`, `btn_menu` wiring (02 Q7).
   - Can the FPGA raise ITU_BUTTON1 from F11, a keyboard combo or the case switch? (05 Q7)
2. **Q-C2: HDMI / overlay.**
   - HPD high-IRQ 5 raise condition and HPD_WASLOW (03 Q3).
   - Overlay pixel palette path, `pixel_opaque`, X_ON/Y_ON origin, big-font height 23 vs 24, `own_keyboard` effect (05 Q2-Q6).
   - Whether `poll_inactive` redraws while hidden (05 Q10).
3. **Q-C3: Keyboard.**
   - Keyboard scanner behind `scan_enable`, and how its results reach KEYB_ROW/JOY (03 Q2; 05 Q6).
   - Blingboard RX semantics and LEDs (03 Q4; 05 Q8).
   - BLACKBOARD meaning (03 Q6).
4. **Q-C4: Board peripherals.**
   - PLL and expander part numbers (03 Q5).
   - LED-strip mapping arithmetic (03 Q12).
   - INT_CONNECTORS / USERPORT_EN bits (03 Q13).
   - MMCM DRP byte addressing (03 Q14).
   - Default system mode NTSC on an empty config (03 Q15).
5. **Q-C5: ESP32.**
   - flowctrl bits 4-6 meaning (04 Q3).
   - Path of front-panel button events (04 Q2).
   - `DEVELOPER` value (04 Q5).
6. **Q-C6: SPI flash.**
   - ICAP trigger on 7-series / reset mapping (06 Q4).
   - S25FL128L response to the single `66 99` frame (06 Q6).
   - Hidden second flash / selector (06 Q7).
7. **Q-C7: SD / RTC.**
   - `sntp_time_received` linkage (07 Q5).
   - SPI divider semantics (07 Q6, irrelevant).
8. **Q-C8: Network.**
   - Does the `mdns_resp_init` omission break DHCP in practice? (08 Q1)
   - UDP stream generator mux into RMII TX and its payload format (08 Q5; 10 Q5).
9. **Q-C9: USB.**
   - U2PIO 0x1010000F bit7/bit6 buffer meaning (09 Q4).
   - Nano return-stack depth (09 Q8, LLE only).
10. **Q-C10: C64 machine.**
    - DMA to $0000/$0001: port or RAM? (10 Q2)
    - ITU bit 7 trigger (10 Q3; 11 Q7).
    - Unlock high-IRQ 6 condition and clear (10 Q4).
    - Resampler 32-bit latching and LABOR (10 Q7).
    - CARTRIDGE_KILL bit1 / cart_active (10 Q8).
    - VOICE_ADSR layout (10 Q9).
    - ROM window hardware readability; the FW does read them (10 Q10, §1c M8).
    - SID decode formula (10 Q11).
    - DMA while the C64 runs (10 Q12).
    - Effect of the 0xFF writes (10 Q14).
11. **Q-C11: Drives / IEC / tape.**
    - DRIVETYPE multi-mode per drive (11 Q1).
    - RAMMAP bits 6:1 (11 Q5).
    - IEC version / UP_FIFO_COUNT (11 Q6).
    - Tape tick and phi2 source with an external C64 (11 Q8).
12. **Q-C12: Sampler.** module.bin register usage is from an opcode scan only (12 Q4). `liblwip.a` was not scanned for MMIO (12 Q5).

### D. Emulator design questions

1. **Q-D1: Time mapping.** Instruction→emulated-time mapping, idle detection (no WFI, no MTIME), and whether busy-wait fast-forward is acceptable given tick collapse. (01 Q10; 02 §Tick collapse)
2. **Q-D2: Overlay UI config.** Cleanest way to get `CFG_USERIF_ITYPE=1`: pre-seeded flash config page vs REST config route. (05 Q9)
3. **Q-D3: Boot ROM.** Only relevant if the boot ROM is run instead of loading the ELF: its register state and the `.app` size limit (`length < 0x180000`). (01 Q6, Q7)
4. **Q-D4: Unused regions.** Owner of `__updater_start` 0x03000000-0x03BFFFFF (no compiled user). (01 Q8)

### Resolved in this doc (removed from the open list)

| Question | Resolved by |
|---|---|
| 01 Q5 (mtvec value / CSR and opcode set) | §3 C6, C1 |
| 01 Q9 (I2C polls) | §3 C5 |
| 01 H17 BOARDREV timing | §3 C3 |
| 03 Q10 (InitFunction tie order) | §3 C7 |
| 06 Q5 (tester order) | §3 C8 |
| 11 constructor order | §3 C9 |
| 02 / 11 edge-mask disagreement | §3 C2; hardware value still Q-B2 |
| Minimum capability word disagreement | §3 C4 |
