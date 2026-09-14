# W4-SID status — SID socket detection and audio on TRX64's reSID

Spec changes: `docs/specs/S14-c64-trx64.md` §W4-SID. Code: `crates/c64-bridge/src/sid.rs`, `crates/ue2emu/src/audio.rs`.

**Reached.** With `--sid-socket1 armsid` the unmodified firmware detects an ARMSID in socket 1, enables the socket
and maps it. UltiSID 1 is on the same reSID, so the firmware's default map ($D400) is audible without a socket. DMA
reads of the SID range answer from the emulated chip. The SID sample stream plays through cpal (`--audio`) and
goes to a WAV file (`--audio-wav`). A BASIC voice typed through the control language gives a 1000.0 Hz WAV; a run
without the POKEs is silent.

Firmware paths are relative to `firmware/1541ultimate/software/`; TRX64 paths to
`<TRX64 checkout>/crates/trx64-core/` (commit `a448229`).

## Options (`ue2emu run`, with `--c64 trx64`)

| Option | Default | Effect |
|---|---|---|
| `--audio on\|off` | on with a window, off `--headless` | the default output device at its own rate if 44.1 or 48 kHz, else 48 or 44.1 kHz; mono to every channel |
| `--audio-wav PATH` | none | mono 16-bit PCM WAV of exactly the samples reSID produced: the device rate with `--audio on`, else 44100 Hz; sizes written when the emulator stops |
| `--sid-socket1 none\|armsid` | none | what SID socket 1 holds |

`--audio-wav` with `--c64 none` warns and writes nothing. No audio device (or a failed stream) is a warning; the
emulator runs on.

## Detection (boot console, `--sid-socket1 armsid`)

```
FPGASID Detection: AB AB
FPGASID Detection: 00 00
ARMSID Detect: 4E 4F
ARMSID Detect: 00 00
$$ SID1 = 5. SID2 = 0
Resulting address map: Slot1: 40/C0 (Enabled) Slot2: 40/C0 (Disabled) SlotSplit: 00.  Emu1: 40/C0  Emu2: 40/C0  Emu Split: 00
```

`AB AB` is reSID's bus value after the FPGASID DIAG writes (sid.cc:205-209). Without the option the console keeps
`$$ SID1 = 0. SID2 = 0` and `Slot1: 40/C0 (Disabled)`.

## Why an ARMSID

- **Detection needs no timing.** `detectRemakes` writes "SID" to $D41D-$D41F and reads "NO" from $D41B/$D41C
  (u64_config.cc:595-637). A real 6581/8580 is only found by `S_SidDetector`: exact OSC3 values after DMA pokes a
  cycle apart (DetectSidImpl u64_config.cc:2227-2274, analysis 2336-2384). Here DMA accesses land on instruction
  boundaries and several fall into one C64 cycle (S14 §4), so that probe would depend on RV32 timing and reSID's
  combined-waveform tables.
- **The socket is switched on.** A detected 6581 leaves the socket disabled (12 V caution), an 8580 enables it,
  anything else enables it (u64_config.cc:707-726). An ARMSID is audible without user action.
- **The chip model is a device setting the firmware reads and writes.** readParams asks the ARMSID for its mode
  ('F','I', sid_device_armsid.cc:105-175) into the "Fundamental Mode" item; changes and the SID player's
  `SetSidType` send it back ('S','E','6'/'8', 241-250, 297-331; u64_config.cc:1939-1977). That maps 1:1 onto reSID's
  6581/8580.
- **Less protocol than the other identities.** FPGASID needs its DIAG identity plus a large configuration register
  map (sid_device_fpgasid.cc). SwinSID Ultimate is detected as easily ("SW"), but its settings live in a firmware
  flash store pushed at the device (sid_device_swinsid.cc:29-33, 55-100), not on the chip. PDsid and SIDKick answer
  their own string probes (sid_device_pdsid.cc:107-114, sid_device_sidkick.cc:154-184).

## ARMSID protocol implemented (`sid.rs` `ArmSid`)

Configuration mode: "SID" in $1D/$1E/$1F; any other $1D leaves it. In it, a write to $1E or $1F runs the pair
($1F, $1E); $1B/$1C read the last answer. Outside it every register goes to reSID.

