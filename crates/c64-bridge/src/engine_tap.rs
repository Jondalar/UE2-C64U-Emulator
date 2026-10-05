//! S39 phase A1 / S40: the board's engine on TRX64 (docs/specs/S40-trx-engine.md §4.2, §5, §6.3, §10).
//!
//! The board's C64 core tells its engine about the frame boundary (the snap on line 0, line 16), stops at a position
//! (`STOP_POS`) and shows its chips on the state bus. Here TRX64's run is cut at those points: the run stops at each
//! of them, the bridge looks at the machine there and hands the port events in order. TRX64 runs whole instructions,
//! so a point is reached at the first instruction boundary at or after it when the 6510 runs (gap T2), and exactly
//! when it is held (the chips run cycle by cycle).

use trx64_core::c64_6510core::{IK_IRQ, IK_IRQPEND, IK_NMI, INTERRUPT_DELAY};
use trx64_core::c64re_snapshot::capture_cia;
use ue2_core::c64host::{EngineCfg, SbShadow, Snap, TapEvent};

use crate::trx_ff::{self, CpuState};
use crate::Trx64Backend;

/// The first raster cycle a snap may be on (x ≥ 96, c64_core.v:644).
const SNAP_CYCLE: u16 = 12;
/// The line the boundary falls back to without a snap (c64_core.v:646).
const LINE16: u16 = 16;

/// The engine's view of the run: what it wants, where the next points are, the position hold, the events.
#[derive(Default)]
pub(crate) struct Tap {
    pub cfg: EngineCfg,
    pub events: Vec<TapEvent>,
    /// The engine's cycle counter: the C64 cycles TRX64 ran (c64_core.v `phi2_tick`).
    pub cycles: u64,
    /// `STOP_POS` armed at this cycle of line 0.
    pos: Option<u16>,
    /// The machine stands at the position.
    pub pos_hold: bool,
    /// The clock of the last position hit: a position armed while standing on it comes a frame later.
    hit_clk: Option<u64>,
    /// The next points as C64 clocks.
    next_snap: Option<u64>,
    next_l16: Option<u64>,
    next_pos: Option<u64>,
    /// The frame length and the clock the points were planned for: a new row or a clock that went back (a reset, a
    /// restore) plans them again.
    plan: Option<(u64, u64)>,
    /// On line 0 past cycle 12 the snap's conditions did not hold: one instruction at a time until they do.
    stepping: bool,
}

impl Tap {
    /// Whether the run has to stop anywhere for the engine.
    pub fn stops(&self) -> bool {
        self.cfg.boundaries || self.pos.is_some()
    }
}

impl Trx64Backend {
    /// Cycles in a frame of the row the C64 runs.
    fn frame_cycles(&self) -> u64 {
        let t = self.m.timing();
        u64::from(t.lines_per_frame) * u64::from(t.cycles_per_line)
    }

    /// The VIC's raster position: (line, cycle in the line, 0-based).
    pub(crate) fn raster(&self) -> (u16, u16) {
        (self.m.vic.raster_line, self.m.vic.raster_cycle)
    }

    /// Cycles from now until the VIC is at `cycle` of `line`, 0 when it is there now.
    fn cycles_until(&self, line: u16, cycle: u16) -> u64 {
        let cpl = u64::from(self.m.timing().cycles_per_line);
        let f = self.frame_cycles().max(1);
        let (l, c) = self.raster();
        let now = (u64::from(l) * cpl + u64::from(c)) % f;
        let at = (u64::from(line) * cpl + u64::from(cycle)) % f;
        (at + f - now) % f
    }

    /// The next time the VIC is at `cycle` of `line`, a whole frame on when it is there now.
    fn next_at(&self, line: u16, cycle: u16) -> u64 {
        let d = self.cycles_until(line, cycle);
        self.m.c64_core.clk + if d == 0 { self.frame_cycles() } else { d }
    }

    /// Plan the points from the machine as it stands: the snap window, line 16, the position.
    fn tap_replan(&mut self) {
        let clk = self.m.c64_core.clk;
        let b = self.tap.cfg.boundaries;
        self.tap.next_snap = b.then(|| clk + self.cycles_until(0, SNAP_CYCLE));
        self.tap.next_l16 = b.then(|| clk + self.cycles_until(LINE16, 0));
        self.tap_replan_pos();
        self.tap.plan = Some((self.frame_cycles(), clk));
        self.tap.stepping = false;
    }

    fn tap_replan_pos(&mut self) {
        let clk = self.m.c64_core.clk;
        self.tap.next_pos = self.tap.pos.map(|c| {
            let d = self.cycles_until(0, c);
            // Standing on the position it was just stopped at: the edge has passed, the next one is a frame on.
            clk + if d == 0 && self.tap.hit_clk == Some(clk) { self.frame_cycles() } else { d }
        });
    }

