//! System bus: 64 MB DDR + IO decode. Spec: docs/specs/S02-core.md
//!
//! Data decode follows the rvlite `bus_converter` (docs/hw/00-memory-map.md §1 bus rules, §1a;
//! docs/hw/01-cpu-boot-memory.md §Address map):
//! - `0x8000xxxx`: rvlite boot BRAM (bus_converter.vhd:96,118). Not modelled because the ELF is loaded
//!   directly (01 §B), so it reads 0.
//! - bit 28 = 0: DDR with a 26-bit address, i.e. mirrored modulo 64 MB (bus_converter.vhd:123; mem_bus_pkg.vhd:47).
//! - `0x10000000-0x10FFFFFF`: the 8-bit IO bus through [`IoMap`]. 16/32-bit accesses are little-endian byte
//!   strobes at addr+0..+3, each with its own side effects (bus_converter.vhd:56,82-93,159-185).
//! - anything else: read 0, write ignored, never a fault (00 §1, 01 H16). rvlite would alias higher bit-28
//!   addresses onto the IO bus; that is OPEN (00 Q-A4) and the firmware never uses them, so they only show up
//!   in the unmapped log.
//!
//! Instruction fetches go through a second converter without IO support (rvlite_wrapper.vhd:101-106), so a
//! fetch ignores bit 28 and only the boot BRAM page is special.

use std::collections::BTreeMap;

use crate::io::{IoCtx, IoMap, IO_BASE, IO_SIZE};
use crate::irq::IrqState;

pub const RAM_SIZE: usize = 64 << 20;
pub const RAM_MASK: u32 = RAM_SIZE as u32 - 1;

/// Data-address bit that selects the IO bus (rvlite_wrapper.vhd:122 `g_io_bit => 28`).
pub const IO_BIT: u32 = 1 << 28;
/// `addr >> 16` of the rvlite boot BRAM (rvlite_wrapper.vhd:21,105; bus_converter.vhd:96,118).
pub const BOOT_BRAM_PAGE: u32 = 0x8000;
/// Hits per distinct unmapped address that are reported one by one; later hits are only counted.
pub const UNMAPPED_REPORT_LIMIT: u64 = 4;

/// One logged byte access. The bus has no symbols, so the machine formats and prints these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub write: bool,
    pub addr: u32,
    /// Byte written, or byte returned by the read (0 when unmapped).
    pub val: u8,
    /// Raw PC of the accessing instruction.
    pub pc: u32,
    /// Print an IO trace line (`trace_io` and the address is on the IO bus).
    pub trace: bool,
    /// Print an unmapped report line (one of the first [`UNMAPPED_REPORT_LIMIT`] hits on `addr`).
    pub unmapped: bool,
    /// Emulated clock of the access (100 MHz ticks). Two lines of the IO trace are only comparable in time
    /// through this: the firmware's order says nothing about how far apart two writes are.
    pub now: u64,
}

/// Unmapped access counts of one address, for the summary at the end of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnmappedCount {
    pub reads: u64,
    pub writes: u64,
}

pub struct SystemBus {
    pub ram: Vec<u8>,
    pub io: IoMap,
    pub irq: IrqState,
    /// UART TX bytes not yet drained.
    pub console: Vec<u8>,
    /// Emulated clock (100 MHz ticks).
    pub now: u64,
    /// PC of the instruction currently executing.
    pub pc: u32,
    /// Set on any IO access; the machine recomputes device deadlines and clears it.
    pub io_touched: bool,
    /// Wait states the devices of this instruction charged, in 100 MHz ticks. The machine adds them to emulated
    /// time after the instruction and clears it, so a device that models a real delay slows the firmware down the
    /// way the hardware does ([`crate::time::DMA_BYTE_CLOCKS`]).
    pub stall: u64,
    /// Record every IO byte access for the IO trace (`--log io`).
    pub trace_io: bool,
    /// Count unmapped accesses and record the first hits per address (`--log unmapped`).
    pub log_unmapped: bool,
    /// Recorded accesses not yet printed; the machine drains them after each instruction.
    pub accesses: Vec<Access>,
    /// Unmapped access counts by address (filled only while `log_unmapped`).
    pub unmapped: BTreeMap<u32, UnmappedCount>,
    /// Set by every CPU write to DDR, every IO access and every device tick; the idle skip clears it where it looks
    /// for a loop that cannot change anything (docs/specs/S19-idle-skip.md §3).
    pub idle_dirty: bool,
}

