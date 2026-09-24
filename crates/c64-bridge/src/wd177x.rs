//! The U64's WD177x for a 1581 at drive position A or B (S31): the FPGA's stand-in for the floppy controller, read
//! from `fpga/1541/vhdl_source/wd177x.vhd` and `stepper.vhd` with the drive mechanics of `c1581_drive.vhd`.
//!
//! The 1581 CPU sees four registers (status/command, track, sector, data). Every command it writes sets BUSY and goes
//! into a small FIFO whose non-empty state is the firmware's interrupt (ITU high 1 or 2); the firmware reads the
//! command, moves the head through GOTO_TRACK, and serves sectors by DMA between guest DDR and the data register
//! (`software/drive/wd177x.cc`). No disk image lives here: the D81 stays the firmware's file.
//!
//! Time is counted in drive cycles (the 1581 runs at 2 MHz on PAL and NTSC alike): the stepper and the index pulse
//! tick at 1 kHz, the write-completion delay at 4 MHz.

use std::collections::VecDeque;

/// Drive cycles per millisecond (1581 at 2 MHz).
const CYCLES_PER_MS: u64 = 2000;
/// The command FIFO (`sync_fifo`, g_depth 7).
const FIFO_DEPTH: usize = 7;
/// Tracks the mechanics reach (`c_max_track`, stepper.vhd; c1581_drive.vhd).
const MAX_TRACK: u8 = 83;
/// Index pulse period, 1 kHz ticks (`index_cnt <= X"C7"`: 200 ms, 300 rpm).
const INDEX_PERIOD: u8 = 0xC7;
/// Status bits.
const ST_BUSY: u8 = 0x01;
const ST_DRQ: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dma {
    Idle,
    /// Write complete: BUSY drops after this many 4 MHz ticks.
    WriteDelay(u16),
}

/// Guest memory the DMA reads and writes, addressed as the FPGA does (24 bits).
pub trait DmaMem {
    fn read(&mut self, addr: u32) -> u8;
    fn write(&mut self, addr: u32, val: u8);
}

/// Guest DDR, borrowed for one firmware access.
pub struct Ram<'a>(pub &'a mut [u8]);

impl DmaMem for Ram<'_> {
    fn read(&mut self, addr: u32) -> u8 {
        self.0.get(addr as usize).copied().unwrap_or(0)
    }
    fn write(&mut self, addr: u32, val: u8) {
        if let Some(b) = self.0.get_mut(addr as usize) {
            *b = val;
        }
    }
}

impl DmaMem for Vec<u8> {
    fn read(&mut self, addr: u32) -> u8 {
        self.get(addr as usize).copied().unwrap_or(0)
    }
    fn write(&mut self, addr: u32, val: u8) {
        if let Some(b) = self.get_mut(addr as usize) {
            *b = val;
        }
    }
}

pub struct Wd177x {
    status: u8,
    track: u8,
    sector: u8,
    command: u8,
    data: u8,
    wdata_valid: bool,
    rdata_valid: bool,
    completion: bool,
    fifo: VecDeque<u16>,
    dma_mode: u8,
    dma: Dma,
    addr: u32,
    len: u16,
    index_enable: bool,
    index_polarity: bool,
    goto_track: u8,
    step_time: u8,
    /// The head, stepped by the stepper (`cur_track`, c1581_drive.vhd): what the firmware reads as the drive's track.
    cur_track: u8,
    step_timer: u8,
    index_cnt: u8,
    index_out: bool,
    /// Drive cycle the model has been run to, and the cycles into the current millisecond.
    clk: u64,
    ms_frac: u64,
    /// The drive's reset line, held.
    in_reset: bool,
}

impl Default for Wd177x {
    fn default() -> Self {
        let mut w = Wd177x {
            status: 0,
            track: 0,
            sector: 0,
            command: 0,
            data: 0,
            wdata_valid: false,
            rdata_valid: false,
            completion: false,
            fifo: VecDeque::new(),
            dma_mode: 0,
            dma: Dma::Idle,
            addr: 0,
            len: 0,
            index_enable: false,
            index_polarity: false,
            goto_track: 0,
            step_time: 0,
            cur_track: 0,
            step_timer: 0,
            index_cnt: INDEX_PERIOD,
            index_out: false,
            clk: 0,
            ms_frac: 0,
            in_reset: false,
        };
        w.reset();
        w
    }
}

