//! Drive windows: registers, DIRTY flags, param RAM and WD177x ([`DriveRegs`]). Registers:
//! docs/hw/11-drives-iec-periph.md §Drive A.
//!
//! Drives A and B are T1 (docs/specs/S14-c64-trx64.md §W4-DRIVE, S27): `devices::c64::C64Port` serves both windows,
//! so each access reaches the drive of the attached C64 backend (`C64Backend::drive`). The firmware stays the owner of
//! the disk: the drive gets the GCR bytes the param RAM points at, and what it writes goes back into that DDR with the
//! DIRTY bits set, as floppy.vhd does. Without a backend the same registers have no drive behind them, which reads as
//! the T0 stub of docs/specs/S04-board-t0.md.

use std::ops::Range;

use crate::c64host::{C64Drive, DriveLines, DriveStatus, DRIVE_HALF_TRACKS};
use crate::devices::board::{at, Reg, RegTable, Span};
use crate::io::IoCtx;
use crate::time::CLOCKS_PER_MS;

/// Drive window parts, split on address bits 12:11 (mm_drive.vhd:142-160). Above 0x1FFF reads 0 (11 OQ4).
const REGS_END: u32 = 0x07FF;
const DIRTY: u32 = 0x0800;
const DIRTY_END: u32 = 0x0FFF;
const PARAM: u32 = 0x1000;
const PARAM_END: u32 = 0x17FF;
const WD: u32 = 0x1800;
const WD_END: u32 = 0x1FFF;

/// Register offsets, decoded on address bits 3:0 (c1541.h:62-75, drive_registers.vhd).
const POWER: usize = 0x0;
const RESET: usize = 0x1;
const HW_ADDR: usize = 0x2;
const SENSOR: usize = 0x3;
const INSERTED: usize = 0x4;
const SIDE: usize = 0x6;
const MAN_WRITE: usize = 0x7;
const TRACK: usize = 0x8;
const STATUS: usize = 0x9;
const DISKCHNG: usize = 0xC;
const DRIVETYPE: usize = 0xD;

