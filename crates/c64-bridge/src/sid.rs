//! SID socket 1 and UltiSID 1 on TRX64's reSID: the firmware's SID decode, an ARMSID identity in socket 1, and the
//! sample stream. Spec: docs/specs/S14-c64-trx64.md §W4-SID; status: docs/status/sid-audio.md.
//!
//! - **Decode.** A SID answers an I/O address when `((A11..A4) & MASK) == BASE` (docs/hw/10 §SID addressing), from
//!   the C64 core config latches the firmware writes (system/u64.h:110-130; u64_config.cc:857-888, 2303-2319). Socket
//!   1 also needs C64_SID1_EN (u64_config.cc:817-818) and a fitted chip (`--sid-socket1 armsid`, [`Sid::set_socket1`]).
//! - **One engine.** TRX64 links one reSID (resid_shim.cc `g_sid`, resid_ffi.rs `RESID_GUARD`), so socket 1 and
//!   UltiSID 1 share it: a write either decodes goes to it. Socket 2 and UltiSID 2 are not modelled.
//! - **ARMSID.** Socket 1 answers the firmware's ARMSID probe and configuration protocol (u64_config.cc:595-637,
//!   sid_device_armsid.cc). Its "Fundamental Mode" picks the reSID model; without socket 1, C64_EMUSID1_WAVES does
//!   (u64_config.cc:1651-1656).
//! - **Timing.** CPU writes come with their address and cycle from a TRX64 [`Observer`] ([`SidTap`]), confirmed by the
//!   `Sid6581` write hook that fires only when the write really reached the SID (I/O mapped, full.rs:554-559).
//!   With an [`AudioSink`] reSID is clocked to each write's cycle before it is applied and to every `advance_to`, and
//!   its samples go to the sink. Without one, CPU writes only set registers and a DMA write or read clocks the gap.

use std::cell::{Cell, RefCell};

use trx64_core::resid_ffi::{MODEL_6581, MODEL_8580};
use trx64_core::sid::Sid6581;
use trx64_core::{BusKind, Observer, Resid, ResidConfig};

/// The mono sample stream of the emulated SID, in emulated-time order at the rate given to [`Sid::set_audio`].
pub trait AudioSink {
    fn samples(&mut self, pcm: &[i16]);
}

/// Core config offsets (0x10180000 + off, u64.h:110-130).
const SID1_BASE: u8 = 0x08;
const EMUSID2_MASK: u8 = 0x0F;
const SID1_EN: u8 = 0x11;
const SID2_EN: u8 = 0x12;
const EMUSID1_WAVES: u8 = 0x20;
const EMUSID_SPLIT: u8 = 0x29;

/// Decoder index: C64_SID1_BASE + i and C64_SID1_MASK + i (u64.h:110-117).
const SOCKET1: usize = 0;
const ULTISID1: usize = 2;

/// `base & UNMAPPED` marks "Unmapped" (u64_sid_offsets[0], u64_config.cc:228; mask 0xFE never matches bit 0).
const UNMAPPED: u8 = 0x01;
/// C64_EMUSID_SPLIT → address bits of A11..A4 that pick UltiSID 1 (all clear) or 2 (u64_config.cc:266, 320).
const SPLIT_BITS: [u8; 8] = [0x00, 0x02, 0x04, 0x08, 0x10, 0x06, 0x12, 0x18];

/// Without an audio sink, a gap longer than this is clocked as this many cycles (1 s): the chip has settled, and a
/// DMA read after minutes of silence does not clock minutes of reSID.
const SILENT_CATCH_UP: u64 = 985_248;
/// Largest cycle count handed to reSID at once (its `clock` takes an `int`).
const CHUNK: u64 = 1 << 20;

thread_local! {
    /// Set by TRX64's `Sid6581` write hook; [`SidTap`] takes it with the matching bus write. Thread-local, so the tap
    /// is zero-sized like TRX64's `NullSink` (a sized observer cost 4 % host MIPS) and the hook shares nothing; a
    /// machine runs on one thread.
    static HIT: Cell<bool> = const { Cell::new(false) };
    /// CPU writes of the current run: (cycle, address, value).
    static WRITES: RefCell<Vec<(u64, u16, u8)>> = const { RefCell::new(Vec::new()) };
}

