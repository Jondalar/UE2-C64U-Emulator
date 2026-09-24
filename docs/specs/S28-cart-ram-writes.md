# S28 — Cart RAM takes its writes whatever the PLA maps

**Status:** built (2026-09-23).

**Owns:**
- `crates/c64-bridge/src/cart.rs`: `CartLogic::writes_ram_by_address`, `snoop_ram_write`; the `RamSnoop` device
- `crates/c64-bridge/src/slot.rs`: the same for a physical logic cartridge
- `crates/c64-bridge/src/lib.rs`: `install_cart` puts `RamSnoop` on the port while such a cartridge is in
- `docs/status/carts.md` (TRX64 gap 6), `docs/status/gaps.md`

**Reads:** `fpga/cart_slot/vhdl_source/slot_slave.vhd:183-199`, `all_carts_v5.vhd:679-761`, TRX64 v0.8.8
`full.rs:1066-1133` (`FullBus::write`), `full.rs:872-884` (`port_snoop`), `expansion.rs:95-110`, `lib.rs:1647-1690`.

## 1. The gap

TRX64 calls `CartMapper::write` for `$8000-$9FFF` only while the PLA maps ROML there, and for `$A000-$BFFF` only
while it maps ROMH. The FPGA writes cart RAM by address alone: `slot_slave.vhd` starts a DDR write on `RWn = 0`,
`PHI2 = 1` when `allow_write` is set, with no look at ROMLn/ROMHn or EXROM/GAME. `allow_write` comes from
`all_carts_v5.vhd`, which takes into account the logic, its mode bits and the address, but not the lines. Writes made while the
window is banked out (`$01`, 8K mode's `$A000`, the lines off) are missed today:

| Logic | Written (all_carts_v5.vhd) | Condition |
|---|---|---|
| Action Replay, Retro Replay, Atomic Power (`c_action`) | `$8000-$9FFF`; RR mode `x10` with `variant(1)`: `$A000-$BFFF` | `mode_bits(2)` (RAM on) |
| Super Snapshot 5 | `$8000-$9FFF` | `mode_bits(1:0) = 00` |
| Pagefox | `$8000-$BFFF` | `mode_bits(1:0) = 10` |

`CartLogic::map` already mirrors these conditions (cart.rs `map`); only the call is missing.

## 2. The hook

TRX64's expansion-port snoop (`ExpansionDevice::snoop_addresses` / `snoop_write`) is called from `FullBus::write`
before the banking dispatch, for every write cycle to a snooped address: CPU writes, the RMW dummy write-back, DMA
and host writes. That is the FPGA's view: the slot sees every bus write cycle.

- `RamSnoop` holds the cart and slot handles and snoops `$8000-$BFFF`. `snoop_write` hands the write to the internal
  logic when the internal cartridge serves ROM windows (`BUS_ROM`), and to a physical logic cartridge when that one
  does — the gate `Slot::write` already applies.
- `CartLogic::snoop_ram_write` writes DDR when `map` says `(Map::Ram, allow_write)`. ROM-window writes that are not
  RAM (EasyFlash's flash) keep their path through the mapper.
- A write into a mapped window reaches the RAM twice, first by the snoop and then by `CartMapper::write`, with the same
  byte at the same offset. The second one changes nothing.
- The snoop writes nothing to C64 RAM and consumes nothing; `FullBus::write` goes on as before.

## 3. When it is on

`install_cart` runs after every change of the cartridge type, the forced ULTIMAX decode and the physical cartridge.
It attaches `RamSnoop` with `attach_expansion_also` while the internal logic or a physical logic cartridge is one of
the three, and removes it with `detach_expansion_device::<RamSnoop>()` otherwise. Without these carts no address is
snooped and `FullBus::write` runs as before.

## 4. Checks

- `CartLogic`: AR in RAM mode, a snooped write lands in cart RAM; SS5 and Pagefox in their RAM modes and out of
  them; RR's `$A000` RAM mode.
- Backend: an Action Replay in RAM mode with `$01 = $35` (ROML banked out): a write to `$8123` reaches cart RAM and
  C64 RAM; with the cart type changed to a normal 8K cartridge the snoop is gone.
- The carts smoke (27 carts, AR frozen).

## 5. Not in this spec

- `$E000-$FFFF` (`kernal_area_i` in `slot_slave.vhd`): the KERNAL replacement, not a cartridge RAM.
- GeoRAM, KCS and EasyFlash RAM live in `$DE00-$DFFF`, which every write reaches already.