    /// The points still fit the machine: same row, the clock not gone back.
    fn tap_check_plan(&mut self) {
        let clk = self.m.c64_core.clk;
        match self.tap.plan {
            Some((f, at)) if f == self.frame_cycles() && clk >= at => self.tap.plan = Some((f, clk)),
            _ => self.tap_replan(),
        }
    }

    /// What the engine wants from now on (`C64Backend::set_engine`).
    pub(crate) fn tap_set_engine(&mut self, cfg: EngineCfg) {
        if cfg == self.tap.cfg {
            return;
        }
        self.tap.cfg = cfg;
        self.tap_replan();
    }

    /// `STOP_POS` armed or disarmed (`C64Backend::set_stop_pos`).
    pub(crate) fn tap_set_pos(&mut self, cycle: Option<u8>) {
        self.tap.pos = cycle.map(u16::from);
        self.tap_replan_pos();
    }

    /// `STOP_POS` b9: the hold goes; the C64 continues from here, as after the machine pause.
    pub(crate) fn tap_release_pos(&mut self) {
        if !self.tap.pos_hold {
            return;
        }
        self.tap.pos_hold = false;
        if !self.paused {
            self.reanchor();
        }
    }

    /// Up to cycle `target`, stopping at each point the engine wants on the way. Returns early when the position
    /// hold stands.
    pub(crate) fn run_tapped(&mut self, target: u64) {
        loop {
            if self.tap.pos_hold {
                return;
            }
            self.tap_check_plan();
            self.tap_boundary();
            if self.tap.pos_hold {
                return;
            }
            let clk = self.m.c64_core.clk;
            if clk >= target {
                return;
            }
            let mut end = target;
            for n in [self.tap.next_snap, self.tap.next_l16, self.tap.next_pos].into_iter().flatten() {
                if n > clk {
                    end = end.min(n);
                }
            }
            if self.tap.stepping {
                end = clk + 1;
            }
            self.run_to_plain(end);
            if self.m.c64_core.clk == clk {
                // The C64 waits for the firmware (UCI): nothing ran.
                return;
            }
        }
    }

    /// The points reached at this boundary (c64_core.v:638-674): the position stop first (it stands for a snap
    /// edge, :662), then line 16 and the snap.
    fn tap_boundary(&mut self) {
        let clk = self.m.c64_core.clk;
        if self.tap.next_pos.is_some_and(|p| clk >= p) {
            self.tap.next_pos = None;
            self.tap.pos = None;
            self.tap.pos_hold = true;
            self.tap.hit_clk = Some(clk);
            self.tap.events.push(TapEvent::PosHit);
            return;
        }
        if !self.tap.cfg.boundaries {
            return;
        }
        let (line, cycle) = self.raster();
        if self.tap.next_l16.is_some_and(|l| clk >= l) {
            self.tap.events.push(TapEvent::Line16 { cycle: self.tap.cycles, x: 8 * cycle + 7 });
            self.tap.next_l16 = Some(self.next_at(LINE16, 0));
        }
        if self.tap.next_snap.is_some_and(|s| clk >= s) {
            // The 6510 held (C64_STOP, reset) makes no opcode fetch: no snap in this frame.
            let held = self.stopped || self.reset_held;
            if line == 0 && cycle >= SNAP_CYCLE && !held {
                if self.snap_conditions() {
                    let snap = self.snap_now();
                    self.tap.events.push(TapEvent::Snap(Box::new(snap)));
                    self.tap.stepping = false;
                    self.tap.next_snap = Some(self.next_at(0, SNAP_CYCLE));
                } else {
                    self.tap.stepping = true;
                }
            } else {
                self.tap.stepping = false;
                self.tap.next_snap = Some(self.next_at(0, SNAP_CYCLE));
            }
        }
    }

    /// The snap's conditions TRX64 can tell (c64_core.v:643-644): the REU idle. The VIC's (border flip-flops, sprite
    /// pixels, its access in the cycle) are taken as met (gap T3).
    fn snap_conditions(&self) -> bool {
        !self.m.reu().is_some_and(|r| r.dma_pending())
    }

    /// The snap at this boundary: the board's edge is the fetch after it, so x and the shadows are of the position
    /// before the fetch and the cycle count is the one before it (S40 §4.2).
    fn snap_now(&self) -> Snap {
        let (line, cycle) = self.raster();
        Snap {
            cycle: self.tap.cycles,
            x: 8 * cycle + 7,
            line,
            intg: self.irq_due() || self.nmi_due(),
            shadow: self.shadow_now(),
            live: self.live_now(),
        }
    }

