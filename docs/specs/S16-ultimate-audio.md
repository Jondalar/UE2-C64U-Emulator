# S16 — Ultimate Audio (the sampler): the integration

**Status:** built (2026-09-16).

The sampler is U64 FPGA hardware, not C64 hardware, so unlike UCI (S15) and the REU (853/854) **none of it is
TRX64's**. UE2 builds the whole block: the voice engine, both register faces, the mixer, and the IRQ line into the
C64. TRX64 only carries the line.

It is the first wall for two of the five Xander programs (`docs/status/xander-tests.md`): heartbeat-demo fails its
detection check and returns to BASIC, UltimateDemo2026 runs every scene without music.

Reference prefixes:

- **FW** = `firmware/1541ultimate` in this repo (v3.15-9, `b617777c`). Short names:
  - `regs` = FW/fpga/io/sampler/vhdl_source/sampler_regs.vhd
  - `s2` = FW/fpga/io/sampler/vhdl_source/sampler2.vhd; `pkg` = .../sampler_pkg.vhd; `accu` = .../sampler_accu.vhd
  - `tb` = FW/fpga/io/sampler/vhdl_sim/sampler_tb.vhd
  - `ss` = FW/fpga/cart_slot/vhdl_source/slot_server_v4.vhd; `bridge` = FW/fpga/ip/busses/vhdl_source/slot_to_io_bridge.vhd
  - `sampler.h` = FW/software/io/audio/sampler.h; `c64.cc` = FW/software/io/c64/c64.cc
- **Manual** = `ULTIMATEAUDIOMANUAL.md`, shipped with heartbeat-demo and UltimateDemo2026 (identical copies).
- Prior analysis: `docs/hw/12-gaps.md` Region A, written from the ELF and the same RTL.

## 1. What the sampler is

Eight DMA voices that stream PCM out of the U64's SDRAM and mix themselves into the analogue output, with no CPU
involvement after the registers are written. The firmware barely uses it — `Sampler::reset()` on every C64 reset and
a 64-byte clear before "Play MOD" — because its real client is a **C64 program**: the MOD-player cartridge, and
third-party software like the two demos above.

Two faces onto one register file:

| Face | Address | Who | Reach |
|---|---|---|---|
| Firmware | `SAMPLER_BASE` 0x10048000, 256 B aliased over 8 K | RISC-V, always | all 8 voices |
| C64 | `$DF20-$DFFF`, gated by `C64_SAMPLER_ENABLE` | 6502, when mapped | voices 0-6 only |

`$DF20 + n` is register offset `n` (`bridge:65,74`, `g_io_base` 0x048000, `g_slot_start` `$120`), so the C64 window
ends at offset 0xDF and **voice 7 is firmware-only** (`ss:953-957`).

## 2. How it works on the hardware

Every claim below is from the RTL. Where the vendor manual and the RTL disagree, the RTL wins and the disagreement
is named — the manual is wrong about byte order and about sample signedness, and the demo library's own header is
wrong about control bit 2.

### 2.1 Decode

Voice = offset bits 7:5, register = bits 4:0 (`regs:58,90`); bits above 7 are ignored, so the 256-byte file aliases
through the whole 8 K window. Stride 32 bytes, 8 voices (`sampler.h:7-8`).

**Writes** (8-bit, write-only):

| Off | Register | Written |
|---|---|---|
| 0x00 | control | see 2.2 |
| 0x01 | volume | `data(5:0)`, 6 bits |
| 0x02 | pan | `data(3:0)`, 4 bits |
| 0x04 | start bits 25:24 | **`data(1:0)` only** (`regs:111`) |
| 0x05/0x06/0x07 | start 23:16 / 15:8 / 7:0 | |
| 0x09/0x0A/0x0B | length 23:16 / 15:8 / 7:0 | |
| 0x0E/0x0F | rate 15:8 / 7:0 | |
| 0x11/0x12/0x13 | repeat A 23:16 / 15:8 / 7:0 | |
| 0x15/0x16/0x17 | repeat B 23:16 / 15:8 / 7:0 | |
| 0x1F | clear IRQ | bit 0 clears this voice; `0xFF` clears every voice (`regs:155-159`) |

