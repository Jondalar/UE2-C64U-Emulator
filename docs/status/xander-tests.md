# Xander Mol's five Ultimate projects in UE2

How far the five released programs of https://github.com/xahmol get in UE2, and which emulator gap stops each one.
A survey, not a fix: nothing in `crates/` was changed for it.

Two passes, both **2026-09-16**:

- **First pass**, TRX64 0.6.0 pin (`2b145c9`), `main` at `0180180`: no C64-side REU, CIA1 TOD frozen.
- **Second pass**, TRX64 0.7.1 pin (`5f93646`), `main` at `92ef812`: the REU answers at `$DF00` (`docs/status/reu.md`)
  and TOD runs (`docs/status/c64.md`, "CIA TOD"). Everything below marked **(2nd pass)** is from this one; the
  first-pass findings are kept where they still explain a wall, and struck through where they are gone.

Firmware: `firmware/1541ultimate` V1.01 3.15 (`v3.15-9-gb617777c`). Every run is

```sh
target/release/ue2emu run --headless --speed max \
    --firmware firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf \
    --roms firmware/1541ultimate/roms --flash <copy of run/flash.bin> --c64-roms \
    --usb-dir run/xander/r2/<share>[,ro] --usb-dir-work run/xander/r2/w-<n> \
    [--net user --hostfwd tcp:23xx:23] --script run/xander/r2/ctl/<script>.ctl
```

The programs, their control scripts, logs and PNGs live under `run/xander` (gitignored): `artefacts/` the unpacked
releases, `share*/` the USB sticks the firmware sees (release layout: `idi8b/<project>/`), `r2/` the second pass
(`ctl/`, `log/`, `png/`), `shots/` the 2× screenshots this survey refers to. Releases were left byte for byte as
shipped; the only additions to a stick are `uboot64.cfg` (see UBoot64) and copies of the shipped
`config/Heartbeat-U64E2.cfg` under the auto-load names `udemo2026.cfg` and `heartbeat-demo.cfg`, which is what the
firmware's "Load Settings" does by hand (`filetype_prg.cc:212`, `filetype_crt.cc:72` call
`ConfigIO::S_load_associated_config`).

## Summary

| Project | How far it gets (2nd pass) | Stopped by |
|---|---|---|
| UBoot64 v3.0.1 | **Boots to its menu**: REU 16 MB detected, slot file written, filebrowser over UCI, information and configuration screens all work | — (reading an existing `DMBSLT.CFG` stalled; fixed in TRX64 0.7.2, gap 2) |
| mandelbrot-upic v1.0.3 | **Runs and draws its picture** | — |
| UltimateDemo2026 v1.0.1 | **Runs the whole demo**: UCI, REU 16 MB, turbo 64 MHz; every scene draws; plays its MOD since S16 | — |
| heartbeat-demo v1.0.1 | Hardware detection all OK; plays its song since S16 | — |
| GeoUTools v1.1 | *(3rd pass, §5)* **GEOS 1.2 boots to its DeskTop** in 30 s emulated; GeoUMount and GeoUConfig run and their UCI calls answer | No pointing device reached the C64 then, so the DeskTop could not be driven (mouse and joysticks exist since S32/S36, not re-run); GeoUMount finds no drive target and GeoUTime crashes after its NTP call |

Three emulator gaps, in the order they cost the most:

1. **Ultimate Audio DMA `$DF20-$DFFF` — built since S16; the wall is down for heartbeat-demo.** The diagnosis here
   was right: it was the registers behind the window, not the capability word. They are modelled now
   (`docs/specs/S16-ultimate-audio.md`, `docs/status/sampler.md`) — eight voices, both register faces, the mixer and
   the IRQ. **heartbeat-demo** reads `Audio : [ OK ]  v16`, loads its song (`Song : [ OK ] tempo 88, 8 SIDs`) and
   **plays it**: an `--audio-wav` capture carries 40 s of sound at about 46000 peak-to-peak of 65535, where the
   program used to fail the check and return to BASIC. **UltimateDemo2026** runs as well: it passes detection, loads
   its MOD over UCI into the REU, draws its scenes and plays the MOD (headless: sound from 20 s on, the tunnel scene
   at 180 s; also watched live). It drew black after S16 until TRX64 was pinned at `1ce84b0` and S17 landed; which
   change fixed it was not isolated (S16 §6).
