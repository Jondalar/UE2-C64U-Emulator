# S43 — With `--trx-io` the app sees the TRX bitstream's reSID8 control, a 1541-only drive A and the expansion-port slot block, as on the board

**Status:** DONE (2026-10-25): S1-S13 green, the unit tests
in `cargo test`, S5 and S12 in `scripts/smoke-trx-s43.sh` with release b151154b. Branch `s37-trx-pixel-layer`, like
S37-S42.
**Repo:** UE2 (u64-emulator). Hardware reference: TRX64-Ultimate `designs/c64_basic` (main, 00f370c): `trx_io.v`,
`trx_sys.v`, `slot_port.v`, `c64_core.v`, `ultimate_sys.vhd`, `drive_unit.vhd`; Gideon's
`cores/gideon/fpga/1541/vhdl_source/drive_registers.vhd` as vendored there. App: `software/ultimate-app` at 234a0abb.
**Origin:** TRX64-FW session, 2026-10-25, for U3 (the generic core app replaces Gideon's app,
TRX64-Ultimate `docs/specs/U3-generic-app-replaces-gideons-app.md`): release b151154b under `--trx-io` differs from
the board in these three places. Owner: "habe ich ja, mach das. Wir brauchen den Emulator jetzt für die UX/UI und um
das Featureset abzugleichen."
**Depends on:** S17 (UltiSID on reSID), S37 (`--trx-io`, the far pages), CARTSLOT (`--cart-slot`,
`docs/status/cart-slot.md`).

---

## §1 The problem

**Who:** the TRX64-FW session building U3, and the owner checking the app's feature set against the board.

**What they are trying to do:** run the release app under `--trx-io` and see the menus, the drive and the cartridge
tools the board shows.

**What the board does** (code read, 00f370c):

1. **reSID8.** `c64_core.v:887-891`: the default build (`SID_MULTI`, `build.tcl:82`) sets `sid_resid8`, so `CAPS3`
   (page 0 +0x006, `top.v:800`) reads 0x01. `trx_io.v` keeps the reSID8 filter control at page 0 +0x040-0x04D for
   UltiSID 1 / 2 (engine 1 / 2), read back masked: 040/041 filter source b1:0, 042/043 b0 resonance Auto and b1 digis
   Auto, 044-045/048-049 6581 base (9 bit), 046-047/04A-04B 6581 offset (11 bit), 04C/04D b0 bypass. Reset values: 0,
   3, 245, 0x4F4 (-780), 0. They feed `trx_sid.v`'s `cfg_rs8`; bypass is reSID's `enable_filter(false)`.
   The app builds its reSID8 menus when CAPS3 b0 is set (`trx_io.cc` `trx_io_resid8`).
