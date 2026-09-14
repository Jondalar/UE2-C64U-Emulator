# ITU (Interrupt / Timer / UART) and console UART

Scope: the ITU block at `ITU_BASE = IOBASE + 0x00000 = 0x10000000` (`software/system/iomap.h:10`, `-DIOBASE=0x10000000` in `target/u64ii/riscv/ultimate/Makefile` OPTIONS line). It covers the IRQ controller, 8-bit µs timer, periodic IRQ timer, button register, FPGA version/capabilities, ms timer, busy LEDs, misc_io, the high-IRQ controller and the embedded console UART.

Paths are relative to `firmware/1541ultimate/` unless marked `ELF`. `ELF @0x…` refers to the locally built `target/u64ii/riscv/ultimate/result/ultimate.elf`, disassembled with `riscv64-elf-objdump`. Its addresses are valid for that build only.

## Sources read

| File | Key content |
|---|---|
| `software/system/itu.h` | register macros, IRQ bit names, CAPAB_* bits, UART flag bits |
| `software/system/itu.c` | `getFpgaCapabilities`, `getFpgaVersion`, `getFpgaType`, `wait_ms`, `wait_10us`, `getMsTimer`, `getButtons`, `uart_*`, `outbyte`, `custom_outbyte` |
| `software/system/iomap.h` | `ITU_BASE`, `ioRead8`/`ioWrite8` are plain volatile byte accesses (`:38-39`) |
| `software/portable/riscv/riscv_main.c` | `install_high_irq`, `deinstall_high_irq`, `freertos_risc_v_application_interrupt_handler`, `main`, `vPortSetupTimerInterrupt`, `C_exception_handler` |
| `software/portable/riscv/crt0.S` | reset path, early dummy trap handler |
| `software/FreeRTOS/Source/portable/risc-v/port.c`, `port_asm.S`, `portmacro.h`, `chip_specific_extensions/RV32I_CLINT_no_extensions/freertos_risc_v_chip_specific_extensions.h` | trap dispatch, MIE handling |
| `software/FreeRTOS/Source/FreeRTOSConfig.h` | tick rate |
| `software/system/small_printf.cc` | `printf`, `puts`, `putchar`, `_diag_write_char` |
| `software/io/stream/stream_uart.cc/.h` | `Stream_UART` (compiled, not linked, see Console) |
| `software/filemanager/ult_syscalls.cc` | newlib `_write` |
| `software/application/ultimate/ultimate.cc` | `ultimate_main`, capability gating, `custom_outbyte` hook |
| `software/system/product.cc` | version string (FPGA version not used for U64) |
| Call sites | `io/c64/c64.cc`, `io/overlay/overlay.h`, `userinterface/userinterface.cc`, `io/usb/usb_base.cc`, `io/usb/usb_scsi.cc`, `io/flash/w25q_flash.cc`, `io/tape/tape_recorder.cc`, `io/command_interface/command_intf.cc`, `io/network/rmii_interface.cc`, `io/acia/acia.cc`, `drive/wd177x.cc`, `drive/c1541.cc`, `io/uart/dma_uart.cc/.h`, `io/wifi/wifi_cmd.cc`, `u64/u64_config.cc`, `io/printer/mps_printer.cc`, `io/stream/keyboard_vt100.cc`, `io/usb/keyboard_usb.cc`, `monitor/machine_monitor.cc`, `filetypes/filetype_u2p.cc`, `application/update_u2p/update_u64ii.cc` (not linked; only used for the FPGA-type id table) |
| `fpga/io/itu/vhdl_source/itu.vhd`, `itu_pkg.vhd` | open ITU RTL |
| `fpga/io/uart_lite/vhdl_source/uart_peripheral_io.vhd` | UART register RTL |
| `fpga/ip/busses/vhdl_source/io_bus_splitter.vhd` | ITU sub-decoder |
| `fpga/ip/clock/vhdl_source/fractional_div.vhd` | tick_1us / tick_1ms generation |
| `fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd` | U2+-family ITU instantiation. It is **not** the U64-II top (closed), but it is the only open reference for generics and IRQ wiring. |
| `fpga/cpu_unit/rvlite/vhdl_source/csr.vhd`, `core.vhd` | how the external IRQ pin reaches mip/mcause |

Build check: the full ELF disassembly was scanned for `lui rX,0x10000` + byte load/store with offset < 0x40. This lists every ITU register the linked firmware touches directly (table below). Access through computed pointers would not show up.

## Address map

Decode inside the ITU (`itu.vhd:277-293`, `io_bus_splitter.vhd:36,51-53`): `addr[5:4]` selects the sub-block: 0 = IRQ/timer, 1 = UART, 2 = ms/LED/high-IRQ, 3 = dummy ack that reads 0. Inside a sub-block only `addr[3:0]` is decoded (the UART only `addr[1:0]`, `uart_peripheral_io.vhd:170,219`). Unlisted reads return 0 (`c_io_resp_init`) and unlisted writes are ignored.