**All multi-byte fields are big-endian** — MSB at the lowest offset (`regs:110-120`, recombined `:65-69`). The
testbench proves it: writing 0x01/0x23/0x45/0x00 to 0x04-0x07 fetches from 0x1234500 (`tb:81-84`). The manual's
"LSB first" (Manual:64-68) is wrong; both shipped C64 clients write big-endian and are right.

Offsets 0x03, 0x08, 0x0C, 0x0D, 0x10, 0x14, 0x18-0x1E are `when others => null` (`regs:161-162`): writes are
accepted and discarded. The gaps are where a 32-bit write's unused MSB lands.

**Reads** decode address bit 0 and nothing else (`regs:81-87`):

- **even** offset → the global IRQ status vector, bit *v* = voice *v* has a pending latch;
- **odd** offset → the constant **0x10** (= 16), the version byte.

There is no read-back of any written register, no per-voice status, no busy flag.

### 2.2 Control register

| Bit | Meaning |
|---|---|
| 0 | enable |
| 1 | repeat: at repeat B, jump to repeat A |
| 2 | **interrupt enable** — only then does the end of the sample set the status latch |
| 3 | not decoded |
| 5:4 | mode: `00` = 8-bit, **anything else** = 16-bit |
| 6 | interleave (skip a channel of a stereo stream) |
| 7 | not decoded |

Bit 2 is **not** "restart from start", which is what the demo library's header calls it (`audio.h`
`AUDIO_CTR_RESTART`). It is the IRQ enable (`s2:151-152`), and that is precisely why `audio_detect()` works: it
starts the probe voice with `$05` = enable + interrupt.

Bit 4 alone is the idiomatic 16-bit encoding (`sampler.h:25` `VOICE_CTRL_16BIT 0x10`, `tb:118`). This explains the
heartbeat player's otherwise undocumented alphabet: `$00` idle, `$10` 16-bit idle, `$11` 16-bit one-shot, `$13`
16-bit looped.

### 2.3 Power-up values and reset

`volume = 0x20`, `pan = 0x8`, everything else zero (`regs:33-54`). The register file **has no reset branch** — the
`reset` port is never read, so **the registers survive a C64 reset**, which is why the firmware clears the voices in
software on every reset IRQ. The IRQ latches are the exception: they *are* cleared by the C64 reset (`s2:229-232`).

`c_voice_control_init` (`pkg:40-52`, volume 63, pan 7, rate 283) is dead code and contradicts the real defaults. It
must not be used to seed a model.

### 2.4 The voice engine

Per output sample, evaluated at the divider's expiry (`s2:137-170`), **repeat B is tested before length**:

1. `position == repeat_b` → if enable *and* repeat, `position := repeat_a`; the length test is skipped this tick.
2. else `position == length` → the voice goes to `finished`, and if bit 2 is set the IRQ latch is set.
3. otherwise → fetch the next sample.

Consequences a model must keep:

- **Equality, never `>=`.** With a step of 2 or 4, an odd `length` or a misaligned `repeat_b` is never hit and the
  voice runs through memory forever. That is hardware behaviour, not a bug to fix.
- **The enable bit does not self-clear.** A finished voice parks in `finished` until software writes bit 0 = 0;
  re-arming needs 1→0→1. Nothing can observe the bit, since reads return only status and version.
- **Stopping from `playing` needs bit 0 *and* bit 1 clear** (`s2:163-165`). Clearing enable alone while repeat is
  set leaves the voice playing to its end.
- **A stopped voice holds its last sample as DC** (`sample_out` is zeroed only on the idle→start transition,
  `s2:134`) and keeps being summed into the mixer.

### 2.5 Fetch and format

Samples are **signed two's complement** (`s2:221-225`, `tb:76`). The manual says "8-bit unsigned" in one place and
"signed" in another; the RTL settles it, and the MOD player is right to convert nothing.

