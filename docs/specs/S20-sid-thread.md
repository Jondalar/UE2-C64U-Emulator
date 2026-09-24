# S20 — The reSID engines on their own thread

**Status:** built (2026-09-17).

heartbeat-demo plays on 8 SIDs while its C64 runs at 64 MHz. In realtime with audio it reaches 95-100 % of realtime at
92-100 % of one core, so the audio device runs dry and the music stutters (measured 2026-09-17: 0.951 / 0.999 with
the idle skip, 0.956 / 0.958 without). Everything runs on the emulation thread: the Ultimate CPU, TRX64, and one
reSID per SID receiver that has been written (S17), clocked cycle by cycle and mixed into the sink.

S20 moves the engines, the mixing and the sink to a worker thread when a live audio device listens. The emulation
thread keeps the SID decode and the write trace and sends the worker what the engines need, in order.

## 1. What stays, what moves

| On the emulation thread | On the SID worker |
|---|---|
| `Decode`, groups, the SID map TRX64 routes by | the `Resid` engines, their build order, model rebuilds |
| TRX64's write trace and host door (`Door`), the ARMSID | clocking to each write's cycle, applying it to every receiver |
| which receivers have an engine and with which model (a mirror) | the mixer gains applied to the samples |
| the mixer bytes | `SamplerMix` and the sink (ring or WAV) |
| the sampler voices (`Sampler::advance_to`, on emulated time as before) | draining the sampler's output queue |

The split point is `Sid::catch_up` and the write loop in `Sid::drain`: they become `Engines::clock_to` and
`Engines::write`, which the inline path calls directly and the worker calls from its command loop.

## 2. Commands

One message per `Sid::advance` or DMA write, a `Vec<Cmd>` in order:

- `Ensure { rx, model }`: build or rebuild receiver `rx`'s engine (first write, `WAVES` or ARMSID mode change).
- `Write { at, mask, reg, val }`: clock all engines to cycle `at`, then write the register to every receiver in
  `mask`. With `Clocking::Untimed`/`At` the emulation thread sets `at` as the inline path would.
- `ClockTo(clk)`: the end of an advance.
- `Gains([i32; RECEIVERS])`: after a mixer write.
- `Reset`, `Reanchor(clk)`, `Drop(rx)` (socket 1 emptied).
- `Read { mask, reg, clk, reply }`: see §3.

The channel is a bounded `sync_channel(256)`. If the worker falls a quarter of a second behind, the emulation thread
waits, which is the backpressure a starved host needs. Dropping `Sid` closes the channel and joins the worker.

## 3. Reads

- **6502 reads of chips 1 and up** (OSC3/ENV3 through the host door): answered, as today, from a readback snapshot
  refreshed after clocking. The engines cache it there and nowhere else, because a reSID read of `$1B/$1C` sets the
  bus value the write-only registers read back. The worker publishes the cache into `Arc<[AtomicU32; RECEIVERS]>`
  after every batch; `refresh_readback` builds the door's table from it. The value is therefore up to one worker lag
  old instead of up to one advance (1 ms). Chip 0 is unaffected: TRX64's own model answers it (855 D3).
- **Firmware DMA reads** of a SID register (`Sid::read`, SID detection at boot): synchronous. The emulation thread
  sends `Read` behind the pending writes and waits for the reply.
- **Debugger peeks** (`Sid::peek`): `$1B/$1C` from the published readback, every other register 0, no clocking.

## 4. The sampler's queue

`SamplerMix` drains the sampler's output queue on the worker while `Sampler::advance_to` fills it on the emulation
thread. The queue moves out of `Sampler` into an `Arc<Mutex<VecDeque<i16>>>`: `advance_to` renders into a local
buffer and appends once, `drain` takes once. The inline path uses the same queue.

## 5. When the worker runs

- Only when the frontend attaches a live device (`audio::Sink` with a ring). `--audio-wav` alone, headless runs
  without audio, and every test keep the inline path, so their output stays reproducible to the sample.
- `--no-sid-thread` keeps the inline path with a device too, for comparison.
- `AudioSink` gains a `Send` bound for the threaded entry (`Trx64Backend::set_audio_threaded`); the test sinks built
  on `Rc` keep using `set_audio`.

## 6. Code

| File | Change |
|---|---|
| `crates/c64-bridge/src/sid.rs` | `Engines` (engines, order, clock, pace, gains, sink, readback cache) split out of `Sid`; `Host::{Inline, Worker}`; commands, worker loop, readback atomics |
| `crates/c64-bridge/src/sampler.rs` | shared output queue |
| `crates/c64-bridge/src/lib.rs` | `set_audio_threaded` |
| `crates/ue2emu/src/runner.rs`, `main.rs`, `audio.rs` | threaded when a device listens; `--no-sid-thread` |

Impact: `Sid::catch_up` is CRITICAL by GitNexus (S16 §3.4: 22 impacted, 7 processes). Its body moves unchanged into
`Engines::clock_to`; the tests below hold the inline output fixed and compare the worker against it.

## 7. Not built

- Engines spread over several workers: all engines of one chunk are mixed into one sample stream, so they would
  have to meet per chunk.
- Stereo, and the drive, tape and speaker mixer channels (S17 §3).

## 8. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean.
2. The existing SID and sampler tests unchanged (inline path).
3. A new test in `sid.rs`: the same scripted writes (a tone on UltiSID 1, a second on UltiSID 2, then the mixer
   muting it) inline and on the worker give the same number of samples, the same tone and the same level. Not
   sample for sample: two reSID instances built the same way do not render bit-identical streams, inline against
   inline either, so the test checks that too.
4. `smoke-sid-tone.ctl` 1000.0 Hz, `smoke-sid-stereo.ctl` 1000.0 Hz, `smoke-c64-carts.ctl` 27 PASS.
5. heartbeat-demo with audio: realtime in the windows of the 2026-09-17 measurement, and less wall time at
   `--speed max`; by ear without dropouts.
6. UltimateDemo2026 plays its MOD without new dropouts.

## 9. Measured (2026-09-17, Apple M4)

- heartbeat-demo at `--speed max`, 131 s emulated with audio on: **102.2 s wall with the worker, 116.6 s without**
  (12 % less). CPU time 118.6 s against 116.3 s, so the work did not grow; it left the emulation thread.
- In realtime the demo now holds realtime either way (0.999/0.999 with the worker, 1.000/0.999 without, at 98-99 %
  and 96 % of a core). The profile it came from: reSID 17.5 % of the emulation thread, the C64 side 99 %.
- Tests: workspace 436 pass; `smoke-sid-tone` and `smoke-sid-stereo` 1000.0 Hz; `smoke-c64-carts` 27 PASS.
