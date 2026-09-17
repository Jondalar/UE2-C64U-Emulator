# S17 — UltiSID: more than one SID (the host side of TRX64 Spec 855)

TRX64 Spec 855 (pinned at `1ce84b0`) gives a machine several SID register sets: one reSID per handle, a SID map the
host sets, a write trace `(chip, reg, value, clk)`, and a host read/peek door. S17 is UE2's half: the U64's SID decode,
the reSID engines behind it, the audio mixer, and the ARMSID behind the door.

Until S17 one reSID served socket 1 and UltiSID 1 together, and socket 2, UltiSID 2 and the split instances were not
there (`docs/status/sid-audio.md` gap 1).

References: **FW** = `firmware/1541ultimate/software`; `cfg.cc` = FW/u64/u64_config.cc; `u64.h` = FW/system/u64.h;
`editor` = FW/u64/sid_editor.cc; `855` = TRX64 `docs/855-more-than-one-sid.md`.

## 1. The hardware, as the firmware drives it

The FPGA's SID mapper and mixer are not in the open tree. Everything below is read off how the firmware uses them.

### 1.1 Four decoders

| Decoder | BASE | MASK | Enable |
|---|---|---|---|
| Socket 1 | `0x10180008` | `0x1018000C` | `SID1_EN 0x11` |
| Socket 2 | `0x09` | `0x0D` | `SID2_EN 0x12` |
| UltiSID 1 | `0x0A` | `0x0E` | — |
| UltiSID 2 | `0x0B` | `0x0F` | — |

`a = (addr >> 4) & 0xFF` (bits A11..A4). A decoder hits when `(a & MASK) == BASE`, the enable is set for a socket, and
`addr` is in `$D400-$D7FF` or `$DE00-$DFFF` (`editor:162-165`). `0x01` is "unmapped": `(a & 0xFE)` never equals it
(`cfg.cc:202-203`). The menu offers `$D400-$D7E0` and `$DE00-$DFE0` in `$20` steps (`cfg.cc:218-226`).

The firmware writes MASK `0xFE` (a 32-byte window), clears split bits from both BASE and MASK (`fix_splits`), and with
"Auto Address Mirroring" clears every A5..A9 bit on which all `$D4-$D7` decoders agree (`cfg.cc:2406-2461`). A fresh
boot therefore maps all four decoders to `0x40/0xC0` — `$D400-$D7FF` mirrored — with both sockets disabled: **UltiSID 1
and 2 both take every write there**, and both are mixed at centre.

### 1.2 Split: up to four SIDs per UltiSID

`C64_EMUSID_SPLIT` (`0x29`, index 0..7) picks address bits inside an UltiSID's window that select **separate register
sets** A-D, not mirrors. The firmware's 8-SID support says so: the LED strip lists `UltiSID1-A..D`, `UltiSID2-A..D`
and reads their envelopes one by one (`led_strip.cc:55-56, 323-325`); the split options came in "for 8sid operation"
(commit `b95fed6c`).

| Index | Bits | Instance |
|---|---|---|
| 0 | none | A |
| 1-4 | A5, A6, A7, A8 | bit set → B |
| 5 | A5, A6 | `(a & 0x06) >> 1` |
| 6 | A5, A8 | `(a & 0x02) >> 1` + `(a & 0x10) >> 3` |
| 7 | A7, A8 | `(a & 0x18) >> 3` |

(`cfg.cc:266, 320`; `editor:166-181`.) The four instances of one UltiSID share its settings and its mixer channel
(`cfg.cc:2019`).

`C64_STEREO_ADDRSEL` (`0x14`) does the same for the sockets, for dual chips. The emulated ARMSID is a single chip, so
both halves reach it.

### 1.3 Per-UltiSID settings

`WAVES` (`0x20/0x21`: 0 = 6581, 1 = 8580), `RES` (`0x22/0x23`), `DIGI` (`0x27/0x28`) and a 1024-entry filter curve
(`0x10185000/0x10185800`), all per UltiSID, not per instance (`cfg.cc:840-847, 1641-1662`).

### 1.4 Mixer

`U64_AUDIO_MIXER` `0x10100500`, 20 bytes, write-only, two bytes per channel: 0 UltiSID 1, 1 UltiSID 2, 2 socket 1,
3 socket 2, 4-5 sampler, 6-7 drives, 8-9 tape. Byte `2c` = `(pan_ctrl[10-pan] · vol) >> 8`, byte `2c+1` =
`(pan_ctrl[pan] · vol) >> 8` (`cfg.cc:1342-1358`). Boot defaults, computed: `5A 5A 5A 5A 79 27 27 79 79 27 27 79
19 0D 0D 19 04 04 04 04`. Muting zeroes bytes 0-7 (`cfg.cc:1318-1331`). The SID player rewrites the SID channels per
tune (`cfg.cc:1986-2028`).

### 1.5 What the SID player exercises

It unmaps all four decoders, then maps SID 1 at `$D400`, SID 2 at the PSID header's `$7A` byte and SID 3 at `$7B`
onto sockets or UltiSIDs, and sets the mixer. A one-SID tune maps UltiSID 1 at `0x40/0xC0`, mirrored
(`cfg.cc:2043-2104`). It never writes `SPLIT`, `ADDRSEL` or the socket enables.

## 2. What UE2 builds

### 2.1 Routing: blocks, receivers, groups