- **8-bit**: one fetch, the byte lands in the **high** half, so the value is `(i8) byte * 256`.
- **16-bit**: two fetches, **little-endian**, low byte first.
- Position step: 8-bit +1, 8-bit interleaved +2, 16-bit +2, 16-bit interleaved +4 (`s2:179,185,195`).

Address is **26 bits, flat**: `start + position` (`s2:174,191`), wrapping mod 2^26. Offset 0x04 keeps only two
bits, so a client writing a 32-bit address must keep bits 31:26 clear or they are dropped. Writing 0x01 there means
bits 25:24 = 01 → 0x0100_0000, which is the REU aperture (`ss:18` `g_ram_base_reu`) — the same SDRAM through the
same arbiter (`ss:1144-1160`). The hardware has no bank concept; "bank 01 = REU" is only a client convention.

Length and both repeat points are **positions relative to start**, not addresses.

### 2.6 Rate

There is no fractional phase accumulator. A voice's divider counts a reference tick that the prescaler normalises
to **exactly 160 ns (6.25 MHz)** for every supported clock, given 8 voices (`s2:44-63,138-161`). On a 100 MHz
build that is `CLOCK_HZ / 16` exactly — the block is FPGA-clocked, not C64-clocked.

```
T_sample = (rate + 1) * 160 ns + N_fetch * (8 / f_clk)      N_fetch = 1 (8-bit) or 2 (16-bit)
f_out    = 6.25 MHz / (rate + 2)      16-bit, 100 MHz
         = 6.25 MHz / (rate + 1.5)     8-bit, 100 MHz
```

The clients' `6250000 / rate` is about 0.7 % high at rate 283 and badly wrong at small rates. **`rate = 0` is
legal and means fastest, not stopped.**

### 2.7 Mixer

Per voice, per 6.25 MHz round (`accu:54-84`):

```
scaled = (sample16 * volume6) >> 5                  volume is unsigned 0..63, so 32 = unity
pan < 8:  L = 7, R = pan(2:0)        pan >= 8:  L = ~pan(2:0), R = 7
current  = scaled * pan_factor                       factors 0..7
accu     = saturating_add_21bit(accu, current)       saturates at +/- 2^20, does not wrap
out18    = accu >> 3
```

Pan 0x7 and 0x8 are both centre, and **centre is not normalised**: a centred voice is twice as loud per side as one
side of a hard-panned pair. The output pair is handed on at 6.25 MHz regardless of any voice's own sample rate.

### 2.8 Memory, and what happens when it is slow

One byte per request, no bursts, no cache (`s2:264`), lowest priority of the three masters. **Nothing ever
stalls**: the FSM issues a request and moves on; if the data is late the previous sample is re-latched, and an
overflowing request FIFO drops silently (`s2:244-258`). For an emulator, memory is always ready.

### 2.9 The C64 face

`C64_SAMPLER_ENABLE` (`C64_CARTREGS_BASE + 0xE` = 0x1004000E, bit 0) gates the window. The firmware clears it at
the start of `set_emulation_flags`, sets it if `CAPAB_SAMPLER` *and* the config item `CFG_C64_MAP_SAMP` (default
**disabled**) are both on (`c64.cc:310-326`), and sets it regardless of the config when a cartridge requires
`CART_SAMPLER` — which the MOD player does. It **reads the register back** and prints it as `Sampler: %b` in both
cart-init lines (`c64.cc:1290-1291,1386-1387`).

The IRQ is a level, the OR of the voice latches (`s2:101`), ANDed with the enable and driven onto the **cartridge
IRQ line, never NMI** (`bridge:50-51`, `ss:1077`). It releases when the last latch is cleared through offset 0x1F,
when the enable goes away, or on a C64 reset.

With the window disabled the C64 reads **open bus**, not zero (`bridge:85-88`) — which is how a detection routine
tells "no sampler" from "sampler idle".

### 2.10 What the C64 detection routine demands

