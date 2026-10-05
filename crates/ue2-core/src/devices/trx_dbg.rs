//! S39: TRX64-Ultimate's debug block, `trx_dbg.v` and `drv_dbg.v` (t21-p4), as a register model shared by the IO
//! block (reads), the C64 port (writes at their clock, the run-control commands) and the C64 backend (the stops it
//! finds on its run, the counters). Spec: docs/specs/S39-trxmon.md §1.1, §1.3, §3.3, §3.5.
//!
//! Registers (block offsets, 32-bit, trx_sys.v's byte rule): page 0 0x118-0x13F (OWNER, CYCLES, CYCLES_HI, INSTRS,
//! CYC_CMP, CYC_CMP_EN) and page 3 (CMD, ARG, STATUS, STOP0/1, IPC_SET, the exec bitmap, two watch units, HITS, the
//! drive's DRV_CMP, DRV_STAT, DCYCLES). The stop logic that needs the machine (where an instruction boundary is,
//! what a unit matches) is the backend's; this module holds what the RTL holds and decides what a command does.
//!
//! UE2 stands at TRX64's instruction boundaries only, so every stop it makes is at an IB (S39 §5 T1): `CYCLES` and
//! `INSTRS` then read as the RTL's do after the stop's edge, the opcode fetch counted (`ib_bias`).

use std::sync::{Arc, Mutex, MutexGuard};

/// Page 0 (trx_sys.v:35, trx_dbg.v:21-31).
pub const OWNER: u32 = 0x118;
pub const CYCLES: u32 = 0x120;
pub const CYCLES_HI: u32 = 0x124;
pub const INSTRS: u32 = 0x128;
pub const CYC_CMP: u32 = 0x130;
pub const CYC_CMP_EN: u32 = 0x134;
/// Page 3 (trx_dbg.v:32-99, drv_dbg.v:13-28).
pub const PAGE3: u32 = 0x3000;
pub const CMD: u32 = 0x000;
pub const ARG: u32 = 0x004;
pub const STATUS: u32 = 0x008;
pub const STOP0: u32 = 0x010;
pub const STOP1: u32 = 0x014;
pub const IPC_SET: u32 = 0x018;
pub const BMP_IDX: u32 = 0x040;
pub const BMP_DATA: u32 = 0x044;
pub const DBG_CTRL: u32 = 0x048;
pub const UNIT0: u32 = 0x080;
pub const UNIT_STRIDE: u32 = 0x20;
pub const HITS: u32 = 0x0B4;
pub const DRV_CMP: u32 = 0x0C0;
pub const DRV_STAT: u32 = 0x0C4;
pub const DCYCLES: u32 = 0x0C8;

/// CMD values (trx_dbg.v:341-364, drv_dbg.v:55-61).
pub const CMD_PAUSE_CB: u8 = 0x01;
pub const CMD_PAUSE_IB: u8 = 0x02;
pub const CMD_RUN: u8 = 0x03;
pub const CMD_STEP_INSTR: u8 = 0x05;
pub const CMD_STEP_CYCLES: u8 = 0x06;
pub const CMD_DRV_PAUSE: u8 = 0x10;
pub const CMD_DRV_RUN: u8 = 0x11;
pub const CMD_DRV_STEP: u8 = 0x12;
pub const CMD_CLEAR_STOP: u8 = 0x20;

/// STOP0 REASON (trx_dbg.v:58-63).
pub const R_PAUSE: u8 = 1;
pub const R_STEP: u8 = 2;
pub const R_BITMAP: u8 = 3;
pub const R_UNIT: u8 = 5;
pub const R_UNIT_RASTER: u8 = 6;
pub const R_CYC_CMP: u8 = 7;
pub const R_DRIVE: u8 = 8;
pub const R_DRV_STEP: u8 = 9;

/// STOP0 FLAGS: b0 the stop edge is an IB, b1 the drive stopped first, b2 the step released a DMA-held CPU.
pub const F_IB: u8 = 1;
pub const F_DRIVE: u8 = 2;

