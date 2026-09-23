# W4-DRIVE status — drive A as a 1541

Design: `docs/specs/S14-c64-trx64.md` §W4-DRIVE. **Reached.** With the unmodified firmware (`ultimate.elf`, V1.01
3.15), a D64 mounted on drive A from the file browser works from the C64: `LOAD"$",8` + `LIST` shows its directory,
`LOAD"TEST",8` + `RUN` runs the program, and `SAVE"NEW",8` ends up in the `.d64` file on the SD image after the
firmware's own write-back.

Drive A is TRX64's drive 8 (`c64-bridge/src/drive.rs`) behind the firmware's drive registers
(`ue2-core/src/devices/drives.rs`, served through `C64Port`):
- the drive ROM comes from the firmware's drive area (DDR 0x00EE8000), loaded there by "Set as 1541 ROM";
- the disk surface is the firmware's GCR in DDR, per half-track as its param RAM points at it, so the firmware keeps
  converting D64/G64 itself;
- POWER, RESET (hold, follow the C64 reset, stop when frozen), HW_ADDR and SENSOR drive the drive; TRACK and STATUS
  (motor, writing, write busy) report it;
- GCR the 1541 writes goes back into that DDR with the DIRTY bits set, and the firmware's drive task decodes it into
  the image (`Writing back binary track N...`).

Drive B stays off (its registers have no drive behind them). With `--c64 none` drive A reads as before (T0).

## Setup

From a worktree, with the firmware built:

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate
FW="--firmware $UE2_FIRMWARE/target/u64ii/riscv/ultimate/result/ultimate.elf --roms $UE2_FIRMWARE/roms"
cargo build --release
rm -rf run && mkdir run
# C64 ROMs into the flash (S14 §12 A2 setup).
scripts/make-sd-image.sh run/roms.img
scripts/add-sd-files.sh run/roms.img $UE2_FIRMWARE/roms/{kernal.901227-03.bin,basic.901226-01.bin,characters.901225-01.bin}
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --sd run/roms.img \
    --script scripts/smoke-c64-roms.ctl > run/roms.log
# The drive test card: 1541.bin and a D64 with known files.
scripts/d64tool.py build run/drive.d64 --title "UE2 DRIVE" --id U2 --print "TEST=DRIVE A LOADED OK" \
    --print "SECOND=SECOND FILE" --filler BIG=30000
scripts/make-sd-image.sh run/drive-sd.img
scripts/add-sd-files.sh run/drive-sd.img $UE2_FIRMWARE/roms/1541.bin run/drive.d64
```

`scripts/d64tool.py` builds the D64 (BAM, directory, PRGs laid out from track 17 down), lists and extracts D64 files,
and reads a file out of the FAT32 SD image without mounting it (`sd-get`).

A blank flash has no drive ROM (`/flash/roms/1541.rom` is the default config, c1541.cc:51), so the firmware powers
drive A off (c1541.cc:202-204, 921-943). The smoke script installs `1541.bin` the way a user does.

## Acceptance

```sh
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --sd run/drive-sd.img \
    --script scripts/smoke-c64-drive.ctl > run/smoke-drive.log
scripts/d64tool.py sd-get run/drive-sd.img drive.d64 run/after.d64
scripts/d64tool.py extract run/after.d64 NEW run/new.prg
scripts/d64tool.py extract run/after.d64 TEST run/test.prg
cmp run/new.prg run/test.prg && echo DRIVE PASS
```

Result (exit 0, every `expect` held, `DRIVE PASS`; 36.9 s emulated):

| Step | Firmware / C64 |
|---|---|
| `1541.bin` → "Set as 1541 ROM" | `Copying 1541.bin to /flash/roms`, `Writing config store 'Drive A Settings' to flash`, `Effectuate 1541 settings:`; drive A powers on and resets |
| `drive.d64` → "Mount Disk" | `Loading...Transferred: 174848 bytes`, `Tracks: 35. Errors: No`, the GCR track table (1.0 at 1DC4A0, 1E0C bytes …), `Inserting...`, `MENU HIDE / EXIT.` |
| `LOAD"$",8`, `LIST` | dump below |
| `LOAD"TEST",8`, `RUN` | `DRIVE A LOADED OK` |
| `SAVE"NEW",8` | `SAVING NEW`, `READY.`; the drive task writes back track 18, 19, then 18 again, 19 sectors each without errors |
| host | `drive.d64` in `run/drive-sd.img` lists `1 "NEW" PRG`, and NEW has the bytes of TEST |

```
LOAD"$",8

