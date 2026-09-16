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
- **The store is the lease, doubled.** TRX64 *holds* the store for the life of the device, while `C64Port` only
  *lends* guest DDR for the duration of one access (`C64Backend::lend_ddr`). A borrow cannot be held, so `ReuRam` is a
  second shared pointer cell of the same shape as `CartLogic::set_ddr`'s, updated on the very same lend. Every path
  that runs the C64 — every cart/DMA/UCI/drive access and `C64Port::tick` — lends first, so a transfer always has DDR
  under it.
- **Nothing lent is not an error** (854 D3). Between those accesses the store has nothing behind it: a read returns
  `0xFF` (the REU's own floating-bus value) and a write is dropped. Same for an address above the fitted size and for
  an offset past the end of the lease. No panic, no out-of-bounds.
- **Resizing keeps the contents** (854 D4). Only the REC's idea of how much DRAM is fitted moves; the bytes are DDR
  and are not touched, so the firmware moving "Size" does not drop a preloaded image.
- **A C64 reset resets the REC, not the RAM.** TRX64's own reset reaches `Machine::cartridge` and not the port
  devices, so the bridge resets the REU on a reset release, as the expansion port's RESET line does (`reu.c:602-615`).
- **Nothing else moves.** The UCI block is the machine profile's own device in a separate slot (Spec 852), and the
  cartridge is `Machine::cartridge`; `attach_expansion_also` / `detach_reu` touch neither.

## GeoRAM stays where it was

`C64_REU_ENABLE` is set only for the REU. The menu's third choice, "GeoRAM Mode", takes a different branch
(`c64.cc:1256`): it selects cartridge type `0x1F`, which the U64 cart logic already serves from the same DDR region
(`crates/c64-bridge/src/cart.rs` `GEORAM_BASE`, `Layout::GUEST`). So only 854's REU half is used here, and TRX64's
GeoRAM device is not attached.

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
writes both registers on save.

### Firmware DDR is the store

That the region the firmware writes is the region the C64 reads is covered by
`a_transfer_moves_bytes_between_c64_ram_and_the_firmwares_ddr` (`crates/c64-bridge/src/lib.rs`): the C64 stashes, the
test asserts the bytes appear in the host's DDR at `REU_MEMORY_BASE`, the host then changes one of them the way a
preload does, and the C64's fetch brings that changed byte back.

## Known gaps

- **The firmware's "REU Preload Image" path was not run end to end.** It needs `REU Preload` enabled plus an image
  file on a mounted volume (`/Usb0/preload.reu` by default, `reu_preloader.cc:84-118`), which is a string setting in
  the menu. The equivalent — a byte the host writes into DDR, fetched by the C64 — is covered by the unit test above,
  but the firmware's own loader task has not been exercised. Neither has `MENU_C64_SAVEREU`.
- **IO2 precedence with a cartridge.** 854 §7: on a read a port device beats a cartridge that also decodes IO2, and
  every device sees a write. An REU enabled alongside a cartridge that uses `$DF00` (Action Replay, Retro Replay) is
  therefore a conflict the firmware avoids by configuration (`CART_PROHIBIT_ALL_BUT_REU`), not something the bridge
  arbitrates.
- **No snapshot of REU RAM.** By construction (854 D7): the bytes are the host's memory image, which has its own
  persistence. `ram_is_owned()` is false and a cloned machine gets no copy.
- **GeoRAM is still the cart logic's**, not TRX64's device (above), so the two cannot be enabled at once in TRX64's
  sense — which matches the firmware, where the setting is one enum.

## TRX64 findings (Spec 854)

Two things worth carrying back upstream, both confirmed while building this:

1. **The trait is held; our DDR is only lent.** 854 D1 is right that a `'static` device cannot hold a `&mut [u8]`,
   but the consequence for a host that lends its memory per access is that the store cannot wrap the borrow — it has
   to be an independently shared pointer cell that the host rewrites on every lend. That is a second unsafe cell in
   the bridge with exactly the lifetime argument the first one (the cartridge's) already carries. Worth a sentence in
   §5, because the obvious reading of D1 ("implement the trait over your own memory") suggests reusing the existing
   borrow, which does not compile.
2. **`fn read(&self, off) -> u8` cannot say "nothing here".** With no DDR lent the store has to invent a byte, and the
   device's own floating-bus value is right there but unreachable from the store. We return `0xFF` because that is
   `Reu`'s `floating_bus` default, so the two agree today — but only by coincidence: if a host set a different
   floating-bus value, or a future REU model drove something else, the store's invented byte would silently disagree
   with the device's. `fn read(&self, off) -> Option<u8>`, with `None` meaning "not backed", would have kept D3 honest
   and cost nothing — `read_from_reu` already has the floating-bus fallback in hand at that exact point.

## Build note

This needs **trx64-core 0.7.0**, which is Specs 853 and 854. It is public: TRX64 v0.7.0, rev `f370a56`, and that is
what `crates/c64-bridge/Cargo.toml` pins, so an ordinary `cargo build` is enough and no patch is needed.

Building against a local TRX64 checkout instead (for TRX64 work) is the usual `.cargo/config.toml` route described in
`docs/status/install.md`, "TRX64 dependency". One trap worth repeating: when the local crate's version differs from
the pinned one, cargo reports "patch … was not used in the crate graph" and silently keeps the GitHub rev;
`cargo update -p trx64-core` switches between the two.