/// Watch unit types (CTRL b3:1).
pub const T_IB: u8 = 1;
pub const T_READ: u8 = 2;
pub const T_WRITE: u8 = 3;
pub const T_RW: u8 = 4;
pub const T_EDGE: u8 = 5;
/// Watch unit actions (CTRL b6:5).
pub const A_STOP: u8 = 0;
pub const A_COUNT: u8 = 1;
pub const A_LOG: u8 = 2;

/// The block offsets of the debug block: page 3, and page 0 0x118-0x13F (trx_sys.v:210-211 `op_dbg`).
pub fn is_dbg(off: u32) -> bool {
    off & !0xFFF == PAGE3 || (0x118..0x140).contains(&off)
}

pub type DbgLink = Arc<Mutex<Dbg>>;

/// The model, also after a panic elsewhere held it.
pub fn lock(link: &DbgLink) -> MutexGuard<'_, Dbg> {
    link.lock().unwrap_or_else(|e| e.into_inner())
}

/// One watch unit's registers (trx_dbg.v:196-215) and its hit counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unit {
    pub ctrl: u16,
    pub lo: u16,
    pub hi: u16,
    pub dval: u8,
    pub dmask: u8,
    /// RSEL b2:0, ROP b9:8, RVAL, RMASK.
    pub rsel: u8,
    pub rop: u8,
    pub rval: u8,
    pub rmask: u8,
    pub pass: u16,
    pub hits: u16,
}

impl Unit {
    /// CTRL b0.
    pub fn on(&self) -> bool {
        self.ctrl & 1 != 0
    }
    pub fn kind(&self) -> u8 {
        (self.ctrl >> 1 & 7) as u8
    }
    /// CTRL b4: the raster position `{line, cycle}` is the value instead of the address.
    pub fn raster(&self) -> bool {
        self.ctrl & 0x10 != 0
    }
    pub fn action(&self) -> u8 {
        (self.ctrl >> 5 & 3) as u8
    }
    pub fn one_shot(&self) -> bool {
        self.ctrl & 0x100 != 0
    }
    pub fn chain(&self) -> bool {
        self.ctrl & 0x200 != 0
    }
    pub fn prev_ret(&self) -> bool {
        self.ctrl & 0x4000 != 0
    }

    /// What UE2 can serve on TRX64's instruction boundaries (S39 §5 T1): the IB, read, write and read-or-write types
    /// on the address. Every-edge units and the raster source need a stop at a cycle boundary; they stay disabled.
    pub fn served(&self) -> bool {
        !self.raster() && matches!(self.kind(), T_IB | T_READ | T_WRITE | T_RW)
    }

    /// Enabled and served: the unit matches anything.
    pub fn armed(&self) -> bool {
        self.on() && self.served()
    }

    /// The range, data and register qualifiers and PREV_RET (trx_dbg.v:216-236); `regs` = A, X, Y, S, P.
    pub fn qualifies(&self, value: u16, data: u8, regs: [u8; 5], prev_ret: bool) -> bool {
        let rq = match self.rsel {
            1..=5 => {
                let rm = regs[usize::from(self.rsel - 1)] & self.rmask;
                match self.rop {
                    0 => rm == self.rval,
                    1 => rm != self.rval,
                    2 => rm < self.rval,
                    _ => rm >= self.rval,
                }
            }
            _ => true,
        };
        value >= self.lo && value <= self.hi && (data ^ self.dval) & self.dmask == 0 && rq && (!self.prev_ret() || prev_ret)
    }

    /// Unit `n`'s register `reg` (0-5) written (trx_dbg.v:203-211).
    fn write(&mut self, reg: u32, v: u32) {
        match reg {
            0 => self.ctrl = v as u16,
            1 => (self.lo, self.hi) = (v as u16, (v >> 16) as u16),
            2 => (self.dval, self.dmask) = (v as u8, (v >> 8) as u8),
            3 => (self.rsel, self.rop, self.rval, self.rmask) = ((v & 7) as u8, (v >> 8 & 3) as u8, (v >> 16) as u8, (v >> 24) as u8),
            4 => self.pass = v as u16,
            5 => self.hits = 0,
            _ => {}
        }
    }
}

