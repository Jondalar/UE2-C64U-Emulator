//! S40: the board's engine (`trx_engine.v`): page 2, the record former, the engine store, the mover and APPLY
//! (docs/specs/S40-trx-engine.md §3, §4, §6).
//!
//! The board's C64 core hands the engine the frame boundaries and the write taps; its drain puts each record into its
//! stream's circular area in DDR2 and its mover copies areas into the app's memory. UE2 forms a record when the C64
//! port moves the bridge's events here, in their order, and writes it at once: no FIFO, nothing dropped, nothing
//! waiting. The areas the board keeps at `0x5C00000-0x7FFFFFF` lie past UE2's 64 MB of guest DDR, so the engine has a
//! store of its own for the whole 27-bit DDR2 address space, allocated in 1 MB chunks on first write. Records below
//! 64 MB are written through to guest DDR as well.

use std::collections::VecDeque;

use crate::c64host::TapEvent;
use crate::devices::trx_sb::{Boundary, StateBus};

/// Page 2 of the block (trx_sys.v: page 2 word accesses go to trx_engine.v).
pub const PAGE2: u32 = 0x2000;
pub const LOG_CTRL: u32 = 0x000;
pub const LOG_STAT: u32 = 0x004;
pub const FRAME: u32 = 0x008;
pub const DROPPED: u32 = 0x00C;
/// `LOG_BASE[s]`, `LOG_MASK[s]`, `LOG_CNT[s]` at `0x020 + 0x10 s`, `+4`, `+8`.
pub const LOG_BASE: u32 = 0x020;
pub const LOG_MASK: u32 = 0x024;
pub const LOG_CNT: u32 = 0x028;
pub const LOG_PUT_LO: u32 = 0x050;
pub const LOG_PUT_HI: u32 = 0x054;
pub const CAP_TAB_IDX: u32 = 0x060;
pub const CAP_TAB_DATA: u32 = 0x064;
pub const CAP_TAB_LEN: u32 = 0x068;
pub const ENG_CMD: u32 = 0x080;
pub const ENG_SRC: u32 = 0x084;
pub const ENG_DST: u32 = 0x088;
pub const ENG_LEN: u32 = 0x08C;
pub const ENG_ARG: u32 = 0x090;
pub const AP_START: u32 = 0x094;
pub const AP_COUNT: u32 = 0x098;
pub const AP_CMD: u32 = 0x09C;

/// `LOG_CTRL` (trx_engine.v:463-471): b0 on, b3:1 taps (b1 outside, b2 cartridge RAM, b3 colour RAM), b8 restart.
pub const LOG_ON: u32 = 0x001;
pub const LOG_RESTART: u32 = 0x100;
const TAP_OUTSIDE: u8 = 0x1;
const TAP_CART: u8 = 0x2;
const TAP_COLOUR: u8 = 0x4;

/// 28-bit counts and record indices.
pub const M28: u32 = 0x0FFF_FFFF;
/// The DDR2 address space the engine reaches: 27 bits.
const DDR_BITS: u32 = 27;
const CHUNK: usize = 1 << 20;
const CHUNKS: usize = (1 << DDR_BITS) / CHUNK;

/// The reset layout (trx_engine.v:198-204): stream 0 16 MB at `0x7000000`, stream 1 8 MB at `0x6000000`, stream 2
/// 4 MB at `0x5C00000`, as bits 26:4.
const BASE_RESET: [u32; 3] = [0x70_0000, 0x60_0000, 0x5C_0000];
const MASK_RESET: [u32; 3] = [0x0F_FFFF, 0x07_FFFF, 0x03_FFFF];

/// The capture table's reset value (trx_engine.v:186-197): `{count 7, state-bus address 12}` for glue 1, 6510 3,
/// VIC 12, CIA 1 5, CIA 2 5, cartridge 1, 6510 w8+ 7, CIA 1 w8+ 12, CIA 2 w8+ 12: 58 words.
const TAB_RESET: [(u32, u32); 9] =
    [(1, 0), (3, 1 << 7), (12, 2 << 7), (5, 3 << 7), (5, 4 << 7), (1, 7 << 7), (7, 1 << 7 | 8), (12, 3 << 7 | 8), (12, 4 << 7 | 8)];