`audio_detect()` (`audio.c:111-189`, a translation of ModPlayer_16k's `detectaudio`) is the gate both demos stand
behind, so the model must satisfy it honestly, not by faking a version byte:

1. Write 0 to control of all 7 reachable voices; write `$FF` to voice 0's 0x1F.
2. Read `$DF20` **exactly 256 times**; every read must be `$00`.
3. Program voice 0: volume 0, start `$01000000`, length 256, rate 1, control `$05` (enable + interrupt).
4. Read `$DF20` up to **128 times** waiting for non-zero.
5. Read it up to **256 more times**; every read must be exactly `$01`.
6. Write `$FF` to 0x1F, return found.

So the status bit must **latch**, reading must **not** clear it, and stale latches from other voices would break
step 5 — which is why step 1's global clear matters. At rate 1 a 256-byte sample lasts about 61-102 µs, well inside
the 128-read budget.

`audio_get_version()` then reads `$DF21` and the demos display it; hardware on 3.15 reports **16**, which is the
RTL's 0x10.

## 3. What UE2 builds

### 3.1 The host interface (`ue2-core/src/c64host.rs`)

Four defaulted methods, so a backend without the block and `--c64 none` are unaffected:

- `has_sampler() -> bool` — whether the backend serves it.
- `sampler_read(&self, off: u16) -> u8` — side-effect free, so `peek8` can use it.
- `sampler_write(&mut self, off: u16, val: u8)`.
- `set_sampler_enabled(&mut self, on: bool)` — from cart register +0x0E bit 0.

### 3.2 The firmware window (`ue2-core/src/devices/c64.rs`)

`SAMPLER` = 0x4_8000, size 0x2000, a twelfth entry in `WINDOWS`. The dead
`add_table(map, 0x1004_8000, 0x2000, "sampler", &[])` **must go**, or `install` panics on the overlap — the same
trap S15 met with UCI. Read, write and peek arms mirror the UCI arms, guarded by `has_sampler()`; without a backend
block the window stays RAZ/WI, as it is today.

Cart register 0x0E gets a named constant and an arm beside `REU_ENABLE`: it keeps its latch (the firmware reads it
back) *and* calls `set_sampler_enabled`.

### 3.3 The block (`c64-bridge/src/sampler.rs`)

The register file, the eight voices and the mixer, per §2. It owns its own DDR pointer cell of `ReuRam`'s shape —
updated on the same `lend_ddr` — because the sampler reads the same SDRAM but is **not** gated by the REU's fitted
size: a sampler DMA works with no REU attached at all.

It is also a `trx64_core::expansion::ExpansionDevice` for `$DF20-$DFFF`, attached with `attach_expansion_also` while
`C64_SAMPLER_ENABLE` is set, and it must be asked **before** the REU: TRX64's REU mirrors its registers across
`$DF20-$DFFF` (`trx64-core/src/reu.rs:775-798`), which the U64's own REU does not do, and that mirror would
otherwise answer `audio_detect()`'s first 256 reads with REU status and fail the probe. Its `lines()` carry the IRQ.

### 3.4 Audio (`c64-bridge/src/sid.rs` untouched)

`Sid::catch_up` is **not modified**: it is CRITICAL by impact (22 impacted symbols, 4 direct callers, 7 processes
including both drive processes and the cart-DMA path), and it is the one place that turns cycles into samples. The
sampler mixes *behind* it instead: the sink `Sid` pushes into becomes a mixer that adds the same number of sampler
samples and forwards the sum. reSID stays the clock master, and the change is confined to what the sink is.

The sampler's own engine advances on the **emulator** clock (6.25 MHz = `CLOCK_HZ / 16`), not on the C64's, and is
resampled to the sink rate.

The whole existing path is mono, the hardware is stereo: UE2 **downmixes** `(L + R) / 2` and scales 18-bit to
16-bit. Stereo is a later change, and it would touch `AudioSink`, the ring, the WAV writer and `wav-tone.py`.

### 3.5 The capability (`c64-bridge`, `ue2emu/src/runner.rs`)

`CAPAB_SAMPLER = 0x0020_0000` (bit 21) joins `CAPAB_EEPROM` and `CAPAB_COMMAND_INTF`, OR'd into the ITU word when
the backend has the block. Without it the firmware offers no "Play MOD", never maps the window, and
`docs/hw/12-gaps.md`'s standing instruction "T0: return bit21 = 0" is reversed for the TRX64 build.

The default word becomes **0x34E40222** with USB, EEPROM, UCI and the sampler.

## 4. What is deliberately not built

- **Stereo out.** The pan law is modelled and then downmixed to the mono sink (3.4).
- **Mixer gains and audio routing.** `U64_AUDIO_MIXER` (0x10100500) and `AUDIO_SEL_BASE` (0x10060700) stay
  write-only stubs; the sampler is always audible at unity. The firmware writes 6/7 there for "Play MOD".
- **The C64 read pipeline.** Hardware returns a previous value, preset `0xAA`, until the internal response lands
  (`bridge:72-79`); UE2 answers immediately.
- **Memory contention.** No FIFO, no dropped fetches, no repeated samples under load (§2.8).
- **Open bus with the window disabled.** UE2 lets the expansion device go away, so whatever else claims
  `$DF20-$DFFF` answers — the REU mirror, in practice.
- **`sampler.vhd` (v1).** Only `sampler2` is instantiated (`ss:987`); v1 differs in its prescaler and is testbench
  only.

## 5. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean (the trait defaults keep a
   backend-less build working).
