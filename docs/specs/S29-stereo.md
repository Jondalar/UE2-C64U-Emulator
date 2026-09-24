# S29 — Stereo: the mixer's pan reaches the speakers

**Status:** built (2026-09-23).

**Owns:**
- `crates/c64-bridge/src/sid.rs`: `AudioSink` carries interleaved stereo; a left and a right gain per receiver
- `crates/c64-bridge/src/sampler.rs`: the sampler's L/R pair through mixer channels 4 and 5
- `crates/c64-bridge/src/lib.rs`: mixer bytes reach the sampler too; the stream tap is stereo
- `crates/ue2emu/src/audio.rs`: ring, device fill and WAV in stereo
- `crates/ue2-core/src/devices/streams.rs`: the audio stream takes stereo as it comes
- `scripts/wav-tone.py`: mono or stereo
- `docs/status/sid-audio.md`, `sampler.md`, `gaps.md`

**Reads:** `software/u64/u64_config.cc:292-317, 452-473, 1343-1365` (tables, defaults, `setMixer`),
`fpga/io/audio/generic_mixer.vhd`, `fpga/io/sampler/vhdl_source/sampler_accu.vhd:54-84`, S16, S17 §2.5, S20, S24.

## 1. The mixer

`setMixer` writes two bytes per channel into U64_AUDIO_MIXER (0x10100500), for ten channels:

| Ch | Source | Default (vol / pan) |
|---|---|---|
| 0, 1 | UltiSID 1, 2 | 0 dB / Center |
| 2, 3 | Socket 1, 2 | 0 dB / Left 3, Right 3 |
| 4, 5 | Sampler L, R | 0 dB / Left 3, Right 3 |
| 6, 7 | Drive 1, 2 | |
| 8, 9 | Tape read, write | |

Byte `2i` is `pan_ctrl[10 - pan] * vol >> 8`, byte `2i + 1` is `pan_ctrl[pan] * vol >> 8`. "Left 5" (pan 0)
puts the whole gain into byte `2i`, so byte `2i` is the **left** gain and `2i + 1` the right. The U64 top-level RTL
is not in the tree; `generic_mixer.vhd` sums even and odd bytes into separate outputs, which agrees.

A centred 0 dB channel is 0x5A in each byte (`pan_ctrl[5] = 181`). The per-side unity is therefore 90: a centred
channel at 0 dB plays at unit gain on each side, as the mono sum did before (`UNITY = 180` over both bytes). Hard left
at 0 dB is 128/90, the hardware's +3 dB pan law.

## 2. The stream

`AudioSink::samples` takes interleaved frames, left then right, at the sink rate. Every producer and consumer moves
to it at once:

- **SIDs.** A receiver gets `(gain_l, gain_r)` from its channel's two bytes; `clock_to` sums two accumulators and
  divides each by 90. The worker (S20) gets the pair through `Cmd::Gains`. Socket 2 has no receiver.
- **Sampler.** `Sampler::mix` keeps its L/R pair (`sampler_accu.vhd` has two outputs) instead of averaging it.
  Mixer channel 4 weights the left output and channel 5 the right, each into both sides:
  `L = (l · b8 + r · b10) / 180`, `R = (l · b9 + r · b11) / 180`. At the default settings that is within 1 dB of the
  mono average the sampler had. The queue holds frames; `SamplerMix` adds them to the SID frames.
  `Trx64Backend::mixer_write` hands the bytes to the sampler as well as the SIDs.
- **Stream tap (S24).** The tap takes the SID frames as they are; `Streams::send_audio` stops duplicating.
- **Ring.** Holds frames, drops whole frames. The device callback plays L and R on channels 0 and 1, their mean on a
  mono device, and the mean on any channel past 1.
- **WAV.** Two channels, 16 bit; `--audio-wav` help says so.
- **wav-tone.py** takes mono or stereo; stereo is analysed as `(L + R) / 2`, and `--channel left|right` picks one.

## 3. Checks

- Unit: a receiver panned hard left is silent on the right; the default socket-1 pan (Left 3) puts more on the left;
  the sampler's pan reaches the sides through channels 4 and 5; a mono device gets the mean.
- The SID tone smoke passes with `wav-tone.py` on the stereo WAV (1000.1 Hz PAL).
- The heartbeat demo plays; the user listens.

## 4. Not in this spec

- Drive sounds (channels 6, 7): an FPGA sample player (`floppy_sound.vhd`) reads `snds1541.bin` from DDR on step,
  head-bang and motor events. UE2 has no such player; its own spec when wanted.
- Tape sounds (8, 9): no tape model in UE2.
- The speaker mixer (+0x40), the resampler (+0x80) and AUDIO_SEL_BASE: stored or ignored as today.