| Pair | Firmware use | Answer / effect |
|---|---|---|
| D,I | detectRemakes (u64_config.cc:608-621) | "NO" |
| I,I | ARM2SID check (622-636) | "NO" (not 'L'/'R': ARMSID) |
| V,I | version (readParams) | 3, 0 ("3.0") |
| F,I | mode | '6' or '8' |
| U,I | supply voltage | 9000 mV, big endian |
| H,I | filter settings | 6581 strength/lowest and 8580 highest/lowest nibbles |
| 6,E / 8,E | S_set_mode, SetSidType | reSID model 6581 / 8580 |
| $8n-$Bn,E | S_set_filt (252-278) | nibble stored, read back by H,I; no effect on reSID |
| $C0,E / $CF,E | save RAM / flash (280-295) | nothing |

The mode starts as 6581 at every emulator start; the emulated ARMSID has no flash.

## SID decode implemented (`sid.rs` `Decode`)

- The bridge receives every C64 core config write (`C64Backend::core_config_write`, S14 §W4-SID).
- A decoder answers I/O address `a` in $D400-$D7FF or $DE00-$DFFF when `((a >> 4) & MASK) == BASE`
  (SIDx/EMUSIDx_BASE and _MASK, system/u64.h:110-117; docs/hw/10 §SID addressing). BASE bit 0 set is "Unmapped"
  (u64_sid_offsets[0]).
- Socket 1 also needs C64_SID1_EN and a fitted chip. It is mono, so C64_STEREO_ADDRSEL (the "B" half of a dual
  device) is ignored and both halves reach it.
- UltiSID 1 also needs the C64_EMUSID_SPLIT bits of `a >> 4` clear (split_bits, u64_config.cc:266, 320); set bits
  select UltiSID 2.
- Until the firmware writes the latches the bridge uses the firmware's default map: sockets off, UltiSID 1 and 2 at
  40/C0, $D400-$D7FF (boot log above; auto-mirroring, u64_config.cc:2406-2465).
- **Writes** that socket 1 or UltiSID 1 decodes go to reSID; socket 1 sees $1D-$1F first. DMA writes also go to
  TRX64's own bus (its SID shadow, or the cartridge at $DE00-$DFFF).
- **DMA reads:** socket 1 decodes → ARMSID answer in configuration mode, else reSID. UltiSID 1 decodes → reSID.
  Neither → 0 in $D400-$D7FF (socket 2 and UltiSID 2 are not modelled, so their probes find nothing), TRX64's bus at
  $DE00-$DFFF.
- **Model:** socket 1 fitted, enabled and mapped → the ARMSID mode; else C64_EMUSID1_WAVES (0 = 6581, 1 = 8580,
  u64_config.cc:1641-1665). A change rebuilds the engine and replays registers $00-$18.
- reSID runs with its filter on, the external filter on and the resampling method (TRX64 `ResidConfig` defaults
  otherwise).

## What TRX64's single SID cannot express (API gaps)

1. **One reSID per process.** The shim drives one global `SID g_sid` (vendor/resid/resid_shim.cc:34), and
   `Resid::new` holds a process-wide guard for the engine's lifetime (src/resid_ffi.rs:38, 157-158). Socket 2,
   UltiSID 2, ARM2SID and stereo tunes cannot sound. A write either decoder of SID 1 takes (UltiSID 1 parked at $D600
   during detection, socket 1 at $D400) reaches the same engine. Parallel tests that build engines take turns.
2. **CPU reads of $D400-$D7FF come from TRX64's fastsid** (src/full.rs:372-380 → src/sid.rs:250), not reSID. C64
   programs see fastsid OSC3/ENV3/POT and no ARMSID identity; only the firmware's DMA reads see reSID and the ARMSID.
   fastsid also keeps running beside reSID (src/lib.rs:2161).
3. **The SID write hook carries neither address nor cycle.** `Sid6581::write_trace` is `FnMut(reg & 0x1F, value)`
   (src/sid.rs:175, 232). The bridge pairs it with its `Observer::on_bus` write record, which has both
   (src/full_sc.rs:252-256). The hook tells a real SID write from a write to RAM under I/O with `$01` banked out.
4. **CPU writes to $DE00-$DFFF go to the cartridge** (src/full.rs:617) and never reach the SID hook: a SID mapped
   there sounds only through DMA writes.