2. **A UCI DOS read of an existing 24 KB file stalled near the end — fixed in TRX64.**
   UBoot64 writes `DMBSLT.CFG` (24480 B) on its first run and reaches its menu; on every later run it reads the same
   file back and stops at `Reading slot data to 24261` / `24270` / `24279` / `24285` / `24289` / `24324` — within
   220 bytes of the end, at a different byte each run.

   **Cause.** `Uci::c64_read` (`trx64-core/src/uci.rs`) advances the response and status pointers by
   `stalled_on_bus + 1` per C64 read. A read that lands in a badline-stolen phase therefore skips bytes, and
   `full.rs:665-666` passes that count on the plain bus too, not only on `full_sc`. The C64 ends up short, re-issues
   `DOS_CMD_READ_DATA` for the full length *without* a new `OPEN_FILE`, the file is at EOF, the firmware answers
   with a zero-length last part, and the C64 waits for ever. The RISC-V sits in the idle task: nobody is running.
   The varying stop byte is the badline dependency showing.

   **Verified**, not guessed: the firmware-side register trace shows the last exchange complete correctly
   (`STATUSBYTE 0x11` → command accepted → `RESPONSE_LEN 0` → validate without the "more" bit), and a copy of the
   TRX64 checkout with the advance changed to exactly one byte per read gets UBoot64 to its menu
   (`F1 Filebrowser … Make your choice.`) with nothing else altered. Reported to TRX64 with that repro and fixed there
   (`db882f4`, 0.7.2: one completed read consumes one byte). With the pin at `1ce84b0` UBoot64 reaches its menu on a
   stick that already holds `DMBSLT.CFG`; the emulator works around nothing.

   Unblocks: UBoot64's menu slots, which are the program's whole point.
3. **"Run Cart" leaves the keyboard with the firmware menu.** After the browser's `Run Cart` the console never
   prints `MENU HIDE / EXIT.` (the `Run` path for a PRG always does), so C64 keys reach the firmware UI instead of
   the cartridge — `key f2` built the firmware's *config* browser (`Creating config menu...`,
   `Unhandled context key: 1FC`) while UBoot64's own F2 did nothing. Pressing the menu `button` once after the cart
   starts hands the keyboard over and UBoot64's whole UI then works (§1). Confirmed a second time on
   heartbeat-demo (`docs/status/sampler.md`): its "Press any key to start playback" ends in `READY.` without the
   button, and starts the song with it. Whether the firmware is meant to hide the
   menu for a cartridge was not established here; it is recorded as observed, with the workaround.

**A fourth gap, found when GEOS was run (§5): no pointing device reached the C64.** GEOS booted and then sat there —
ten key presses changed 0 pixels of its DeskTop. Closed since: the USB mouse (`--usb-mouse`, the `usbmouse` command,
S32) and the joystick commands (`joy`, S36) reach the control ports; GEOS has not been run with them. A smaller one
from the same runs, `--usb-dir` refusing to sync back a stick that holds one file, is fixed (§5).

Carried over unchanged from `docs/status/c64.md` ("CIA TOD"), neither worked around: **TOD runs 5.3 % slow while the
screen is on**, and **the mains frequency comes from CRA bit 7 rather than from the machine**. Nothing in this
survey depends on either — every wait these five programs make is a timeout, not a measurement — but
`turbo_detect()`'s `benchmark_delay()` is exactly the kind of measurement that would be 5 % off.

**Gone since the first pass:** ~~no C64-side REU~~ (all three REU checks now pass, §1-§3) and ~~CIA1 TOD never
advances~~ (mandelbrot-upic draws, §4).

Screenshots (2×, nearest-neighbour) are under `run/xander/shots/<project>/`; each section names the ones that matter.

## 1. UBoot64 v3.0.1

