//! S39: TRX64-Ultimate's debug block (`trx_dbg.v`, `drv_dbg.v`) on TRX64's run (docs/specs/S39-trxmon.md §3.3-§3.5).
//!
//! The register model is ue2-core's `trx_dbg::Dbg`, shared with the IO block and the C64 port. Here the run looks
//! for its stops. TRX64 runs whole instructions, so UE2's edges are its instruction boundaries (S39 §5 T1): a
//! boundary whose next `execute` takes no interrupt is an IB. At every boundary that may stop (an exec bitmap bit,
//! an IB unit's range, a pause or an instruction step: every boundary) the run halts through TRX64's exec watch and
//! the boundary is looked at; read and write units watch their ranges through `on_access` and stop at the next
//! boundary; the comparator and a step of cycles cut the run's budget. A stop holds the whole machine, as the pause
//! does (S37), until RUN or a step.

use trx64_core::{AccessCtx, BusKind, Observer};
use ue2_core::devices::trx_dbg::{self as td, DbgLink, Record, Step, Unit};

use crate::Trx64Backend;

/// Bus cycles an instruction step may take at most before it is left to the running machine (7 cycles, badlines,
/// sprites and an interrupt sequence on the way).
const STEP_CYCLES_PER_INSTR: u64 = 128;

/// An access unit's stop action in the last run: the unit, the access's address and byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccStop {
    unit: u8,
    addr: u16,
    data: u8,
}

/// The debugger as the run sees it: the model's configuration as of its generation, the tables built from it, and
/// what the last runs found.
#[derive(Default)]
pub(crate) struct DbgRun {
    pub link: Option<DbgLink>,
    gen: Option<u64>,
    /// The machine stands for the debugger (its hold, no step).
    pub held: bool,
    /// OWNER as last seen.
    own: bool,
    units: [Unit; 2],
    bmp_en: bool,
    bmp: Option<Box<[u32; 2048]>>,
    /// Boundaries to look at: the bitmap's bits and the IB units' ranges.
    exec: Option<Box<[u8; 0x1_0000]>>,
    /// Every boundary: a pause, an instruction step, the drive's comparator.
    every: bool,
    /// The read and write units' ranges.
    pub access: Option<Box<[u8; 0x1_0000]>>,
    /// The engine count at which CYC_CMP fires.
    cmp_at: Option<u64>,
    step: Option<Step>,
    /// The step's start: the engine count, and the IBs counted since.
    step_from: u64,
    step_ibs: u16,
    pend_ib: bool,
    pend_cb: bool,
    drv_cmp: Option<u16>,
    /// The drive's PC at the last boundary looked at: the comparator fires when it comes to the address.
    drv_last: Option<u16>,
    /// The boundary already looked at: a run that resumes there does not stop there again.
    eval_clk: Option<u64>,
    /// The opcode of the instruction retired before this boundary (PREV_RET of an IB unit: trx_dbg.v `op`).
    prev_op: u8,
    acc: Option<AccStop>,
    /// Instructions retired since power on (INSTRS).
    pub instrs: u64,
    /// The merged watch tables of the last runs, and what they were built from.
    pub exec_cache: Option<(Box<[u8; 0x1_0000]>, (usize, u64))>,
    pub access_cache: Option<(Box<[u8; 0x1_0000]>, (usize, u64))>,
}

impl DbgRun {
    /// A boundary at `pc` is one the run has to look at.
    fn exec_at(&self, pc: u16) -> bool {
        self.every || self.exec.as_ref().is_some_and(|t| t[usize::from(pc)] != 0)
    }

    /// The run of a backend with the debug block `link` (or none).
    pub fn with_link(link: Option<DbgLink>) -> Self {
        DbgRun { link, ..DbgRun::default() }
    }

    /// The boundaries the exec watch stops at for the debugger (the bitmap, the IB units' ranges).
    pub fn exec_table(&self) -> Option<&[u8; 0x1_0000]> {
        self.exec.as_deref()
    }

    /// The generation the tables were built from (for the merged tables' keys).
    pub fn gen(&self) -> u64 {
        self.gen.unwrap_or(0)
    }
}

fn prev_ret(op: u8) -> bool {
    op == 0x60 || op == 0x40
}

/// The run's observer while the debugger is on: the instruction count, the opcode of the instruction that runs
/// (PREV_RET of an access unit), the read and write units on the accesses `on_access` sees.
pub(crate) struct DbgObs<'a, O> {
    pub inner: &'a mut O,
    pub units: [Unit; 2],
    pub access: bool,
    want_op: bool,
    op: u8,
    ipc: u16,
    pub retired: u64,
    pub last_op: Option<u8>,
    pub stop: Option<AccStop>,
    pub logs: Vec<u64>,
    /// The engine count at C64 cycle `base.0` (a log record's cycle field).
    base: (u64, u64),
    pub inner_halt: bool,
    pub dbg_halt: bool,
}

