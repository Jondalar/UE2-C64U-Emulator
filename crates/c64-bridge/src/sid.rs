//! The U64's SIDs on TRX64's reSID: the firmware's SID decode (sockets, UltiSIDs and their split instances), one reSID
//! per receiver, the audio mixer, and an ARMSID identity in socket 1. Spec: docs/specs/S17-ultisid.md (after S14
//! §W4-SID) and TRX64 Spec 855; status: docs/status/sid-audio.md.
//!
//! - **Decode.** A decoder hits an address in $D400-$D7FF or $DE00-$DFFF when `(A11..A4 & MASK) == BASE`
//!   (sid_editor.cc:162-165), from the core config latches the firmware writes (u64.h:110-128). A socket also needs its
//!   enable (u64_config.cc:815-818) and a fitted chip (`--sid-socket1 armsid`, [`Sid::set_socket1`]). C64_EMUSID_SPLIT
//!   picks an UltiSID's register set A-D from address bits (u64_config.cc:320; sid_editor.cc:166-181).
//! - **Routing.** TRX64 routes an address to one chip, the U64 a write to every decoder that hits. So a TRX64 chip is a
//!   group, one distinct set of receivers, and each traced write fans out to its group (S17 §2.1).
//! - **Engines.** One reSID per receiver that received a write, all clocked with the same cycle deltas and mixed with the
//!   mixer's channel gains (S17 §2.2, §2.5).
//! - **Readback.** TRX64 answers the 6502 for chip 0 (855 D3). For the other chips, whose models TRX64 does not tick,
//!   and for the ARMSID in configuration mode, UE2 answers $1B/$1C through TRX64's host door (855 D5). Firmware DMA
//!   reads go through UE2's own decode.
//! - **Timing.** TRX64's write trace (855 D4) queues every write with its chip and cycle. With an [`AudioSink`] each is
//!   applied at its cycle and reSID follows every `advance_to`; without one, CPU writes only set registers and a DMA
//!   access clocks the gap.

use std::cell::RefCell;

use trx64_core::resid_ffi::{MODEL_6581, MODEL_8580, PAL_CLOCK_FREQ};
use trx64_core::sid::SidMapping;
use trx64_core::{BusKind, Machine, Observer, Resid, ResidConfig};

/// The mono sample stream of the emulated SIDs, in emulated-time order at the rate given to [`Sid::set_audio`].
pub trait AudioSink {
    fn samples(&mut self, pcm: &[i16]);
}

/// Core config offsets (0x10180000 + off, u64.h:110-128).
const SID1_BASE: u8 = 0x08;
const EMUSID2_BASE: u8 = 0x0B;
const SID1_MASK: u8 = 0x0C;
const EMUSID2_MASK: u8 = 0x0F;
const SID1_EN: u8 = 0x11;
const SID2_EN: u8 = 0x12;
const STEREO_ADDRSEL: u8 = 0x14;
const EMUSID1_WAVES: u8 = 0x20;
const EMUSID2_WAVES: u8 = 0x21;
const EMUSID_SPLIT: u8 = 0x29;

/// Decoder index: C64_SID1_BASE + i and C64_SID1_MASK + i (u64.h:110-117), the firmware's SID slots 0-3 (MapSid,
/// u64_config.cc:1943-1947).
const SOCKET1: usize = 0;
const ULTISID1: usize = 2;

/// C64_EMUSID_SPLIT index → the address bits of A11..A4 that pick an instance (split_bits, u64_config.cc:320).
const SPLIT_BITS: [u8; 8] = [0x00, 0x02, 0x04, 0x08, 0x10, 0x06, 0x12, 0x18];

/// Receivers, one bit each in a set: socket 1, then UltiSID 1 and 2 instances A-D. Socket 2 is never fitted.
const RECEIVERS: usize = 9;
const SOCKET1_RX: usize = 0;
const ULTISID1_A: usize = ultisid_rx(0, 0);

/// UltiSID `n` (0 or 1), instance `i` (0-3 = A-D).
const fn ultisid_rx(n: usize, i: usize) -> usize {
    1 + 4 * n + i
}

const fn bit(rx: usize) -> u16 {
    1 << rx
}

/// The 32-byte blocks a SID can occupy (u64_sid_base, u64_config.cc:218-226): 32 in $D400-$D7FF, 16 in $DE00-$DFFF.
const BLOCKS: [u16; 48] = {
    let mut blocks = [0; 48];
    let mut i = 0;
    while i < 48 {
        blocks[i] = if i < 32 { 0xD400 + 32 * i as u16 } else { 0xDE00 + 32 * (i as u16 - 32) };
        i += 1;
    }
    blocks
};

/// A SID mapped into $DE00-$DFFF answers reads ahead of the expansion port. An unverified assumption (S17 §2.1, 855
/// §7): the RTL that would settle it is closed; the firmware offers those addresses as SID addresses, so a SID there is
/// meant to be heard.
const AHEAD_OF_EXPANSION: bool = true;

/// U64_AUDIO_MIXER (0x10100500): two bytes per channel (u64_config.cc:1349-1358).
const MIXER_BYTES: usize = 20;
/// The mixer the default settings give (S17 §1.4), until the firmware writes it.
const MIXER_DEFAULT: [u8; MIXER_BYTES] = [
    0x5A, 0x5A, 0x5A, 0x5A, 0x79, 0x27, 0x27, 0x79, 0x79, 0x27, 0x27, 0x79, 0x19, 0x0D, 0x0D, 0x19, 0x04, 0x04, 0x04, 0x04,
];
/// A channel's two bytes summing to this are unity mono gain: the firmware's 0 dB centre, 0x5A/0x5A (S17 §2.5).
const UNITY: i32 = 180;
/// Mixer channels of the receivers (u64_config.cc:436-446).
const CH_ULTISID1: usize = 0;
const CH_ULTISID2: usize = 1;
const CH_SOCKET1: usize = 2;

/// Without an audio sink, a gap longer than this is clocked as this many cycles (1 s): the chip has settled, and a
/// DMA read after minutes of silence does not clock minutes of reSID.
const SILENT_CATCH_UP: u64 = 985_248;
/// Largest cycle count handed to reSID at once (its `clock` takes an `int`).
const CHUNK: u64 = 1 << 20;
/// reSID's clock (TRX64 `ResidConfig` default), for the samples owed while no engine runs.
const CLOCK_HZ: u64 = PAL_CLOCK_FREQ as u64;