    /// Whether the 6510 takes an IRQ at the start of its next instruction: `interrupt_check_irq_delay` and the gate of
    /// `do_interrupt` (c64_6510core.rs:448-461, :2513-2516) on the state as it stands. TRX64 keeps them private.
    pub(crate) fn irq_due(&self) -> bool {
        const DELAYS_INTERRUPT: u32 = 1 << 8;
        const DISABLES_IRQ: u32 = 1 << 9;
        const ENABLES_IRQ: u32 = 1 << 10;
        let (int, op) = (&self.m.c64_int, self.m.c64_core.last_opcode_info);
        let i_flag = self.m.c64_core.status() & 0x04 != 0;
        let gate = int.global_pending_int & (IK_IRQ | IK_IRQPEND) != 0 && (!i_flag || op & DISABLES_IRQ != 0);
        let delay = INTERRUPT_DELAY + u64::from(op & DELAYS_INTERRUPT != 0);
        gate && int.irq_delay_cycles >= delay && op & ENABLES_IRQ == 0
    }

    /// `$01` as the 6510 reads it (TRX64's `cpu_port_data_read`, full.rs:363-382): the pulled-up lines 0-2 and 4 high,
    /// 5 low as an input, 6 and 7 decayed.
    fn port_read(&self) -> u8 {
        let (dir, data) = (self.m.port_dir, self.m.port_data);
        let mut v = (data | !dir) & ((data & dir) | 0x17);
        for bit in [0x20, 0x40, 0x80] {
            if dir & bit == 0 {
                v &= !bit;
            }
        }
        v
    }

    /// Words 1.3-1.5: `{IB, opcode at PC, PC}` (UE2 always stands at an instruction boundary), `{S, Y, X, A}`, `{P}`.
    pub(crate) fn live_now(&self) -> [u32; 3] {
        let c = &self.m.c64_core;
        [
            1 << 24 | u32::from(self.m.read_full(c.reg_pc)) << 16 | u32::from(c.reg_pc),
            u32::from_le_bytes([c.reg_a, c.reg_x, c.reg_y, c.reg_sp]),
            u32::from(c.status()),
        ]
    }

    /// The state bus as the machine stands (S39 §3.2, S40 §5): the visible words and the flip-flop images.
    pub(crate) fn shadow_now(&self) -> SbShadow {
        let c = &self.m.c64_core;
        let io = |a: u16| self.m.peek_lens(a, "io");
        let vic: [u8; 48] = std::array::from_fn(|r| io(0xD000 + r as u16));
        let cia_read: [[u8; 16]; 2] = [std::array::from_fn(|r| io(0xDC00 + r as u16)), std::array::from_fn(|r| io(0xDD00 + r as u16))];
        let cia_regs = [self.m.cia1.c_cia, self.m.cia2.c_cia];
        let (present, exrom, game) = ue2_core::c64host::C64Backend::cart_lines(self);
        let bank = if self.slot.with(|s| s.physical().is_some()) { 0 } else { self.cart.with(|l| l.state_bus_bank()) };
        let regs = (c.reg_a, c.reg_x, c.reg_y, c.reg_sp, c.status(), c.reg_pc);
        let mut sh = SbShadow::visible(
            regs,
            (self.m.port_dir, self.m.port_data, self.port_read()),
            &vic,
            cia_read,
            cia_regs,
            (present, exrom, game),
            u32::from(bank),
        );
        let op = c.last_opcode_info as u8;
        let at = c.last_opcode_addr;
        let int = &self.m.c64_int;
        sh.cpuf = trx_ff::encode_cpu(&CpuState {
            a: c.reg_a,
            x: c.reg_x,
            y: c.reg_y,
            s: c.reg_sp,
            p: c.status(),
            pc: c.reg_pc,
            ir: op,
            op1: self.m.read_full(at.wrapping_add(1)),
            op2: self.m.read_full(at.wrapping_add(2)),
            din: self.m.read_full(c.reg_pc),
            pdir: self.m.port_dir,
            pout: self.m.port_data,
            intg: self.irq_due() || self.nmi_due(),
            irq: int.nirq > 0,
            nmi: int.nnmi > 0,
            nmi_edge: int.global_pending_int & IK_NMI != 0,
        });
        sh.ciaf = [
            trx_ff::encode_cia(&capture_cia(&self.m.cia1), &cia_read[0]),
            trx_ff::encode_cia(&capture_cia(&self.m.cia2), &cia_read[1]),
        ];
        sh
    }
}

#[cfg(test)]
mod tests {
    use ue2_core::c64host::C64Backend;

    use super::*;
    use crate::tests::{booted, run_ms};

