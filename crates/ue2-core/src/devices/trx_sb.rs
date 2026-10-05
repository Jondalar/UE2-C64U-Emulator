//! S39 phase A1 / S40: the board's state bus (`c64_core.v:575-766`) and the gate's capture control (`trx_sys.v`,
//! page 0 0x104-0x117), one model for the debugger and the engine. Specs: docs/specs/S39-trxmon.md §1.2, §3.2,
//! docs/specs/S40-trx-engine.md §4.2, §6.3.
//!
//! The words a state-bus read returns are shadows: loaded at a frame's snap when a capture is due, and kept following
//! the machine while it is halted (the pause or the position hold). Word 0.0 b0 and words 1.3-1.5 are live. Staged
//! writes go to the hidden-state words 8 and up of chips 1, 3, 4 while the machine is halted; `SB_LOAD` makes them
//! load at the next edge that opens the gate.

use crate::c64host::{SbShadow, Snap};

/// `SB_LOAD` bits (trx_sys.v:47-48): b1 CPU, b3 CIA 1, b4 CIA 2.
pub const LOAD_CPU: u8 = 0x02;
pub const LOAD_CIA1: u8 = 0x08;
pub const LOAD_CIA2: u8 = 0x10;

/// The first raster cycle a snap may end (x ≥ 96, c64_core.v:644).
pub const SNAP_CYCLE: u8 = 12;

/// The state bus and the capture control, as the C64 port keeps them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateBus {
    pub shadow: SbShadow,
    /// Words 1.3, 1.4, 1.5: `{IB, opcode at IPC, IPC}` and the settled registers, live (as of the last sync).
    pub live: [u32; 3],
    /// `CAP_CTRL` b7:0 as it acts on the machine: a capture every n frames, 0 none.
    pub cap_every: u8,
    /// `CAP_STAT`: captures and halts since power on (`cap_gen`), and frames of the cadence whose snap did not come.
    pub cap_gen: u16,
    pub cap_nosnap: u16,
    /// c64_core.v:642: the snap is armed for this frame, this frame captures, the cadence counter.
    pub armed: bool,
    pub cap_this: bool,
    pub fcnt: u8,
    /// The machine is halted: the pause or the position hold (`halted_r`).
    pub halted: bool,
    /// `STOP_POS`: armed, the cycle, the hold (c64_core.v:374-388).
    pub pos_arm: bool,
    pub pos_cyc: u8,
    pub pos_hold: bool,
    /// The staged hidden-state words of chips 1, 3, 4 (`g_cpu`, `g_cia1`, `g_cia2`).
    pub staged_cpu: [u32; 7],
    pub staged_cia: [[u32; 12]; 2],
    /// `SB_STATUS` b1: a staged write was refused.
    pub err: bool,
    /// `SB_LOAD` as armed, until the next opening edge takes it.
    pub ldarm: u8,
    /// Captures and halt starts since power on (`cap_tgl`), for `IRQ_CAUSE` b3.
    pub cap_events: u64,
}

/// What a frame boundary candidate turned out to be (c64_core.v:648-674).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Boundary {
    /// `mk`: the engine makes a marker here.
    pub marker: bool,
    /// `mk_snap`, `mk_cap`.
    pub snap: bool,
    pub cap: bool,
}

/// CIA register r of a 16-register file in byte r % 4 of word r / 4 (c64_core.v:713-716).
fn reg_word(regs: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(std::array::from_fn(|j| regs.get(4 * n + j).copied().unwrap_or(0)))
}

impl SbShadow {
    /// The visible words from register files: chip 1 w0-w2, the VIC's `$D000-$D02F` as read, each CIA's registers
    /// as read and its `{DDRB, DDRA, PRB, PRA}`.
    #[allow(clippy::too_many_arguments)]
    pub fn visible(
        regs: (u8, u8, u8, u8, u8, u16),
        port: (u8, u8, u8),
        vic: &[u8; 48],
        cia_read: [[u8; 16]; 2],
        cia_regs: [[u8; 16]; 2],
        cart: (bool, bool, bool),
        bank: u32,
    ) -> SbShadow {
        let (a, x, y, s, p, pc) = regs;
        let (pdir, pout, pread) = port;
        let cia = |i: usize| -> [u32; 5] {
            let r = &cia_regs[i];
            std::array::from_fn(|n| match n {
                4 => u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
                _ => reg_word(&cia_read[i], n),
            })
        };
        SbShadow {
            cart: u8::from(cart.0) << 2 | u8::from(cart.1) << 1 | u8::from(cart.2),
            cpu: [
                u32::from_le_bytes([a, x, y, s]),
                1 << 24 | u32::from(p) << 16 | u32::from(pc),
                u32::from(pread) << 16 | u32::from(pout) << 8 | u32::from(pdir),
            ],
            vic: std::array::from_fn(|n| reg_word(vic, n)),
            cia: [cia(0), cia(1)],
            bank,
            ..SbShadow::default()
        }
    }
}

impl StateBus {
    /// `sb_frozen`: staged writes are taken only while the machine is halted (c64_core.v:708).
    pub fn frozen(&self) -> bool {
        self.halted
    }

