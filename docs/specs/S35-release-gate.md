# S35 — The release gate

**Status:** built (2026-09-24). The tests of §3 run in `smoke-c64-all.sh`; they found two bugs, both fixed: a frozen
freezer stayed frozen into the next cartridge, and the GeoRAM bank ignored the REU size.

**Owns:** `scripts/gate.sh`, `scripts/smoke-c64-all.sh`, the new smoke scripts of §3, `docs/status/tooling.md`.

## 1. What the gate is

One command, `scripts/gate.sh`, that has to pass before a minor or a major release (0.x.0, x.0.0). Patch releases
keep the lighter rule they have (workspace tests, CI, the smokes a change touches). It runs, stopping at the first
failure:

1. `cargo test --workspace --all-features`.
2. `scripts/smoke-all.sh`: the firmware without the C64 (menu, SD, flash, settings, monitor, UCI, VICE, a negative).
3. `scripts/smoke-c64-all.sh`: the C64 smokes, each on images and flash copies built from nothing: READY, typing, a
   PRG from SD, the freeze UI, the SID tone, 27 test cartridges with the Action Replay frozen, drive A with a SAVE
   written back into the D64, the 1581 at A and B, Software IEC, a USB stick with the USB keyboard.
4. `scripts/smoke-usb-dir.sh`: a host directory as a USB stick, written both ways.
5. `scripts/run-e2e.sh smoke` with `E2E_REST_SHIM=1`: the upstream suite's smoke profile, 12 of 12.
6. The tests of §3, inside `smoke-c64-all.sh`.

It needs the firmware build (`$UE2_FIRMWARE`), a macOS login session for the SD images, and the ports the smokes use
(6400, 8080, 18021-18080). It prints one PASS line per part and the wall time.

## 2. Not in the gate

The upstream quick profile (known failures, E1-E5 in `docs/status/e2e.md`), anything that needs a person (the window,
sound by ear), and anything that needs the device.

## 3. Never run end to end — new tests

- **REU preload and "Save REU":** an image on a `--usb-dir` stick, "REU Preload" on; after the reset a C64 program
  reads REU memory back by DMA and prints it; then "Save REU" writes the REU to a file that must equal what was
  loaded plus what the program changed.
- **KCS, SS5 and FC1 frozen from the firmware:** test cartridges with a freeze handler like `c28-ar-freeze.crt`'s,
  each mapped as its freezer maps itself in (`all_carts_v5.vhd`), frozen with the freeze button.
- **GeoRAM and TwoMegabyter started by the firmware:** GeoRAM through the REU setting "GeoRAM" with a C64 program that
  writes and reads back pages through `$DE00` and `$DFFE`/`$DFFF`; the TwoMegabyter as a test cartridge whose banks
  print their numbers.
