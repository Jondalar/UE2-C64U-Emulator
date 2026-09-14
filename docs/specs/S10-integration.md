# S10 — Integration: boot to M1, M2, M3

**Owns:** anything needed to reach the milestones. Prefer fixing inside the owning module; update the
matching `docs/hw` doc when real firmware behaviour contradicts it.
**Reads:** `docs/ARCHITECTURE.md`, `docs/hw/00-memory-map.md` §2 (boot order), all S01-S09 specs

**Status: M1, M2 and M3 are done.** Reproducible commands, boot log, task list, stats and known gaps:
`docs/status/boot.md`.

## Procedure

1. Build: `scripts/build-firmware.sh` (if the ELF is missing), `cargo build --release`.
2. Run `target/release/ue2emu run --headless --speed max --max-seconds 20 --log unmapped`. Iterate:
   - **Fault hook fired:** symbolize, find the hazard in 00 §2, fix the model.
   - **Hang** (console silent, PC stuck in a small loop): add a temporary `--log io` run, identify the polled
     register, and match it to the hazard list.
   - **Unmapped access on the boot path:** model it, or document it as harmless in 00 §1b.
3. Record the boot log excerpt and the unmapped summary in `docs/status/boot.md`.

## Milestones

- **M1:** the boot log shows `*** FPGA Capabilities: 34000222 ***` (or the actual banner lines), with no halt.
- **M2:** 60 s emulated without a halt. The log shows the network, WiFi and SD init lines that the T0
  models produce. The idle loop is reached (sample PCs in `prvIdleTask`). The unmapped summary contains no
  boot-relevant surprises.
- **M3:** `scripts/smoke-menu.ctl` does:

  ```
  wait 4000
  screen
  button
  wait 800
  screen
  png run/menu1.png
  key down
  wait 300
  key down
  wait 300
  screen
  png run/menu2.png
  quit
  ```

  The `screen` after `button` shows the overlay menu text. The selection is colour-only in the default scheme
  (docs/hw/05 T1), so the last `screen` text is unchanged; `run/menu1.png` and `run/menu2.png` show the
  selection moving. Run it with `--flash run/flash.bin`, so S06 seeding enables the overlay UI.

## Acceptance

All three milestones reproduce from a clean `run/` directory with one command each, documented in
`docs/status/boot.md`.
