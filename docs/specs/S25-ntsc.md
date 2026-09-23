# S25 — NTSC: the C64 runs the standard System Mode asks for

**Owns:**
- `crates/c64-bridge/src/lib.rs`: C64_VIDEOFORMAT decoded to a TRX64 model row, the switch at a frame boundary
- `crates/c64-bridge/src/clock.rs`: the emulator clock → C64 cycle ratio from the model's `cpu_hz`
- `crates/c64-bridge/src/sid.rs`: reSID's clock from the model
- `crates/ue2-core/src/devices/c64.rs`: the "PAL-only" notice goes
- `docs/status/c64.md`, `docs/status/gaps.md`: the PAL-only gap closed

**Reads:** `firmware/1541ultimate/software/system/u64.h:105,163-170` (the bits),
`firmware/1541ultimate/software/u64/color_timings.cc` (the six modes), `u64_config.cc:1100-1150` (the writer),
TRX64 v0.8.7 `Machine::switch_model` / `put_on_model` (lib.rs:905-940), `models.toml` (the rows), Spec 863 D3/D5.

## 1. What the firmware writes

`U64Config::effectuate_settings` writes `C64_VIDEOFORMAT = mode_bits | RGB` at boot and on every settings change.

| Bit | Meaning |
|---|---|
| 0 | NTSC colour encoding |
| 1 | 60 Hz |
| 2 | RGB output |
| 3 | NTSC clock reference |
| 4-5 | cycles per line: 0x00 = 63, 0x10 = 64, 0x20 = 65 |

| System Mode | Bits | TRX64 row |
|---|---|---|
| PAL | 63, 50 Hz, PAL clock | `c64-pal` |
| NTSC (the default) | 65, 60 Hz, NTSC clock, NTSC encoding: 0x2b | `c64-ntsc` |
| PAL-60, PAL-60/L | 65, 60 Hz, NTSC clock | `c64-ntsc` |
| NTSC-50, NTSC-50/L | 63, 50 Hz, PAL clock, NTSC encoding | `c64-pal` |

A row is a timing. The encoding only changes the analogue carrier, which neither TRX64 nor UE2 produces, and the
palette is the firmware's in every mode (TRX64, 2026-09-23). The row is chosen from the cycles field: 63 → `c64-pal`,
65 → `c64-ntsc`. 64 cycles (6567R56A) is in no table the firmware writes; it keeps the row and prints a notice once.

## 2. The switch

`switch_model` refuses a VIC position the new row does not have, so it is made at a frame boundary: line 0, early.

- A write that asks for another row sets `pending_model`. Nothing else changes at the write.
- `advance_to` caps its target at the end of the current frame while a model is pending:
  `(lines_per_frame - raster_line) * cycles_per_line - raster_cycle` cycles ahead, plus 2. TRX64 reports line 0 from
  the cycle after the boundary; aimed at the boundary itself the VIC still stood in the last line, and the next slice
  was past cycle 50. That holds for a running, a stopped and a reset-held C64 alike: the VIC sweeps in all three
  (`run_held`, Spec 850 D7).
- After that run, if the VIC stands in line 0, `switch_model` is called. An instruction can overshoot, so the check
  is on the position, not on the cycle count; a refusal leaves the model pending for the next frame.
- The rest of the slice then runs on the new row.

The KERNAL decides PAL/NTSC at `$FF5E`, about 1.5 s after UNRESET and later under the 2.06 s turbo hold; a running
program keeps what it detected. A frame boundary is at most 20 ms away, so a write made before or at UNRESET is in
force when the KERNAL looks.

## 3. What follows the row

TRX64's `put_on_model` moves the VIC's cycle table and frame, both CIAs' timing (TOD 50/60 Hz) and the drive's sync
factor. Turbo, UCI and the REU count cycles and need nothing. The reset hold keeps its cycle count, about 2.01 s at
the NTSC clock.

The bridge moves the rest:
- **Clock.** `Clock` takes the rate from `timing().cpu_hz` (985 248 or 1 022 730) and is re-anchored at the switch,
  so the cycles owed before it are counted at the old rate.
- **reSID.** Every engine gets `set_clock_freq(cpu_hz)`, which re-samples without restarting the oscillators. The
  cadence for the samples owed while no engine runs follows the same rate. The SID worker (S20) gets it in order with
  the writes.

## 4. The picture

The NTSC canvas is 384x247, raster lines 28-274, wrapping at 263 (`display_window`, `wraps()`). The renderer centres
the firmware's crop on whatever canvas it gets (S07), so a 240-line crop shows 3 and 4 lines of it trimmed, and a
270-line crop gets backdrop above and below. Nothing changes there.

## 5. Checks

- `$02A6` after a reset: 00 under NTSC and PAL-60, 01 under PAL and NTSC-50, at 1 to 48 MHz turbo.
- Switching while running: the switch lands in line 0 (cycle 6 in the unit test), and one second then counts
  1 022 730 cycles.
- The SID tone smoke under NTSC plays its register at the NTSC clock: 1038.1 Hz where PAL gives 1000.1 Hz.
- Workspace tests, the C64 smokes.

## 6. What changes for users

The firmware's default System Mode is NTSC. A flash that never changed it now boots an NTSC C64, as the device
does; before, every C64 was PAL. Scripts and tests that assume PAL timing set System Mode PAL.

## 7. Not in this spec

- 50/60 Hz in the overlay and the window's redraw (S07, S08).
- The UDP video stream's NTSC framing (S24).
