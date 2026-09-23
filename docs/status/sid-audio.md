# W4-SID / S17 status — the U64's SIDs on TRX64's reSID

Specs: `docs/specs/S14-c64-trx64.md` §W4-SID, `docs/specs/S17-ultisid.md` (UltiSID, TRX64 Spec 855). Code:
`crates/c64-bridge/src/sid.rs`, `crates/ue2-core/src/devices/c64.rs` (mixer window), `crates/ue2emu/src/audio.rs`.

**Reached.** UltiSID 1 and 2 with their split instances A-D and the ARMSID in socket 1 each play on their own reSID,
routed by the firmware's decode and mixed with its mixer gains. With `--sid-socket1 armsid` the unmodified firmware
detects an ARMSID in socket 1, enables the socket and maps it. The SID player maps a two-SID PSID onto UltiSID 1 and 2,
and the second SID alone plays a 1000.0 Hz tone. C64 programs read OSC3/ENV3 of every SID and the ARMSID's answers.
The sample stream plays through cpal (`--audio`) and goes to a WAV file (`--audio-wav`). A BASIC voice typed through
the control language gives a 1000.0 Hz WAV; a run without the POKEs is silent.

Firmware paths are relative to `firmware/1541ultimate/software/`; TRX64 paths to
`<TRX64 checkout>/crates/trx64-core/` (commit `1ce84b0`).

## Options (`ue2emu run`, with `--c64 trx64`)

| Option | Default | Effect |
|---|---|---|
| `--audio on\|off` | on with a window, off `--headless` | the default output device at its own rate if 44.1 or 48 kHz, else 48 or 44.1 kHz; left and right on channels 0 and 1, their mean on a mono device and past channel 1 (S29) |
| `--audio-wav PATH` | none | stereo 16-bit PCM WAV of exactly the frames the SIDs and the sampler produced (S29): the device rate with `--audio on`, else 44100 Hz; sizes written when the emulator stops |
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

## SID player (`scripts/smoke-sid-stereo.ctl`)

```
Trying to map SID 0 (type 6581) on logical SID 2 (type Either), at address $D400
Trying to map SID 1 (type 6581) on logical SID 3 (type Either), at address $D420
Resulting address map: Slot1: 01/FE (Disabled) Slot2: 01/FE (Disabled)  Emu1: 40/FE  Emu2: 42/FE
Sid 0 was mapped to slot 2, which uses mixer channel 2 with volume setting 80. Setting pan to Left 2.
Sid 1 was mapped to slot 3, which uses mixer channel 3 with volume setting 80. Setting pan to Right 2.
```

The player screen shows `SID #1: $D400 : 6581 / PAL` and `SID #2: $D420 : UNKNOWN / PAL`.

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
($1F, $1E); $1B/$1C read the last answer. Outside it every register goes to reSID. TRX64's write trace drives the
protocol as the write happens, so a C64 program's probe reads the answer through the host door in the same run, as
the firmware's DMA probe does.

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

- The bridge receives every C64 core config write (`C64Backend::core_config_write`).
- A decoder hits an address in $D400-$D7FF or $DE00-$DFFF when `((a >> 4) & MASK) == BASE` (SIDx/EMUSIDx_BASE and
  _MASK, system/u64.h:110-117; sid_editor.cc:162-165). BASE `0x01` is "Unmapped" (u64_config.cc:202-203). A4 is not
  decoded: every MASK the firmware writes leaves it open, so a SID occupies whole 32-byte blocks.
- Socket 1 also needs C64_SID1_EN and a fitted chip. Socket 2 is never fitted. C64_STEREO_ADDRSEL selects the second
  half of a dual chip; the emulated ARMSID is one chip, so both halves reach it.
- C64_EMUSID_SPLIT picks instance A-D of both UltiSIDs from address bits: one bit set → B; 1/4 (A5,A6), (A5,A8),
  (A7,A8) → A-D (sid_editor.cc:166-181). The instances are separate register sets, not mirrors (S17 §1.2).
- Until the firmware writes the latches the bridge uses its default map: sockets off, all four decoders at 40/C0, so
  UltiSID 1 and 2 both take $D400-$D7FF.

