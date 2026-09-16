//! Ultimate Audio: the U64's eight DMA voices (docs/specs/S16-ultimate-audio.md).
//!
//! Unlike UCI and the REU this is not C64 hardware and not TRX64's: the block sits in the U64 FPGA, streams PCM out of
//! the same SDRAM the REU uses, and mixes itself into the analogue output. UE2 builds all of it. TRX64 only carries the
//! IRQ line, through the expansion port (Spec 850).
//!
//! Two faces onto one register file: the firmware's at `SAMPLER_BASE` 0x10048000, and the C64's at `$DF20-$DFFF` while
//! `C64_SAMPLER_ENABLE` is set. The C64 window starts at voice 0 register 0 and ends at offset 0xDF, so the C64 reaches
//! voices 0-6 and **voice 7 is firmware-only** (`slot_server_v4.vhd:953-957`, `slot_to_io_bridge.vhd:65`).
//!
//! The model follows `sampler2.vhd` and `sampler_regs.vhd`, not the vendor manual, which is wrong about byte order
//! (it is big-endian) and about sample signedness (they are signed). The demo library's own header is wrong about
//! control bit 2: it is the interrupt enable, not a "restart" flag, which is exactly why `audio_detect()` works.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::Arc;

use trx64_core::expansion::{Access, ExpansionDevice, PortLines};

use crate::sid::AudioSink;

/// Where a voice's `start` of 0x0100_0000 points: the REU aperture in guest DDR (`reu::REU_BASE`). The hardware has no
/// bank concept — `start` is a flat 26-bit byte address into the same SDRAM (`slot_server_v4.vhd:18,1144-1160`).
const DDR_MASK: u32 = 0x03FF_FFFF;

/// Voices in the register file. The C64 sees the first seven (S16 §1).
const VOICES: usize = 8;

/// Bytes per voice (`sampler.h:8`).
const STRIDE: u16 = 0x20;

/// Every **odd** offset in the window reads this (`sampler_regs.vhd:84-85`); the firmware's `SAMPLER_VERSION` is just
/// the first of them. Hardware on 3.15 reports 16, which is what the demos display.
const VERSION: u8 = 0x10;

/// The C64 window (`$DF20-$DFFF`), offset 0 = voice 0 register 0.
const C64_WINDOW_START: u16 = 0xDF20;
const C64_WINDOW_END: u16 = 0xDFFF;

/// Control bits (`sampler_regs.vhd:91-102`).
const CTRL_ENABLE: u8 = 0x01;
const CTRL_REPEAT: u8 = 0x02;
const CTRL_IRQ: u8 = 0x04;
/// Mode field 5:4 — `00` is 8-bit, **any other value** 16-bit.
const CTRL_MODE: u8 = 0x30;
const CTRL_INTERLEAVE: u8 = 0x40;

/// The reference tick is 160 ns for every supported FPGA clock, so twice that resolution carries the half-tick a fetch
/// slot costs: 6.25 MHz x 2 (`sampler2.vhd:44-63`, S16 §2.6).
const HALF_TICK_HZ: u64 = 12_500_000;

/// Emulator clocks per half-tick: `time::CLOCK_HZ` 100 MHz / 12.5 MHz. The block is FPGA-clocked, so it hangs off the
/// emulator's clock and not the C64's.
const CLOCKS_PER_HALF_TICK: u64 = 8;

/// The rate the voices run at until [`Sampler::set_sample_rate`] says otherwise. The engine must turn whether or not
/// anyone is listening: a voice reaching its end sets a status bit the C64 polls, and `audio_detect()` waits for it on
/// a headless run with no audio device and no WAV.
const DEFAULT_RATE: u32 = 44_100;

/// Rendered samples kept for the sink. Without one they are produced and dropped, so the engine still runs.
const QUEUE_CAP: usize = 1 << 16;

/// Guest DDR as the voices' memory, lent per access the way [`super::reu::ReuRam`] is — the same lease, a second cell.
///
/// It is deliberately **not** the REU's store: the sampler reads the same SDRAM but is not gated by `C64_REU_SIZE`, so
/// a voice plays with no REU attached at all.
#[derive(Clone, Default)]
pub struct SamplerRam(Arc<Cell>);

#[derive(Default)]
struct Cell(UnsafeCell<Option<(*mut u8, usize)>>);