/// The two units' matches on one edge (trx_dbg.v:190-193): `m` what each matched, the result per unit: hit (counts),
/// fire (its action runs).
pub fn hits(units: &[Unit; 2], m: [bool; 2]) -> [(bool, bool); 2] {
    std::array::from_fn(|i| {
        let u = &units[i];
        let hit = m[i] && (!u.chain() || m[1 - i]);
        (hit, hit && u.hits >= u.pass)
    })
}

/// The stop record (trx_dbg.v:319-328).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub reason: u8,
    pub unit: u8,
    pub flags: u8,
    pub data: u8,
    pub addr: u16,
    pub ipc: u16,
    /// `{raster line 9, cycle in the line 7}`.
    pub pos: u16,
}

/// A step that runs (trx_dbg.v:254-262).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// STEP_INSTR: instruction boundaries; STEP_CYCLES: phi2 edges.
    pub ins: bool,
    pub n: u16,
}

/// drv_dbg.v's state as UE2 has it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drive {
    /// `cmp` {type b17, enable b16, address}.
    pub cmp: u32,
    /// The debugger's hold of the drive, a step that runs (`hold`, `step`).
    pub hold: bool,
    pub step: bool,
    /// DPC (the address of the drive's last opcode fetch) and the drive's CPU cycles, as of the last sync.
    pub dpc: u16,
    pub dcycles: u32,
}

impl Drive {
    /// DRV_CMP exec type armed.
    pub fn exec_cmp(&self) -> Option<u16> {
        (self.cmp & 0x3_0000 == 0x1_0000).then_some(self.cmp as u16)
    }
}

/// What a command asks of the C64 port once the register model took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    None,
    /// RUN: end the stop's session (its list runs), then let go.
    Run,
    /// STEP_INSTR / STEP_CYCLES: end the session, then run the step.
    Step(Step),
    /// PAUSE_IB or PAUSE_CB on a running machine: stop at the next boundary that qualifies.
    Pause { ib: bool },
    /// DRV_STEP: the drive alone for n instructions.
    DrvStep(u16),
    /// DRV_PAUSE / DRV_RUN: the drive's own hold.
    DrvHold(bool),
    /// OWNER changed.
    Owner(bool),
}

/// trx_dbg.v and drv_dbg.v's registers and state.
#[derive(Clone, Debug)]
pub struct Dbg {
    pub own: bool,
    /// CYCLES as of the last sync (the backend's count) and CYCLES_HI as the last CYCLES read latched it.
    pub cycles: u64,
    pub cyc_hi: u16,
    pub instrs: u32,
    /// UE2 stands at an IB stop: CYCLES and INSTRS read one more, the fetch's edge counted.
    pub ib_bias: bool,
    pub cyc_cmp: u32,
    pub cmp_en: bool,
    pub arg: u16,
    pub bmp: Box<[u32; 2048]>,
    pub bmp_idx: u16,
    pub bmp_en: bool,
    pub units: [Unit; 2],
    /// The debugger's hold, a step that runs, a pause waiting for its boundary (PAUSE_IB; PAUSE_CB on a running
    /// machine), the drive comparator's stop waiting for the C64's next edge.
    pub hold: bool,
    pub step: Option<Step>,
    pub pend_ib: bool,
    pub pend_cb: bool,
    pub pend_drv: bool,
    pub stop_new: bool,
    pub stops: u8,
    pub rec: Record,
    /// IPC_SET: the reported IB state the app set after a redirect, until the machine moves.
    pub ipc_set: Option<(u16, u8, bool)>,
    /// Stops and records made (`ev`, IRQ_CAUSE b0).
    pub ev: u64,
    /// Bumped by every write that changes what the backend's run has to watch.
    pub gen: u64,
    pub drive: Drive,
}