**Routing (S17 §2.1).** Each of the 48 blocks has a set of receivers: socket 1, UltiSID 1 A-D, UltiSID 2 A-D. TRX64
routes an address to one chip, the U64 a write to every decoder that hits, so a TRX64 chip is a group: one distinct
receiver set. The bridge hands TRX64 a `SidMapping` per block (`Machine::set_sid_map`) whenever a decode latch changes
the table:

- all 32 $D400-$D7FF blocks, a block nobody decodes on an empty group, so TRX64's fallback to chip 0 never fires;
- $DE00-$DFFF blocks only when something receives them, with `ahead_of_expansion` set (unverified, 855 §7);
- chip 0, the chip TRX64 ticks, is the first group holding UltiSID 1-A, else the first block's.

**Writes.** TRX64's write trace queues `(cycle, chip, register, value)` for CPU writes, REU transfers and host pokes;
the bridge applies each to every receiver of its chip's group. A DMA write goes through `Machine::write_full`, whose
trace stamps a stale clock, so its records are applied at once at the C64's current cycle.

**Reads.**
- 6502, chip 0: TRX64's own SID model (855 D3).
- 6502, chips 1 and up: TRX64 does not tick them. The bridge answers $1B/$1C through TRX64's host door (855 D5) from
  the group's first reSID, cached after every catch-up.
- ARMSID in configuration mode: $1B/$1C of socket 1's group through the same door, read and peek alike.
- Firmware DMA reads: the bridge's own decode. Socket 1 decodes → ARMSID answer in configuration mode; otherwise the
  first receiver with an engine (socket 1, UltiSID 1 A-D, UltiSID 2 A-D). Nobody decodes → 0 in $D400-$D7FF, TRX64's
  bus at $DE00-$DFFF.

**Engines (S17 §2.2).** One reSID per receiver that has received a write. Its model is C64_EMUSIDn_WAVES of its
UltiSID (0 = 6581, 1 = 8580, u64_config.cc:1651-1656) or the ARMSID's mode for socket 1. A model change rebuilds that
engine and replays registers $00-$18. reSID runs with its filter on, the external filter on and the resampling method
(TRX64 `ResidConfig` defaults otherwise).

## Mixer (S17 §2.5)

- `C64Port` serves 0x10100500-0x101005FF. Bytes 0x00-0x13 reach the backend (`C64Backend::mixer_write`); the speaker
  mixer (+0x40) and the resampler (+0x80) stay write sinks; the page reads 0.
- Byte 2c is a channel's left gain, byte 2c+1 its right, each / 90: the firmware's 0 dB centre (`5A/5A`) is unity on
  both sides, hard left at 0 dB is 128/90 on the left (S29). Until the firmware writes the mixer the boot values
  `5A 5A 5A 5A 79 27 27 79 …` apply.
- Channel 0 weights UltiSID 1 A-D, channel 1 UltiSID 2 A-D, channel 2 socket 1, channels 4 and 5 the sampler's pair. Muting (bytes 0-7
  zero, `u64_mute_sids`) silences all SIDs.
- The boot map mixes UltiSID 1 and 2 playing the same writes at unity: twice one UltiSID's level, as the hardware sums
  them. With the ARMSID fitted socket 1 adds 160/180 of it.

## TRX64 API: resolved by Spec 855, and what is left

Resolved at `1ce84b0`: one reSID per `Resid` (D1), the decode table (D2), a register file per chip (D3), the write
trace with chip and cycle (D4), the host read/peek door (D5). The observer tap and the `Sid6581` hook are gone; CPU
writes to a SID mapped at $DE00-$DFFF reach it. Left:

1. **`ResidConfig` is fixed at construction** (src/resid_ffi.rs, no model setter). A model change rebuilds the
   engine; oscillator and envelope phase restart.
2. **`Resid::clock_silent` must not be used.** It is reSID's batch `SID::clock(delta)` (vendor/resid/sid.cc:745),
   which never updates the ENV3 latch (vendor/resid/envelope.h:118). Mixed with sampled clocking on one engine, it
   froze the envelope: ENV3 70 after 100-20 000 cycles of attack 0, 255 with sampled clocking only. The bridge always
   clocks through `emit` and drops the samples when nothing listens.