/// The engine's own DDR2: what lies past guest DDR, and a copy of what it wrote below.
#[derive(Default)]
pub struct Store {
    chunks: Vec<Option<Box<[u8]>>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({} MB)", self.chunks.iter().filter(|c| c.is_some()).count())
    }
}

impl Store {
    pub fn read(&self, addr: u32) -> u8 {
        let a = (addr & ((1 << DDR_BITS) - 1)) as usize;
        self.chunks.get(a / CHUNK).and_then(|c| c.as_ref()).map_or(0, |c| c[a % CHUNK])
    }

    pub fn write(&mut self, addr: u32, val: u8) {
        let a = (addr & ((1 << DDR_BITS) - 1)) as usize;
        if self.chunks.len() < CHUNKS {
            self.chunks.resize_with(CHUNKS, || None);
        }
        self.chunks[a / CHUNK].get_or_insert_with(|| vec![0; CHUNK].into_boxed_slice())[a % CHUNK] = val;
    }

    /// Megabytes allocated.
    pub fn megabytes(&self) -> usize {
        self.chunks.iter().filter(|c| c.is_some()).count()
    }
}

/// An APPLY job the C64 side has not taken yet: records from `start`, `count` of them, downwards (undo) or up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApJob {
    pub start: u32,
    pub count: u32,
    pub undo: bool,
}

/// trx_engine.v's registers and state, the store, and what is waiting for the C64 side.
#[derive(Debug)]
pub struct Engine {
    pub on: bool,
    /// `tap_en`: b0 outside, b1 cartridge RAM, b2 colour RAM.
    taps: u8,
    /// `LOG_BASE`, `LOG_MASK` as bits 26:4.
    base: [u32; 3],
    mask: [u32; 3],
    /// Records written per stream since the restart, mod 2^28.
    pub cnt: [u32; 3],
    /// Markers since the restart.
    pub frame: u32,
    /// Records lost (never in UE2).
    pub dropped: u16,
    /// Since the last marker: records lost, outside writes, REU transfers.
    gap: bool,
    ext: bool,
    reu: bool,
    put_lo: u32,
    tab: [u32; 64],
    tab_idx: u8,
    tab_len: u8,
    /// The mover's registers (byte addresses, 16-byte units).
    src: u32,
    dst: u32,
    len: u32,
    arg: u32,
    ap_start: u32,
    ap_count: u32,
    /// APPLY jobs written but not yet applied by the C64 side; `AP_CMD` b0 reads 1 while there is one.
    pub ap_jobs: VecDeque<ApJob>,
    /// COPY, FILL and APPLY jobs ended (`eng_tgl`, IRQ_CAUSE b1).
    pub done: u64,
    pub store: Store,
    /// Bytes the engine wrote below 64 MB, for guest DDR (written when the access hands DDR back).
    pub guest_out: Vec<(u32, u8)>,
}

impl Default for Engine {
    fn default() -> Self {
        let mut tab = [0u32; 64];
        for (t, (count, addr)) in tab.iter_mut().zip(TAB_RESET) {
            *t = count << 12 | addr;
        }
        Engine {
            on: false,
            taps: 0,
            base: BASE_RESET,
            mask: MASK_RESET,
            cnt: [0; 3],
            frame: 0,
            dropped: 0,
            gap: false,
            ext: false,
            reu: false,
            put_lo: 0,
            tab,
            tab_idx: 0,
            tab_len: 9,
            src: 0,
            dst: 0,
            len: 0,
            arg: 0,
            ap_start: 0,
            ap_count: 0,
            ap_jobs: VecDeque::new(),
            done: 0,
            store: Store::default(),
            guest_out: Vec::new(),
        }
    }
}