    fn snaps(events: &[TapEvent]) -> Vec<&Snap> {
        events.iter().filter_map(|e| if let TapEvent::Snap(s) = e { Some(&**s) } else { None }).collect()
    }

    fn line16s(events: &[TapEvent]) -> usize {
        events.iter().filter(|e| matches!(e, TapEvent::Line16 { .. })).count()
    }

    /// S40 §4.2: with the boundaries on, one snap a frame at the first instruction boundary on line 0 from cycle 12,
    /// one line 16; F2's steps are a frame give or take the instruction the snap waited for; the shadows are the
    /// machine there (the BASIC idle loop at READY).
    #[test]
    fn a_snap_a_frame_on_line_0_from_cycle_12() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_engine(EngineCfg { boundaries: true, records: false });
        let (clk0, cycles0) = (c64.m.c64_core.clk, c64.engine_cycles());
        run_ms(&mut c64, &mut now, 200);
        assert_eq!(c64.engine_cycles() - cycles0, c64.m.c64_core.clk - clk0, "the counter counts what ran");
        let ev = c64.take_tap();
        let s = snaps(&ev);
        assert!((9..=11).contains(&s.len()), "{} snaps in 200 ms", s.len());
        assert!((9..=11).contains(&line16s(&ev)));
        for w in s.windows(2) {
            let d = w[1].cycle - w[0].cycle;
            assert!(d.abs_diff(19_656) <= 8, "a frame between snaps: {d}");
        }
        for snap in &s {
            let cycle = (snap.x - 7) / 8;
            assert_eq!((snap.line, snap.x % 8), (0, 7));
            assert!((12..20).contains(&cycle), "the first boundary from cycle 12: {cycle}");
            let pc = snap.shadow.cpu[1] & 0xFFFF;
            assert!((0xE5CD..=0xE5D5).contains(&pc), "READY's loop: {pc:#06x}");
            assert_eq!(snap.shadow.cpu[1] >> 24, 1, "sync");
            assert_eq!(trx_ff::get(&snap.shadow.cpuf, trx_ff::cpu::PC, 16), u64::from(pc), "the image's PC");
        }
        // Off: no events.
        c64.set_engine(EngineCfg::default());
        run_ms(&mut c64, &mut now, 100);
        assert!(c64.take_tap().is_empty());
    }

    /// c64_core.v:643: a held 6510 makes no opcode fetch, so no snap; line 16 still comes.
    #[test]
    fn a_held_6510_gives_line_16_and_no_snap() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_engine(EngineCfg { boundaries: true, records: false });
        c64.set_stopped(true);
        run_ms(&mut c64, &mut now, 100);
        let ev = c64.take_tap();
        assert!(snaps(&ev).is_empty());
        assert!((4..=6).contains(&line16s(&ev)));
        c64.set_stopped(false);
        run_ms(&mut c64, &mut now, 100);
        assert!(!snaps(&c64.take_tap()).is_empty());
    }

    /// S40 §6.3: with the 6510 running the stop is the first boundary on line 0 at or after the cycle (T2); held,
    /// the chips stop exactly there. The hold stands the whole machine until the release.
    #[test]
    fn the_stop_position_running_and_held() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_stop_pos(Some(12));
        run_ms(&mut c64, &mut now, 30);
        assert_eq!(c64.take_tap(), [TapEvent::PosHit]);
        let (line, cycle) = c64.raster();
        assert_eq!(line, 0);
        assert!((12..20).contains(&cycle), "at or just after cycle 12: {cycle}");
        let clk = c64.m.c64_core.clk;
        run_ms(&mut c64, &mut now, 10);
        assert_eq!(c64.m.c64_core.clk, clk, "the hold stands");
        // Held 6510, armed while standing on line 0: the chips run a frame to cycle 30 and stop on it.
        c64.set_stopped(true);
        c64.set_stop_pos(Some(30));
        c64.release_pos();
        run_ms(&mut c64, &mut now, 30);
        assert_eq!(c64.take_tap(), [TapEvent::PosHit]);
        assert_eq!(c64.raster(), (0, 30), "exact with the 6510 held");
        assert!(c64.m.c64_core.clk - clk < 19_656, "within the frame");
        // Armed again at the cycle it stands on: a whole frame on.
        let clk = c64.m.c64_core.clk;
        c64.set_stop_pos(Some(30));
        c64.release_pos();
        run_ms(&mut c64, &mut now, 30);
        assert_eq!((c64.raster(), c64.m.c64_core.clk - clk), ((0, 30), 19_656));
        c64.release_pos();
        c64.set_stopped(false);
        let clk = c64.m.c64_core.clk;
        run_ms(&mut c64, &mut now, 10);
        assert!(c64.m.c64_core.clk > clk, "released: it runs");
    }
}