// SAFETY: as for `reu::ReuRam` — every handle lives on the emulation thread; `Send` is only required because the
// device this store belongs to is one.
unsafe impl Send for Cell {}
unsafe impl Sync for Cell {}

impl SamplerRam {
    /// Guest DDR for the accesses that follow, or `None` when `C64Port` takes it back.
    pub fn set_ddr(&self, ddr: Option<(*mut u8, usize)>) {
        // SAFETY: `Option<(*mut u8, usize)>` is `Copy` and is written whole; no reference is held across anything.
        unsafe { *self.0 .0.get() = ddr };
    }

    /// The DDR byte at 26-bit address `addr`. Outside a lease there is nothing to read: the hardware never stalls on
    /// memory (`sampler2.vhd:244-265`), so a fetch that finds nothing keeps silence rather than inventing a byte.
    fn byte(&self, addr: u32) -> u8 {
        // SAFETY: the pointer is `IoCtx::ram`, lent by `C64Port` for the access in progress.
        let lent = unsafe { *self.0 .0.get() };
        let Some((ptr, len)) = lent else { return 0 };
        let at = (addr & DDR_MASK) as usize;
        if at < len {
            // SAFETY: `at < len` keeps the offset inside the lease.
            unsafe { *ptr.add(at) }
        } else {
            0
        }
    }
}

/// Where a voice is in its life (`sampler2.vhd:126-170`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Run {
    #[default]
    Idle,
    Playing,
    /// The sample ended. The voice sits here until software clears the enable bit — it never clears it itself.
    Finished,
}

/// One voice: its registers and its engine.
#[derive(Clone, Copy)]
struct Voice {
    control: u8,
    /// 6 bits, power-up 0x20 = unity (`sampler_regs.vhd:53`).
    volume: u8,
    /// 4 bits, power-up 8 = centre (`:54`).
    pan: u8,
    /// 26 bits: offset 0x04 keeps only `data(1:0)` (`:111`).
    start: u32,
    length: u32,
    rate: u16,
    rep_a: u32,
    rep_b: u32,
    run: Run,
    position: u32,
    /// Half-ticks left of the current output sample's period.
    phase: u64,
    /// The last sample fetched, held as DC while the voice is stopped (`sampler2.vhd:134`).
    sample: i16,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            control: 0,
            volume: 0x20,
            pan: 0x8,
            start: 0,
            length: 0,
            rate: 0,
            rep_a: 0,
            rep_b: 0,
            run: Run::Idle,
            position: 0,
            phase: 0,
            sample: 0,
        }
    }
}

impl Voice {
    fn enabled(&self) -> bool {
        self.control & CTRL_ENABLE != 0
    }

    fn repeat(&self) -> bool {
        self.control & CTRL_REPEAT != 0
    }

    fn wants_irq(&self) -> bool {
        self.control & CTRL_IRQ != 0
    }

    fn mode16(&self) -> bool {
        self.control & CTRL_MODE != 0
    }

    /// Bytes `position` advances per sample (`sampler2.vhd:179,185,195`).
    fn step(&self) -> u32 {
        let interleave = self.control & CTRL_INTERLEAVE != 0;
        match (self.mode16(), interleave) {
            (false, false) => 1,
            (false, true) => 2,
            (true, false) => 2,
            (true, true) => 4,
        }
    }

    /// Half-ticks per output sample: `(rate + 1)` ticks plus the fetch slots, which are not prescaled — one slot for
    /// 8-bit, two for 16-bit, each half a tick at 100 MHz with eight voices (S16 §2.6).
    fn period(&self) -> u64 {
        2 * (u64::from(self.rate) + 1) + if self.mode16() { 2 } else { 1 }
    }
}