2. Unit tests: the register file (big-endian assembly, the write holes, even/odd reads, power-up values, no reset),
   the voice engine (one-shot end, repeat B→A, equality misses, enable not self-clearing, stop needing both bits,
   DC hold), the mixer (volume >> 5, the pan law, saturation), and the 26-bit address wrap.
3. A test that replays `audio_detect()`'s exact sequence — 256 zero reads, the probe, ≤128 reads to the latch, 256
   reads of exactly `$01`, ack — against the modelled block.
4. `$DF20` reaches voice 0 and `$DFE0` voice 6; voice 7 is firmware-only; the window answers only while
   `C64_SAMPLER_ENABLE` is set, and the firmware reads that bit back.
5. Firmware in the loop: the cart-init line reads `Sampler: 01`, and `--caps` no longer needs to be overridden.
6. heartbeat-demo shows `Audio [ OK ] v16` and continues past its detection screen instead of returning to BASIC;
   UltimateDemo2026 shows the same and plays its MOD.
7. A WAV assertion in the shape of `smoke-sid-tone.ctl`: a sampler voice alone (SID silent) produces the expected
   dominant frequency through `scripts/wav-tone.py --expect`.
8. `scripts/smoke-c64-carts.ctl` still 27/27.

## 6. Open questions

1. **The U64-II FPGA top is not in this repo**, so `g_clock_freq`, `g_num_voices` and `g_support_16bit` for the
   shipped bitstream are not provable. The model assumes 100 MHz, 8 voices, 16-bit support — the only combination
   consistent with the firmware's `-DCLOCK_FREQ=100000000` and with `sampler2`'s prescaler table.
2. **Does the U64's REU mirror `$DF20-$DFFF`?** The manual says the two coexist, which implies it does not; TRX64's
   REU does. UE2 resolves it by asking the sampler first (3.3), but the disabled-window case still differs from
   hardware (§4).
3. **What the 6502 sees mid-fetch.** The hardware's `0xAA` preset is observable in principle; no known client reads
   the window fast enough to see it.
4. **Interleave with 8-bit** is decoded (step +2) but no client uses it; untested against hardware.
5. **Whether the firmware ever reads the block.** It does not today (`docs/hw/12-gaps.md` Region A: "Firmware
   READS: none"), so the firmware face is write-only in practice.
6. **Resolved: UltimateDemo2026's black screen.** After S16 the demo passed `audio_detect()` and loaded, then drew
   black. With TRX64 pinned at `1ce84b0` (0.7.2's UCI response-pointer fix plus Spec 855) and S17 it loads its MOD
   over UCI into the REU, draws its scenes and plays the MOD. Which change fixed it was not isolated; an earlier
   test against a TRX64 copy with only the pointer fix still drew black.