impl Wd177x {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reset branch of wd177x.vhd, the stepper and the mechanics (`drv_reset`).
    fn reset(&mut self) {
        self.wdata_valid = false;
        self.rdata_valid = false;
        self.track = 0x01;
        self.sector = 0;
        self.command = 0;
        self.status = 0;
        self.dma_mode = 0;
        self.dma = Dma::Idle;
        self.step_time = 0x0C;
        self.completion = false;
        self.goto_track = 0;
        self.fifo.clear();
        self.step_timer = 1;
        self.index_cnt = INDEX_PERIOD;
        self.index_out = false;
        self.cur_track = 0;
    }

    /// The drive's reset line: held from `true` to `false`; the model stays in its reset state meanwhile.
    pub fn set_reset(&mut self, held: bool) {
        self.in_reset = held;
        if held {
            self.reset();
        }
    }

    /// The drive clock is `clk` without time having passed: TRX64 restarts it at every drive reset (Spec 875).
    pub fn rebase(&mut self, clk: u64) {
        (self.clk, self.ms_frac) = (clk, 0);
    }

    /// The interrupt to the firmware: a command or a completion waiting (`io_irq <= command_fifo_valid`).
    pub fn irq(&self) -> bool {
        !self.fifo.is_empty()
    }

    /// The head's track, for the drive window's TRACK register.
    pub fn cur_track(&self) -> u8 {
        self.cur_track
    }

    // ---- the 1581 CPU's side ($6000-$7FFF, four registers mirrored) ----

    /// A 1581 read of register `addr & 3`.
    pub fn cpu_read(&mut self, addr: u16, mem: &mut dyn DmaMem) -> u8 {
        let v = self.cpu_peek(addr);
        if addr & 3 == 3 {
            if self.rdata_valid {
                self.status &= !ST_DRQ;
            }
            self.rdata_valid = false;
            self.settle(mem);
        }
        v
    }

    pub fn cpu_peek(&self, addr: u16) -> u8 {
        match addr & 3 {
            0 => {
                let mut s = self.status;
                if self.command & 0x80 == 0 && self.index_enable {
                    s = (s & !ST_DRQ) | u8::from(self.index_out != self.index_polarity) << 1;
                }
                s
            }
            1 => self.track,
            2 => self.sector,
            _ => self.data,
        }
    }

    /// A 1581 store to register `addr & 3`. A command sets BUSY at once and queues for the firmware; only a Force
    /// Interrupt ($Dx) is taken while busy.
    pub fn cpu_store(&mut self, addr: u16, val: u8, mem: &mut dyn DmaMem) {
        if self.in_reset {
            return;
        }
        match addr & 3 {
            0 => {
                if self.status & ST_BUSY == 0 || val >> 4 == 0xD {
                    self.command = val;
                    self.status |= ST_BUSY;
                    self.completion = false;
                    self.push();
                    self.wdata_valid = false;
                }
            }
            1 => self.track = val,
            2 => self.sector = val,
            _ => {
                self.data = val;
                self.wdata_valid = true;
            }
        }
        self.settle(mem);
    }

    // ---- the firmware's side (drive window + 0x1800, wd177x.vhd `io_req`) ----

    pub fn fw_read(&self, off: u16) -> u8 {
        let head = self.fifo.front().copied().unwrap_or(0);
        match off & 0xF {
            0x0 => head as u8,
            0x1 => self.track,
            0x2 => self.sector,
            0x3 => self.data,
            0x4 | 0x5 => self.status,
            0x6 => (head >> 8) as u8 & 1 | u8::from(self.in_reset) << 6 | u8::from(!self.fifo.is_empty()) << 7,
            0x7 => self.dma_mode,
            0xC => self.len as u8,
            0xD => (self.len >> 8) as u8 & 0x3F,
            0xE => u8::from(self.cur_track != self.goto_track),
            0xF => self.step_time,
            _ => 0,
        }
    }

    pub fn fw_write(&mut self, off: u16, val: u8, mem: &mut dyn DmaMem) {
        match off & 0xF {
            0x0 => (self.index_enable, self.index_polarity) = (val & 1 != 0, val & 2 != 0),
            0x1 => self.track = val,
            0x4 => self.status &= !val,
            0x5 => self.status |= val,
            0x6 => {
                self.fifo.pop_front();
            }
            0x7 => self.dma_mode = val & 3,
            0x8 => self.addr = (self.addr & !0xFF) | u32::from(val),
            0x9 => self.addr = (self.addr & !0xFF00) | u32::from(val) << 8,
            0xA => self.addr = (self.addr & !0xFF_0000) | u32::from(val) << 16,
            0xC => self.len = (self.len & !0xFF) | u16::from(val),
            0xD => self.len = (self.len & 0xFF) | u16::from(val & 0x3F) << 8,
            0xE => self.goto_track = val & 0x7F,
            0xF => self.step_time = val & 0x1F,
            _ => {}
        }
        self.settle(mem);
    }