impl Default for Dbg {
    fn default() -> Self {
        Dbg {
            own: false,
            cycles: 0,
            cyc_hi: 0,
            instrs: 0,
            ib_bias: false,
            cyc_cmp: 0,
            cmp_en: false,
            arg: 0,
            bmp: Box::new([0; 2048]),
            bmp_idx: 0,
            bmp_en: false,
            units: [Unit::default(); 2],
            hold: false,
            step: None,
            pend_ib: false,
            pend_cb: false,
            pend_drv: false,
            stop_new: false,
            stops: 0,
            rec: Record::default(),
            ipc_set: None,
            ev: 0,
            gen: 0,
            drive: Drive::default(),
        }
    }
}

/// What the IO block knows of the machine for a read (trx_dbg.v's inputs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ctx {
    /// The gate is shut: the machine is halted (the pause, the position hold or the debugger's hold).
    pub shut: bool,
    /// The app's C64_STOP (raw).
    pub fw_stop: bool,
}

impl Dbg {
    /// A debugger stop holds the machine: the hold and no step (trx_dbg.v:268 `stop`, with the gate shut).
    pub fn holds(&self) -> bool {
        self.hold && self.step.is_none()
    }

    /// The bitmap bit for `pc`.
    pub fn bmp_bit(&self, pc: u16) -> bool {
        self.bmp[usize::from(pc >> 5)] >> (pc & 31) & 1 != 0
    }

    /// CYCLES as a read shows it (bits 47:0).
    pub fn cycles_now(&self) -> u64 {
        (self.cycles + u64::from(self.ib_bias)) & 0xFFFF_FFFF_FFFF
    }

    pub fn instrs_now(&self) -> u32 {
        self.instrs.wrapping_add(u32::from(self.ib_bias))
    }

    /// STATUS (trx_dbg.v:397-398).
    pub fn status(&self, c: Ctx) -> u32 {
        let stop = self.hold && c.shut && self.step.is_none();
        let at_ib = self.ipc_set.map_or(true, |s| s.2);
        u32::from(self.stops) << 24
            | u32::from(self.own) << 20
            | u32::from(self.cmp_en) << 19
            | u32::from(self.bmp_en) << 18
            | u32::from(self.units[1].armed()) << 17
            | u32::from(self.units[0].armed()) << 16
            | u32::from(self.rec.reason) << 8
            | u32::from(self.step.is_some()) << 7
            | u32::from(!self.hold && c.shut) << 6
            | u32::from(c.fw_stop) << 5
            | u32::from(self.stop_new) << 3
            | u32::from(at_ib) << 2
            | u32::from(stop) << 1
            | u32::from(c.shut)
    }

    /// DRV_STAT (drv_dbg.v:123): `{DPC, comparator on b3, step b2, held b1, halted b0}`, halted = the machine's halt
    /// with no step of the drive, or the drive's own hold.
    pub fn drv_stat(&self, c: Ctx) -> u32 {
        let d = &self.drive;
        let halted = (c.shut && !d.step) || d.hold;
        u32::from(d.dpc) << 16 | u32::from(d.cmp & 0x1_0000 != 0) << 3 | u32::from(d.step) << 2 | u32::from(d.hold) << 1 | u32::from(halted)
    }

    /// A register as a read returns it (trx_dbg.v:391-405, drv_dbg.v:120-127); `side` makes the read's side effects
    /// (the CYCLES read latches CYCLES_HI, a BMP_DATA read moves BMP_IDX on). Write-only registers read 0.
    pub fn read(&mut self, off: u32, c: Ctx, side: bool) -> u32 {
        match off {
            CYCLES => {
                let v = self.cycles_now();
                if side {
                    self.cyc_hi = (v >> 32) as u16;
                }
                v as u32
            }
            CYCLES_HI => u32::from(self.cyc_hi),
            INSTRS => self.instrs_now(),
            o if o & !0xFFF == PAGE3 => match o & 0xFFF {
                STATUS => self.status(c),
                STOP0 => {
                    let r = &self.rec;
                    u32::from(r.pos) << 16 | u32::from(r.data) << 8 | u32::from(r.flags & 7) << 5 | u32::from(r.unit & 1) << 4 | u32::from(r.reason & 15)
                }
                STOP1 => u32::from(self.rec.addr) << 16 | u32::from(self.rec.ipc),
                BMP_DATA => {
                    let v = self.bmp[usize::from(self.bmp_idx)];
                    if side {
                        self.bmp_idx = (self.bmp_idx + 1) & 0x7FF;
                    }
                    v
                }
                HITS => u32::from(self.units[1].hits) << 16 | u32::from(self.units[0].hits),
                DRV_STAT => self.drv_stat(c),
                DCYCLES => self.drive.dcycles,
                _ => 0,
            },
            _ => 0,
        }
    }