impl Default for SystemBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Data-bus decode of one byte address.
enum Region {
    Ram(usize),
    Io,
    Open,
}

#[inline(always)]
fn region(addr: u32) -> Region {
    if addr & IO_BIT == 0 {
        if addr >> 16 == BOOT_BRAM_PAGE {
            Region::Open
        } else {
            Region::Ram((addr & RAM_MASK) as usize)
        }
    } else if addr.wrapping_sub(IO_BASE) < IO_SIZE {
        Region::Io
    } else {
        Region::Open
    }
}

/// RAM index of a `len`-byte data access that stays inside one 64 MB DDR mirror. Crossing a 64 MB boundary
/// can reach the boot BRAM page or the IO bus, so such accesses take the byte path.
#[inline(always)]
fn ram_span(addr: u32, len: usize) -> Option<usize> {
    let i = (addr & RAM_MASK) as usize;
    (addr & IO_BIT == 0 && addr >> 16 != BOOT_BRAM_PAGE && i + len <= RAM_SIZE).then_some(i)
}

impl SystemBus {
    pub fn new() -> Self {
        SystemBus {
            ram: vec![0; RAM_SIZE],
            io: IoMap::new(),
            irq: IrqState::new(),
            console: Vec::new(),
            now: 0,
            pc: 0,
            io_touched: false,
            stall: 0,
            trace_io: false,
            log_unmapped: false,
            accesses: Vec::new(),
            unmapped: BTreeMap::new(),
            idle_dirty: true,
        }
    }

    /// Earliest `next_event` over all devices; `u64::MAX` when none is scheduled.
    pub fn next_deadline(&self) -> u64 {
        self.io.devices.iter().filter_map(|d| d.next_event()).min().unwrap_or(u64::MAX)
    }