    fn push(&mut self) {
        if self.fifo.len() < FIFO_DEPTH {
            self.fifo.push_back(u16::from(self.completion) << 8 | u16::from(self.command));
        }
    }

    /// The DMA state machine until it waits: for the 1581 to take a byte (read) or give one (write), or for time.
    fn settle(&mut self, mem: &mut dyn DmaMem) {
        if self.in_reset {
            return;
        }
        loop {
            match (self.dma, self.dma_mode) {
                (Dma::Idle, 0b01) => {
                    if self.rdata_valid {
                        return;
                    }
                    if self.len == 0 {
                        self.status &= !ST_BUSY;
                        self.dma_mode = 0;
                        return;
                    }
                    self.data = mem.read(self.addr & 0xFF_FFFF);
                    self.rdata_valid = true;
                    self.status |= ST_DRQ;
                    self.len -= 1;
                    self.addr = self.addr.wrapping_add(1) & 0xFF_FFFF;
                }
                (Dma::Idle, 0b10) => {
                    self.status |= ST_DRQ;
                    if !self.wdata_valid {
                        return;
                    }
                    self.status &= !ST_DRQ;
                    self.wdata_valid = false;
                    self.len = self.len.wrapping_sub(1) & 0x3FFF;
                    mem.write(self.addr & 0xFF_FFFF, self.data);
                    self.addr = self.addr.wrapping_add(1) & 0xFF_FFFF;
                    if self.len == 0 {
                        self.dma_mode = 0b11;
                        self.dma = Dma::WriteDelay(0xFF);
                        self.completion = true;
                        self.push();
                        return;
                    }
                }
                (Dma::Idle, 0b00) => {
                    self.status &= !ST_DRQ;
                    self.wdata_valid = false;
                    return;
                }
                _ => return,
            }
        }
    }

    /// Run to drive cycle `clk`: the stepper and the index pulse on 1 kHz, the write delay on 4 MHz. `motor` is the
    /// 1581's motor line (CIA PA2).
    pub fn run_to(&mut self, clk: u64, motor: bool) {
        let cycles = clk.saturating_sub(self.clk);
        self.clk = self.clk.max(clk);
        if self.in_reset {
            return;
        }
        if let Dma::WriteDelay(n) = self.dma {
            let ticks = cycles.saturating_mul(2).min(u64::from(u16::MAX)) as u16;
            if ticks >= n {
                self.dma = Dma::Idle;
                self.status &= !ST_BUSY;
            } else {
                self.dma = Dma::WriteDelay(n - ticks);
            }
        }
        self.ms_frac += cycles;
        let mut ms = self.ms_frac / CYCLES_PER_MS;
        self.ms_frac %= CYCLES_PER_MS;
        // The stepper steps whenever its timer is 0, so a step can come without a tick.
        self.step();
        while ms > 0 {
            ms -= 1;
            self.step_timer = self.step_timer.saturating_sub(1);
            if motor {
                if self.index_cnt == 0 {
                    (self.index_out, self.index_cnt) = (true, INDEX_PERIOD);
                } else {
                    (self.index_out, self.index_cnt) = (false, self.index_cnt - 1);
                }
            }
            self.step();
        }
    }

