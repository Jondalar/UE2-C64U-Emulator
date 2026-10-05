//! S39 phase A1 / S40: the board's engine on TRX64 (docs/specs/S40-trx-engine.md §4.2, §5, §6.3, §10).
//!
//! The board's C64 core tells its engine about the frame boundary (the snap on line 0, line 16), stops at a position
//! (`STOP_POS`) and shows its chips on the state bus. Here TRX64's run is cut at those points: the run stops at each
//! of them, the bridge looks at the machine there and hands the port events in order. TRX64 runs whole instructions,
//! so a point is reached at the first instruction boundary at or after it when the 6510 runs (gap T2), and exactly
//! when it is held (the chips run cycle by cycle).

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use trx64_core::c64_6510core::{IntStatus, IK_IRQ, IK_IRQPEND, IK_NMI, INTERRUPT_DELAY};
use trx64_core::c64re_snapshot::{capture_cia, capture_cpu, restore_cia, restore_cpu, CiaSnapshot, CpuSnapshot};
use trx64_core::{AccessCtx, Bank8, BankA, BankE, BusKind, MemConfig, Observer};
use ue2_core::c64host::{EngineCfg, SbShadow, Snap, TapEvent};
use ue2_core::devices::trx_sb::SbLoad;

use crate::trx_ff::{self, CpuState};
use crate::Trx64Backend;

/// The first raster cycle a snap may be on (x ≥ 96, c64_core.v:644).
const SNAP_CYCLE: u16 = 12;
/// The line the boundary falls back to without a snap (c64_core.v:646).
const LINE16: u16 = 16;
/// The cartridge RAM window in guest DDR (cart.rs `RAM_BASE`; the board's `cart_slot` RAM, S40 §4.3).
const CART_RAM: std::ops::Range<usize> = 0x00EF_0000..0x00F0_0000;
/// The REU's memory in guest DDR (`REU_MEMORY_BASE`, c64.h:14), up to 16 MB.
const REU_MEMORY: std::ops::Range<usize> = 0x0100_0000..0x0200_0000;
/// Side-table entries kept per chip: the app's timeline holds 4096 frames (`TL_CAP`, trx_ring.cc:103).
const SIDE_CAP: usize = 4096;

/// What the C64 RAM and colour RAM hold as far as the taps have seen: the old byte of a write is the memory's own
/// (c64_mem.v:91-105), which TRX64's `on_bus` does not give (S40 gap T1).
pub(crate) struct Mirror {
    ram: Box<[u8; 0x1_0000]>,
    col: Box<[u8; 0x400]>,
}

/// Where a write lands (TRX64's `FullBus::write`, full.rs:1087-1150, with the instruction's memory configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Landing {
    Ram,
    Colour,
    None,
    /// A window the cartridge may take: decided by the RAM after the instruction.
    Cart,
}

fn landing(addr: u16, c: &MemConfig) -> Landing {
    match addr {
        0x0000..=0x0001 => Landing::Ram,
        0xD000..=0xDFFF if c.io => {
            if (0xD800..0xDC00).contains(&addr) {
                Landing::Colour
            } else {
                Landing::None
            }
        }
        0xD000..=0xDFFF => Landing::Ram,
        0x8000..=0x9FFF if c.bank8 == Bank8::CartLo => Landing::Cart,
        0xA000..=0xBFFF if c.bank_a == BankA::CartHi => Landing::Cart,
        0xA000..=0xCFFF if c.ultimax => Landing::None,
        0xE000..=0xFFFF if c.bank_e == BankE::CartHiUltimax => Landing::Cart,
        0x1000..=0x7FFF if c.ultimax => Landing::None,
        _ => Landing::Ram,
    }
}

/// The windows a cartridge may take writes in, as bits: 0 ROML, 1 ROMH, 2 Ultimax ROMH.
fn cart_windows(c: &MemConfig) -> u8 {
    u8::from(c.bank8 == Bank8::CartLo) | u8::from(c.bank_a == BankA::CartHi) << 1 | u8::from(c.bank_e == BankE::CartHiUltimax) << 2
}

fn in_cart_window(addr: u16, windows: u8) -> bool {
    match addr {
        0x8000..=0x9FFF => windows & 1 != 0,
        0xA000..=0xBFFF => windows & 2 != 0,
        0xE000..=0xFFFF => windows & 4 != 0,
        _ => false,
    }
}

/// The observer around a run's own while the engine records: every write in order, and a halt at the next boundary
/// after a write to the CPU port (the memory configuration changes) or into a window a cartridge may take.
pub(crate) struct Tapped<'a, O> {
    pub inner: &'a mut O,
    pub writes: &'a mut Vec<(u16, u8, u64)>,
    pub windows: u8,
    /// A halt this run asked for, and one the run's own observer asked for.
    pub own_halt: bool,
    pub inner_halt: bool,
}