/// ARMSID "VI" answer: firmware version, shown as "%d.%d" (readParams, sid_device_armsid.cc:105-175). Any value serves.
const ARMSID_VERSION: [u8; 2] = [3, 0];
/// ARMSID "UI" answer: supply voltage in mV, big endian (readParams). A socket not holding a 6581 gets the 9 V
/// regulator setting (effectuate_settings, u64_config.cc:753-790).
const ARMSID_MILLIVOLTS: u16 = 9000;

/// What TRX64's trace and host door reach from inside a machine run, where [`Sid`] cannot be borrowed. Thread-local,
/// because a machine runs on one thread: one machine per thread.
struct Door {
    /// Traced writes not applied yet: (cycle, chip, register, value).
    writes: Vec<(u64, u8, u8, u8)>,
    /// Socket 1's ARMSID. The trace drives it, so a probe reads its answer in the same run.
    armsid: ArmSid,
    /// Bit `chip` set: that chip's group holds socket 1.
    armsid_chips: u64,
    /// $1B/$1C of every chip from its group's first reSID as of the last catch-up; None for chip 0 and for a group
    /// without an engine.
    readback: Vec<Option<[u8; 2]>>,
}

impl Door {
    const fn new() -> Self {
        Door { writes: Vec::new(), armsid: ArmSid::new(), armsid_chips: 0, readback: Vec::new() }
    }

    fn armsid_on(&self, chip: u8) -> bool {
        self.armsid_chips.checked_shr(u32::from(chip)).is_some_and(|bits| bits & 1 != 0)
    }
}

thread_local! {
    static DOOR: RefCell<Door> = const { RefCell::new(Door::new()) };
}

/// TRX64's SID write trace (855 D4): CPU writes at their cycle, REU and host pokes at `c64_core.clk`, `write_full` at
/// the stale `Machine.clk`.
fn traced(chip: u8, reg: u8, val: u8, clk: u64) {
    DOOR.with(|door| {
        let mut door = door.borrow_mut();
        if door.armsid_on(chip) {
            door.armsid.write(reg, val);
        }
        door.writes.push((clk, chip, reg, val));
    });
}

/// TRX64's host read and peek door (855 D5): $1B/$1C from the ARMSID in configuration mode, else from the reSID of a
/// chip TRX64 does not tick. Side-effect free, so the peek is the same function.
fn answered(chip: u8, reg: usize) -> Option<u8> {
    if !(0x1B..=0x1C).contains(&reg) {
        return None;
    }
    DOOR.with(|door| {
        let door = door.borrow();
        door.armsid_on(chip)
            .then(|| door.armsid.read(reg as u8))
            .flatten()
            .or_else(|| door.readback.get(usize::from(chip)).copied().flatten().map(|r| r[reg - 0x1B]))
    })
}

/// The SID decode latches (docs/hw/10 §SID addressing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decode {
    /// SID1, SID2, EMUSID1, EMUSID2 BASE and MASK.
    base: [u8; 4],
    mask: [u8; 4],
    socket_en: [bool; 2],
    /// C64_EMUSID1/2_WAVES: 0 = 6581, 1 = 8580 (u64_config.cc:1651-1656).
    waves: [u8; 2],
    /// C64_EMUSID_SPLIT index, both UltiSIDs (get_sid_addresses, u64_config.cc:255-256).
    split: u8,
}

impl Default for Decode {
    /// The firmware's default map until it writes the latches: sockets disabled, all four decoders at $D400 mirrored
    /// through $D7FF (boot log "Resulting address map: Slot1: 40/C0 (Disabled) … Emu1: 40/C0  Emu2: 40/C0",
    /// u64_config.cc:881-887).
    fn default() -> Self {
        Decode { base: [0x40; 4], mask: [0xC0; 4], socket_en: [false; 2], waves: [0; 2], split: 0 }
    }
}

impl Decode {
    fn write(&mut self, off: u8, val: u8) {
        match off {
            SID1_BASE..=EMUSID2_BASE => self.base[usize::from(off - SID1_BASE)] = val,
            SID1_MASK..=EMUSID2_MASK => self.mask[usize::from(off - SID1_MASK)] = val,
            SID1_EN | SID2_EN => self.socket_en[usize::from(off - SID1_EN)] = val & 1 != 0,
            // The address-select pin of a dual chip in a socket (SetSidAddress, u64_config.cc:1889-1891). The emulated
            // ARMSID is one chip, so both halves reach it, and the firmware has already cleared the bit from MASK.
            STEREO_ADDRSEL => {}
            EMUSID1_WAVES | EMUSID2_WAVES => self.waves[usize::from(off - EMUSID1_WAVES)] = val & 1,
            EMUSID_SPLIT => self.split = val & 7,
            _ => {}
        }
    }

    /// Receivers of the block holding `addr`; socket 1 only when `socket1` is fitted. `0x01` in BASE is "unmapped":
    /// `a & MASK` never has bit 0 (u64_config.cc:202-203).
    ///
    /// A4 is not decoded: every MASK the firmware writes leaves bit 0 open (`0xFE & ~other`, u64_config.cc:1905-1918;
    /// 0xC0 at boot, 0xF0/0xFE for detection), and its address editor steps in 32-byte blocks (sid_editor.cc:160-163).
    fn receivers(&self, addr: u16, socket1: bool) -> u16 {
        if !Sid::window(addr) {
            return 0;
        }
        let a = (addr >> 4) as u8 & !0x01;
        let hits = |dev: usize| a & self.mask[dev] == self.base[dev];
        let mut rx = 0;
        if socket1 && self.socket_en[SOCKET1] && hits(SOCKET1) {
            rx |= bit(SOCKET1_RX);
        }
        for n in 0..2 {
            if hits(ULTISID1 + n) {
                rx |= bit(ultisid_rx(n, self.instance(a)));
            }
        }
        rx
    }

    /// The UltiSID instance, 0-3 for A-D, that `a` = A11..A4 selects (sid_editor.cc:166-181).
    fn instance(&self, a: u8) -> usize {
        let bits = SPLIT_BITS[usize::from(self.split)];
        usize::from(match bits {
            0x06 => (a & 0x06) >> 1,
            0x12 => ((a & 0x02) >> 1) + ((a & 0x10) >> 3),
            0x18 => (a & 0x18) >> 3,
            _ => u8::from(a & bits != 0),
        })
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
        Self::new()
    }
}

