# Physical cartridge slot (`--cart-slot`)

A cartridge in the U64's expansion port, next to the internal cartridge emulation: a `.crt` served by TRX64's cartridge
mappers, by bridge boards built on TRX64's flash and EEPROM chips, or (for types TRX64 lacks) by the ported U64 cart
logic fed from the CRT. The firmware's DMA accesses, an app such as the TREX CRT Tool and REST calls reach it like a
board in the port, with its own flash and EEPROM. The internal cartridge (the one the firmware loads into guest DDR,
`docs/status/carts.md`) is unchanged and can be active at the same time.

Firmware paths are relative to `firmware/1541ultimate/software/`, TRX64 paths to
`<TRX64 checkout>/crates/trx64-core/src/`, the app SDK's to the fork's `software/`
(`<1541ultimate checkout>`, branch `feature/app-loader`, `ce34871d`).

## Usage

```sh
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --net user --hostfwd tcp:127.0.0.1:8080:80 \
    --control 127.0.0.1:6400 --cart-slot run/cart-slot-ef/game.crt          # read-only (default)
    --cart-slot run/cart-slot-ef/game.crt,rw                                 # flash/EEPROM changes go back into game.crt
    --cart-slot run/cart-slot-ef/game.crt,save=run/cart-slot-ef/after.crt    # ... into another file
    --cart-slot run/cart-slot-ef/game.crt,rw,flash-decode=15                 # options combine
```

`--cart-slot SPEC` is accepted by `run` and `install` (needs the C64; `install` never writes back). `SPEC` is the CRT
path followed by comma-separated options:

- **`ro`** (default): the CRT file is read once and never written. Its hash is unchanged after every acceptance run.
- **`rw`**: the cartridge as it is now is written back into the same file on a clean exit (`quit`) and, while the
  emulator runs, once flash and EEPROM have been quiet for 2 s emulated after a change. Each write goes to a hidden
  temporary file (`.NAME.tmp-PID`) in the same directory that is renamed over the target; before the first write of a
  run the original is copied to `NAME.bak`. An unchanged cartridge is not rewritten.
- **`save=OUT.crt`**: the same debounced writes into `OUT.crt` (no backup), and `OUT.crt` is always written on a clean
  exit, changed or not.
- **`flash-decode=11|15|both`** (default `both`): which command addresses the flash chips decode, see *Flash command
  decode* below.

Control commands (`--control`, `--script`, `emu_control`):

- `cart-save <path>` writes the cartridge as it is now (every bank, flash contents, EEPROM) to `<path>`, whatever the
  mode, without touching the source. Answers `saved:`, `bytes:`, `generation:`.