/// ARMSID "VI" answer: firmware version, shown as "%d.%d" (readParams, sid_device_armsid.cc:105-175). Any value serves.
const ARMSID_VERSION: [u8; 2] = [3, 0];
/// ARMSID "UI" answer: supply voltage in mV, big endian (readParams). A socket not holding a 6581 gets the 9 V
/// regulator setting (effectuate_settings, u64_config.cc:753-790).
const ARMSID_MILLIVOLTS: u16 = 9000;

/// The SID decode latches (docs/hw/10 §SID addressing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decode {
    /// SID1, SID2, EMUSID1, EMUSID2 BASE and MASK.
    base: [u8; 4],
    mask: [u8; 4],
    socket_en: [bool; 2],
    /// C64_EMUSID1_WAVES: 0 = 6581, 1 = 8580.
    ultisid1_waves: u8,
    /// C64_EMUSID_SPLIT index.
    split: u8,
}

impl Default for Decode {
    /// The firmware's default map until it writes the latches: sockets disabled, UltiSID 1 and 2 at $D400 mirrored
    /// through $D7FF (boot log "Resulting address map: Slot1: 40/C0 (Disabled) … Emu1: 40/C0", u64_config.cc:881-886).
    fn default() -> Self {
        Decode { base: [0x40; 4], mask: [0xC0; 4], socket_en: [false; 2], ultisid1_waves: 0, split: 0 }
    }
}

impl Decode {
    fn write(&mut self, off: u8, val: u8) {
        match off {
            SID1_BASE..=EMUSID2_MASK if off < SID1_BASE + 4 => self.base[usize::from(off - SID1_BASE)] = val,
            SID1_BASE..=EMUSID2_MASK => self.mask[usize::from(off - SID1_BASE - 4)] = val,
            SID1_EN | SID2_EN => self.socket_en[usize::from(off - SID1_EN)] = val & 1 != 0,
            EMUSID1_WAVES => self.ultisid1_waves = val & 1,
            EMUSID_SPLIT => self.split = val & 7,
            _ => {}
        }
    }

    /// Whether decoder `dev` answers C64 I/O address `addr`. Only $D400-$D7FF and $DE00-$DFFF carry SIDs
    /// (u64_sid_base, u64_config.cc:218-226). Socket 1 is mono, so C64_STEREO_ADDRSEL (its "B" half) is ignored.
    fn hits(&self, dev: usize, addr: u16) -> bool {
        if !Sid::window(addr) || (dev < 2 && !self.socket_en[dev]) {
            return false;
        }
        let a = (addr >> 4) as u8;
        a & self.mask[dev] == self.base[dev] && (dev != ULTISID1 || a & SPLIT_BITS[usize::from(self.split)] == 0)
    }

    /// Socket 1 is enabled and mapped somewhere.
    fn socket1_live(&self) -> bool {
        self.socket_en[SOCKET1] && self.base[SOCKET1] & UNMAPPED == 0
    }
}

/// The ARMSID configuration interface on registers $1D-$1F, as the firmware drives it.
///
/// "SID" into $1D/$1E/$1F enters configuration mode (u64_config.cc:599-604); any other $1D exits it
/// (u64_config.cc:629; sid_device_armsid.cc:222-229). In configuration mode a write to $1E or $1F runs the command
/// pair ($1F, $1E):
/// - `($1F, 'I')` answers on $1B/$1C: 'D' → "NO" (the ARMSID reply, u64_config.cc:621), 'I' → not 'L'/'R' (ARMSID,
///   not ARM2SID, u64_config.cc:622-636), 'V' version, 'F' '6'/'8' mode, 'U' voltage, 'H' filter nibbles
///   (readParams, sid_device_armsid.cc:105-175);
/// - `($1F, 'E')` sets: '6'/'8' the mode (S_set_mode, sid_device_armsid.cc:241-250), $8n/$9n/$An/$Bn the four filter
///   nibbles (S_set_filt, 252-278), $C0/$CF save to RAM/flash (280-295, nothing to save here).
#[derive(Clone, Debug)]
struct ArmSid {
    /// Last values written to $1D, $1E, $1F.
    cfg: [u8; 3],
    config: bool,
    answer: [u8; 2],
    model: i32,
    /// 6581 strength, 6581 lowest, 8580 highest, 8580 lowest: signed nibbles around the firmware defaults
    /// (readParams and S_set_filt, sid_device_armsid.cc:105-175, 252-278).
    filter: [u8; 4],
}

