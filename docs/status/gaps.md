# Gaps against the device

What the emulator does not do that a U64-II / C64 Ultimate does, and who has to build it. Details are in the linked
status files.

## TRX64 (the C64 core)

- **Cycle-exact stops and DMA — parked (2026-09-23).** Stops land on instruction boundaries and resume at once;
  the device stops and releases on the STOP_MODE condition (14 cycles into a badline, a read after a write, or at
  once; `slot_master_v4.vhd`). VIC and CIAs run through the stop on both, so the only difference is the sub-line
  phase at resume: code that re-syncs each frame shows at most one bad frame, code that syncs once stays shifted
  after menu, freeze or DMA load. TRX64: medium change in the cycle core (RDY hold on read cycles, R/W history,
  BA-low count), needs a device measurement. No program known to break; revisit when one does. `c64.md`, `carts.md`
- **Cartridge API.** `carts.md` §TRX64 API gaps:
  - EXROM/GAME by address and R/W (`get_lines`); Business BASIC's dynamic mode is off without it.
  - PLA re-evaluation after reads and by time.
  - A hook between the interrupt pushes and the vector fetch.
- **Drives.** Drive B, 1571 and 1581 (MFM, WD177x, side 1). API: a held or powered-off drive, drive power on the
  IEC bus, ROM from memory instead of a file. `drive.md`
- **SID.** C64 programs read OSC3/ENV3 from fastsid instead of reSID; several SID instances for socket 2 and
  UltiSID 2. `sid-audio.md`
- **ACIA** as a device. `carts.md`

## UE2 (board, firmware side, host)

- **50/60 Hz outside the C64.** The C64 runs NTSC since S25; the overlay, the window's redraw and the UDP video
  stream still assume 50 Hz. `c64.md`
- **Audio.** Stereo sink, the mixer registers (`U64_AUDIO_MIXER`, `AUDIO_SEL_BASE`), C64_VOICE_ADSR for the LED
  strip, UltiSID filter curves. `sid-audio.md`, `sampler.md`
- **IEC processor** (SoftIEC, printer, UltiCopy). FPGA logic, so ours, but it needs TRX64's IEC bus. `drive.md`
- **I2C devices.** Codec (NAU8822), hub (USB2513), expanders, PLLs: they ACK and read 0xFF. `fixes.md`
- **Peripherals.** WiFi beyond the stub (a scan finds nothing), USB mouse, AX88772, detach on the root port, HDMI
  hot-plug. `boot.md`, `usb.md`, `fixes.md`
- **Never run end to end.** REU preload and "Save REU"; KCS, SS5 and FC1 freezing, GeoRAM and TwoMegabyter from
  the firmware. `reu.md`, `c64.md`
- **Network.** No link loss when the vmnet daemon goes away; no mDNS (the RX filter drops multicast, as on the
  hardware). `network.md`

## Needs a measurement on the device

- **HDMI scan lines.** Accepted and ignored; the video path is in the closed FPGA. `c64.md`
- **Expansion port timing.** No contention, no PHI2 or address-setup timing; only the DMA byte cost is measured.
  `cart-slot.md`
- **Top level.** Capability word, BOARDREV, flash part and the freeze button's matrix position are assumptions.
  `boot.md`, `carts.md`