impl<'a, O: Observer> DbgObs<'a, O> {
    pub fn new(inner: &'a mut O, d: &DbgRun, clk: u64, count: u64, pc: u16) -> Self {
        DbgObs {
            inner,
            units: d.units,
            access: d.access.is_some(),
            want_op: true,
            op: d.prev_op,
            ipc: pc,
            retired: 0,
            last_op: None,
            stop: None,
            logs: Vec::new(),
            base: (clk, count),
            inner_halt: false,
            dbg_halt: false,
        }
    }

    /// The read and write units on one access (trx_dbg.v:229-235, :190-213): type 2 reads, 3 writes, 4 both.
    fn units_access(&mut self, kind: BusKind, addr: u16, value: u8, cx: AccessCtx) -> bool {
        let we = kind == BusKind::Write;
        let regs = [cx.a, cx.x, cx.y, cx.sp, cx.p];
        let ret = prev_ret(self.op);
        let m: [bool; 2] = std::array::from_fn(|i| {
            let u = &self.units[i];
            let t = u.kind();
            u.armed() && (t == td::T_RW || t == if we { td::T_WRITE } else { td::T_READ }) && u.qualifies(addr, value, regs, ret)
        });
        let mut stop = false;
        for (i, (hit, fire)) in td::hits(&self.units, m).into_iter().enumerate() {
            let u = &mut self.units[i];
            if hit {
                u.hits = u.hits.wrapping_add(1);
            }
            if !fire {
                continue;
            }
            match u.action() {
                td::A_STOP => {
                    if self.stop.is_none() {
                        self.stop = Some(AccStop { unit: i as u8, addr, data: value });
                    }
                    stop = true;
                }
                td::A_LOG => {
                    let cycle = self.base.1 + cx.clk.saturating_sub(self.base.0);
                    self.logs.push(log_rec(cycle, self.ipc, false, we, i as u8, value, addr));
                }
                _ => {}
            }
            if u.one_shot() {
                u.ctrl &= !1;
            }
        }
        stop
    }
}

/// A unit's log record, engine stream 1 space 8 (trx_dbg.v:100-101, :251): `[15:0]` ADDR, `[23:16]` DATA, `[27:24]`
/// `{0, IB, write, unit}`, `[31:28]` 8, `[47:32]` IPC, `[62:48]` CYCLES low 15.
fn log_rec(cycle: u64, ipc: u16, ib: bool, we: bool, unit: u8, data: u8, addr: u16) -> u64 {
    (cycle & 0x7FFF) << 48
        | u64::from(ipc) << 32
        | 8 << 28
        | u64::from(ib) << 26
        | u64::from(we) << 25
        | u64::from(unit & 1) << 24
        | u64::from(data) << 16
        | u64::from(addr)
}

impl<O: Observer> Observer for DbgObs<'_, O> {
    const RECORDS_VIC: bool = O::RECORDS_VIC;

    #[inline(always)]
    fn on_instruction(&mut self, pc: u16, op: u8, b1: u8, b2: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, clk: u64) {
        self.retired += 1;
        self.last_op = Some(op);
        self.want_op = true;
        self.inner.on_instruction(pc, op, b1, b2, a, x, y, sp, p, clk);
    }

    #[inline(always)]
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        if kind == BusKind::Fetch && self.want_op {
            (self.op, self.ipc, self.want_op) = (value, addr, false);
        }
        self.inner.on_bus(kind, addr, value, pc, clk, old);
    }

    #[inline(always)]
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.inner.on_interrupt(vector, clk);
    }

    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8, cx: AccessCtx) -> bool {
        let inner = self.inner.on_access(kind, addr, value, cx);
        let mine = self.access && matches!(kind, BusKind::Read | BusKind::Write) && self.units_access(kind, addr, value, cx);
        self.inner_halt |= inner;
        self.dbg_halt |= mine;
        inner || mine
    }

    fn on_vic_reg(&mut self, clk: u64, raster_y: u16, kind: u8, value: u8) {
        self.inner.on_vic_reg(clk, raster_y, kind, value);
    }
}

impl Trx64Backend {
    /// The engine's count now: inside a run the cycles run so far, outside the counter.
    pub(crate) fn count_now(&self) -> u64 {
        if self.tap.in_run {
            self.tap.run_cycles + self.m.c64_core.clk.saturating_sub(self.tap.run_clk)
        } else {
            self.tap.cycles
        }
    }

    /// The machine stands: the pause, the position hold or the debugger's hold.
    pub(crate) fn stands(&self) -> bool {
        self.paused || self.tap.pos_hold || self.dbg.held
    }