`run/xander/artefacts/UBoot64-v2/uboot64.crt`, 48 K, CRT hardware type 3, 3 × 16 K chips at `$8000`, exrom 0
game 0. Started from the file browser with "Run Cart"; `--cart-slot` was not needed and was not used: the firmware's
own CRT loader takes the file. `run/xander/share/uboot64.cfg` sits next to the CRT
(`RAM Expansion Unit=Enabled`, `REU Size=16 MB`, `Command Interface=Enabled`) and "Run Cart" auto-loads it —
console `Begin of cart init: Type: 19. REU: 01. REU_SZ: 07, UCI: 01 (DF18), Mode: 04, Sampler: 00`.

**First pass: `No REU detected.`** `src/main.c:346-361` probes the REU with a marker byte at page 0 and calls
`errorexit()`. Everything the program is for sat behind that line.

**Second pass: it boots.** `ctl/uboot-01.ctl`, `log/uboot-01.log`:

```
UBoot64:  Boot Menu for Ultimate devices
Starting....        v3.0.1-20260913-1321

Detecting and reading...
Ultimate Command Interface detected.
Storage found: /usb0/
DOS version: ultimate-ii dos v1.2
REU detected, size: 16384 KB
Writing slot data at 21000.
```

then the menu: `Welcome to your C64. 2026/09/16 17:38:01`, `F1 Filebrowser  F2 Information  F3 Edit/Order/Del
F5 Configuration  F7 Quit to BASIC`. The stick gains `DMBSLT.CFG`, 24480 bytes, written back to the host directory
(`usb-dir port 1 (…/share-uboot): wrote DMBSLT.CFG`), next to the `DMBCFG.CFG` the first pass had already created.

**What works** (`ctl/uboot-06.ctl`, `log/uboot-06.log`, after one `button` press to take the keyboard off the
firmware menu — gap 3 above):

- **F1, the filebrowser over UCI.** `/USB0/` lists every file and directory of the stick with the sidebar's full key
  legend, `UCI mode` in the corner (`shots/uboot64/04-filebrowser-uci.png`). `DEL` walks up to the Ultimate's own
  root and lists `SD`, `Flash`, `Temp`, `USB0` as directories — the native filesystem over UCI, not the C64's idea
  of a disk (`shots/uboot64/05-uci-root-sd-flash-usb.png`).
- **F2, information**: the U64 logo splash, then the credits page (`shots/uboot64/06-info-credits.png`).
- **F5, configuration**: `NTP time update settings: Update on boot toggle: On`, `Offset to UTC in seconds: 7200`,
  `NTP server hostname: pool.ntp.org`, verbose startup, auto-boot timeout, SoftIEC root partition
  (`shots/uboot64/07-configuration-ntp.png`).
- **F7 from each screen returns to the menu.** 41.4 s emulated for the whole tour.

**Where it stopped in the second pass: reading an existing `DMBSLT.CFG`** (fixed since, Summary gap 2). Every run on a stick that already has the file stalls at

```
REU detected, size: 16384 KB
Reading slot data to 24289.
```

and stays there (`ctl/uboot-04.ctl` gave it 180 s emulated — `shots/uboot64/03-stall-reading-slot-data.png`). The
counter stops at 24285, 24289 and 24324 in three runs against the same 24480-byte file, so the read does not
complete and does not fail the same way twice. Delete the file and the very next run writes a fresh one and reaches
the menu (`ctl/uboot-05.ctl`, `ctl/uboot-06.ctl`, `ctl/uboot-07.ctl`, three for three). **It is not the network:**
the stall is identical with and without `--net user`.

**The network itself is up.** With `--net user --hostfwd tcp:2399:23` the firmware takes its DHCP lease
(`net: libslirp 4.9.4 user network, guest 10.0.2.15 by DHCP`, console `Status update IP = 10.0.2.15`) and its own
NTP client answers: `--> Time Received: 1789580898 (current TZ = CEST-1CET,M3.2.0/2:00:00,M11.1.0/2:00:00)`. UBoot64
then shows local time on its menu (`19:48:20` with the network, `17:47:07` = UTC without it). **UBoot64's own NTP
sync could not be told apart from the firmware's**: its verbose startup prints no NTP line in either case, and its
own offset (7200) lands on the same local time the firmware's timezone already produced. Recorded as observed, not
diagnosed.

Screenshots: `shots/uboot64/01-startup-detection.png` (the detection block with `REU detected, size: 16384 KB`),
`02-main-menu.png`, `03-stall-reading-slot-data.png`, `04`-`07` above.