impl ArmSid {
    const fn new() -> Self {
        ArmSid { cfg: [0; 3], config: false, answer: [0; 2], model: MODEL_6581, filter: [0; 4] }
    }

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

/// One receiver's reSID, and registers $00-$18 as written, replayed into a rebuilt engine.
struct Engine {
    resid: Resid,
    model: i32,
    regs: [u8; 0x19],
}

impl Engine {
    /// TRX64's `ResidConfig` is fixed at construction, so a model or rate change builds the engine again; oscillator and
    /// envelope phase restart.
    fn build(model: i32, rate: u32, regs: [u8; 0x19]) -> Self {
        let rate = f64::from(rate);
        let mut resid = Resid::new(ResidConfig {
            model,
            sample_rate: rate,
            passband: rate * 90.0 / 200.0,
            filter: true,
            ..ResidConfig::default()
        });
        for (reg, &val) in regs.iter().enumerate() {
            resid.write(reg as u8, val);
        }
        Engine { resid, model, regs }
    }
}

/// How [`Sid::drain`] clocks the engines before each queued write.
#[derive(Clone, Copy)]
enum Clocking {
    /// Registers only.
    Untimed,
    /// To the write's own traced cycle.
    Traced,
    /// To this cycle, once, for all of them.
    At(u64),
}

/// The SIDs behind the U64's decoders, their reSID engines and the mixer.
pub struct Sid {
    decode: Decode,
    /// Socket 1 holds the ARMSID; else it is empty and never decodes, whatever C64_SID1_EN says.
    socket1: bool,
    /// Receivers of each TRX64 chip.
    groups: Vec<u16>,
    /// The table TRX64 routes by (855 D2), and whether it changed since [`Sid::take_map`].
    map: Vec<SidMapping>,
    map_changed: bool,
    engines: [Option<Engine>; RECEIVERS],
    /// Receivers with an engine, in the order the engines were built. The first one's sample count is each chunk's.
    order: Vec<usize>,
    sample_rate: u32,
    /// C64 cycle the engines have been clocked to.
    clk: u64,
    /// Cycles × sample rate not yet turned into silence while no engine runs.
    pace: u64,
    mixer: [u8; MIXER_BYTES],
    audio: Option<Box<dyn AudioSink>>,
}

impl Sid {
    /// Unused rate while no sink is attached (reSID still needs one to be configured).
    const SILENT_RATE: u32 = 44_100;

    pub fn new() -> Self {
        DOOR.with(|door| *door.borrow_mut() = Door::new());
        let mut sid = Sid {
            decode: Decode::default(),
            socket1: false,
            groups: Vec::new(),
            map: Vec::new(),
            map_changed: false,
            engines: Default::default(),
            order: Vec::new(),
            sample_rate: Self::SILENT_RATE,
            clk: 0,
            pace: 0,
            mixer: MIXER_DEFAULT,
            audio: None,
        };
        sid.route();
        sid
    }

    /// Hook into `m`: the decode table, TRX64's write trace and its host door. TRX64's resets keep all three (855 D2).
    pub fn install(&mut self, m: &mut Machine) {
        m.set_sid_write_trace(Some(Box::new(traced)));
        m.set_sid_host_access(Some(Box::new(answered)), Some(Box::new(answered)));
        m.set_sid_map(self.map.clone());
        self.map_changed = false;
    }

    /// The decode table for `Machine::set_sid_map` when it changed since the last call.
    pub fn take_map(&mut self) -> Option<Vec<SidMapping>> {
        std::mem::take(&mut self.map_changed).then(|| self.map.clone())
    }

    /// Fit (`true`) or empty socket 1. The firmware detects a fitted ARMSID at its next boot. Changes the table.
    pub fn set_socket1(&mut self, fitted: bool) {
        self.drain(self.clocking());
        self.socket1 = fitted;
        if !fitted {
            self.engines[SOCKET1_RX] = None;
            self.order.retain(|&rx| rx != SOCKET1_RX);
        }
        self.route();
    }

    /// Send samples at `sample_rate` Hz to `sink` from cycle `clk` on. Without a sink the engines lag the C64, so they
    /// first catch up silently: the stream starts at `clk`, not with the backlog.
    pub fn set_audio(&mut self, sample_rate: u32, sink: Box<dyn AudioSink>, clk: u64) {
        self.catch_up(clk);
        self.clk = self.clk.max(clk);
        if sample_rate != self.sample_rate {
            self.sample_rate = sample_rate;
            for e in self.engines.iter_mut().flatten() {
                *e = Engine::build(e.model, sample_rate, e.regs);
            }
        }
        self.audio = Some(sink);
    }

    /// The observer for a CPU run. Writes come from TRX64's trace now (S17 §2.3); it stays so the run calls do not
    /// change.
    pub fn tap(&self) -> SidTap {
        SidTap
    }

    /// Apply the writes traced during the last run, then follow the C64 to cycle `clk` when a sink listens.
    ///
    /// Without a sink the writes only set registers: nothing hears their timing, and the next DMA access clocks the
    /// gap. A program that keeps writing the SID (the placeholder KERNAL's jingle, default_kernal.tas:98-135) then costs
    /// no reSID clocking.
    pub fn advance(&mut self, clk: u64) {
        self.drain(self.clocking());
        if self.audio.is_some() {
            self.catch_up(clk);
        }
    }

    /// Apply, at cycle `clk`, the writes a firmware DMA write just made through `Machine::write_full`: that path's trace
    /// stamps `Machine.clk`, which is stale between runs (855 D4). Clocks the gap, as any DMA access does.
    pub fn dma_written(&mut self, clk: u64) {
        self.drain(Clocking::At(clk));
    }

    /// A C64 core config latch changed (0x10180000 + `off`). A decode change rebuilds the table and the engines whose
    /// model changed.
    pub fn core_config(&mut self, off: u8, val: u8) {
        let mut decode = self.decode;
        decode.write(off, val);
        if decode == self.decode {
            return;
        }
        // Writes traced so far went to the old groups.
        self.drain(self.clocking());
        self.decode = decode;
        self.route();
        self.remodel();
    }

    /// U64_AUDIO_MIXER byte `off` (0x10100500 + `off`); the gains apply from the next samples on.
    pub fn mixer_write(&mut self, off: u8, val: u8) {
        if let Some(byte) = self.mixer.get_mut(usize::from(off)) {
            *byte = val;
        }
    }

