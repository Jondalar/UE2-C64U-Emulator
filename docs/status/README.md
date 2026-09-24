# Status

What works, by milestone. Details are in the linked files.

- **M1-M3** (`docs/status/boot.md`): boot log with `*** FPGA Capabilities: 34640226 ***` (`34000226` with
  `--c64 none`; the TRX64 C64 adds EEPROM, Command Interface and sampler), 60 s emulated in the idle loop with 0
  unmapped IO accesses, overlay menu driven by the menu button and cursor keys.
- **M4** (`docs/status/storage.md`): config persists in the flash image; an SD image is listed, browsed and written.
- **M5** (`docs/status/network.md`, `docs/status/e2e.md`): REST, Telnet and FTP through libslirp. The upstream E2E
  smoke profile passes 12 of 12 with the REST shim.
- **M6** (`docs/status/usb.md`): USB stick in the file browser (read and write), HID keyboard and mouse, devices
  plugged in and out while the machine runs (S33), and a host directory as a stick with safe write-back
  (`--usb-dir`, `docs/status/usb-dir.md`).
- **M7** (`docs/status/c64.md`): TRX64 boots to BASIC `READY.` under the overlay, runs a PRG from the file browser
  through the boot cartridge, and shows the Freeze UI. PAL and NTSC follow System Mode (S25). UltiSID 1 and 2 on
  reSID, an ARMSID in socket 1, and stereo out with the mixer's volume and pan (`docs/status/sid-audio.md`); the
  FPGA's cartridge types with the freezers, the GMOD2 EEPROM, EasyFlash writes and the SID/MUS players
  (`docs/status/carts.md`); drives A and B as 1541 or 1581 on TRX64's drive parts, and Software IEC
  (`docs/status/drive.md`). `--c64-roms` writes the C64 ROMs into the flash image before boot. A cartridge in the
  physical expansion port (`--cart-slot`, `docs/status/cart-slot.md`) is served by TRX64's mappers, flash boards
  (EasyFlash, GMod2, MegaByter, C64MegaCart) or the U64 cart logic, next to the internal cartridge; firmware DMA and
  REST dumps and flash writes reach it, and the TREX CRT Tool dumps it. The Ultimate Command Interface is served by
  TRX64's own block on its `u64` machine profile, so the firmware starts its UCI task and the menu offers "Command
  Interface" (`docs/specs/S15-uci.md`). The REU is TRX64's `Reu` over the firmware's own DDR, attached and sized
  while the machine runs by `C64_REU_ENABLE` and `C64_REU_SIZE` (`docs/status/reu.md`). Joysticks on both control
  ports, the USB mouse on POTX/POTY (S32, S36). What is still missing is in `docs/status/gaps.md`.
- **Install** (`docs/status/install.md`): the upstream `update.ue2` and the Commodore `c64u_v1.1.0.ue2` populate the
  flash; the Commodore 1.1.0 application boots and opens its menu. Its network services ship disabled.
- **Speed** (60 s emulated at `--speed max`, upstream ELF, no flash, Apple M4): 6.0 s wall with `--c64 trx64`,
  0.74 s with `--c64 none`. Realtime headless with TRX64 takes about 19 % of one core.
- **Debugging** (`docs/status/gdb.md`): GDB on the firmware's RISC-V (`--gdb`) with symbols, software
  breakpoints and the FreeRTOS task list; the C64 side is served by the VICE binary monitor port
  (`docs/status/monitor.md`).

Architecture: `docs/ARCHITECTURE.md`. Specs: `docs/specs/`.