    /// A register write at the C64's clock (trx_dbg.v:331-364, drv_dbg.v:52-61). `shut`: the gate is shut now.
    /// Returns what the C64 port has to do for it.
    pub fn write(&mut self, off: u32, v: u32, shut: bool) -> Act {
        match off {
            OWNER => {
                let own = v & 1 != 0;
                let changed = own != self.own;
                self.own = own;
                if changed {
                    return Act::Owner(own);
                }
            }
            CYC_CMP => {
                self.cyc_cmp = v;
                self.cmp_en = true;
                self.gen += 1;
            }
            CYC_CMP_EN => {
                self.cmp_en = v & 0x8000_0000 != 0;
                self.gen += 1;
            }
            o if o & !0xFFF == PAGE3 => match o & 0xFFF {
                CMD => return self.command(v as u8, shut),
                ARG => self.arg = v as u16,
                IPC_SET => self.ipc_set = Some((v as u16, (v >> 16) as u8, v & 0x100_0000 != 0)),
                BMP_IDX => self.bmp_idx = (v & 0x7FF) as u16,
                BMP_DATA => {
                    self.bmp[usize::from(self.bmp_idx)] = v;
                    self.bmp_idx = (self.bmp_idx + 1) & 0x7FF;
                    self.gen += 1;
                }
                DBG_CTRL => {
                    self.bmp_en = v & 1 != 0;
                    self.gen += 1;
                }
                r @ UNIT0..=0x0BF if r != HITS => {
                    let rel = r - UNIT0;
                    let n = (rel / UNIT_STRIDE) as usize;
                    if n < 2 {
                        self.units[n].write(rel % UNIT_STRIDE / 4, v);
                        self.gen += 1;
                    }
                }
                DRV_CMP => {
                    self.drive.cmp = v & 0x3_FFFF;
                    self.gen += 1;
                }
                _ => {}
            },
            _ => {}
        }
        Act::None
    }

    /// CMD (trx_dbg.v:341-364, drv_dbg.v:55-61).
    fn command(&mut self, cmd: u8, shut: bool) -> Act {
        match cmd {
            CMD_PAUSE_CB => {
                if shut {
                    // The gate is shut already: the hold, and only the record says so (trx_dbg.v:344-346); `ev` fires.
                    self.hold = true;
                    if !self.stop_new {
                        self.stop_new = true;
                        self.rec.reason = R_PAUSE;
                    }
                    self.ev += 1;
                    self.gen += 1;
                    return Act::None;
                }
                // On a running machine the hold and the record come with the stop at the next boundary (the RTL sets
                // the hold at once and records at the next edge; UE2 has no edge in between).
                self.pend_cb = true;
                self.gen += 1;
                Act::Pause { ib: false }
            }
            CMD_PAUSE_IB => {
                self.pend_ib = true;
                self.gen += 1;
                Act::Pause { ib: true }
            }
            CMD_RUN => {
                self.stop_new = false;
                Act::Run
            }
            CMD_STEP_INSTR | CMD_STEP_CYCLES if shut && self.step.is_none() => {
                self.stop_new = false;
                Act::Step(Step { ins: cmd == CMD_STEP_INSTR, n: self.arg.max(1) })
            }
            CMD_DRV_PAUSE => Act::DrvHold(true),
            CMD_DRV_RUN => Act::DrvHold(false),
            CMD_DRV_STEP => {
                self.stop_new = false;
                Act::DrvStep(self.arg.max(1))
            }
            CMD_CLEAR_STOP => {
                self.stop_new = false;
                Act::None
            }
            _ => Act::None,
        }
    }