3. **`write_full` stamps `Machine.clk`**, which is synced only after a run (855 D4). The bridge re-stamps DMA writes.
4. **Extra chips are not ticked.** Their OSC3/ENV3 come from the bridge's reSID cache, refreshed only when reSID is
   clocked: without an audio sink that is a DMA access.
5. **Engines built at different times** can produce a sample more or less per call (855 §2). Each chunk takes the
   first engine's count; the others are padded with their last sample or trimmed.

## Timing and audio path

- **With a sink:** traced writes are applied at their cycle (every engine is clocked to it first), and the engines
  follow every `advance_to` (1 ms emulated, S14 §4). Every engine gets the same cycle deltas in one loop; the mixed
  stereo frames go to the sink in emulated-time order. Before the first write no engine exists, and the sink gets
  silence at reSID's cadence.
- **Without a sink:** CPU writes only set registers; a DMA write or read clocks the gap, at most 1 s of cycles.
  - C64 programs read chip 0 from TRX64's own SID, and the firmware's DMA reads come with the 6510 stopped, so
    nothing observes the difference. OSC3/ENV3 of chips 1 and up stand still between DMA accesses.
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

S17 runs used copies of `run/flash.bin` with `--c64-roms`.

| Run | Options and script | Result |
|---|---|---|
| tone, ARMSID | `--flash run/flash-tone.bin --sid-socket1 armsid --audio-wav run/sid-tone.wav --script scripts/smoke-sid-tone.ctl` | detection lines above; `scripts/wav-tone.py run/sid-tone.wav --expect 1000`: 44100 Hz, 781 445 samples, peak-to-peak 27 157 (41.44 %: UltiSID 1 + UltiSID 2 + 160/180 socket 1), dominant **1000.0 Hz**, PASS |
| silence, ARMSID | `--flash run/flash-silent.bin --sid-socket1 armsid --audio-wav run/sid-silent.wav --script scripts/smoke-c64-type.ctl` | ` 42` printed; `wav-tone.py --silent`: peak-to-peak **0**, PASS |
| tone, default map | `--flash run/flash-ulti.bin --audio-wav run/sid-ulti.wav --script scripts/smoke-sid-tone.ctl` | `$$ SID1 = 0`; 781 794 samples, peak-to-peak 18 799 (28.69 %: UltiSID 1 + 2), dominant **1000.0 Hz**, PASS |
| two-SID PSID, SID player | `--flash run/flash-stereo.bin --sd run/sid-stereo.img --usb-keyboard --audio-wav run/sid-stereo.wav --script scripts/smoke-sid-stereo.ctl` | player lines above; 822 893 samples, peak-to-peak 8981 (13.70 %: one UltiSID at 172/180), dominant **1000.0 Hz**, PASS |
| cartridges and players | `--sd run/carts.img --usb-keyboard --script scripts/smoke-c64-carts.ctl` (docs/status/carts.md) | 27 `<NAME> PASS`, no FAIL/BAD, `ACTION REPLAY FROZEN`, both `Bytes loaded`, no `Time out!`; 153.6 s emulated, 32 s wall |
| device | `--audio on --script` (`wait 8000`) | W4-SID: stream opened on the Mac's default device, no warning; 8.0 s emulated at `--speed max`, 119 MIPS last interval. Not re-run for S17 |

`smoke-sid-tone.ctl` types `poke 54296,15:poke 54277,0:poke 54278,240` and `poke 54273,66:poke 54272,133:poke
54276,33`: volume 15, attack 0, sustain 15, F = 17029 (1000.1 Hz at PAL), sawtooth + gate. `wav-tone.py` takes the
last 32768 samples (mean removed, Hann window, FFT, parabolic peak); pure Python.

`scripts/make-stereo-sid.py run/sid-stereo/s03-stereo.sid` writes the PSID v3 for `smoke-sid-stereo.ctl`: header
$7A = $42, init writes the same voice to $D420-$D438 and leaves SID 1 alone, play is an RTS. The image is
`scripts/make-sd-image.sh run/sid-stereo.img` plus `scripts/add-sd-files.sh run/sid-stereo.img
run/sid-stereo/s03-stereo.sid`.