    /// A state-bus read of `{chip b11:7, word b6:0}` (c64_core.v:751-764).
    pub fn word(&self, addr: u16) -> u32 {
        let (chip, w) = (addr >> 7 & 0x1F, usize::from(addr & 0x7F));
        let s = &self.shadow;
        match chip {
            0 if w == 0 => u32::from(self.cap_gen) << 16 | u32::from(s.cart) << 1 | u32::from(self.halted),
            1 => match w {
                0..=2 => s.cpu[w],
                3..=5 => self.live[w - 3],
                8..=14 => s.cpuf[w - 8],
                _ => 0,
            },
            2 if w < 12 => s.vic[w],
            3 | 4 => {
                let i = usize::from(chip - 3);
                match w {
                    0..=4 => s.cia[i][w],
                    8..=19 => s.ciaf[i][w - 8],
                    _ => 0,
                }
            }
            7 if w == 0 => s.bank,
            _ => 0,
        }
    }

    /// A staged `SB_DATA` write to `addr` (trx_sys.v:317-323, c64_core.v:689-695): only the hidden-state words of
    /// chips 1, 3, 4, and only while the machine is halted; otherwise `SB_STATUS` b1.
    pub fn stage(&mut self, addr: u16, v: u32) {
        if !self.frozen() {
            self.err = true;
            return;
        }
        let (chip, w) = (addr >> 7 & 0x1F, usize::from(addr & 0x7F));
        match (chip, w) {
            (1, 8..=14) => self.staged_cpu[w - 8] = v,
            (3, 8..=19) => self.staged_cia[0][w - 8] = v,
            (4, 8..=19) => self.staged_cia[1][w - 8] = v,
            _ => {}
        }
    }

    /// A capture or a halt start: `CAP_GEN` + 1 and `IRQ_CAUSE` b3 (c64_core.v:675-678).
    fn cap_event(&mut self) {
        self.cap_gen = self.cap_gen.wrapping_add(1);
        self.cap_events += 1;
    }

    /// The machine came to a halt (pause or position hold) or left it. A halt start counts as a capture; the caller
    /// then loads the shadows from the machine.
    pub fn set_halted(&mut self, halted: bool) -> bool {
        let start = halted && !self.halted;
        self.halted = halted;
        if start {
            self.cap_event();
        }
        start
    }

    /// Line 16 (c64_core.v:646, :653, :663-674): a boundary without a snap if the frame was still armed, then the
    /// frame's arming and cadence. `log_on` is `LOG_CTRL` b0.
    pub fn line16(&mut self, log_on: bool) -> Boundary {
        let b = Boundary { marker: self.armed, snap: false, cap: false };
        if self.armed && self.cap_this {
            self.cap_nosnap = self.cap_nosnap.wrapping_add(1);
        }
        self.armed = log_on || self.cap_every != 0;
        self.cap_this = false;
        if self.cap_every != 0 {
            if self.fcnt.wrapping_add(1) >= self.cap_every {
                self.fcnt = 0;
                self.cap_this = true;
            } else {
                self.fcnt += 1;
            }
        }
        b
    }

    /// A snap candidate (the first instruction boundary on line 0 from cycle 12 where the conditions hold, the
    /// machine running): the snap if the frame is armed; a capture loads `snap`'s shadows.
    pub fn snap(&mut self, snap: &Snap) -> Boundary {
        if !self.armed || self.halted {
            return Boundary::default();
        }
        self.armed = false;
        let cap = self.cap_this;
        if cap {
            self.shadow = snap.shadow.clone();
            self.live = snap.live;
            self.cap_event();
        }
        Boundary { marker: true, snap: true, cap }
    }

    /// The machine stood at the stop position (c64_core.v:378-381, :662): the hold, the arm gone, no snap in this
    /// frame.
    pub fn pos_hit(&mut self) {
        self.pos_hold = true;
        self.pos_arm = false;
        self.armed = false;
    }

    /// The staged words a load takes, by `SB_LOAD`'s bits.
    pub fn load_words(&self, mask: u8) -> SbLoad {
        SbLoad {
            cpu: (mask & LOAD_CPU != 0).then_some(self.staged_cpu),
            cia: [
                (mask & LOAD_CIA1 != 0).then_some(self.staged_cia[0]),
                (mask & LOAD_CIA2 != 0).then_some(self.staged_cia[1]),
            ],
        }
    }
}

/// The staged hidden-state words of a load (S40 §6.3): chip 1's seven, each CIA's twelve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SbLoad {
    pub cpu: Option<[u32; 7]>,
    pub cia: [Option<[u32; 12]>; 2],
}

impl SbLoad {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.cia.iter().all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with(pc: u16) -> Snap {
        let mut shadow = SbShadow::default();
        shadow.cpu[1] = 1 << 24 | u32::from(pc);
        Snap { cycle: 0, x: 103, line: 0, intg: false, shadow, live: [7, 8, 9] }
    }