    /// Take the model's configuration (S39 §3.3): the tables when its generation moved, the hold always. A release
    /// lets the machine run on from the cycle it stood at.
    pub(crate) fn dbg_sync(&mut self) {
        let Some(link) = self.dbg.link.clone() else { return };
        let was = self.dbg.held;
        {
            let m = td::lock(&link);
            self.dbg.held = m.holds();
            if self.dbg.gen != Some(m.gen) {
                let count = self.count_now();
                let d = &mut self.dbg;
                d.gen = Some(m.gen);
                d.units = m.units;
                d.bmp_en = m.bmp_en;
                d.bmp = m.bmp_en.then(|| m.bmp.clone());
                d.pend_ib = m.pend_ib;
                d.pend_cb = m.pend_cb;
                d.drv_cmp = m.drive.exec_cmp();
                if m.step != d.step {
                    d.step = m.step;
                    (d.step_from, d.step_ibs) = (count, 0);
                }
                // CYC_CMP fires on the edge that passes while CYCLES[31:0] equals it (trx_dbg.v:166), CYCLES the count
                // with the stop's own fetch (`ib_bias`).
                let cyc = (count + u64::from(m.ib_bias)) as u32;
                d.cmp_at = m.cmp_en.then(|| count + u64::from(m.cyc_cmp.wrapping_sub(cyc)));
                d.every = d.pend_ib || d.pend_cb || d.step.is_some_and(|s| s.ins) || d.drv_cmp.is_some();
                let mut exec: Option<Box<[u8; 0x1_0000]>> = None;
                let mut access: Option<Box<[u8; 0x1_0000]>> = None;
                if let Some(bmp) = &d.bmp {
                    let t = exec.get_or_insert_with(|| Box::new([0; 0x1_0000]));
                    for (a, b) in t.iter_mut().enumerate() {
                        *b = (bmp[a >> 5] >> (a & 31) & 1) as u8;
                    }
                }
                for u in d.units.iter().filter(|u| u.armed()) {
                    let t = if u.kind() == td::T_IB { &mut exec } else { &mut access };
                    let t = t.get_or_insert_with(|| Box::new([0; 0x1_0000]));
                    t[usize::from(u.lo)..=usize::from(u.hi.max(u.lo))].fill(1);
                }
                d.exec = exec;
                d.access = access;
                for (u, &mu) in d.units.iter_mut().zip(&m.units) {
                    u.hits = mu.hits;
                }
            }
        }
        // S39 §3.8: OWNER taken while S38's transport holds the C64 on an anchor: the transport lets go (the anchors
        // after it are cut) and the C64 stays where it is, now held by the debugger.
        let own = td::lock(&link).own;
        if own && !self.dbg.own && self.ring_holds() {
            self.ring_release();
            let mut m = td::lock(&link);
            m.hold = true;
            m.gen += 1;
            self.dbg.held = true;
        }
        self.dbg.own = own;
        // The drive's own hold (DRV_PAUSE/DRV_RUN; RUN takes it away).
        let drive_hold = td::lock(&link).drive.hold;
        if self.drives[0].dbg_hold != drive_hold {
            self.drives[0].dbg_hold = drive_hold;
            self.drives[0].update(&mut self.m, self.stopped, self.reset_held);
        }
        if was && !self.dbg.held {
            self.dbg.eval_clk = Some(self.m.c64_core.clk);
            if !self.stands() {
                self.reanchor();
            }
        }
    }

    /// The counters, the units' hits and one-shot enables, the IB state into the model.
    pub(crate) fn dbg_publish(&mut self) {
        let Some(link) = self.dbg.link.clone() else { return };
        let count = self.count_now();
        let mut m = td::lock(&link);
        m.cycles = count;
        m.instrs = self.dbg.instrs as u32;
        let mut off = false;
        for (mu, u) in m.units.iter_mut().zip(&self.dbg.units) {
            mu.hits = u.hits;
            // A one-shot unit switched itself off (trx_dbg.v:213): the tables follow at the next sync.
            if !u.on() && mu.on() {
                mu.ctrl &= !1;
                off = true;
            }
        }
        if off {
            m.gen += 1;
        }
        let d = self.m.drive(trx64_core::drive::DrivePosition::A);
        m.drive.dpc = d.cpu().reg_pc;
        m.drive.dcycles = d.cpu().clk as u32;
    }

    /// What a run with the debugger's observer found: the instructions, the last opcode, the units' hits, an access
    /// stop, the log records.
    pub(crate) fn dbg_after<O>(&mut self, obs: DbgObs<'_, O>) {
        self.dbg.instrs += obs.retired;
        if let Some(op) = obs.last_op {
            self.dbg.prev_op = op;
            // The machine moved: the reported IB state is UE2's again.
            if let Some(link) = &self.dbg.link {
                td::lock(link).ipc_set = None;
            }
        }
        let changed = obs.units != self.dbg.units;
        self.dbg.units = obs.units;
        if obs.stop.is_some() {
            self.dbg.acc = obs.stop;
        }
        for rec in obs.logs {
            self.tap.events.push(ue2_core::c64host::TapEvent::DbgLog(rec));
        }
        if changed {
            self.dbg_publish();
            self.dbg_sync();
        }
    }

