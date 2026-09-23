# Gaps against the device

What the emulator does not do that a U64-II / C64 Ultimate does, and who has to build it. Details are in the linked
status files. Each item was reviewed on 2026-09-23 and is marked **Planned**, **parked**/**Deferred** (until a program
or a need shows up), accepted as a limit, or **dropped**.

## TRX64 (the C64 core)

- **Cycle-exact stops and DMA — parked (2026-09-23).** Stops land on instruction boundaries and resume at once;
  the device stops and releases on the STOP_MODE condition (14 cycles into a badline, a read after a write, or at
  once; `slot_master_v4.vhd`). VIC and CIAs run through the stop on both, so the only difference is the sub-line
  phase at resume: code that re-syncs each frame shows at most one bad frame, code that syncs once stays shifted
  after menu, freeze or DMA load. TRX64: medium change in the cycle core (RDY hold on read cycles, R/W history,
  BA-low count), needs a device measurement. No program known to break; revisit when one does. `c64.md`, `carts.md`
- **Drives:** Specs 870 and 871 are in TRX64 v0.8.8 and UE2 uses them (S27). 1571 and 1581 are TRX64's, later.

## UE2 (board, firmware side, host)

- **Cartridges beyond TRX64's families** (freezers, Atomic Power, Business BASIC, Pagefox). TRX64 keeps `CartMapper`
  as it is (2026-09-23), so these stay on the bridge's workarounds: PLA recompute after line-changing reads and on
  timers; a replicated NMI check to switch a freezer in before the vector (an IRQ-first freeze runs one KERNAL
  instruction); Business BASIC's dynamic mode off. A hook the fix needs has to live in UE2. Cart RAM under a
  banked-out window takes its writes since S28 (TRX64's port snoop). `carts.md`
- **UDP video stream under NTSC.** The window shows every VIC picture under PAL and NTSC since S26; the UDP video
  stream's framing still assumes PAL. `c64.md`
- **ACIA** (the SwiftLink/modem cartridge at `$DE00`/`$DF00`, which the firmware bridges to the network as a Hayes
  modem): not modelled. A cartridge device like the freezers, so UE2's (2026-09-23). **Deferred:** C64 programs reach
  the network through UCI, which is enough for now. `carts.md`
- **Audio.** Stereo with the mixer's volume and pan for the SIDs and the sampler since S29. Drive sounds (an FPGA
  sample player reading `snds1541.bin`) and tape sounds have no source here; C64_VOICE_ADSR (the LED strip) and the
  UltiSID filter curves are dropped. `sid-audio.md`, `sampler.md`
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

- **HDMI scan lines. Planned once a photo exists:** accepted and ignored today; a cosmetic darkening of every other
  line in the renderer, matched to a photo of a C64U with scan lines on. `c64.md`
- **Expansion port timing — dropped.** No contention, no PHI2 or address-setup timing; only the DMA byte cost is
  measured. It would take a logic analyser on the port, for exotic hardware carts only. `cart-slot.md`
- **Top level.** Capability word, BOARDREV, flash part and the freeze button's matrix position are assumptions that
  work. Read what can be read on the C64U (REST, monitor) when the occasion comes; no plan of its own. `boot.md`,
  `carts.md`