impl Default for ArmSid {
    fn default() -> Self {
        ArmSid { cfg: [0; 3], config: false, answer: [0; 2], model: MODEL_6581, filter: [0; 4] }
    }
}

impl ArmSid {
    /// A write to register `reg` (0x00-0x1F). True when it changed the mode.
    fn write(&mut self, reg: u8, val: u8) -> bool {
        if !(0x1D..=0x1F).contains(&reg) {
            return false;
        }
        self.cfg[usize::from(reg - 0x1D)] = val;
        if reg == 0x1D {
            if val != b'S' {
                self.config = false;
            }
            return false;
        }
        if &self.cfg == b"SID" {
            self.config = true;
        }
        self.config && self.command(self.cfg[2], self.cfg[1])
    }

    /// True when the command changed the mode.
    fn command(&mut self, cmd: u8, sub: u8) -> bool {
        match (cmd, sub) {
            (b'D' | b'I', b'I') => self.answer = *b"NO",
            (b'V', b'I') => self.answer = ARMSID_VERSION,
            (b'F', b'I') => self.answer = [if self.model == MODEL_8580 { b'8' } else { b'6' }, 0],
            (b'U', b'I') => self.answer = ARMSID_MILLIVOLTS.to_be_bytes(),
            (b'H', b'I') => {
                let f = self.filter;
                self.answer = [f[0] << 4 | f[1], f[2] << 4 | f[3]];
            }
            (b'6' | b'8', b'E') => {
                let model = if cmd == b'8' { MODEL_8580 } else { MODEL_6581 };
                let changed = model != self.model;
                self.model = model;
                return changed;
            }
            (0x80..=0xBF, b'E') => self.filter[usize::from((cmd >> 4) - 8)] = cmd & 0x0F,
            _ => {}
        }
        false
    }

    /// The configuration answer on $1B/$1C, None outside configuration mode.
    fn read(&self, reg: u8) -> Option<u8> {
        match reg {
            0x1B | 0x1C if self.config => Some(self.answer[usize::from(reg - 0x1B)]),
            _ => None,
        }
    }
}

/// Socket 1 (ARMSID) and UltiSID 1 on one reSID engine.
pub struct Sid {
    /// Built on first use (it takes TRX64's process-wide reSID guard) and rebuilt on a model or rate change.
    resid: Option<Resid>,
    model: i32,
    sample_rate: u32,
    /// C64 cycle reSID has been clocked to.
    clk: u64,
    /// Last values written to registers $00-$18, replayed into a rebuilt engine.
    regs: [u8; 0x19],
    decode: Decode,
    /// Socket 1 holds the ARMSID; else it is empty and never answers, whatever C64_SID1_EN says.
    socket1: bool,
    armsid: ArmSid,
    audio: Option<Box<dyn AudioSink>>,
}

impl Sid {
    /// Unused rate while no sink is attached (reSID still needs one to be configured).
    const SILENT_RATE: u32 = 44_100;

    pub fn new() -> Self {
        Sid {
            resid: None,
            model: MODEL_6581,
            sample_rate: Self::SILENT_RATE,
            clk: 0,
            regs: [0; 0x19],
            decode: Decode::default(),
            socket1: false,
            armsid: ArmSid::default(),
            audio: None,
        }
    }

    /// Subscribe to TRX64's SID write hook, which is kept across its resets (sid.rs `reset`).
    pub fn install_hook(&self, sid: &mut Sid6581) {
        sid.set_write_trace(Some(Box::new(|_, _| HIT.with(|hit| hit.set(true)))));
    }

    /// Fit (`true`) or empty socket 1. The firmware detects a fitted ARMSID at its next boot.
    pub fn set_socket1(&mut self, fitted: bool) {
        self.socket1 = fitted;
        self.apply_model();
    }

    /// Whether socket 1 is fitted and decodes `addr`.
    fn socket1_hits(&self, addr: u16) -> bool {
        self.socket1 && self.decode.hits(SOCKET1, addr)
    }

