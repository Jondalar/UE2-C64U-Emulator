# Gaps against the device

What the emulator does not do that a U64-II / C64 Ultimate does, and who has to build it. Details are in the linked
status files. Each item was reviewed on 2026-09-23 and is marked **parked**/**Deferred** (until a program or a need
shows up), accepted as a limit, **out of scope**, or **dropped**; an item without a marker is open.

## TRX64 (the C64 core)

- **Cycle-exact stops and DMA — parked (2026-09-23).** Stops land on instruction boundaries and resume at once;
  the device stops and releases on the STOP_MODE condition (14 cycles into a badline, a read after a write, or at
  once; `slot_master_v4.vhd`). VIC and CIAs run through the stop on both, so the only difference is the sub-line
  phase at resume: code that re-syncs each frame shows at most one bad frame, code that syncs once stays shifted
  after menu, freeze or DMA load. TRX64: medium change in the cycle core (RDY hold on read cycles, R/W history,
  BA-low count), needs a device measurement. No program known to break; revisit when one does. `c64.md`, `carts.md`
- **1571 — out of scope (2026-09-24).** Drives A and B are 1541 or 1581; DRIVETYPE 1571 keeps the drive off.
  `drive.md`

## UE2 (board, firmware side, host)

- **Cartridges beyond TRX64's families** (freezers, Atomic Power, Business BASIC, Pagefox). TRX64 keeps `CartMapper`
  as it is (2026-09-23), so these stay on the bridge's workarounds: PLA recompute after line-changing reads and on
  timers; a replicated NMI check to switch a freezer in before the vector (an IRQ-first freeze runs one KERNAL
  instruction); Business BASIC's dynamic mode off. A hook the fix needs has to live in UE2. `carts.md`
- **UDP video stream under NTSC:** which 240 of the 247 canvas lines the FPGA sends is not in any source; UE2 sends
  the middle ones (S34), not measured. `c64.md`
- **Joystick swap on the C64 side:** the swap bit (`U64II_KEYB_JOY` bit 0) selects which port the menu reads, but
  the C64's two ports are not swapped (S36). The FPGA is closed; whether the same bit swaps the CIA's ports needs a
  device check. `c64.md`
- **ACIA** (the SwiftLink/modem cartridge at `$DE00`/`$DF00`, which the firmware bridges to the network as a Hayes
  modem): not modelled. A cartridge device like the freezers, so UE2's (2026-09-23). **Deferred:** C64 programs reach
  the network through UCI, which is enough for now. `carts.md`
- **Audio.** Drive sounds (an FPGA sample player reading `snds1541.bin`) are **out of scope** (2026-09-24); tape
  sounds have no source here; C64_VOICE_ADSR (the LED strip) and the UltiSID filter curves are dropped.
  `sid-audio.md`, `sampler.md`
- **Disk surface.** An external surface with a write hook and the firmware's per-track bit time is not TRX64's
  (2026-09-23): UE2 keeps setting the image in and polling for written tracks; G64 tracks whose length differs from
  their zone's wrap at another rate than on hardware. Accepted as a known limit, not planned. `drive.md`
- **IEC processor — dropped:** the IEC printer and UltiCopy (master mode; UltiCopy needs a real drive). Software
  IEC runs (S30). `drive.md`
- **I2C devices — dropped.** Codec (NAU8822), hub (USB2513), expanders, PLLs ACK and read 0xFF; they configure
  hardware the emulator does not have, and the boot is clean. Revisit only if the firmware stalls on one. `fixes.md`
- **USB mouse on the POTs:** how the 7-bit position becomes the POT byte is assumed as a 1351's (`position << 1`)
  until measured on a C64U (S32 §4). `usb.md`
- **Peripherals — dropped:** WiFi beyond the stub (Ethernet covers the network), AX88772 (same), HDMI hot-plug (no
  second monitor). The USB root port never detaches: the hub is on the board. `usb.md`, `boot.md`
- **Network — dropped.** No link loss when the vmnet daemon goes away (vmnet mode only, unused); no mDNS, which the
  hardware's RX filter drops as well. `network.md`

## Needs a measurement on the device

- **HDMI scan lines — out of scope (2026-09-24):** accepted and ignored. `c64.md`
- **Expansion port timing — dropped.** No contention, no PHI2 or address-setup timing; only the DMA byte cost is
  measured. It would take a logic analyser on the port, for exotic hardware carts only. `cart-slot.md`
- **Top level.** Capability word, BOARDREV, flash part and the freeze button's matrix position are assumptions that
  work. Read what can be read on the C64U (REST, monitor) when the occasion comes; no plan of its own. `boot.md`,
  `carts.md`