| Abs addr | W | R/W | Name (macro, file:line) | Meaning (RTL) | FW writes | FW reads (ELF scan) |
|---|---|---|---|---|---|---|
| 0x10000000 | 8 | R/W | ITU_IRQ_GLOBAL `itu.h:11` | bit0 = irq_en, reset **1** (`itu.vhd:134-135,165-166,256`) | 1 at `riscv_main.c:186`; 0 at `filetype_u2p.cc:97` (`jump_run`) | none |
| 0x10000001 | 8 | W (R=mask) | ITU_IRQ_ENABLE `itu.h:12` | mask \|= data; read = mask (`itu.vhd:136-137,167-168`), reset 0 | 0x01 `riscv_main.c:185`; 0x04 `usb_base.cc:345`; 0x08 `tape_recorder.cc:47`; 0x10 `command_intf.cc:118`; 0x20 `rmii_interface.cc:108`; 0x80 `command_intf.cc:102` | none |
| 0x10000002 | 8 | W | ITU_IRQ_DISABLE `itu.h:13` | mask &= ~data (`itu.vhd:138-139`) | 0xFF `riscv_main.c:180`; 0x04 `usb_base.cc:43,201,212`; 0x08 `tape_recorder.cc:53`; 0x10 `command_intf.cc:70`; 0x20 `rmii_interface.cc:114` | none |
| 0x10000003 | 8 | R/(W) | ITU_IRQ_EDGE `itu.h:14` | edge-select per bit. Writable only if g_edge_write (`itu.vhd:140-143`). U2+ top: init `"10000101"`=0x85, write disabled (`ultimate_logic_32.vhd:507-508`) | never | never |
| 0x10000004 | 8 | W | ITU_IRQ_CLEAR `itu.h:15` | edge_flag &= ~data (`itu.vhd:144-145`) | 0xFF `riscv_main.c:181`; `pending` `riscv_main.c:87`; 0x04 `usb_base.cc:202,344`; 0x80 `command_intf.cc:101` | none |
| 0x10000005 | 8 | R | ITU_IRQ_ACTIVE `itu.h:16` | (edge_flag \| (src & ~edge)) & mask (`itu.vhd:171-172,275`) | none | ISR `riscv_main.c:86` |
| 0x10000006 | 8 | R/W | ITU_TIMER `itu.h:17` | 8-bit down-counter, −1 per 5 µs, sticks at 0 (`itu.vhd:92-105,146-147,173-174`) | `wait_ms` 200 (`itu.c:67`), `wait_10us` 2m+1 (`itu.c:75`), `c64.cc:380,431,453,468,480,550,561` 200, `c64.cc:619` 20 | same functions poll it |
| 0x10000007 | 8 | R/W | ITU_IRQ_TIMER_EN `itu.h:18` | bit0 en, bit1 select. On write with old en=0: cnt := val<<8 \| 0xFF (`itu.vhd:148-153,175-177`) | 0 then 1 `riscv_main.c:179,184` | none |
| 0x10000008 | 8 | W (R=cnt[15:8]) | ITU_IRQ_TIMER_HI `itu.h:19` | reload val[15:8]; read returns the **counter**, not val (`itu.vhd:156-157,180-181`). Reset val = 0x8000 (`:264`) | 0x07 `riscv_main.c:182` | none |
| 0x10000009 | 8 | W (R=cnt[7:0]) | ITU_IRQ_TIMER_LO `itu.h:20` | reload val[7:0] (`itu.vhd:154-155,178-179`) | 0xA0 `riscv_main.c:183` | none |
| 0x1000000A | 8 | R | ITU_BUTTON_REG `itu.h:21` | `buttons[2:0] & "00000"`; bit6 forced 1 when btn_menu (`itu.vhd:192-196`). BUTTON0=0x20, BUTTON1=0x40, BUTTON2=0x80 (`itu.h:93-96`) | none | `C64::checkButton` `c64.cc:1501`, `Overlay::checkButton` `overlay.h:114`, `UserInterface::buttonDownFor` `userinterface.cc:370` |
| 0x1000000B | 8 | R | ITU_FPGA_VERSION `itu.h:22` | g_version (`itu.vhd:182-183`). U64-II value unknown (Q4); the emulator returns 0x25, the `ultimate_logic_32.vhd:13` default | none | `getFpgaVersion` `itu.c:36` |
| 0x1000000C | 8 | R | CAPABILITIES_0 `itu.c:5` | cap[31:24] (`itu.vhd:184-185`) | none | `getFpgaCapabilities` `itu.c:23` |
| 0x1000000D | 8 | R | CAPABILITIES_1 `itu.c:6` | cap[23:16] | none | `itu.c:24` |
| 0x1000000E | 8 | R | CAPABILITIES_2 `itu.c:7` | cap[15:8] | none | `itu.c:25` |
| 0x1000000F | 8 | R | CAPABILITIES_3 `itu.c:8` | cap[7:0] | none | `itu.c:26` |
| 0x10000010 | 8 | R/W | UART_DATA `itu.h:105` | W: push TX FIFO. R: RX FIFO head (`uart_peripheral_io.vhd:171-177,220`) | `outbyte` `itu.c:290`; raw ISR debug writes (see Console) | **never read** |
| 0x10000011 | 8 | W | UART_GET `itu.h:106` | pop RX FIFO (`uart_peripheral_io.vhd:179-180`) | never (ELF) | — |
| 0x10000012 | 8 | R/W | UART_FLAGS `itu.h:107` | R: b0 overflow, b4 TxFifoFull (almost-full), b5 RxFifoFull, b6 TX done/idle, b7 RxDataAv. W: b0=1 clears overflow (`uart_peripheral_io.vhd:182-183,208-215`) | never | `outbyte` `itu.c:289` only |
| 0x10000013 | 8 | R/W | UART_ICTRL `itu.h:108` | imask[1:0], only if g_impl_irq. ITU does not set it, so the default is false (`itu.vhd:296-301`, `uart_peripheral_io.vhd:10,185-188`) and the register reads 0 | never | never |
| 0x10000014-1F | 8 | — | aliases of 0x10-0x13 (`addr[1:0]`) | 0x1000001F = ICTRL alias | 0x49 by `__crt0_dummy_trap_handler` (`crt0.S:307-309`) | — |
| 0x10000020-21 | 8 | — | unused in ms sub-block | reads 0 | — | — |
| 0x10000022 | 8 | R | ITU_MS_TIMER_HI `itu.h:24` | ms_timer[15:8] (`itu.vhd:225-226`) | — | `getMsTimer` `itu.c:84-85` |
| 0x10000023 | 8 | R | ITU_MS_TIMER_LO `itu.h:23` | ms_timer[7:0], +1 per tick_1ms, reset 0 (`itu.vhd:107-109,223-224,266`) | — | `getMsTimer` |
| 0x10000024 | 8 | W | ITU_USB_BUSY `itu.h:26` | bit0 → busy_led OR (`itu.vhd:206-207,327`) | `usb_scsi.cc:843,849,855,873,881,885` | — |
| 0x10000025 | 8 | W | ITU_SD_BUSY `itu.h:27` | bit0 → busy_led (`itu.vhd:208-209`) | no writer in ELF | — |
| 0x10000026 | 8 | W | ITU_MISC_IO `itu.h:28` | misc_io[7:0] out port, reset 0 (`itu.vhd:212-213,270`) | no writer in ELF | — |
| 0x10000027 | 8 | R/W | ITU_IRQ_HIGH_EN `itu.h:29` | imask_high, reset 0 (`itu.vhd:214-215,229-230,258`) | 0 `riscv_main.c:178`; read-modify-write `riscv_main.c:43,50,126`, `acia.cc:75`, `dma_uart.cc:73` | same sites |
| 0x10000028 | 8 | R | ITU_IRQ_HIGH_ACT `itu.h:30` | irq_high & imask_high, level, no latch (`itu.vhd:227-228`) | — | ISR `riscv_main.c:118` |
| 0x10000029 | 8 | W | ITU_PRINTER_BUSY `itu.h:31` | bit0 → busy_led (`itu.vhd:210-211`) | `mps_printer.cc:1807,1839` | — |
| 0x1000002A-2F | — | — | none | reads 0 | — | — |
| 0x10000030-3F | — | — | splitter port 3 | dummy ack, reads 0 (`io_bus_splitter.vhd:51-53`) | — | — |

