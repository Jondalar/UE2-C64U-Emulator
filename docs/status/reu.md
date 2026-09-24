# REU — the RAM Expansion Unit

The C64's REU behind `C64_REU_ENABLE` and `C64_REU_SIZE`, served by TRX64's `Reu` over **the firmware's own DDR**
(TRX64 Spec 854, "the expansion RAM the host owns"). Part of M7 (`docs/status/c64.md`); the registers themselves are
S14 §8's cart-register table, which listed both as latches until now.

## Why the RAM is not the REU's

On the U64 the REU does not own its memory. It is DDR at `REU_MEMORY_BASE 0x1000000`, `REU_MAX_SIZE 0x1000000`
(`c64.h:14-15`), and the firmware writes that region with its own CPU — a preload image goes straight there
(`reu_preloader.cc:104`), and `MENU_C64_SAVEREU` reads it back out (`c64.cc:1606`). A device that allocated sixteen
megabytes of its own would give two copies of the REU, and every preload would land in the one the C64 never reads.

TRX64 0.7.0 answers that with `ExpansionRam`, a store the device holds instead of a `Vec<u8>` it owns. The bridge
implements it over guest DDR.

## How it works

| Piece | Where |
|---|---|
| `C64_REU_ENABLE` (+0x8) and `C64_REU_SIZE` (+0x9) routed to the backend | `crates/ue2-core/src/devices/c64.rs` `cart_write` |
| `C64Backend::set_reu_enabled` / `set_reu_size_kb` / `reu_attached` | `crates/ue2-core/src/c64host.rs` |
| `ExpansionRam` over guest DDR at 0x1000000 | `crates/c64-bridge/src/reu.rs` (`ReuRam`) |
| Attach, detach, resize on TRX64 | `crates/c64-bridge/src/lib.rs` |

- **Size.** `C64_REU_SIZE` 0..7 is `128 << n` KiB, 128 KB … 16 MB — the firmware's `reu_size` table (`c64.cc:60`),
  written at `c64.cc:315`. The register resets to "111" (16 MB); the firmware's own default setting is 2 MB.
- **Enable.** `C64_REU_ENABLE` 1 attaches `Reu::new_with_store` with `Machine::attach_expansion_also`, 0 detaches with
  `Machine::detach_reu`. Both at runtime: `set_emulation_flags` (`c64.cc:309-318`) runs from `effectuate_settings`
  whenever the setting changes, not only at reboot. Size first, then enable — the order the firmware writes them.
- **Enable toggle.** `set_emulation_flags` writes the enable 0 and then 1 within microseconds on every cartridge
  change, and `restoreCart` does that after a PRG start while the program already runs (`c64_subsys.cc`, "Resuming.."
  before "Cart got disabled, now restoring"). A program that probes the REU at once, as UltimateDemo2026 does at
  16 MHz turbo, is in the middle of a transfer then. The bridge is lenient here:
  - the REU stays on the bus for 10 ms after a 0 (`REU_OFF_GRACE`), so the firmware's pulse never takes it away. On
    hardware the enable gates the decode, a gap a program rarely hits;
  - an REU off for longer is kept (`detach_reu` returns it) and comes back with its REC registers when switched on, as
    the FPGA's gated REU would; a C64 reset while it is off still resets it.

  Before UE2 0.3.5 the toggle built a new REU with reset registers, and such a program reported "REU too small" or
  "REU not detected".
- **The store is the lease, doubled.** TRX64 *holds* the store for the life of the device, while `C64Port` only
  *lends* guest DDR for the duration of one access (`C64Backend::lend_ddr`). A borrow cannot be held, so `ReuRam` is a
  second shared pointer cell of the same shape as `CartLogic::set_ddr`'s, updated on the very same lend. Every path
  that runs the C64 — every cart/DMA/UCI/drive access and `C64Port::tick` — lends first, so a transfer always has DDR
  under it.
- **Nothing lent is not an error** (854 D3). Between those accesses the store has nothing behind it, and since
  TRX64 0.7.1 it can say so: `ExpansionRam::read` returns `Option<u8>`, and `ReuRam` answers `None` — "nothing backs
  this address" — so the **device** supplies the byte from its own floating bus. A write is dropped. `None` covers
  three states, and only one is a hardware fact: an address above the fitted size (a real REU has no DRAM there
  either), against nothing lent and an offset past the end of the lease, which are host-side gaps. `Some` is the only
  real read. No panic, no out-of-bounds.
- **Resizing keeps the contents** (854 D4). Only the REC's idea of how much DRAM is fitted moves; the bytes are DDR
  and are not touched, so the firmware moving "Size" does not drop a preloaded image.
- **A C64 reset resets the REC, not the RAM.** TRX64 0.7.1 carries the port's /RESET line to the device itself
  (`ExpansionDevice::reset`, which `Reu` overrides), so `Machine::warm_reset` does it and **the bridge's own
  compensation is gone**. The REC returns to power-on and the DDR keeps every byte (`reu.c:602-615`).