    /// RUN taken (trx_dbg.v:369-381, the release branch; drv_dbg.v: RUN releases the drive too).
    pub fn release(&mut self) {
        self.hold = false;
        self.step = None;
        self.pend_ib = false;
        self.pend_cb = false;
        self.pend_drv = false;
        self.drive.hold = false;
        self.drive.step = false;
        self.ipc_set = None;
        self.ib_bias = false;
        self.gen += 1;
    }

    /// A step taken (the step branch): the hold stays, the gate opens for the step.
    pub fn start_step(&mut self, s: Step) {
        self.step = Some(s);
        self.hold = true;
        self.ipc_set = None;
        self.gen += 1;
    }

    /// A stop on the edge UE2 stands at (trx_dbg.v:315-329): the hold, the step and the drive's pending stop end, the
    /// record is written unless one waits, and `ev` fires. `cmp` clears the comparator.
    pub fn stop(&mut self, rec: Record, cmp: bool) {
        self.hold = true;
        self.step = None;
        self.pend_drv = false;
        if rec.reason == R_PAUSE {
            self.pend_ib = false;
            self.pend_cb = false;
        }
        if cmp {
            self.cmp_en = false;
        }
        self.stops = self.stops.wrapping_add(1);
        if !self.stop_new {
            self.stop_new = true;
            self.rec = rec;
        }
        self.ev += 1;
        self.ib_bias = true;
        self.gen += 1;
    }

    /// A drive step ended (drv_dbg.v `done_ev`; trx_dbg.v:319-329 with `drv_done`): the record (REASON 9) and `ev`;
    /// the C64 is not touched.
    pub fn drive_done(&mut self, rec: Record) {
        self.stops = self.stops.wrapping_add(1);
        if !self.stop_new {
            self.stop_new = true;
            self.rec = Record { reason: R_DRV_STEP, ..rec };
        }
        self.ev += 1;
    }

    /// The units, bitmap, comparator, pauses or step want the backend to look at boundaries.
    pub fn armed(&self) -> bool {
        self.bmp_en
            || self.units.iter().any(Unit::armed)
            || self.cmp_en
            || self.pend_ib
            || self.pend_cb
            || self.step.is_some()
            || self.drive.exec_cmp().is_some()
    }