/// The record of a stream 1 colour RAM write (trx_engine.v:318-320).
fn colour_rec(addr: u16, old: u8, new: u8, outside: bool, cycle: u64) -> u64 {
    u64::from(outside) << 63
        | (cycle & 0x7FFF) << 48
        | u64::from(new & 0x0F) << 40
        | u64::from(old & 0x0F) << 32
        | 1 << 28
        | u64::from(addr & 0x3FF)
}

impl Engine {
    /// Where record `n` of stream `s` lies (trx_engine.v:621-624): `{base | (off[26:4] & mask), off[3:0]}`, off =
    /// n × 4 for stream 0, n × 8 else.
    pub fn place(&self, s: usize, n: u32) -> u32 {
        let off = u64::from(n) * if s == 0 { 4 } else { 8 };
        ((self.base[s] | ((off >> 4) as u32 & self.mask[s])) << 4) | (off & 15) as u32
    }

    /// One record into its area, as the drain writes it.
    fn put(&mut self, s: usize, rec: u64) {
        let at = self.place(s, self.cnt[s]);
        let bytes = rec.to_le_bytes();
        let n = if s == 0 { 4 } else { 8 };
        for (i, &b) in bytes[..n].iter().enumerate() {
            let a = at + i as u32;
            self.store.write(a, b);
            if (a as usize) < crate::bus::RAM_SIZE {
                self.guest_out.push((a, b));
            }
        }
        self.cnt[s] = (self.cnt[s] + 1) & M28;
    }

    /// A stream 0 record by index, from the store.
    pub fn rec0(&self, n: u32) -> u32 {
        let at = self.place(0, n);
        u32::from_le_bytes(std::array::from_fn(|i| self.store.read(at + i as u32)))
    }

    /// A tap's event (trx_engine.v:295-343, :373-374): a record if the log and its tap are on.
    pub fn tap(&mut self, ev: &TapEvent) {
        let on = self.on;
        match *ev {
            TapEvent::Ram { addr, old, new, outside, .. } => {
                if outside && on && self.taps & TAP_OUTSIDE != 0 {
                    self.ext = true;
                }
                if on && (!outside || self.taps & TAP_OUTSIDE != 0) {
                    self.put(0, u64::from(new) << 24 | u64::from(old) << 16 | u64::from(addr));
                }
            }
            TapEvent::Colour { addr, old, new, outside, cycle } => {
                if outside && on && self.taps & TAP_OUTSIDE != 0 {
                    self.ext = true;
                }
                let tap = self.taps & TAP_COLOUR != 0 && (!outside || self.taps & TAP_OUTSIDE != 0);
                if on && tap {
                    self.put(1, colour_rec(addr, old, new, outside, cycle));
                }
            }
            TapEvent::CartRam { off, old, new, cycle } => {
                if on && self.taps & TAP_CART != 0 {
                    let rec = (cycle & 0x7FFF) << 48 | u64::from(new) << 40 | u64::from(old) << 32 | 2 << 28 | u64::from(off);
                    self.put(1, rec);
                }
            }
            TapEvent::Reu => self.reu = true,
            _ => {}
        }
    }

