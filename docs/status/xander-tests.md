# Xander Mol's five Ultimate projects in UE2

How far the five released programs of https://github.com/xahmol get in UE2, and which emulator gap stops each one.
A survey, not a fix: nothing in `crates/` was changed for it.

Binary: `target/release/ue2emu` built at the TRX64 v0.6.0 pin (`2b145c9`), run while `main` was at `0180180`. The
pin has since moved to 0.7.1 (`5f93646`), where the CIA TOD clock is fixed; the frozen-TOD findings below were
measured against `2b145c9`, `69c9b30` and `f370a56`, in which the CIA code is byte-identical.

Firmware: `firmware/1541ultimate` V1.01 3.15 (`v3.15-9-gb617777c`). Every run is

```sh
target/release/ue2emu run --headless --speed max \
    --firmware firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf \
    --roms firmware/1541ultimate/roms --flash <copy of run/flash.bin> --c64-roms \
    --usb-dir run/xander/share,ro --usb-dir-work run/xander/w<n> --script run/xander/ctl/<script>.ctl
```

The programs, their control scripts, logs and PNGs live under `run/xander` (gitignored): `artefacts/` the unpacked
releases, `share/` the USB stick the firmware sees (release layout: `idi8b/<project>/`), `ctl/` the scripts,
`out/` logs and screen dumps. Releases were left byte for byte as shipped; the only additions to the stick are
`uboot64.cfg` (see UBoot64) and copies of the shipped `config/Heartbeat-U64E2.cfg` under the auto-load names
`udemo2026.cfg` and `heartbeat-demo.cfg`, which is what the firmware's "Load Settings" does by hand
(`filetype_prg.cc:212`, `filetype_crt.cc:72` call `ConfigIO::S_load_associated_config`).

## Summary

| Project | How far it gets | Stopped by |
|---|---|---|
| UBoot64 v3.0.1 | Cartridge starts, UCI up, config file created on the stick, DOS version read | No C64-side REU |
| mandelbrot-upic v1.0.3 | **Runs and draws its picture** (fixed: TRX64 0.7.1 TOD) | — |
| UltimateDemo2026 v1.0.1 | Hardware detection: UCI OK, machine type OK; REU fails | No C64-side REU |
| heartbeat-demo v1.0.1 | Hardware detection: UCI OK, machine type OK; REU fails | No C64-side REU |
| GeoUTools v1.1 | D64 mounts on drive A, the C64 lists it | No GEOS system disk to boot from |

Three emulator gaps, in the order they cost the most:

1. **No REU on the C64 side.** The firmware register is there (`C64_REU_ENABLE`/`C64_REU_SIZE`, reset 0x07 in
   `crates/ue2-core/src/devices/c64.rs:117, 852`) and the firmware sets it from the menu, but no REU answers at
   `$DF00` on the C64. Three of the five projects stop there. Being built by another agent.
