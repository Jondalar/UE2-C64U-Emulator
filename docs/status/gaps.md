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
- **Drives** (owner decision 2026-09-23, one spec each, spec numbers to follow):
  - (a) Drive API: a held drive, drive A switched off (DRIVE_POWER: it must leave the emulated IEC lines alone), ROM
    from memory (16 K and 32 K), device number, independence from the C64 warm reset, VIA2/RAM accessors. First.
  - (c) A second drive on the bus (unit 9+), for drive B. After (a).
  - 1571 and 1581: TRX64, as their own specs later. Not built in UE2. `drive.md`

## UE2 (board, firmware side, host)

- **Cartridges beyond TRX64's families** (freezers, Atomic Power, Business BASIC, Pagefox). TRX64 keeps `CartMapper`
  as it is (2026-09-23), so these stay on the bridge's workarounds: PLA recompute after line-changing reads and on
  timers; a replicated NMI check to switch a freezer in before the vector (an IRQ-first freeze runs one KERNAL
  instruction); cart RAM written only while its window is mapped (AR/RR/SS5/Pagefox writes under a banked-out ROM
  are lost); Business BASIC's dynamic mode off. A hook the fix needs has to live in UE2. `carts.md`
  **Planned:** the lost RAM writes — an observer hands every `$8000-$BFFF`/`$E000-$FFFF` write to the cart logic
  while one of these carts is in.
- **50/60 Hz outside the C64.** The C64 runs NTSC since S25; the overlay, the window's redraw and the UDP video
  stream still assume 50 Hz. `c64.md`
- **ACIA** (the SwiftLink/modem cartridge at `$DE00`/`$DF00`, which the firmware bridges to the network as a Hayes
  modem): not modelled. A cartridge device like the freezers, so UE2's (2026-09-23). `carts.md`
- **SID socket 2** and the second SID of a dual chip (ARM2SID): their probes find nothing. UltiSID 1 and 2 run on
  reSID in the bridge already; TRX64 only routes the addresses (Spec 855). Chip 0's OSC3/ENV3 reads come from
  TRX64's fastsid, not reSID — accepted, not planned. `sid-audio.md`
- **Audio.** Stereo sink, the mixer registers (`U64_AUDIO_MIXER`, `AUDIO_SEL_BASE`), C64_VOICE_ADSR for the LED
  strip, UltiSID filter curves. `sid-audio.md`, `sampler.md`
- **Disk surface.** An external surface with a write hook and the firmware's per-track bit time is not TRX64's
  (2026-09-23): UE2 keeps setting the image in and polling for written tracks; G64 tracks whose length differs from
  their zone's wrap at another rate than on hardware. `drive.md`
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