/// Read-back mask of each register latch; 0 = not a latch (drive_registers.vhd write and read branches). DRIVETYPE
/// reads back because the U64 drive is multi-mode (`g_multi_mode => true`, mm_drive.vhd; 11 H3).
const MASK: [u8; 16] = [0x01, 0x07, 0x03, 0x01, 0x01, 0xFE, 0, 0, 0, 0, 0, 0, 0x03, 0x03, 0, 0];
/// Latch reset values: powered off, RESET = held | follow the C64 reset | stop when frozen (drive_registers.vhd).
const INIT: [u8; 16] = [0, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// STATUS bits (c1541.h:85-87).
const ST_MOTOR: u8 = 0x01;
const ST_WRITING: u8 = 0x02;
const ST_WRITE_BUSY: u8 = 0x04;

/// Write busy holds 2047 ms after the last write or MAN_WRITE pulse (drive_registers.vhd `write_delay`, 1 kHz).
const WRITE_BUSY_CLOCKS: u64 = 2047 * CLOCKS_PER_MS;

/// DDR area of each drive (`__drive_a_area`, `__drive_b_area`, linker.x:271-272): CPU RAM at +0, ROM at +0x8000
/// (c1541.cc:96-99, 929-940).
const AREA: [usize; 2] = [0x00EE_0000, 0x00ED_0000];
const ROM: Range<usize> = 0x8000..0x1_0000;
const RAM_LEN: usize = 0x800;

/// Param RAM word 0 of a track: 26-bit DDR address of its first GCR byte; word 1 bits 13:0: its last offset
/// (floppy_param_mem.vhd:68-79). Word 1 bits 25:16, the bit time, are not used: the drive clocks bits from its own
/// speed zone.
const TRACK_START: u32 = 0x03FF_FFFF;
const MAX_OFFSET: u32 = 0x3FFF;

/// WD177x part (wd177x.vhd register process). No 1541 uses it, so it stays the T0 model (11 §Emulator model tiers).
const WD177X: &[Span] = &[
    // WD_TRACK, reset 1.
    at(0x1801, Reg::Latch { mask: 0xFF, init: 0x01 }),
    // WD_STATUS_CLEAR / WD_STATUS_SET: status &= !val / status |= val; both read the status.
    at(0x1804, Reg::Clear { cell: 0x1804, mask: 0xFF, read: None }),
    at(0x1805, Reg::Set { cell: 0x1804, mask: 0xFF, read: None }),
    // 00 §2 C28, 11 H2/H9: WD_IRQ_ACK bit7 (command FIFO valid) reads 0, or WD177x::init's
    // `while (R & 0x80) W 1;` (wd177x.cc:58-61) hangs; high IRQ 1/2 stays low.
    at(0x1806, Reg::Raz),
    // WD_DMA_MODE.
    at(0x1807, Reg::Latch { mask: 0x03, init: 0 }),
    // WD_DMA_LEN, 14 bits.
    at(0x180C, Reg::Latch { mask: 0xFF, init: 0 }),
    at(0x180D, Reg::Latch { mask: 0x3F, init: 0 }),
    // 11 H13: WD_STEPPER_TRACK reads step_busy = 0, not the goto track.
    at(0x180E, Reg::Raz),
    // WD_STEP_TIME, reset 12.
    at(0x180F, Reg::Latch { mask: 0x1F, init: 0x0C }),
];

/// One drive window: 0x10020000 (unit 0, drive A) or 0x10024000 (unit 1, drive B).
pub struct DriveRegs {
    unit: u8,
    /// Register latches, indexed by offset.
    regs: [u8; 16],
    /// DIRTY (floppy.vhd:148-189): a write happened since the firmware cleared it, and one bit per track.
    any_dirty: bool,
    dirty: u128,
    /// Param RAM: 512 little-endian words, write-only (floppy_param_mem.vhd:53).
    params: Box<[u8; 0x800]>,
    /// Side-0 half-tracks whose param words were written since the drive last got their surface.
    stale: u128,
    wd: RegTable,
    /// Emulator clock until which STATUS reports write busy.
    write_busy_until: u64,
    /// STATUS write busy as of the last access.
    write_busy: bool,
    /// The drive's head and motor as of the last access; peeks report it.
    status: DriveStatus,
}

impl DriveRegs {
    pub fn new(unit: u8) -> Self {
        let name = if unit == 0 { "drive-a-wd177x" } else { "drive-b-wd177x" };
        DriveRegs {
            unit,
            regs: INIT,
            any_dirty: false,
            dirty: 0,
            params: Box::new([0; 0x800]),
            stale: 0,
            wd: RegTable::new(name, WD177X),
            write_busy_until: 0,
            write_busy: false,
            status: DriveStatus::default(),
        }
    }

    /// A read at window offset `off`. Registers and DIRTY first bring in the drive's state.
    pub fn read(&mut self, off: u32, ctx: &mut IoCtx, drive: Option<&mut dyn C64Drive>) -> u8 {
        match (off, drive) {
            (0..=REGS_END, Some(drive)) => self.refresh(ctx.now, drive),
            (DIRTY..=DIRTY_END, Some(drive)) => self.sync_disk(ctx, drive),
            // S31: a 1581's controller answers its window itself.
            (WD..=WD_END, Some(drive)) if drive.has_wd() => return drive.wd_read(((off - WD) & 0x0F) as u16),
            _ => {}
        }
        self.write_busy = ctx.now < self.write_busy_until;
        self.peek(off)
    }

    /// A write at window offset `off`.
    pub fn write(&mut self, off: u32, val: u8, ctx: &mut IoCtx, drive: Option<&mut dyn C64Drive>) {
        match off {
            0..=REGS_END => {
                let reg = (off & 0x0F) as usize;
                self.regs[reg] = val & MASK[reg];
                match reg {
                    MAN_WRITE => self.write_busy_until = ctx.now + WRITE_BUSY_CLOCKS,
                    // drive_registers.vhd: `drv_reset_i = '1'` clears write busy.
                    RESET if val & 0x01 != 0 => self.write_busy_until = 0,
                    _ => {}
                }
                if let (POWER | RESET | HW_ADDR | SENSOR | INSERTED | DISKCHNG | DRIVETYPE, Some(drive)) = (reg, drive) {
                    let area = AREA[usize::from(self.unit)];
                    drive.set_lines(self.lines(), &ctx.ram[area + ROM.start..area + ROM.end]);
                }
            }
            DIRTY..=DIRTY_END => {
                if let Some(drive) = drive {
                    self.sync_disk(ctx, drive);
                }
                // floppy.vhd:176-183: bit 7 set clears any_dirty, otherwise the addressed track bit.
                if val & 0x80 != 0 {
                    self.any_dirty = false;
                } else {
                    self.dirty &= !(1 << (off & 0x7F));
                }
            }
            PARAM..=PARAM_END => {
                let at = (off - PARAM) as usize;
                self.params[at] = val;
                // Word index = side << 8 | halftrack << 1 | w (floppy_param_mem.vhd:66): 8 bytes per half-track.
                if at / 8 < DRIVE_HALF_TRACKS {
                    self.stale |= 1 << (at / 8);
                }
            }
            WD..=WD_END => match drive {
                Some(drive) if drive.has_wd() => drive.wd_write(((off - WD) & 0x0F) as u16, val, ctx.ram),
                _ => self.wd.set(off, val),
            },
            _ => {}
        }
    }

    /// What a read returns, without side effects: drive state as of the last access.
    pub fn peek(&self, off: u32) -> u8 {
        match off {
            0..=REGS_END => match (off & 0x0F) as usize {
                SIDE => self.status.side,
                TRACK => self.status.half_track & 0x7F,
                STATUS => {
                    let powered = self.powered();
                    (if powered && self.status.motor { ST_MOTOR } else { 0 })
                        | (if powered && self.status.writing { ST_WRITING } else { 0 })
                        | (if self.write_busy { ST_WRITE_BUSY } else { 0 })
                }
                reg => self.regs[reg],
            },
            DIRTY..=DIRTY_END => (u8::from(self.any_dirty) << 7) | ((self.dirty >> (off & 0x7F)) & 1) as u8,
            PARAM..=PARAM_END => 0,
            WD..=WD_END => self.wd.get(off),
            _ => 0,
        }
    }

    /// Periodic sync (`C64Port::tick`, every 1 ms): carry the drive's writes into DDR and mirror its RAM into the
    /// drive area, where the firmware reads the device number at $78 (c1541.cc:384-389).
    pub fn tick(&mut self, ctx: &mut IoCtx, drive: Option<&mut dyn C64Drive>) {
        let Some(drive) = drive else { return };
        self.sync_disk(ctx, drive);
        if self.powered() {
            let area = AREA[usize::from(self.unit)];
            drive.read_ram(&mut ctx.ram[area..area + RAM_LEN]);
        }
    }

    fn powered(&self) -> bool {
        self.regs[POWER] & 0x01 != 0
    }

    /// The lines the registers drive (drive_registers.vhd output assignments).
    fn lines(&self) -> DriveLines {
        DriveLines {
            power: self.powered(),
            reset: self.regs[RESET] & 0x01 != 0,
            follow_c64_reset: self.regs[RESET] & 0x02 != 0,
            stop_on_freeze: self.regs[RESET] & 0x04 != 0,
            device: self.regs[HW_ADDR] & 0x03,
            write_protect: self.regs[SENSOR] & 0x01 == 0,
            drive_type: self.regs[DRIVETYPE] & 0x03,
            inserted: self.regs[INSERTED] & 0x01 != 0,
            disk_change: self.regs[DISKCHNG] & 0x01 != 0,
            force_ready: self.regs[DISKCHNG] & 0x02 != 0,
        }
    }

    /// Take the drive's head and motor state; writing with the motor on restarts write busy.
    fn refresh(&mut self, now: u64, drive: &mut dyn C64Drive) {
        self.status = drive.status();
        if self.powered() && self.status.motor && self.status.writing {
            self.write_busy_until = now + WRITE_BUSY_CLOCKS;
        }
    }

    /// DDR bytes of a side-0 half-track's surface, as floppy_mem.vhd addresses them: `track_start + offset` for
    /// offsets up to the track's max offset.
    fn surface(&self, half_track: usize, ram_len: usize) -> Range<usize> {
        let word = |i: usize| u32::from_le_bytes([0, 1, 2, 3].map(|b| self.params[4 * i + b]));
        let start = ((word(2 * half_track) & TRACK_START) as usize).min(ram_len);
        let len = (word(2 * half_track + 1) & MAX_OFFSET) as usize + 1;
        start..(start + len).min(ram_len)
    }

    /// Hand the drive the surfaces whose param words changed, then carry what it wrote into DDR and DIRTY.
    fn sync_disk(&mut self, ctx: &mut IoCtx, drive: &mut dyn C64Drive) {
        let ram = &mut *ctx.ram;
        for half_track in bits(std::mem::take(&mut self.stale)) {
            drive.set_track(half_track as u8, &ram[self.surface(half_track, ram.len())]);
        }
        self.refresh(ctx.now, drive);
        // floppy_stream.vhd `do_write`: bits reach the disk only with a disk inserted and the sensor lit; otherwise
        // the head only advances, so the drive's copy goes back to what the disk holds.
        let writable = self.regs[INSERTED] & 0x01 != 0 && self.regs[SENSOR] & 0x01 != 0;
        let writing = self.powered() && self.status.motor && self.status.writing;
        for half_track in bits(drive.take_written()) {
            let surface = self.surface(half_track, ram.len());
            if !writable {
                drive.set_track(half_track as u8, &ram[surface]);
                continue;
            }
            let written = drive.track(half_track as u8);
            let len = written.len().min(surface.len());
            let target = &mut ram[surface.start..surface.start + len];
            let changed = *target != written[..len];
            if changed {
                target.copy_from_slice(&written[..len]);
            }
            // floppy.vhd:156-163: writing marks the track under the head; the index drops the half-track bit.
            if changed || (writing && half_track == usize::from(self.status.half_track)) {
                self.dirty |= 1 << (half_track >> 1);
                self.any_dirty = true;
            }
        }
    }
}

/// Indices of the set bits of `mask`, lowest first.
fn bits(mut mask: u128) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            i
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::RAM_SIZE;
    use crate::devices::board::rig::Rig;
    use crate::irq::IrqState;

    #[test]
    fn c28_wd177x_idle() {
        let mut rig = Rig::new(crate::devices::c64::install);
        for base in [0x1002_0000, 0x1002_4000] {
            // C1541 ctor (c1541.cc:119-123).
            rig.w8(base + 0x0D, 0x02);
            assert_eq!(rig.r8(base + 0x0D) & 3, 2, "multi_mode");
            // WD177x::init (wd177x.cc:50-64).
            rig.w8(base + 0x1800, 0x03);
            let mut pops = 0;
            while rig.r8(base + 0x1806) & 0x80 != 0 {
                rig.w8(base + 0x1806, 0x01);
                pops += 1;
                assert!(pops < 10, "WD177x FIFO never drains");
            }
            assert_eq!(rig.r8(base + 0x1806), 0);
            assert_eq!(rig.r8(base + 0x180E), 0, "not step-busy");
            // drive_power(true) (c1541.cc:324-330), then the drive task poll.
            rig.w8(base, 0x01);
            rig.w8(base + 0x0800, 0x80);
            assert_eq!(rig.r8(base), 0x01);
            assert_eq!(rig.r8(base + 0x0800), 0, "DIRTY is not RAM (11 H4)");
            assert_eq!(rig.r8(base + 0x0009), 0, "no write busy");
            rig.w32(base + 0x1000, 0x00EE_0000);
            assert_eq!(rig.r32(base + 0x1000), 0, "param RAM reads 0");
            // Status set/clear, reset values.
            rig.w8(base + 0x1805, 0x21);
            rig.w8(base + 0x1804, 0x01);
            assert_eq!((rig.r8(base + 0x1804), rig.r8(base + 0x1805)), (0x20, 0x20));
            assert_eq!((rig.r8(base + 0x1801), rig.r8(base + 0x180F), rig.r8(base + 0x01)), (0x01, 0x0C, 0x07));
        }
    }

    /// A drive that records what it is given and reports what the test sets.
    struct MockDrive {
        lines: Vec<(DriveLines, u8)>,
        tracks: Vec<Vec<u8>>,
        written: u128,
        status: DriveStatus,
        ram: Vec<u8>,
    }

    impl C64Drive for MockDrive {
        /// Records the lines and the byte at ROM $FFFC.
        fn set_lines(&mut self, lines: DriveLines, rom: &[u8]) {
            self.lines.push((lines, rom[0x7FFC]));
        }
        fn set_track(&mut self, half_track: u8, gcr: &[u8]) {
            self.tracks[usize::from(half_track)] = gcr.to_vec();
        }
        fn track(&self, half_track: u8) -> &[u8] {
            &self.tracks[usize::from(half_track)]
        }
        fn take_written(&mut self) -> u128 {
            std::mem::take(&mut self.written)
        }
        fn status(&self) -> DriveStatus {
            self.status
        }
        fn read_ram(&self, out: &mut [u8]) {
            out.copy_from_slice(&self.ram[..out.len()]);
        }
    }

    struct Bench {
        regs: DriveRegs,
        drive: MockDrive,
        ram: Vec<u8>,
        irq: IrqState,
        console: Vec<u8>,
        now: u64,
    }

    impl Bench {
        fn new() -> Self {
            let drive = MockDrive {
                lines: Vec::new(),
                tracks: vec![Vec::new(); DRIVE_HALF_TRACKS],
                written: 0,
                status: DriveStatus::default(),
                ram: vec![0; RAM_LEN],
            };
            Bench { regs: DriveRegs::new(0), drive, ram: vec![0; RAM_SIZE], irq: IrqState::new(), console: Vec::new(), now: 0 }
        }

        fn ctx(&mut self) -> (&mut DriveRegs, &mut MockDrive, IoCtx<'_>) {
            let ctx = IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            (&mut self.regs, &mut self.drive, ctx)
        }

        fn r8(&mut self, off: u32) -> u8 {
            let (regs, drive, mut ctx) = self.ctx();
            regs.read(off, &mut ctx, Some(drive))
        }

        fn w8(&mut self, off: u32, val: u8) {
            let (regs, drive, mut ctx) = self.ctx();
            regs.write(off, val, &mut ctx, Some(drive));
        }

        fn w32(&mut self, off: u32, val: u32) {
            for (i, b) in val.to_le_bytes().into_iter().enumerate() {
                self.w8(off + i as u32, b);
            }
        }

        fn tick(&mut self) {
            let (regs, drive, mut ctx) = self.ctx();
            regs.tick(&mut ctx, Some(drive));
        }
    }

    const DUMMY: usize = 0x0020_0000;
    const TRACK18: usize = 0x0030_0000;
    const TRACK18_LEN: u32 = 7142;

    #[test]
    fn param_ram_hands_the_drive_its_surfaces() {
        let mut b = Bench::new();
        // C1541::init (c1541.cc:214-225): every word pair points at the zero dummy track.
        for i in 0..256 {
            b.w32(0x1000 + 8 * i, DUMMY as u32);
            b.w32(0x1004 + 8 * i, (650 << 16) | 0x100);
        }
        assert!(b.drive.tracks.iter().all(Vec::is_empty), "surfaces go over at the next sync");
        b.tick();
        assert!(b.drive.tracks.iter().all(|t| t.len() == 0x101));
        // insert_disk (c1541.cc:467-481): track 18 = half-track 34.
        for (i, v) in b.ram[TRACK18..TRACK18 + TRACK18_LEN as usize].iter_mut().enumerate() {
            *v = i as u8;
        }
        b.w32(0x1000 + 8 * 34, TRACK18 as u32);
        b.w32(0x1004 + 8 * 34, (TRACK18_LEN - 1) | (700 << 16));
        assert_eq!(b.r8(0x0800), 0, "a DIRTY read syncs");
        assert_eq!(b.drive.tracks[34], b.ram[TRACK18..TRACK18 + TRACK18_LEN as usize]);
        assert_eq!(b.drive.tracks[35].len(), 0x101);
    }

    #[test]
    fn registers_drive_the_lines_with_the_rom() {
        let mut b = Bench::new();
        b.ram[AREA[0] + 0xFFFC] = 0xAA;
        assert_eq!(b.r8(0x01), 0x07, "held in reset, follows the C64, stops when frozen");
        b.w8(0x00, 0x01);
        b.w8(0x01, 0x06);
        b.w8(0x02, 0x01);
        b.w8(0x03, 0x01);
        b.w8(0x04, 0x01);
        b.w8(0x0D, 0x00);
        let on = DriveLines {
            power: true,
            reset: false,
            follow_c64_reset: true,
            stop_on_freeze: true,
            device: 1,
            write_protect: false,
            drive_type: 0,
            inserted: true,
            disk_change: false,
            force_ready: false,
        };
        assert_eq!(b.drive.lines.len(), 6, "INSERTED is a line, the 1581's /RDY (S31): {:?}", b.drive.lines);
        assert_eq!(b.drive.lines.last(), Some(&(on, 0xAA)));
        assert_eq!(b.drive.lines[0].0, DriveLines { power: true, ..DriveLines::default() });
        // Head and motor come from the drive.
        b.drive.status = DriveStatus { half_track: 36, motor: true, writing: false, led: true, side: 0 };
        assert_eq!((b.r8(0x08), b.r8(0x09), b.r8(0x06)), (36, ST_MOTOR, 0));
        b.w8(0x00, 0x00);
        assert_eq!(b.r8(0x09), 0, "no motor without power (mm_drive_cpu.vhd:743)");
        assert_eq!((b.r8(0x12), b.r8(0x7F8)), (0x01, 36), "registers repeat every 16 bytes");
    }

    #[test]
    fn drive_writes_reach_ddr_and_dirty() {
        let mut b = Bench::new();
        b.w32(0x1000 + 8 * 34, TRACK18 as u32);
        b.w32(0x1004 + 8 * 34, TRACK18_LEN - 1);
        b.w32(0x1000 + 8 * 35, DUMMY as u32);
        b.w32(0x1004 + 8 * 35, 0x100);
        b.tick();
        for reg in [0x00, 0x03, 0x04] {
            b.w8(reg, 0x01);
        }
        // The drive wrote one byte of track 18 and reports the neighbouring half-track unchanged.
        b.drive.tracks[34][100] = 0x5A;
        b.drive.written = 1 << 34 | 1 << 35;
        b.drive.status = DriveStatus { half_track: 34, motor: true, writing: true, led: true, side: 0 };
        b.now = 1000;
        b.tick();
        assert_eq!(b.ram[TRACK18 + 100], 0x5A);
        assert_eq!((b.r8(0x0800), b.r8(0x0800 + 17), b.r8(0x0800 + 18)), (0x80, 0x81, 0x80), "track 18, not 18.5");
        assert_eq!(b.r8(0x09), ST_MOTOR | ST_WRITING | ST_WRITE_BUSY);
        // Write busy outlasts the write by 2047 ms.
        b.drive.status.writing = false;
        b.now += 2046 * CLOCKS_PER_MS;
        assert_eq!(b.r8(0x09), ST_MOTOR | ST_WRITE_BUSY);
        b.now += 2 * CLOCKS_PER_MS;
        assert_eq!(b.r8(0x09), ST_MOTOR);
        b.w8(0x07, 0xFF);
        assert_eq!(b.r8(0x09), ST_MOTOR | ST_WRITE_BUSY, "MAN_WRITE restarts it");
        b.w8(0x01, 0x07);
        assert_eq!(b.r8(0x09), ST_MOTOR, "the drive reset clears it");
        // The firmware's poll (c1541.cc:779-795) clears any_dirty, then the track bit.
        b.w8(0x0800, 0x80);
        assert_eq!(b.r8(0x0800 + 17), 0x01);
        b.w8(0x0800 + 17, 0x00);
        assert_eq!(b.r8(0x0800 + 17), 0x00);
        // Sensor dark: the write does not reach the disk, and the drive gets the disk's bytes back.
        b.w8(0x03, 0x00);
        b.drive.tracks[34][101] = 0xA5;
        b.drive.written = 1 << 34;
        b.tick();
        assert_eq!((b.ram[TRACK18 + 101], b.drive.tracks[34][101], b.r8(0x0800)), (0, 0, 0));
    }

    #[test]
    fn drive_ram_is_mirrored_while_powered() {
        let mut b = Bench::new();
        b.drive.ram[0x78] = 0x49;
        b.tick();
        assert_eq!(b.ram[AREA[0] + 0x78], 0, "off");
        b.w8(0x00, 0x01);
        b.tick();
        assert_eq!(b.ram[AREA[0] + 0x78], 0x49, "the device number at $78 (c1541.cc:386-387)");
    }
}