    /// Whether `addr` is in a range a SID can be mapped to.
    pub fn window(addr: u16) -> bool {
        matches!(addr, 0xD400..=0xD7FF | 0xDE00..=0xDFFF)
    }

    /// A firmware DMA read at cycle `clk` (I/O mapped) through UE2's own decode: the ARMSID's answer, else the first
    /// receiver's reSID (S17 §5 Q1). None when no SID decodes `addr`.
    pub fn read(&mut self, addr: u16, clk: u64) -> Option<u8> {
        let (rx, reg) = (self.receivers(addr), (addr & 0x1F) as u8);
        if rx == 0 {
            return None;
        }
        if let Some(v) = Self::armsid_answer(rx, reg) {
            return Some(v);
        }
        self.catch_up(clk);
        Some(self.first_engine(rx).map_or(0, |e| e.resid.read(reg)))
    }

    /// [`Sid::read`] for a debugger: no clocking.
    pub fn peek(&self, addr: u16) -> Option<u8> {
        let (rx, reg) = (self.receivers(addr), (addr & 0x1F) as u8);
        let resid = || self.first_engine(rx).map_or(0, |e| e.resid.read(reg));
        (rx != 0).then(|| Self::armsid_answer(rx, reg).unwrap_or_else(resid))
    }

    /// The C64 reset line was asserted: the chips clear their registers; the ARMSID leaves configuration mode but keeps
    /// its mode.
    pub fn reset(&mut self) {
        self.drain(self.clocking());
        for e in self.engines.iter_mut().flatten() {
            e.regs = [0; 0x19];
            e.resid.reset();
        }
        DOOR.with(|door| door.borrow_mut().armsid.config = false);
        self.refresh_readback();
    }

    /// TRX64 restarted its cycle counter at `clk` (warm reset, c64_6510core.rs:677).
    pub fn reanchor(&mut self, clk: u64) {
        self.clk = clk;
    }

    fn clocking(&self) -> Clocking {
        if self.audio.is_some() {
            Clocking::Traced
        } else {
            Clocking::Untimed
        }
    }

    fn receivers(&self, addr: u16) -> u16 {
        self.decode.receivers(addr, self.socket1)
    }

    /// Groups and table from the decode (S17 §2.1). Every $D400-$D7FF block is listed, a block nobody decodes on an
    /// empty group, so TRX64's fallback to chip 0 never fires; a $DE00-$DFFF block only when something receives it.
    /// Chip 0, the one TRX64 ticks, is the first group holding UltiSID 1-A, else the first block's.
    fn route(&mut self) {
        let blocks = BLOCKS.map(|start| (start, self.receivers(start)));
        let chip0 = blocks.iter().find(|(_, rx)| rx & bit(ULTISID1_A) != 0).unwrap_or(&blocks[0]).1;
        let mut groups = vec![chip0];
        let mut map = Vec::with_capacity(BLOCKS.len());
        for (start, rx) in blocks {
            if start >= 0xDE00 && rx == 0 {
                continue;
            }
            let chip = groups.iter().position(|&g| g == rx).unwrap_or_else(|| {
                groups.push(rx);
                groups.len() - 1
            });
            map.push(SidMapping::window(start, chip as u8, AHEAD_OF_EXPANSION));
        }
        let armsid_chips = (groups.iter().enumerate())
            .filter(|(_, rx)| **rx & bit(SOCKET1_RX) != 0)
            .fold(0, |chips, (chip, _)| chips | 1u64 << chip);
        DOOR.with(|door| door.borrow_mut().armsid_chips = armsid_chips);
        self.groups = groups;
        if map != self.map {
            self.map = map;
            self.map_changed = true;
        }
        self.refresh_readback();
    }

    /// The ARMSID's configuration answer when socket 1 is among `rx`.
    fn armsid_answer(rx: u16, reg: u8) -> Option<u8> {
        (rx & bit(SOCKET1_RX) != 0).then(|| DOOR.with(|door| door.borrow().armsid.read(reg))).flatten()
    }

    /// The model a receiver's settings ask for: the ARMSID's mode for socket 1, WAVES of its UltiSID otherwise.
    fn wanted_model(&self, rx: usize) -> i32 {
        if rx == SOCKET1_RX {
            DOOR.with(|door| door.borrow().armsid.model)
        } else if self.decode.waves[(rx - 1) / 4] != 0 {
            MODEL_8580
        } else {
            MODEL_6581
        }
    }

    /// Receiver `rx`'s engine: built at its first write, rebuilt with its registers when its model changed.
    fn engine(&mut self, rx: usize) -> &mut Engine {
        let model = self.wanted_model(rx);
        match self.engines[rx].as_ref().map(|e| (e.model, e.regs)) {
            Some((built, _)) if built == model => {}
            Some((_, regs)) => self.engines[rx] = Some(Engine::build(model, self.sample_rate, regs)),
            None => {
                self.engines[rx] = Some(Engine::build(model, self.sample_rate, [0; 0x19]));
                self.order.push(rx);
            }
        }
        self.engines[rx].as_mut().expect("built above")
    }

    /// Rebuild every engine whose model no longer matches its settings.
    fn remodel(&mut self) {
        for rx in 0..RECEIVERS {
            if self.engines[rx].is_some() {
                self.engine(rx);
            }
        }
    }

    /// The first receiver among `rx` that has an engine.
    fn first_engine(&self, rx: u16) -> Option<&Engine> {
        (0..RECEIVERS).filter(|&r| rx & bit(r) != 0).find_map(|r| self.engines[r].as_ref())
    }

    /// Mono gain of receiver `rx` in 1/[`UNITY`] steps: the sum of its mixer channel's two bytes.
    fn gain(&self, rx: usize) -> i32 {
        let ch = match rx {
            SOCKET1_RX => CH_SOCKET1,
            _ if rx < ultisid_rx(1, 0) => CH_ULTISID1,
            _ => CH_ULTISID2,
        };
        i32::from(self.mixer[2 * ch]) + i32::from(self.mixer[2 * ch + 1])
    }