## 2. UltimateDemo2026 v1.0.1

`idi8b/ultdemo2026/{udemo2026.prg,4ev.mod}`, "Run" from the browser with the shipped Elite-II preset under the
auto-load name `udemo2026.cfg`. Console: `Speed regs: 01 89`, `Begin of cart init: Type: 41. REU: 01. REU_SZ: 07,
UCI: 01 (DF18), Mode: 04, Sampler: 00`, `DMA load complete: $0801-$97C8`.

**First pass: `REU : [Fail] Not detected` right after `UCI: [OK]`.**

**Second pass: three of the four checks pass** (`ctl/udemo-final.ctl`, `log/udemo-final.log`,
`shots/ultimatedemo2026/01-hardware-detection.png`):

```
            UltimateDemo2026
Hardware Detection  v1.0.1-20260603-2309

Waiting for Ultimate firmware...
  UCI   : [ OK ]  UCI Ok
  Type  : ultimate 64-ii
Checking REU...
  REU   : [ OK ]  16 MB
Checking turbo mode...
  Turbo : [ OK ]  64 MHz
Checking Ultimate Audio...
  Audio : [Fail]  Module not found
  -> F2 > C64/Cart settings > Audio

Detection complete.

Press any key to start the demo.
```

**The audio check is the only failure, and it does not stop the demo** — it waits for a key, and the first pass
never pressed one. With a key press the whole demo runs, silently:

| t (emulated) | Screen |
|---|---|
| 3 s | Gears scene, `3 MHz - warming up` (`shots/ultimatedemo2026/02-gears-3mhz.png`) |
| 20 s | the same gears, `64 MHz ULTIMATE SPEED!!` — the speed ramp works end to end (`03-gears-64mhz.png`) |
| 60 s | the shaded ball on its rotating wireframe floor (`04-ball-scene.png`) |
| 120 s | the multicolour plasma (`05-plasma-120s.png`) — brown/yellow/black here, where the release's own screenshot is black/cyan/purple/yellow; observed, not diagnosed |
| 180 s | the PETSCII rose, spinning (`06-flower-petscii-180s.png`) |
| 240 s | the sprite scrolltext over the full-colour plasma (`07-scroller-plasma-240s.png`) |

262.7 s emulated, 24 MIPS. The end screen sits past the capture window and was not reached.

**The exact check that fails.** `audio_detect()` (`include/audio.c`, manual `ULTIMATEAUDIOMANUAL.md`) probes the
7-channel DMA block at `$DF20-$DFFF`. The firmware only maps that window under `CAPAB_SAMPLER`
(`c64.cc:319-323`), bit 21 / `0x00200000` (`itu.h:70`), and this machine's word is
`*** FPGA Capabilities: 34C40222 ***` — that bit is clear, so the cart-init line says `Sampler: 00`. **Setting the
bit does not help**: with `--caps 34E40222` (`log/udemo-caps.log`) the firmware prints `Sampler found in FPGA...
IO map: Enabled!` and `Begin of cart init: Type: 41. REU: 01. REU_SZ: 07, UCI: 01 (DF18), Mode: 04, Sampler: 01`,
and the C64 still reads `Audio : [Fail] Module not found`. The gap is the register block itself, not the
capability word. `4ev.mod` is therefore never loaded into the REU by the demo.

**Speed (TRX64 Spec 856).** In its 64 MHz turbo the demo ran below realtime at TRX64 `1ce84b0`: 0.85, 0.77, 0.72
and 0.98 of realtime in four 15 s windows (headless, realtime, audio on, Apple M4), with the emulation thread at
100 % of one core and the audio crackling from underruns. About 90 % of that thread was the C64 side, most of it
work TRX64 did once per 6510 instruction rather than once per PHI2 cycle. Spec 856 (`a6e0465`) runs instructions that
only touch RAM back to back inside one PHI2 cycle. The same windows now hold realtime (1.000, 0.999, 0.999, 0.997)
at 69-97 % of one core, and the audio is clean. `TRX64_TURBO_FASTPATH=0` switches the fast path off. TRX64 Spec 857
then made the CIA alarm a comparison, which took the CIA update (12-16 %) out of the profile: 66/74/59/81 %. With
the idle skip (S19) and the SID worker (S20) the four windows end at **58/65/50/72 %** on the shipped 0.3.2
build.

