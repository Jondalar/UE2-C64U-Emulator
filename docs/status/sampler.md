# Ultimate Audio — the sampler

Spec: `docs/specs/S16-ultimate-audio.md`. Prior hardware analysis: `docs/hw/12-gaps.md` Region A.

Eight DMA voices in the U64 FPGA that stream PCM out of the same SDRAM the REU uses and mix themselves into the
analogue output. Unlike UCI (S15) and the REU (TRX64 Specs 853/854), **none of this is TRX64's**: UE2 builds the
register file, the voice engine, the mixer and both faces, and TRX64 only carries the IRQ line on the expansion port.

It was the first wall for two of the five Xander programs (`docs/status/xander-tests.md`): heartbeat-demo failed its
detection check and returned to BASIC, UltimateDemo2026 ran every scene without music.

## The two faces

| Face | Address | Reach |
|---|---|---|
| Firmware | `SAMPLER_BASE` 0x10048000, 256 B aliased over 8 K | all 8 voices |
| C64 | `$DF20-$DFFF` while `C64_SAMPLER_ENABLE` (cart regs +0x0E) is set | voices 0-6 |

`$DF20 + n` is register offset `n`, so the C64 window ends at offset 0xDF and **voice 7 is firmware-only**.

Reads decode one address bit: **even** offsets are the IRQ status vector (bit *v* = voice *v*), **odd** offsets the
version constant **0x10** — which is the 16 both demos display. Nothing else reads back; there is no per-voice status
and no read-back of any written register.

## What UE2 builds

| File | What |
|---|---|
| `crates/c64-bridge/src/sampler.rs` | the block: register file, eight voices, mixer, the `$DF20-$DFFF` device, the DDR store, and `SamplerMix` |
| `crates/ue2-core/src/devices/c64.rs` | the firmware window (a twelfth `WINDOWS` entry) and cart register +0x0E |
| `crates/ue2-core/src/c64host.rs` | `has_sampler`, `sampler_read`, `sampler_write`, `set_sampler_enabled`, all defaulted |
| `crates/c64-bridge/src/cart.rs`, `lib.rs` | `CAPAB_SAMPLER` (bit 21) and the backend wiring |
| `crates/ue2emu/src/runner.rs` | the capability into the ITU word |

Four decisions worth keeping in view:

- **The engine runs on emulated time, not on the audio sink.** `Trx64Backend::advance_to` drives it at
  6.25 MHz = the emulator's clock / 16, and rendered samples go into a bounded queue the mixer drains. The first
  build had the mixer pull the voices instead, which looks equivalent and is not: a `--headless` run without
  `--audio` or `--audio-wav` has **no sink at all**, so the voices never turned, the end-of-sample status bit never
  set, and `audio_detect()` failed against a block that was otherwise correct. A voice reaching its end is
  emulation, not audio. `the_voices_run_with_nobody_listening` pins it.
- **`Sid::catch_up` is untouched.** It is what turns cycles into samples for the whole machine — impact rates it
  CRITICAL, 22 impacted symbols, 4 direct callers, 7 processes including both drive processes and the cart-DMA path.
  The sampler mixes *behind* it: the sink `Sid` pushes into is a mixer that adds the voices and forwards the sum, so
  reSID keeps owning the clock and the only thing that changed is what the sink is.
- **The block is attached before the REU** and stays on the port for the life of the machine, because
  `C64_SAMPLER_ENABLE` gates the window, not the block's existence. That order matters: TRX64's REU mirrors its
  registers across `$DF20-$DFFF`, the U64's does not, and TRX64's device chain keeps the **first** answer. Without
  the sampler in front, `audio_detect()`'s 256 zero reads would get the REU's status register instead.
- **The store is not the REU's.** Same DDR, same lease, a second cell: the sampler is not gated by `C64_REU_SIZE`,
  so a voice plays with no REU fitted at all.

## What the hardware says, where the manual is wrong

The model follows `sampler2.vhd` / `sampler_regs.vhd`. The vendor manual shipped with the demos disagrees three
times, and the RTL wins each time:

