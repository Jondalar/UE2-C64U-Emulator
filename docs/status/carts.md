# W4-CART status — cartridge types beyond NORMAL, the SID and MUS players

Spec: docs/specs/S14-c64-trx64.md §W4-CART. The unmodified firmware runs every cartridge
logic of the U64 FPGA that means something on a C64: it loads a CRT from the file browser, the bridge serves the
banks from the guest DDR where the firmware's CRT loader put them, and a C64 program banks, writes flash, talks to
the GMOD2 EEPROM and freezes through the same register semantics as `all_carts_v5.vhd`. The SID and MUS player
cartridges start from the browser and play.

Firmware paths are relative to `firmware/1541ultimate/software/`, VHDL paths to `firmware/1541ultimate/fpga/`, TRX64
paths to `<TRX64 checkout>/crates/trx64-core/src/` (commit `a448229`).

## How it works

**Firmware side (unchanged).** `C64_CRT::read_crt` copies each CHIP packet to DDR `__cart_rom_start + bank × 16 K`
(+0x2000 for `$A000`/`$E000` chips), mirrors to 64 banks and picks the FPGA type/variant (c64_crt.cc:226-337, 339-358,
466-676). `start_cartridge` holds the C64 in reset, writes C64_CARTRIDGE_TYPE and releases the reset
(c64.cc:1154-1224, 1242-1391). GMOD2 EEPROM data goes to EEPROM_BASE `0x1004C800` (c64.cc:1640-1660). The SID and MUS
player carts are CART_TYPE_16K images built at start-up (filetype_sid.cc:65-96).

**Bridge (`crates/c64-bridge/src/cart.rs`, `cart_eeprom.rs`).**
- `CartLogic` ports the clocked process of `cart_slot/vhdl_source/all_carts_v5.vhd` for the U64-II generics: ROM at
  DDR `0x03C00000` with 22 cart bits (u2p_riscv_lattice.vhd:603-604; before 3.15 `0x00F00000` with 20, below), cart
  RAM at `0x00EF0000`, GeoRAM at `0x01000000`.
  Bank and mode registers, `rom_mode`, `addr_map`/`allow_write`, the IO1/IO2 register reads (`slot_resp`), `cart_en`,
  the reset/force branch (all_carts_v5.vhd:182-197) and `cart_kill` (654-658) follow the VHDL line by line. EXROM/GAME
  reach the C64 gated by `cart_en` (slot_server_v4.vhd:1083-1098); memory reads are served only while `cart_en`, writes
  whenever `allow_write` (slot_slave.vhd:166-199).
- The ROM and RAM are not copied: `C64Port` lends `IoCtx::ram` to the backend for every access that can run the C64 or
  touch its bus (`C64Backend::lend_ddr`) and takes it back before returning, so the cartridge reads and writes guest DDR
  live, as the FPGA does. EasyFlash flash writes therefore land in the firmware's own image of the cartridge: they
  survive a C64 reset and "Save Cartridge" writes them to the CRT file (below).