    /// Look at the boundary the machine stands on (S39 §3.3): the stop sources in the RTL's order of precedence
    /// (trx_dbg.v:281-284). A stop holds the machine, writes the record and returns true.
    pub(crate) fn dbg_boundary(&mut self) -> bool {
        let Some(link) = self.dbg.link.clone() else { return false };
        if self.dbg.held {
            return true;
        }
        let clk = self.m.c64_core.clk;
        let acc = self.dbg.acc.take();
        // The sources of an edge (units, bitmap, the step's count, the drive comparator) are looked at once per
        // boundary; a run resuming where it stood passes them. Pauses, CYC_CMP and a step's end are levels.
        let first = self.dbg.eval_clk != Some(clk);
        self.dbg.eval_clk = Some(clk);
        // A boundary whose fetch an interrupt replaces is no IB (trx_dbg.v:152); a held 6510 fetches nothing.
        let cpu_runs = !(self.stopped || self.reset_held);
        let ib = cpu_runs && !(self.irq_due() || self.nmi_due());
        let c = &self.m.c64_core;
        let pc = c.reg_pc;
        let regs = [c.reg_a, c.reg_x, c.reg_y, c.reg_sp, c.status()];
        let data = self.m.read_full(pc);
        let count = self.count_now();
        // IB units: the value is IPC, the data the opcode, the registers the settled ones (§7 Q7).
        let mut ustop = None;
        if ib && first {
            let ret = prev_ret(self.dbg.prev_op);
            let m: [bool; 2] = std::array::from_fn(|i| {
                let u = &self.dbg.units[i];
                u.armed() && u.kind() == td::T_IB && u.qualifies(pc, data, regs, ret)
            });
            let mut logs = Vec::new();
            for (i, (hit, fire)) in td::hits(&self.dbg.units, m).into_iter().enumerate() {
                let u = &mut self.dbg.units[i];
                if hit {
                    u.hits = u.hits.wrapping_add(1);
                }
                if !fire {
                    continue;
                }
                match u.action() {
                    td::A_STOP => {
                        ustop.get_or_insert(i as u8);
                    }
                    td::A_LOG => logs.push(log_rec(count, pc, true, false, i as u8, data, pc)),
                    _ => {}
                }
                if u.one_shot() {
                    u.ctrl &= !1;
                }
            }
            for rec in logs {
                self.tap.events.push(ue2_core::c64host::TapEvent::DbgLog(rec));
            }
            if m.iter().any(|&x| x) {
                self.dbg_publish();
                self.dbg_sync();
            }
        }
        let d = &mut self.dbg;
        // An instruction step counts the IBs after its start (trx_dbg.v:259).
        if first && ib && d.step.is_some_and(|s| s.ins) && count > d.step_from {
            d.step_ibs = d.step_ibs.saturating_add(1);
        }
        let bmp_hit = ib && first && d.bmp.as_ref().is_some_and(|b| b[usize::from(pc >> 5)] >> (pc & 31) & 1 != 0);
        let cmp_hit = d.cmp_at.is_some_and(|t| count >= t);
        let dpc = self.m.drive(trx64_core::drive::DrivePosition::A).cpu().reg_pc;
        let drv_hit = first && d.drv_cmp == Some(dpc) && d.drv_last != Some(dpc);
        d.drv_last = Some(dpc);
        let step_end = match d.step {
            Some(s) if s.ins => d.step_ibs >= s.n,
            Some(s) => count >= d.step_from + u64::from(s.n),
            None => false,
        };
        let pause = (d.pend_ib && ib) || d.pend_cb;
        let (reason, unit, addr, sdata) = if let Some(a) = acc {
            (td::R_UNIT, a.unit, a.addr, a.data)
        } else if let Some(u) = ustop {
            (td::R_UNIT, u, pc, data)
        } else if bmp_hit {
            (td::R_BITMAP, 0, pc, data)
        } else if cmp_hit {
            (td::R_CYC_CMP, 0, pc, data)
        } else if drv_hit {
            (td::R_DRIVE, 0, pc, data)
        } else if step_end {
            (td::R_STEP, 0, pc, data)
        } else if pause {
            (td::R_PAUSE, 0, pc, data)
        } else {
            return false;
        };
        let (line, cycle) = self.raster();
        let rec = Record {
            reason,
            unit,
            flags: u8::from(ib) * td::F_IB | u8::from(drv_hit) * td::F_DRIVE,
            data: sdata,
            addr,
            ipc: pc,
            pos: (line & 0x1FF) << 7 | (cycle & 0x7F),
        };
        {
            let mut m = td::lock(&link);
            m.stop(rec, cmp_hit);
            if drv_hit {
                m.drive.hold = true;
            }
        }
        self.dbg_publish();
        self.dbg_sync();
        true
    }

    /// S40 phase E, the trace former's feed (t21-p4 trx_dbg.v:139-143, :288-294; trx_engine.v:343-377): at an IB with
    /// the trace on, the instruction about to run. UE2 stands before the fetch, so the count after the fetch's edge is
    /// one more than now; the registers are the settled ones (chip 1 w4/w5), the two bytes the operand bytes (the app
    /// masks them by the opcode's length, S40 answer 9). A boundary whose fetch an interrupt replaces is no IB: no
    /// record, and the handler's first instruction is one `execute` with the interrupt (S39 T2).
    pub(crate) fn trace_boundary(&mut self) {
        let clk = self.m.c64_core.clk;
        if !self.tap.cfg.trace || self.tap.trace_clk == Some(clk) {
            return;
        }
        self.tap.trace_clk = Some(clk);
        if self.stopped || self.reset_held || self.irq_due() || self.nmi_due() {
            return;
        }
        let c = &self.m.c64_core;
        let (pc, regs) = (c.reg_pc, [c.reg_a, c.reg_x, c.reg_y, c.reg_sp, c.status()]);
        let (op, b1, b2) = (self.m.read_full(pc), self.m.read_full(pc.wrapping_add(1)), self.m.read_full(pc.wrapping_add(2)));
        let cycle = self.count_now() + 1;
        self.tap.events.push(ue2_core::c64host::TapEvent::Trace { cycle, pc, op, b1, b2, regs });
    }