5. **`ResidConfig` is fixed at construction** (src/resid_ffi.rs:157-190, no model setter). A model change rebuilds
   the engine; oscillator and envelope phase restart.
6. **`Resid::clock_silent` must not be used.** It is reSID's batch `SID::clock(delta)` (vendor/resid/sid.cc:745),
   which never updates the ENV3 latch (vendor/resid/envelope.h:118). Mixed with sampled clocking on one engine, it
   froze the envelope: ENV3 70 after 100-20 000 cycles of attack 0, 255 with sampled clocking only. The bridge always
   clocks through `emit` and drops the samples when nothing listens.

## Timing and audio path

- **With a sink:** CPU writes are applied at their cycle (reSID is clocked to it first), and reSID follows every
  `advance_to` (1 ms emulated, S14 §4). The mono samples go to the sink in emulated-time order.
- **Without a sink:** CPU writes only set registers; a DMA write or read clocks the gap, at most 1 s of cycles.
  - C64 programs read TRX64's own SID anyway (gap 2), and the firmware's DMA reads come with the 6510 stopped, so
    nothing observes the difference.
  - A headless run without `--audio-wav` pays no reSID clocking, even while a program plays. The placeholder KERNAL's
    welcome jingle (u64/default_kernal.tas:98-135) cost 5 % host MIPS while every write clocked reSID.
- While the 6510 is stopped or held in reset the chips run on; the C64 reset line clears reSID.
- **WAV:** every sample, unchanged. At `--speed max` the file still holds emulated time (17.72 s run → 781 590
  samples at 44.1 kHz).
- **Device ring (`audio.rs` `Ring`):**
  - It holds 150 ms. Past that the oldest samples are dropped down to 60 ms, so the emulator never waits.
  - Playback starts once 60 ms are buffered. An underrun plays silence without waiting for a new pre-roll.
  - `--speed max` therefore plays chopped audio and is not slowed.
- The cpal stream lives in `EmuHandle` on the thread that called `runner::spawn`.

## Socket 1 is empty by default: the first-boot popup

A detected SID type that differs from the saved "SID Sockets Configuration" makes the firmware post "SID changed.
Please review settings" (u64_config.cc:707-750). The file browser shows it as a popup at the first menu open
(tree_browser.cc:212-216), until the settings are saved to flash. The emulator cannot pre-seed that store: U64
stores share page id 0x55363443 and are packed into one config page (components/config.cc:113-190). With the ARMSID
fitted by default:

- every menu-driven script on a fresh flash (smoke-all.sh, C64 A2-A5, e2e) meets the popup;
- a flash saved with the ARMSID shows it again under `--c64 none`.

So `--sid-socket1` defaults to `none`, and UltiSID 1 keeps the default configuration audible. With a persistent
`--flash`, press OK once, review and save; later boots are quiet. **Open decision:** make `armsid` the default. That
needs the smoke scripts to dismiss the popup, or a flash seed for the packed store.

## Acceptance

Setup as docs/status/c64.md (ROM image, `smoke-c64-roms.ctl` on a fresh `run/flash.bin`), then copies of that flash.
Every run: `target/release/ue2emu run --headless --speed max $FW …`.

| Run | Options and script | Result |
|---|---|---|
| tone, ARMSID | `--flash run/flash-tone.bin --sid-socket1 armsid --audio-wav run/sid-tone.wav --script scripts/smoke-sid-tone.ctl` | detection lines above; `scripts/wav-tone.py run/sid-tone.wav --expect 1000`: 44100 Hz, 781 590 samples, peak-to-peak 9399 (14.34 %), dominant **1000.0 Hz**, PASS |
| silence, ARMSID | `--flash run/flash-silent.bin --sid-socket1 armsid --audio-wav run/sid-silent.wav --script scripts/smoke-c64-type.ctl` | ` 42` printed; `wav-tone.py --silent`: peak-to-peak **0**, PASS |
| tone, UltiSID 1 (default map) | `--flash run/flash-ulti.bin --audio-wav run/sid-ulti.wav --script scripts/smoke-sid-tone.ctl` | `$$ SID1 = 0`; dominant **1000.0 Hz**, peak-to-peak 9400, PASS |
| device | `--audio on --script` (`wait 8000`) | stream opened on the Mac's default device, no warning; 8.0 s emulated at `--speed max`, 119 MIPS last interval |

