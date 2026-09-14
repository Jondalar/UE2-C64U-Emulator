# M4 status — storage: SD card image and flash persistence

Milestone M4 (docs/ARCHITECTURE.md §Milestones): config persists across runs in the flash image, and an SD
image shows up in the file browser. **Reached.** The unmodified firmware (`ultimate.elf`, V1.01 3.15) worked
against the S06 flash model and the S09 SD model as merged; neither `flash.rs` nor `sdcard.rs` needed a fix.
`flash.rs` gained one regression test built from the user-interface page the firmware wrote in these runs.

## Commands

From the repo root (or a worktree, where the firmware lives outside the checkout):

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate
FW="--elf $UE2_FIRMWARE/target/u64ii/riscv/ultimate/result/ultimate.elf --roms $UE2_FIRMWARE/roms"
cargo build --release
rm -rf run

# SD: build the image, then list it in the browser.
scripts/make-sd-image.sh run/sd.img
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --sd run/sd.img \
    --script scripts/smoke-sd.ctl > run/smoke-sd.log

# Flash: run 1 changes and saves a setting; run 2 starts without seeding and reads it back.
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin \
    --script scripts/smoke-flash-1.ctl > run/smoke-flash-1.log
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --no-overlay-ui \
    --script scripts/smoke-flash-2.ctl > run/smoke-flash-2.log