/// The block: the register file, the eight voices, the mixer and the C64 window.
pub struct Sampler {
    voices: [Voice; VOICES],
    /// One sticky latch per voice, set at end-of-sample when control bit 2 was on (`sampler2.vhd:74,151`).
    irq: u8,
    /// `C64_SAMPLER_ENABLE` (cart regs +0x0E): whether the C64 window answers and the IRQ reaches the port.
    enabled: bool,
    ram: SamplerRam,
    /// Half-ticks owed to the engine, 16.16 fixed point, from the audio sample rate.
    frac: u64,
    /// Half-ticks per output sample, 16.16.
    per_sample: u64,
    /// Emulator clock the voices have been advanced to.
    now: u64,
    /// Half-ticks elapsed but not yet turned into output samples.
    avail: u64,
    /// Samples waiting for the sink, oldest first.
    queue: VecDeque<i16>,
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut s = Sampler {
            voices: [Voice::default(); VOICES],
            irq: 0,
            enabled: false,
            ram: SamplerRam::default(),
            frac: 0,
            per_sample: 0,
            now: 0,
            avail: 0,
            queue: VecDeque::new(),
        };
        s.set_sample_rate(DEFAULT_RATE);
        s
    }

    /// The store to lend guest DDR through, shared with whatever holds the device.
    pub fn ram(&self) -> SamplerRam {
        self.ram.clone()
    }

    /// `C64_SAMPLER_ENABLE`. The firmware reads the bit back from its own latch; here it decides whether the C64
    /// window answers at all and whether the IRQ reaches the port (`slot_to_io_bridge.vhd:51,85-88`).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// The sample rate the mixed output is rendered at. Changing it drops what is queued: those samples were made
    /// for a different grid.
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.per_sample = if rate == 0 { 0 } else { (HALF_TICK_HZ << 16) / u64::from(rate) };
        self.frac = 0;
        self.queue.clear();
    }

    /// A C64 reset clears the IRQ latches and nothing else: the register file has no reset branch, which is why the
    /// firmware clears the voices in software on every reset (`sampler2.vhd:229-232`, `sampler_regs.vhd:73-167`).
    pub fn c64_reset(&mut self) {
        self.irq = 0;
    }

    /// A read of the register file. Only address bit 0 is decoded: **even** is the IRQ status vector, **odd** the
    /// version constant (`sampler_regs.vhd:81-87`). Side-effect free — reading never clears a latch.
    pub fn read(&self, off: u16) -> u8 {
        if off & 1 == 0 {
            self.irq
        } else {
            VERSION
        }
    }

    /// A write to the register file. `off` is the byte offset inside the 256-byte file; everything above bit 7 aliases
    /// (`sampler_regs.vhd:58,90`).
    pub fn write(&mut self, off: u16, val: u8) {
        let (v, reg) = ((off / STRIDE) as usize & (VOICES - 1), off % STRIDE);
        let byte = u32::from(val);
        let voice = &mut self.voices[v];
        match reg {
            0x00 => {
                let was = voice.enabled();
                voice.control = val;
                if voice.enabled() && !was {
                    // idle -> start: position 0, divider reloaded, the held sample dropped (`sampler2.vhd:129-135`).
                    voice.position = 0;
                    voice.phase = 0;
                    voice.sample = 0;
                    voice.run = Run::Playing;
                } else if !voice.enabled() && !voice.repeat() {
                    // Stopping from `playing` needs both bits clear (`sampler2.vhd:163-165`); `finished` needs only
                    // the enable (`:167-170`).
                    voice.run = Run::Idle;
                } else if !voice.enabled() && voice.run == Run::Finished {
                    voice.run = Run::Idle;
                }
            }
            0x01 => voice.volume = val & 0x3F,
            0x02 => voice.pan = val & 0x0F,
            // Big-endian, MSB at the lowest offset. Offset 0x04 keeps two bits: the address is 26 bits, not 32.
            0x04 => voice.start = (voice.start & 0x00FF_FFFF) | ((byte & 0x03) << 24),
            0x05 => voice.start = (voice.start & 0xFF00_FFFF) | (byte << 16),
            0x06 => voice.start = (voice.start & 0xFFFF_00FF) | (byte << 8),
            0x07 => voice.start = (voice.start & 0xFFFF_FF00) | byte,
            0x09 => voice.length = (voice.length & 0x0000_FFFF) | (byte << 16),
            0x0A => voice.length = (voice.length & 0x00FF_00FF) | (byte << 8),
            0x0B => voice.length = (voice.length & 0x00FF_FF00) | byte,
            0x0E => voice.rate = (voice.rate & 0x00FF) | ((val as u16) << 8),
            0x0F => voice.rate = (voice.rate & 0xFF00) | u16::from(val),
            0x11 => voice.rep_a = (voice.rep_a & 0x0000_FFFF) | (byte << 16),
            0x12 => voice.rep_a = (voice.rep_a & 0x00FF_00FF) | (byte << 8),
            0x13 => voice.rep_a = (voice.rep_a & 0x00FF_FF00) | byte,
            0x15 => voice.rep_b = (voice.rep_b & 0x0000_FFFF) | (byte << 16),
            0x16 => voice.rep_b = (voice.rep_b & 0x00FF_00FF) | (byte << 8),
            0x17 => voice.rep_b = (voice.rep_b & 0x00FF_FF00) | byte,
            // Bit 0 clears this voice's latch; 0xFF clears every voice (`sampler_regs.vhd:155-159`).
            0x1F => {
                if val == 0xFF {
                    self.irq = 0;
                } else if val & 1 != 0 {
                    self.irq &= !(1 << v);
                }
            }
            // 0x03, 0x08, 0x0C, 0x0D, 0x10, 0x14, 0x18-0x1E: `when others => null`. The holes are where a 32-bit
            // write's unused MSB lands.
            _ => {}
        }
    }

    /// Whether the block drives the cartridge IRQ line: the OR of the latches, gated by the enable
    /// (`sampler2.vhd:101`, `slot_to_io_bridge.vhd:51`). Never NMI.
    pub fn irq(&self) -> bool {
        self.enabled && self.irq != 0
    }

    /// Run the voices up to emulator clock `now`, queueing what they produce.
    ///
    /// This hangs off emulated time, not off the audio sink, and that is the whole point: the status bit a finished
    /// voice sets is what `audio_detect()` polls, so the engine has to turn on a headless run with no audio at all.
    /// Samples nobody drains fall off the back of the queue.
    pub fn advance_to(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.now);
        self.now = now;
        if self.per_sample == 0 {
            return;
        }
        self.avail += elapsed / CLOCKS_PER_HALF_TICK;
        let ram = self.ram.clone();
        loop {
            let next = self.frac + self.per_sample;
            let want = next >> 16;
            if want > self.avail {
                break;
            }
            self.frac = next & 0xFFFF;
            self.avail -= want;
            let sample = self.mix(&ram, want);
            if self.queue.len() == QUEUE_CAP {
                self.queue.pop_front();
            }
            self.queue.push_back(sample);
        }
    }

    /// Take `n` queued samples for the sink, padding with silence if the voices are behind.
    pub fn drain(&mut self, out: &mut Vec<i16>, n: usize) {
        for _ in 0..n {
            out.push(self.queue.pop_front().unwrap_or(0));
        }
    }

    /// One output sample: every voice advanced by `halves` half-ticks, time-averaged over the interval, then through
    /// the hardware's volume, pan and saturating sum (`sampler_accu.vhd:54-84`).
    fn mix(&mut self, ram: &SamplerRam, halves: u64) -> i16 {
        let (mut left, mut right) = (0i64, 0i64);
        for v in 0..VOICES {
            let avg = Self::advance(&mut self.voices[v], &mut self.irq, ram, v, halves);
            let voice = &self.voices[v];
            // (sample * volume) >> 5: volume is unsigned, so 32 is unity and 63 is about x1.97.
            let scaled = (avg * i64::from(voice.volume)) >> 5;
            let (fl, fr) = pan_factors(voice.pan);
            left = sat21(left + scaled * i64::from(fl));
            right = sat21(right + scaled * i64::from(fr));
        }
        // accu >> 3 gives the hardware's 18-bit pair; the sink is mono, so the pair is downmixed and scaled to i16.
        let mono = ((left >> 3) + (right >> 3)) / 2;
        (mono >> 2).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
    }

    /// Advance one voice by `halves` half-ticks and return its time-averaged sample over that interval. The average is
    /// the box filter that stands in for the hardware's 6.25 MHz output stage feeding a 48 kHz sink.
    fn advance(voice: &mut Voice, irq: &mut u8, ram: &SamplerRam, index: usize, halves: u64) -> i64 {
        if halves == 0 {
            return i64::from(voice.sample);
        }
        if voice.run != Run::Playing {
            // A stopped or finished voice holds its last sample as DC and keeps being summed in.
            return i64::from(voice.sample);
        }
        let (mut left, mut acc) = (halves, 0i64);
        let period = voice.period();
        while left > 0 {
            let due = period.saturating_sub(voice.phase).max(1);
            let held = due.min(left);
            acc += i64::from(voice.sample) * held as i64;
            voice.phase += held;
            left -= held;
            if voice.phase < period {
                continue;
            }
            voice.phase -= period;
            // repeat B is tested first, and the length test is skipped on that tick (`sampler2.vhd:145-153`).
            if voice.position == voice.rep_b {
                if voice.enabled() && voice.repeat() {
                    voice.position = voice.rep_a;
                }
            } else if voice.position == voice.length {
                voice.run = Run::Finished;
                if voice.wants_irq() {
                    *irq |= 1 << index;
                }
                // The held sample stays on the output; nothing more is fetched.
                acc += i64::from(voice.sample) * left as i64;
                return acc / halves as i64;
            }
            voice.sample = fetch(voice, ram);
            voice.position = voice.position.wrapping_add(voice.step()) & 0x00FF_FFFF;
        }
        acc / halves as i64
    }
}

