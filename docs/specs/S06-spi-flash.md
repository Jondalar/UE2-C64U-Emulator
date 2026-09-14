# S06 — SPI flash (NOR, persistent) + overlay-UI config seeding

**Owns:** `crates/ue2-core/src/devices/flash.rs`
**Reads:** `docs/hw/06-spi-flash-config.md` (all), `docs/hw/00-memory-map.md` §2 C6-C10, C23, C35, §3 C7, C8, Q-B5, Q-D2; firmware `software/io/flash/*`, `software/infra/config.cc`, `software/userinterface/userinterface.cc` (CFG_USERIF_ITYPE)

## Scope

- **Window:** `0x10060200-0x100602FF`: DATA +0, RATE +4, CTRL +8, CRC +C, with the documented aliasing and
  CS framing rules (CTRL = 0 per-byte frame, 1 joins the frame, 3 drops bytes; `DATA_32` byte order).
- **Chip:** S25FL128L, JEDEC `01 60 18`, 16 MiB.
  - Command set actually used by the firmware: RDID, RDSR1/2/CR, WREN/WRDI, READ/FAST READ (3- and 4-byte,
    incl. 0x13), PP (3/4-byte), SE/BE/CE variants used, RSFDP if probed, UID 0x4B, reset 0x66/0x99.
  - WEL gating; program ANDs bits; erase sets 0xFF.
  - SR1 BUSY always 0 (operations are instant).
  - The tester order W25Q → S25FL → S25FL-L must end in the S25FL-L driver claiming the chip (doc 06 H1,
    00 §3 C8).
- **Persistence:**
  - `cfg.flash_image = Some(path)`: load at start (create an erased 16 MiB file if missing); write back on
    each completed program/erase, debounced ≤ 1 s wall.
  - `None`: volatile, starts erased.
- **Overlay-UI seeding** (`cfg.overlay_ui`):
  - When the config area holds no valid page for the user-interface config store, write one that sets
    `CFG_USERIF_ITYPE = 1`.
  - Page layout, store ids and checksum rules are derived from `config.cc`, the flash driver's
    config-page code and doc 06 (config pages at 0xFE8000 for S25FL128L).
  - Document the derived format as a comment block with `file:line` citations.
  - Leave every other store absent so the firmware writes defaults.
- `install(map, cfg)`.

## Tests

- JEDEC via the firmware's CS framing; RDSR BUSY = 0; program/erase/read round trip at 3- and 4-byte
  addresses; WEL required.
- `DATA_32` byte order; UID stable.
- Persistence: write, drop, reload from a temp file.
- Seeding: the page parses back with the documented format and sets ITYPE = 1.

## Acceptance

`cargo test -p ue2-core flash` passes.