Not linked into this ELF (absent from the symbol table): `itu_clear_irqs`, `getButtons`, `uart_read_buffer`, `uart_write_buffer`, `uart_get_byte`, `uart_data_available`, `uart_write_hex`, and all `Stream_UART` methods. Linked ITU helpers (ELF): `getFpgaCapabilities` 0x33AF8 (weak, no override), `getFpgaVersion` 0x33B28, `getFpgaType` 0x33B34, `wait_ms` 0x33B54, `wait_10us` 0x33B7C, `getMsTimer` 0x33BA0, `outbyte` 0x33BCC, `install_high_irq` 0x3DDC4, `deinstall_high_irq` 0x3DE04, `freertos_risc_v_application_interrupt_handler` 0x3DE44, `vPortSetupTimerInterrupt` 0x3DFC4, `C_exception_handler` 0x3E00C, `freertos_risc_v_trap_handler` 0x33700, `custom_outbyte` (.bss) 0x156AF0.

### Capability bits (`itu.h:49-81`), as used by this build

Reference RTL bit assignment: `ultimate_logic_32.vhd:285-320` (bit19 is `g_wifi_uart` there, `CAPAB_COPPER` in `itu.h:68`; bit20 is 0).

| Bit | Mask | Name | Readers in linked code | Effect |
|---|---|---|---|---|
| 0 | 0x00000001 | UART | none | — |
| 1 | 0x00000002 | DRIVE_1541_1 | `c1541.cc:1260`, `filetype_d64.cc:72`, `filetype_g64.cc:59` | drive A object + high IRQ 1 (`c1541.cc:115`, `wd177x.cc:63`) |
| 2 | 0x00000004 | DRIVE_1541_2 | `c1541.cc:1263`, `filetype_d64.cc:83`, `filetype_g64.cc:71` | drive B + high IRQ 2 |
| 3,4 | 0x18 | DRIVE_SOUND, HARDWARE_GCR | none | — |
| 5 | 0x00000020 | HARDWARE_IEC | `iec_interface.cc:20`, `iec_ulticopy.cc:18` | software IEC |
| 6 | 0x00000040 | BUS_MEASURE | `route_machine.cc:591` | REST feature |
| 7 | 0x00000080 | C2N_STREAMER | `tape_controller.cc:13,46`, `filetype_tap.cc:161` | tape playback |
| 8 | 0x00000100 | C2N_RECORDER | `tape_recorder.cc:18,45` | recorder task + low IRQ bit3 (`:47`) |
| 9 | 0x00000200 | CARTRIDGE | `ultimate.cc:100`, `command_intf.cc:44` | **C64 machine + main UI loop** (`ultimate.cc:100-107,166`) |
| 10,12-17 | — | RAM_EXPANSION, RTC_CHIP, RTC_TIMER, SPI_FLASH, ICAP, EXTENDED_REU, STEREO_SID | none | — |
| 11 | 0x00000800 | MM_DRIVE | `filetype_d64.cc:66`, `dos.cc:362` | non-1541 image types |
| 18 | 0x00040000 | COMMAND_INTF | `command_intf.cc:44`, `c64.cc:311,328,1306-1354` | UCI + low IRQ bits 4, 7 |
| 19,20 | — | COPPER / (unused) | none | — |
| 21 | 0x00200000 | SAMPLER | `c64.cc:319`, `filetype_reu.cc:42`, `route_runners.cc:239,270` | sampler |
| 22 | 0x00400000 | EEPROM | `c64_crt.cc:214,272,702,746`, `system_info.cc:87` | cart EEPROM |
| 23 | 0x00800000 | USB_HOST2 | `usb_base.cc:190` | USB stack + low IRQ bit2 |
| 24 | 0x01000000 | ETH_RMII | `rmii_interface.cc:50` | Ethernet + low IRQ bit5 |
| 25 | 0x02000000 | ULTIMATE2PLUS | `w25q_flash.cc:215` (only if bit26=0) | must be 0 |
| 26 | 0x04000000 | ULTIMATE64 | `c64.cc:205`, `u64_config.cc:904,1723`, `ultimate.cc:114`, `w25q_flash.cc:213`, `system_info.cc:175`, `route_input.cc:601` | **U64 configurator, ROMs, matrix keyboard, HDMI/unlock high IRQs** |
| 27 | 0x08000000 | ACIA | `acia.cc:10,34,76`, `modem.cc:924`, `system_info.cc:106` | ACIA + high IRQ 0 |
| 29:28 | 0x30000000 | FPGA_TYPE | `getFpgaType` → `w25q_flash.cc:81-83`, `filetype_u2p.cc:82` | ids: 0=5CEBA2, 1=5CEBA4, 2=XC7A50T, 3=XC7A100T (`update_u64ii.cc:109`). ≥3 selects the 100T flash map |
| 30 | 0x40000000 | BOOT_FPGA | none in ELF (bootloader only, `bootloader_u64ii.c:171`) | — |
| 31 | 0x80000000 | SIMULATION | none | — |

