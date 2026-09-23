# S26 — Frame pacing: the window shows every picture the VIC makes

**Status:** built (2026-09-23).

**Owns:**
- `crates/ue2emu/src/runner.rs`: when a display snapshot is published
- `crates/ue2emu/src/window.rs`: when the window redraws
- `crates/c64-bridge/src/lib.rs`: the `frame_counter` doc ("once per PAL frame" → once per frame)

**Reads:** S25 (NTSC), S24 §4 (the VIC frame counter), S07 (the overlay rides in the same snapshot).

## 1. Today

Two fixed 50 Hz clocks sit between the VIC and the screen:

| Where | What | Clock |
|---|---|---|
| `runner.rs` `DISPLAY_PERIOD_MS = 20` | publishes `machine.display()` (C64 frame + overlay) | every 20 ms emulated |
| `window.rs` `FRAME = 20 ms` | redraws from the latest snapshot | every 20 ms wall |

The VIC makes 50.12 pictures a second under PAL and 59.83 under NTSC (S25). Under NTSC one picture in six is never
published; under PAL the two clocks beat against the VIC's. Scrolling judders in both, worse under NTSC. The overlay is
part of the same snapshot, so it follows the same path and has no clock of its own.

## 2. Publishing

- With a C64: publish when the VIC's frame counter (`C64Backend::frame_counter`, TRX64 `vic.frame`) has advanced since
  the last publish, checked after every run slice. The slice is 4 ms emulated at most, so a picture is published at
  most 4 ms after the VIC finished it. The C64's row decides the rate; nothing here knows PAL or NTSC.
- Without a C64 (`--c64 none`): the 20 ms period stays; there is no VIC to follow.
- Every snapshot has its own `now_ms` (pictures are at least 16 ms apart, the fallback 20 ms), so a reader tells a new
  picture from the one it already has by that; `DisplaySnapshot` keeps its fields.

## 3. Drawing

- The window polls every 4 ms and redraws only when the snapshot's `now_ms` changed (or the window was resized or
  exposed).
- No vsync to the host display: softbuffer has none. A 50 Hz picture on a 60 Hz display still shows one picture
  twice every fifth refresh; that is the display, not the emulator.

## 4. Checks

- A unit test on the publish rule: a counter step publishes, no step does not, `--c64 none` keeps 20 ms.
- The frame counter itself is TRX64's `vic.frame`: 50.12 steps per second under PAL, 59.83 under NTSC.
- The user looks at a scrolling program in the window under PAL and NTSC.

## 5. Not in this spec

- The UDP video stream's NTSC framing (S24).
- Frame blending or vsync to the host display.