    /// Tick, in install order, every device whose `next_event()` is due at `now`.
    pub fn tick_due(&mut self) {
        // A device may write DDR through `IoCtx::ram` (S19 §3).
        self.idle_dirty = true;
        let now = self.now;
        for dev in &mut self.io.devices {
            if dev.next_event().is_some_and(|t| t <= now) {
                let mut ctx =
                    IoCtx { stall: 0, now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
                dev.tick(&mut ctx);
            }
        }
    }

    fn io_read8(&mut self, addr: u32) -> u8 {
        self.io_touched = true;
        self.idle_dirty = true;
        match self.io.resolve(addr) {
            Some((dev, off)) => {
                let mut ctx = IoCtx { stall: 0,
                    now: self.now,
                    pc: self.pc,
                    ram: &mut self.ram,
                    irq: &mut self.irq,
                    console: &mut self.console,
                };
                let val = self.io.devices[dev].read8(off, &mut ctx);
                self.stall += ctx.stall;
                if self.trace_io {
                    self.record(false, addr, val, true, true);
                }
                val
            }
            None => {
                if self.trace_io || self.log_unmapped {
                    self.record(false, addr, 0, false, true);
                }
                0
            }
        }
    }

    /// One write into a device register from outside the CPU, with every side effect the firmware's own write
    /// would have (S23 §3: the monitor stops the C64 through `C64_STOP`, the register the firmware uses itself).
    /// DDR addresses are not this door's business and are refused.
    pub fn poke_io8(&mut self, addr: u32, val: u8) -> bool {
        if !matches!(region(addr), Region::Io) || self.io.resolve(addr).is_none() {
            return false;
        }
        self.io_write8(addr, val);
        true
    }

    fn io_write8(&mut self, addr: u32, val: u8) {
        self.io_touched = true;
        self.idle_dirty = true;
        match self.io.resolve(addr) {
            Some((dev, off)) => {
                let mut ctx = IoCtx { stall: 0,
                    now: self.now,
                    pc: self.pc,
                    ram: &mut self.ram,
                    irq: &mut self.irq,
                    console: &mut self.console,
                };
                self.io.devices[dev].write8(off, val, &mut ctx);
                self.stall += ctx.stall;
                if self.trace_io {
                    self.record(true, addr, val, true, true);
                }
            }
            None => {
                if self.trace_io || self.log_unmapped {
                    self.record(true, addr, val, false, true);
                }
            }
        }
    }

    /// Log bookkeeping for one byte access. `mapped`: something decodes the address; `io`: it is on the IO bus.
    #[cold]
    fn record(&mut self, write: bool, addr: u32, val: u8, mapped: bool, io: bool) {
        let mut unmapped = false;
        if !mapped && self.log_unmapped {
            let count = self.unmapped.entry(addr).or_default();
            if write {
                count.writes += 1;
            } else {
                count.reads += 1;
            }
            unmapped = count.reads + count.writes <= UNMAPPED_REPORT_LIMIT;
        }
        let trace = io && self.trace_io;
        if trace || unmapped {
            self.accesses.push(Access { write, addr, val, pc: self.pc, trace, unmapped, now: self.now });
        }
    }

    /// Instruction-bus byte: the boot BRAM page reads 0, every other address is DDR.
    fn fetch_byte(&self, addr: u32) -> u8 {
        if addr >> 16 == BOOT_BRAM_PAGE {
            0
        } else {
            self.ram[(addr & RAM_MASK) as usize]
        }
    }
}

impl rv32::Bus for SystemBus {
    #[inline]
    fn read8(&mut self, addr: u32) -> u8 {
        match region(addr) {
            Region::Ram(i) => self.ram[i],
            Region::Io => self.io_read8(addr),
            Region::Open => {
                if self.log_unmapped {
                    self.record(false, addr, 0, false, false);
                }
                0
            }
        }
    }

    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        match region(addr) {
            Region::Ram(i) => {
                self.ram[i] = val;
                self.idle_dirty = true;
            }
            Region::Io => self.io_write8(addr, val),
            Region::Open => {
                if self.log_unmapped {
                    self.record(true, addr, val, false, false);
                }
            }
        }
    }

    #[inline]
    fn read16(&mut self, addr: u32) -> u16 {
        match ram_span(addr, 2) {
            Some(i) => u16::from_le_bytes([self.ram[i], self.ram[i + 1]]),
            None => u16::from(self.read8(addr)) | u16::from(self.read8(addr.wrapping_add(1))) << 8,
        }
    }

    #[inline]
    fn read32(&mut self, addr: u32) -> u32 {
        match ram_span(addr, 4) {
            Some(i) => {
                let b = &self.ram[i..i + 4];
                u32::from_le_bytes([b[0], b[1], b[2], b[3]])
            }
            None => (0..4).fold(0, |acc, k| acc | u32::from(self.read8(addr.wrapping_add(k))) << (8 * k)),
        }
    }

    #[inline]
    fn write16(&mut self, addr: u32, val: u16) {
        match ram_span(addr, 2) {
            Some(i) => {
                self.ram[i..i + 2].copy_from_slice(&val.to_le_bytes());
                self.idle_dirty = true;
            }
            None => {
                self.write8(addr, val as u8);
                self.write8(addr.wrapping_add(1), (val >> 8) as u8);
            }
        }
    }

    #[inline]
    fn write32(&mut self, addr: u32, val: u32) {
        match ram_span(addr, 4) {
            Some(i) => {
                self.ram[i..i + 4].copy_from_slice(&val.to_le_bytes());
                self.idle_dirty = true;
            }
            None => {
                for (k, byte) in (0u32..).zip(val.to_le_bytes()) {
                    self.write8(addr.wrapping_add(k), byte);
                }
            }
        }
    }