    /// A frame boundary (trx_engine.v:351-371, :393-452): with the log on the marker F1-F4 and, for a capture, the
    /// chip block over the state bus as it stands right after the snap. `cycle` is the engine counter after the
    /// boundary's edge, `x` and `line` the VIC's position, `intg` the snap is a fetch an interrupt drops.
    pub fn boundary(&mut self, b: Boundary, cycle: u64, x: u16, line: u16, intg: bool, sb: &StateBus) {
        if !b.marker || !self.on {
            return;
        }
        let flags = u64::from(b.snap)
            | u64::from(b.cap) << 1
            | u64::from(self.gap) << 2
            | u64::from(self.ext) << 3
            | u64::from(intg && b.snap) << 4
            | u64::from(self.reu) << 6;
        let gen = u64::from(sb.cap_gen);
        let frame = self.frame;
        self.put(2, 0xF1 << 56 | flags << 48 | gen << 32 | u64::from(frame));
        self.put(2, 0xF2 << 56 | (cycle & 0xFF_FFFF));
        self.put(2, 0xF3 << 56 | u64::from(line & 0x1FF) << 40 | u64::from(x & 0x3FF) << 24);
        self.put(2, 0xF4 << 56 | u64::from(self.cnt[1]) << 28 | u64::from(self.cnt[0]));
        (self.gap, self.ext, self.reu) = (false, false, false);
        self.frame = frame.wrapping_add(1);
        if !b.cap {
            return;
        }
        self.put(2, 0xB1 << 56 | u64::from(self.tab_len) << 48 | gen << 24 | u64::from(frame & 0xFF_FFFF));
        let mut words = Vec::new();
        for &e in &self.tab[..usize::from(self.tab_len)] {
            let (count, addr) = (e >> 12 & 0x7F, e & 0xFFF);
            words.extend((0..count).map(|k| sb.word(((addr + k) & 0xFFF) as u16)));
        }
        for pair in words.chunks(2) {
            let hi = pair.get(1).copied().unwrap_or(0);
            self.put(2, u64::from(hi) << 32 | u64::from(pair[0]));
        }
        self.put(2, 0xB2 << 56 | (words.len() as u64 & 0xFFFF) << 32 | gen << 16 | gen);
    }

    /// `LOG_STAT` (trx_engine.v:528): b0 on; the copy, the mover, the FIFO and the cartridge miss never stand.
    pub fn stat(&self) -> u32 {
        u32::from(self.on)
    }

    /// A page 2 register as a read returns it (trx_engine.v:525-538): write-only ones read 0.
    pub fn read(&self, off: u32) -> u32 {
        match off {
            LOG_STAT => self.stat(),
            FRAME => self.frame,
            DROPPED => u32::from(self.dropped),
            o if o >= LOG_CNT && o <= LOG_CNT + 0x20 && (o - LOG_CNT) % 0x10 == 0 => self.cnt[((o - LOG_CNT) / 0x10) as usize],
            ENG_CMD => 0,
            AP_CMD => u32::from(!self.ap_jobs.is_empty()),
            _ => 0,
        }
    }

    /// A page 2 write the C64 side takes at its clock (trx_engine.v:461-497): `LOG_CTRL`, the areas, `LOG_PUT`, the
    /// capture table.
    pub fn write(&mut self, off: u32, v: u32) {
        match off {
            LOG_CTRL => {
                self.on = v & LOG_ON != 0;
                self.taps = (v >> 1 & 7) as u8;
                if v & LOG_RESTART != 0 {
                    (self.frame, self.dropped, self.gap, self.ext) = (0, 0, false, false);
                    self.cnt = [0; 3];
                }
            }
            o if (LOG_BASE..=LOG_MASK + 0x20).contains(&o) && (o - LOG_BASE) % 0x10 <= 4 => {
                let s = ((o - LOG_BASE) / 0x10) as usize;
                let bits = v >> 4 & 0x7F_FFFF;
                if (o - LOG_BASE) % 0x10 == 0 {
                    self.base[s] = bits;
                } else {
                    self.mask[s] = bits;
                }
            }
            LOG_PUT_LO => self.put_lo = v,
            LOG_PUT_HI => {
                if self.on {
                    self.put(1, u64::from(v) << 32 | u64::from(self.put_lo));
                }
            }
            CAP_TAB_IDX => self.tab_idx = (v & 0x3F) as u8,
            CAP_TAB_DATA => {
                self.tab[usize::from(self.tab_idx)] = (v >> 16 & 0x7F) << 12 | (v & 0xFFF);
                self.tab_idx = (self.tab_idx + 1) & 0x3F;
            }
            CAP_TAB_LEN => self.tab_len = ((v & 0x7F) as u8).min(64),
            _ => {}
        }
    }