    /// Send samples at `sample_rate` Hz to `sink` from cycle `clk` on. Without a sink reSID lags the C64, so it first
    /// catches up silently: the stream starts at `clk`, not with the backlog.
    pub fn set_audio(&mut self, sample_rate: u32, sink: Box<dyn AudioSink>, clk: u64) {
        if self.resid.is_some() {
            self.catch_up(clk);
        }
        self.clk = self.clk.max(clk);
        if sample_rate != self.sample_rate {
            self.sample_rate = sample_rate;
            self.resid = None;
        }
        self.audio = Some(sink);
    }

    /// The observer for a CPU run.
    pub fn tap(&self) -> SidTap {
        SidTap
    }

    /// Apply the CPU writes of the last run, then follow the C64 to cycle `clk` when a sink listens.
    ///
    /// Without a sink the writes only set registers: nothing hears their timing, C64 programs read TRX64's own SID
    /// (TRX64 API gap), and the next DMA access clocks the gap. A program that keeps writing the SID (the placeholder
    /// KERNAL's jingle, default_kernal.tas:98-135) then costs no reSID clocking.
    pub fn advance(&mut self, clk: u64) {
        let timed = self.audio.is_some();
        WRITES.with(|writes| {
            let mut writes = writes.borrow_mut();
            for &(at, addr, val) in writes.iter() {
                self.write_at(addr, val, timed.then_some(at));
            }
            writes.clear();
        });
        if timed {
            self.catch_up(clk);
        }
    }

    /// A C64 core config latch changed (0x10180000 + `off`).
    pub fn core_config(&mut self, off: u8, val: u8) {
        self.decode.write(off, val);
        self.apply_model();
    }

    /// Forget a hook hit from a write that did not come from the CPU run (a DMA write).
    pub fn clear_hit(&self) {
        HIT.with(|hit| hit.set(false));
    }

    /// Whether `addr` is in a range a SID can be mapped to.
    pub fn window(addr: u16) -> bool {
        matches!(addr, 0xD400..=0xD7FF | 0xDE00..=0xDFFF)
    }

    /// A bus write at cycle `clk` (I/O mapped): to reSID when socket 1 or UltiSID 1 decodes it.
    pub fn write(&mut self, addr: u16, val: u8, clk: u64) {
        self.write_at(addr, val, Some(clk));
    }

    /// [`Sid::write`], clocking reSID to `clk` first when given.
    fn write_at(&mut self, addr: u16, val: u8, clk: Option<u64>) {
        let socket = self.socket1_hits(addr);
        if !socket && !self.decode.hits(ULTISID1, addr) {
            return;
        }
        if let Some(clk) = clk {
            self.catch_up(clk);
        }
        let reg = (addr & 0x1F) as u8;
        if socket && self.armsid.write(reg, val) {
            self.apply_model();
        }
        if let Some(r) = self.regs.get_mut(usize::from(reg)) {
            *r = val;
        }
        self.engine().write(reg, val);
    }

    /// A bus read at cycle `clk` (I/O mapped). None when no modelled SID decodes `addr`.
    pub fn read(&mut self, addr: u16, clk: u64) -> Option<u8> {
        let reg = (addr & 0x1F) as u8;
        let socket = self.socket1_hits(addr);
        if socket {
            if let Some(v) = self.armsid.read(reg) {
                return Some(v);
            }
        }
        if !socket && !self.decode.hits(ULTISID1, addr) {
            return None;
        }
        self.catch_up(clk);
        Some(self.engine().read(reg))
    }

    /// [`Sid::read`] for a debugger: no clocking, 0 before the engine exists.
    pub fn peek(&self, addr: u16) -> Option<u8> {
        let reg = (addr & 0x1F) as u8;
        let socket = self.socket1_hits(addr);
        if let Some(v) = self.armsid.read(reg).filter(|_| socket) {
            return Some(v);
        }
        (socket || self.decode.hits(ULTISID1, addr)).then(|| self.resid.as_ref().map_or(0, |r| r.read(reg)))
    }

    /// The C64 reset line was asserted: the chip clears its registers; the ARMSID leaves configuration mode but
    /// keeps its mode.
    pub fn reset(&mut self) {
        self.regs = [0; 0x19];
        self.armsid.config = false;
        if let Some(r) = &mut self.resid {
            r.reset();
        }
    }

