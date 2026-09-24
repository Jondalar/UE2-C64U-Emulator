# Features

One line per feature, as of 0.5.0 (TRX64 v0.9.2). Details: `docs/status/`, specs: `docs/specs/`. What is missing:
`docs/status/gaps.md`.

## Firmware and board

- Runs the unmodified firmware application: `ultimate.elf` or the application inside a `.ue2` update.
- Ultimate 64 Elite II and C64 Ultimate (`--board u64ii|c64u`); Commodore C64U firmware 1.1.0 boots.
- `ue2emu install`: runs a `.ue2` updater into a flash image, as on the device.
- `ue2emu settings` / `run --settings`: print and preset every firmware setting as a `.cfg`.
- `--config FILE.toml`: all run options in one file.
- RV32 CPU with idle skip; `--speed realtime|max`.
- SPI flash image persists the configuration (`--flash`).
- Menu overlay with the menu button and cursor keys; window or `--headless`.
- UART console, logged (`--log`).
- WiFi module as a stub: identified, voltages and power settings, "Link Down". Network is Ethernet.

## Storage

- SD card image: browse, read, write (`--sd`).
- USB sticks from images (`--usb`), several on the hub.
- Host directory as a USB stick with safe write-back (`--usb-dir`, read-only option).
- USB devices plugged and unplugged at runtime (`usb-plug`, `usb-unplug`, `--usb-hub`).

## Input

- USB HID keyboard (`--usb-keyboard`), host keys mapped to the C64 matrix.
- USB HID mouse (`--usb-mouse`): 1351 on port 1, window capture, PageDown releases.
- Joysticks on both control ports (`joy`, `joy-hold`, `joy-release`); port select as the firmware sets it.
- Key chords and held keys (`key cbm+z`, `hold`, `release`, `--hold-key`).

## Network

- Ethernet via libslirp NAT with port forwards (`--net user`, `--hostfwd`), or vmnet bridged.
- REST API, web UI (`--web-port`), Telnet, FTP.
- UDP video and audio streams, PAL and NTSC.

## C64 (TRX64)

- C64 boots to `READY.`, PAL, NTSC, PAL-60 and NTSC-50 as System Mode asks.
- C64 ROMs written into the flash image (`--c64-roms`).
- PRG start from the file browser (DMA load).
- Freeze and the Freeze UI.
- Smooth picture: every VIC frame shown, one-step scaling.
- Ultimate Command Interface (UCI).
- REU on the firmware's DDR, preload and "Save REU"; GeoRAM.

## Cartridges

- Internal cartridge: 28 CRT types of the U64 cart logic.
- Freezers: Action Replay, KCS, Super Snapshot 5, Final Cartridge.
- EasyFlash flash writes, GMOD2 EEPROM, MegaByter, TwoMegabyter, Pagefox.
- Cartridge RAM takes writes while banked out (AR/RR, SS5, Pagefox).
- SID and MUS player cartridges.
- Physical expansion port cartridge next to the internal one (`--cart-slot`), with save and flash decode.

## Drives

- Drive A and B as 1541 or 1581, D64/D81 mounted from the menu, write-back.
- 1581 floppy controller modelled; the firmware serves the D81.
- Software IEC: the firmware's IEC processor microcode on the C64 bus.

## Sound

- UltiSID 1 and 2 (split instances A-D) on reSID.
- ARMSID in socket 1 (`--sid-socket1 armsid`).
- Ultimate Audio sampler.
- Stereo mixer as set in the menu; audio device (`--audio`) and WAV (`--audio-wav`).

## Debugging and automation

- GDB on the firmware's RISC-V with symbols and FreeRTOS tasks (`--gdb`).
- VICE binary monitor port for the C64 (`--vice-monitor`).
- Monitor verbs: firmware, C64, cart, ITU, USB, net, SD, flash, audio, joy; step, next, until, return.
- Control port and scripts (`--control`, `--script`): keys, type, wait, expect, screenshots, USB, joy.
- MCP server (`ue2-mcp`): Claude Code sessions start, drive and screenshot emulator instances.
- Release gate `scripts/gate.sh`: workspace tests, firmware and C64 smokes, upstream E2E smoke profile.

## Platforms

- macOS (Homebrew tap `jondalar/ue2emu`), Linux, Windows 11 x86-64 (zip on the release).