Flash layout selected by FPGA_TYPE (`w25q_flash.cc:51-63`):
- **50T (type ≠ 3):** BOOTFPGA 0x000000, APPL 0x220000, FLASHDRIVE 0x400000 (len 0xBE8000), CONFIG 0xFE8000.
- **100T (type ≥ 3):** BOOTFPGA 0x000000, APPL 0x3C0000, FLASHDRIVE 0x580000 (len 0xA68000), CONFIG 0xFE8000.

Suggested emulator profiles. Neither is a measured hardware value; see Open questions.
- **T0 minimum:** `0x34000200` = ULTIMATE64 + CARTRIDGE + type 3. Use `0x24000200` for type 2.
- **T1 full feature set:** `0x3DE40BE7` = bits 0,1,2,5,6,7,8,9,11,18,21,22,23,24,26,27 + type 3. Only enable a feature bit once the matching block is modelled.

`ITU_FPGA_VERSION` has no control-flow effect. It is only formatted into strings: `system_info.cc:174`, REST `/v1/info` `routes.cc:199`, `socket_dma.cc:595`. For U64 products the product string uses `C64_CORE_VERSION`, not this byte (`product.cc:139-144`).

## Init / boot sequence as seen from the bus

1. `_start` @0x00030000 (`linker.x:7`, ELF): mstatus.MIE cleared, mie/mip zeroed (`crt0.S:53,79-81`). No ITU access. On any trap in this phase, the dummy handler writes 0x49 to **0x1000001F** and returns (`crt0.S:280-315`).
2. .data copy, .bss clear, C++ constructors (`crt0.S:161-209`). Any printf here goes through `outbyte`: read 0x10000012, write 0x10000010.
3. `main` → `puts("-- Custom Hardware Init --")` (`riscv_main.c:154`) → UART.
4. `custom_hardware_init` (`u64ii_init.cc:114-138`) → `nau8822_init` (`nau8822.cc:13`) → printf + `wait_ms(2)` (`nau8822.cc:18`): write 200 to 0x10000006 and poll to 0, twice. **Interrupts are still off.**
5. `puts("-- Start Scheduler --")`, `xTaskCreate(ultimate_main)`, `vTaskStartScheduler` (`riscv_main.c:158-162`) → `xPortStartScheduler` (`port.c:151`) → `vPortSetupTimerInterrupt` (`riscv_main.c:169-187`). Write order, confirmed in ELF @0x3DFC4:
   `csrw mtvec` ; `[0x27]←0x00` ; `[0x07]←0x00` ; `[0x02]←0xFF` ; `[0x04]←0xFF` ; `[0x08]←0x07` ; `[0x09]←0xA0` ; `[0x07]←0x01` ; `[0x01]←0x01` ; `[0x00]←0x01`.
   Reload value: `(100000000>>8)/200 − 1 = 1952 = 0x07A0` (`riscv_main.c:173-176`).
6. `csrs mie, 0x800` (MEIE only; `port.c:192`). `xPortStartFirstTask` rewrites mtvec (`port_asm.S:309-310`) and sets MIE (`port_asm.S:348-349`).
7. About 5 ms later (499,968 clocks) comes the first tick. ISR: R 0x05 → W 0x04 ← pending → R 0x28 (`riscv_main.c:86-87,118`).
8. `ultimate_main` (`ultimate.cc:82`): R 0x0C-0x0F (`ultimate.cc:87`), prints `*** FPGA Capabilities: %8x ***` (`ultimate.cc:90`). `InitFunction::executeAll` (`ultimate.cc:93`) then triggers the capability-gated init described above:
   - drives (`c1541.cc:1257-1275`, prio 65), tape recorder (`tape_recorder.cc:16-20`, prio 61), RMII, ACIA
   - `U64Config`: `install_high_irq(5)`, `install_high_irq(6)` → R/W 0x27 (`u64_config.cc:971,974`)
   - command interface tasks: 0x04←0x80, 0x01←0x80, 0x01←0x10 (`command_intf.cc:101-102,118`)