    /// TRX64 restarted its cycle counter at `clk` (warm reset, c64_6510core.rs:677).
    pub fn reanchor(&mut self, clk: u64) {
        self.clk = clk;
    }

    /// reSID model the firmware's settings ask for.
    fn wanted_model(&self) -> i32 {
        if self.socket1 && self.decode.socket1_live() {
            self.armsid.model
        } else if self.decode.ultisid1_waves != 0 {
            MODEL_8580
        } else {
            MODEL_6581
        }
    }

    /// TRX64's `ResidConfig` is fixed at construction, so a model change rebuilds the engine (TRX64 API gap).
    fn apply_model(&mut self) {
        let model = self.wanted_model();
        if model != self.model {
            self.model = model;
            self.resid = None;
        }
    }

    fn engine(&mut self) -> &mut Resid {
        let (model, rate, regs) = (self.model, self.sample_rate, self.regs);
        self.resid.get_or_insert_with(|| {
            let rate = f64::from(rate);
            let mut r = Resid::new(ResidConfig {
                model,
                sample_rate: rate,
                passband: rate * 90.0 / 200.0,
                filter: true,
                ..ResidConfig::default()
            });
            for (reg, &val) in regs.iter().enumerate() {
                r.write(reg as u8, val);
            }
            r
        })
    }

    /// Clock reSID to cycle `clk`: all of it into the sink, or at most [`SILENT_CATCH_UP`] cycles without one.
    fn catch_up(&mut self, clk: u64) {
        if clk <= self.clk {
            return;
        }
        let mut delta = clk - self.clk;
        self.clk = clk;
        if self.audio.is_none() {
            delta = delta.min(SILENT_CATCH_UP);
        }
        // Always the sampled (per-cycle) clock, samples discarded without a sink: reSID's batch `clock(delta)`
        // (`Resid::clock_silent`) leaves the ENV3 latch alone (envelope.h:118) and, mixed with sampled clocking on
        // one engine, froze the envelope counter (ENV3 70 for 20 ms of attack 0; S14 §W4-SID).
        while delta > 0 {
            let n = delta.min(CHUNK);
            delta -= n;
            let pcm = self.engine().emit(n as u32);
            if let Some(sink) = &mut self.audio {
                sink.samples(&pcm);
            }
        }
    }
}

impl Default for Sid {
    fn default() -> Self {
        Self::new()
    }
}

/// TRX64 [`Observer`] for a CPU run: records each write to $D400-$D7FF that TRX64's SID hook confirms, with its cycle.
/// The hook fires inside the bus write, right before this record (full_sc.rs:252-256), and only when I/O is mapped.
pub struct SidTap;

impl Observer for SidTap {
    #[inline(always)]
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