- `Eeprom` ports `devices/vhdl_source/microwire_eeprom.vhd` (M93C86 over GMOD2's `$DE00` bits 6/5/4, DO in bit 7), and
  the firmware window `0x1004C000` (dirty flag) / `0x1004C800` (2 K) reaches it through `C64Backend::eeprom_read/write`.
- The freezer state machine of `cart_slot/vhdl_source/freezer.vhd` runs off MATRIX_KEYB[10] (the firmware sets it from
  USB F11 while the menu is closed, keyboard_usb.cc:228, 419-435). Triggered carts pull NMI (and IRQ) on TRX64's source 3.
  Only the system reset and the reset button idle the freezer, not a C64 reset; `start_cartridge` does it by writing
  type 0 under the reset (c64.cc:1177-1178), which drops `freezer_ena` (all_carts_v5.vhd:213-214, freezer.vhd:95-99).
  So `C64Port` hands every type written under the reset to the cart logic, not only the one at the release. Before
  S35 it did not, and a KCS started after a frozen AR came up in its freeze mode.
- `CartProxy` is the `CartMapper` TRX64 holds; it shares `CartLogic` with the backend (`CartHandle`) and folds in the
  firmware's forced ULTIMAX decode (C64_MODE bit 1). Without a cartridge type and without the forced decode TRX64's slot
  stays empty, so the no-cart run path is the plain one.

**How line changes reach TRX64's PLA.** TRX64 re-runs its PLA only after `$00/$01` writes and consumed `$DE00-$DFFF`
writes (full.rs:237-252, 612-619). The proxy consumes every cart I/O write. For the rest `Trx64Backend::run_cpu` ends
TRX64's run where lines can change and recomputes the memconfig:
- carts whose I/O *reads* switch modes (Westermann, Simons BASIC, Business BASIC, Blackbox V9, KCS, Final Cartridge,
  Epyx) run with TRX64's access watch on `$DE00-$DFFF`; the observer stops at the instruction boundary after an access
  that changed EXROM/GAME (lib.rs:2009-2012, full_sc.rs:209-212, 274-277);
- Epyx FastLoad's capacitor (512 cycles after the last IO1/ROML access, slot_slave.vhd:117-145) ends the run at its
  deadline; the timer is held while the 6510 is stopped;
- a pushed freeze button steps instruction by instruction and switches the cart in right before the 6510 takes the NMI,
  so the stack pushes go to RAM and the vector comes from the cart, as freezer.vhd's "three writes" rule has it.

**Core and front end (additive).** `C64Backend` gained `lend_ddr`, `eeprom_read`, `eeprom_write` and
`set_freeze_button` with no-op defaults. `C64Port` lends DDR around CART/DMA/MATRIX accesses and ticks, maps the
EEPROM window (the T0 stub table there is gone; without a backend it still reads 0) and forwards MATRIX_KEYB[10].
`runner::attach_trx64` sets CAPAB_EEPROM (itu.h:71), which the firmware requires for GMOD2 (c64_crt.cc:213-219).

**Cartridge ROM in DDR.** The FPGA reads the internal cartridge's ROM at `g_rom_base_cart`, and the firmware puts it at
`__cart_rom_start`; the two come together in one update. 3.15 moved it ("Large cart support", 1541ultimate
`2e5c9e05`, linker.x:269-287): 4 MB at `0x03C00000` (22 cart bits) instead of 1 MB at `0x00F00000` (20 bits, ending
where the REU starts). The C64 Ultimate 1.x firmware is built on 3.14 and keeps the old place. `fwlayout::cart_rom`
reads it from the loaded image: `C64::set_cartridge` loads the address with a `lui` shortly before its message
"Copying %d bytes from array %p to mem addr %p" (0x0C and 0xB8-0xC0 bytes before it in 3.14, 3.15 and C64U 1.1.0).
`Machine::new` gives it to `C64Port`, which hands it to the backend (`C64Backend::set_cart_rom`); `CartLogic` then
serves the ROM from there with the bank bits of that size. Without it (GitHub issue #1) every PRG start from the file
browser on firmware before 3.15 ended at `READY.`: the boot cartridge the firmware DMA-loads through
(`c64_subsys.cc:550ff`) was empty, `dma_load` gave up after 60 handshake polls and reset the C64 with `init_cartridge`.
An image that does not show the address gets 3.15's, with a warning.

`set_cart` now distinguishes the two ways the FPGA takes a type: with the reset line held the cart comes up enabled;
the C64_CARTRIDGE_KILL bit 1 force alone leaves it disabled until a freeze (all_carts_v5.vhd:192). `restoreCart` after a
PRG run relies on that (c64_subsys.cc:178-192).

## Test files and runs

`scripts/make-test-crts.py <dir>` writes 32 CRTs, a PSID and a MUS file. Each CRT carries a small 6502 program
assembled by the script: it sets up the VIC itself, prints `<NAME> START`, copies a stub to RAM `$0C00` (visible in
every memory mode) and runs it. The stub drives the cart's registers, compares bytes against fillers that name their
bank (ROML 0x40+n, ROMH 0x80+n, strings `ROML BANK nn`/`ROMH BANK nn`), prints one line per check and ends with
`<NAME> PASS` or `<NAME> FAIL`. `c28`-`c31` (AR, KCS, SS5, FC) wait for the freeze button, the last three with their
cart switched off first; each has a freeze handler at `$F800` of bank 0, in the ULTIMAX window its freezer maps in
(all_carts_v5.vhd:174-179 with 530-533, 600-605, 562-563, 625-627), that prints `<NAME> FROZEN`.
`c32-twomegabyter.crt` has banks 0, 1, `$4D` and `$FF` only, each carrying its own number. `s01-tune.sid` is a PSID v2 (init `$1000`, play `$1003`, triangle voice with a frequency sweep),
`s02-tune.mus` a Sidplayer file with three HLT voices.

Setup (ROMs installed on `run/flash.bin` as in docs/status/c64.md A2):

```sh
python3 scripts/make-test-crts.py run/carts
scripts/make-sd-image.sh run/carts.img
scripts/add-sd-files.sh run/carts.img run/carts/*
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --sd run/carts.img --usb-keyboard \
    --script scripts/smoke-c64-carts.ctl > run/carts.log
```

**`scripts/smoke-c64-carts.ctl`** (153.6 s emulated, 30 s wall, exit 0). The overlay menu stays open while a cart runs
(C64_START_CART releases only a C64-screen client, c64_subsys.cc:273-279), so each cart is one `key down`, RETURN,
RETURN (Run Cart). Results:
- 28 `c64screen` dumps end in `<NAME> PASS`: NORMAL 8K, NORMAL 16K, ULTIMAX, OCEAN, MAGIC DESK, EASYFLASH, GMOD2,
  ACTION REPLAY, RETRO REPLAY, FINAL CARTRIDGE III, SUPER SNAPSHOT 5, KCS POWER, FINAL CARTRIDGE, EPYX FASTLOAD,
  WESTERMANN, SIMONS BASIC, C64 GAME SYSTEM, ZAXXON, MEGABYTER, SUPER GAMES, COMAL 80, PAGEFOX, BLACKBOX V3,
  BLACKBOX V4, BLACKBOX V8, BLACKBOX V9, ATOMIC POWER, TWOMEGABYTER; no FAIL or BAD line.
- `c28`-`c31`: `PRESS FREEZE`; after `button` (menu closed) and `usbkey f11 300` the dumps show `ACTION REPLAY
  FROZEN`, `KCS FROZEN`, `SUPER SNAPSHOT FROZEN` and `FINAL CARTRIDGE FROZEN` from the carts' freeze handlers.
- `s01-tune.sid`: console `Loading SID..`, `Bytes loaded: 66. $1000-$1042`, no `Time out!`. The C64 shows the player
  (`run/c64-sid.png`), and its clock runs, so the play routine is called every frame:

```
  *** THE ULTIMATE C-64 SID PLAYER ***
TITLE : UE2EMU TEST TUNE
AUTHOR: UE2EMU
REL.BY: UE2EMU
YEAR  : 2026
SYSTEM: $D400 : 6581 / PAL
SID   : $D400 : 6581 / PAL
SONG  : 1 / 1
00:07                              05:00      (3 s later: 00:10)
```

- `s02-tune.mus`: `Bytes loaded: 13. $1000-$100D`; `*** THE ULTIMATE C-64 MUS PLAYER ***`, `TITLE : S02-TUNE`, clock
  `00:07 / 03:00`.

**EasyFlash flash writes in the firmware's model** (control steps in a scratch script, fresh flash, same image):
- Run `c06-easyflash.crt`: `FLASH FRESH`, `FLASH WRITE ROML OK`, `FLASH WRITE ROMH OK` (bank 1 `$8123` ← `$A5` and
  `$F456` ← `$3C` in ULTIMAX mode 101 after `$DE09` ← `$65`, read back in 16K mode), `NO WRITE WITHOUT KEY OK`.
- F5 → C64 Machine → Reset C64 (the firmware toggles the reset line, it does not reload the CRT, c64.cc:615-623): the
  cart starts again and prints `FLASH KEPT OVER RESET`.
- F5 → C64 Machine → Save Cartridge, name `ue2save`: the firmware writes `ue2save.crt` (1 050 688 bytes, 128 CHIP
  packets) from DDR (c64_crt.cc:678-741). Read back from the image: bank 1 `$8000` chip offset 0x123 = `0xA5`, bank 1
  `$A000` chip offset 0x1456 = `0x3C`, the neighbours still hold their fillers.

**Unit tests** (`cargo test -p c64-bridge`): cart logic per family (NORMAL variants/kill/force,
Ocean/Magic Desk/GMOD2, EasyFlash modes, RAM and ULTIMAX writes into DDR, AR/RR/FC3, freeze button, read-triggered KCS
and timed Epyx lines, forced ULTIMAX and GeoRAM through the proxy), the EEPROM (EWEN/WRITE/READ streaming, ERASE,
ERAL, WRDIS, start-bit error), and on a running TRX64: the boot cart from DDR, a KCS `$DE00` read that switches
16K → 8K before the next instruction, and an FC3 freeze entering through the cart's NMI vector from a RAM loop.
`ue2-core`: `ddr_is_lent_per_access_and_eeprom_and_freeze_reach_the_backend`.

## Every type the firmware knows

CRT hardware ids from `c_recognized_c64_carts` (c64_crt.cc:19-106), mapped by `configure_cart` (c64_crt.cc:466-676) to
C64_CARTRIDGE_TYPE (c64.h:125-163). "Done" means implemented after the VHDL and run from the browser with its test CRT.

| CRT id | Name | FPGA type | Status |
|---|---|---|---|
| 0 | Normal cartridge | 0x41 8K / 0x01 16K / 0xA1 ULTIMAX | done (c01-c03) |
| 1 | Action Replay | 0x1B | done (c08); freezer done (c28, USB F11) |
| 2 | KCS Power Cartridge | 0x1C | done (c12); freezer done (c29) |
| 3 | Final Cartridge III | 0x19 (0x39 > 64 K) | done (c10); freezer unit-tested (NMI vector entry) |
| 4 | Simons Basic | 0x05 | done (c16) |
| 5 | Ocean type 1 | 0x08 | done (c04). The firmware never selects its 16K variant (`a000_seen` is never set, c64_crt.cc:498) |
| 8 | Super Games | 0x0B | done (c20) |
| 9 | Atomic Power | 0x5B | done (c27), including mode 110 RAM at `$A000` |
| 10 | Epyx Fastload | 0x02 | done (c14) |
| 11 | Westermann | 0x44 | done (c15) |
| 13 | Final Cartridge I | 0x18 | done (c13); freezer done (c31) |
| 15 | C64 Game System | 0x0A | done (c17). A read of IO1 selects bank 0: the VHDL loads the undriven data bus |
| 18 | Zaxxon | 0x0D | done (c18) |
| 19 | Magic Desk, Domark, HES Australia | 0x28 | done (c05) |
| 20 | Super Snapshot 5 | 0x1A (0x3A > 64 K) | done (c11); freezer done (c30) |
| 21 | COMAL 80 | 0x09 (0x29 > 64 K) | done (c21) |
| 32 | EasyFlash | 0x11 | done (c06, flash writes kept over reset and saved to CRT) |
| 36 | Retro Replay | 0x3B | done (c09) |
| 44 | EXOS | none (CART_KERNAL) | no cart logic: the firmware copies the ROM into the KERNAL window; not run |
| 53 | Pagefox | 0x10 | done (c22) |
| 54 | Kingsoft Business Basic | 0x06 | partial: the 16K mode works; the dynamic mode (EXROM/GAME per address, all_carts_v5.vhd:287-297) is shown to TRX64 as off. Not run |
| 60 | GMod2 | 0x48 + EEPROM | done (c07: banks, EEPROM read of the CRT's chunk, EWEN/WRITE/READ) |
| 64 | Blackbox V8 | 0x0C | done (c25) |
| 65 | Blackbox V3 | 0x07 | done (c23) |
| 66 | Blackbox V4 | 0x24 | done (c24) |
| 71 | Blackbox V9 | 0x0E | done (c26) |
| 86 | Protovision Megabyter | 0x0F | done (c19) |
| 87 | Protovision TwoMegabyter | 0x2F | done (c32) |
| 6, 7, 12, 14, 16, 17, 33-35, 37-43, 45-52, 55-59, 61-63, 67-70, 72-85 | Expert, Fun Play, Rex, Magic Formel, Warpspeed, Dinamic, EasyFlash X-Bank, Capture, AR3, MMC64, MMC Replay, IDE64, SS4, IEEE 488, Game Killer, Prophet 64, Freeze Frame, … GMod3, … Magic Desk 16 | – | not done: the firmware rejects them (`CART_NOT_IMPL`, "Not implemented") |
| C128 0, 1 | C128 Cartridge (with I/O mirror) | 0x03 / 0x63 / 0xE3 | not done: the logic serves `$8000-$FFFF` of a C128; on a C64 the bridge attaches nothing and says so once |
| – | Boot cartridge (DMA load) | 0x41 | done (A4) |
| – | SID Player Cartridge | 0x01 + UCI `$DFFC` | done (s01). The UCI answers at `$DFFC` since S15 (TRX64 Spec 852); sidcrt uses it only for an invalid header |
| – | MUS Player Cartridge | 0x01 + UCI `$DFFC` | done (s02) |
| – | GeoRAM (REU setting "GeoRAM") | 0x1F | done (DDR `0x01000000`, the REU size masks the banks; `scripts/smoke-georam.ctl`) |

## What TRX64's cartridge API does not cover (and how the bridge works around it)

TRX64 supports its own families — Normal 8K/16K/Ultimax, MagicDesk, Ocean, EasyFlash (+XL), GMOD2, GMOD4, MegaByter,
C64MegaCart, possibly GMOD3 later — and none of them needs items 1-3 and 6 below. Freezers, Atomic Power, Business
BASIC and Pagefox are out of TRX64's scope (owner decision, 2026-09-23): `CartMapper` stays as it is, and any hook these
families need lives in UE2. The items are UE2's to carry, not TRX64 gaps.

1. **No PLA re-evaluation after reads or by time.** `pla_config_changed` runs only for `$00/$01` and consumed
   `$DE00-$DFFF` writes (full.rs:237-252, 612-619); I/O reads fall through to the mapper without it (full.rs:486-491).
   Workaround: TRX64's access watch plus an observer that halts after a line-changing access
   (`run_for_full_capped_dbg`, lib.rs:2017-2140; full_sc.rs:209-212), run deadlines for timers, and a memconfig
   recompute after every slice.
2. **`get_lines` has no address or R/W.** `CartMapper::get_lines` (cart.rs:354) cannot express EXROM/GAME that depend on
   the bus address: Business BASIC's dynamic mode stays off; Atomic Power's ULTIMAX-for-writes trick is modelled by
   consuming the `$A000-$BFFF` write.
3. **No hook between the interrupt pushes and the vector fetch.** `do_interrupt` pushes and loads `$FFFA` inside one
   `execute_one` (c64_6510core.rs:2205-2240), and `interrupt_check_nmi_delay` / `OPINFO_DELAYS_INTERRUPT_MSK` are
   private (c64_6510core.rs:81, 411-420). The bridge replicates the NMI check on the public `IntStatus` fields to switch
   the cart in before the instruction, and if an interrupt is taken unforeseen (an IRQ first) it switches after and
   moves PC to the cart's vector; the first KERNAL handler instruction has then run.
4. ~~**Four interrupt sources** (c64_6510core.rs:143-155): cartridge NMI and IRQ share source 3 with the RESTORE
   NMI.~~ **Closed by TRX64 Spec 850:** `INT_SRC_EXPANSION` is a fifth source, driven per cycle from the port and
   from the host's `Machine::set_expansion_lines`. The bridge puts both cartridges' IRQ and NMI there, and RESTORE
   has source 3 to itself again (docs/specs/S15-uci.md §3.4).
5. ~~**The VIC view has no cartridge ROM**: ULTIMAX carts that serve the VIC (`serve_vic`: CART_TYPE_UMAX, FC3 mode
   10, KCS) showed RAM instead of ROMH at VIC `$3000/$7000/$B000/$F000`.~~ **Closed by TRX64 v0.8.5:**
   `CartMapper::vic_romh`, which the bridge answers for both cartridges (`cart.rs`, `slot.rs`).
6. ~~**Cartridge writes only in mapped windows.** FullBus calls `CartMapper::write` for `$8000-$BFFF`/`$E000-$FFFF`
   only when the PLA maps the window (full.rs:1100-1133); the FPGA writes cart RAM by address alone
   (slot_slave.vhd:183-199).~~ **Closed in UE2 by S28:** while an AR/RR, SS5 or Pagefox logic is in, `cart::RamSnoop`
   sits on the expansion port and snoops `$8000-$BFFF` (Spec 850's `snoop_write`), so every write cycle reaches the
   cart RAM where `allow_write` says so (docs/specs/S28-cart-ram-writes.md).

## Known gaps

- **Stops and DMA** land on instruction boundaries; `SERVE_WHILE_STOPPED` is not honoured, the cart
  always serves DMA reads.
- **`dma_peek`** (debugger, `ue2emu install`) has no DDR lent: cartridge ROM windows read as unserved there.
- **Freeze entry** is exact for NMI-driven entry; an IRQ taken first (Action Replay, SS5 and KCS pull both) costs one
  KERNAL instruction. Freezing through the firmware's own C64-screen UI path is unrelated and unchanged.
- **MATRIX_KEYB[10] as the cart freeze button** is inferred from keyboard_usb.cc:228 and freezer.vhd; the U64-II top
  level that wires it is closed.
- **ACIA** (SwiftLink/modem) is not modelled (`docs/status/gaps.md`). UCI answers at any slot base, EasyFlash's
  `$DE1C` included (`docs/specs/S15-uci.md`).
- **Audio** comes from the SID stream (docs/status/sid-audio.md). A cart smoke run with `--audio-wav run/carts.wav` holds the `s01-tune.sid` tune: peak-to-peak about 9300 in every second the SID player
  runs (133-146 s emulated), so the player's CPU writes reach reSID through the cartridge run path (`CartObserver`
  forwards bus writes to the SID tap). The MUS test file (HLT voices) stays silent.