The address space a SID can occupy is 48 blocks of 32 bytes: 32 in `$D400-$D7FF`, 16 in `$DE00-$DFFF`. For each
block UE2 computes its **receivers** — every (decoder, instance) that hits — from the latched decoders:

- an UltiSID receiver is instance A-D of that UltiSID, from the split bits of `a`;
- a socket receiver needs the enable and a fitted chip (`--sid-socket1 armsid`; socket 2 is never fitted).

TRX64 routes an address to **one** chip, but the U64 routes a write to every decoder that hits. So a TRX64 chip is
not a physical SID but a **group**: one distinct receiver set. UE2 numbers the groups (the group holding UltiSID 1-A
is chip 0, the one TRX64 ticks), hands TRX64 one `SidMapping` per block, and fans each traced write out to every
receiver of its chip's group.

- All 32 `$D400-$D7FF` blocks are always listed, blocks nobody decodes pointing at an empty group, so TRX64's
  fallback to chip 0 never fires.
- `$DE00-$DFFF` blocks are listed only when something receives them, with `ahead_of_expansion = true`. That is an
  **unverified assumption**: the firmware offers those addresses as SID addresses, so a mapped SID there is meant to
  be heard. The RTL that would settle it is closed (855 §7).
- The map is rebuilt and set whenever a decode latch changes.

### 2.2 Engines

One reSID per receiver that has ever received a write: UltiSID *n* instance *i* and socket 1. Its model is `WAVES`
of its UltiSID, or the ARMSID's mode for socket 1. A model change rebuilds that engine and replays its registers.
Every engine is clocked with the same cycle deltas in the same loop, so their sample counts stay aligned (855 §2); an
engine built later is padded or trimmed to the first engine's count, at most a sample per chunk.

### 2.3 Writes

- **CPU, REU DMA and host pokes:** TRX64's write trace pushes `(clk, chip, reg, value)` into a thread-local queue;
  `advance` applies them in order, each clocked to its cycle when a sink listens, as before.
- **Firmware DMA:** `dma_write` goes through `Machine::write_full`, whose trace stamps a stale clock (855 D4), so the
  queued records are re-stamped with the live cycle and applied at once.
- `SidTap` stays in the CPU run calls as an empty observer: `run_cpu` and `run_held` are CRITICAL by impact
  analysis (25 symbols), and nothing in them needs to change.

### 2.4 Reads

- **6502, chip 0:** TRX64's own model (855 D3), unchanged.
- **6502, chips 1 and up:** TRX64 does not tick them, so their OSC3/ENV3 would read 0. UE2 answers `$1B/$1C`
  through the host door (855 D5) from the group's first reSID, cached after every catch-up.
- **ARMSID:** in configuration mode socket 1's group answers `$1B/$1C` through the same door, read and peek alike.
- **Firmware DMA reads:** UE2's own decode, as before — the address is known there.

### 2.5 Mixer

`C64Port` takes over `0x10100500-0x101005FF` from the board's write-sink table; bytes `0x00-0x13` reach the backend
(`mixer_write`), the rest (speaker mixer, resampler) stay write sinks, and all of it reads 0. Mono gain of a channel
is `(byte 2c + byte 2c+1) / 180`, so the firmware's 0 dB centre (`5A/5A`) is unity: a single UltiSID at the boot mixer
is as loud as today. Channels 0-2 weight the SID engines; the sampler, drive and tape channels are not applied.

### 2.6 Host interface

`C64Backend::mixer_write(off, val)`, defaulted.

## 3. Deliberately not built

- `RES`, `DIGI` and the filter curves: no reSID equivalent (855 D7). Stored nowhere.
- Stereo: pan pairs are summed to the mono sink.
- Socket 2, and the second SID of a dual chip in a socket.
- `C64_VOICE_ADSR` (`0x10180080+`, LED strip): reads 0.
- Speaker mixer (`0x10100540`); sampler, drive and tape mixer channels.
- Readback of chips 1 and up without an audio sink: their OSC3/ENV3 cache is refreshed only when reSID is clocked.
- Monitor peeks of `$DE00-$DFFF` show the cartridge, not a SID mapped there (855 §4).

## 4. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean.
2. Unit tests: the boot map (UltiSID 1 and 2 both receive `$D400-$D7FF`), the SID player's 1- and 2-SID maps, splits
   1/2 and 1/4, socket enable and fitting, group numbering and chip 0, write fan-out, per-UltiSID model, mixer gains
   and mute, ENV3 readback for chip 1, ARMSID readback through the door.
3. A bridge test: with UltiSID 2 at `$D420`, a 6502 program's writes to `$D420+` play a tone and leave chip 0 silent.
4. Firmware in the loop: the SID player plays a PSID v3 whose second SID alone sounds a 1000 Hz tone, and
   `scripts/wav-tone.py --expect 1000` passes.
5. `smoke-sid-tone.ctl` still gives 1000 Hz, `smoke-c64-carts.ctl` still 27/27.

## 5. Open questions

1. Which decoder wins a read when several hit is not visible in the sources; UE2 reads the group's first receiver.
2. Whether the FPGA builds four instances per UltiSID or mirrors — the firmware assumes four.
3. `ahead_of_expansion` for `$DE00-$DFFF` (§2.1).
4. Which mixer byte is left: the labels say byte `2c`, the variable names say right. Irrelevant for mono.