**Idle skip (S19).** In the demo the firmware idles: 0.7 C64-window accesses a second and no writes into DDR the C64
uses. With the FreeRTOS idle loop fast-forwarded, 97.6 % of the RISC-V instructions are skipped (140 M executed, 5 840 M
skipped over 239 s). The same four windows stay at realtime, at 45, 52, 35 and 36 % host CPU by `ps` (69-97 % before).
At `--speed max` with audio off, 231 s of the demo take 129.0 s CPU instead of 153.3 s with `--no-idle-skip`, 16 % less.

## 3. heartbeat-demo v1.0.1

`idi8b/heartbeat-demo/{heartbeat-demo.prg,maniac.reu,Knight Rider Theme.reu}` with the shipped
`config/Heartbeat-U64E2.cfg` under the auto-load name (`ctl/heartbeat3.ctl`, `log/heartbeat3.log`). The preset is
applied in full: `Effectuating settings of store 'Audio Mixer' after loading.`, `Speed regs: 01 89`,
`Begin of cart init: Type: 41. REU: 01. REU_SZ: 07, UCI: 01 (DF18), Mode: 04, Sampler: 00`,
`DMA load complete: $0801-$6B7E`.

**Second pass: the same three checks pass** (`shots/heartbeat-demo/01-hardware-detection.png`) —
`UCI [ OK ]`, `Type ultimate 64-ii`, `REU [ OK ] 16 MB`, `Turbo [ OK ] 64 MHz`, `Audio [Fail] Module not found`,
`Press any key to continue.`

**Where it stopped in the second pass: the key press returned it to BASIC** (it plays since S16, Summary gap 1). Four seconds after `key space` the screen is cleared with
`READY.` and nothing else (`shots/heartbeat-demo/02-exits-to-basic.png`); 143 s emulated total, no further output.
The demo does not continue without Ultimate Audio, so **it never attempts its own song load**: the 8 SIDs, the 7
DMA channels and the tick IRQ stay untested. `Sampler: 00` again, for the reason in §2.

**The REU load path itself is fine, from the other side.** A mis-navigated run selected
`Knight Rider Theme.reu` in the firmware browser instead of the PRG and the firmware's own action ran to completion:
`Action set was: Load into REU`, `REU Select: 5201`, `REU Load.. Knight Rider Theme.reu  UI = 0031EC10
FSize = 2621440 (OK!)` — 2.5 MB into the REU over the firmware's loader (`log/heartbeat2.log`). That is not the
program's `uii_load_reu` path, but it is the same DDR.

## 4. mandelbrot-upic v1.0.3

`idi8b/mandelupic/{mandelupic.prg,mandelupic.cfg}`, "Run" from the browser (`ctl/mandel.ctl`, `log/mandel.log`).
Our TOD regression test, and it still passes.

Console unchanged from the first pass: `Speed regs: 01 89` (`setCpuSpeed`, `u64_config.cc:1625-1636`: turbo
registers on, badlines suppressed, speed index 9 = 16 MHz), `Begin of cart init: Type: 41. REU: 00. REU_SZ: 04,
UCI: 01 (DF18), Mode: 04, Sampler: 00`, `DMA load complete: $0801-$FFEE`. 188.3 s emulated, 18-20 MIPS (the C64 is
at 16 MHz turbo, so a host cycle buys less emulated time than the 130 MIPS of a 1 MHz run).

The picture is a **Upic border-colour raster**, so `c64screen` shows the BASIC text underneath it and only the PNGs
carry the result:

| t (emulated) | Frame |
|---|---|
| 10 s | one colour — nothing drawn yet |
| 30 s | the fractal complete in the release's own blue/orange gradient (`shots/mandelbrot-upic/01-fractal-complete-30s.png`) |
| 90 s | the same picture: 1040 of 104448 pixels differ from the 30 s frame |
| 180 s | the same picture in a **greyscale** ramp — 64900 pixels differ (`shots/mandelbrot-upic/02-greyscale-180s.png`) |