2. **CIA1 TOD never advances.** ~~`Cia::tick` bumps a free-running `tod_prescaler` and stops there — it never wraps
   and never carries into the BCD registers; the comment says so ("the 50/60 Hz tick … out of scope").~~ **Fixed in
   TRX64 0.7.1** (`fix-cia-tod-and-port-reset` `85721a6`, VICE's `ciacore.c` ported): the clock runs, and
   mandelbrot-upic draws (§2). Every wait loop in Xander's libraries is a TOD loop, because TOD is the one C64
   timer that keeps real time when the U64 runs at 16 or 64 MHz (`TURBOCONTROLMANUAL.md` §2). One caveat survives
   the fix — TOD runs ~5 % slow whenever the screen is on; see `docs/status/c64.md`, "CIA TOD".
3. **Ultimate Audio DMA `$DF20-$DFFF` and the extra SIDs are unmodelled.** The firmware config item exists
   (`Map Ultimate Audio $DF20-DFFF` → `C64_SAMPLER_ENABLE`, `c64.cc:95, 321-323`) and the emulator answers it from
   the T0 stub table (`devices/c64.rs:751`, `docs/status/carts.md` "sampler stays unmodelled"). Neither demo
   reached it: both check the REU first.

## 1. UBoot64 v3.0.1

`run/xander/artefacts/UBoot64-v2/uboot64.crt`, 48 K, CRT hardware type 3, 3 × 16 K chips at `$8000`, exrom 0
game 0. Started from the file browser with "Run Cart" (`run/xander/ctl/01-uboot-browse.ctl`,
`out/01-uboot.log`). `--cart-slot` was not needed and was not used: the firmware's own CRT loader takes the file.

The cartridge runs. Console:

```
Cartridge Load.. uboot64.crt
CRT Hardware type: 3 Header OK. Now reading chip packets starting from 000040.
Reading chip data bank 0 ($8000) with size $4000 to 0x03C00000.
Reading chip data bank 1 ($8000) with size $4000 to 0x03C04000.
Reading chip data bank 2 ($8000) with size $4000 to 0x03C08000.
Name: Final Cartridge III
Begin of cart init: Type: 19. REU: 00. REU_SZ: 04, UCI: 00 (DF18), Mode: 04, Sampler: 00
```

FC3 banking is `docs/status/carts.md` c10, already covered. What is new here is UCI under a real program
(`out/uboot-01.png`):

```
UBoot64:  Boot Menu for Ultimate devices
Starting....        v3.0.1-20260913-1321

Detecting and reading...
Ultimate Command Interface detected.
Storage found: /usb0/
DOS version: ultimate-ii dos v1.2

No REU detected.
Press key to exit to BASIC.
```

Everything above the REU line is UCI DOS traffic, and it all works. The cartridge enables UCI from the C64 side
itself (`uii_enable()`, the 3.15+ unlock sequence): the menu setting was left `Disabled` in this run, the
firmware's own cart init prints `UCI: 00 (DF18)`, and the program still reaches the block — README v3.0.0
claims exactly that, and it holds in UE2. Then device/storage search over the four candidate devices,
`uii_identify()`, and file creation. The last is visible on the host: the stick was read-write for that first run and

```
usb-dir port 1 (…/run/xander/share): wrote DMBCFG.CFG
```

`DMBCFG.CFG`, 101 bytes, is UBoot64's configuration file, created through UCI DOS and written back to the host
directory.

**Where it stops.** `src/main.c:346-361` probes the REU (`reu_store`/`reu_load` of a marker byte at page 0) and
calls `errorexit("No REU detected.")` when it reads back nothing. Everything UBoot64 is for sits behind that line:
`read_slotsfile()`, `uii_parse_deviceinfo()` (the drive detection), the REU-backed file browser and the NTP clock
in `time.c`. None of it was reached.

To make sure the wall is the emulator's and not a missing setting, the run was repeated with
`run/xander/share/uboot64.cfg` next to the CRT (`RAM Expansion Unit=Enabled`, `REU Size=16 MB`,
`Command Interface=Enabled`), which "Run Cart" auto-loads (`ctl/04-uboot-reu.ctl`, `out/09-uboot-reu.log`):

```
Action set was: Run Cart
Begin of cart init: Type: 19. REU: 01. REU_SZ: 07, UCI: 01 (DF18), Mode: 04, Sampler: 00
```

`REU: 01, REU_SZ: 07` — the firmware enabled a 16 MB REU — and the C64 screen is unchanged: `No REU detected.`
The firmware side of the REU is complete; the cartridge-side memory behind `$DF00` is what is missing.

## 2. mandelbrot-upic v1.0.3

`idi8b/mandelupic/{mandelupic.prg,mandelupic.cfg}`, started with "Run" from the browser
(`ctl/02-mandel.ctl`, `out/02-mandel.log`).

**The `.cfg` auto-load works,** and so does the turbo plumbing it asks for. Console:

```
Action set was: Run
DMA Load.. mandelupic.prg
Effectuating settings of store 'U64 Specific Settings' after loading.
Speed regs: 01 89
Effectuating settings of store 'C64 and Cartridge Settings' after loading.
Begin of cart init: Type: 41. REU: 00. REU_SZ: 04, UCI: 01 (DF18), Mode: 04, Sampler: 00
_Load address: 0801...Now loading...DMA load complete: $0801-$FFEE
```

`Speed regs: 01 89` is `setCpuSpeed` (`u64_config.cc:1625-1636`) with the cfg's three answers: turbo registers on
(`C64_TURBOREGS_EN` = 01), badlines suppressed and speed index 9 (`C64_SPEED_PREFER` = 0x89). On the boot default
it prints `00 80`. **What the setting does in our machine:** the bridge latches both and applies them on the
`C64_SPEED_UPDATE` strobe (`crates/c64-bridge/src/lib.rs:571-573`), TRX64 stores them in the VIC
(`set_u64_turbo`), `$D031` becomes readable and writable on the `u64` profile
(`vic.rs:1944, 2048`), and the speed index really scales the CPU: `lib.rs:2410-2411` sets `c64_core.turbo_div`
from `U64SpeedTable::U64II` (index 9 = 16 MHz, index 15 = 64 MHz) and `lib.rs:2277` scales the instruction budget
by it. So turbo is modelled, and index 9 matches the cfg's `CPU Speed=16`.

**It draws its picture since TRX64 0.7.1** (the CIA TOD fix, branch `fix-cia-tod-and-port-reset` `85721a6`).
Re-run 2026-09-16 exactly as before — "Run" from the browser, `.cfg` auto-loaded, `Speed regs: 01 89`,
`Begin of cart init: Type: 41. REU: 00. REU_SZ: 04, UCI: 01 (DF18)`, `DMA load complete: $0801-$FFEE` — with
screen dumps at 10, 30, 90 and 180 s emulated:

| t | Screen |
|---|---|
| 10 s | the Mandelbrot is part-drawn, blue/orange, building left to right — exactly the README's "builds up live" |
| 30 s | the picture is complete |
| 90 s | the same picture, but in a **greyscale** ramp |
| 180 s | complete and blue/orange again — byte-identical PNG to the 30 s dump (md5 `f8d96976ad8aba449669df4c0e12fff8`) |

188.3 s emulated, 20 MIPS (the C64 is at 16 MHz turbo, so a host cycle buys less emulated time than the 130 MIPS
of a 1 MHz run).

**The palette change takes effect.** The picture is drawn in the release's own blue/orange gradient, not in stock
C64 colours, so `uii_setpalette` over UCI reaches the U64 palette. What is *not* explained is the 90 s frame: the
same completed picture in greyscale, with 30 s and 180 s identical to each other. Nothing in the console marks a
palette call, and no key was pressed (`C` cycles the gradient by hand), so this is recorded as observed and not
diagnosed.

**Why it used to hang: CIA1 TOD was frozen** (TRX64 ≤ 0.7.0). Measured at the time directly from BASIC
(`ctl/16-tod.ctl`, `out/16-tod.log`, `out/tod.png`) — set the clock to zero, let 10 s of emulated time pass, read
it back:

```
POKE 56331,0   POKE 56330,0   POKE 56329,0   POKE 56328,0
PRINT PEEK(56329)
 0
PRINT PEEK(56328)
 0
```

Seconds and tenths are both still 0. In TRX64 `Cia::tick` (`cia.rs:549-558`) only does
`self.tod_prescaler = self.tod_prescaler.wrapping_add(1)`; the BCD registers at `$DC08-$DC0B` are written and
read (`tod_store`, `tod_read`) but never advanced — the file says as much: "Stage-1: clock set + latched read;
CRB-bit7 alarm split + the 50/60 Hz tick are out of scope".

That is exactly what mandelupic waits on. `main()` runs `rombank_out()`, `sei`, then `uii_wait_for_uci(5)`, whose
loop is `while (!uii_detect() && cia1.tods < timeout_seconds);` — with the Command Interface off, TOD is the only
way out and the program can never leave it. Confirmed by running the PRG with no `.cfg` at all
(`run/xander/share2`, `ctl/11-mandel-nocfg.ctl`): same hang, `out/mandel-nocfg-60s.png`. With UCI on but turbo
off (`run/xander/share3`, `ctl/12-mandel-ucionly.ctl`, `out/15-mandel-ucionly.log`) it hangs as well, so turbo is
not involved; the next TOD wait on that path is `setpalette_retry()`'s settle delay
(`while (cia1.todt < 2)`, `src/main.c:36-38`), taken whenever a `uii_setpalette` does not come back `UII_SUCCESS`.

TOD matters beyond this one program: it is the only C64 timer that still measures real time at turbo speed, which
is why `turbo_detect()` is built on it (`TURBOCONTROLMANUAL.md` §2, §7). With the tick missing,
`benchmark_delay()` returned 0 and any caller classified the machine as `TURBO_64MHZ` whatever the real speed.

**Now that the clock runs, one error is left:** TOD is 5.3 % slow while the screen is on, because TRX64's new
divider counts `Cia::tick()` calls and the VIC's stolen badline cycles advance `clk` without one. Measured
0:28.4 over a 30.0 s wait with the display on, 0:30.1 with it blanked — `docs/status/c64.md`, "CIA TOD", has the
numbers and the mechanism. A timeout loop like `uii_wait_for_uci` does not care; `benchmark_delay()` would still
misreport by 5 %. The `C64_VIDEOFORMAT` notice visible in every run here
(`c64: C64_VIDEOFORMAT 0x2b asks for 60 Hz; the C64 core is PAL-only (S14 §13 OQ3)`) turns out **not** to reach
the TOD rate at all: TRX64 takes the mains frequency from CIA CRA bit 7, not from the machine's region.

## 3. UltimateDemo2026 v1.0.1

`idi8b/ultdemo2026/{udemo2026.prg,4ev.mod}`, "Run" from the browser (`ctl/05-udemo-uci.ctl`,
`out/14-udemo-uci.log`, `out/udemo-uci-01.png`).

Without a config the demo stops on its first check — `UCI : [Fail] Not detected`, console
`Begin of cart init: … UCI: 00 (DF18)` (`out/03-udemo.log`): it does not enable UCI itself, so the setting has to
be on. With the shipped Elite-II preset under the auto-load name `udemo2026.cfg` the demo gets one step further
and stops at the same wall as the others:

```
            UltimateDemo2026
Hardware Detection  v1.0.1-20260603-2309

Waiting for Ultimate firmware...
  UCI   : [ OK ]  UCI Ok
  Type  : ultimate 64-ii
Checking REU...
  REU   : [Fail]  Not detected

16 MB REU is required.
  -> F2 > C64 settings > REU > 16 MB
```

Console for the same run: `Speed regs: 01 89`, `Begin of cart init: Type: 41. REU: 01. REU_SZ: 07, UCI: 01
(DF18)`, `DMA load complete: $0801-$97C8`. The machine-type line comes over UCI (`ultimate 64-ii`), so UCI
identify works on this path too.

**First wall: the REU**, not audio. The Ultimate Audio DMA and the MOD player sit behind the detection screen and
were never reached; `4ev.mod` is never opened. Nothing about `$DF20-$DFFF` can be concluded from this run beyond
the static fact that the emulator does not model it.

## 4. heartbeat-demo v1.0.1

`idi8b/heartbeat-demo/{heartbeat-demo.prg,maniac.reu,Knight Rider Theme.reu}` with the shipped
`config/Heartbeat-U64E2.cfg` under the auto-load name (`ctl/07-heartbeat.ctl`, `out/07-heartbeat.log`,
`out/heartbeat-01.png`). The preset is applied in full — console has `Effectuating settings of store 'Audio
Mixer' after loading.`, `Speed regs: 01 89` and `Begin of cart init: Type: 41. REU: 01. REU_SZ: 07, UCI: 01
(DF18), Mode: 04, Sampler: 00`:

```
     Heartbeat Tracker Player Demo
Hardware Detection  v1.0.1-20260730-2130

Waiting for Ultimate firmware...
  UCI   : [ OK ]  UCI Ok
  Type  : ultimate 64-ii
Checking REU...
  REU   : [Fail]  Not detected

16 MB REU is required.
  -> F2 > C64 settings > REU > 16 MB
```

Identical wall, identical line. The song is loaded into the REU before anything plays, so the 8 SIDs, the 7
Ultimate Audio DMA channels and the tick IRQ are all untested; `Sampler: 00` in the cart-init line shows the
firmware did not even arm the audio window, because the cfg's `Map Ultimate Audio $DF20-DFFF=Enabled` only takes
effect under `CAPAB_SAMPLER` (`c64.cc:319-323`), bit 21 / `0x00200000` (`itu.h:70`), and this run's word is
`*** FPGA Capabilities: 34C40222 ***` — that bit is clear.

## 5. GeoUTools v1.1

`GeoUTools.d64` (171 K) and `GeoUTools.d81` (800 K). The disk holds the three GEOS applications and two GEOS
documents and nothing else — `scripts/d64tool.py list`:

```
63   "GeoUMount"        SEQ
58   "GeoUTime"         SEQ
55   "GeoUConfig"       SEQ
139  "GeoUTools UK"     USR
152  "GeoUTools DE"     USR
```

Mounting works once drive A has a ROM. On a flash without one the mount succeeds but the drive stays off and the
C64 answers `?DEVICE NOT PRESENT ERROR` (`ctl/13-geos-dir.ctl`, `out/13-geos-dir.log`); the browser also offers
`Change drive type to 1541?` there. With `1541.bin` installed from the browser first ("Set as 1541 ROM", the
`docs/status/drive.md` path) the disk mounts and the C64 reads it (`ctl/18-geos-drive.ctl`,
`out/18-geos-drive.log`, `out/geos-drive2.png`):

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
"Known gaps".)