    #[inline]
    fn fetch(&mut self, addr: u32) -> u32 {
        let i = (addr & RAM_MASK) as usize;
        if addr >> 16 != BOOT_BRAM_PAGE && i + 4 <= RAM_SIZE {
            let b = &self.ram[i..i + 4];
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            (0..4).fold(0, |acc, k| acc | u32::from(self.fetch_byte(addr.wrapping_add(k))) << (8 * k))
        }
    }
}

#[cfg(test)]
mod tests {
    use rv32::Bus;

    use super::*;
    use crate::io::IoDevice;

    /// Logs byte accesses (reads return `0xA0 | off`); when due, a tick pushes `id` to the console.
    struct Probe {
        id: u8,
        due: Option<u64>,
        log: Vec<(bool, u32, u8)>,
    }

    impl IoDevice for Probe {
        fn name(&self) -> &'static str {
            "probe"
        }

        fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
            let val = 0xA0 | off as u8;
            self.log.push((false, off, val));
            val
        }

        fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
            self.log.push((true, off, val));
        }

        fn next_event(&self) -> Option<u64> {
            self.due
        }

        fn tick(&mut self, ctx: &mut IoCtx) {
            ctx.console.push(self.id);
            self.due = None;
        }

        crate::impl_as_any!();
    }

    fn probe(id: u8, due: Option<u64>) -> Box<Probe> {
        Box::new(Probe { id, due, log: Vec::new() })
    }

    fn probe_log(bus: &SystemBus, idx: usize) -> &[(bool, u32, u8)] {
        &bus.io.devices[idx].as_any().downcast_ref::<Probe>().unwrap().log
    }

    #[test]
    fn ram_word_fast_path_equals_byte_path() {
        let mut bus = SystemBus::new();
        bus.write32(0x1000, 0x1122_3344);
        assert_eq!([bus.read8(0x1000), bus.read8(0x1001), bus.read8(0x1002), bus.read8(0x1003)], [0x44, 0x33, 0x22, 0x11]);
        assert_eq!((bus.read16(0x1001), bus.fetch(0x1000)), (0x2233, 0x1122_3344));

        for (k, byte) in (0u32..).zip([0xDE, 0xAD, 0xBE, 0xEF]) {
            bus.write8(0x2000 + k, byte);
        }
        assert_eq!((bus.read32(0x2000), bus.read16(0x2002)), (0xEFBE_ADDE, 0xEFBE));
        bus.write16(0x2001, 0x5566);
        assert_eq!(bus.read32(0x2000), 0xEF55_66DE);

        // A word straddling the 64 MB wrap takes the byte path and wraps per byte.
        bus.write32(0x03FF_FFFE, 0xA1B2_C3D4);
        assert_eq!((bus.ram[RAM_SIZE - 2], bus.ram[RAM_SIZE - 1], bus.ram[0], bus.ram[1]), (0xD4, 0xC3, 0xB2, 0xA1));
        assert_eq!((bus.read32(0x03FF_FFFE), bus.fetch(0x03FF_FFFE)), (0xA1B2_C3D4, 0xA1B2_C3D4));
        assert!(!bus.io_touched);
    }

    #[test]
    fn io_32bit_write_reaches_device_as_ordered_bytes() {
        let mut bus = SystemBus::new();
        bus.io.add(0x1000_0100, 0x100, probe(1, None));
        bus.write32(0x1000_0104, 0x1122_3344);
        assert!(bus.io_touched);
        assert_eq!(probe_log(&bus, 0), [(true, 4, 0x44), (true, 5, 0x33), (true, 6, 0x22), (true, 7, 0x11)]);

        assert_eq!(bus.read16(0x1000_0108), 0xA9A8);
        assert_eq!(bus.read32(0x1000_0110), 0xA3A2_A1A0 | 0x1010_1010);
        assert_eq!(probe_log(&bus, 0)[4..6], [(false, 8, 0xA8), (false, 9, 0xA9)]);
    }

    #[test]
    fn unmapped_reads_are_zero_and_writes_ignored() {
        let mut bus = SystemBus::new();
        bus.ram[0] = 0x77;
        assert_eq!(bus.read8(0x10FF_0000), 0, "IO grain without a device");
        assert_eq!(bus.read32(0x8000_0000), 0, "boot BRAM page is not DDR");
        assert_eq!(bus.read32(0x3000_0000), 0, "bit 28 set above the IO window");
        bus.write32(0x8000_0000, 0xFFFF_FFFF);
        bus.write8(0x10FF_0000, 0xFF);
        assert_eq!(bus.ram[0], 0x77);
        assert_eq!(bus.fetch(0x8000_0000), 0);
    }

    #[test]
    fn ram_mirrors_above_64mb() {
        let mut bus = SystemBus::new();
        bus.write32(0x0400_1000, 0xCAFE_F00D);
        assert_eq!(bus.read32(0x1000), 0xCAFE_F00D);
        assert_eq!(bus.read32(0x2000_1000), 0xCAFE_F00D);
        assert_eq!(bus.read32(0x8001_1000 - 0x10000), 0, "0x80001000 is still the boot BRAM page");
        bus.write8(0x8001_0000, 0x42);
        assert_eq!(bus.read8(0x0001_0000), 0x42, "0x8001xxxx is DDR again");
    }

    #[test]
    fn fetch_ignores_the_io_bit() {
        let mut bus = SystemBus::new();
        bus.io.add(0x1000_0000, 0x100, probe(1, None));
        bus.write32(0x40, 0x0000_0013);
        assert_eq!(bus.fetch(0x1000_0040), 0x0000_0013);
        assert!(!bus.io_touched && probe_log(&bus, 0).is_empty());
    }

    #[test]
    fn unmapped_log_reports_first_hits_then_only_counts() {
        let mut bus = SystemBus::new();
        bus.log_unmapped = true;
        bus.pc = 0x1234;
        for _ in 0..6 {
            bus.read8(0x10FF_0010);
        }
        bus.write8(0x8000_0004, 0x5A);
        assert_eq!(bus.accesses.len(), 5);
        assert!(bus.accesses.iter().all(|a| a.unmapped && !a.trace && a.pc == 0x1234));
        assert_eq!(bus.accesses[4], Access { write: true, addr: 0x8000_0004, val: 0x5A, pc: 0x1234, trace: false, unmapped: true, now: bus.now });
        assert_eq!(bus.unmapped[&0x10FF_0010], UnmappedCount { reads: 6, writes: 0 });
        assert_eq!(bus.unmapped[&0x8000_0004], UnmappedCount { reads: 0, writes: 1 });
    }

    #[test]
    fn io_trace_records_every_io_byte() {
        let mut bus = SystemBus::new();
        bus.io.add(0x1000_0000, 0x100, probe(1, None));
        bus.trace_io = true;
        bus.write16(0x1000_0002, 0xBEEF);
        bus.read8(0x10FF_0000);
        bus.write8(0x1000, 1);
        let seen: Vec<_> = bus.accesses.iter().map(|a| (a.write, a.addr, a.val, a.trace, a.unmapped)).collect();
        assert_eq!(
            seen,
            [(true, 0x1000_0002, 0xEF, true, false), (true, 0x1000_0003, 0xBE, true, false), (false, 0x10FF_0000, 0, true, false)]
        );
        assert!(bus.unmapped.is_empty(), "unmapped counting is off");
    }

    #[test]
    fn tick_due_runs_due_devices_in_install_order() {
        let mut bus = SystemBus::new();
        bus.io.add(0x1000_0000, 0x100, probe(1, Some(100)));
        bus.io.add(0x1000_0100, 0x100, probe(2, Some(150)));
        bus.io.add(0x1000_0200, 0x100, probe(3, Some(50)));
        assert_eq!(bus.next_deadline(), 50);
        bus.now = 100;
        bus.tick_due();
        assert_eq!(bus.console, [1, 3]);
        assert_eq!(bus.next_deadline(), 150);
        assert_eq!(SystemBus::new().next_deadline(), u64::MAX);
    }
}