**The palette change takes effect** (`uii_setpalette` over UCI reaches the U64 palette); the greyscale frame is the
same anomaly the first pass saw and is still **observed, not diagnosed** — no key was pressed, and nothing in the
console marks a palette call.

**Why it used to hang** (first pass, TRX64 ≤ 0.7.0): CIA1 TOD was frozen, and `uii_wait_for_uci(5)`'s loop
`while (!uii_detect() && cia1.tods < timeout_seconds);` could never leave. Measured then directly from BASIC — set
the clock to zero, let 10 s pass, read back ` 0` seconds and ` 0` tenths (`ctl/16-tod.ctl`, `out/16-tod.log`).
`Cia::tick` only bumped `tod_prescaler`; the BCD registers at `$DC08-$DC0B` never advanced. Fixed in TRX64 0.7.1.

## 5. GeoUTools v1.1 and GEOS 1.2

`GeoUTools.d64` (171 K) and `GeoUTools.d81` (800 K). The disk holds the three GEOS applications and two GEOS
documents and nothing else. Re-run in the second pass (`ctl/geos.ctl`, `log/geos.log`, 31 s emulated) with the same
result as before: with `1541.bin` installed from the browser first ("Set as 1541 ROM", the `docs/status/drive.md`
path — console `Copying 1541.bin to /flash/roms`, `Tracks: 35. Errors: No`) the disk mounts on drive A and the C64
reads it (`shots/geoutools/01-directory-listing.png`):

```
LOAD"$",8
SEARCHING FOR $
LOADING
READY.
LIST
0 "G##UT####       " 1# 2A
63   "G##UM####"        SEQ
58   "G##UT###"         SEQ
55   "G##UC#####"       SEQ
139  "G##UT#### UK"     USR
152  "G##UT#### DE"     USR
194 BLOCKS FREE.
```

(The `#` are lower-case letters: `c64screen` maps a ROM upper-case font as screen codes, `docs/status/c64.md`
"Known gaps".) On a flash without a drive ROM the mount still succeeds but the drive stays off and the C64 answers
`?DEVICE NOT PRESENT ERROR`.

**GEOS 1.2 boots.** A system disk exists now, so the GEOS side was run in a **third run set**, `run/xander/r3/`
(`ctl/`, `log/`, `png/`, one USB-stick share per run, the same command line as above).
`Geos V1.2 (1985)(BS).d64` — 29 files, BAM signature `GEOS format V1.0`, 56 blocks free — was copied onto the stick
as `GEOS12.D64`, mounted on drive A the same way as the GeoUTools disk, and started from BASIC with
`LOAD"GEOS",8,1`. It needs no `RUN`: the boot file starts itself. From the LOAD (`ctl/geos-02.ctl`,
`log/geos-02.log`):

| t (emulated, from the LOAD) | Screen |
|---|---|
| 10 s | `BOOTING GEOS...` on the boot screen (`shots/geos/01-boot-screen.png`) |
| 20 s | the GEOS desktop pattern, nothing drawn on it yet |
| 25 s | the menu bar, the printer and the waste basket |
| 30 s | the DeskTop: window `GEOS12`, `29 files  152 K bytes used  14 K bytes free`, eight file icons (`shots/geos/02-desktop.png`) |

and nothing changes after that. The whole run — mount, boot, three minutes of idle DeskTop — is 226.9 s emulated in
46 s wall at 125 MIPS. **`c64screen` is useless here:** GEOS runs its own RAM font, so the dump is 40×25 `?`
(the documented guess in `docs/status/c64.md`, "Known gaps"). Only the PNGs carry the result.

**The DeskTop cannot be driven: no pointing device reaches the C64.** A sweep of ten keys (`down`, `up`, `left`,
`right`, `space`, `a`, `return`, `f1`, `runstop`, `home`, each held 1-2 s emulated) changes **0 pixels** —
`shots/geos/03-desktop-after-key-sweep.png` is byte for byte `02-desktop.png`. So keys do not bleed into the
joystick lines either, and no icon and no menu could be opened. At the time the control language had no joystick
or mouse input and the USB stack no mouse; both exist now (S32, S36). RESTORE still has no control command; it is
Page Up in the window.

