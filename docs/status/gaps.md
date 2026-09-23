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
- **50/60 Hz outside the C64.** The C64 runs NTSC since S25, but snapshots are published every 20 ms emulated and the
  window redraws every 20 ms wall: under NTSC one picture in six is lost, under PAL the clocks beat. The overlay rides
  in the same snapshot. **Planned:** `docs/specs/S26-frame-pacing.md`. The UDP video stream's NTSC framing stays
  open. `c64.md`
- **ACIA** (the SwiftLink/modem cartridge at `$DE00`/`$DF00`, which the firmware bridges to the network as a Hayes
  modem): not modelled. A cartridge device like the freezers, so UE2's (2026-09-23). **Deferred:** C64 programs reach
  the network through UCI, which is enough for now. `carts.md`
- **Audio. Planned:** a stereo sink (UltiSID 1/2 and the sampler panned as on the device; touches the sink, the
  ring, the WAV writer and `wav-tone.py`), and the mixer registers (`U64_AUDIO_MIXER`, `AUDIO_SEL_BASE`) for the
  sampler, drive and tape channels. Dropped: C64_VOICE_ADSR (the LED strip, no LEDs here) and the UltiSID filter
  curves (FPGA filters, no source; reSID's own apply). `sid-audio.md`, `sampler.md`
- **Disk surface.** An external surface with a write hook and the firmware's per-track bit time is not TRX64's
  (2026-09-23): UE2 keeps setting the image in and polling for written tracks; G64 tracks whose length differs from
  their zone's wrap at another rate than on hardware. Accepted as a known limit, not planned. `drive.md`
- **IEC processor. Planned:** Software IEC only (the virtual drive, device 11 by default, loading straight from SD
  and USB directories). The processor and its microcode come from the open FPGA source; it drives TRX64's IEC lines.
  Spec not written yet. Dropped: the IEC printer and UltiCopy (needs a real drive). `drive.md`
- **I2C devices — dropped.** Codec (NAU8822), hub (USB2513), expanders, PLLs ACK and read 0xFF; they configure
  hardware the emulator does not have, and the boot is clean. Revisit only if the firmware stalls on one. `fixes.md`
- **Peripherals. Planned:** a USB mouse (host mouse → HID mouse → the firmware's 1351 emulation on the joyport, for
  GEOS), and detach on the root port (the nano's disconnect path, `RAM_STATUS = 0x8000`). Dropped: WiFi beyond the
  stub (Ethernet covers the network), AX88772 (same), HDMI hot-plug (no second monitor). `usb.md`, `boot.md`
- **Never run end to end. Planned** as one test package with smoke scripts: REU preload and "Save REU"; KCS, SS5
  and FC1 freezing from the firmware menu; GeoRAM and TwoMegabyter started by the firmware. A failure becomes a fix.
  `reu.md`, `c64.md`
- **Network — dropped.** No link loss when the vmnet daemon goes away (vmnet mode only, unused); no mDNS, which the
  hardware's RX filter drops as well. `network.md`

## Needs a measurement on the device

- **HDMI scan lines.** Accepted and ignored; the video path is in the closed FPGA. `c64.md`
- **Expansion port timing.** No contention, no PHI2 or address-setup timing; only the DMA byte cost is measured.
  `cart-slot.md`
- **Top level.** Capability word, BOARDREV, flash part and the freeze button's matrix position are assumptions.
  `boot.md`, `carts.md`