/// The sample at the voice's position. Bytes are **signed**: an 8-bit sample lands in the high half, so its value is
/// `byte * 256`; 16 bits are little-endian, low byte first (`sampler2.vhd:175-198,221-225`).
fn fetch(voice: &Voice, ram: &SamplerRam) -> i16 {
    let at = voice.start.wrapping_add(voice.position);
    if voice.mode16() {
        let lo = ram.byte(at);
        let hi = ram.byte(at.wrapping_add(1));
        i16::from_le_bytes([lo, hi])
    } else {
        i16::from(ram.byte(at) as i8) * 256
    }
}

/// The pan law (`sampler_accu.vhd:65-71`): 0x0 hard left, 0x7 and 0x8 both centre, 0xF hard right. Centre is **not**
/// normalised — a centred voice is twice as loud per side as one side of a hard-panned pair.
fn pan_factors(pan: u8) -> (u8, u8) {
    if pan & 0x8 == 0 {
        (7, pan & 0x07)
    } else {
        (!pan & 0x07, 7)
    }
}

/// The accumulator saturates at +/- 2^20; it does not wrap (`my_math_pkg.vhd:20-35`).
fn sat21(v: i64) -> i64 {
    v.clamp(-(1 << 20), (1 << 20) - 1)
}

/// A handle on the block. The bridge keeps one and serves the firmware window through it; TRX64 holds another as the
/// port device, the way `CartHandle` carries the cartridge logic into TRX64's slot.
#[derive(Clone, Default)]
pub struct SamplerHandle(Arc<SharedSampler>);