    /// c64_core.v:663-674: armed at line 16 while the log or the cadence is on; a cadence of n captures every n-th
    /// frame; a frame of the cadence without a snap counts in `CAP_STAT`'s high half.
    #[test]
    fn line_16_arms_and_the_cadence_captures_every_nth_frame() {
        let mut sb = StateBus { cap_every: 3, ..StateBus::default() };
        let mut caps = Vec::new();
        for frame in 0..9 {
            sb.line16(false);
            let b = sb.snap(&snap_with(frame));
            assert!(b.marker && b.snap, "every armed frame snaps");
            caps.push(b.cap);
        }
        assert_eq!(caps, [false, false, true, false, false, true, false, false, true]);
        assert_eq!(sb.cap_gen, 3);
        assert_eq!(sb.word(1 << 7 | 1) & 0xFFFF, 8, "the last capture's PC");
        assert_eq!(sb.word(1 << 7 | 4), 8, "and its live words");
        // A cadence frame without a snap, then line 16 again.
        sb.line16(false);
        sb.line16(false);
        sb.line16(false);
        let b = sb.line16(false);
        assert_eq!((b.marker, sb.cap_nosnap), (true, 1), "the unsnapped cadence frame: a line-16 boundary, counted");
        // Neither log nor cadence: not armed, no boundary.
        sb.cap_every = 0;
        sb.line16(false);
        assert!(!sb.line16(false).marker);
        assert!(!sb.snap(&snap_with(0)).marker);
        assert!(sb.line16(true).marker == false && sb.armed, "the log alone arms");
    }

    /// The word map (c64_core.v:751-764): chip 0 with `CAP_GEN` and the live halted bit, the CPU words, VIC, CIAs,
    /// the cartridge, the hidden words; everything else 0.
    #[test]
    fn the_word_map() {
        let vic: [u8; 48] = std::array::from_fn(|i| i as u8);
        let cia_read = [[0x11; 16], std::array::from_fn(|i| 0x20 + i as u8)];
        let cia_regs = [[0; 16], std::array::from_fn(|i| 0x40 + i as u8)];
        let mut sh = SbShadow::visible((1, 2, 3, 4, 0x22, 0xE5CF), (0x2F, 0x37, 0x17), &vic, cia_read, cia_regs, (true, false, true), 0x0102);
        sh.cpuf[6] = 0x4000;
        sh.ciaf[1][11] = 0x55;
        let sb = StateBus { shadow: sh, cap_gen: 7, halted: true, live: [0xA, 0xB, 0xC], ..StateBus::default() };
        assert_eq!(sb.word(0), 7 << 16 | 0b101 << 1 | 1);
        assert_eq!([sb.word(0x80), sb.word(0x81), sb.word(0x82)], [0x0403_0201, 0x0122_E5CF, 0x0017_372F]);
        assert_eq!([sb.word(0x83), sb.word(0x85), sb.word(0x8E), sb.word(0x8F)], [0xA, 0xC, 0x4000, 0]);
        assert_eq!([sb.word(0x100), sb.word(0x10B), sb.word(0x10C)], [0x0302_0100, 0x2F2E_2D2C, 0]);
        assert_eq!([sb.word(0x200 | 1), sb.word(0x200 | 4), sb.word(0x200 | 19)], [0x2726_2524, 0x4342_4140, 0x55]);
        assert_eq!([sb.word(0x380), sb.word(0x381), sb.word(0x180), sb.word(0x300)], [0x0102, 0, 0x1111_1111, 0]);
    }

    /// Staging only while halted, only hidden-state words; the load takes the chips its bits name.
    #[test]
    fn staging_and_the_load_words() {
        let mut sb = StateBus::default();
        sb.stage(1 << 7 | 8, 1);
        assert!(sb.err, "refused while running");
        sb.err = false;
        sb.set_halted(true);
        sb.stage(1 << 7 | 8, 0x11);
        sb.stage(3 << 7 | 19, 0x33);
        sb.stage(4 << 7 | 8, 0x44);
        sb.stage(1 << 7 | 1, 0xFF);
        assert!(!sb.err);
        let l = sb.load_words(LOAD_CPU | LOAD_CIA2);
        assert_eq!((l.cpu.map(|w| w[0]), l.cia[0], l.cia[1].map(|w| w[0])), (Some(0x11), None, Some(0x44)));
        assert_eq!(sb.load_words(LOAD_CIA1).cia[0].map(|w| w[11]), Some(0x33));
        assert!(sb.load_words(0).is_empty());
        assert_eq!(sb.word(1 << 7 | 1), 0, "a staged write to a visible word does nothing");
    }

    /// A halt start counts as a capture; the position stop disarms the frame's snap.
    #[test]
    fn halts_and_the_position_stop() {
        let mut sb = StateBus { cap_every: 1, ..StateBus::default() };
        assert!(sb.set_halted(true));
        assert!(!sb.set_halted(true), "once per halt");
        assert_eq!((sb.cap_gen, sb.cap_events), (1, 1));
        sb.set_halted(false);
        sb.line16(false);
        sb.pos_hit();
        assert!(sb.pos_hold && !sb.pos_arm);
        assert!(!sb.snap(&snap_with(1)).marker, "a position stop stands for the snap");
        assert!(!sb.line16(false).marker, "and no line-16 marker for that frame");
    }
}