    fn step(&mut self) {
        if self.step_timer != 0 {
            return;
        }
        if self.cur_track < self.goto_track && self.cur_track != MAX_TRACK {
            self.cur_track += 1;
            self.step_timer = self.step_time;
        } else if self.cur_track > self.goto_track {
            self.cur_track -= 1;
            self.step_timer = self.step_time;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUF: u32 = 0x1000;

    /// The firmware's ISR: the command and its completion flag, then the pop (wd177x.cc:106-117).
    fn take(w: &mut Wd177x, mem: &mut Vec<u8>) -> (u8, bool) {
        let (cmd, done) = (w.fw_read(0), w.fw_read(6) & 1 != 0);
        w.fw_write(6, 0, mem);
        (cmd, done)
    }

    fn dma(w: &mut Wd177x, mem: &mut Vec<u8>, mode: u8, len: u16) {
        for (i, b) in BUF.to_le_bytes().into_iter().take(3).enumerate() {
            w.fw_write(8 + i as u16, b, mem);
        }
        w.fw_write(0xC, len as u8, mem);
        w.fw_write(0xD, (len >> 8) as u8, mem);
        w.fw_write(7, mode, mem);
    }

    #[test]
    fn a_command_is_busy_at_the_store_and_waits_for_the_firmware() {
        let (mut w, mut mem) = (Wd177x::new(), vec![0u8; 0x2000]);
        assert_eq!((w.cpu_peek(0), w.cpu_peek(1), w.irq()), (0, 1, false), "reset: idle, track register 1");
        w.cpu_store(0, 0x88, &mut mem);
        assert_eq!(w.cpu_peek(0) & ST_BUSY, ST_BUSY, "BUSY at the store: the DOS's $CBFA sees it");
        w.cpu_store(0, 0x0B, &mut mem);
        assert!(w.irq() && w.fifo.len() == 1, "a second command while busy is dropped");
        w.cpu_store(0, 0xD0, &mut mem);
        assert_eq!(w.fifo.len(), 2, "Force Interrupt is taken while busy");
        assert_eq!(take(&mut w, &mut mem), (0x88, false));
        assert_eq!(take(&mut w, &mut mem), (0xD0, false));
        assert!(!w.irq());
    }

    #[test]
    fn read_sector_by_dma() {
        let (mut w, mut mem) = (Wd177x::new(), vec![0u8; 0x2000]);
        mem[BUF as usize..BUF as usize + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        w.cpu_store(0, 0x88, &mut mem);
        take(&mut w, &mut mem);
        dma(&mut w, &mut mem, 0b01, 4);
        let mut got = Vec::new();
        while w.cpu_peek(0) & ST_BUSY != 0 {
            if w.cpu_peek(0) & ST_DRQ != 0 {
                got.push(w.cpu_read(3, &mut mem));
            }
            assert!(got.len() <= 4);
        }
        assert_eq!(got, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(w.fw_read(7), 0, "DMA off once the length ran out");
    }

    #[test]
    fn write_sector_by_dma_completes_after_64_us() {
        let (mut w, mut mem) = (Wd177x::new(), vec![0u8; 0x2000]);
        w.cpu_store(0, 0xA8, &mut mem);
        take(&mut w, &mut mem);
        dma(&mut w, &mut mem, 0b10, 3);
        for b in [0xAA, 0xBB, 0xCC] {
            assert_eq!(w.cpu_peek(0) & ST_DRQ, ST_DRQ, "asks for the next byte");
            w.cpu_store(3, b, &mut mem);
        }
        assert_eq!(&mem[BUF as usize..BUF as usize + 3], [0xAA, 0xBB, 0xCC]);
        assert_eq!(take(&mut w, &mut mem), (0xA8, true), "the completion entry: the firmware writes the file now");
        assert_eq!(w.fw_read(7), 0b11);
        w.run_to(100, false);
        assert_eq!(w.cpu_peek(0) & ST_BUSY, ST_BUSY, "still in the 64 µs delay");
        w.run_to(130, false);
        assert_eq!(w.cpu_peek(0) & ST_BUSY, 0);
    }

    #[test]
    fn the_stepper_moves_the_head_at_step_time() {
        let (mut w, mut mem) = (Wd177x::new(), vec![0u8; 16]);
        w.fw_write(0xF, 3, &mut mem);
        w.fw_write(0xE, 2, &mut mem);
        w.run_to(1, false);
        assert_eq!(w.cur_track(), 0, "the reset timer runs out on the first tick");
        w.run_to(2000, false);
        assert_eq!((w.cur_track(), w.fw_read(0xE)), (1, 1));
        w.run_to(2000 + 3 * 2000, false);
        assert_eq!((w.cur_track(), w.fw_read(0xE)), (2, 0), "3 ms a step, then not busy");
    }

    #[test]
    fn the_index_pulse_shows_in_type_i_status_with_the_motor_on() {
        let (mut w, mut mem) = (Wd177x::new(), vec![0u8; 16]);
        w.fw_write(0, 1, &mut mem);
        let mut pulses = 0;
        for ms in 1..=1000 {
            w.run_to(ms * 2000, true);
            pulses += usize::from(w.cpu_peek(0) & ST_DRQ != 0);
        }
        assert_eq!(pulses, 5, "one 1 ms pulse per 200 ms");
        w.set_reset(true);
        w.cpu_store(0, 0x08, &mut mem);
        assert!(!w.irq(), "held in reset");
    }
}