`smoke-sid-tone.ctl` types `poke 54296,15:poke 54277,0:poke 54278,240` and `poke 54273,66:poke 54272,133:poke
54276,33`: volume 15, attack 0, sustain 15, F = 17029 (1000.1 Hz at PAL), sawtooth + gate. `wav-tone.py` takes the
last 32768 samples (mean removed, Hann window, FFT, parabolic peak); pure Python.

**No regressions:** `cargo test --workspace` green (c64-bridge 21, ue2-core 187, ue2emu 52, …); `scripts/smoke-all.sh`
all pass; C64 A2-A5 pass with default options (A2 READY and menu dump, A3 ` 42`, A4 `DMA load complete: $0801-$081C`
and `Cart got disabled`, A5 `Frozen on Bad line`); `cargo build -p ue2emu --no-default-features` without warnings.
The window was not started (no GUI in this workflow).

## Tests added

- `sid.rs`: decode (base/mask/enable/split/unmapped), the ARMSID probe and readParams/set-mode/set-filter sequences,
  socket 1 reads (ARMSID then reSID ENV3, empty socket, model from mode and from UltiSID waves), the observer tap
  (only hook-confirmed writes, DummyWrite included), CPU writes clocked only with a sink, audio (48 kHz count for
  985 000 cycles, 1000 Hz).
- `lib.rs`: DMA reads of the default map (bus value, OSC3, ENV3; unmapped → 0); a 6510 program at $C000 writing the
  tone plays 600 ms → 26 460 samples, 500 periods in 0.5 s.
- `c64.rs`: core config writes reach the backend, unsynced, still latched.
- `audio.rs`: WAV bytes and header sizes, ring pre-roll/drop/underrun, option defaults.

## Performance

A6 method (docs/status/c64.md): `wait 60000`, `--log unmapped`, fresh flash, `--speed max`, one run at a time,
variants interleaved. Host MIPS = instructions / wall seconds of the whole process. The fresh flash runs the
placeholder KERNAL, whose welcome jingle writes the SID. Baseline is main `9b2b7a8` built from the same sources.

| Build / option | Host MIPS per run | Last interval | vs baseline |
|---|---|---|---|
| baseline `9b2b7a8` | 128.1, 127.3, 130.9, 130.3, 129.5, 129.6 (median 129.6) | 129-132 | — |
| W4-SID, default (no sink, socket empty) | 125.0, 125.3, 126.8; one outlier 120.1 | 131-132 | about −3 % (60 s in 11.8-12.0 s wall) |
| W4-SID, `--audio-wav` + `--sid-socket1 armsid` | 114.0, 113.3 | 117-118 | about −12.5 % (13.2 s wall) |

- **First version:** every CPU write clocked reSID, sink or not. The default path measured 122.3/123.1: the profile
  (`sample`) showed reSID clocked by the jingle.
- **Zero-sized tap:** turning `SidTap` into a zero-sized observer changed nothing (122.6/123.9).
- **Untimed CPU writes without a sink:** the last-interval MIPS equal the baseline, and the profile has no reSID
  frames.
- **The remaining ~3 %:** it has no reSID frames and is within this host's run-to-run spread. Other workflows were
  building and running in parallel; two later rounds at 52-66 MIPS were discarded.
- **With a sink:** reSID's per-cycle clock, filter and 44.1 kHz resampling cost about 1.4 s wall per 60 s emulated
  at `--speed max`, about 7 % of one core in realtime.

## Known gaps

- Socket 2, UltiSID 2, ARM2SID and stereo: one reSID (gap 1). Their probes find nothing, and their writes are
  dropped.
- Mixer gains (U64_AUDIO_MIXER, pan, the freezer mute) and UltiSID filter curves, resonance and digi level are
  ignored. C64_VOICE_ADSR still reads 0, so the LED strip sees no envelopes.
- ARMSID filter settings are stored and read back but do not change reSID. The mode is not kept across emulator
  starts.
- C64 programs read SID registers from fastsid (gap 2). A SID at $DE00-$DFFF is silent for CPU writes (gap 4).
- PAL clock only (reSID at 985 248 Hz), like the rest of S14.
- The window's default `--audio on` was not heard by the agent; the device path was checked headless on silence.
