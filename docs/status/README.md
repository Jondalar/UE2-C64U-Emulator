# Status

What works, by milestone. Details are in the linked files.

- **M1-M3** (`docs/status/boot.md`): boot log with `*** FPGA Capabilities: 34000222 ***`
  (`34400222` with the TRX64 C64, which adds CAPAB_EEPROM), 60 s emulated in the
  idle loop with 0 unmapped IO accesses, overlay menu driven by the menu button and cursor keys.
- **M4** (`docs/status/storage.md`): config persists in the flash image; an SD image is listed, browsed and written.
- **M5** (`docs/status/network.md`, `docs/status/e2e.md`): REST, Telnet and FTP through libslirp. The upstream E2E
  smoke profile passes 12 of 12 with the REST shim.
- **M6** (`docs/status/usb.md`): USB stick in the file browser (read and write), HID keyboard input, hot-plug on the
  hub ports, and a host directory as a stick with safe write-back (`--usb-dir`, `docs/status/usb-dir.md`).
- **M7** (`docs/status/c64.md`): TRX64 boots to BASIC `READY.` under the overlay, runs a PRG from the file browser
  through the boot cartridge, and shows the Freeze UI (phase A). Wave 4 added an ARMSID in socket 1 and UltiSID 1 on
  reSID with audio out (`docs/status/sid-audio.md`); 27 cartridge types with the Action Replay freezer, the GMOD2
  EEPROM, EasyFlash writes and the SID/MUS players (`docs/status/carts.md`); and drive A as a 1541 with directory,
  LOAD and SAVE on a mounted D64 (`docs/status/drive.md`). `--c64-roms` writes the C64 ROMs into the flash image
  before boot. A cartridge in the physical expansion port (`--cart-slot`, `docs/status/cart-slot.md`) is served by
  TRX64's mappers, flash boards (EasyFlash, GMod2, MegaByter, C64MegaCart) or the U64 cart logic, next to the internal
  cartridge; firmware DMA and REST dumps and flash writes reach it, and the TREX CRT Tool dumps it. Open: UCI, REU,
  the IEC processor, drive B and 1571/1581, a second SID, NTSC.
- **Install** (`docs/status/install.md`): the upstream `update.ue2` and the Commodore `c64u_v1.1.0.ue2` populate the
  flash; the Commodore 1.1.0 application boots and opens its menu. Its network services ship disabled.
- **Speed** (60 s emulated at `--speed max`, upstream ELF): about 124 host MIPS with `--c64 trx64` (12.1 s wall), 111
  with drive A on and the ARMSID playing into a WAV (13.5 s), and 208 with `--c64 none` (7.2 s). Realtime needs
  25 MIPS; with drive, SID and the audio device on it keeps up at 26 % of one core.

Architecture: `docs/ARCHITECTURE.md`. Specs: `docs/specs/`.