- **Nothing else moves.** The UCI block is the machine profile's own device in a separate slot (Spec 852), and the
  cartridge is `Machine::cartridge`; `attach_expansion_also` / `detach_reu` touch neither.

## GeoRAM stays where it was

`C64_REU_ENABLE` is set only for the REU. The menu's third choice, "GeoRAM Mode", takes a different branch
(`c64.cc:1256`): it selects cartridge type `0x1F`, which the U64 cart logic already serves from the same DDR region
(`crates/c64-bridge/src/cart.rs` `GEORAM_BASE`, `Layout::GUEST`). So only 854's REU half is used here, and TRX64's
GeoRAM device is not attached.

"REU Size" is the GeoRAM size too: `size_ctrl` is `C64_REU_SIZE` (`slot_server_v4.vhd:809`) and masks the bank
registers when they are written (`all_carts_v5.vhd:144-152, 642-645`), so banks past the size wrap. The bridge had no
mask until S35; `set_reu_size_kb` now passes the size to the cart logic. `scripts/smoke-georam.ctl` checks it at
512 KB: block 37 reads block 5.

## Verified

Release binary, real firmware (`ultimate.elf`), `--headless --speed max`, scratch flash images seeded with
`--c64-roms`. The C64-side program stashes two bytes from `$C000` into REU `$000000`, clears `$C000`, fetches them
back and prints them, then prints the REU status register at `$DF00`.

| Check | Result |
|---|---|
| `cargo build --workspace` | clean, no warnings |
| `cargo test --workspace` (`UE2_FIRMWARE` set) | 403 passed, 1 ignored, 0 failed |
| `cargo build -p ue2emu --no-default-features` | clean; `--c64 none` unaffected (the trait methods default to no-ops) |
| New tests | 12: 6 in `c64-bridge` `reu` (nothing lent, lent, above the fitted size, short lease, lease ends, shared clone), 4 in `c64-bridge` (attach/resize/detach, a real transfer, a transfer with nothing lent, reset), 2 in `ue2-core` (the size table, both registers reaching the backend) |
| Firmware, REU **Disabled** (default) | BASIC prints ` 0  0`; `PEEK($DF00)` = ` 0`. Nothing answers `$DF00-$DF0A` |
| Firmware, REU **Enabled** through Memory Configuration → "RAM Expansion Unit", saved to flash | BASIC prints ` 65  66` — the bytes went into the REU and came back — and `PEEK($DF00)` = ` 80` = `$50`, the REU status register. `Writing config store 'C64 and Cartridge Settings' to flash` on the console |
| Cartridge regression, `scripts/smoke-c64-carts.ctl` (`--flash` with `--c64-roms`, `--sd` with the test CRTs, `--usb-keyboard`) | exit 0, 27/27 `… PASS`, `ACTION REPLAY FROZEN`, 2 × `Loading SID`, 2 × `Bytes loaded`, no `Time out!`. 153.6 s emulated, 134 MIPS. `lend_ddr` now also updates the REU store, and the cartridge path is unchanged |

The menu path is F2 → Memory Configuration → "RAM Expansion Unit" → Enabled; "Size" sits next to it and the store
writes both registers on save. In the config browser the cursor skips the blank separators and typed letters do
**not** seek (they log `Unhandled key`), so a script navigates it with `down` alone: 4 × down reaches Memory
Configuration, then RIGHT, then 4 × down reaches RAM Expansion Unit.

### Re-verified against TRX64 0.7.1

Re-run after deleting the bridge's REC reset and switching the store to `Option<u8>` (2026-09-16, TRX64
`fix-cia-tod-and-port-reset` `85721a6`):

| Check | Result |
|---|---|
| `cargo build --workspace` | clean, 0 warnings |
| `cargo test --workspace` (`UE2_FIRMWARE` set) | 403 passed, 1 ignored, 0 failed |
| `cargo build -p ue2emu --no-default-features` | clean |
| `a_c64_reset_resets_the_rec_and_keeps_the_ram` | passes with the bridge's own reset **deleted** — TRX64's `warm_reset` puts the REC back to power-on and the DDR byte at `REU_BASE + 0x100` is still `0x77` |
| Firmware, REU enabled through the menu, BASIC stash/fetch | ` 65  66  80` — the bytes went into the REU and came back, and `PEEK($DF00)` = `$50`. Console: `Writing config store 'C64 and Cartridge Settings' to flash..Page: 3 done.` 57.4 s emulated, 133 MIPS |
| Cartridge regression, `scripts/smoke-c64-carts.ctl` | exit 0, 27/27 `… PASS`, 0 `FAIL`, `ACTION REPLAY FROZEN`, 2 × `Loading SID`, 2 × `Bytes loaded`, no `Time out!`. 153.588 s emulated, 130 MIPS |

### Preload and Save REU (S35)

`scripts/smoke-reu.ctl`, run by `scripts/smoke-c64-all.sh`: a 128 KB image as `preload.reu` on a `--usb-dir` stick,
`REU Preload` on (`scripts/smoke-reu.cfg`). The preloader loads it when USB0 appears (`reu_preloader.cc:52-117`);
BASIC fetches 8 bytes from REU `$010203` by DMA and checks them, then stashes 4 bytes to `$000100`. "Save REU Memory"
from the F5 menu (`c64_subsys.cc:281-327`) writes `memory.reu`, which must equal the image plus those 4 bytes.