    /// Apply the queued traced writes in order, each to every receiver of its chip's group.
    fn drain(&mut self, clocking: Clocking) {
        let mut writes = DOOR.with(|door| std::mem::take(&mut door.borrow_mut().writes));
        if !writes.is_empty() {
            if let Clocking::At(clk) = clocking {
                self.catch_up(clk);
            }
            for &(at, chip, reg, val) in &writes {
                if let Clocking::Traced = clocking {
                    self.catch_up(at);
                }
                let group = self.groups.get(usize::from(chip)).copied().unwrap_or(0);
                for rx in (0..RECEIVERS).filter(|&rx| group & bit(rx) != 0) {
                    let e = self.engine(rx);
                    if let Some(r) = e.regs.get_mut(usize::from(reg)) {
                        *r = val;
                    }
                    e.resid.write(reg, val);
                }
            }
            writes.clear();
        }
        // The queue keeps its allocation.
        DOOR.with(|door| {
            let mut door = door.borrow_mut();
            if door.writes.is_empty() {
                door.writes = writes;
            }
        });
    }

    /// Clock every engine to cycle `clk` with the same deltas: all of it into the sink, or at most [`SILENT_CATCH_UP`]
    /// cycles without one. Then refresh the readback the door answers from.
    fn catch_up(&mut self, clk: u64) {
        if clk <= self.clk {
            return;
        }
        let mut delta = clk - self.clk;
        self.clk = clk;
        let listening = self.audio.is_some();
        if !listening {
            delta = delta.min(SILENT_CATCH_UP);
        }
        // Always the sampled (per-cycle) clock, samples discarded without a sink: reSID's batch `clock(delta)`
        // (`Resid::clock_silent`) leaves the ENV3 latch alone (envelope.h:118) and, mixed with sampled clocking on
        // one engine, froze the envelope counter (ENV3 70 for 20 ms of attack 0; S14 §W4-SID).
        while delta > 0 {
            let n = delta.min(CHUNK);
            delta -= n;
            let mut mixed = Vec::new();
            for i in 0..self.order.len() {
                let rx = self.order[i];
                let gain = self.gain(rx);
                let pcm = self.engines[rx].as_mut().expect("ordered engines exist").resid.emit(n as u32);
                if listening {
                    if i == 0 {
                        mixed = vec![0; pcm.len()];
                    }
                    mix_into(&mut mixed, &pcm, gain);
                }
            }
            if !listening {
                continue;
            }
            let pcm = if self.order.is_empty() { vec![0; self.silence(n)] } else { mixed_down(&mixed) };
            if let Some(sink) = &mut self.audio {
                sink.samples(&pcm);
            }
        }
        self.refresh_readback();
    }

    /// Samples in `n` cycles while no engine runs, at reSID's cadence.
    fn silence(&mut self, n: u64) -> usize {
        self.pace += n * u64::from(self.sample_rate);
        let samples = self.pace / CLOCK_HZ;
        self.pace %= CLOCK_HZ;
        samples as usize
    }

    fn refresh_readback(&self) {
        DOOR.with(|door| {
            let mut door = door.borrow_mut();
            door.readback.clear();
            door.readback.extend(self.groups.iter().enumerate().map(|(chip, &rx)| {
                self.first_engine(rx).filter(|_| chip > 0).map(|e| [e.resid.read(0x1B), e.resid.read(0x1C)])
            }));
        });
    }

    #[cfg(test)]
    pub(crate) fn ultisid_regs(&self, n: usize, instance: usize) -> Option<[u8; 0x19]> {
        self.engines[ultisid_rx(n, instance)].as_ref().map(|e| e.regs)
    }
}

impl Default for Sid {
    fn default() -> Self {
        Self::new()
    }
}

/// Add `pcm` at `gain` into `acc`, padded with its last sample or trimmed to `acc`'s length: an engine built after the
/// first can be a sample apart per chunk (855 §2).
fn mix_into(acc: &mut [i32], pcm: &[i16], gain: i32) {
    let last = pcm.last().copied().unwrap_or(0);
    for (i, a) in acc.iter_mut().enumerate() {
        *a += i32::from(pcm.get(i).copied().unwrap_or(last)) * gain;
    }
}

fn mixed_down(acc: &[i32]) -> Vec<i16> {
    acc.iter().map(|&s| (s / UNITY).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16).collect()
}

/// TRX64 [`Observer`] for a CPU run. Empty: TRX64's write trace carries the chip and cycle now (855 D4).
pub struct SidTap;

impl Observer for SidTap {
    #[inline(always)]
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

    #[inline(always)]
    fn on_bus(&mut self, _: BusKind, _: u16, _: u8, _: u16, _: u64, _: u8) {}

    #[inline(always)]
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    use trx64_core::sid::resolve_sid;
    use ue2_core::c64host::C64Backend;

    use super::*;

    const U1A: u16 = bit(ULTISID1_A);
    const U1B: u16 = bit(ultisid_rx(0, 1));
    const U1C: u16 = bit(ultisid_rx(0, 2));
    const U1D: u16 = bit(ultisid_rx(0, 3));
    const U2A: u16 = bit(ultisid_rx(1, 0));
    const U2B: u16 = bit(ultisid_rx(1, 1));
    const S1: u16 = bit(SOCKET1_RX);

    /// A write as TRX64's trace reports it, routed by the table `sid` handed out.
    fn poke(sid: &Sid, addr: u16, val: u8, clk: u64) {
        let (chip, reg) = resolve_sid(&sid.map, addr).expect("every $D400-$D7FF block is listed");
        traced(chip, reg as u8, val, clk);
    }

    /// The SID player's map: unmapAllSids, then SetSidAddress for UltiSID 1 and 2 (u64_config.cc:1886-1920, 2031).
    fn player_map(sid: &mut Sid, ultisids: [(u8, u8); 2]) {
        for (dev, (base, mask)) in [(0x01, 0xFE), (0x01, 0xFE)].into_iter().chain(ultisids).enumerate() {
            sid.core_config(SID1_BASE + dev as u8, base);
            sid.core_config(SID1_MASK + dev as u8, mask);
        }
    }

    const UNMAPPED: (u8, u8) = (0x01, 0xFE);

    /// The firmware's post-detection map with socket 1 enabled (S_SetupDetectionAddresses, u64_config.cc:2303-2319).
    fn detection_map(sid: &mut Sid) {
        for (off, val) in [(0x08, 0x40), (0x09, 0x50), (0x0A, 0x60), (0x0B, 0x60), (0x0C, 0xF0), (0x0D, 0xF0)] {
            sid.core_config(off, val);
        }
        for (off, val) in [(0x0E, 0xFE), (0x0F, 0xFE), (0x11, 1), (0x12, 1), (0x14, 0)] {
            sid.core_config(off, val);
        }
    }

