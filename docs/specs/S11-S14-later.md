# Later specs (outline, detailed when their wave starts)

## S11 — Debug

**Status:** done (merged with wave 2): `--gdb`, `--trace`, `monitor tasks`, `--log irq`; `docs/ARCHITECTURE.md` §Debugging.

- **GDB stub:** `--gdb 127.0.0.1:1234` using `gdbstub` + `gdbstub_arch::riscv::Riscv32`. Supports register
  and memory read/write through `peek8` for IO, software breakpoints (`Machine::breakpoints`), step,
  continue, and Ctrl-C interrupt.
- **CPU trace ring:** the last N PCs with symbols, dumped on halt.
- **IRQ log**, and a task list from FreeRTOS `pxCurrentTCB` / ready lists.

## S12 — Network

**Status:** done. RMII MAC, MDIO PHY and `--net user` merged with wave 2; `--net vmnet-bridged` and
`--net socket-vmnet` in wave 3 (`docs/status/network.md`). The E2E smoke profile passes with the REST shim
(`docs/status/e2e.md`). WiFi L2 is not done.

- **RMII MAC:** per `docs/hw/08-network-rmii.md` T1 — RX filter, TX, free queue, level IRQ bit 5, DMA
  buffers.
- **MDIO PHY:** decoder behind U2PIO `0x1010000A/B` and `0x10100006`; reg2 = 0x0022; link up.
- **Host bridge:** libslirp (Homebrew 4.9.4, `libslirp-sys`) as a virtual L2 segment with DHCP.
  `hostfwd` localhost:8080→80, 2323→23, 2121→21, 6464→64, all configurable.
- **Optional:** WiFi L2 via the S05 command handler (0x08 frames).
- **Acceptance:** `curl localhost:8080/v1/info`, telnet UI, and FTP listing work. The upstream
  `tests/run-tests --profile smoke` runs against the emulator.

## S13 — USB HLE

**Status:** done (wave 3), `docs/status/usb.md`. Follow-up done: hot-plug on hub ports and `--usb-dir`, a host
directory as a stick with guest-to-host sync (`crates/ue2-vfat`, `docs/status/usb-dir.md`).

**Owns:** `crates/ue2-core/src/devices/usb/`, `crates/ue2emu/src/usb.rs`, `scripts/smoke-usb.ctl`,
`docs/status/usb.md`. Additive elsewhere: `HostInput::UsbKey`, `MachineConfig::usb` and its input route, the
`usbkey` control command, the `--usb-keyboard` key route in the window.

- HLE of the nano CPU protocol per `docs/hw/09-usb.md` T1 (F1-F3), with the USB2513 hub as the root device
  (T1b, 3 ports).
- Mass-storage device backed by an image (SCSI BOT); HID keyboard fed from the host.
- **CLI:** `--usb <image>` (repeatable), `--usb-keyboard`. `CAPAB_USB_HOST2` is set only when a device is
  configured, so a run without them boots as before.
- **Acceptance:** `scripts/smoke-usb.ctl` shows the stick and its files; 60 s emulated with devices attached,
  no halt; `cargo test --workspace` passes.

## S14 — C64 via TRX64

**Status:** phase A done (wave 3). Wave 4 (merged): SID socket 1 as an ARMSID and UltiSID 1 on reSID with audio out
(`docs/status/sid-audio.md`), every FPGA cartridge type the firmware loads plus the SID/MUS players
(`docs/status/carts.md`), drive A as a 1541 with write-back into the image (`docs/status/drive.md`). Detailed spec
`docs/specs/S14-c64-trx64.md`, results `docs/status/c64.md`. Follow-ups done: `--c64-roms` (the C64 ROMs written into
the flash image before boot, `docs/status/c64.md`) and a cartridge in the physical expansion port (`--cart-slot`, with
its own flash and EEPROM and write-back into the CRT, `docs/status/cart-slot.md`). Open: UCI, REU, the IEC processor
(SoftIEC, printer), drive B and 1571/1581, socket 2/UltiSID 2, exact stops, NTSC.

- `trx64-core` crate as the C64 behind `c64.rs`:
  - `0x10050000` DMA → `Machine::poke`/`read_full`;
  - STOP/reset → pause/reset;
  - U64 expansion mapper (cart, UCI `$DF1C`);
  - drive A → `drive8`, fed from firmware GCR buffers;
  - keyboard matrix → CIA;
  - video frame composited under the overlay.
- Scope drops, accepted by the user: 1571/1581/IDE, multi-SID, REU.