**No regressions (S17):** `UE2_FIRMWARE=… cargo test --workspace` green, 429 tests (c64-bridge 75, ue2-core 202,
ue2emu 75, ue2-net 20, ue2-vfat 12 + 20, rv32 13 + 1, ue2-mcp 11), no warnings; `cargo build -p ue2emu
--no-default-features` clean. `scripts/smoke-all.sh` and C64 A2-A5 were last run at W4-SID. The window was not
started (no GUI in this workflow).

## Tests

- `sid.rs`: the boot map (both UltiSIDs on every $D400-$D7FF block, nothing at $DE00); the SID player's 1- and 2-SID
  maps; splits 1/2 and 1/4 (all three bit pairs); socket enable and fitting; group numbering and chip 0 (UltiSID 1-A
  away from $D400, nothing mapped, UltiSID 2 at $DE00 listed ahead of the port); write fan-out; per-UltiSID model with
  registers kept; the ARMSID probe and readParams/set-mode/set-filter sequences; DMA reads of socket 1 (ARMSID, then
  reSID ENV3, the ARMSID's mode picks its engine's model); CPU writes clocked only with a sink; audio (silence before
  the first write, 48 kHz count for 985 000 cycles, 1000 Hz with two engines); mixer gains (padding and trimming,
  saturation, half gain, channel separation, mute); on a TRX64 machine: ENV3 of chip 1 through the door (peek and bus
  read) and the ARMSID's answer through the door.
- `lib.rs`: DMA reads of the default map (bus value, OSC3, ENV3; UltiSID 2 still at $D400; unmapped → 0); a 6510
  program writing the tone plays 600 ms → 26 460 samples, 500 periods in 0.5 s; with UltiSID 2 at $D420 a 6510 tone
  there plays the same, reaches TRX64's chip 1 and leaves chip 0 and UltiSID 1 untouched.
- `c64.rs`: core config writes reach the backend, unsynced, still latched; mixer bytes 0x00-0x13 reach the backend,
  the rest of the page swallows writes, all of it reads 0.
- `audio.rs`: WAV bytes and header sizes, ring pre-roll/drop/underrun, option defaults.

## Performance

**S17** (one run each, twice, host load average about 3): `wait 60000` on a fresh flash, `--log unmapped`,
`--speed max`. Default options: 121.2 and 120.5 host MIPS (12.4 s wall). `--audio-wav` + `--sid-socket1 armsid`: 92.9
and 93.7 host MIPS (16.0 s wall). The boot map puts socket 1 and both UltiSIDs at $D400, so the jingle runs three reSIDs
where W4-SID ran one: about 3.6 s wall per 60 s emulated instead of 1.4 s.

**W4-SID.** A6 method (docs/status/c64.md): `wait 60000`, `--log unmapped`, fresh flash, `--speed max`, one run at a time,
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

- Socket 2 and the second SID of a dual chip in a socket (ARM2SID) are not built; their probes find nothing.
- The drive and tape mixer channels have no source (no drive sound player, no tape model); the speaker mixer is not
  applied (S29 §4).
- UltiSID filter curves, resonance and digi level have no reSID equivalent and are ignored (855 D7). C64_VOICE_ADSR
  still reads 0, so the LED strip sees no envelopes.
- Chip 0's OSC3/ENV3 for C64 programs come from TRX64's fastsid, not reSID. Chips 1 and up read reSID only as of its
  last clocking: without an audio sink, the last DMA access.
- Unverified: a SID at $DE00-$DFFF answers reads ahead of the expansion port (855 §7); which receiver answers a read
  several decode (S17 §5 Q1); whether the FPGA builds four instances per UltiSID (Q2). Monitor peeks of $DE00-$DFFF
  show the cartridge.
- ARMSID filter settings are stored and read back but do not change reSID. The mode is not kept across emulator
  starts.
- PAL clock only (reSID at 985 248 Hz), like the rest of S14.
- The window's default `--audio on` was not heard by the agent; the device path was checked headless on silence.