    /// Chip of each block as TRX64 would route its first address.
    fn chip_at(sid: &Sid, addr: u16) -> Option<u8> {
        resolve_sid(&sid.map, addr).map(|(chip, _)| chip)
    }

    fn group_at(sid: &Sid, addr: u16) -> Option<u16> {
        chip_at(sid, addr).map(|chip| sid.groups[usize::from(chip)])
    }

    #[test]
    fn boot_map_gives_d400_d7ff_to_both_ultisids() {
        let mut sid = Sid::new();
        let map = sid.take_map().expect("a new Sid has a table to hand over");
        assert_eq!(sid.take_map(), None, "handed over once");
        assert_eq!(map.len(), 32, "the $D400-$D7FF blocks, nothing at $DE00-$DFFF");
        for (i, w) in map.iter().enumerate() {
            assert_eq!((w.start, w.end, w.chip), (0xD400 + 32 * i as u16, 0xD41F + 32 * i as u16, 0));
        }
        assert_eq!(sid.groups, [U1A | U2A]);
        assert_eq!(chip_at(&sid, 0xDE00), None, "the expansion port keeps $DE00");
    }

    #[test]
    fn sid_player_maps_one_and_two_sids() {
        let mut sid = Sid::new();
        sid.take_map();
        player_map(&mut sid, [(0x40, 0xC0), UNMAPPED]);
        assert_eq!(sid.groups, [U1A], "one SID: UltiSID 1 mirrored through $D7FF (u64_config.cc:1893-1896)");
        assert!(sid.map.iter().all(|w| w.chip == 0) && sid.map.len() == 32);
        assert_eq!(sid.take_map(), None, "every block is still chip 0: only its group changed");

        player_map(&mut sid, [(0x40, 0xFE), (0x42, 0xFE)]);
        assert_eq!((group_at(&sid, 0xD400), chip_at(&sid, 0xD400)), (Some(U1A), Some(0)));
        assert_eq!((group_at(&sid, 0xD43F), chip_at(&sid, 0xD420)), (Some(U2A), Some(1)));
        assert_eq!((group_at(&sid, 0xD440), chip_at(&sid, 0xD7E0)), (Some(0), Some(2)), "nobody decodes: an empty group");
        assert_eq!(sid.groups, [U1A, U2A, 0]);
        assert_eq!(sid.map.len(), 32);
        assert!(sid.take_map().is_some());
    }

    #[test]
    fn splits_pick_separate_instances() {
        let mut sid = Sid::new();
        sid.core_config(EMUSID_SPLIT, 1);
        let alternating = [U1A | U2A, U1B | U2B, U1A | U2A, U1B | U2B];
        assert_eq!([0xD400, 0xD420, 0xD440, 0xD460].map(|a| group_at(&sid, a).unwrap()), alternating, "1/2 (A5)");
        assert_eq!(sid.groups, [U1A | U2A, U1B | U2B]);

        let fours: [(u8, [u16; 4]); 3] =
            [(5, [0x40, 0x42, 0x44, 0x46]), (6, [0x40, 0x42, 0x50, 0x52]), (7, [0x40, 0x48, 0x50, 0x58])];
        for (split, a) in fours {
            sid.core_config(EMUSID_SPLIT, split);
            let groups = a.map(|a| group_at(&sid, 0xD000 | a << 4).unwrap() & (U1A | U1B | U1C | U1D));
            assert_eq!(groups, [U1A, U1B, U1C, U1D], "1/4, split {split}");
        }
        assert_eq!(sid.groups.len(), 4, "A-D of both UltiSIDs, mirrored");
        assert_eq!(group_at(&sid, 0xD600), Some(U1A | U2A), "split 7: A7 and A8 clear again");
    }

    #[test]
    fn a_socket_needs_its_enable_and_a_fitted_chip() {
        let mut sid = Sid::new();
        detection_map(&mut sid);
        assert_eq!(group_at(&sid, 0xD400), Some(0), "socket 1 is empty");
        assert_eq!(group_at(&sid, 0xD600), Some(U1A | U2A), "UltiSIDs parked at $D600");
        assert_eq!(chip_at(&sid, 0xD600), Some(0));
        sid.set_socket1(true);
        assert_eq!(group_at(&sid, 0xD400), Some(S1));
        assert_eq!(group_at(&sid, 0xD4FF), Some(S1), "MASK 0xF0: $D400-$D4FF");
        assert_eq!(group_at(&sid, 0xD500), Some(0), "socket 2 is enabled at $D500, but never fitted");
        assert_eq!(group_at(&sid, 0xD620), Some(0), "MASK 0xFE: 32 bytes");
        sid.core_config(SID1_EN, 0);
        assert_eq!(group_at(&sid, 0xD400), Some(0), "disabled");
    }

    #[test]
    fn groups_are_numbered_from_ultisid_1a() {
        let mut sid = Sid::new();
        player_map(&mut sid, [(0x42, 0xFE), (0x40, 0xFE)]);
        assert_eq!(sid.groups, [U1A, U2A, 0], "chip 0 holds UltiSID 1-A although $D400 is UltiSID 2");
        assert_eq!([0xD400, 0xD420, 0xD440].map(|a| chip_at(&sid, a)), [Some(1), Some(0), Some(2)]);

        player_map(&mut sid, [UNMAPPED, UNMAPPED]);
        assert_eq!(sid.groups, [0], "nothing mapped: the first group, empty, is chip 0");
        assert!(sid.map.iter().all(|w| w.chip == 0) && sid.map.len() == 32);

        player_map(&mut sid, [(0x40, 0xFE), (0xE0, 0xFE)]);
        let de00 = *sid.map.last().unwrap();
        assert_eq!(sid.map.len(), 33, "one $DE00-$DFFF block, because UltiSID 2 receives it");
        assert_eq!((de00.start, de00.end, de00.ahead_of_expansion), (0xDE00, 0xDE1F, true));
        assert_eq!(sid.groups[usize::from(de00.chip)], U2A);
        assert_eq!(chip_at(&sid, 0xDE20), None);
    }