```

Pass criteria as measured for M4, before the control language could assert: each run exits 0 and its log holds
these screen lines. The scripts now check the same texts themselves with `expect`/`expect-console`, and
`scripts/smoke-all.sh` runs them in a fresh temporary directory (`docs/status/tooling.md`):

```sh
grep -q '^SD      SD Card                Ready' run/smoke-sd.log &&
grep -q '^demo.d64                      D64  171K' run/smoke-sd.log &&
grep -q '^hello.prg                     PRG   29' run/smoke-sd.log &&
grep -q '^readme.txt                    TXT   71' run/smoke-sd.log &&
grep -q '^HELLO                         PRG  254' run/smoke-sd.log &&
grep -q "^Writing config store 'User Interface Settings' to flash" run/smoke-flash-1.log &&
grep -q '^Flash   Flash Disk             Ready' run/smoke-flash-2.log &&
grep -q '^|Interface Type         Overlay on HDMI|' run/smoke-flash-2.log &&
grep -q '^|Color Scheme                C128 Style|' run/smoke-flash-2.log && echo M4 PASS
```

The whole sequence was run three times from an empty `run/` (about 7 s, 13 s and 8 s emulated; 170-210 MIPS
on the host) with identical screens.

## `scripts/make-sd-image.sh <path> [sizeMB]`

- **Layout:** raw file of `sizeMB` MiB (default 64, minimum 40), MBR with one FAT32-LBA partition (type 0x0C)
  at sector 2048, volume `UE2SD`.
- **Files:** `hello.prg` (BASIC `10 PRINT"HELLO FROM UE2EMU"`, 29 bytes), `demo.d64` (35 tracks, BAM, disk name
  `UE2EMU DEMO` id `UE`, one PRG `HELLO` at 17/0, all written byte by byte), `readme.txt`.
- **Tools:** stock macOS only, no root.
  - `hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount` exposes the file as `/dev/diskN`
    with slice `s1`; the attaching user owns the nodes.
  - `newfs_msdos -F 32` formats `/dev/rdiskNs1`. On a plain file, `newfs_msdos` prefixes `/dev/` to a
    name without a slash and cannot format a partition inside the file.
  - `diskutil mount -mountPoint <tmp>` mounts it privately, never under `/Volumes`.
- **Clean copy:** `.fseventsd/no_log` and `.metadata_never_index` keep fseventsd and Spotlight away during
  the copy. `cp -X` with `COPYFILE_DISABLE=1` writes no AppleDouble files. The markers are removed before
  unmounting. The firmware would hide dot files anyway (filemanager.cc:556).
- **Not used:** mtools (not installed here) and `diskutil partitionDisk`, which auto-mounts under `/Volumes`.

## Results

### SD (`scripts/smoke-sd.ctl`)

Firmware console: `SD V2.00`, then on entering the card `MBR Start: 2048 Size: 129024 Type: 12`,
`3 children fetched from SD.`, and `2 children fetched from demo.d64.`

```
SD      SD Card                Ready          <- root, before entering
demo.d64                      D64  171K       <- /SD/
hello.prg                     PRG   29
readme.txt                    TXT   71
UE2EMU DEMO       UE 2A       VOLUME          <- /SD/demo.d64/
HELLO                         PRG  254
```

Firmware writes to the card work too. This was checked by hand, not scripted, because it changes the image.
- **Steps:** in `/SD/`, F5 → Create → D64 Image, name `blank`.
- **Firmware:** `Result of save: 0.`, and `/SD/` lists `blank.d64 D64 171K`.
- **Host:** `fsck_msdos -n` on the partition reports no errors and 4 files. The mounted `blank.d64` has the
  BAM name `BLANK`. That is 174 848 bytes through CMD24 and FAT updates.

### Flash (`scripts/smoke-flash-1.ctl`, `scripts/smoke-flash-2.ctl`)

**Run 1** (fresh `run/flash.bin`):
- S06 seeds config page 0 with `2E 4E 45 47 08 02 01 01`.
- Boot: the firmware claims pages 1-10 for its other stores (`Page: 1 done.` … `Page: 10 done.`).
- **Key path** (from the firmware sources, cited in the script header):
  1. F2 opens the config browser.
  2. 9 × DOWN reaches "User Interface Settings" (separators are skipped); RIGHT enters it.
  3. 2 × DOWN reaches "Color Scheme"; RETURN opens its choices.
  4. `c` seeks "C128 Style"; RETURN takes it.
  5. LEFT, LEFT leaves the store and the browser.
  6. "Save changes to Flash?" → RETURN (Yes). Console: `Writing config store 'User Interface Settings' to flash..Page: 0 done.`
  7. RUN/STOP hides the overlay (`run/flash1.png` is blank).
  8. `quit`: the flash image is flushed on drop.

Page 0 afterwards is the whole store in definition order:

```
2E4E4547 | 08 02 01 01 | 0D 02 01 00 | 0E 02 01 02 | 06 02 01 00 | 07 03 00 | 0A 02 01 01 | 0B 02 01 01 |
0C 02 01 00 | 0F 02 01 01 | 10 02 01 01 | FF
```

That is ITYPE = 1 (overlay), NAVIGATION = 0, COLORSCHEME = 2 (C128 Style), START_HOME = 0, HOME_DIR = "",
CFG_SAVE = 1, ULTICOPY_NAME = 1, FILENAME_OVERFLOW_SQUEEZE = 0, TEMP_AUTO_CLEANUP = 1,
TEMP_USE_CACHE_SUBFOLDER = 1 (userinterface.h:36-46). The test
`flash_overlay_ui_seed_keeps_the_firmware_written_store` pins this layout.

**Run 2** (same file, `--no-overlay-ui`, so nothing is seeded):
- The menu button opens the overlay, so the firmware-written ITYPE = 1 survived on its own.
- The root shows `Flash   Flash Disk             Ready`.
- User Interface Settings shows `Color Scheme                C128 Style`.

## Known gaps

- **Self-checking scripts (resolved):** `smoke-sd.ctl`, `smoke-flash-1.ctl` and `smoke-flash-2.ctl` assert their
  screens and console lines and exit non-zero on a mismatch; `scripts/smoke-all.sh` runs them. The `grep` block above
  is kept as the M4 record.
- **`smoke-flash-1.ctl` expects a fresh flash** (at least one where Color Scheme is not already C128 Style).
  Otherwise no save popup appears, and its RETURN opens the browser's context menu instead; the flash still
  holds C128 Style, so run 2 still passes. Run the sequence from an empty `run/`.
- **Image script needs DiskArbitration:** `make-sd-image.sh` needs `hdiutil`/`diskutil`, i.e. a normal macOS
  login session. It has no mtools path.
- **Flash write-back window:** the image is written about 0.5 s of wall time after a program/erase, and when
  the machine drops. Killing the process (Ctrl-C in headless mode, `kill`) inside that window loses the last
  save. `quit` and closing the window are safe.
- **No SD hot-plug:** the image is opened once at machine construction and card detect is fixed. There is no
  insert/eject command, and the image must not be modified on the host while the emulator runs.
- **Colours not in the text dump:** `screen` shows no colours or reverse video. The colour-scheme change is
  checked through its menu text, not its effect on the palette.
- **Flash Disk not written:** the `/Flash` FAT volume (flash block device) was listed as Ready but not browsed
  or written in these runs.