**Getting GeoUTools to GEOS.** Swapping the mounted image is honest but pointless: the D64 does swap, and the
DeskTop then has to be told to read the new disk, which is a mouse click. So the tools were copied onto a working
copy of the system disk instead. `scripts/d64tool.py` writes plain PRGs only, while a GEOS file also needs its info
block and the GEOS directory fields, so `run/xander/r3/geoscopy.py` (new, in the gitignored run tree) copies those.
`GEOSWORK.D64` is the system disk with geoPaint and geoWrite deleted for room (56 blocks free, the three tools need
179) and GeoUMount, GeoUTime and GeoUConfig copied in; the DeskTop draws them with their own icons
(`shots/geos/04-desktop-with-geoutools.png`), so file, info block and icon all arrived intact.

**The settings GeoUTools needs, set once through the menu and saved** (`ctl/cfg-05.ctl`): F2 → Memory Configuration
→ `RAM Expansion Unit  Enabled`, `Size  16 MB`, `Command Interface  Enabled`
(`shots/geos/09-firmware-uci-reu-enabled.png`), console `Writing config store 'C64 and Cartridge Settings' to
flash..Page: 3 done.`; every later boot on that flash prints `Begin of cart init: Type: 00. REU: 01. REU_SZ: 07,
UCI: 01 (DF18), Mode: 04, Sampler: 00`. The key path is the one `scripts/smoke-flash-1.ctl` documents: RIGHT enters
a store, RETURN opens an enum. `UltiDOS: Allow SetDate` was left alone — GeoUTime never reaches the clock (below).

**GEOS 1.2's DeskTop ignores auto-exec files.** GeoUTime is GEOS file type 14, the auto-exec type, and did not start
at boot; flagging GeoUMount 14 as well changed nothing (`ctl/geos-04.ctl`, the frame is pixel-identical to the plain
DeskTop). That is the 1985 DeskTop, not the emulator. To start a tool at all, each one was stored under the name
`DESK TOP`, which is what the GEOS KERNAL loads after the boot — a disk-side hack, recorded here so that nothing
below reads as "started from the DeskTop".

**GeoUMount runs, and its UCI side works** (`ctl/geos-05.ctl`, `shots/geos/05-geoumount-uci-browser.png`): menu bar
`GEOS | Save REU`, `ID: ULTIMATE-II DOS V1.2`, and its file browser lists `SD`, `Flash`, `Temp` and `USB0` as
directories. `uii_detect()`, `uii_change_dir_home()` and the DOS listing all answer from behind GEOS's own drive
layer. **What it finds no trace of is a mount target:** `Target is drive A`, and `Drive A` to `Drive D` all say
`No target`.

**GeoUConfig states the verdict itself** (`ctl/geos-06.ctl`, `shots/geos/06-geouconfig-detection.png`):

```
GeoUMount config data:
Detection override: No
Autodetection of valid drives succeeded.
Drive A: No target
Drive B: No target
Drive C: No target
Drive D: No target
Target drive: A

GeoUTime config data:
Hostname: pool.ntp.org
UTC offset: 3600
Update from NTP: Enabled
Verbose: Disabled
```

The firmware therefore answers the 3.10f+ device-info command (`Checkcommandsupport()` reads UCI status `21`,
`src/mount_common.c:120-128`) and the detection runs to its end — and matches nothing. `SetValidDrives` takes a
drive only when GEOS's own `DRIVETYPES` entry for it is below 4 *and* the UCI device info reports that ID as an
Ultimate drive; GEOS 1.2 predates the `DRIVETYPES` table GEOS 2.0 keeps, and the Ultimate's own emulated A/B drives
are its Software IEC drives, which were still T0 then (built since S30, not re-run). Observed, not diagnosed further.

**A GEOS RAM drive was therefore never reachable** — and would not have been anyway: GEOS 1.2 has no REU RAM drive
(that is GEOS 2.0 with Wheels or MegaPatch), and GeoUMount's mount and `Save REU` paths need a valid target. The REU
half of GeoUTools stays untested, by GEOS's age rather than by the emulator.