impl<O: Observer> Observer for Tapped<'_, O> {
    const RECORDS_VIC: bool = O::RECORDS_VIC;

    #[inline(always)]
    fn on_instruction(&mut self, pc: u16, op: u8, b1: u8, b2: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, clk: u64) {
        self.inner.on_instruction(pc, op, b1, b2, a, x, y, sp, p, clk);
    }

    #[inline(always)]
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        if matches!(kind, BusKind::Write | BusKind::DummyWrite) {
            self.writes.push((addr, value, clk));
        }
        self.inner.on_bus(kind, addr, value, pc, clk, old);
    }

    #[inline(always)]
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.inner.on_interrupt(vector, clk);
    }

    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8, cx: AccessCtx) -> bool {
        let mine = kind == BusKind::Write && (addr < 2 || in_cart_window(addr, self.windows));
        let inner = self.inner.on_access(kind, addr, value, cx);
        self.own_halt |= mine;
        self.inner_halt |= inner;
        inner || mine
    }

    fn on_vic_reg(&mut self, clk: u64, raster_y: u16, kind: u8, value: u8) {
        self.inner.on_vic_reg(clk, raster_y, kind, value);
    }
}

/// The exact TRX64 state of a CPU image UE2 made (S40 §5.3).
struct SideCpu {
    cpu: CpuSnapshot,
    int: IntStatus,
    op_info: u32,
    op_addr: u16,
    port: (u8, u8),
    clk: u64,
    /// RAM and colour RAM at the snap, as a hash: a load checks the memories the app put back against it.
    mem: u64,
}

/// The exact TRX64 state of a CIA image UE2 made.
struct SideCia {
    cia: CiaSnapshot,
    clk: u64,
}

/// The side table: per chip, the kept parts by the hash of their image, oldest dropped first.
#[derive(Default)]
pub(crate) struct Side {
    cpu: HashMap<u64, SideCpu>,
    cia: [HashMap<u64, SideCia>; 2],
    order: VecDeque<[u64; 3]>,
}