9. `custom_outbyte` = textLog/syslog tee (`ultimate.cc:95`). C64 machine init if CAPAB_CARTRIDGE (`ultimate.cc:100-104`). `usb2.initHardware`: 0x02←0x04, 0x04←0x04 (`usb_base.cc:201-202`). The USB task later writes 0x04←0x04, 0x01←0x04 (`usb_base.cc:344-345`).
10. Main loop (`ultimate.cc:166-200`): `c64->checkButton()` reads 0x1000000A, then `vTaskDelay(3)` (15 ms). While a UI is active, `host->checkButton()` (`userinterface.cc:285,323,489,495`) reads 0x1000000A via `Overlay::checkButton`.

## Boot hazards

| # | Where | Poll/probe | Wrong emulator answer → effect | Required response |
|---|---|---|---|---|
| H1 | `itu.c:289` (`outbyte`, ELF @0x33BF0) | `while (FLAGS & 0x10)` before every console byte, from the first printf (constructors / `riscv_main.c:154`) | bit4 = 1 (e.g. 0xFF) → hang on the very first character | 0x10000012 reads with bit4 = 0. Recommended constant **0x40** (TX idle, no RX). Accept writes to 0x10000010. |
| H2 | `itu.c:66-70` (`wait_ms`), `itu.c:75-77` (`wait_10us`) | write N to 0x10000006, `while(read)` | constant non-zero → hang. First hit is pre-scheduler with IRQs off (`nau8822.cc:18`); also `u64_config.cc:600-630`, `c64.cc:408,1178`, `usb_base.cc:306,910`, `usb_hub.cc:168` | Read must reach 0: value − ⌊Δt / 5 µs⌋ in emulated time. Returning 0 at once is acceptable for T0 (delays vanish), but must not rely on interrupts. |
| H3 | `c64.cc:379-385,431-456,468-483,549-566` | "timer==0 ⇒ budget++" loops (25/10/40 × 1 ms) | constant non-zero → budget never expires; instant 0 → budget used up at once, falls through to the forced-stop path `c64.cc:490-497` (C64_STOP poll, other block) | Same as H2 (real countdown preferred). |
| H4 | `itu.c:83-86` (`getMsTimer`, ELF @0x33BA0) | read LO,HI,LO,HI, loop until equal | value changes on every read (per-read increment) → infinite loop | 0x10000022/23 must be stable between consecutive reads: derive from emulated time, `⌊t_ms⌋ & 0xFFFF`. |
| H5 | `usb_scsi.cc:129-131,167-170`, `w25q_flash.cc:494-500`, `keyboard_vt100.cc:40,74`, `keyboard_usb.cc:496,521`, `machine_monitor.cc:3306,3321,3750,6437-6445`, `dma_uart.cc:267`, `wifi.cc:155` | ms-delta timeouts / poll intervals | constant ms timer → USB mass-storage LUN poll runs once, then never (media never detected); flash `wait_ready` never times out if the flash model reports busy; lone ESC never delivered; monitor refresh stalls | ms timer increments by 1 per emulated millisecond, wraps at 16 bits. |
| H6 | `riscv_main.c:173-186`, `port.c:192`, `FreeRTOSConfig.h:21` | FreeRTOS tick exists **only** as ITU low IRQ bit0; no CLINT (`FreeRTOSConfig.h:62-63`, `portasmHAS_MTIME 0` chip header `:58`) | no periodic IRQ → first `vTaskDelay`/timed semaphore wait blocks forever (`ultimate.cc:199`, `u64_config.cc:1006`, `rmii_interface.cc:92`, …) → boot stalls after the scheduler starts | While 0x10000007 bit0 = 1 (select = 0): set flag bit0 every **499,968 clocks @100 MHz = 4.99968 ms** of emulated time. Raise the CPU external IRQ (mip.MEIP, mcause 0x8000000B) while `GLOBAL.0 && ((ACTIVE & mask) ≠ 0 \|\| HIGH_ACT ≠ 0)`. Emulated time must advance while the CPU spins: the idle task has no WFI (`FreeRTOSConfig.h:40`, no wfi outside `crt0.S:262`). |
| H7 | `riscv_main.c:86-87` | R ACTIVE, W CLEAR ← same value | ACTIVE not masked by ENABLE, or the edge flag not cleared by the CLEAR write → IRQ line stays high → ISR re-entered immediately after every mret → tasks starve (hang) | ACTIVE = (flag \| (src & ~edge)) & mask. CLEAR write clears flag bits. The timer source must be edge/latched (it is a 1-cycle pulse, `itu.vhd:113-116`). |
| H8 | `riscv_main.c:118-128` | R HIGH_ACT; for bits 0..6 call the handler, or disable the EN bit if no handler | unmasked or stuck non-zero, e.g. 0xFF → storm. HDMI (5) and unlock (6) handlers are installed (`u64_config.cc:971,974`) and never clear an emulator-side stuck bit. Bit7 is never serviced (`HIGH_IRQS 7`, `riscv_main.c:35`) | 0x10000028 = src_high & HIGH_EN. 0 when no source is active. Each source must drop when its own ack register is written (T1). |
| H9 | `ultimate.cc:100-107,166,209-213` | CAPAB_CARTRIDGE (bit9) | 0 → `c64 = NULL` → main loop skipped → "GUI running on C64 host has terminated?" → main task suspended, no menu | capabilities bit9 = 1 |
| H10 | `u64_config.cc:904`, `c64.cc:205`, `ultimate.cc:114`, `w25q_flash.cc:213` | CAPAB_ULTIMATE64 (bit26) | 0 → no U64 config store, no ROM init, no matrix keyboard, no HDMI/unlock IRQs; wrong flash table in `read_image` | bit26 = 1, bit25 = 0 |
| H11 | `w25q_flash.cc:81-83`, `filetype_u2p.cc:82` | FPGA type = cap[29:28] | 0/1 (e.g. all-zero caps) → 50T flash layout and U2-style update files accepted. The type must match the emulated flash image, otherwise config/flash-disk/APPL offsets are wrong (`w25q_flash.cc:51-63`) | 2 (XC7A50T) or 3 (XC7A100T), consistent with the flash image |
| H12 | `c64.cc:1501-1503`, `overlay.h:114-116`, `userinterface.cc:311,370` | bit6 of 0x1000000A | reads 0xFF/0x40 at boot → the first poll sees a rising edge (`button_prev` static 0) → UI `run_once` → `buttonDownFor(1000)` loops while bit6 is set → after 1 s `swapDisk()` instead of the menu. The button is then permanently "held" and never gives a new edge | idle **0x00**. Menu press = bit6 = 1 for ≥ 1 poll (≥ 15 ms) and < 1000 ms, then 0. Bits 5 and 7 are not read on U64 (`config.cc:50-58`, `blockdev_flash.cc:160-168` take the `U64_RESTORE_REG` branch). |
| H13 | Capability feature bits (`usb_base.cc:190`, `rmii_interface.cc:50`, `c1541.cc:1260-1265`, `command_intf.cc:44`, `tape_recorder.cc:45-47`, `acia.cc:10`) | init probes | A set bit makes the firmware drive that peripheral and enable its IRQ. An unmodelled block can hang there (other blocks' hazards) or raise unexpected IRQs. | T0: only bits 26, 9 + type. Add feature bits per modelled block. |
| H14 | `crt0.S:306-309` | early trap marker write to 0x1000001F | bus fault on write → nested trap loop | accept and ignore (UART ICTRL alias). Log it as "early trap". |

Diagnostics (not hazards): on a synchronous exception, `C_exception_handler` prints `** GURU MEDITATION:` with Address/Cause/Value, then `print_tasks`, then `while(1)` (`riscv_main.c:189-195`; entered from `port_asm.S:240-247`). An unexpected async trap cause never reaches `as_yet_unhandled` because MTIME=0 sends every async cause to the ITU handler (`port_asm.S:166-227`).

## Interrupts

### CPU side
- The ITU `irq_out` is registered and level (`itu.vhd:245-253`). RV core: `mip.MEIP = int_i` (level); an IRQ is taken when `mie.MEIE & mstatus.MIE` (`csr.vhd:93-96`), cause 0x8000000B (`core.vhd:9-10`). This is the rvlite RTL, which doc 01 identifies as the U64-II CPU (the closed top stays open, 00 Q-A1).
- FreeRTOS enables only MEIE (`port.c:192`). mtvec is direct mode (asserted at `port.c:162`), set at `riscv_main.c:171` and `port_asm.S:309-310`.
- Trap entry: any async cause → `freertos_risc_v_application_interrupt_handler` (`port_asm.S:166,225-226`, `portasmHAS_MTIME 0`). `ecall` (cause 11) = yield (`port_asm.S:233-238`, `portmacro.h:97`). Critical sections toggle mstatus.MIE (`portmacro.h:110-113`, `itu.h:86-87`).

### Low IRQ byte (0x10000001/2/3/4/5)

| Bit | Mask | itu.h | Reference source (U2+ top) | Edge (0x85 ref.) | Enabled by | Handler / ack |
|---|---|---|---|---|---|---|
| 0 | 0x01 | TIMER | internal periodic timer (`itu.vhd:113-125`) | edge | `riscv_main.c:185` | `xTaskIncrementTick` (`riscv_main.c:114-116`). Ack = CLEAR |
| 1 | 0x02 | UART | `uart_irq` (`itu.vhd:112`), always 0 (no g_impl_irq) | level | never | commented out (`riscv_main.c:110-112`) |
| 2 | 0x04 | USB | `sys_irq_usb` (`ultimate_logic_32.vhd:536`) | edge | `usb_base.cc:344-345` (clear, then enable) | `usb_irq` (`usb_base.cc:179`) drains the nano FIFO |
| 3 | 0x08 | TAPE | `sys_irq_tape` (`:535`) | level | `tape_recorder.cc:47` | `tape_recorder_irq` (`tape_recorder.cc:23-28`) |
| 4 | 0x10 | CMDIF | `sys_irq_cmdif` (`:534`) | level | `command_intf.cc:118` | `command_interface_irq` masks the source: `CMD_IF_IRQMASK_SET` (`command_intf.cc:85-96`). Note it **assigns** `do_switch` (`riscv_main.c:101`). |
| 5 | 0x20 | RMIIRX | `sys_irq_eth_rx` (`:533`) | level | `rmii_interface.cc:108` | `RmiiRxInterruptHandler` → `rx_interrupt_handler` (`rmii_interface.cc:41-44`) |
| 6 | 0x40 | RMIITX | `sys_irq_eth_tx` (`:532`) | level | never | none |
| 7 | 0x80 | RESET (C64 reset in) | `c64_reset_in` (`:531`) | edge | `command_intf.cc:101-102` (clear, then enable) | `ResetInterruptHandlerCmdIf` (`command_intf.cc:29`) + `ResetInterruptHandlerU64` (`u64_config.cc:2114-2117`) |

Dispatch order in the ISR: 0x80, 0x20, 0x10, 0x08, 0x04, 0x01, then the high IRQs (`riscv_main.c:91-129`). The firmware never writes ITU_IRQ_EDGE (ELF scan), so the edge mask is a fixed hardware property (see Open questions).

RTL corner cases:
- Edge flags latch **regardless of mask** (`itu.vhd:236-243`). Enabling a source with a stale flag fires at once, which is why the firmware clears bits 2 and 7 before enabling them.
- A set in the same cycle as a CLEAR write wins: the set is applied after the clear (`itu.vhd:130,145,236-243`).
- Global enable resets to 1, mask resets to 0 (`itu.vhd:256-257`).

### High IRQ byte (0x10000027 EN, 0x10000028 ACT)

| Bit | itu.h | Reference source (`ultimate_logic_32.vhd:523-530`) | Installed | Source-side ack |
|---|---|---|---|---|
| 0 | ACIA | `sys_irq_acia` | `acia.cc:67` (EN cleared `acia.cc:74-75`) | `Acia::IrqHandler` |
| 1 | 1581 (drive A WD177x) | `sys_irq_1541_1` | `wd177x.cc:63`, irqNr = 1 (`c1541.cc:115`) | `WD177x::IrqHandler` |
| 2 | (drive B WD177x) | `sys_irq_1541_2` | irqNr = 2 | same |
| 3 | WIFI | `sys_irq_wifi` | `dma_uart.h:80` (`esp32.cc:73`); EN re-set `dma_uart.cc:73` | `DmaUART::DmaUartInterrupt` |
| 4 | BLING | `bling_irq` | never in this build | — |
| 5 | HDMI | `hdmi_irq` | `u64_config.cc:971` | `U64_HDMI_REG = U64_HDMI_HPD_RESET` (`u64_config.cc:996`) |
| 6 | UNLOCK | `unlock_irq` | `u64_config.cc:974` | `C64_POKE(0xD038,0)` (`u64_config.cc:1014`) |
| 7 | GURU | `guru_irq` | never; the ISR loop stops at bit 6 (`riscv_main.c:119`) | — |

Protocol: HIGH_ACT = src & EN, level-sensitive, with no clear register in the ITU. If a bit is active but has no handler, the ISR clears that EN bit by read-modify-write (`riscv_main.c:124-127`).

## Functional model

### Clocks
A single emulated clock `t` drives all three time bases. Reference ticks: `tick_1us` = 1 MHz and `tick_1ms` = 1 kHz from `fractional_div` (`fractional_div.vhd:75-83`, wired at `ultimate_logic_32.vhd:518-519`). The CPU clock is 100 MHz (`CLOCK_FREQ`, Makefile).

- **ms timer** = `⌊t / 1 ms⌋ mod 65536`. Reset 0.
- **µs timer (0x06)**: store `(v0, t_w)` on write. Read = `max(0, v0 − ⌊(t − t_w) / 5 µs⌋)`. Hardware decrements on a free-running 5 µs divider (`itu.vhd:92-105`), so the first step comes 0-5 µs after the write; this is not firmware-visible in any meaningful way.
  - `wait_ms`: 200 counts = 1.000 ms.
  - `wait_10us(m)`: (2m+1) × 5 µs.
  - `C64::reset`: 20 counts = 100 µs.
- **Periodic IRQ timer**: register state `en`, `sel`, `val` (reset 0x8000), `cnt`.
  - Write 0x07: if old en == 0, `cnt = val*256 + 255`; then en = d0, sel = d1.
  - With `en = 1, sel = 0`: every CPU clock, `cnt == 0` ⇒ pulse src0 and reload `val*256 + 255`, else `cnt − 1` (`itu.vhd:114-125`). Period = (val+1) × 256 clocks. Firmware value 0x07A0 → 499,968 × 10 ns = **4.99968 ms** (200.013 Hz), matching `configTICK_RATE_HZ 200` / `portTICK_PERIOD_MS 5` (`FreeRTOSConfig.h:21`, `portmacro.h:85`).
  - The first pulse comes one full period after the enabling write.
  - `sel = 1` (count external `irq_timer_tick`, reload without <<8) is not used by the firmware.
  - Reads of 0x08/0x09 return `cnt` bytes (not used).
- **Tick collapse**: the edge flag is one bit. Ticks that occur while the flag is still set (for example across a long `ENTER_SAFE_SECTION`, `itu.h:86`) merge into one tick. An emulator that skips time in busy-wait loops (H2/H5) loses ticks the same way. FreeRTOS then sees time dilation, not a failure.

### IRQ controller state
- State: `irq_en` (reset 1), `mask` (0), `edge` (const, see Open questions), `flag` (0), `mask_high` (0).
- Per step: `flag |= edge & rise(src)`. `active = flag | (src & ~edge)`. `line = irq_en & (((active & mask) ≠ 0) | ((src_high & mask_high) ≠ 0))` → MEIP.
- Register writes: 0x01 or, 0x02 and-not, 0x04 and-not on flag, 0x27 set; 0x00 bit0.
- Reads: 0x00 bit0, 0x01 mask, 0x03 edge, 0x05 `active & mask`, 0x27 mask_high, 0x28 `src_high & mask_high`.

### Button
`0x1000000A = (buttons[2:0] << 5) | (menu ? 0x40 : 0)` (`itu.vhd:192-196`). The firmware uses only bit6 (0x40) as the edge-detected "menu" button (`c64.cc:1501-1503`, `overlay.h:114-116`). Held ≥ 1000 ms at UI entry means swap disk (`userinterface.cc:311-314,362-379`).

### LEDs
`busy_led = usb_busy | sd_busy | printer_busy` (bit0 of 0x24/0x25/0x29, `itu.vhd:327`). These are write-only; reads return 0. `misc_io` 0x26 is write-only and unused by this ELF.

### Console UART
- **TX path.** Every character from `printf` (`small_printf.cc:253-262`, ELF @0x42BF8 → `_my_vprintf` → `_diag_write_char`), `putchar` (`:457-461`) and `puts` (`:445-455`, ELF @0x43014) goes to `outbyte`.
  - `printf`/`putchar` expand `'\n'` to `"\r\n"` (`small_printf.cc:236-243`). `puts` appends `"\r\n"`. The host console should strip `\r`.
  - `outbyte` first calls `custom_outbyte` (textLog / syslog tee, set at `ultimate.cc:95`, cleared at `ultimate.cc:209`), then **always** waits on FLAGS bit4 and writes DATA (`itu.c:282-292`; the `else` is commented out).
- **Raw DATA writes without a flag check** (stray debug characters, mostly from ISRs):
  - `tape_recorder.cc:25` `-`, `tape_recorder.cc:194` `+`
  - `wifi_cmd.cc:23` `N`, `wifi_cmd.cc:26` `~`
  - `dma_uart.cc:179` `^`, `dma_uart.cc:221` `!`
  - `usb_base.cc:942` `#`
  - `command_intf.cc:92` `\` (via `outbyte`)
- **Newlib stdio** (`fwrite`/`fprintf` to fd 1/2) is **dropped**: `_write` looks up `_fm_file_mapping[fd]`, which is NULL for fds 0-2, and returns EBADF (`ult_syscalls.cc:111-117`, ELF @0xB335C).
- **RX path: unused in this build.** `Stream_UART` and `uart_read_buffer`/`uart_get_byte`/`uart_data_available` are not linked (symbol table), and the ELF never reads 0x10000010 and never writes 0x10000011. VT100 key input arrives over Telnet sockets (`keyboard_vt100.cc:70-72` comment), not the UART.
- **Hardware reference.** 115200 baud (`itu.vhd:19`, `ultimate_logic_32.vhd:20`). TX FIFO is an SRL FIFO with almost-full threshold 12 (`uart_peripheral_io.vhd:128-143`), or 1023 entries with threshold 1000 if g_uart_big_fifo (`:105-126`). A byte is sent when the FIFO has data and the transmitter is done (`:162`).

## Emulator model tiers

**T0 — boot to the main loop without a hang**
- Decode `0x10000000-0x1000003F` by `addr & 0x3F`. Unlisted reads 0, writes ignored, including 0x1000001F.
- 0x0C-0x0F: constant capabilities, big-endian bytes. Minimum `0x34000200` (or `0x24000200`; must match the flash image layout).
- 0x0B: constant (cosmetic). The emulator uses 0x25 (`ultimate_logic_32.vhd:13` default; Q4).
- 0x0A: 0x00.
- 0x12: 0x40. 0x10 writes → host stdout (strip `\r`).
- 0x06: countdown in emulated time, or return 0 immediately.
- 0x22/0x23: `⌊t_ms⌋`, stable between reads, advancing with emulated time that advances with executed instructions.
- IRQ core: global/mask/flag/clear/active semantics with GLOBAL reset to 1 (`itu.vhd:256`), timer source bit0 at 4.99968 ms period when 0x07 bit0 = 1, MEIP level output.
- 0x27 storage. 0x28 = 0.
- 0x24/0x25/0x26/0x29: accept writes.

**T1 — functional**
- Exact IRQ-timer counter (`cnt` reload and readback).
- Edge/level per bit with a configurable edge mask (default 0x85, see Open questions). Source lines from the USB, tape, UCI, RMII RX/TX and C64-reset models.
- High-IRQ sources from ACIA, WD177x A/B, WiFi DMA UART, HDMI HPD and unlock, each dropping on its own ack register.
- Host "menu button" → bit6 pulse (and long press). Busy-LED indicator from 0x24/0x25/0x29.
- Capability profile per modelled feature (e.g. `0x3DE40BE7`).
- UART TX pacing/FIFO-full emulation (optional). RX FIFO + flags for completeness (not exercised by this ELF).
- Optional host-time pacing of `t` for realistic UI/network timing. Optional busy-wait fast-forward on 0x06 / ms-timer loops (accepts tick collapse).

## Open questions

1. OPEN QUESTION: the real U64-II / C64 Ultimate `g_capabilities` value, including FPGA type 2 (XC7A50T) vs 3 (XC7A100T). The firmware prints it on the UART at boot as `*** FPGA Capabilities: %8x ***` (`ultimate.cc:90`); a hardware boot log would settle it. It must agree with the flash image layout (`w25q_flash.cc:51-63`).
2. OPEN QUESTION: U64-II ITU `g_edge_init` / `g_edge_write` (closed top). The only open reference is 0x85 / false (`ultimate_logic_32.vhd:507-508`). Firmware behaviour is consistent with bits 0, 2, 7 edge and 3, 4, 5 level (clear-before-enable at `usb_base.cc:344`, `command_intf.cc:101`; source masking at `command_intf.cc:87-89`), but this is inferred.
3. OPEN QUESTION: whether the U64-II wires `irq_in[7:2]` and `irq_high[7:0]` as in `ultimate_logic_32.vhd:523-536`. The `itu.h:33-47` names match that wiring.
4. OPEN QUESTION: the U64-II `g_version` byte. Cosmetic only (REST `fpga_version` "1xx", system info, DMA identify).
5. OPEN QUESTION: whether the closed top decodes the whole 0x10000000-0x1001FFFF ITU window (aliasing every 0x40) or only 0x00-0x3F. The firmware only touches 0x00-0x2F plus 0x1F.
6. OPEN QUESTION: whether 0x1000001F is a real debug/marker register on the U64-II top. `crt0.S:307-309` writes 'I' there on an early trap, while in the open ITU RTL it is the inert UART ICTRL alias.
7. OPEN QUESTION: what `misc_io` (0x10000026), `busy_led`, `buttons[2:0]` (bits 5/7) and `btn_menu` connect to on U64-II. Not needed by the firmware beyond bit6.
8. OPEN QUESTION: U64-II UART generics (baud, `g_uart_big_fifo`). They only affect how often TxFifoFull asserts.
