# S34 — The UDP streams under NTSC

**Status:** built (2026-09-24).

**Owns:** `crates/ue2-core/src/devices/streams.rs` (`send_frame`), `crates/c64-bridge/src/lib.rs` (`stream_rate`).

**Reads:** `tests/e2e/lib/streams.py:281-286` (the two frame heights), `:661-662` (the two audio rates), S24, S25.

## 1. Video

The receiver knows two frame heights: 272 lines under PAL, 240 under NTSC; the last datagram's line field carries
the height. UE2's PAL canvas is 272 lines and goes out whole (68 datagrams). The NTSC canvas is 247 lines (S25 §4);
the stream sends its middle 240 (canvas lines 3-242, 60 datagrams), dropping 3 border lines above and 4 below. Which
240 lines the FPGA takes is not in any source; the middle is the choice that keeps the borders even.

## 2. Audio

The stream's sample rate is derived from the video clock: 47 983 Hz under PAL, 47 940 Hz under NTSC. The tap takes
the rate of the model the C64 runs and is set again at a model switch. With an audio device the device's rate stands,
as before (S24 M3).

## 3. Checks

- Unit: a 247-line frame is 60 datagrams, the last one's line field is 236 with the last-packet flag, and the first
  line sent is canvas line 3.
- The PAL tests stand: 68 datagrams for 272 lines.