#[derive(Default)]
struct SharedSampler(UnsafeCell<Sampler>);

// SAFETY: as for `cart::SharedCell` — every copy lives on the emulation thread; `Send` is only required because
// `ExpansionDevice: Send`.
unsafe impl Send for SharedSampler {}
unsafe impl Sync for SharedSampler {}

impl SamplerHandle {
    /// Run `f` on the block. Calls do not nest: the bridge never holds one across a call into TRX64, and the port
    /// device holds one only inside a single bus access.
    pub fn with<R>(&self, f: impl FnOnce(&mut Sampler) -> R) -> R {
        // SAFETY: single thread, and no two borrows are live at once (see above).
        f(unsafe { &mut *self.0 .0.get() })
    }
}

/// The C64 face: `$DF20-$DFFF` while `C64_SAMPLER_ENABLE` is set. With the enable clear the device answers nothing, so
/// whatever else claims the range does — on hardware that is the open bus.
impl ExpansionDevice for SamplerHandle {
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        self.with(|s| s.window(a.addr).map(|off| s.read(off)))
    }

    fn peek(&self, addr: u16, _cart: Option<u8>) -> Option<u8> {
        self.with(|s| s.window(addr).map(|off| s.read(off)))
    }

    fn write(&mut self, a: Access, value: u8) {
        self.with(|s| {
            if let Some(off) = s.window(a.addr) {
                s.write(off, value);
            }
        });
    }

    fn lines(&self) -> PortLines {
        PortLines { irq: self.with(|s| s.irq()), ..PortLines::default() }
    }
}

/// The sink `Sid` pushes into once the sampler exists: reSID's block arrives, the voices render the same number of
/// samples for the same interval, and the sum goes on to the real sink.
///
/// Mixing here rather than inside `Sid::catch_up` is deliberate (S16 §3.4): that function turns cycles into samples
/// for the whole machine — impact rates it CRITICAL, seven processes deep — so reSID keeps owning the clock and the
/// only thing that changes is what the sink is.
pub struct SamplerMix {
    sampler: SamplerHandle,
    sink: Box<dyn AudioSink>,
    voices: Vec<i16>,
    mixed: Vec<i16>,
}