    /// Units that are switched on but cannot be served on TRX64's boundaries (S39 §5 T1): (unit, why).
    pub fn refused_units(&self) -> Vec<(usize, &'static str)> {
        self.units
            .iter()
            .enumerate()
            .filter(|(_, u)| u.on() && !u.served())
            .map(|(i, u)| (i, if u.raster() { "raster source" } else { "every-edge type" }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHUT: Ctx = Ctx { shut: true, fw_stop: false };
    const RUNNING: Ctx = Ctx { shut: false, fw_stop: false };

    /// trx_dbg.v:394-404: CYCLES latches CYCLES_HI; write-only registers read 0; the units' registers too, but HITS.
    #[test]
    fn counters_and_write_only_registers() {
        let mut d = Dbg { cycles: 0x1234_5678_9ABC, instrs: 7, ..Dbg::default() };
        assert_eq!(d.read(CYCLES_HI, RUNNING, true), 0);
        assert_eq!(d.read(CYCLES, RUNNING, true), 0x5678_9ABC);
        d.cycles = 0x9999_0000_0000;
        assert_eq!(d.read(CYCLES_HI, RUNNING, true), 0x1234, "latched by the read of CYCLES");
        assert_eq!(d.read(INSTRS, RUNNING, true), 7);
        for off in [OWNER, CYC_CMP, CYC_CMP_EN, PAGE3 + CMD, PAGE3 + ARG, PAGE3 + IPC_SET, PAGE3 + BMP_IDX, PAGE3 + DBG_CTRL, PAGE3 + UNIT0, PAGE3 + DRV_CMP] {
            d.write(off, 0xFFFF_FFFF & !0xFF | 0x40, false);
            assert_eq!(d.read(off, RUNNING, true), 0, "{off:#x}");
        }
        d.units[1].hits = 3;
        d.units[0].hits = 2;
        assert_eq!(d.read(PAGE3 + HITS, RUNNING, true), 0x0003_0002);
        // IB bias: at a stop the fetch's edge is counted.
        d.ib_bias = true;
        assert_eq!((d.read(CYCLES, SHUT, true), d.read(INSTRS, SHUT, true)), (1, 8));
    }

    /// trx_dbg.v:173-182, :333-339: BMP_DATA writes and reads move BMP_IDX on.
    #[test]
    fn the_bitmap_index_moves_on() {
        let mut d = Dbg::default();
        d.write(PAGE3 + BMP_IDX, 0x600, false);
        d.write(PAGE3 + BMP_DATA, 0x0000_0001, false);
        d.write(PAGE3 + BMP_DATA, 0x8000_0000, false);
        assert!(d.bmp_bit(0xC000) && d.bmp_bit(0xC03F) && !d.bmp_bit(0xC001));
        d.write(PAGE3 + BMP_IDX, 0x600, false);
        assert_eq!([d.read(PAGE3 + BMP_DATA, RUNNING, true), d.read(PAGE3 + BMP_DATA, RUNNING, true)], [1, 0x8000_0000]);
        assert_eq!(d.bmp_idx, 0x602);
        d.write(PAGE3 + DBG_CTRL, 1, false);
        assert!(d.armed() && d.status(RUNNING) & 1 << 18 != 0);
    }

    /// trx_dbg.v:343-364: PAUSE_CB on a shut gate writes REASON 1 and STOP_NEW only, keeps the other fields; RUN and
    /// the steps clear STOP_NEW; a step on a running gate is ignored; ARG 0 counts 1.
    #[test]
    fn commands() {
        let mut d = Dbg::default();
        d.rec = Record { reason: 3, ipc: 0xC000, addr: 0xC000, data: 0xEA, flags: 1, unit: 0, pos: 5 };
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_PAUSE_CB), true), Act::None);
        assert_eq!((d.stop_new, d.rec.reason, d.rec.ipc, d.ev, d.hold), (true, R_PAUSE, 0xC000, 1, true));
        assert_eq!(d.status(SHUT) & 0xF0F, 0x10F, "REASON 1, STOP_NEW, at an IB, stop, shut");
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_STEP_INSTR), false), Act::None, "a step on a running gate");
        d.write(PAGE3 + ARG, 0, true);
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_STEP_INSTR), true), Act::Step(Step { ins: true, n: 1 }));
        assert!(!d.stop_new);
        d.start_step(Step { ins: true, n: 1 });
        assert_eq!(d.status(SHUT) & 0x86, 0x84, "b7 step, at an IB, no debugger stop while stepping");
        d.stop(Record { reason: R_STEP, flags: F_IB, ipc: 0xC002, addr: 0xC002, ..Record::default() }, false);
        assert_eq!((d.rec.reason, d.rec.ipc, d.stops, d.ev), (R_STEP, 0xC002, 1, 2));
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_RUN), true), Act::Run);
        d.release();
        assert!(!d.hold && d.status(SHUT) & 0x4A == 0x40, "b6: the gate shut by something else");
        d.write(PAGE3 + ARG, 5, false);
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_STEP_CYCLES), true), Act::Step(Step { ins: false, n: 5 }));
    }

    /// trx_dbg.v:281-286, :319-329: the record is written only while STOP_NEW is 0; every stop counts and fires.
    #[test]
    fn the_record_waits_for_clear_stop() {
        let mut d = Dbg::default();
        d.stop(Record { reason: R_BITMAP, ipc: 0xC000, ..Record::default() }, false);
        d.stop(Record { reason: R_CYC_CMP, ipc: 0xD000, ..Record::default() }, true);
        assert_eq!((d.rec.reason, d.stops, d.ev, d.cmp_en), (R_BITMAP, 2, 2, false));
        d.write(PAGE3 + CMD, u32::from(CMD_CLEAR_STOP), true);
        d.stop(Record { reason: R_CYC_CMP, ipc: 0xD000, ..Record::default() }, true);
        assert_eq!(d.read(PAGE3 + STOP1, SHUT, true), 0xD000);
        assert_eq!(d.read(PAGE3 + STOP0, SHUT, true) & 0xF, 7);
    }

    /// trx_dbg.v:216-240: range, data mask, register qualifier, PREV_RET, chain, PASS.
    #[test]
    fn unit_qualifiers_chain_and_pass() {
        let mut d = Dbg::default();
        let u = PAGE3 + UNIT0;
        d.write(u + 4, 0x07E7_0400, false);
        d.write(u + 8, 0xF000 | 0x50, false);
        d.write(u + 12, 4 | 3 << 8 | 0xF0 << 16 | 0xFF << 24, false);
        d.write(u, 1 | u32::from(T_WRITE) << 1, false);
        let x = d.units[0];
        assert!(x.armed() && x.kind() == T_WRITE);
        let regs = [0, 0, 0, 0xF5, 0];
        assert!(x.qualifies(0x0400, 0x5A, regs, false));
        assert!(!x.qualifies(0x07E8, 0x5A, regs, false), "range");
        assert!(!x.qualifies(0x0400, 0x6A, regs, false), "data mask");
        assert!(!x.qualifies(0x0400, 0x5A, [0, 0, 0, 0xEF, 0], false), "S >= $F0");
        let ret = Unit { ctrl: 1 | 2 | 0x4000, hi: 0xFFFF, ..Unit::default() };
        assert!(!ret.qualifies(1, 0, regs, false) && ret.qualifies(1, 0, regs, true), "PREV_RET");
        // Chain: unit 0 hits only when unit 1 matches on the same edge; PASS: the action from hit PASS + 1.
        let units = [Unit { ctrl: 0x201, pass: 0, ..Unit::default() }, Unit { ctrl: 1, pass: 2, hits: 1, ..Unit::default() }];
        assert_eq!(hits(&units, [true, false]), [(false, false), (false, false)]);
        assert_eq!(hits(&units, [true, true]), [(true, true), (true, false)], "unit 1 has 1 hit of 2 passed");
        // Raster source and every-edge units are not served.
        d.write(u, 1 | u32::from(T_EDGE) << 1, false);
        assert!(!d.units[0].armed() && d.status(RUNNING) & 1 << 16 == 0);
        assert_eq!(d.refused_units(), [(0, "every-edge type")]);
    }

    /// OWNER and the drive registers.
    #[test]
    fn owner_and_the_drive() {
        let mut d = Dbg::default();
        assert_eq!(d.write(OWNER, 1, false), Act::Owner(true));
        assert_eq!(d.write(OWNER, 1, false), Act::None);
        assert_eq!(d.status(Ctx { shut: false, fw_stop: true }) & 0x10_0020, 0x10_0020, "OWNER, C64_STOP");
        d.write(PAGE3 + DRV_CMP, 0x1_E853, false);
        assert_eq!(d.drive.exec_cmp(), Some(0xE853));
        d.write(PAGE3 + DRV_CMP, 0x3_E853, false);
        assert_eq!(d.drive.exec_cmp(), None, "the write type is not served");
        d.drive.dpc = 0xEBFF;
        assert_eq!(d.read(PAGE3 + DRV_STAT, SHUT, true), 0xEBFF_0009, "comparator on, halted with the machine");
        assert_eq!(d.write(PAGE3 + CMD, u32::from(CMD_DRV_STEP), true), Act::DrvStep(1));
        d.drive_done(Record { ipc: 0x1234, ..Record::default() });
        assert_eq!((d.rec.reason, d.rec.ipc, d.stop_new), (R_DRV_STEP, 0x1234, true));
    }
}