**Not tested: the GEOS side.** These are GEOS applications; they need a booted GEOS, and no GEOS system disk
exists in this tree — the GeoUTools disk carries none. So GeoUTools' UCI drive detection (`uii_parse_deviceinfo`
behind GEOS's own drive layer) was not exercised. Nothing here says it would fail: the same UCI calls work from
UBoot64. Two of the three tools also want the REU (GEOS RAM drives) and `UltiDOS: Allow SetDate` for the clock.

## What the projects' own documentation says about the firmware

- **The associated-config mechanism is real and is what these releases rely on.** A `.prg` or a `.crt` started
  from the browser auto-loads `<name>.cfg`, else `<name>.usr`, from the same directory
  (`filetype_prg.cc:186-188, 212-213`, `filetype_crt.cc:72`, `ConfigIO::S_load_associated_config`). Every store
  in the file is effectuated at once — the console prints one line per store — so a test can set turbo, UCI and
  the REU per program without touching the menu. `mandelupic.cfg` is 150 bytes and does exactly that.
- **`$D031` and the speed table.** `TURBOCONTROLMANUAL.md` §4: bits 3-0 speed index, bit 7 badline mask, and
  `$D031` reads `$FF` when the turbo registers are not enabled — the detection every U64 program uses. Index 15
  is 48 MHz on a U64/Elite-I and 64 MHz on an Elite-II/C64U and software cannot tell them apart. That matches
  TRX64's two tables (`vic.rs:698-716`) exactly.
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