impl SamplerMix {
    pub fn new(sampler: SamplerHandle, sink: Box<dyn AudioSink>) -> Self {
        SamplerMix { sampler, sink, voices: Vec::new(), mixed: Vec::new() }
    }
}

impl AudioSink for SamplerMix {
    fn samples(&mut self, pcm: &[i16]) {
        self.voices.clear();
        self.sampler.with(|s| s.drain(&mut self.voices, pcm.len()));
        self.mixed.clear();
        self.mixed.extend(pcm.iter().zip(&self.voices).map(|(&sid, &voice)| sid.saturating_add(voice)));
        self.sink.samples(&self.mixed);
    }
}

impl Sampler {
    /// The register offset a C64 address names, or None when the window is closed or the address is elsewhere.
    fn window(&self, addr: u16) -> Option<u16> {
        (self.enabled && (C64_WINDOW_START..=C64_WINDOW_END).contains(&addr)).then(|| addr - C64_WINDOW_START)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emulator clocks one output sample covers at 48 kHz.
    const CLOCKS_PER_SAMPLE: u64 = 100_000_000 / 48_000;

    /// Advance the voices by `samples` output samples' worth of emulated time.
    fn run(s: &mut Sampler, now: &mut u64, samples: u64) {
        *now += samples * CLOCKS_PER_SAMPLE;
        s.advance_to(*now);
    }

    /// Guest DDR with a voice's sample data at `at`.
    fn ddr_with(at: u32, data: &[u8]) -> Vec<u8> {
        let mut ddr = vec![0u8; 0x0200_0000];
        ddr[at as usize..at as usize + data.len()].copy_from_slice(data);
        ddr
    }

    fn lend(s: &Sampler, ddr: &mut [u8]) {
        s.ram.set_ddr(Some((ddr.as_mut_ptr(), ddr.len())));
    }

    /// Program a voice the way a C64 client does: big-endian, MSB first.
    fn program(s: &mut Sampler, v: u16, start: u32, length: u32, rate: u16, control: u8) {
        let base = v * STRIDE;
        for (off, byte) in [(0x04, start >> 24), (0x05, start >> 16), (0x06, start >> 8), (0x07, start)] {
            s.write(base + off, byte as u8);
        }
        for (off, byte) in [(0x09, length >> 16), (0x0A, length >> 8), (0x0B, length)] {
            s.write(base + off, byte as u8);
        }
        s.write(base + 0x0E, (rate >> 8) as u8);
        s.write(base + 0x0F, rate as u8);
        s.write(base + 0x00, control);
    }

    #[test]
    fn power_up_values_and_the_two_read_decodes() {
        let s = Sampler::new();
        assert_eq!(s.voices[0].volume, 0x20, "unity, not 63: `c_voice_control_init` is dead code");
        assert_eq!(s.voices[0].pan, 0x8, "centre");
        assert_eq!(s.read(0x00), 0, "even: the IRQ status vector");
        assert_eq!(s.read(0x01), VERSION, "odd: the version constant");
        assert_eq!(s.read(0xE0), 0, "every even offset is the same vector");
        assert_eq!(s.read(0x0D), VERSION, "and every odd one the same constant");
    }

    #[test]
    fn multi_byte_registers_are_big_endian() {
        // The testbench writes 0x01/0x23/0x45/0x00 and the DUT fetches from 0x1234500 (`sampler_tb.vhd:81-84`).
        let mut s = Sampler::new();
        for (off, val) in [(0x04, 0x01), (0x05, 0x23), (0x06, 0x45), (0x07, 0x00)] {
            s.write(off, val);
        }
        assert_eq!(s.voices[0].start, 0x0123_4500);
        s.write(0x04, 0xFF);
        assert_eq!(s.voices[0].start, 0x0323_4500, "offset 0x04 keeps two bits: the address is 26 bits");
        for (off, val) in [(0x09, 0x12), (0x0A, 0x34), (0x0B, 0x56)] {
            s.write(off, val);
        }
        assert_eq!(s.voices[0].length, 0x0012_3456);
        s.write(0x0E, 0x01);
        s.write(0x0F, 0x18);
        assert_eq!(s.voices[0].rate, 0x0118);
    }

    #[test]
    fn the_write_holes_are_discarded() {
        let mut s = Sampler::new();
        let before = (s.voices[0].start, s.voices[0].length, s.voices[0].rate);
        for off in [0x03, 0x08, 0x0C, 0x0D, 0x10, 0x14, 0x18, 0x1E] {
            s.write(off, 0xFF);
        }
        assert_eq!((s.voices[0].start, s.voices[0].length, s.voices[0].rate), before);
    }

    #[test]
    fn a_voice_plays_once_latches_the_irq_and_holds_its_last_sample() {
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[0x40; 4]);
        lend(&s, &mut ddr);
        // Four 8-bit samples at rate 1, interrupt enabled.
        program(&mut s, 0, 0x0100_0000, 4, 1, CTRL_ENABLE | CTRL_IRQ);
        let mut now = 0;
        run(&mut s, &mut now, 4);
        assert_eq!(s.read(0x00) & 1, 1, "the end-of-sample latch is set");
        assert_eq!(s.voices[0].run, Run::Finished);
        assert!(s.voices[0].enabled(), "and the enable bit does not clear itself");
        // Reading does not clear it; only 0x1F does.
        assert_eq!(s.read(0x00) & 1, 1);
        s.write(0x1F, 0xFF);
        assert_eq!(s.read(0x00), 0);
    }