    #[test]
    fn a_write_fans_out_to_its_group() {
        let mut sid = Sid::new();
        sid.set_socket1(true);
        detection_map(&mut sid);
        for (off, val) in [(0x0A, 0x40), (0x0B, 0x40), (0x0E, 0xC0), (0x0F, 0xC0)] {
            sid.core_config(off, val);
        }
        assert_eq!(group_at(&sid, 0xD400), Some(S1 | U1A | U2A), "socket 1 and both UltiSIDs at $D400");
        poke(&sid, 0xD418, 0x0F, 0);
        poke(&sid, 0xD501, 66, 0);
        sid.advance(1000);
        for rx in [SOCKET1_RX, ULTISID1_A, ultisid_rx(1, 0)] {
            assert_eq!(sid.engines[rx].as_ref().map(|e| e.regs[0x18]), Some(0x0F), "an engine per receiver: {rx}");
        }
        assert_eq!(sid.engines[SOCKET1_RX].as_ref().map(|e| e.regs[0x01]), Some(0), "$D501 is not socket 1's");
        assert_eq!(sid.ultisid_regs(1, 0).map(|r| r[0x01]), Some(66), "the UltiSIDs mirror through $D7FF");
        assert!(sid.ultisid_regs(0, 1).is_none(), "no write, no engine");

        player_map(&mut sid, [(0x40, 0xFE), (0x42, 0xFE)]);
        poke(&sid, 0xD440, 0x55, 0);
        sid.advance(2000);
        assert_eq!(sid.order.len(), 3, "a write nobody decodes builds nothing");
    }

    #[test]
    fn each_ultisid_takes_its_own_model() {
        let mut sid = Sid::new();
        poke(&sid, 0xD414, 0xF0, 0);
        sid.advance(0);
        let model = |sid: &Sid, rx: usize| sid.engines[rx].as_ref().map(|e| (e.model, e.regs[0x14]));
        sid.core_config(EMUSID2_WAVES, 1);
        assert_eq!(model(&sid, ULTISID1_A), Some((MODEL_6581, 0xF0)));
        assert_eq!(model(&sid, ultisid_rx(1, 0)), Some((MODEL_8580, 0xF0)), "rebuilt with its registers");
        sid.core_config(EMUSID1_WAVES, 1);
        sid.core_config(EMUSID2_WAVES, 0);
        assert_eq!(model(&sid, ULTISID1_A), Some((MODEL_8580, 0xF0)));
        assert_eq!(model(&sid, ultisid_rx(1, 0)), Some((MODEL_6581, 0xF0)));
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

    /// Firmware DMA reads through UE2's decode: the ARMSID, then socket 1's reSID; the ARMSID's mode picks its model.
    #[test]
    fn dma_reads_socket1_armsid_then_resid() {
        let mut sid = Sid::new();
        detection_map(&mut sid);
        let mut clk = 1000;
        let dma = |sid: &mut Sid, reg: u16, val: u8, clk: u64| {
            poke(sid, 0xD400 + reg, val, clk);
            sid.dma_written(clk);
        };
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D')] {
            dma(&mut sid, reg, val, clk);
        }
        assert_eq!(sid.read(0xD41B, clk), None, "empty socket 1 does not answer");
        sid.set_socket1(true);
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D')] {
            dma(&mut sid, reg, val, clk);
        }
        assert_eq!((sid.read(0xD41B, clk), sid.read(0xD41C, clk)), (Some(b'N'), Some(b'O')));
        assert_eq!(sid.peek(0xD41C), Some(b'O'));
        assert_eq!(sid.read(0xD51B, clk), None, "socket 2 is empty");
        assert_eq!(sid.read(0xD61B, clk), Some(0), "UltiSID 1 parked at $D600, never written");
        assert_eq!(sid.read(0xD71B, clk), None, "nothing at $D700");
        dma(&mut sid, 0x1D, 0, clk);
        // Voice 3 envelope through reSID: attack 0, gate on.
        dma(&mut sid, 0x13, 0x00, clk);
        dma(&mut sid, 0x14, 0xF0, clk);
        dma(&mut sid, 0x12, 0x21, clk);
        clk += 20_000;
        assert_eq!(sid.read(0xD41C, clk), Some(0xFF), "ENV3 from reSID after 20 ms of attack 0");
        assert_eq!(sid.engines[SOCKET1_RX].as_ref().map(|e| e.model), Some(MODEL_6581));
        for (reg, val) in [(0x1D, b'S'), (0x1E, b'I'), (0x1F, b'D'), (0x1E, b'E'), (0x1F, b'8')] {
            dma(&mut sid, reg, val, clk);
        }
        let socket1 = sid.engines[SOCKET1_RX].as_ref().map(|e| (e.model, e.regs[0x14]));
        assert_eq!(socket1, Some((MODEL_8580, 0xF0)), "the ARMSID's mode, registers kept");
        sid.set_socket1(false);
        assert!(sid.engines[SOCKET1_RX].is_none() && !sid.order.contains(&SOCKET1_RX), "the chip left the socket");
    }

    #[test]
    fn cpu_writes_clock_resid_only_for_a_sink() {
        let mut sid = Sid::new();
        for (clk, reg, val) in [(500, 0x18, 0x0F), (800, 0x14, 0xF0), (900, 0x12, 0x21)] {
            poke(&sid, 0xD400 + reg, val, clk);
        }
        sid.advance(1000);
        let regs = sid.ultisid_regs(0, 0).unwrap();
        assert_eq!((sid.clk, regs[0x18], regs[0x12]), (0, 0x0F, 0x21), "registers set, reSID not clocked");
        assert_eq!(sid.read(0xD41C, 50_000), Some(0xFF), "a DMA read clocks the gap: ENV3 after attack 0, sustain 15");
        assert_eq!(sid.clk, 50_000);
        sid.set_audio(48_000, Box::new(Collect::default()), 50_000);
        poke(&sid, 0xD418, 0x00, 60_000);
        sid.advance(70_000);
        let regs = sid.ultisid_regs(0, 0).unwrap();
        assert_eq!((sid.clk, regs[0x18]), (70_000, 0x00), "with a sink, writes and advances are clocked");
    }

    #[derive(Default)]
    struct Collect(Rc<RefCell<Vec<i16>>>);

    impl AudioSink for Collect {
        fn samples(&mut self, pcm: &[i16]) {
            self.0.borrow_mut().extend_from_slice(pcm);
        }
    }