**GeoUTime crashes after the NTP exchange.** Started the same way it dies in a GEOS `System error near $30C6`; with
verbose switched on in its own `GeoUTimeDat` (byte 92 of the config buffer, `src/time_common.c:127`) the window
shows `Connecting t…`, `Sending NTP…`, `Reading resu…`, `UNIX epoch:` and then `System error near $30E0`
(`shots/geos/07-geoutime-verbose-progress.png`, `08-geoutime-system-error.png`). The firmware's own NTP is up in the
same run (`--> Time Received: 1789582710`). Whether this is a program built for GEOS 2.0 meeting GEOS 1.2, or
something in the UCI network path, was not pinned down.

**Writing to the disk from GEOS works.** Both GeoUTime runs created `GeoUTimeDat` (2 blocks, GEOS type 7) in the
mounted D64: console `Writing back binary track 18...`, `Writing back binary track 32...`, and the file is in the
directory of the image the run kept (`run/xander/r3/out-time.d64`, read out of the stick image with
`d64tool.py sd-get`).

**A second emulator gap, from that last step: `--usb-dir` could not sync back a one-file stick.** The changed D64
never reached the host directory:

```
usb-dir port 1 (…/share-time): REFUSED: this sync would delete or overwrite 1 of 1 files on the host
(limit: 25 % or 50); nothing synced, the image is kept; check the stick, then run usb-sync --force
```

The guard counted an overwrite as destructive, so with a single file on the stick every guest change was 100 % of
it. **Fixed:** the guard now counts deletions only (`docs/status/usb-dir.md`), which is what it is named for and
what the trash exists for. At the time of the run the way out was `usb-sync --force`; the evidence above was read
out of the kept image instead.

Screenshots (2×, nearest-neighbour) under `run/xander/shots/geos/`: `01-boot-screen.png`, `02-desktop.png`,
`03-desktop-after-key-sweep.png`, `04-desktop-with-geoutools.png`, `05-geoumount-uci-browser.png`,
`06-geouconfig-detection.png`, `07-geoutime-verbose-progress.png`, `08-geoutime-system-error.png`,
`09-firmware-uci-reu-enabled.png`.

## What the projects' own documentation says about the firmware

- **The associated-config mechanism is real and is what these releases rely on.** A `.prg` or a `.crt` started
  from the browser auto-loads `<name>.cfg`, else `<name>.usr`, from the same directory
  (`filetype_prg.cc:186-188, 212-213`, `filetype_crt.cc:72`, `ConfigIO::S_load_associated_config`). Every store
  in the file is effectuated at once — the console prints one line per store — so a test can set turbo, UCI and
  the REU per program without touching the menu. `mandelupic.cfg` is 150 bytes and does exactly that.
- **`$D031` and the speed table.** `TURBOCONTROLMANUAL.md` §4: bits 3-0 speed index, bit 7 badline mask, and
  `$D031` reads `$FF` when the turbo registers are not enabled — the detection every U64 program uses. Index 15
  is 48 MHz on a U64/Elite-I and 64 MHz on an Elite-II/C64U and software cannot tell them apart. That matches
  TRX64's two tables (`vic.rs:698-716`) exactly, and UltimateDemo2026's gears scene now walks the whole ramp
  from 1 MHz to `64 MHz ULTIMATE SPEED!!` on screen.
- **CIA1 TOD is the only usable clock at turbo speed** (`TURBOCONTROLMANUAL.md` §2): CIA timers and the raster
  counter are clocked at the CPU frequency on the U64, so they carry no speed information; TOD runs off the mains
  frequency. Any U64 program that measures time therefore depends on it.
- **Firmware 3.15 behaviour UBoot64 documents from hardware**: UCI can be enabled by the cartridge itself, no menu
  setting needed (README v3.0.0); SoftIEC grew CMD-HD-style partitions, and a partition created over UCI lives in
  RAM only until the user saves it (v3.0.1); the SoftIEC DOS parser was rewritten in 3.15, changing what a
  partition listing's name field contains and how "go up one directory" must be sent; and the firmware treats a
  single leading slash in `cd:` as relative, only `cd://` as absolute.
- **A C64U firmware bug worth knowing** (`heartbeat-demo/README.md`): USB drives randomly disconnect while
  Ultimate Audio channels are in use, which is why that demo recommends the internal SD slot.