    /// The budget the next run may take before a cycle point of the debugger: CYC_CMP, the end of a step of cycles.
    pub(crate) fn dbg_cycles_left(&self) -> Option<u64> {
        let count = self.count_now();
        let d = &self.dbg;
        let cmp = d.cmp_at.map(|t| t.saturating_sub(count));
        let step = d.step.filter(|s| !s.ins).map(|s| (d.step_from + u64::from(s.n)).saturating_sub(count));
        match (cmp, step) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether a boundary at the PC the machine stands on is one the run has to step past before its tables apply.
    pub(crate) fn dbg_single(&self) -> bool {
        self.dbg.every || self.dbg.exec_at(self.m.c64_core.reg_pc)
    }

    /// A step the model holds, run now (S39 §3.3): an instruction step to its n-th IB, a cycle step to the first
    /// boundary at or after its count. The machine stands before and after; if the step cannot end here (the 6510
    /// is held, the C64 waits for the firmware), it is left to the running machine, as the gate stays open on the
    /// board.
    pub(crate) fn dbg_run_step(&mut self) {
        self.dbg_sync();
        let Some(step) = self.dbg.step else { return };
        if self.paused || self.tap.pos_hold {
            return;
        }
        let clk = self.m.c64_core.clk;
        let budget = if step.ins { u64::from(step.n) * STEP_CYCLES_PER_INSTR + 1_000 } else { u64::from(step.n) };
        self.run_to_plain(clk + budget.max(1));
        if !self.dbg.held {
            // Not there yet: the next slices run on until it ends.
            self.dbg_boundary();
        }
        self.dbg_publish();
    }

    /// DRV_STEP (drv_dbg.v:96-101): drive A alone, `n` instructions, the C64 standing. TRX64 pairs the drive's clock
    /// with the C64's (S39 §5 T5): the drive runs here on its own target and keeps that lead afterwards.
    pub(crate) fn dbg_drive_step_now(&mut self, n: u16) {
        let Some(link) = self.dbg.link.clone() else { return };
        let pos = trx64_core::drive::DrivePosition::A;
        if !self.m.drive(pos).is_clocked() {
            // A drive that is off, in reset or stopped runs nothing: the step never ends (the app times out).
            return;
        }
        for _ in 0..n.max(1) {
            let before = self.m.drive(pos).cpu().clk;
            for _ in 0..64 {
                self.m.drive_mut(pos).run_cycles(1);
                if self.m.drive(pos).cpu().clk != before {
                    break;
                }
            }
        }
        {
            let mut m = td::lock(&link);
            m.drive.hold = true;
            let ipc = m.rec.ipc;
            m.drive_done(Record { ipc, ..Record::default() });
        }
        self.dbg_publish();
        self.dbg_sync();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ue2_core::c64host::{C64Backend, TapEvent};
    use ue2_core::devices::trx_dbg::{Act, Dbg};
    use ue2_core::devices::trx_sb::SbLoad;

    use super::*;
    use crate::tests::{booted, run_ms};
    use crate::trx_ff;

    /// A C64 at READY with the debug block, its registers written as the C64 port takes them (c64.rs
    /// `trx_dbg_op`), without a session.
    struct Rig {
        c64: Trx64Backend,
        now: u64,
        d: DbgLink,
    }

    impl Rig {
        fn new() -> Option<Rig> {
            let (mut c64, now) = booted()?;
            let d = Arc::new(Mutex::new(Dbg::default()));
            c64.set_dbg(Some(d.clone()));
            Some(Rig { c64, now, d })
        }

        fn w(&mut self, off: u32, v: u32) {
            let shut = self.c64.stands();
            let act = td::lock(&self.d).write(off, v, shut);
            match act {
                Act::Run => td::lock(&self.d).release(),
                Act::Step(s) => {
                    td::lock(&self.d).start_step(s);
                    self.c64.dbg_changed();
                    self.c64.dbg_step();
                }
                Act::DrvStep(n) => self.c64.dbg_drive_step(n),
                _ => {}
            }
            self.c64.dbg_changed();
        }

        fn cmd(&mut self, c: u8) {
            self.w(td::PAGE3 + td::CMD, u32::from(c));
        }

        fn step(&mut self, n: u16, ins: bool) {
            self.w(td::PAGE3 + td::ARG, u32::from(n));
            self.cmd(if ins { td::CMD_STEP_INSTR } else { td::CMD_STEP_CYCLES });
        }

        fn run(&mut self, ms: u64) {
            run_ms(&mut self.c64, &mut self.now, ms);
        }

        fn rec(&self) -> Record {
            td::lock(&self.d).rec
        }

        fn held(&self) -> bool {
            td::lock(&self.d).holds()
        }

        fn model(&self) -> Dbg {
            td::lock(&self.d).clone()
        }

        fn pc(&self) -> u16 {
            self.c64.m.c64_core.reg_pc
        }

        /// `code` at `$C000`, the 6510 sent there; the stop record cleared.
        fn code(&mut self, code: &[u8]) {
            self.c64.m.ram[0xC000..0xC000 + code.len()].copy_from_slice(code);
            self.c64.m.c64_core.reg_pc = 0xC000;
            self.cmd(td::CMD_CLEAR_STOP);
        }

        fn unit(&mut self, n: u32, ctrl: u32, lo: u16, hi: u16, reg: u32) {
            let b = td::PAGE3 + td::UNIT0 + td::UNIT_STRIDE * n;
            self.w(b, 0);
            self.w(b + 4, u32::from(hi) << 16 | u32::from(lo));
            self.w(b + 8, 0);
            self.w(b + 12, reg);
            self.w(b + 16, 0);
            self.w(b + 20, 0);
            self.w(b, ctrl);
        }
    }

    const IB_ONCE: u32 = 1 | (td::T_IB as u32) << 1 | 0x100;
    /// REG `S >= v`.
    fn s_ge(v: u8) -> u32 {
        4 | 3 << 8 | u32::from(v) << 16 | 0xFF << 24
    }

    /// trx_dbg.v:343-381, S39 §9: PAUSE_IB stops at the boundary UE2 stands on (REASON 1, IPC = PC, the IB flag, IRQ
    /// cause `ev`); STEP_INSTR 1 and 5 run that many instructions to the next IB (REASON 2), INSTRS + n, CYCLES + the
    /// instructions' cycles; a step on a running machine is ignored; RUN lets go.
    #[test]
    fn pause_and_instruction_steps() {
        let Some(mut r) = Rig::new() else { return };
        #[rustfmt::skip]
        r.code(&[
            0x78,             // C000 SEI
            0xA9, 0x01,       // C001 LDA #1
            0xA2, 0x02,       // C003 LDX #2
            0xC8,             // C005 INY
            0xEA,             // C006 NOP
            0x8D, 0x00, 0x04, // C007 STA $0400
            0x4C, 0x0A, 0xC0, // C00A JMP $C00A
        ]);
        r.step(1, true);
        assert!(!r.held(), "a step on a running gate is ignored");
        r.cmd(td::CMD_PAUSE_IB);
        assert!(r.held() && r.c64.stands());
        let (rec, ev) = (r.rec(), r.model().ev);
        assert_eq!((rec.reason, rec.ipc, rec.addr, rec.data, rec.flags & td::F_IB), (td::R_PAUSE, 0xC000, 0xC000, 0x78, 1));
        assert_eq!(ev, 1);
        let clk = r.c64.m.c64_core.clk;
        r.run(5);
        assert_eq!(r.c64.m.c64_core.clk, clk, "the debugger's hold stands the machine");
        let (cycles, instrs) = (r.model().cycles_now(), r.model().instrs_now());
        r.step(0, true);
        assert_eq!((r.pc(), r.rec().reason, r.rec().ipc), (0xC001, td::R_STEP, 0xC001), "ARG 0 is one instruction");
        assert_eq!((r.model().instrs_now() - instrs, r.model().cycles_now() - cycles), (1, 2), "SEI");
        let (cycles, instrs) = (r.model().cycles_now(), r.model().instrs_now());
        r.step(5, true);
        assert_eq!(r.pc(), 0xC00A);
        assert_eq!((r.model().instrs_now() - instrs, r.model().cycles_now() - cycles), (5, 2 + 2 + 2 + 2 + 4));
        assert_eq!(r.c64.m.ram[0x0400], 0x01);
        // STEP_CYCLES 5: whole instructions, the first boundary at or after the count (S39 §5 T1).
        r.c64.m.c64_core.reg_pc = 0xC001;
        r.cmd(td::CMD_CLEAR_STOP);
        let cycles = r.model().cycles_now();
        r.step(5, false);
        assert_eq!((r.pc(), r.model().cycles_now() - cycles, r.rec().reason), (0xC006, 6, td::R_STEP));
        r.cmd(td::CMD_RUN);
        assert!(!r.held());
        let clk = r.c64.m.c64_core.clk;
        r.run(2);
        assert!(r.c64.m.c64_core.clk > clk, "RUN: it runs");
    }

    /// trx_dbg.v:183, S39 §9: a bitmap bit at `$C001` stops there before the instruction runs (REASON 3); RUN goes on
    /// and stops there again only when it is reached again.
    #[test]
    fn the_exec_bitmap() {
        let Some(mut r) = Rig::new() else { return };
        r.code(&[0x78, 0xEE, 0x00, 0x04, 0x4C, 0x01, 0xC0]); // SEI; INC $0400; JMP $C001
        r.c64.m.ram[0x0400] = 0;
        r.w(td::PAGE3 + td::BMP_IDX, 0xC001 >> 5);
        r.w(td::PAGE3 + td::BMP_DATA, 1 << (0xC001 & 31));
        r.w(td::PAGE3 + td::DBG_CTRL, 1);
        r.run(2);
        assert!(r.held());
        assert_eq!((r.rec().reason, r.rec().ipc, r.c64.m.ram[0x0400]), (td::R_BITMAP, 0xC001, 0), "before INC");
        r.cmd(td::CMD_CLEAR_STOP);
        r.cmd(td::CMD_RUN);
        r.run(2);
        assert_eq!((r.rec().reason, r.pc(), r.c64.m.ram[0x0400], r.model().stops), (td::R_BITMAP, 0xC001, 1, 2));
    }

    /// S39 §5 T2: the interrupt sequence and the handler's first instruction are one TRX64 `execute`, so a bitmap bit
    /// at the KERNAL IRQ handler's first opcode (`$FF48`) never sees an IB; one at its second (`$FF49`) does.
    #[test]
    fn a_handlers_first_opcode_is_no_boundary_in_trx64() {
        let Some(mut r) = Rig::new() else { return };
        r.w(td::PAGE3 + td::BMP_IDX, 0xFF48 >> 5);
        r.w(td::PAGE3 + td::BMP_DATA, 1 << (0xFF48 & 31));
        r.w(td::PAGE3 + td::DBG_CTRL, 1);
        r.run(60);
        assert!(!r.held(), "T2: no stop at $FF48");
        r.w(td::PAGE3 + td::BMP_IDX, 0xFF48 >> 5);
        r.w(td::PAGE3 + td::BMP_DATA, 1 << (0xFF49 & 31));
        r.run(60);
        assert_eq!((r.held(), r.pc()), (true, 0xFF49));
    }

    /// trxmon's `until`, `n` and `ret` (mach.c:505-531) and a store observer (S39 §9): IB units with a one-shot, the S
    /// qualifier and PREV_RET; a write unit stops at the boundary after the store with its address and byte; HITS.
    #[test]
    fn units_until_next_ret_and_a_store_observer() {
        let Some(mut r) = Rig::new() else { return };
        #[rustfmt::skip]
        r.code(&[
            0x78,             // C000 SEI
            0x20, 0x10, 0xC0, // C001 JSR $C010
            0xEA,             // C004 NOP
            0x4C, 0x05, 0xC0, // C005 JMP $C005
            0, 0, 0, 0, 0, 0, 0, 0,
            0xA2, 0x05,       // C010 LDX #5
            0xCA,             // C012 DEX
            0xD0, 0xFD,       // C013 BNE $C012
            0x8E, 0x00, 0x04, // C015 STX $0400
            0x60,             // C018 RTS
        ]);
        r.cmd(td::CMD_PAUSE_IB);
        r.step(1, true);
        // `until $C015`: unit 0, IB, one-shot.
        r.unit(0, IB_ONCE, 0xC015, 0xC015, 0);
        r.cmd(td::CMD_RUN);
        r.run(2);
        assert_eq!((r.held(), r.pc(), r.rec().reason, r.rec().unit), (true, 0xC015, td::R_UNIT, 0));
        assert!(!r.model().units[0].on() && r.model().units[0].hits == 1, "one-shot: off after it fired");
        // A store observer over the screen: unit 1, write, stop.
        r.cmd(td::CMD_CLEAR_STOP);
        r.unit(1, 1 | (td::T_WRITE as u32) << 1, 0x0400, 0x07E7, 0);
        r.cmd(td::CMD_RUN);
        r.run(2);
        let rec = r.rec();
        assert_eq!((rec.reason, rec.unit, rec.addr, rec.data, rec.ipc), (td::R_UNIT, 1, 0x0400, 0x00, 0xC018), "after STX");
        assert_eq!(r.model().units[1].hits, 1);
        r.w(td::PAGE3 + td::UNIT0 + td::UNIT_STRIDE, 0);
        // `ret` from inside the subroutine: IB, PREV_RET, S >= S0 + 1.
        r.cmd(td::CMD_CLEAR_STOP);
        let s0 = r.c64.m.c64_core.reg_sp;
        r.unit(0, IB_ONCE | 0x4000, 0x0000, 0xFFFF, s_ge(s0 + 1));
        r.cmd(td::CMD_RUN);
        r.run(2);
        assert_eq!((r.pc(), r.c64.m.c64_core.reg_sp, r.rec().reason), (0xC004, s0 + 2, td::R_UNIT));
        // `n` over the JSR: unit 0 at the return address with S >= S0, unit 1 any IB with S >= S0 + 1.
        r.c64.m.c64_core.reg_pc = 0xC001;
        r.cmd(td::CMD_CLEAR_STOP);
        let s0 = r.c64.m.c64_core.reg_sp;
        r.unit(0, IB_ONCE, 0xC004, 0xC004, s_ge(s0));
        r.unit(1, IB_ONCE, 0x0000, 0xFFFF, s_ge(s0 + 1));
        r.cmd(td::CMD_RUN);
        r.run(2);
        assert_eq!((r.pc(), r.c64.m.c64_core.reg_sp, r.rec().unit), (0xC004, s0, 0), "over the subroutine");
    }

    /// trx_dbg.v:165-166, :308: CYC_CMP stops at the first boundary at or after the count (S39 §5 T1), REASON 7,
    /// disarmed after, STATUS b19.
    #[test]
    fn the_cycle_comparator() {
        let Some(mut r) = Rig::new() else { return };
        r.code(&[0x78, 0xEE, 0x00, 0x04, 0x4C, 0x01, 0xC0]);
        r.cmd(td::CMD_PAUSE_IB);
        let at = r.model().cycles_now() as u32 + 1000;
        r.w(td::CYC_CMP, at);
        assert!(r.model().status(td::Ctx::default()) & 1 << 19 != 0);
        r.cmd(td::CMD_CLEAR_STOP);
        r.cmd(td::CMD_RUN);
        r.run(5);
        let m = r.model();
        assert_eq!((r.held(), m.rec.reason, m.cmp_en), (true, td::R_CYC_CMP, false));
        let past = m.cycles_now() as u32 - (at + 1);
        assert!(past < 8, "at the edge or the boundary after it: {past}");
    }

    /// S39 §3.4: the CPU write path: `r a=77` staged into the image, loaded at the step's opening edge, then
    /// `STA $0400` stores it in 4 cycles.
    #[test]
    fn a_cpu_write_then_a_step() {
        let Some(mut r) = Rig::new() else { return };
        r.code(&[0x78, 0x8D, 0x00, 0x04, 0x4C, 0x04, 0xC0]);
        r.cmd(td::CMD_PAUSE_IB);
        r.step(1, true);
        let mut w = r.c64.shadow_now().cpuf;
        trx_ff::put(&mut w, trx_ff::cpu::A, 8, 0x77);
        trx_ff::put(&mut w, trx_ff::cpu::IR, 8, 0xEA);
        r.c64.sb_load(&SbLoad { cpu: Some(w), cia: [None, None] });
        let cycles = r.model().cycles_now();
        r.step(1, true);
        assert_eq!((r.c64.m.ram[0x0400], r.model().cycles_now() - cycles, r.pc()), (0x77, 4, 0xC004));
    }

    /// S40 phase E: with the trace on every instruction comes at its IB with its PC, opcode, operand bytes and the
    /// registers before it, its RAM writes after it, so the engine counts them to it; the count is one past the fetch.
    #[test]
    fn the_trace_feed_comes_before_each_instructions_writes() {
        let Some(mut r) = Rig::new() else { return };
        r.code(&[0x78, 0xA9, 0x55, 0x8D, 0x00, 0x04, 0xEE, 0x00, 0x04, 0x4C, 0x09, 0xC0]);
        r.cmd(td::CMD_PAUSE_IB);
        r.c64.set_engine(ue2_core::c64host::EngineCfg { boundaries: false, records: true, trace: true });
        let count = r.c64.count_now();
        r.c64.take_tap();
        r.step(4, true);
        let ev: Vec<TapEvent> = r.c64.take_tap().into_iter().filter(|e| matches!(e, TapEvent::Trace { .. } | TapEvent::Ram { .. })).collect();
        let kinds: Vec<String> = ev
            .iter()
            .map(|e| match e {
                TapEvent::Trace { pc, .. } => format!("T{pc:04x}"),
                TapEvent::Ram { addr, new, .. } => format!("W{addr:04x}={new:02x}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(kinds, ["Tc000", "Tc001", "Tc003", "W0400=55", "Tc006", "W0400=55", "W0400=56"]);
        let TapEvent::Trace { cycle, op, b1, b2, regs, .. } = ev[2] else { unreachable!() };
        assert_eq!((cycle, op, b1, b2, regs[0]), (count + 4 + 1, 0x8D, 0x00, 0x04, 0x55), "after SEI and LDA, its fetch");
    }

    /// PAUSE_CB with the 6510 held (C64_STOP alone): the stop comes at once, no IB (FLAGS b0 0). A log action puts
    /// its record into the engine's events.
    #[test]
    fn pause_cb_with_the_6510_held_and_a_log_action() {
        let Some(mut r) = Rig::new() else { return };
        r.code(&[0x78, 0x8D, 0x00, 0x04, 0x4C, 0x01, 0xC0]);
        r.unit(0, 1 | (td::T_WRITE as u32) << 1 | (td::A_LOG as u32) << 5, 0x0400, 0x0400, 0);
        r.run(1);
        let logs: Vec<u64> = r.c64.take_tap().into_iter().filter_map(|e| if let TapEvent::DbgLog(v) = e { Some(v) } else { None }).collect();
        assert!(!logs.is_empty());
        assert_eq!(logs[0] & 0xF000_FFFF, 0x8000_0400, "space 8, address $0400");
        assert_eq!(logs[0] >> 25 & 1, 1, "a write");
        assert!(!r.held(), "a log action does not stop");
        r.c64.set_stopped(true);
        r.run(1);
        r.cmd(td::CMD_PAUSE_CB);
        assert_eq!((r.held(), r.rec().reason, r.rec().flags & td::F_IB), (true, td::R_PAUSE, 0));
    }
}