- **Byte order is big-endian**, MSB at the lowest offset. The manual says "LSB first"; the testbench writes
  0x01/0x23/0x45/0x00 and the DUT fetches from 0x1234500, and both shipped C64 clients write big-endian.
- **Samples are signed** two's complement. The manual says unsigned in one place and signed in another; the MOD
  player converts nothing, which is correct.
- **Control bit 2 is the interrupt enable**, not a "restart" flag as the demo library's own header calls it. That is
  precisely why `audio_detect()` works: it starts its probe voice with `$05` = enable + interrupt.

Bit 4 belongs to the mode field 5:4 (`00` = 8-bit, anything else 16-bit), which explains the heartbeat player's
otherwise undocumented `$10`/`$11`/`$13` alphabet.

Behaviour that looks like a bug and is not, so the model keeps it: the position tests are **equality, never `>=`**,
so a misaligned length or repeat point is stepped over and the voice runs on; the enable bit **never clears
itself**; stopping a playing voice needs bit 0 *and* bit 1 clear; a stopped voice **holds its last sample as DC**
and keeps being summed in; power-up is volume 0x20 and pan 8, and the register file **survives a C64 reset** (only
the IRQ latches are cleared), which is why the firmware clears the voices in software on every reset.

## Verified

| Check | Result |
|---|---|
| Capability word | bit 21 (CAPAB_SAMPLER) set beside EEPROM and UCI: `34640226` |
| Firmware console | `Sampler found in FPGA... IO map: Enabled!`, cart init `Sampler: 01` |
| heartbeat-demo v1.0.1 | `Audio : [ OK ]  v16` — it passes the check that used to send it back to BASIC, and goes on to `Song : [ OK ] tempo 88, 8 SIDs` and its start prompt (`run/xander/ctl/07-heartbeat.ctl`) |
| UltimateDemo2026 v1.0.1 | runs: detection passes, the MOD loads over UCI into the REU, the scenes draw and the MOD plays (headless: sound from 20 s on, the tunnel scene at 180 s; also watched live). It drew black until TRX64 `1ce84b0` and S17; which change fixed it was not isolated (S16 §6) |
| heartbeat-demo playback | the song plays: `--audio-wav` carries 40 s of sound at about 46000 peak-to-peak of 65535, from 70 s emulated on. The start prompt needs the menu button pressed first, because "Run Cart" leaves the keyboard with the firmware menu (`docs/status/xander-tests.md` gap 3) |

The unit tests cover the register file (big-endian assembly, the write holes, both read decodes, power-up values),
the voice engine (one-shot end, repeat B→A, equality misses, the enable bit, the two-bit stop, DC hold), the mixer
(volume `>>5`, the pan law with both centre codes, saturation), the C64 window's reach, and a replay of
`audio_detect()`'s exact sequence — 256 zero reads, the probe, the wait inside 128 reads, 256 reads of exactly
`$01`, the acknowledge.

## Known gaps

- **Output routing.** The pair leaves through mixer channels 4 and 5 in stereo (S29). `AUDIO_SEL_BASE` (0x10060700)
  stays a write-only stub; the firmware writes 6/7 there for "Play MOD", and only with `CAPAB_SAMPLER`.
- **No read pipeline.** Hardware returns a previous value, preset `0xAA`, until the internal response lands; UE2
  answers immediately.
- **No memory contention.** One byte per request with no FIFO, no dropped fetches and no repeated samples under
  load: for the model, memory is always ready.
- **Open bus.** With the window closed UE2 lets the device answer nothing, so whatever else claims
  `$DF20-$DFFF` does — in practice the REU's mirror, where hardware would float the bus.
- **8-bit interleave** is decoded (step +2) but no client uses it, so it is untested against hardware.
- **Switching the block off** is `--caps`, and only because this work made the option mean what it says: the
  frontend used to OR `CAPAB_EEPROM`, `CAPAB_COMMAND_INTF` and now `CAPAB_SAMPLER` into the word *after* the command
  line was applied, so an explicit word could not clear any of them. It is now used as given
  (`MachineConfig::capabilities_explicit`), which is what makes a machine without one of these features modellable
  — and what a control run needs. The firmware's own "Map Ultimate Audio $DF20-DFFF" setting (default disabled)
  still decides whether the C64 window opens.