SEARCHING FOR $
LOADING
READY.
LIST

0 "UE2 DRIVE       " U2 2A
1    "TEST"             PRG
1    "SECOND"           PRG
119  "BIG"              PRG
543 BLOCKS FREE.
READY.
LOAD"TEST",8

SEARCHING FOR TEST
LOADING
READY.
RUN
DRIVE A LOADED OK

READY.
SAVE"NEW",8

SAVING NEW
READY.
```

Console after the SAVE:

```
Writing back binary track 18...
19 sectors found. (0C:00 0D:00 0E:00 0F:00 10:00 11:00 12:00 00:00 01:00 02:00 03:00 04:00 05:00 06:00 07:00 08:00 09:00 0A:00 0B:00 )
Writing back binary track 19...
19 sectors found. (06:00 07:00 08:00 09:00 0A:00 0B:00 0C:00 0D:00 0E:00 0F:00 10:00 11:00 12:00 00:00 01:00 02:00 03:00 04:00 05:00 )
Writing back binary track 18...
19 sectors found. (0C:00 0D:00 0E:00 0F:00 10:00 11:00 12:00 00:00 01:00 02:00 03:00 04:00 05:00 06:00 07:00 08:00 09:00 0A:00 0B:00 )
```

## Unit tests

- ue2-core `devices::drives`: C28 for drive A (through `C64Port`) and drive B; param RAM hands the drive its
  surfaces from DDR; register writes drive the lines with the ROM image; drive writes reach DDR and DIRTY (track, not
  half-track), write busy lasts 2047 ms, MAN_WRITE restarts it and the drive reset clears it; a dark sensor keeps the
  disk unchanged; the drive RAM is mirrored while powered.
- c64-bridge `drive` (skips without `UE2_FIRMWARE`), on TRX64 0.8.8's drive part (S27): an unpowered drive is off
  the IEC bus and not clocked, held in reset it is off the bus too, stopped (RESET bit 2 with the C64 frozen) it stays
  on the bus unclocked, and each of RESET bits 0-2 holds or releases it; the 1541 DOS lists, loads and runs a D64
  surface and saves a file whose directory entry and block decode back from the written GCR; HW_ADDR 1 is unit 9;
  drive B at unit 9 lists its own disk while drive A, off, gives DEVICE NOT PRESENT.

## Performance

`--speed max`, upstream ELF, one run at a time, 2 runs each. Before = `main` at 9b2b7a8 (phase A: TRX64's drive 8
runs without ROM and is not connected); after = this branch. MIPS is the last 500 ms interval.

| Case | Before: MIPS, wall, RSS | After: MIPS, wall, RSS |
|---|---|---|
| M2, no flash, 60 s emulated (drive A off: no ROM) | 129, 130; 11.66 s, 11.65 s; 67.3 MB | 133, 133; 11.38 s, 11.46 s; 67.4 MB |
| 60 s emulated, flash with the drive ROM: drive A on and idle, no disk | 134, 133; 11.37 s, 11.33 s; 67.2 MB | 131, 131; 11.74 s, 11.57 s; 67.5 MB |
| Mount `drive.d64`, `LOAD"BIG",8`, quit 20 s into the load (31.2 s emulated) | 134, 133; 6.10 s, 6.12 s; 67.4 MB | 128, 124; 6.31 s, 6.30 s; 68.1 MB |

- **Drive off** is about 2 % faster than phase A: the parked stand-in runs no 6502, where phase A ran TRX64's
  ROM-less drive.
- **Drive idle** costs about 2.5 % wall time: the DOS idle loop and its timer IRQ.
- **Loading** costs 5-7 % MIPS in the loading interval and 3 % wall over the run. Before, the C64 was waiting for a
  drive that never answered (`SEARCHING FOR BIG`); after, it is `LOADING`.
- **RSS:** at most +0.7 MB (the second `Drive1541` and the GCR surface).

## TRX64 API gaps

Items 1-5 and 9 of the list kept here until 0.4.5 are closed by TRX64 v0.8.8 (Specs 870/871) and the bridge uses the
drive part instead (docs/specs/S27-drives-870.md): power, reset held, stopped, the C64's RESET line (cut in TRX64, the
bridge drives the level), ROM from memory (the whole 32 K), unit 8-11 (the FPGA's range too), and read access to RAM
and ports. Open, and not TRX64's (2026-09-23):

1. **No external disk surface or write hook.** The firmware's GCR goes into `rotation.image`; every drive reset
   builds a new rotation model and re-mounts only TRX64's own disk, so the bridge holds the surface across the calls
   that may reset. Written tracks are found by polling the head, the write mode and the dirty half-track.
2. **Bit rate from the speed zone**, not the firmware's per-track bit time (param word 1 bits 25:16,
   floppy_stream.vhd:50-66): a G64 track whose length differs from its zone's wraps at a different rate. Accepted.
3. **1541 only:** 1571 and 1581 are TRX64's, later.
4. **A new ROM comes into force at power-on only** (870 §4). The firmware changes the ROM with a reset
   (c1541.cc:945, 1132); the bridge turns that reset into off-and-on.

## Known gaps

- **1571/1581:** DRIVETYPE 1/2 keeps drive A off with a one-time notice; MFM tracks, the WD177x path and side 1 are
  not modelled. Extra RAM (RAMMAP bit 7), DISKCHANGE/force ready and the drive sounds are latched only.
- **Drive B** runs as TRX64's position B (S27), but the firmware only builds it when the capability word has
  `CAPAB_DRIVE_1541_2` (bit 2, c1541.cc:1263); the default word lacks it, so it takes `--caps 35640226` today.
  Checked with it: Drive B Settings enabled, `drive.d64` mounted over REST, `LOAD"$",9` and `LIST` show the directory.
  The IEC processor (SoftIEC, printer, UltiCopy) is still T0.
- **Held drive:** a drive in reset releases the bus lines (TRX64 leaves Conf0, 870 §10), where the FPGA drive pulls
  CLK and DATA while its VIA is reset.
- **Written-track detection** runs at every C64 sync (1 ms, and every DMA or cart register access): a write followed
  by two head steps within one sync period would be missed. The 1541 DOS steps from its 10 ms IRQ.
- **Write busy** starts when a sync sees the drive writing, not at the first written bit.
- **The LED** is in `DriveStatus` but no register or UI shows it.

## Regressions

After the change, from an empty run directory (A2 flash as in Setup):

| | Result |
|---|---|
| S14 A2 `smoke-c64-ready.ctl` | `**** COMMODORE 64 BASIC V2 ****`, `38911 BASIC BYTES FREE`, `READY.`; `c64-ready.png` 384×272 |
| S14 A3 `smoke-c64-type.ctl` | ` 42` |
| S14 A4 `smoke-c64-prg.ctl` | `DMA load complete: $0801-`, `Cart got disabled, now restoring.`, `HELLO FROM UE2EMU` |
| S14 A5 `smoke-c64-freeze.ctl` | `Frozen on Bad line.`, no `Hard stop!!!`, the menu in the second dump, the third dump equal to the first |
| M1, `--c64 trx64` / `none` | capabilities banner, no halt; 134 / 227 MIPS |
| M2, `--c64 trx64` / `none` | 60.004 s emulated, `unmapped summary: 0 addresses`, no halt; 134 / 228 MIPS |
| M3 `smoke-menu.ctl`, M4 `smoke-sd.ctl`, both modes | pass |
| `cargo build -p ue2emu --no-default-features`, its tests | builds without TRX64, 49 tests pass |
