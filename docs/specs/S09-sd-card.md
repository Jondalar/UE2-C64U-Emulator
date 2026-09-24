# S09 — SD card over SPI (image-backed)

**Status:** built.

**Owns:** `crates/ue2-core/src/devices/sdcard.rs`
**Reads:** `docs/hw/07-sd-card-filesystems.md` (all), `docs/hw/00-memory-map.md` §1b row 0x10060000, §2 C3, runtime hazards "SD insert"; firmware `software/io/sd_card/*`, `software/filesystem/sdcard_manager.cc`

## Scope

- **Window:** `0x10060000-0x100600FF`: SPI registers with the documented 4-byte aliasing, card detect,
  write protect, busy flags.
- **No image** (`cfg.sd_image = None`): card detect reports "no card" (T0: 0x10060008 reads 0x00), never
  hangs the 100 ms poll.
- **With image:** an SDHC card in SPI mode.
  - CMD0, CMD8 (echo check pattern), CMD55/ACMD41 (ready at once or after the documented retries), CMD58
    (OCR, CCS = 1), CMD9/CMD10 (CSD v2 with the correct capacity, CID), CMD16, CMD17/18 (single/multi
    read with 0xFE token and dummy CRC), CMD24/25 (write, 0xFC/0xFD tokens, data-response 0x05), CMD12,
    CMD13, and whatever else the driver sends.
  - Idle byte 0xFF between responses; R1 timing within the driver's poll budget (240 k / 600 k polls with
    IRQs off — respond at once).
  - Block addressing for SDHC; the image is the raw card, whose length defines the capacity (round down to
    512).
  - Writes go straight to the file.
- `install(map, cfg)`.

## Tests

- Driver-shaped init sequence → ready; CSD capacity for a 64 MiB temp image.
- Read block with a pattern; write block then read back from the file.
- No-card poll value; multi-block read/write with stop.

## Acceptance

`cargo test -p ue2-core sdcard` passes.
