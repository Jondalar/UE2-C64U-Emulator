# S27 — Drives A and B on TRX64's drive parts (TRX64 Specs 870/871)

**Status:** built (2026-09-23) on TRX64 v0.8.8.

**Owns:**
- `crates/c64-bridge/src/drive.rs`: one `DriveSlot` per position on the 870 API; the stand-in, the TRUEDRIVE
  re-set, the ROM file and the `$77/$78` patch go
- `crates/c64-bridge/src/lib.rs`: two slots, `drive(1)` answers
- `crates/ue2-core/src/devices/c64.rs`, `devices/drives.rs`: drive B's window moves into `C64Port` beside drive A
- `crates/ue2emu/src/monitor/mod.rs`: `devices` lists the powered drives by unit
- `docs/status/drive.md`, `docs/status/gaps.md`

**Reads:** TRX64 v0.8.8 `docs/_archive/870-the-drive-as-a-part.md` §10 and `871-a-second-drive-on-the-bus.md` §9,
`fpga/1541/vhdl_source/drive_registers.vhd`, `c1541_drive.vhd`, `software/drive/c1541.cc`.

## 1. Register → drive part

Each U64 drive window (A at 0x10020000, B at 0x10024000) drives one TRX64 position (A = `drive8`, B = `drive_b`).

| Firmware line | TRX64 call |
|---|---|
| POWER bit 0, DRIVETYPE 0 (1541) | `Machine::set_drive_power(pos, on)`; a refusal (two powered drives at one unit) is printed once |
| RESET bit 0, or bit 1 with the C64's reset held | `set_reset_held` — the level `drv_reset or (use_c64_reset and c64_reset)` of `c1541_timing.vhd` |
| RESET bit 2 with the C64 stopped | `set_stopped` (870 §3a, `c1541_drive.vhd:164-165`) |
| HW_ADDR bits 1:0 | `Machine::set_drive_unit(pos, 8 + n)`, in force at the drive's next reset. The FPGA has two bits too (`drive_registers.vhd:77`), so 8-11 is the whole range and no DOS patch is left |
| ROM, 32 K at area + 0x8000 | `set_rom` with the whole 32 K (the firmware mirrors a 16 K file itself, `c1541.cc:937-940`) |
| SENSOR bit 0 | `rotation.read_only`, as before |

The IEC RESET line is cut in TRX64 (`set_reset_line_connected(false)`): the bridge drives the reset level itself from
the formula above, so `warm_reset`'s own pulse would only reset a second time.

**ROM change.** TRX64 brings a new ROM into force at the next power-on only (870 §4, owner decision). The firmware
loads a ROM into DDR and resets, without switching off (`c1541.cc:945, 1132`); on the device the FPGA reads the ROM from
DDR, so the reset runs it. The bridge therefore turns the reset of a powered drive with a new ROM pending into
off-and-on. RAM is cleared by that; the DOS's own reset initialises it anyway.

**Head.** A released reset puts the head on track 1, as `floppy_stream.vhd` `p_move` clears `track_i` — kept from S14.

## 2. Clocking

TRX64 clocks both positions after every instruction and while the C64 CPU is held (`Hold::Cpu`). Under `Hold::Reset`
it clocks only the VIC; there the bridge runs `drive::pair_catch_up` and `pair_fold_into_iec` itself, for both
positions, as it did for drive 8 alone.

## 3. The surface

Unchanged (TRX64 keeps it out of scope): the firmware's GCR goes into `rotation.image`, written half-tracks are found
by watching the head, the write mode and the dirty half-track, per position.

## 4. Drive B in UE2

Drive B's registers were a separate T0 device with no drive. They move into `C64Port` beside drive A and reach
`C64Backend::drive(1)`. Without a backend they stay registers only, as drive A's do.

## 5. Checks

- The bridge's drive tests on 0.8.8: directory, LOAD, RUN, SAVE with the bytes decoded from the surface; off → DEVICE
  NOT PRESENT; held and stopped run nothing; unit 9 answers at 9.
- Drive B: powered at unit 9, `LOAD"$",9` lists its disk while drive A keeps its own.
- The drive smoke and the C64 smokes.

## 6. Not in this spec

The 1581 (S31), the 1571 (out of scope), the disk surface as a TRX64 API (not planned).