2. **Drive A.** `ultimate_sys.vhd:62-64`: drive A is `drive_unit` (Gideon's `c1541_drive`), drive B is not there.
   `c1541_drive.vhd:308` instantiates `drive_registers` without a generic map, so `g_multi_mode` is its default
   `false` (`drive_registers.vhd:12`): DRIVETYPE (+0xD) ignores writes and reads 0 (`:89-92`, `:121-124`). The app's
   probe (`c1541.cc:119-123`) writes 2, reads 0, so it hides "Switch to 1541/1571/1581" and limits the drive type to
   1541.
3. **The slot block** (`slot_port.v`, `trx_sys.v` page 7, `c64_core.v:598-660`):
   - `SLOT_CTRL` +0x7000 b1 `ROM_WR_EN`, RW, 0 at configuration, no reset term (`trx_sys.v:217`, `:366`).
   - `SLOT_STATUS` +0x7004 (`slot_port.v` `status`): b0 GAME pin, b1 EXROM, b2 DMA, b3 IRQ, b4 NMI (1 = high), b5
     port RESET low, b11:8 EXT mask in effect, b15:12 INT mask in effect, b16 armed, b17 fault, b18 `ROM_WR_EN`,
     b23:20 fault cause, b25:24 the SID socket enables, b26 a socket armed, b31 1.
   - The masks in effect take `C64_BUS_INTERNAL`/`EXTERNAL` (0x1018002B/2C) only while the CPU is in reset or the
     machine is paused. The port serves ROM (GAME/EXROM, ROML/ROMH) when EXT b2 and not INT b2 and not the app's
     Ultimax; it serves IO1/IO2 strobes when EXT b0/b1; it answers IO1/IO2 reads when also not (INT b0/b1 and a CRT
     configured); its IRQ/NMI count when EXT b3. Where the port does not answer, the internal cartridge does: the
     internal cartridge has no bus mask (`c64_core.v:598-601`).
   - `armed`: at each C64 cycle (the gate open: also in the CPU's reset, and app DMA cycles in the pause) it takes
     `present`, a resource routed; only the core's own reset (`rst`) clears it. Only an armed port drives strobes:
     reads, IO writes and, with `ROM_WR_EN`, ROML/ROMH in a write cycle.
   - Fault: the DMA pin low while armed, or a turnaround rule broken; idle until the next C64 reset.
   - Reset: our own reset drives the port's RESET and holds the C64 32 µs longer (`rcnt`, 1024 clocks); a cartridge
     pulling RESET resets the C64 while a resource is routed (`reset_req`: `present`, not `armed`).
   - trex_crt (`cart_base.c` `cart_session_open`, T57 §5) needs b31, an armed port with EXT ROM and not INT ROM from
     the last reset, then after its bus set-up ARMED | EXT IO1/IO2 | EXT ROM; it raises `ROM_WR_EN` around writes and
     resets the C64 at the end when it touched the cartridge.

**What UE2 does today:** CAPS3 and page 0 0x040-0x04D read 0, writes go nowhere. DRIVETYPE reads back 1 or 2 on
drive A, so the app offers 1571/1581. Page 7 reads 0, so trex_crt stops with "This bitstream has no cartridge port
support"; the `--cart-slot` cartridge follows the U64-II model (masks at once, internal cartridge masked, both sides
ANDed, no write gate).

**Parts (MECE):**
- **P1 reSID8 registers:** CAPS3 b0 and page 0 0x040-0x04D as `trx_io.v` reads and writes them.
- **P2 reSID8 effect:** bypass switches the filter of every reSID of that UltiSID; the other fields are kept for the
  menus only (owner, 2026-10-25: the 8 UltiSIDs on reSID stay as they are).
- **P3 Drive:** DRIVETYPE has no multi-mode on either drive unit.
- **P4 Slot registers:** SLOT_CTRL and SLOT_STATUS on page 7.
- **P5 Slot behaviour:** the masks in effect, arming, the port's share of the bus beside the internal cartridge, the
  write gate, the reset.
- **P6 Unchanged without `--trx-io`:** the U64-II models stay as they are.

## §2 Scenarios

```gherkin
Scenario: the app finds reSID8 (S1, P1)
  Given UE2 runs with --trx-io
  When  the app reads CAPS3, then page 0 0x040-0x04D
  Then  CAPS3 is 0x01 and the bytes read 00 00 03 03 F5 00 F4 04 F5 00 F4 04 00 00

Scenario: the filter control reads back masked (S2, P1)
  When  0xFF is written to each of 0x040-0x04D
  Then  they read 03 03 03 03 FF 01 FF 07 FF 01 FF 07 01 01
  And   an FPGA reset puts the reset values back

Scenario: bypass reaches the engines of its UltiSID (S3, P2)
  Given UltiSID 1 and 2 have reSIDs
  When  0x04C is written 1
  Then  every reSID of UltiSID 1 runs with its filter off, UltiSID 2's keep theirs, the registers they hold stay
  And   writing 0 turns UltiSID 1's filters on again

Scenario: the drive has no multi-mode (S4, P3)
  Given --trx-io
  When  the app writes 2, then 1, to drive A's DRIVETYPE
  Then  it reads 0 both times and the drive stays a 1541; drive B behaves the same

Scenario: the release app's drive menu (S5, P3)
  Given the release app under --trx-io
  Then  drive A's menu has no "Switch to" entries

Scenario: the slot block is there (S6, P4)
  Given --trx-io, no cartridge in the port, the C64 out of reset with masks INT 0xF EXT 0x0
  Then  SLOT_STATUS reads 0x8000F01F: b31, INT 0xF, pins high, not armed
  And   SLOT_CTRL reads 0; after a write of 0x02 it reads 0x02 and SLOT_STATUS has b18

Scenario: the masks take effect in reset or the pause (S7, P5)
  Given the C64 runs
  When  C64_BUS_EXTERNAL is written 0x0F
  Then  SLOT_STATUS keeps EXT 0x0 until the machine is paused or the CPU is reset; then it shows 0xF

Scenario: arming (S8, P5)
  Given EXT 0x7, INT 0x0 taken in a reset
  When  the C64 runs one cycle
  Then  SLOT_STATUS b16 is set
  And   masks taken in the pause leave it as it was until a cycle (an app DMA cycle) runs

Scenario: the write gate (S9, P5)
  Given an EasyFlash in the port, armed
  When  the flash's program sequence is written to ROML with ROM_WR_EN 0
  Then  the flash is unchanged and the IO1 bank register writes still land
  And   with ROM_WR_EN 1 the byte is programmed

Scenario: the port beside the internal cartridge (S10, P5)
  Given an internal cartridge configured and a cartridge in the port
  When  the masks are INT 0x0 EXT 0x0
  Then  the internal cartridge serves ROM and IO (it has no mask)
  And   with EXT ROM and not INT ROM the port's GAME/EXROM and ROM data replace the internal cartridge's
  And   with EXT IO1 and INT IO1 and a CRT configured, IO1 reads come from the internal cartridge, the strobe still
        reaches the port

Scenario: the reset (S11, P5)
  When  the app releases the C64's reset
  Then  the C64 leaves reset 32 µs later
  And   an EasyFlash 3 pulling RESET resets the C64 only while a resource is routed to the port
  And   with the core absent there is no stretch

Scenario: trex_crt on the TRX core (S12, P4+P5)
  Given the release app, --trx-io, --cart-slot with an EasyFlash CRT, Cartridge Preference Auto
  When  the dumper, the writer (verify) and the inspector run from the freeze menu
  Then  each reports the core "TRX64 Ultimate", the dump equals the CRT, the written image verifies, and the session
        closes with ROM_WR_EN as found and a C64 reset

Scenario: unchanged without --trx-io (S13, P6)
  Then  CAPS3, page 0 0x040-0x04D and page 7 are not there, DRIVETYPE reads back, the U64-II slot model is unchanged
```

| # | Solves | Scenario | Test | Where it runs |
|---|---|---|---|---|
| S1 | P1 | reSID8 found | `trx::tests::caps3_and_the_resid8_reset_values`, `trx::tests::without_resid8_caps3_is_0_and_the_bypass_goes_nowhere`; smoke `menus` | `cargo test` / by hand |
| S2 | P1 | read back masked | `trx::tests::the_resid8_registers_read_back_masked` | `cargo test` |
| S3 | P2 | bypass | `sid::tests::bypass_turns_the_filter_of_one_ultisid_off`, `c64::tests::rs8_bypass_reaches_the_backend` | `cargo test` |
| S4 | P3 | no multi-mode | `drives::tests::without_multi_mode_drivetype_reads_0` | `cargo test` |
| S5 | P3 | drive menu | `scripts/smoke-trx-s43.sh` | by hand, release `.ue2` |
| S6 | P4 | slot block | `trx::tests::page_7_is_the_slot_block`, `c64::tests::the_slot_block_reports_through_page_7`, `slot::tests::trx_masks_take_in_reset_or_pause_and_arm_at_a_cycle` | `cargo test` |
| S7 | P5 | masks in effect | `slot::tests::trx_masks_take_in_reset_or_pause_and_arm_at_a_cycle` | `cargo test` |
| S8 | P5 | arming | `slot::tests::trx_masks_take_in_reset_or_pause_and_arm_at_a_cycle` | `cargo test` |
| S9 | P5 | write gate | `slot::tests::trx_rom_writes_need_rom_wr_en` | `cargo test` |
| S10 | P5 | port and internal | `slot::tests::trx_port_beside_the_internal_cartridge` | `cargo test` |
| S11 | P5 | reset | `c64::tests::the_port_holds_the_reset_32_us`, `slot::tests::trx_cart_reset_needs_a_routed_port`; `scripts/smoke-core-swap.sh` (no stretch without the core) | `cargo test` / by hand |
| S12 | P4, P5 | trex_crt | `scripts/smoke-trx-s43.sh` | by hand, release `.ue2` |
| S13 | P6 | unchanged | the existing `trx`, `drives`, `slot`, `cart-slot` tests; `drives::tests::without_multi_mode_drivetype_reads_0` (without the block); the TRX smokes `smoke-trx`, `-overlay`, `-freeze`, `-ring`, `-rocker`, `smoke-core-dev`, `-swap` | `cargo test` / by hand |

## §3 Decision

- **D1 Page 0 in `Trx`.** CAPS3 and the fourteen bytes are block registers (`trx_io.v`, app clock): `Trx` keeps them,
  the FPGA reset restores them. A write to 0x04C/0x04D also goes to the C64 port as a block op at its clock, which
  tells the backend (`C64Backend::set_sid_bypass`).
- **D2 Bypass by rebuild.** TRX64's `ResidConfig` is fixed at construction, so the bridge builds the reSIDs of that
  UltiSID again with `filter` off or on and replays their registers, as a model change does (S17).
- **D3 DRIVETYPE.** With `--trx-io` both drive units read DRIVETYPE as 0 and ignore writes to it (`MASK` 0 there),
  replacing S37's drive-B-only refusal of the 1581: one rule for the board's `drive_registers`.
- **D4 The slot in the bridge.** The bridge's `Slot` gets the TRX mode (`TrxPort`) with `set_trx_bitstream`: masks in
  effect, `armed`, `ROM_WR_EN`, the app's Ultimax. The bridge knows reset, pause and the debugger's hold already
  (`slot_follow`); every DMA window cycle arms. `C64Backend` gains `slot_status()` and `set_slot_wr_en()`. Without a
  C64 the block reads `0x8000F01F`.
- **D5 Page 7 through the link.** A `SLOT_CTRL` commit is a block op (C64 clock); the C64 port keeps the last
  `SLOT_STATUS` in the link after every sync, bus-mask write, reset change, pause change and slot op, and the block
  reads it from there.
- **D6 Fault never latches.** No modelled cartridge pulls DMA and the model has no transceiver turnarounds, so b17 and
  b23:20 read 0. Stated, not built.
- **D7 The reset stretch.** With `--trx-io` the C64 port holds the C64's reset 1024 dot4x clocks (32.5 µs) after our
  own reset (C64_MODE, the ESP32's hold) lets go, as a third source of `reset_held`, ended at its clock like S42's
  hold; the reset sense and SLOT_STATUS b5 see it. Not while the core is absent (slot_port.v is the core's).
- **D8 `--absent resid8`** (S41 §4 had promised it): the two-SID reDIP build (`TRX_SID=redip2`), CAPS3 0, the RS8
  bytes still read and write, the bypass reaches no engine (`cfg_r` is used only under `SID_MULTI`).

## §4 Out

- reDIP's filter source, 6581 base/offset, resonance Auto and digis Auto change nothing in reSID (P2; owner,
  2026-10-25). The values are kept and shown.
- SLOT_CNT0/1 and SLOT_LAST (+0x7008-0x7010) read 0 (owner, 2026-10-25). Only trxdbg reads them, over JTAG on the
  board; no app does, and UE2 has no JTAG.
- Hot-plug of the port cartridge while running (`--cart-slot` inserts at start).
- The fault guard and its causes (D6), the SID socket bits b24-26 (UE2's sockets are not on the port).

## §5 Decisions taken while building

- **B1 Armed follows every running cycle** (slot_port.v:187-189, `if (rst)` the core's reset, not `cpu_reset`): the
  first draft said a CPU reset disarms; the code does not. `armed` is set wherever a cycle runs: after each run slice,
  at a reset edge, at the pause's release and at every DMA window cycle.
- **B2 The cartridge's reset needs `present`**, not `armed` (`reset_req`, slot_port.v:276).
- **B3 No stretch without the core:** `smoke-core-swap` booted with the core absent and stood in reset forever.
- **B4 The smoke's interfaces:** the menus in the C64U's Commodore interface (`smoke-trx-overlay.cfg`), trex_crt in
  the Ultimate interface (`smoke-trx-ultimate.cfg`, owner 2026-10-25: "Besser ist es das UI auf Ultimate
  umzustellen"), where the file action and the screen dump work; in the freeze interface `screen` shows the overlay,
  not the pixel layer.
- **B5 The app pin:** the app code read here is `software/ultimate-app` at 234a0abb, the commit TRX64-Ultimate pins
  (the FW session, 2026-10-25); the binaries are release b151154b.