    /// 1000 Hz sawtooth (F = 17029, PAL), volume 15, sustain 15, on the SID at `base`.
    fn tone(sid: &Sid, base: u16, clk: u64) {
        for (reg, val) in [(0x18, 15), (0x05, 0), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x04, 0x21)] {
            poke(sid, base + reg, val, clk);
        }
    }

    /// Rising crossings of the mean per second.
    fn hz(pcm: &[i16], rate: f64) -> f64 {
        let mean = pcm.iter().map(|&s| f64::from(s)).sum::<f64>() / pcm.len() as f64;
        let crossings = pcm.windows(2).filter(|w| f64::from(w[0]) < mean && f64::from(w[1]) >= mean).count();
        crossings as f64 * rate / pcm.len() as f64 * (985_248.0 / 985_000.0)
    }

    /// RMS around the mean: steadier than peak-to-peak, whose extremes fall on a different sample every window.
    fn level(pcm: &[i16]) -> f64 {
        let mean = pcm.iter().map(|&s| f64::from(s)).sum::<f64>() / pcm.len() as f64;
        (pcm.iter().map(|&s| (f64::from(s) - mean).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
    }

    #[test]
    fn audio_follows_emulated_time_and_carries_the_tone() {
        let mut sid = Sid::new();
        let out = Collect::default();
        let pcm = Rc::clone(&out.0);
        sid.set_audio(48_000, Box::new(out), 0);
        sid.advance(985);
        assert_eq!(pcm.borrow().len(), 47, "no engine yet: 985 cycles of silence all the same");
        assert!(pcm.borrow().iter().all(|&s| s == 0));
        // Default map: UltiSID 1 and 2 at $D400, two engines playing the same.
        tone(&sid, 0xD400, 985);
        for ms in 2..=1000u64 {
            sid.advance(ms * 985);
        }
        assert_eq!(sid.order.len(), 2);
        let pcm = pcm.borrow();
        assert!((47_900..=48_000).contains(&pcm.len()), "985 000 cycles at 48 kHz: {}", pcm.len());
        let hz = hz(&pcm[24_000..], 48_000.0);
        assert!((990.0..1010.0).contains(&hz), "{hz} Hz");
    }

    #[test]
    fn mixer_gains_scale_and_mute() {
        let mut acc = vec![0; 4];
        mix_into(&mut acc, &[100, -200, 300], UNITY);
        mix_into(&mut acc, &[10, 20, 30, 40, 50], UNITY / 2);
        assert_eq!(mixed_down(&acc), [105, -190, 315, 320], "padded with the last sample, trimmed to the first engine");
        assert_eq!(mixed_down(&[i32::from(i16::MAX) * UNITY * 2]), [i16::MAX], "saturates");

        let mut sid = Sid::new();
        player_map(&mut sid, [(0x40, 0xC0), UNMAPPED]);
        let out = Collect::default();
        let pcm = Rc::clone(&out.0);
        sid.set_audio(48_000, Box::new(out), 0);
        tone(&sid, 0xD400, 0);
        let mut clk = 0;
        let mut run = |sid: &mut Sid, ms: u64| {
            let start = pcm.borrow().len();
            for _ in 0..ms {
                clk += 985;
                sid.advance(clk);
            }
            pcm.borrow()[start + (pcm.borrow().len() - start) / 2..].to_vec()
        };
        let unity = level(&run(&mut sid, 500));
        assert!(unity > 1000.0, "the default mixer's 0 dB centre: {unity}");
        sid.mixer_write(0, 0x2D);
        sid.mixer_write(1, 0x2D);
        let half = level(&run(&mut sid, 500));
        assert!((half / unity - 0.5).abs() < 0.01, "UltiSID 1 at half gain: {half} of {unity}");
        sid.mixer_write(2, 0);
        sid.mixer_write(3, 0);
        let other = level(&run(&mut sid, 200));
        assert!((other / half - 1.0).abs() < 0.01, "UltiSID 2's channel is not UltiSID 1's: {other} vs {half}");
        for off in 0..8 {
            sid.mixer_write(off, 0);
        }
        assert!(run(&mut sid, 100).iter().all(|&s| s == 0), "u64_mute_sids (u64_config.cc:1318-1331)");
    }

    /// Door tests run TRX64 itself: its bus read and peek reach [`answered`].
    fn machine() -> crate::Trx64Backend {
        crate::Trx64Backend::new(Path::new("/nonexistent"))
    }

    #[test]
    fn chip_1_reads_env3_from_its_resid_through_the_door() {
        let mut c64 = machine();
        for (off, val) in [(0x0A, 0x40), (0x0E, 0xFE), (0x0B, 0x42), (0x0F, 0xFE)] {
            c64.core_config_write(off, val);
        }
        assert_eq!(c64.m.sid_map()[1].chip, 1, "UltiSID 2 at $D420 is chip 1");
        c64.sid.set_audio(48_000, Box::new(Collect::default()), c64.m.c64_core.clk);
        // Voice 3 of UltiSID 2: attack 0, sustain 15, gate on.
        c64.m.poke_io(0xD433, &[0x00, 0xF0]);
        c64.m.poke_io(0xD432, &[0x21]);
        let clk = c64.m.c64_core.clk;
        c64.sid.advance(clk + 20_000);
        assert_eq!(c64.m.read_full(0xD43C), 0xFF, "peek: ENV3 of UltiSID 2's reSID after 20 ms of attack 0");
        assert_eq!(c64.m.read_full_live(0xD43C), 0xFF, "bus read");
        assert_eq!(c64.m.read_full_live(0xD41C), 0, "chip 0 is TRX64's own, and nothing played there");
    }

    #[test]
    fn the_armsid_answers_through_the_door() {
        let mut c64 = machine();
        c64.set_sid_socket1(true);
        let detection = [(0x08, 0x40), (0x0C, 0xFE), (0x11, 1), (0x0A, 0x60), (0x0E, 0xFE), (0x0B, 0x60), (0x0F, 0xFE)];
        for (off, val) in detection {
            c64.core_config_write(off, val);
        }
        let chip = c64.m.sid_map()[0].chip;
        assert_ne!(chip, 0, "socket 1 alone at $D400; chip 0 holds the UltiSIDs at $D600");
        c64.m.poke_io(0xD41D, b"SID");
        assert_eq!((c64.m.read_full(0xD41B), c64.m.read_full(0xD41C)), (b'N', b'O'), "peek");
        let bus = (c64.m.read_full_live(0xD41B), c64.m.read_full_live(0xD41C));
        assert_eq!(bus, (b'N', b'O'), "bus read, before any drain");
        assert_eq!(c64.m.read_full_live(0xD61B), 0, "the UltiSIDs are no ARMSID");
        c64.m.poke_io(0xD41D, &[0]);
        assert_eq!(c64.m.read_full_live(0xD41B), 0, "configuration mode left");
    }
}