    #[inline(always)]
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, _pc: u16, clk: u64, _old: u8) {
        if matches!(kind, BusKind::Write | BusKind::DummyWrite)
            && addr & 0xFC00 == 0xD400
            && HIT.with(|hit| hit.replace(false))
        {
            WRITES.with(|writes| writes.borrow_mut().push((clk, addr, value)));
        }
    }

    #[inline(always)]
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// The firmware's post-detection map with socket 1 enabled (S_SetupDetectionAddresses, u64_config.cc:2303-2319).
    fn detection_map(sid: &mut Sid) {
        for (off, val) in [(0x08, 0x40), (0x09, 0x50), (0x0A, 0x60), (0x0B, 0x60), (0x0C, 0xF0), (0x0D, 0xF0)] {
            sid.core_config(off, val);
        }
        for (off, val) in [(0x0E, 0xFE), (0x0F, 0xFE), (0x11, 1), (0x12, 1), (0x14, 0)] {
            sid.core_config(off, val);
        }
    }

    #[test]
    fn decode_follows_base_mask_enable_and_split() {
        let mut d = Decode::default();
        assert!(d.hits(ULTISID1, 0xD400) && d.hits(ULTISID1, 0xD7FF), "default: UltiSID 1 mirrored $D400-$D7FF");
        assert!(!d.hits(SOCKET1, 0xD400), "sockets disabled");
        assert!(!d.hits(ULTISID1, 0xD3FF) && !d.hits(ULTISID1, 0xDC00));
        d.write(0x08, 0x40);
        d.write(0x0C, 0xFE);
        d.write(0x11, 1);
        assert!(d.hits(SOCKET1, 0xD41F) && !d.hits(SOCKET1, 0xD420), "32-byte window");
        d.write(0x0A, 0xE0);
        d.write(0x0E, 0xFE);
        assert!(d.hits(ULTISID1, 0xDE1B) && !d.hits(ULTISID1, 0xD400), "UltiSID 1 at $DE00");
        d.write(0x0A, 0x40);
        d.write(0x0E, 0xC0 & !0x02);
        d.write(0x29, 1);
        assert!(d.hits(ULTISID1, 0xD400) && !d.hits(ULTISID1, 0xD420), "split A5: $D420 is UltiSID 2");
        d.write(0x08, UNMAPPED);
        assert!(!d.hits(SOCKET1, 0xD400) && !d.socket1_live(), "unmapped");
    }

    #[test]
    fn armsid_answers_the_firmware_probe_and_configuration() {
        let mut a = ArmSid::default();
        // detectRemakes (u64_config.cc:599-637).
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D')] {
            a.write(reg, val);
        }
        assert_eq!((a.read(0x1B), a.read(0x1C)), (Some(b'N'), Some(b'O')));
        a.write(0x1F, b'I');
        a.write(0x1E, b'I');
        assert!(!matches!(a.read(0x1B), Some(b'L' | b'R')), "ARMSID, not ARM2SID");
        a.write(0x1D, 0);
        assert_eq!(a.read(0x1B), None, "configuration mode left");
        // readParams (sid_device_armsid.cc:108-179).
        let info = |a: &mut ArmSid, cmd: u8| {
            for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D'), (0x1F, cmd), (0x1E, b'I')] {
                a.write(reg, val);
            }
            [a.read(0x1B).unwrap(), a.read(0x1C).unwrap()]
        };
        assert_eq!(info(&mut a, b'V'), ARMSID_VERSION);
        assert_eq!(info(&mut a, b'F')[0], b'6');
        assert_eq!(info(&mut a, b'U'), [0x23, 0x28]);
        // S_set_mode then S_set_filt (sid_device_armsid.cc:238-275).
        assert!([(0x1D, b'S'), (0x1E, b'E'), (0x1F, b'8')].iter().any(|&(r, v)| a.write(r, v)), "mode change");
        assert_eq!(a.model, MODEL_8580);
        for cmd in [0x8E, 0x91, 0xA2, 0xB3] {
            a.write(0x1F, cmd);
            a.write(0x1E, b'E');
        }
        assert_eq!(info(&mut a, b'H'), [0xE1, 0x23]);
        assert_eq!(info(&mut a, b'F')[0], b'8');
        a.write(0x1D, 0);
        assert!(!a.write(0x1F, b'6'), "no commands outside configuration mode");
        assert_eq!(a.model, MODEL_8580);
    }

    #[test]
    fn socket1_reads_armsid_then_resid_and_model_follows_the_settings() {
        let mut sid = Sid::new();
        detection_map(&mut sid);
        let mut clk = 1000;
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D')] {
            sid.write(0xD400 + reg, val, clk);
        }
        assert_eq!(sid.read(0xD41B, clk), None, "empty socket 1 does not answer");
        sid.set_socket1(true);
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D')] {
            sid.write(0xD400 + reg, val, clk);
        }
        assert_eq!((sid.read(0xD41B, clk), sid.read(0xD41C, clk)), (Some(b'N'), Some(b'O')));
        assert_eq!(sid.peek(0xD41C), Some(b'O'));
        assert_eq!(sid.read(0xD51B, clk), None, "socket 2 is empty");
        assert_eq!(sid.read(0xD61B, clk), Some(0), "UltiSID 1 parked at $D600 answers there (same engine)");
        assert_eq!(sid.read(0xD71B, clk), None, "nothing at $D700");
        sid.write(0xD41D, 0, clk);
        // Voice 3 envelope through reSID: attack 0, gate on.
        sid.write(0xD413, 0x00, clk);
        sid.write(0xD414, 0xF0, clk);
        sid.write(0xD412, 0x21, clk);
        clk += 20_000;
        assert_eq!(sid.read(0xD41C, clk), Some(0xFF), "ENV3 from reSID after 20 ms of attack 0");
        assert_eq!(sid.model, MODEL_6581);
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D'), (0x1E, b'E'), (0x1F, b'8')] {
            sid.write(0xD400 + reg, val, clk);
        }
        assert_eq!(sid.model, MODEL_8580, "ARMSID mode picks the model");
        assert_eq!(sid.regs[0x14], 0xF0, "registers kept for the rebuilt engine");
        sid.core_config(SID1_EN, 0);
        assert_eq!(sid.model, MODEL_6581, "without socket 1: UltiSID 1 waves 0");
        sid.core_config(EMUSID1_WAVES, 1);
        assert_eq!(sid.model, MODEL_8580);
    }

    #[test]
    fn tap_takes_only_writes_the_sid_hook_confirmed() {
        let sid = Sid::new();
        let mut trx = Sid6581::new();
        sid.install_hook(&mut trx);
        let regs = [0; 32];
        let mut tap = sid.tap();
        tap.on_bus(BusKind::Write, 0xD418, 0x0F, 0, 5, 0);
        trx.write(0x18, 0x0F, &regs);
        tap.on_bus(BusKind::Write, 0xD418, 0x0F, 0, 6, 0);
        trx.write(0x01, 0x10, &regs);
        tap.on_bus(BusKind::Fetch, 0xD401, 0x10, 0, 7, 0);
        tap.on_bus(BusKind::DummyWrite, 0xD401, 0x10, 0, 8, 0);
        let writes = WRITES.with(|w| std::mem::take(&mut *w.borrow_mut()));
        assert_eq!(writes, [(6, 0xD418, 0x0F), (8, 0xD401, 0x10)], "RAM under I/O (no hook) is not the SID");
    }

    #[test]
    fn cpu_writes_clock_resid_only_for_a_sink() {
        let mut sid = Sid::new();
        WRITES.with(|w| w.borrow_mut().extend([(500, 0xD418, 0x0F), (800, 0xD414, 0xF0), (900, 0xD412, 0x21)]));
        sid.advance(1000);
        assert_eq!((sid.clk, sid.regs[0x18], sid.regs[0x12]), (0, 0x0F, 0x21), "registers set, reSID not clocked");
        assert_eq!(sid.read(0xD41C, 50_000), Some(0xFF), "a DMA read clocks the gap: ENV3 after attack 0, sustain 15");
        assert_eq!(sid.clk, 50_000);
        sid.set_audio(48_000, Box::new(Collect::default()), 50_000);
        WRITES.with(|w| w.borrow_mut().push((60_000, 0xD418, 0x00)));
        sid.advance(70_000);
        assert_eq!((sid.clk, sid.regs[0x18]), (70_000, 0x00), "with a sink, writes and advances are clocked");
    }

    #[derive(Default)]
    struct Collect(Rc<RefCell<Vec<i16>>>);

    impl AudioSink for Collect {
        fn samples(&mut self, pcm: &[i16]) {
            self.0.borrow_mut().extend_from_slice(pcm);
        }
    }

    #[test]
    fn audio_follows_emulated_time_and_carries_the_tone() {
        let mut sid = Sid::new();
        let out = Collect::default();
        let pcm = Rc::clone(&out.0);
        sid.set_audio(48_000, Box::new(out), 0);
        // Default map: UltiSID 1 at $D400. 1000 Hz sawtooth (F = 17029, PAL), volume 15, sustain 15.
        let pokes = [(0x18, 15), (0x05, 0), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x04, 0x21)];
        for (reg, val) in pokes {
            sid.write(0xD400 + reg, val, 0);
        }
        for ms in 1..=1000u64 {
            sid.advance(ms * 985);
        }
        let pcm = pcm.borrow();
        assert!((47_900..=48_000).contains(&pcm.len()), "985 000 cycles at 48 kHz: {}", pcm.len());
        // Rising zero crossings over the last half second.
        let tail = &pcm[24_000..];
        let mean = tail.iter().map(|&s| f64::from(s)).sum::<f64>() / tail.len() as f64;
        let crossings = tail.windows(2).filter(|w| f64::from(w[0]) < mean && f64::from(w[1]) >= mean).count();
        let hz = crossings as f64 * 48_000.0 / tail.len() as f64 * (985_248.0 / 985_000.0);
        assert!((990.0..1010.0).contains(&hz), "{hz} Hz");
    }
}