    /// The mover's and APPLY's registers, taken at once (they act only through `ENG_CMD` and `AP_CMD`).
    pub fn write_regs(&mut self, off: u32, v: u32) {
        let units = v & 0x07FF_FFF0;
        match off {
            ENG_SRC => self.src = units,
            ENG_DST => self.dst = units,
            ENG_LEN => self.len = units,
            ENG_ARG => self.arg = v,
            AP_START => self.ap_start = v & M28,
            AP_COUNT => self.ap_count = v & M28,
            _ => {}
        }
    }

    /// `AP_CMD` (trx_engine.v:518-522): a job unless one waits or the count is 0; the C64 side applies it.
    pub fn ap_cmd(&mut self, v: u32) -> bool {
        if !self.ap_jobs.is_empty() || self.ap_count == 0 {
            return false;
        }
        self.ap_jobs.push_back(ApJob { start: self.ap_start, count: self.ap_count, undo: v & 1 != 0 });
        true
    }

    /// Apply a job (trx_engine.v:499-523, :654-700): each record's old byte from the newest down, or its new byte
    /// from the oldest up, to `put`.
    pub fn apply(&mut self, job: ApJob, mut put: impl FnMut(u16, u8)) {
        for i in 0..job.count {
            let n = if job.undo { job.start.wrapping_sub(i) } else { job.start.wrapping_add(i) } & M28;
            let r = self.rec0(n);
            put(r as u16, if job.undo { (r >> 16) as u8 } else { (r >> 24) as u8 });
        }
        self.done += 1;
    }

    /// `ENG_CMD` (trx_engine.v:485-488, :677-761): 1 COPY `ENG_LEN` bytes `ENG_SRC` → `ENG_DST`, 2 FILL with
    /// `ENG_ARG`; refused while an APPLY job waits. Guest DDR below 64 MB, the store above.
    pub fn eng_cmd(&mut self, v: u32, ram: &mut [u8]) -> bool {
        let op = v & 0x3;
        if v & 0xF == 0 || !self.ap_jobs.is_empty() {
            return false;
        }
        let space = 1u32 << DDR_BITS;
        for i in 0..self.len {
            let d = (self.dst + i) & (space - 1);
            let b = match op {
                1 => {
                    let s = (self.src + i) & (space - 1);
                    match ram.get(s as usize) {
                        Some(&b) => b,
                        None => self.store.read(s),
                    }
                }
                2 => (self.arg >> (8 * (i & 3))) as u8,
                _ => break,
            };
            match ram.get_mut(d as usize) {
                Some(slot) => *slot = b,
                None => self.store.write(d, b),
            }
        }
        self.done += 1;
        true
    }

    /// The areas as bytes: (base, size) per stream.
    pub fn areas(&self) -> [(u32, u32); 3] {
        std::array::from_fn(|s| (self.base[s] << 4, (self.mask[s] << 4) + 16))
    }