/// RAM and colour RAM as a hash (FNV-1a over 8-byte words).
fn mem_hash(ram: &[u8; 0x1_0000], col: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for c in ram.chunks_exact(8).chain(col.chunks(8)) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn key(words: &[u32]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    words.hash(&mut h);
    h.finish()
}

/// A clock of a kept snapshot moved by `delta`; 0 (never set) and the never-sentinels stay.
fn shift(v: u64, delta: i128) -> u64 {
    if v == 0 || v >= u64::MAX / 2 {
        v
    } else {
        (i128::from(v) + delta).max(1) as u64
    }
}

fn shift_i(v: i64, delta: i128) -> i64 {
    // The JSON never-sentinel (`Number.MAX_SAFE_INTEGER`) and negative values stay.
    if v <= 0 || v >= (1 << 53) - 1 {
        v
    } else {
        (i128::from(v) + delta).max(1) as i64
    }
}

/// Every clock field of a CIA's context moved by `delta` (S40 gap T5).
fn shift_cia(s: &mut CiaSnapshot, delta: i128) {
    for v in [&mut s.rdi, &mut s.ifr_clock, &mut s.ta_clk, &mut s.tb_clk, &mut s.todclk, &mut s.read_clk] {
        *v = shift_i(*v, delta);
    }
    for v in [&mut s.ta_alarmclk, &mut s.tb_alarmclk].into_iter().flatten() {
        *v = shift_i(*v, delta);
    }
    if let Some(a) = s.alarms.as_mut() {
        for v in [&mut a.idle, &mut a.ta, &mut a.tb, &mut a.tod, &mut a.sdr].into_iter().flatten() {
            *v = shift(*v, delta);
        }
    }
}

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
    /// While the log is on: the memories as the taps saw them, and the writes of the run in progress.
    pub mirror: Option<Mirror>,
    pub writes: Vec<(u16, u8, u64)>,
    /// The clock and the counter where the run in progress began: a write's cycle on the counter.
    pub run_clk: u64,
    pub run_cycles: u64,
    /// A run is in progress (`run_to_plain`): the count is `run_cycles` plus what ran since `run_clk`.
    pub in_run: bool,
    /// The merged watch table of the last tapped run and what it was built from (base table, cartridge windows).
    pub watch: Option<(Box<[u8; 0x1_0000]>, (usize, u8))>,
    pub side: Side,
    /// Loads of images UE2 did not make (M2/M3: the decoder).
    pub misses: u64,
    /// Loads of UE2's own CPU images, and of them those whose memories were not the snap's.
    pub loads: u64,
    pub mem_mismatches: u64,
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

    /// What the engine wants from now on (`C64Backend::set_engine`). The log going on takes the memories as they
    /// stand into the mirror.
    pub(crate) fn tap_set_engine(&mut self, cfg: EngineCfg) {
        if cfg == self.tap.cfg {
            return;
        }
        if cfg.records && self.tap.mirror.is_none() {
            let mut col = Box::new([0u8; 0x400]);
            for (c, s) in col.iter_mut().zip(&self.m.io_shadow[0x800..0xC00]) {
                *c = s & 0x0F;
            }
            self.tap.mirror = Some(Mirror { ram: self.m.ram.clone(), col });
        } else if !cfg.records {
            self.tap.mirror = None;
            self.tap.writes.clear();
        }
        self.hwlog.set_engine(cfg.records);
        self.tap.cfg = cfg;
        self.tap_replan();
    }

    /// Whether the taps record.
    pub(crate) fn recording_writes(&self) -> bool {
        self.tap.mirror.is_some()
    }

    /// The engine counter at C64 cycle `clk` of the run in progress.
    fn cycle_at(&self, clk: u64) -> u64 {
        self.tap.run_cycles + clk.saturating_sub(self.tap.run_clk)
    }

    /// The writes of a run that began under memory configuration `cfg`, in order: where each landed, with the
    /// memory's old byte from the mirror. A write into a cartridge's window landed if the RAM holds the last byte
    /// written there.
    pub(crate) fn tap_resolve(&mut self, cfg: MemConfig) {
        let writes = std::mem::take(&mut self.tap.writes);
        let Some(mut mir) = self.tap.mirror.take() else { return };
        for (i, &(addr, val, clk)) in writes.iter().enumerate() {
            let cycle = self.cycle_at(clk);
            let to = match landing(addr, &cfg) {
                Landing::Cart => {
                    let last = writes[i..].iter().rev().find(|w| w.0 == addr).map_or(val, |w| w.1);
                    if self.m.ram[usize::from(addr)] == last {
                        Landing::Ram
                    } else {
                        Landing::None
                    }
                }
                l => l,
            };
            match to {
                Landing::Ram => {
                    let old = std::mem::replace(&mut mir.ram[usize::from(addr)], val);
                    self.tap.events.push(TapEvent::Ram { addr, old, new: val, outside: false, cycle });
                }
                Landing::Colour => {
                    let a = usize::from(addr & 0x3FF);
                    let old = std::mem::replace(&mut mir.col[a], val & 0x0F);
                    self.tap.events.push(TapEvent::Colour { addr: addr & 0x3FF, old, new: val & 0x0F, outside: false, cycle });
                }
                _ => {}
            }
        }
        self.tap.mirror = Some(mir);
        let mut writes = writes;
        writes.clear();
        self.tap.writes = writes;
        self.tap_ddr();
    }

    /// The guest DDR writes the cartridge logic and the REU made: cartridge RAM records, the REU's flag.
    fn tap_ddr(&mut self) {
        let cycle = self.tap.cycles;
        let mut reu = false;
        for (off, old, new) in self.hwlog.take_engine() {
            if CART_RAM.contains(&off) {
                let off = (off - CART_RAM.start) as u16;
                self.tap.events.push(TapEvent::CartRam { off, old, new, cycle });
            } else if REU_MEMORY.contains(&off) {
                reu = true;
            }
        }
        if reu && self.m.reu().is_some() {
            self.tap.events.push(TapEvent::Reu);
        }
    }

    /// Writes no tap saw (the REU's DMA, a monitor's poke; `outside`: a restore behind the firmware's back): the
    /// difference of the memories to the mirror, logged now (S40 gap T1).
    pub(crate) fn tap_compare(&mut self, outside: bool) {
        let Some(mut mir) = self.tap.mirror.take() else { return };
        let cycle = self.tap.cycles;
        let mut any = false;
        if *mir.ram != *self.m.ram {
            for (a, (m, &r)) in mir.ram.iter_mut().zip(self.m.ram.iter()).enumerate() {
                if *m != r {
                    self.tap.events.push(TapEvent::Ram { addr: a as u16, old: *m, new: r, outside, cycle });
                    *m = r;
                    any = true;
                }
            }
        }
        for (a, (m, &s)) in mir.col.iter_mut().zip(&self.m.io_shadow[0x800..0xC00]).enumerate() {
            let r = s & 0x0F;
            if *m != r {
                self.tap.events.push(TapEvent::Colour { addr: a as u16, old: *m, new: r, outside, cycle });
                *m = r;
            }
        }
        self.tap.mirror = Some(mir);
        if any && !outside && self.m.reu().is_some() {
            self.tap.events.push(TapEvent::Reu);
        }
        self.tap_ddr();
    }

    /// A firmware window write through the bus (no MEMONLY): the machine's port A, landing as the PLA decided under
    /// `cfg` (before the write); `before` is what the RAM held.
    pub(crate) fn tap_bus_write(&mut self, addr: u16, val: u8, cfg: MemConfig, before: u8) {
        let to = match landing(addr, &cfg) {
            Landing::Cart if self.m.ram[usize::from(addr)] != before || before == val => Landing::Ram,
            Landing::Cart => Landing::None,
            l => l,
        };
        let cycle = self.tap.cycles;
        if let Some(mir) = self.tap.mirror.as_mut() {
            match to {
                Landing::Ram => {
                    let old = std::mem::replace(&mut mir.ram[usize::from(addr)], val);
                    self.tap.events.push(TapEvent::Ram { addr, old, new: val, outside: false, cycle });
                }
                Landing::Colour => {
                    let a = usize::from(addr & 0x3FF);
                    let old = std::mem::replace(&mut mir.col[a], val & 0x0F);
                    self.tap.events.push(TapEvent::Colour { addr: addr & 0x3FF, old, new: val & 0x0F, outside: false, cycle });
                }
                _ => {}
            }
            self.tap_ddr();
        }
    }

    /// A write through the RAM's second port (MEMONLY, the session): an outside record with the RAM's old byte.
    pub(crate) fn tap_outside_ram(&mut self, addr: u16, val: u8) {
        let cycle = self.tap.cycles;
        if let Some(mir) = self.tap.mirror.as_mut() {
            let old = std::mem::replace(&mut mir.ram[usize::from(addr)], val);
            self.tap.events.push(TapEvent::Ram { addr, old, new: val, outside: true, cycle });
        }
    }

    /// A write to colour RAM through its second port (the session's commit): an outside record.
    pub(crate) fn tap_outside_colour(&mut self, addr: u16, val: u8) {
        let cycle = self.tap.cycles;
        if let Some(mir) = self.tap.mirror.as_mut() {
            let a = usize::from(addr & 0x3FF);
            let old = std::mem::replace(&mut mir.col[a], val & 0x0F);
            self.tap.events.push(TapEvent::Colour { addr: addr & 0x3FF, old, new: val & 0x0F, outside: true, cycle });
        }
    }

    /// APPLY's byte: into RAM through the second port, logged by nothing; the mirror follows.
    pub(crate) fn tap_apply(&mut self, addr: u16, val: u8) {
        self.m.ram[usize::from(addr)] = val;
        if let Some(mir) = self.tap.mirror.as_mut() {
            mir.ram[usize::from(addr)] = val;
        }
    }

    /// The watch table for a tapped run: `base` (the run's own, named `base_id`) with the CPU port and the
    /// cartridge's windows; kept while neither changes.
    pub(crate) fn tap_watch(&mut self, base_id: usize, base: Option<&[u8; 0x1_0000]>) {
        let windows = cart_windows(&self.m.memconfig);
        let id = (base_id, windows);
        if self.tap.watch.as_ref().is_none_or(|(_, k)| *k != id) {
            let mut t = Box::new(base.copied().unwrap_or([0; 0x1_0000]));
            t[0] = 1;
            t[1] = 1;
            for (a, b) in t.iter_mut().enumerate() {
                if in_cart_window(a as u16, windows) {
                    *b = 1;
                }
            }
            self.tap.watch = Some((t, id));
        }
    }

    /// The cartridge windows of the memory configuration now.
    pub(crate) fn cart_windows_now(&self) -> u8 {
        cart_windows(&self.m.memconfig)
    }

    /// RAM and colour RAM as they stand, hashed.
    fn mem_now(&self) -> u64 {
        let col: Vec<u8> = self.m.io_shadow[0x800..0xC00].iter().map(|c| c & 0x0F).collect();
        mem_hash(&self.m.ram, &col)
    }

    /// Keep the exact TRX64 parts of the images of a snap (S40 §5.3).
    fn side_keep(&mut self, sh: &SbShadow) {
        let clk = self.m.c64_core.clk;
        let keys = [key(&sh.cpuf), key(&sh.ciaf[0]), key(&sh.ciaf[1])];
        let mem = self.mem_now();
        let side = &mut self.tap.side;
        side.cpu.insert(
            keys[0],
            SideCpu {
                cpu: capture_cpu(&self.m),
                int: self.m.c64_int.clone(),
                op_info: self.m.c64_core.last_opcode_info,
                op_addr: self.m.c64_core.last_opcode_addr,
                port: (self.m.port_dir, self.m.port_data),
                clk,
                mem,
            },
        );
        side.cia[0].insert(keys[1], SideCia { cia: capture_cia(&self.m.cia1), clk });
        side.cia[1].insert(keys[2], SideCia { cia: capture_cia(&self.m.cia2), clk });
        side.order.push_back(keys);
        while side.order.len() > SIDE_CAP {
            let old = side.order.pop_front().expect("over the cap");
            if !side.order.iter().any(|k| k[0] == old[0]) {
                side.cpu.remove(&old[0]);
            }
            for i in 0..2 {
                if !side.order.iter().any(|k| k[1 + i] == old[1 + i]) {
                    side.cia[i].remove(&old[1 + i]);
                }
            }
        }
    }

    /// `SB_LOAD`: the staged images load into the chips, before the machine moves on. An image UE2 made loads its
    /// kept parts with their clocks moved to now; one it did not make needs the decoder (M2/M3) and loads nothing.
    pub(crate) fn tap_load(&mut self, load: &SbLoad) {
        let now = self.m.c64_core.clk;
        if let Some(w) = load.cpu {
            let kept = self.tap.side.cpu.contains_key(&key(&w));
            if !kept && !self.tap_cpu_write(&w) {
                self.tap_miss("6502");
            }
            match self.tap.side.cpu.get(&key(&w)).filter(|_| kept) {
                Some(e) => {
                    // The app put the memories back first (APPLY, colour RAM by the bus): they must be the snap's.
                    let (back, mem) = (now.saturating_sub(e.clk), self.mem_now() == e.mem);
                    self.tap.loads += 1;
                    self.tap.mem_mismatches += u64::from(!mem);
                    eprintln!(
                        "c64: SB_LOAD: the 6510 and the CIAs of the snap {back} cycles back; RAM and colour RAM {}",
                        if mem { "equal to the snap's" } else { "DIFFER from the snap's" }
                    );
                    let e = self.tap.side.cpu.get(&key(&w)).expect("found above");
                    let delta = i128::from(now) - i128::from(e.clk);
                    let mut cpu = e.cpu.clone();
                    cpu.cycles = now as i64;
                    let mut int = e.int.clone();
                    let (op_info, op_addr, port) = (e.op_info, e.op_addr, e.port);
                    for v in [&mut int.irq_clk, &mut int.nmi_clk, &mut int.last_stolen_cycles_clk, &mut int.irq_pending_clk] {
                        *v = shift(*v, delta);
                    }
                    for v in int.dma_start_clk.iter_mut() {
                        *v = shift(*v, delta);
                    }
                    restore_cpu(&mut self.m, &cpu);
                    self.m.c64_int = int;
                    self.m.c64_core.last_opcode_info = op_info;
                    self.m.c64_core.last_opcode_addr = op_addr;
                    // The port's flip-flops only; RAM[0]/[1] are the RAM's own and came back by APPLY.
                    (self.m.port_dir, self.m.port_data) = port;
                    self.update_pla();
                }
                None => {}
            }
        }
        for (i, w) in load.cia.iter().enumerate() {
            let Some(w) = w else { continue };
            match self.tap.side.cia[i].get(&key(w)) {
                Some(e) => {
                    let mut cia = e.cia.clone();
                    shift_cia(&mut cia, i128::from(now) - i128::from(e.clk));
                    let (chip, name) = if i == 0 { (&mut self.m.cia1, "cia1") } else { (&mut self.m.cia2, "cia2") };
                    restore_cia(chip, &cia, name);
                    if i == 1 {
                        // The port's lines to the serial bus and the VIC bank, as a load of the chip drives them.
                        let pa = self.m.cia2.pa_out();
                        self.m.cia2_pa_out = pa;
                        self.m.iec.iecbus_cpu_undump(pa ^ 0xFF);
                    }
                }
                None => self.tap_miss(if i == 0 { "CIA 1" } else { "CIA 2" }),
            }
        }
    }

    /// S39 §3.4, the CPU write path: an image that is the machine's own as it stands with only the registers, the
    /// PC, IR, the write-back controls (`copy_d2p`, `flags_update`, `trigger_upd`) and `din_r` changed is the
    /// debugger's write (trxmon's `hw_cpu_write`, hw_trx.c:190-220). A, X, Y, S, P load into TRX64; a new PC sends the
    /// 6510 there, and TRX64 fetches the opcode itself. TRX64 has no pending write-back to take out. False for any
    /// other image.
    fn tap_cpu_write(&mut self, w: &[u32; 7]) -> bool {
        use trx_ff::cpu::{A, CTL, DIN, IR, PC};
        let mut mask = [0u32; 7];
        for (lo, n) in [(A, 56), (IR, 8), (CTL + 12, 1), (CTL + 15, 2), (DIN, 8)] {
            trx_ff::put(&mut mask, lo, n, u64::MAX);
        }
        let now = self.shadow_now().cpuf;
        if (0..7).any(|i| (w[i] ^ now[i]) & !mask[i] != 0) {
            return false;
        }
        let f = |lo: usize, n: usize| (0..n).fold(0u64, |v, b| v | u64::from(w[(lo + b) / 32] >> ((lo + b) % 32) & 1) << b);
        let c = &mut self.m.c64_core;
        c.reg_a = f(A, 8) as u8;
        c.reg_x = f(A + 8, 8) as u8;
        c.reg_y = f(A + 16, 8) as u8;
        c.reg_sp = f(A + 24, 8) as u8;
        c.set_status_composite(f(A + 32, 8) as u8);
        let pc = f(PC, 16) as u16;
        if pc != c.reg_pc {
            c.reg_pc = pc;
        }
        self.m.sync_after_monitor();
        eprintln!("c64: SB_LOAD: the CPU write path: A X Y S P and PC ${pc:04x} into the 6510");
        true
    }

    fn tap_miss(&mut self, chip: &str) {
        self.tap.misses += 1;
        eprintln!(
            "c64: SB_LOAD of a {chip} image UE2 did not make: not loaded (the decoder of foreign images is M2/M3,              S40 §0)"
        );
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
        if !self.stands() {
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
            self.tap_compare(false);
            self.tap.events.push(TapEvent::Line16 { cycle: self.tap.cycles, x: 8 * cycle + 7 });
            self.tap.next_l16 = Some(self.next_at(LINE16, 0));
        }
        if self.tap.next_snap.is_some_and(|s| clk >= s) {
            // The 6510 held (C64_STOP, reset) makes no opcode fetch: no snap in this frame.
            let held = self.stopped || self.reset_held;
            if line == 0 && cycle >= SNAP_CYCLE && !held {
                if self.snap_conditions() {
                    // The writes no tap saw belong to the frame that ends here (S40 gap T1).
                    self.tap_compare(false);
                    let snap = self.snap_now();
                    self.side_keep(&snap.shadow);
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

    fn rams(events: &[TapEvent]) -> Vec<(u16, u8, u8, bool)> {
        events
            .iter()
            .filter_map(|e| if let TapEvent::Ram { addr, old, new, outside, .. } = *e { Some((addr, old, new, outside)) } else { None })
            .collect()
    }

    /// Code at `$C000` and the 6510 sent there, as a debugger would put it: no tap sees it.
    fn put_code(c64: &mut Trx64Backend, code: &[u8]) {
        c64.m.ram[0xC000..0xC000 + code.len()].copy_from_slice(code);
        c64.m.c64_core.reg_pc = 0xC000;
    }

    /// S40 §4.3: stream 0 is every write that lands in RAM with the RAM's own old byte (also under BASIC ROM and at
    /// `$00/$01`), an RMW's two writes, nothing for I/O; colour RAM its nibbles; a `$01` write changes where the
    /// next store lands; the firmware's MEMONLY write is outside, its bus write the machine's.
    #[test]
    fn the_taps_log_where_writes_land() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.m.ram[0xA000] = 0x77;
        c64.m.ram[0xD020] = 0x88;
        let col0 = c64.m.io_shadow[0x800] & 0x0F;
        #[rustfmt::skip]
        let code = [
            0x78,                   // SEI
            0xA9, 0x55,             // LDA #$55
            0x8D, 0x00, 0x04,       // STA $0400
            0xEE, 0x00, 0x04,       // INC $0400
            0x8D, 0x00, 0xA0,       // STA $A000 (under BASIC)
            0x8D, 0x20, 0xD0,       // STA $D020 (I/O)
            0xA9, 0x03,             // LDA #$03
            0x8D, 0x00, 0xD8,       // STA $D800 (colour RAM)
            0xA9, 0x34,             // LDA #$34
            0x85, 0x01,             // STA $01 (all RAM)
            0xA9, 0x66,             // LDA #$66
            0x8D, 0x20, 0xD0,       // STA $D020 (now RAM)
            0xA9, 0x37,             // LDA #$37
            0x85, 0x01,             // STA $01
            0x4C, 0x22, 0xC0,       // JMP *
        ];
        let before = c64.m.ram[0x0400];
        let port_ram = c64.m.ram[1];
        put_code(&mut c64, &code);
        c64.set_engine(EngineCfg { boundaries: false, records: true });
        run_ms(&mut c64, &mut now, 2);
        let ev = c64.take_tap();
        assert_eq!(
            rams(&ev),
            [
                (0x0400, before, 0x55, false),
                (0x0400, 0x55, 0x55, false),
                (0x0400, 0x55, 0x56, false),
                (0xA000, 0x77, 0x55, false),
                (0x0001, port_ram, 0x34, false),
                (0xD020, 0x88, 0x66, false),
                (0x0001, 0x34, 0x37, false),
            ]
        );
        let cols: Vec<_> = ev.iter().filter(|e| matches!(e, TapEvent::Colour { .. })).collect();
        assert_eq!(cols.len(), 1);
        assert!(matches!(*cols[0], TapEvent::Colour { addr: 0, old, new: 3, outside: false, .. } if old == col0));
        // The firmware's window: MEMONLY through the second port, a bus write as the machine's.
        c64.dma_write(0x0402, 0x11, true);
        c64.dma_write(0x0403, 0x22, false);
        c64.dma_write(0xD021, 0x05, false);
        assert_eq!(rams(&c64.take_tap())[..2], [(0x0402, 0x20, 0x11, true), (0x0403, 0x20, 0x22, false)], "screen spaces before");
        // APPLY's byte: no record, and the mirror follows (a later write's old byte is it).
        c64.apply_ram(0x0400, 0x99);
        assert!(c64.take_tap().is_empty());
        c64.dma_write(0x0400, 0x9A, true);
        assert_eq!(rams(&c64.take_tap()), [(0x0400, 0x99, 0x9A, true)]);
        // A poke no tap saw comes out of the compare at the end of the slice.
        c64.m.ram[0x0410] = 0xEE;
        run_ms(&mut c64, &mut now, 1);
        assert_eq!(rams(&c64.take_tap()), [(0x0410, 0x20, 0xEE, false)]);
    }

    /// S40 §4.2: every write of an instruction that ended at or before the snap's boundary comes before the snap,
    /// every later one after it (the F4 counts).
    #[test]
    fn writes_and_snaps_in_order() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_engine(EngineCfg { boundaries: true, records: true });
        run_ms(&mut c64, &mut now, 100);
        let ev = c64.take_tap();
        let mut last_snap = None;
        let (mut before, mut after) = (0, 0);
        for e in &ev {
            match e {
                TapEvent::Snap(s) => last_snap = Some(s.cycle),
                TapEvent::Ram { cycle, .. } => {
                    if let Some(s) = last_snap {
                        assert!(*cycle >= s, "a write of cycle {cycle} after the snap at {s}");
                        after += 1;
                    } else {
                        before += 1;
                    }
                }
                _ => {}
            }
        }
        assert!(before > 0 && after > 0 && snaps(&ev).len() >= 4);
        for (i, e) in ev.iter().enumerate() {
            if let TapEvent::Snap(s) = e {
                let next = ev[i + 1..].iter().find_map(|e| if let TapEvent::Ram { cycle, .. } = e { Some(*cycle) } else { None });
                let prev = ev[..i].iter().rev().find_map(|e| if let TapEvent::Ram { cycle, .. } = e { Some(*cycle) } else { None });
                assert!(prev.is_none_or(|p| p < s.cycle) && next.is_none_or(|n| n >= s.cycle), "{prev:?} {} {next:?}", s.cycle);
            }
        }
    }

    /// S40 §4.3, §0: the REU's DMA into C64 RAM is a stream 0 record (found by the compare, T1), and the REU's flag
    /// comes with it; its stash writes no C64 RAM but raises the flag too.
    #[test]
    fn the_reus_dma_is_logged() {
        let Some((mut c64, mut now)) = booted() else { return };
        let mut ddr = crate::tests::ddr();
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);
        let reu_base = 0x0100_0000;
        for i in 0..16 {
            ddr[reu_base + i] = 0xB0 + i as u8;
        }
        let code = crate::tests::reu_program(0x91, 0x0500, 0, 16);
        crate::tests::load(&mut c64, 0xC000, &code);
        c64.set_engine(EngineCfg { boundaries: false, records: true });
        c64.m.c64_core.reg_pc = 0xC000;
        crate::tests::run_ms_lent(&mut c64, &mut now, 5, &mut ddr);
        let ev = c64.take_tap();
        let fetched: Vec<_> = rams(&ev).into_iter().filter(|r| (0x0500..0x0510).contains(&r.0)).collect();
        assert_eq!(fetched.len(), 16, "{ev:?}");
        assert!(fetched.iter().enumerate().all(|(i, r)| r.2 == 0xB0 + i as u8 && !r.3));
        assert!(ev.contains(&TapEvent::Reu));
    }

    /// S40 §4.3: the cartridge logic's writes into its RAM (`0xEF0000 + a`) are stream 1 space 2 records with the old
    /// byte from DDR; the session's commit to colour RAM is an outside record; S38's restore behind the firmware's
    /// back gives outside records of what changed.
    #[test]
    fn cartridge_ram_the_commit_and_a_restore() {
        let mut c64 = Trx64Backend::new(std::path::Path::new("/nonexistent"));
        let mut ddr = crate::tests::ddr();
        c64.lend_ddr(Some(&mut ddr));
        c64.set_reset(true);
        c64.set_cart(0x1B, &[]);
        c64.dma_write(0xDE00, 0x20, false);
        c64.dma_write(0x0001, 0x35, false);
        c64.set_engine(EngineCfg { boundaries: false, records: true });
        c64.dma_write(0x8123, 0x5A, false);
        let ev = c64.take_tap();
        assert!(ev.contains(&TapEvent::CartRam { off: 0x123, old: 0, new: 0x5A, cycle: 0 }), "{ev:?}");
        c64.io_commit(&[(0xD800, 0x07), (0xD020, 0x01)]);
        let ev = c64.take_tap();
        assert!(matches!(ev[..], [TapEvent::Colour { addr: 0, new: 7, outside: true, .. }]), "{ev:?}");
        c64.lend_ddr(None);

        let Some((mut c64, mut now)) = crate::tests::ringed() else { return };
        c64.m.ram[0xC000] = 0x11;
        run_ms(&mut c64, &mut now, 60);
        let back = c64.ring().unwrap().ids()[0].0.clone();
        c64.m.ram[0xC000] = 0x22;
        run_ms(&mut c64, &mut now, 60);
        c64.set_engine(EngineCfg { boundaries: false, records: true });
        c64.ring_goto(&back, crate::RingMode::StepBack).unwrap();
        let r = rams(&c64.take_tap());
        assert!(r.contains(&(0xC000, 0x22, 0x11, true)), "{r:?}");
        assert!(r.iter().all(|x| x.3), "all outside");
    }

    /// S40 §5.3: an image UE2 made loads its kept parts: the CPU and both CIAs as they were at the snap, their
    /// clocks moved to now, the machine running on from there as it did.
    #[test]
    fn a_load_of_ue2s_own_image_is_exact() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_engine(EngineCfg { boundaries: true, records: false });
        run_ms(&mut c64, &mut now, 25);
        let ev = c64.take_tap();
        let snap = snaps(&ev).last().map(|s| (*s).clone()).expect("a snap");
        // Where it was: the registers, the CIAs as read, and what it ran next.
        let regs = snap.shadow.cpu;
        let cia_read = snap.shadow.cia;
        // Run on two frames, then stand the machine on line 0 at the snap's cycle with the 6510 held.
        run_ms(&mut c64, &mut now, 40);
        let cycle = (snap.x - 7) / 8;
        c64.set_stopped(true);
        c64.set_stop_pos(Some(cycle as u8));
        run_ms(&mut c64, &mut now, 30);
        assert_eq!(c64.raster(), (0, cycle));
        let load = SbLoad { cpu: Some(snap.shadow.cpuf), cia: [Some(snap.shadow.ciaf[0]), Some(snap.shadow.ciaf[1])] };
        c64.tap_load(&load);
        assert_eq!(c64.tap.misses, 0);
        let now_shadow = c64.shadow_now();
        assert_eq!(now_shadow.cpu, regs, "A X Y S P PC and the port as at the snap");
        assert_eq!(now_shadow.cia[0][..4], cia_read[0][..4], "CIA 1 as read: timers, TOD, ICR as at the snap");
        assert_eq!(now_shadow.cia[1], cia_read[1]);
        // A foreign image (the address latch differs from the machine's): nothing loads.
        let mut foreign = snap.shadow.cpuf;
        foreign[2] ^= 1;
        c64.tap_load(&SbLoad { cpu: Some(foreign), cia: [None, None] });
        assert_eq!((c64.tap.misses, c64.shadow_now().cpu), (1, regs));
        // S39 §3.4: the machine's own image with other registers is the CPU write path: A loads, nothing else moves.
        let mut write = c64.shadow_now().cpuf;
        trx_ff::put(&mut write, trx_ff::cpu::A, 8, 0x77);
        trx_ff::put(&mut write, trx_ff::cpu::IR, 8, 0xEA);
        c64.tap_load(&SbLoad { cpu: Some(write), cia: [None, None] });
        assert_eq!((c64.tap.misses, c64.m.c64_core.reg_a, c64.shadow_now().cpu[1]), (1, 0x77, regs[1]));
        c64.m.c64_core.reg_a = regs[0] as u8;
        c64.release_pos();
        c64.set_stopped(false);
        run_ms(&mut c64, &mut now, 20);
        assert!(snaps(&c64.take_tap()).iter().all(|s| (0xE5CD..=0xE5D5).contains(&(s.shadow.cpu[1] & 0xFFFF))), "READY's loop runs on");
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