    #[test]
    fn repeat_jumps_from_b_to_a_and_never_ends() {
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[1, 2, 3, 4, 5, 6, 7, 8]);
        lend(&s, &mut ddr);
        s.write(0x13, 2); // repeat A = 2
        s.write(0x17, 4); // repeat B = 4
        program(&mut s, 0, 0x0100_0000, 8, 1, CTRL_ENABLE | CTRL_REPEAT | CTRL_IRQ);
        let mut now = 0;
        run(&mut s, &mut now, 64);
        assert_eq!(s.read(0x00), 0, "a looping voice never reaches its length");
        assert_eq!(s.voices[0].run, Run::Playing);
        assert!(s.voices[0].position >= 2 && s.voices[0].position <= 4, "{}", s.voices[0].position);
    }

    #[test]
    fn a_misaligned_length_is_never_hit() {
        // 16-bit steps by two, so an odd length is stepped over and the voice runs on — hardware behaviour
        // (`sampler2.vhd:149`, equality not >=).
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[0; 64]);
        lend(&s, &mut ddr);
        program(&mut s, 0, 0x0100_0000, 7, 1, CTRL_ENABLE | CTRL_IRQ | 0x10);
        let mut now = 0;
        run(&mut s, &mut now, 32);
        assert_eq!(s.read(0x00), 0, "no end, no latch");
        assert_eq!(s.voices[0].run, Run::Playing);
    }

    #[test]
    fn stopping_needs_both_bits_while_playing() {
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[0; 16]);
        lend(&s, &mut ddr);
        program(&mut s, 0, 0x0100_0000, 16, 100, CTRL_ENABLE | CTRL_REPEAT);
        s.write(0x00, CTRL_REPEAT);
        assert_eq!(s.voices[0].run, Run::Playing, "clearing enable alone leaves it playing");
        s.write(0x00, 0);
        assert_eq!(s.voices[0].run, Run::Idle);
    }

    #[test]
    fn the_pan_law_and_the_volume_shift() {
        assert_eq!(pan_factors(0x0), (7, 0), "hard left");
        assert_eq!(pan_factors(0x7), (7, 7), "centre");
        assert_eq!(pan_factors(0x8), (7, 7), "centre, the other code");
        assert_eq!(pan_factors(0xF), (0, 7), "hard right");
        assert_eq!(sat21((1 << 20) + 5), (1 << 20) - 1, "the accumulator saturates, it does not wrap");
        assert_eq!(sat21(-(1 << 21)), -(1 << 20));
    }

    #[test]
    fn the_c64_window_reaches_voices_0_to_6_only_while_enabled() {
        let mut s = Sampler::new();
        assert_eq!(s.window(0xDF20), None, "closed until C64_SAMPLER_ENABLE");
        s.set_enabled(true);
        assert_eq!(s.window(0xDF20), Some(0x00), "voice 0 control");
        assert_eq!(s.window(0xDF21), Some(0x01), "the version byte the demos read");
        assert_eq!(s.window(0xDFE0), Some(0xC0), "voice 6 control");
        assert_eq!(s.window(0xDFFF), Some(0xDF), "the last byte of voice 6");
        assert_eq!(s.window(0xDF1F), None, "below the window: UCI's, not ours");
        assert_eq!(s.window(0xDE20), None, "IO1 is not ours");
        // Voice 7 begins at offset 0xE0, which would be $E000 — off the end of the window.
        assert_eq!(0xDF20u16 + 0xE0, 0xE000);
    }

    /// The routine both demos stand behind (`audio.c:111-189`, S16 §2.10), replayed exactly.
    #[test]
    fn audio_detect_finds_the_block() {
        let mut s = Sampler::new();
        s.set_enabled(true);
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[0x20; 256]);
        lend(&s, &mut ddr);
        let read = |s: &Sampler| s.read(0x00);

        // 1. stop all seven reachable voices, ack voice 0.
        for v in 0..7u16 {
            s.write(v * STRIDE, 0);
        }
        s.write(0x1F, 0xFF);
        // 2. 256 reads, all zero.
        for _ in 0..256 {
            assert_eq!(read(&s), 0x00, "a stale latch would fail the probe here");
        }
        // 3. volume 0, start $01000000, length 256, rate 1, control $05 = enable + interrupt.
        s.write(0x01, 0);
        program(&mut s, 0, 0x0100_0000, 256, 1, CTRL_ENABLE | CTRL_IRQ);
        // 4. up to 128 reads waiting for non-zero; the sample lasts ~102 us, so a few output samples suffice.
        let mut now = 0;
        let mut waited = 0;
        while read(&s) == 0 && waited < 128 {
            run(&mut s, &mut now, 1);
            waited += 1;
        }
        assert!(waited < 128, "the latch must come up inside the budget, took {waited}");
        // 5. exactly $01 for the rest, reading never clears it.
        for _ in 0..256 {
            assert_eq!(read(&s), 0x01);
        }
        // 6. ack.
        s.write(0x1F, 0xFF);
        assert_eq!(read(&s), 0x00);
        assert_eq!(s.read(0x01), 16, "and the version the demos display is 16");
    }

    /// The regression this rework exists for: headless, no audio device and no WAV, so nothing ever drains the
    /// voices — and they must still reach the end of a sample, because the C64 polls the status bit.
    #[test]
    fn the_voices_run_with_nobody_listening() {
        let mut s = Sampler::new();
        let mut ddr = ddr_with(0x0100_0000, &[0x30; 8]);
        lend(&s, &mut ddr);
        program(&mut s, 0, 0x0100_0000, 8, 1, CTRL_ENABLE | CTRL_IRQ);
        let mut now = 0;
        // No `set_sample_rate` call at all: the default rate is what a run without a sink gets.
        run(&mut s, &mut now, 8);
        assert_eq!(s.read(0x00) & 1, 1, "the voice reached its end without a sink");
    }

    #[test]
    fn the_irq_line_follows_the_latch_and_the_enable() {
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        let mut ddr = ddr_with(0x0100_0000, &[0x10; 2]);
        lend(&s, &mut ddr);
        program(&mut s, 0, 0x0100_0000, 2, 1, CTRL_ENABLE | CTRL_IRQ);
        let mut now = 0;
        run(&mut s, &mut now, 4);
        assert!(!s.irq(), "the latch is set, but the window is closed");
        s.set_enabled(true);
        assert!(s.irq());
        let handle = SamplerHandle::default();
        handle.with(|h| *h = Sampler { irq: 1, enabled: true, ..Sampler::new() });
        assert_eq!(ExpansionDevice::lines(&handle), PortLines { irq: true, nmi: false, hold: false }, "IRQ, never NMI");
        s.c64_reset();
        assert!(!s.irq(), "a C64 reset clears the latches");
        assert_eq!(s.voices[0].length, 2, "but not the register file");
    }

    #[test]
    fn without_a_lease_the_voices_are_silent_but_keep_running() {
        let mut s = Sampler::new();
        s.set_sample_rate(48_000);
        program(&mut s, 0, 0x0100_0000, 8, 1, CTRL_ENABLE | CTRL_IRQ);
        let (mut now, mut out) = (0, Vec::new());
        run(&mut s, &mut now, 8);
        s.drain(&mut out, 8);
        assert!(out.iter().all(|&v| v == 0), "nothing lent, nothing to play");
        assert_eq!(s.read(0x00) & 1, 1, "the voice still reached its end");
    }
}