    /// `tap_en` as `LOG_CTRL` b3:1 has it.
    pub fn taps(&self) -> u8 {
        self.taps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c64host::SbShadow;

    fn ram(addr: u16, old: u8, new: u8, outside: bool) -> TapEvent {
        TapEvent::Ram { addr, old, new, outside, cycle: 0 }
    }

    /// trx_engine.v:621-624: record n of stream s at `base | ((n × size) & mask)`, ORed, wrapping in its area; stream
    /// 0's records are four lanes of a 16-byte word.
    #[test]
    fn placement() {
        let mut e = Engine::default();
        assert_eq!([e.place(0, 0), e.place(0, 1), e.place(0, 4)], [0x700_0000, 0x700_0004, 0x700_0010]);
        assert_eq!(e.place(0, 0x40_0000), 0x700_0000, "16 MB of 4-byte records wrap");
        assert_eq!([e.place(1, 1), e.place(2, 2)], [0x600_0008, 0x5C0_0010]);
        assert_eq!(e.place(2, 0x8_0000), 0x5C0_0000, "4 MB of 8-byte records wrap");
        e.write(LOG_BASE, 0x700_0000);
        e.write(LOG_MASK, 0xFF);
        assert_eq!(e.place(0, 64), 0x700_0000, "a 256-byte area");
        e.write(LOG_BASE + 0x10, 0x10_0000);
        assert_eq!(e.place(1, 3), 0x10_0018);
    }

    /// The taps as the RTL gates them (trx_engine.v:295-296, :321, :373): the machine's RAM writes with b0, outside
    /// ones with b1 (and they mark the frame), colour RAM with b3, cartridge RAM with b2; nothing while off; the
    /// restart puts counts and frame to 0.
    #[test]
    fn the_taps_gate_each_source() {
        let mut e = Engine::default();
        e.tap(&ram(0x400, 1, 2, false));
        assert_eq!(e.cnt, [0; 3], "off");
        e.write(LOG_CTRL, LOG_ON);
        e.tap(&ram(0x400, 1, 2, false));
        e.tap(&ram(0x401, 1, 2, true));
        e.tap(&TapEvent::Colour { addr: 5, old: 1, new: 2, outside: false, cycle: 0 });
        e.tap(&TapEvent::CartRam { off: 5, old: 1, new: 2, cycle: 0 });
        assert_eq!(e.cnt, [1, 0, 0], "b0 only: the machine's RAM writes");
        assert_eq!(e.rec0(0), 0x0201_0400, "new, old, addr");
        e.write(LOG_CTRL, 0x00F);
        e.tap(&ram(0x401, 3, 4, true));
        e.tap(&TapEvent::Colour { addr: 0x3FF, old: 0xE, new: 0x1, outside: true, cycle: 0x1_2345 });
        e.tap(&TapEvent::CartRam { off: 0x1FFF, old: 0xAA, new: 0x55, cycle: 7 });
        assert_eq!(e.cnt, [2, 2, 0]);
        assert!(e.ext, "outside writes mark the frame");
        let rec1 = |e: &Engine, n: u32| u64::from_le_bytes(std::array::from_fn(|i| e.store.read(e.place(1, n) + i as u32)));
        assert_eq!(rec1(&e, 0), 1 << 63 | 0x2345 << 48 | 1 << 40 | 0xE << 32 | 1 << 28 | 0x3FF, "outside colour RAM");
        assert_eq!(rec1(&e, 1), 7 << 48 | 0x55 << 40 | 0xAA << 32 | 2 << 28 | 0x1FFF, "cartridge RAM");
        e.write(LOG_PUT_LO, 0x3000_0001);
        e.write(LOG_PUT_HI, 0x0000_00FF);
        assert_eq!(rec1(&e, 2), 0xFF_3000_0001, "LOG_PUT");
        e.write(LOG_CTRL, LOG_RESTART | 0x00F);
        assert_eq!((e.cnt, e.frame, e.ext), ([0; 3], 0, false));
    }

    /// trx_engine.v:351-371, :393-452: F1-F4 with the counts of everything before, B1, the table's words two per
    /// record (the odd last one padded), B2; a 3-entry table written through CAP_TAB_*.
    #[test]
    fn markers_and_blocks() {
        let mut e = Engine::default();
        e.write(LOG_CTRL, LOG_ON);
        let mut shadow = SbShadow::default();
        shadow.cpu = [0x11, 0x0122_E5CF, 0x33];
        shadow.vic = std::array::from_fn(|i| 0x100 + i as u32);
        let sb = StateBus { shadow, cap_gen: 9, ..StateBus::default() };
        e.tap(&ram(0x400, 0, 1, false));
        e.tap(&TapEvent::Reu);
        e.boundary(Boundary { marker: true, snap: true, cap: true }, 0x123_4567, 103, 0, true, &sb);
        let r = |e: &Engine, n: u32| u64::from_le_bytes(std::array::from_fn(|i| e.store.read(e.place(2, n) + i as u32)));
        assert_eq!(r(&e, 0), 0xF1 << 56 | 0b101_0011 << 48 | 9 << 32, "snap, block, intg, REU; CAP_GEN; frame 0");
        assert_eq!(r(&e, 1), 0xF2 << 56 | 0x23_4567);
        assert_eq!(r(&e, 2), 0xF3 << 56 | 103 << 24);
        assert_eq!(r(&e, 3), 0xF4 << 56 | 1, "one stream 0 record before it");
        assert_eq!(r(&e, 4), 0xB1 << 56 | 9 << 48 | 9 << 24);
        assert_eq!(r(&e, 5), 0x11 << 32 | 9 << 16, "glue word 0 (CAP_GEN 9 << 16), then the 6510's w0");
        assert_eq!(r(&e, 4 + 1 + 29), 0xB2 << 56 | 58 << 32 | 9 << 16 | 9, "58 words, never torn");
        assert_eq!((e.cnt[2], e.frame), (35, 1));
        // An outside write in the frame: F1 b3 at the next marker.
        e.write(LOG_CTRL, LOG_ON | 0x2);
        e.tap(&ram(0x402, 0, 1, true));
        e.boundary(Boundary { marker: true, snap: true, cap: false }, 0, 103, 0, false, &sb);
        assert_eq!(r(&e, e.cnt[2] - 4) >> 48 & 0xFF, 0b1001, "snap, outside writes");
        e.write(LOG_CTRL, LOG_ON);
        let base = e.cnt[2];
        // A line-16 boundary: no snap, no block; flags clear again.
        e.boundary(Boundary { marker: true, snap: false, cap: false }, 0x10, 7, 16, false, &sb);
        assert_eq!(r(&e, base), 0xF1 << 56 | 9 << 32 | 2);
        assert_eq!(r(&e, base + 2), 0xF3 << 56 | 16 << 40 | 7 << 24);
        // A 3-entry table: glue, PC word, the CIA 1 port word: 3 words, the last padded.
        for (i, d) in [1u32 << 16, 1 << 16 | (1 << 7 | 1), 1 << 16 | (3 << 7 | 4)].into_iter().enumerate() {
            e.write(CAP_TAB_IDX, i as u32);
            e.write(CAP_TAB_DATA, d);
        }
        e.write(CAP_TAB_LEN, 3);
        let n = e.cnt[2];
        e.boundary(Boundary { marker: true, snap: true, cap: true }, 0, 103, 0, false, &sb);
        assert_eq!(r(&e, n + 4) >> 48 & 0x7F, 3, "B1 entries");
        assert_eq!(r(&e, n + 5), 0x0122_E5CF << 32 | 9 << 16);
        assert_eq!(r(&e, n + 6), 0, "the CIA word, padded");
        assert_eq!(r(&e, n + 7) >> 32 & 0xFFFF, 3);
        // Off: nothing.
        e.write(LOG_CTRL, 0);
        let n = e.cnt[2];
        e.boundary(Boundary { marker: true, snap: true, cap: true }, 0, 103, 0, false, &sb);
        assert_eq!(e.cnt[2], n);
    }

    /// The mover: COPY of an area across its wrap into guest DDR (two jobs, as the app's `fetch`), FILL; a job ends
    /// with `done`.
    #[test]
    fn copy_and_fill() {
        let mut e = Engine::default();
        let mut ram = vec![0u8; 0x10000];
        e.write(LOG_CTRL, LOG_ON);
        for i in 0..0x40_0000u32 + 2 {
            if i < 2 || i >= 0x40_0000 {
                e.tap(&ram_ev(i));
            } else {
                e.cnt[0] += 1;
            }
        }
        fn ram_ev(i: u32) -> TapEvent {
            TapEvent::Ram { addr: i as u16, old: 0, new: (i >> 22) as u8 + 1, outside: false, cycle: 0 }
        }
        // Records 0x3FFFFF.. wrap: the last word of the area and its first.
        e.write_regs(ENG_SRC, 0x7FF_FFF0);
        e.write_regs(ENG_DST, 0x100);
        e.write_regs(ENG_LEN, 16);
        assert!(e.eng_cmd(1, &mut ram));
        e.write_regs(ENG_SRC, 0x700_0000);
        e.write_regs(ENG_DST, 0x110);
        assert!(e.eng_cmd(1, &mut ram));
        assert_eq!(&ram[0x110..0x118], &[0, 0, 0, 2, 1, 0, 0, 2], "the wrapped records, overwritten by the second lap");
        e.write_regs(ENG_ARG, 0xDEAD_BEEF);
        e.write_regs(ENG_DST, 0x200);
        e.write_regs(ENG_LEN, 32);
        assert!(e.eng_cmd(2, &mut ram));
        assert_eq!(&ram[0x200..0x208], &[0xEF, 0xBE, 0xAD, 0xDE, 0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(ram[0x21F], 0xDE);
        assert_eq!(e.done, 3);
        // Into the store and back.
        e.write_regs(ENG_SRC, 0x200);
        e.write_regs(ENG_DST, 0x500_0000);
        assert!(e.eng_cmd(1, &mut ram));
        assert_eq!(e.store.read(0x500_001F), 0xDE);
    }

    /// APPLY (T20 §17.3 as a unit test): random writes logged, undone from the newest down to the start, redone from
    /// the oldest up: RAM back to each state exactly; a count of 0 is refused.
    #[test]
    fn apply_undoes_and_redoes_exactly() {
        let mut e = Engine::default();
        e.write(LOG_CTRL, LOG_ON);
        let mut mem = vec![0u8; 0x10000];
        let mut x: u32 = 0x1234_5678;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let start = mem.clone();
        for _ in 0..5000 {
            let (a, v) = ((rnd() & 0x3FF) as u16, rnd() as u8);
            e.tap(&ram(a, mem[usize::from(a)], v, false));
            mem[usize::from(a)] = v;
        }
        let end = mem.clone();
        e.write_regs(AP_COUNT, 0);
        assert!(!e.ap_cmd(1), "count 0");
        e.write_regs(AP_START, 4999);
        e.write_regs(AP_COUNT, 5000);
        assert!(e.ap_cmd(1));
        assert_eq!(e.read(AP_CMD), 1, "busy until the C64 side applies it");
        assert!(!e.ap_cmd(1), "one job at a time");
        let job = e.ap_jobs.pop_front().unwrap();
        e.apply(job, |a, v| mem[usize::from(a)] = v);
        assert_eq!(mem, start);
        e.write_regs(AP_START, 0);
        e.ap_cmd(0);
        let job = e.ap_jobs.pop_front().unwrap();
        e.apply(job, |a, v| mem[usize::from(a)] = v);
        assert_eq!(mem, end);
        assert_eq!(e.read(AP_CMD), 0);
    }

    /// Registers: write-only ones read 0, the counts `{4'd0, 28}`.
    #[test]
    fn register_reads() {
        let mut e = Engine::default();
        e.write(LOG_CTRL, 0x10F);
        assert_eq!([e.read(LOG_CTRL), e.read(LOG_STAT), e.read(FRAME), e.read(DROPPED)], [0, 1, 0, 0]);
        e.cnt = [M28, 2, 3];
        assert_eq!([e.read(LOG_CNT), e.read(LOG_CNT + 0x10), e.read(LOG_CNT + 0x20)], [M28, 2, 3]);
        for off in [LOG_BASE, LOG_MASK, LOG_PUT_LO, CAP_TAB_IDX, ENG_SRC, ENG_ARG, AP_START] {
            assert_eq!(e.read(off), 0, "{off:#05x}");
        }
        assert_eq!(e.areas(), [(0x700_0000, 0x100_0000), (0x600_0000, 0x80_0000), (0x5C0_0000, 0x40_0000)]);
    }
}