### Firmware DDR is the store

That the region the firmware writes is the region the C64 reads is covered by
`a_transfer_moves_bytes_between_c64_ram_and_the_firmwares_ddr` (`crates/c64-bridge/src/lib.rs`): the C64 stashes, the
test asserts the bytes appear in the host's DDR at `REU_MEMORY_BASE`, the host then changes one of them the way a
preload does, and the C64's fetch brings that changed byte back.

## Known gaps

- **IO2 precedence with a cartridge.** 854 §7: on a read a port device beats a cartridge that also decodes IO2, and
  every device sees a write. An REU enabled alongside a cartridge that uses `$DF00` (Action Replay, Retro Replay) is
  therefore a conflict the firmware avoids by configuration (`CART_PROHIBIT_ALL_BUT_REU`), not something the bridge
  arbitrates.
- **No snapshot of REU RAM.** By construction (854 D7): the bytes are the host's memory image, which has its own
  persistence. `ram_is_owned()` is false and a cloned machine gets no copy.
- **GeoRAM is still the cart logic's**, not TRX64's device (above), so the two cannot be enabled at once in TRX64's
  sense — which matches the firmware, where the setting is one enum.

## TRX64 findings (Spec 854) — both fixed in 0.7.1

Reported upstream, confirmed there on 2026-09-16, and **fixed on TRX64 `fix-cia-tod-and-port-reset` (`19dfbd6`)**,
which this bridge is now built against:

- **The port reset was a TRX64 defect, not a design choice.** A real expansion port carries /RESET, so an REU sees a
  C64 reset: its REC registers go back to power-on while the DRAM keeps its contents (VICE `c64carthooks.c:2412`
  calls `reu_reset`). TRX64's `cold_reset` reached `Machine::cartridge` only. 850's blanket "no reset calls a device"
  is right for UCI (`command_protocol.vhd` clears that block on the FPGA reset alone) and wrong for the REU. The fix
  is the per-device `ExpansionDevice::reset()` asked for: it defaults to nothing, so UCI keeps 850's behaviour, and
  `Reu` overrides it. **The bridge's own REC reset on a reset release has been deleted** — TRX64 does it now.
- **`fn read(&self, off) -> u8` could not say "nothing here",** and is now `Option<u8>`. One signature change here.

The two findings themselves, both confirmed while building this:

1. **The trait is held; our DDR is only lent.** 854 D1 is right that a `'static` device cannot hold a `&mut [u8]`,
   but the consequence for a host that lends its memory per access is that the store cannot wrap the borrow — it has
   to be an independently shared pointer cell that the host rewrites on every lend. That is a second unsafe cell in
   the bridge with exactly the lifetime argument the first one (the cartridge's) already carries. Worth a sentence in
   §5, because the obvious reading of D1 ("implement the trait over your own memory") suggests reusing the existing
   borrow, which does not compile.
2. **`fn read(&self, off) -> u8` cannot say "nothing here".** With no DDR lent the store had to invent a byte, and the
   device's own floating-bus value was right there but unreachable from the store. We returned `0xFF` because that is
   `Reu`'s `floating_bus` **power-on** value, and argued the two therefore agreed. **They did not**, and switching to
   `Option<u8>` proved it: `floating_bus` is a *latch*, not a constant — `dma_host_to_reu` ends with
   `self.floating_bus = value`, the last byte it drove (`reu.rs`). Our own test
   `a_transfer_with_nothing_lent_…` failed on the first build against 0.7.1, reading `0xAF` where it had asserted
   `0xFF`: the stash of `0xA0..0xAF` was dropped for want of DDR but still drove the bus, so the device's latch held
   `0xAF` while our invented byte still said `0xFF`. The old code was wrong, not merely fragile, and the wrong value
   was written into a passing test. This is the exact silent disagreement the `Option` was asked for.

## Build note

The two changes above need **TRX64 0.7.1**, which is public on TRX64's `main` as `c3d34bb` — there is no tag and no
release for it, so the pin names the rev. `crates/c64-bridge/Cargo.toml` points there, and an ordinary
`cargo build` is enough.

Building against a local TRX64 checkout instead is the usual `.cargo/config.toml` route in `docs/status/install.md`,
"TRX64 dependency", with the same trap as before: when the local crate's version equals or differs from the pinned
one in the wrong way, cargo keeps the git source until `cargo update -p trx64-core` switches it, and
`cargo tree -p c64-bridge -i trx64-core` is what proves which source is in the graph.

Building against a local TRX64 checkout instead (for TRX64 work) is the usual `.cargo/config.toml` route described in
`docs/status/install.md`, "TRX64 dependency". One trap worth repeating: when the local crate's version differs from
the pinned one, cargo reports "patch … was not used in the crate graph" and silently keeps the GitHub rev;
`cargo update -p trx64-core` switches between the two.