- `cart-info` answers `key: value` lines: `type`, `name`, `hardware` (CRT hardware type), `model` (`trx64`,
  `trx64-flash`, `u64-logic`), `banks`, `exrom`, `game`, `mode` (8K/16K/ULTIMAX/off from the cartridge's own lines),
  `cart_detect`, `bus_internal`, `bus_external`, `bus_bridge`, `flash_decode`, `source`, `persist`, `writable`, `dirty`,
  `generation` (flash/EEPROM change counter), `saved_generation`, `unsaved`.

MCP (`docs/status/mcp.md`): `emu_start {cart_slot: {path, mode?: "ro"|"rw"|"save", save_path?, flash_decode?}}`,
`emu_cart_info {id}`, `emu_cart_save {id, path}`.

The C64 needs its system ROMs on the flash (`docs/status/c64.md` A2) for anything that boots through the KERNAL (an
autostarting 8 K cartridge, BASIC behind a cartridge that switched itself off).

Test tools:

- `scripts/make-slot-crts.py DIR`: 15 CRTs (every family below) with distinct per-bank patterns, `manifest.json`.
- `scripts/cart-dump-rest.py --url URL [--probe --roms DIR] [--source CRT] [--out CRT]`: REST-only dump (pause, bank
  selects by writemem, windows by readmem, resume), the dumper app's probe sequences, comparison with the source CRT.
- `scripts/cart-flash-rest.py --url URL --family easyflash|megabyter|c64megacart|gmod2 [--unlock short|long]`:
  autoselect, sector or chip erase with DQ6/DQ7/DQ5 polling, byte program, read back, optional `cart-save` comparison.
- `scripts/cart-slot-acceptance.py`: the whole acceptance below, one emulator per run.
- `scripts/cartslot_common.py`: CRT files, patterns, family bank selects, the REST and control clients.

## Which model serves a CRT

| CRT hardware type | Family | `model` |
|---|---|---|
| 0 (by EXROM/GAME) | Normal 8K, 16K, ULTIMAX | `trx64` (`NormalMapper`) |
| 5 | Ocean type 1 (512 K: 8 K mode, else 16 K) | `trx64` |
| 19, 85 | Magic Desk, Magic Desk 16 | `trx64` |
| 87 | GMod4 (TRX64's numbering) | `trx64` |
| 32, 232 | EasyFlash (2 × AM29F040B), EasyFlash XL (TRX64 development type) | `trx64-flash` |
| 60 | GMod2 (AM29F040 + M93C86 EEPROM from the CRT's EEPROM chip) | `trx64-flash` |
| 61 | C64MegaCart (M29F160FT, 14-bit bank register, TRX64's numbering) | `trx64-flash` |
| 86 | MegaByter (MX29F800CB, byte mode) | `trx64-flash` |
| 1-4, 8-11, 13, 15, 18, 20, 21, 36, 53, 54, 64-66, 71 | Action Replay, KCS, FC III, Simons, Super Games, Atomic Power, Epyx, Westermann, FC I, C64 Game System, Zaxxon, SS5, COMAL 80, Retro Replay, Pagefox, Business Basic, Blackbox V3/V4/V8/V9 | `u64-logic` (`cart::CartLogic`, all_carts_v5.vhd) with the cartridge's own 4 MB ROM and 64 K RAM, laid out as `C64_CRT::read_crt` does (c64_crt.cc:291-358, `configure_cart` 466-651) |

Any other type (and any C128 CRT) is refused at start with the list above.

`trx64-flash` boards (`crates/c64-bridge/src/slot.rs`) implement TRX64's `CartMapper` with TRX64's own `Flash040` and
`M93c86` chips, register decoding as TRX64's mappers, but without the EAPI replacement TRX64's EasyFlash mapper does:
the flash holds exactly the CRT's bytes, so a dump equals the CRT. They add the `flash-decode` override, a C64MegaCart
bank register that wraps at the chip size, and a flash clock that survives the C64's reset (below).

## Model

The U64-II top level that connects the expansion port to the C64 core is closed. The model follows the firmware that
drives it and the open cartridge-side VHDL (`crates/c64-bridge/src/slot.rs`, `lib.rs`):

- **Bus sharing.** C64_BUS_INTERNAL / C64_BUS_EXTERNAL (0x1018002B/2C) choose which side serves IO1 (bit 0), IO2
  (bit 1), the ROM windows with EXROM/GAME (bit 2) and the interrupt lines (bit 3). The firmware writes them in
  `ConfigureU64SystemBus` (c64.cc:1509-1596) from "Cartridge Preference" and U64_CART_DETECT: at boot
  (u64_config.cc:911, c64.cc:277), in `init_cartridge` (c64.cc:1444-1478) and in `start_cartridge` (c64.cc:1209).
  Auto: external 15 when a cartridge is detected (c64.cc:1514), else internal 15. Internal: internal 15. External:
  external 15. Manual: from the four sharing items. The model applies each write at once.
- **Bridge.** C64_BUS_BRIDGE (0x1018002A) bit 0 ("Writes") mirrors IO1/IO2 writes to the side that does not serve them.
- **Lines.** EXROM/GAME of the sides that serve ROM pull the PLA inputs low together (open collector, wired AND).
  C64_MODE bit 1 (the freezer's ULTIMAX) forces GAME low and EXROM high whatever a cartridge drives
  (slot_server_v4.vhd:1083-1098).
- **Data.** A read in a ROM window or IO1/IO2 is answered by every side that serves it and drives the bus; two drivers
  are ANDed; nothing driving is open bus (TRX64's phi1 value for I/O, 0xFF for ULTIMAX holes).
- **Writes** reach every side that serves the range. A ROM-window write lands in C64 RAM unless a side consumes it
  (flash in its programming mode) or the lines are ULTIMAX.
- **U64_CART_DETECT** (0x10100403, u64.h:68) reads the physical cartridge's own GAME (bit 0) and EXROM (bit 1), before
  sharing and before the forced ULTIMAX; the C64 port refreshes it after every access that ran the C64.
- **Reset.** The C64's reset line reaches both cartridges. TRX64's warm reset zeroes the C64 clock; the slot adds the
  old clock to its flash epoch at each reset release, so an erase in progress keeps its end time.
- **DMA.** Every firmware DMA read and write (`dma_read`/`dma_write`, the forced ULTIMAX lens) goes through TRX64's bus
  and reaches the physical cartridge exactly as a CPU access does; DMA_MEMONLY bypasses it (RAM only).
- **Flash timing.** TRX64's flash models finish an erase step at an absolute C64 cycle and catch up on the next access
  (flash040.rs `catch_up_erase`). Every access carries the live cycle, and the C64 clock advances while the 6510 is
  stopped (S14 §4), so an erase started over DMA or REST completes while the C64 is paused (unit test
  `physical_easyflash_over_dma_while_stopped`: sector erase done 1000-1100 ms emulated after the command).

### Flash command decode

Datasheets differ in how many address bits the command sequence decodes: the AM29F040**B** decodes 11 bits
(`555h/2AAh`), the older AM29F040 that VICE and TRX64 model for GMod2 decodes 15 bits (`5555h/2AAAh`); byte-mode chips
(MX29F800CB, M29F160FT) use `AAAh/555h` or `AAAAh/5555h`. TRX64 fixes one per chip type. `flash-decode=` overrides the
command addresses of every flash chip on the board:

| `flash-decode` | 8-bit chips accept | byte-mode chips accept |
|---|---|---|
| `11` | `x555/x2AA` (mask 7FF) | `xAAA/x555` (mask FFF) |
| `15` | `5555/2AAA` (mask 7FFF) only | `AAAA/5555` (mask FFFF) only |
| `both` (default) | both: a 15-bit address also matches the 11-bit decode | both |

`11` and `both` behave the same; `15` refuses the short sequence (the chip stays in read mode and returns data).

## The dumper app's sequences

`scripts/cart-dump-rest.py --probe` runs the fork's cartlib sequences over REST and prints what `cartlib_probe` and
`cartlib_find_flash` decide: mode from the KERNAL/BASIC compare, the `$DE00` banking compare at `$8000`, the EasyFlash
`$DF00` RAM test (`$A5`), `cartlib_internal_count_banks` (FNV hashes at 8..512, cartlib.c:158-199), MegaByter
`$DE02=$03` changing `$E000`, C64MegaCart `$DF00=$C0` changing `$E000`, the GMod2 M93C86 read through `$DE00` (CS `$40`,
CLK `$20`, DI `$10`, DO in bit 7). App bus setup as `api_c64_cart_setup_bus` (app_loader/app_api_impl.cc:497): MEMONLY 0,
bridge 3, internal 0, external 15, `$00=$2F`, `$01=$37`, C64_MODE 0. Results on the acceptance CRTs:

| CRT | mode | `$DE00` banking | `$DF00` RAM | count | MB `$E000` | MC `$E000` | EEPROM | `cartlib_probe` | `cartlib_find_flash` |
|---|---|---|---|---|---|---|---|---|---|
| s01 Normal 8K | 8k | no | no | 8 | no | no | no | Normal 8K | — |
| s02 Normal 16K | 16k | no | no | 8 | no | no | no | Normal 16K | — |
| s03 ULTIMAX | ultimax | no | no | 8 | no | no | no | Ultimax | — |
| s04 Ocean 16 banks | 16k | yes | no | 16 | no | no | no | Ocean 16K | — |
| s05 Ocean 512K | 8k | yes | no | 64 | no | no | no | Ocean/Magic Desk | — |
| s06 Magic Desk | 8k | yes | no | 16 | no | no | no | Ocean/Magic Desk | — |
| s07 Magic Desk 16 | 16k | yes | no | 8 | no | no | no | Ocean 16K | — |
| s08 EasyFlash | ultimax | yes | yes | 64 | — | no | no | EasyFlash | EasyFlash |
| s09 EasyFlash (EAPI, switched off) | none | no | yes | 64 | — | no | no | Normal (no cartridge lines) | — |
| s10 GMod2 64 banks | 8k | yes | no | **128** | no | no | yes | **MegaByter** | **—** |
| s11 MegaByter 128 banks | 8k | yes | no | 128 | yes | no | no | MegaByter | MegaByter |
| s12 C64MegaCart 256 banks | 8k | yes | no | **128** | no | yes | no | **MegaByter** | **—** |
| s13 Super Games | 16k | no | no | 8 | no | no | (noise) | Normal 16K | — |
| s14 C64 Game System | 8k | yes | no | 8 | no | no | no | Ocean/Magic Desk | — |
| s15 Action Replay | 8k | no | no | 64 | — | no | no | Normal 8K | — |

The boards answer as their hardware does; the bold rows are cartlib decisions that follow from its own sequences:

- **count_banks never exceeds 128.** It never writes `$DE00 ≥ $80` and treats 128/256/512 as a wrap
  (cartlib.c:171-176), so every cartridge with more than 64 distinct banks counts 128. `cartlib_find_flash`'s
  C64MegaCart branch (`banks >= 256`, cartlib_write.c:130) can therefore never match, and the probe names a 256-bank
  C64MegaCart "MegaByter".
- **GMod2 counts 128.** `$DE00=$40` is not bank 64 on a GMod2: bit 6 is its EXROM control, so the ROM leaves `$8000`
  and the hash differs; 128 (`$80`, the write-enable bit with bank 0) is skipped as a wrap. The count is 128, so the
  probe says MegaByter and `find_flash`'s GMod2 branch (`banks <= 64`) is never reached, although the EEPROM read works
  (words `5545 3245`, the CRT's `"UE2E"`).
- **GMod2 unlock.** cartlib's GMod2 writer unlocks with `$E555/$E2AA` (11-bit). That works with `flash-decode=both`
  or `11`, not with `15` (the AM29F040 decode VICE and TRX64 use; the chip answers with array data, `aa b1`).
- **An EasyFlash that switched itself off** (EAPI boot stub, `$DE02=$04`) shows no cartridge lines (CART_DETECT 0x03),
  so the mode is none; the `$DF00` RAM still answers, and the REST dumper (which enables it with `$DE02=$05`) dumps it
  exactly.

The flash writer sequences (AMD/MX autoselect, sector and chip erase, byte program with DQ7/DQ5 polling, EasyFlash
ROMH programmed at `$E000` in ULTIMAX via `$DE02=$05`) are in `scripts/cart-flash-rest.py`; results under Acceptance.

## A real U64 quirk and the model

Reported from hardware: with Cartridge Preference Auto or External and a cartridge in the port, `init_cartridge`
prints `External Cartridge Selected. Not initializing cartridge.` and returns early (c64.cc:1458-1464); DMA reads in
normal C64_MODE then return RAM, while the freezer's ULTIMAX shows the cartridge at `$8000`; with Internal and a reboot
the dumper works.

**Not reproduced; documented difference.** The model applies C64_BUS_INTERNAL/EXTERNAL as live registers. With Auto
the firmware sets external 15 (c64.cc:1514-1520) and every DMA read reaches the cartridge in any C64_MODE: all
acceptance dumps above ran with Auto and are exact. With Internal + reboot, bus internal 15 / external 0 hides the
cartridge (the Normal 8K test boots BASIC) until an app's `api_c64_cart_setup_bus` routes external 15, and the dumper
sees it (the TREX run below). The early return skips `C64_CARTRIDGE_KILL = 2` and `set_cartridge(NULL)`
(c64.cc:1466-1473); on hardware the external ROM path in normal mode presumably depends on state set there or at
cartridge start in the closed top level. Without that VHDL the model does not guess it. The TREX CRT Tool itself
refuses with "No access to the cartridge. Set 'Cartridge Preference' to 'Internal' and reboot." unless the preference
is Internal, so on both hardware and emulator the working procedure is the same.

REST `PUT /v1/machine:pause` / `:resume` work with a physical cartridge in every mode; every acceptance dump and flash
run below ran paused (one pause/resume per run, ULTIMAX and EasyFlash-off states included).

## Worked example: the TREX CRT Tool dumps a physical EasyFlash

Run on the fork firmware (`feature/app-loader` `ce34871d`, `target/u64ii/riscv/ultimate/result/ultimate.elf`) with
`trex_crt.u2a` on the SD image and the acceptance EasyFlash (`s08`: banks 0-9, ROML 37, ROMH 63) in the slot:

```sh
D=run/cart-slot-trex; mkdir -p $D
python3 scripts/make-slot-crts.py $D/crts && cp $D/crts/s08-easyflash.crt $D/ef.crt
scripts/make-sd-image.sh $D/sd.img && scripts/add-sd-files.sh $D/sd.img path/to/trex_crt.u2a
cp run/flash.bin $D/flash.bin                        # a flash with the C64 ROMs (docs/status/c64.md A2)
target/release/ue2emu run --headless --speed max \
    --firmware <1541ultimate checkout>/target/u64ii/riscv/ultimate/result/ultimate.elf \
    --roms firmware/1541ultimate/roms --flash $D/flash.bin --sd $D/sd.img \
    --net user --hostfwd tcp:127.0.0.1:62301:80 --control 127.0.0.1:62300 --cart-slot $D/ef.crt
```

1. Apps need the C64-screen UI and cartridge access:
   `PUT /v1/configs/User%20Interface%20Settings/Interface%20Type?value=Freeze`,
   `PUT /v1/configs/C64%20and%20Cartridge%20Settings/Cartridge%20Preference?value=Internal`, `PUT /v1/machine:reboot`.
   (With Overlay on HDMI "Run App" answers "Apps require Freeze mode. Freeze the C64 first, then try again."; with
   Auto the tool shows "No access to the cartridge".)
2. Control: `button`, `key right` (/SD/), `key down` ×3 (trex_crt.u2a), `key return`, `key return` (Run App),
   `key n` (do not copy the tool to flash), `key down`, `key return` (Dump Cartridge).
3. The tool prints `Dumper: EasyFlash 1024KB` and the scan map (data in ROML/ROMH 0-9, ROML 37, ROMH 63);
   the console shows `[CartDumper] Probe: EasyFlash, 64 banks, has_romh=1`. `Cart Inspector` shows
   `Cart:G=0 E=1 (UMX)` and bank 0's pattern at `$8000`.
4. `key f3`, `key y` (Detected: EasyFlash, dump as type 32), `key right`, `key return` (Select Current Dir),
   `key return` (name `cartridge`) → `Done: 64 banks, 1024KB saved.`, console `Saving as type 32 to: '/SD//cartridge.crt'`.
5. `quit`, then copy `cartridge.crt` out of the image (mount it with
   `hdiutil attach -imagekey diskimage-class=CRawDiskImage`).

Result: 1 050 688 bytes, type 32, EXROM 1 / GAME 0; ROML and ROMH of all 64 banks equal the source CRT byte for byte
(the tool writes 128 chip packets, the source holds 22). `cart-info` afterwards: unchanged (`dirty: no`), source hash
unchanged.

## Acceptance

`scripts/cart-slot-acceptance.py --bin target/release/ue2emu --firmware $FW_ELF --roms $FW/roms --flash <ROM flash>
--work DIR`, headless, `--net user`, release build: **0 failed checks** (72 PASS).

- **REST dumps, every family** (Auto preference, default `ro`): 15/15 exact, including 256-bank C64MegaCart
  (2 097 152 bytes, 1100 REST calls, 2.5 s), GMod2 with EEPROM (8 words), the u64-logic Super Games, C64 Game System
  and Action Replay. Every run: clean exit, source hash unchanged. The Normal 8K autostart CRT boots and shows
  `CART SLOT NORMAL 8K` / `PHYSICAL CARTRIDGE BOOTED`.
- **Flash**, over REST while paused:

  | Run | Slot option | Unlock | autoselect | erase (DQ6 toggles) | program / read back | other |
  |---|---|---|---|---|---|---|
  | ef-save | `save=exit.crt` | 11-bit | `01 a4` ROML and ROMH | sector 83, ROMH chip 673 | 128 bytes, exact | long unlock works; `cart-save` and `exit.crt` equal the flash |
  | mb-rw | `rw` | `AAA/555` | `c2 58` | sector 59 | 64 bytes, exact | written back while running, `.bak` holds the original, no temp file left |
  | gmod2-both | `flash-decode=both` | 11-bit | `01 a4` | sector 55 | 64 bytes, exact | long unlock works; `cart-save` equals the flash |
  | gmod2-15 | `flash-decode=15` | 15-bit | `01 a4` | sector 55 | 64 bytes, exact | short unlock refused (reads `aa b1`) |
  | ef-15 | `flash-decode=15` | 15-bit | `01 a4` ROML and ROMH | sector 83, ROMH chip 673 | 128 bytes, exact | short unlock refused (reads `88 8f`) |

- **Preference:** Auto shows the Normal 8K cartridge; `Cartridge Preference=Internal` + `machine:reboot` shows BASIC
  `READY.` (bus internal 0x0F, external 0x00).
- **MCP:** `emu_start` with `cart_slot {mode: "save", flash_decode: "15"}`, `emu_cart_info` (`flash_decode: 15`,
  `persist: save=…`), `emu_cart_save` (`PASS`, content equals the source), `emu_stop` graceful; the `save=` file is
  written at exit.
- **Real tools:** the TREX CRT Tool dump above.
- **Unit tests:** `c64-bridge` slot tests (types and models, EasyFlash registers and CART_DETECT, bus sharing and
  bridge mirroring, flash erase/program and CRT image, reset keeps flash time, flash decode, EAPI and C64MegaCart
  banks, new packets and EEPROM round trip, u64-logic memory, DMA while stopped), `ue2emu` spec parsing, CRT write with
  backup, control `cart-info`/`cart-save`, C64 port CART_DETECT.

## Limits

- **Closed top level.** Bus sharing, bridge mirroring and CART_DETECT follow the firmware and the cartridge VHDL; the
  U64-II's own expansion-port logic is not public. Routing is live, not latched at cartridge init (the Auto/External
  DMA difference above).
- **Bridge:** only bit 0 ("Writes") of C64_BUS_BRIDGE is modelled; reads are never mirrored.
- **No port timing:** no bus contention, no PHI2 or address-setup timing; two data drivers are simply ANDed. What
  is modelled is the price of a DMA byte through the C64 memory window, because a flash erase is timed in C64
  cycles and a firmware that polls over DMA has to reach the due cycle. Measured on a real C64 Ultimate (3.15)
  over `machine:readmem`, a byte costs **3.26 us**, about three cycles; the emulator charged 1.61 us of firmware
  instructions and nothing for the bus, so the upstream cart tool's chip erase -- 6 000 000 polls, 20 s on the
  device -- ran out in under 3 s here and never saw the chip finish. `time::DMA_BYTE_CLOCKS` adds the remainder;
  the same measurement now reads 3.25 us. `--log cart` shows the cycle a cartridge is handed, which is the only
  place that clock is visible.
- **Freeze button** of a physical freezer (Action Replay, Retro Replay in the slot) is not wired; the U64 freeze
  button reaches the internal cartridge as before.
- **GMod4** (type 87) and **EasyFlash XL** (232) are served but not exercised by the acceptance; GMod4's SPI flash is
  not written back.
- **Write-back** needs a clean exit for the final state; a killed emulator keeps what the last debounced write saved.
  `cart-save` and write-back rewrite the CRT: the source's packets are updated in place and banks that became non-empty
  get new packets, so the file is content-equal but not byte-equal to a dump written by another tool.
- **DMA** lands on instruction boundaries as for the internal cartridge (docs/status/carts.md).
- **`,rw` backup and temp files:** `NAME.bak` is refreshed at the first write of every run, so after two runs that
  changed the cartridge it holds the state at the start of the last run, not the file as first inserted. The copy to
  `NAME.bak` is not atomic. A SIGKILL between creating `.NAME.tmp-PID` and the rename leaves that hidden temp file
  behind (the CRT itself stays whole); nothing removes it later.

## Checks on an erased flash

Every flash with the C64 ROMs below was made by `--c64-roms` on an erased image (no menu setup).

| Check | Result |
|---|---|
| `scripts/cart-slot-acceptance.py` | 72 PASS, 0 failed checks |
| `--c64-roms` + `--cart-slot` on an erased flash | Magic Desk: `c64-roms: … formatted`, 3 × written, BASIC `READY.` with `30719 BASIC BYTES FREE` (the 8 K cartridge is mapped), REST dump 16/16 banks equal, source hash unchanged |
| MCP, separate client | `emu_start {flash: <new file>, c64_roms: true, net: true, cart_slot: {mode: "save", flash_decode: "both"}}` passes `--c64-roms --cart-slot --net`; MegaByter boots BASIC; `emu_cart_info` MegaByter / trx64-flash / 128 banks / 8K / bus_external 0x0F; a dump through `emu_rest` (pause, writemem, readmem with `save_body_to`) 0/128 banks differ; `emu_cart_save` equals the source; `emu_stop` writes `save_path`; source unchanged. Refused: `c64_roms` with `flash: "none"`, `mode: "save"` without `save_path`, `--cart-slot` in `extra_args` |

Independent spot checks (own CRT parser and REST/control client; bank registers as in TRX64's mappers):

- **REST dumps** (pause, `$DE00`/`$DE02`/`$DF00` by writemem, 8 K windows by readmem, every bank of the chip, missing
  flash banks expected as `$FF`): EasyFlash 128/128 windows (ULTIMAX, ROML `$8000`, ROMH `$E000`), Ocean 128 K 16/16
  (the `$A000` mirror equals ROML 16/16), Ocean 512 K 64/64, MegaByter 128/128, C64MegaCart 256/256, GMod2 64/64. Every
  run exit 0, source hash unchanged, no files next to the source.
- **EasyFlash flash** (default `ro`): autoselect `01 a4` on ROML and ROMH, reset returns the CRT bytes; sector erase of
  ROML sector 4 (banks 32-39, holds bank 37): DQ6 toggled 42 times, then 8/8 banks `$FF`, bank 0 untouched; 64 bytes
  programmed into ROML bank 37 and 16 into ROMH bank 20, read back exact; `cart-info` `dirty: yes`, `unsaved: yes`;
  `cart-save` equals memory in 128/128 windows and equals the CRT plus exactly those changes. Source hash unchanged
  while running and after SIGKILL; the source directory holds only the CRT.
- **`,rw` under SIGKILL:** kills 0.07-0.98 s after programming: the CRT is either the old file or the new one with the
  programmed bytes, always a complete CRT; `.bak` is written at the first write-back. Eight kills timed to the moment
  `.NAME.tmp-PID` appears (between temp write and rename): CRT whole and unchanged, `.bak` the original, one hidden temp
  file left per kill.
