//! The U64's IEC processor (S30): the microcoded engine behind Software IEC, and the register shell the firmware
//! talks to at 0x10028000. A clock-for-clock reading of `fpga/io/iec_interface/vhdl_source/iec_processor.vhd` and
//! `iec_processor_io.vhd`; the program is the firmware's own (`iec_code.iec`), uploaded into the code RAM at boot
//! (`iec_interface.cc:71-81`), so nothing of it lives here.
//!
//! Lines are one nibble each way, bit 0 CLK, 1 DATA, 2 ATN, 3 SRQ, `1` = released (high): the engine's drivers are
//! open collector and the bus is the wired AND of everyone's.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::Arc;

use trx64_core::iec_device::{IecDevice, IecLines, IecOut};

/// System clocks per microsecond: the engine's clock (`CLOCK_FREQ=100000000`, target/u64ii/riscv/ultimate/Makefile)
/// against its 1 MHz `tick`.
pub const SYS_PER_US: u32 = 100;

/// VERSION (iec_processor_io.vhd:182).
const VERSION: u8 = 0x25;
/// Code RAM: 512 words of 30 bits, written bytewise little-endian from offset 0x800 (iec_processor_io.vhd:113-124).
const CODE_WORDS: usize = 512;
const CODE_BASE: u16 = 0x800;
/// Up FIFO: 2048 × 9 bits, almost full at 1535 (iec_processor_io.vhd:126-148).
const UP_DEPTH: usize = 2048;
const UP_AFULL: usize = 1535;
/// Down FIFO: 15 × 10 bits (iec_processor_io.vhd:150-165).
const DOWN_DEPTH: usize = 15;
/// Return stack: `distributed_stack` with a 4-bit pointer.
const STACK_DEPTH: usize = 15;

/// Everything released, status and data register 0 (`out_vector <= X"F0000"` at reset).
const OUT_RESET: u32 = 0xF_0000;
/// `out_vector` bits: the drivers, IRQ_EN (status bit 0), EOI (status bit 4).
const OUT_DRIVERS: u32 = 16;
const OUT_IRQ_EN: u32 = 8;
const OUT_EOI: u32 = 12;

const OPC_LOAD: u32 = 0x0;
const OPC_POP: u32 = 0x1;
const OPC_PUSHC: u32 = 0x2;
const OPC_PUSHD: u32 = 0x3;
const OPC_SUB: u32 = 0x4;
const OPC_COPY_BIT: u32 = 0x5;
const OPC_IRQ: u32 = 0x6;
const OPC_RET: u32 = 0x7;
const OPC_IF: u32 = 0x8;
const OPC_CLRSTACK: u32 = 0x9;
const OPC_WAIT: u32 = 0xC;
const OPC_RESET_ST: u32 = 0xD;
const OPC_RESET_DRV: u32 = 0xE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    GetInst,
    SlowRam,
    Decode,
    WaitTrue,
}

/// The engine, its code RAM and both FIFOs.
pub struct IecProc {
    code: Box<[u32; CODE_WORDS]>,
    /// RESET_ENABLE bit 0: the engine runs only while it is set (`proc_reset <= not enable`).
    enabled: bool,
    pc: u16,
    state: State,
    instr: u32,
    timer: u16,
    timer_done: bool,
    /// `out_vector`: drivers 19:16, status 15:8, data register 7:0.
    out: u32,
    valid: bool,
    ctrl: bool,
    timeout: bool,
    stack: Vec<u16>,
    atn_prev: bool,
    /// Engine → firmware, bit 8 = control code.
    up: VecDeque<u16>,
    /// Firmware → engine, bits 9:8 = the register written (8 data, 9 control, A EOI).
    down: VecDeque<u16>,
    /// `down_fifo_flush`: set by an ATN edge and by reset, cleared by TX_FIFO_RELEASE. The FIFO stays empty and
    /// reads as full meanwhile.
    flush: bool,
    irq_status: bool,
    irq_enable: bool,
}

impl Default for IecProc {
    fn default() -> Self {
        let mut p = IecProc {
            code: Box::new([0; CODE_WORDS]),
            enabled: false,
            pc: 0,
            state: State::GetInst,
            instr: 0,
            timer: 0xFFF,
            timer_done: false,
            out: OUT_RESET,
            valid: false,
            ctrl: false,
            timeout: false,
            stack: Vec::new(),
            atn_prev: true,
            up: VecDeque::new(),
            down: VecDeque::new(),
            flush: true,
            irq_status: false,
            irq_enable: false,
        };
        p.reset();
        p
    }
}

impl IecProc {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines the engine drives, `1` = released.
    pub fn drivers(&self) -> u8 {
        (self.out >> OUT_DRIVERS) as u8 & 0x0F
    }

    /// Whether the engine runs (RESET_ENABLE).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// `proc_reset`: the engine to address 0 with every line released, both FIFOs emptied.
    fn reset(&mut self) {
        self.flush = true;
        self.state = State::GetInst;
        self.pc = 0;
        self.out = OUT_RESET;
        self.stack.clear();
        self.up.clear();
        self.down.clear();
    }

    /// A firmware read of register `off` (0x000-0xFFF). Reads of the up FIFO's data registers take the entry.
    pub fn read(&mut self, off: u16) -> u8 {
        let v = self.peek(off);
        if off & CODE_BASE == 0 && (off & 0xF == 0x6 || off & 0xC == 0x8) {
            self.up.pop_front();
        }
        v
    }

    /// Register `off` without side effects. Only address bits 3:0 are decoded, the code RAM included: it is write-only.
    pub fn peek(&self, off: u16) -> u8 {
        let head = self.up.front().copied().unwrap_or(0);
        match off & 0xF {
            0x0 => VERSION,
            0x1 => u8::from(self.down.is_empty()) | u8::from(self.down.len() >= DOWN_DEPTH || self.flush) << 1,
            0x2 => u8::from(self.up.is_empty()) | u8::from(self.up.len() >= UP_DEPTH) << 1 | ((head >> 8) as u8) << 7,
            0x6 | 0x8..=0xB => head as u8,
            0x7 => (head >> 8) as u8 & 1,
            0xC => u8::from(self.irq_status),
            _ => 0,
        }
    }

    /// A firmware write of register `off`.
    pub fn write(&mut self, off: u16, val: u8) {
        if off & CODE_BASE != 0 {
            let (word, byte) = (usize::from(off & 0x7FF) >> 2, u32::from(off & 3) * 8);
            let w = &mut self.code[word];
            *w = (*w & !(0xFF << byte)) | u32::from(val) << byte;
            return;
        }
        match off & 0xF {
            0x3 => {
                self.enabled = val & 1 != 0;
                self.reset();
            }
            0xC => {
                self.irq_status = false;
                self.irq_enable = val & 1 != 0;
            }
            0xD => self.flush = false,
            0x8..=0xB if !self.flush && self.down.len() < DOWN_DEPTH => {
                self.down.push_back((off & 3) << 8 | u16::from(val));
            }
            _ => {}
        }
    }

    /// Run `us` microseconds. `bus` gets the engine's drivers and answers the bus lines as the wired AND of everyone,
    /// so it is asked again whenever the drivers may have changed.
    pub fn run_us(&mut self, us: u64, bus: &mut impl FnMut(u8) -> u8) {
        for _ in 0..us {
            if !self.enabled {
                return;
            }
            let mut inputs = bus(self.drivers());
            for clock in 0..SYS_PER_US {
                let before = (self.pc, self.state, self.drivers(), self.timer);
                self.clock(inputs, clock == 0);
                if self.drivers() != before.2 {
                    inputs = bus(self.drivers());
                } else if (self.pc, self.state, self.timer) == (before.0, before.1, before.3) {
                    // Blocked (a POP or PUSH that cannot, a WAIT whose condition is false): nothing changes until the
                    // next tick or the lines move, and the lines are sampled again next microsecond.
                    break;
                }
            }
        }
    }

    /// One system clock of `iec_processor.vhd`'s process; `tick` is the 1 MHz strobe.
    fn clock(&mut self, inputs: u8, tick: bool) {
        if tick {
            if self.timer == 1 {
                self.timer_done = true;
            }
            self.timer = self.timer.saturating_sub(1);
        }
        match self.state {
            State::GetInst => {
                self.instr = self.code[usize::from(self.pc)] & 0x3FFF_FFFF;
                self.pc = (self.pc + 1) % CODE_WORDS as u16;
                self.state = State::SlowRam;
            }
            State::SlowRam => self.state = State::Decode,
            State::Decode => self.decode(inputs),
            State::WaitTrue => {
                if self.timer_done {
                    self.state = State::GetInst;
                    self.timeout = true;
                } else if self.selected(inputs) {
                    self.state = State::GetInst;
                }
            }
        }
        let atn = inputs & 4 != 0;
        if !atn && self.atn_prev && self.out & 1 << OUT_IRQ_EN != 0 {
            self.flush = true;
            self.down.clear();
            self.pc = 1;
            self.state = State::GetInst;
        }
        self.atn_prev = atn;
    }

    fn decode(&mut self, inputs: u8) {
        let i = self.instr;
        let (opcode, operand, databyte) = (i >> 20 & 0xF, (i >> 8 & 0xFFF) as u16, (i & 0xFF) as u8);
        self.timer_done = false;
        self.timer = operand;
        self.state = State::GetInst;
        match opcode {
            OPC_LOAD => self.set_data(databyte),
            OPC_RESET_ST => self.out = (self.out & !0xFF00) | 0x0100,
            OPC_RESET_DRV => self.out |= 0xF << OUT_DRIVERS,
            OPC_IRQ => self.irq_status = true,
            OPC_PUSHC | OPC_PUSHD => {
                if self.up.len() < UP_DEPTH {
                    let ctrl = u16::from(opcode == OPC_PUSHC) << 8;
                    self.up.push_back(ctrl | (self.out & 0xFF) as u16);
                } else {
                    self.state = State::Decode;
                }
            }
            OPC_POP => {
                let head = self.down.front().copied();
                let h = head.unwrap_or(0);
                self.set_data(h as u8);
                self.ctrl = h & 0x100 != 0;
                self.out = (self.out & !(1 << OUT_EOI)) | u32::from(h >> 9 & 1) << OUT_EOI;
                self.valid = head.is_some();
                if head.is_some() && databyte & 2 == 0 {
                    self.down.pop_front();
                }
                if head.is_none() && databyte & 1 == 0 {
                    self.state = State::Decode;
                }
            }
            OPC_COPY_BIT => {
                let bit = u32::from(databyte & 0x1F);
                if bit < 20 {
                    self.out = (self.out & !(1 << bit)) | u32::from(self.selected(inputs)) << bit;
                }
            }
            OPC_IF => {
                if self.selected(inputs) {
                    self.pc = operand % CODE_WORDS as u16;
                }
            }
            OPC_WAIT => {
                self.timeout = false;
                self.state = State::WaitTrue;
            }
            OPC_SUB => {
                if self.stack.len() < STACK_DEPTH {
                    self.stack.push(self.pc);
                }
                self.pc = operand % CODE_WORDS as u16;
            }
            OPC_RET => self.pc = self.stack.pop().unwrap_or(0),
            OPC_CLRSTACK => self.stack.clear(),
            _ => {}
        }
    }

    fn set_data(&mut self, v: u8) {
        self.out = (self.out & !0xFF) | u32::from(v);
    }

    /// `selected_bit`: bit `select` of `input_vector`, inverted when bit 29 says so (iec_processor.vhd:108-122).
    fn selected(&self, inputs: u8) -> bool {
        let i = self.instr;
        let (mask, value) = ((i >> 4 & 0xF) as u8, (i & 0xF) as u8);
        let iv: u32 = 1 << 31
            | u32::from(self.ctrl) << 29
            | u32::from(self.valid) << 28
            | u32::from(self.timeout) << 27
            | u32::from(self.up.len() >= UP_AFULL) << 26
            | u32::from(inputs & mask == value) << 25
            | u32::from(self.out & 0xFF == i & 0xFF) << 24
            | u32::from(inputs & 0xF) << 20
            | (self.out & 0xF_FFFF);
        (iv >> (i >> 24 & 0x1F) & 1 != 0) != (i >> 29 & 1 != 0)
    }
}

/// The engine shared by the firmware's register window (`Trx64Backend::iec_*`) and the device TRX64 holds on the bus.
#[derive(Clone, Default)]
pub struct IecHandle(Arc<IecCell>);

#[derive(Default)]
struct IecCell(UnsafeCell<IecProc>);

// SAFETY: the backend, TRX64's `Machine` and every copy of the handle live on the emulation thread; `Send` is only
// required because `IecDevice: Send`.
unsafe impl Send for IecCell {}
unsafe impl Sync for IecCell {}

impl IecHandle {
    /// Run `f` on the engine. Calls do not nest: the backend never holds one across a call into TRX64, and the device
    /// holds one only inside a single `IecDevice` call.
    pub fn with<R>(&self, f: impl FnOnce(&mut IecProc) -> R) -> R {
        // SAFETY: single thread, and no two borrows are live at once (see above).
        f(unsafe { &mut *self.0 .0.get() })
    }
}

/// The engine on TRX64's IEC bus (Spec 874), in slot 4: C64 cycles turned into the engine's microseconds at the
/// model's rate, the bus lines as the rest of it drives them, the engine's CLK and DATA pulls back.
pub struct IecBusDevice {
    proc: IecHandle,
    /// C64 cycle the engine has been run to.
    clk: u64,
    /// The model's clock, Hz.
    hz: u64,
    /// Cycles × 1 000 000 not yet turned into whole microseconds.
    frac: u64,
    /// The lines since the last call: the rest of the bus until `clk_to`'s cycle.
    lines: IecLines,
}

impl IecBusDevice {
    pub fn new(proc: IecHandle, hz: u64) -> Self {
        IecBusDevice { proc, clk: 0, hz: hz.max(1), frac: 0, lines: IecLines::RELEASED }
    }
}

/// The engine's input nibble: the rest of the bus ANDed with its own drivers (`inputs_raw`), SRQ released.
fn wire(rest: IecLines, drivers: u8) -> u8 {
    u8::from(rest.clk && drivers & 1 != 0)
        | u8::from(rest.data && drivers & 2 != 0) << 1
        | u8::from(rest.atn && drivers & 4 != 0) << 2
        | 8
}

impl IecDevice for IecBusDevice {
    fn name(&self) -> String {
        "Ultimate IEC processor".into()
    }

    fn clock_to(&mut self, clk: u64, bus: IecLines) {
        self.frac += clk.saturating_sub(self.clk) * 1_000_000;
        self.clk = self.clk.max(clk);
        let us = self.frac / self.hz;
        self.frac %= self.hz;
        let rest = self.lines;
        self.proc.with(|p| p.run_us(us, &mut |d| wire(rest, d)));
        self.lines = bus;
    }

    fn outputs(&self) -> IecOut {
        let d = self.proc.with(|p| p.drivers());
        IecOut { clk: d & 1 == 0, data: d & 2 == 0 }
    }

    fn rebase(&mut self, clk: u64, bus: IecLines) {
        (self.clk, self.frac, self.lines) = (clk, 0, bus);
    }

    fn set_cpu_hz(&mut self, hz: u32) {
        self.hz = u64::from(hz).max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instruction word: invert, select, opcode, operand, low byte.
    fn op(invert: bool, select: u32, opcode: u32, operand: u32, low: u32) -> u32 {
        u32::from(invert) << 29 | select << 24 | opcode << 20 | operand << 8 | low
    }

    fn load(p: &mut IecProc, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            for (b, byte) in w.to_le_bytes().into_iter().enumerate() {
                p.write(CODE_BASE + (i * 4 + b) as u16, byte);
            }
        }
    }

    /// Everyone else releases every line: the bus is the engine's own drivers.
    fn alone(drivers: u8) -> u8 {
        drivers
    }

    #[test]
    fn registers_and_the_down_fifo() {
        let mut p = IecProc::new();
        assert_eq!((p.read(0), p.read(1), p.read(2)), (0x25, 0b11, 0b01), "flushing: reads full until released");
        p.write(0x8, 0x11);
        assert_eq!(p.read(1) & 1, 1, "dropped while flushing");
        p.write(0xD, 0);
        p.write(0x8, 0x11);
        p.write(0x9, 0x22);
        p.write(0xA, 0x33);
        assert_eq!(p.read(1), 0);
        assert_eq!(p.down.iter().copied().collect::<Vec<_>>(), [0x011, 0x122, 0x233], "data, control, EOI");
        p.write(0x3, 1);
        assert!(p.enabled() && p.down.is_empty() && p.flush, "enabling resets the engine and its FIFOs");
    }

    #[test]
    fn the_engine_pushes_waits_and_calls() {
        let mut p = IecProc::new();
        load(
            &mut p,
            &[
                op(false, 0, OPC_LOAD, 0, 0x41),
                op(false, 0, OPC_PUSHC, 0, 0),
                op(false, 0, OPC_SUB, 6, 0),
                op(false, 30, OPC_WAIT, 50, 0),     // 3: 50 µs; vector bit 30 is constant 0, so no condition
                op(false, 30, OPC_COPY_BIT, 0, 17), // 4: DATA := 0: pull DATA
                op(false, 31, OPC_IF, 4, 0),        // 5: always: spin at 4
                op(false, 0, OPC_LOAD, 0, 0x99),    // 6: subroutine
                op(false, 0, OPC_PUSHD, 0, 0),
                op(false, 0, OPC_RET, 0, 0),
            ],
        );
        p.write(0x3, 1);
        p.run_us(10, &mut alone);
        assert_eq!((p.read(0x2), p.read(0x6)), (0x80, 0x41), "control code first");
        assert_eq!((p.read(0x7), p.read(0x6)), (0, 0x99), "then the subroutine's data byte");
        assert_eq!(p.drivers(), 0xF, "still waiting");
        p.run_us(45, &mut alone);
        assert_eq!(p.drivers(), 0xD, "DATA pulled after the 50 µs wait");
        assert!(p.timeout);
    }

    #[test]
    fn a_blocking_pop_waits_for_the_firmware() {
        let mut p = IecProc::new();
        load(&mut p, &[op(false, 0, OPC_POP, 0, 0), op(false, 0, OPC_PUSHD, 0, 0), op(false, 31, OPC_IF, 0, 0)]);
        p.write(0x3, 1);
        p.write(0xD, 0);
        p.run_us(5, &mut alone);
        assert_eq!(p.read(0x2) & 1, 1, "nothing to echo");
        p.write(0x8, 0x5A);
        p.run_us(1, &mut alone);
        assert_eq!(p.read(0x6), 0x5A, "echoed once the byte arrived");
    }

    /// `iec_code.b` as the firmware uploads it, with slot 0 at `device` and the other slots empty
    /// (`IecInterface::get_patch_locations`, `set_slot_devnum`, `configure`; iec_interface.cc:83-145), or None when
    /// the firmware is not built.
    fn firmware_engine(device: u8) -> Option<IecProc> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/1541ultimate/target/u64ii/riscv/ultimate/output/iec_code.b");
        let Ok(mut code) = std::fs::read(path) else {
            eprintln!("skipping: {path} not built");
            return None;
        };
        for i in (0..code.len() & !3).step_by(4) {
            let w = u32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
            let slot = w & 7;
            let dev = if slot == 0 { device } else { 0x1F };
            match w & 0x1F80_00F8 {
                0x1880_00F0 => code[i] = dev | 0x40,
                0x1880_00F8 => code[i] = dev | 0x20,
                _ => {}
            }
        }
        let mut p = IecProc::new();
        for (i, &b) in code.iter().enumerate() {
            p.write(CODE_BASE + i as u16, b);
        }
        p.write(0x3, 1);
        p.write(0xD, 0);
        Some(p)
    }

    /// A C64 on the other end of the bus: `lines` is what it drives (CLK, DATA, ATN; `1` released).
    struct Host {
        lines: u8,
    }

    impl Host {
        fn run(&self, p: &mut IecProc, us: u64) {
            let lines = self.lines;
            p.run_us(us, &mut |d| d & lines);
        }

        /// Run until the bus has `mask` at `value`, at most `max` µs; the µs it took.
        fn until(&self, p: &mut IecProc, mask: u8, value: u8, max: u64) -> Option<u64> {
            (0..max).find(|_| {
                self.run(p, 1);
                p.drivers() & self.lines & mask == value
            })
        }

        /// One byte, LSB first, the KERNAL's way: CLK released means ready, the listener releases DATA, then eight
        /// bits clocked on CLK, then the listener's DATA acknowledge.
        fn send(&mut self, p: &mut IecProc, byte: u8) {
            self.lines |= 1;
            assert!(self.until(p, 2, 2, 1000).is_some(), "listener ready (DATA released)");
            for bit in 0..8 {
                self.lines = (self.lines & !3) | if byte >> bit & 1 != 0 { 2 } else { 0 };
                self.run(p, 60);
                self.lines |= 1;
                self.run(p, 60);
            }
            self.lines = (self.lines & !1) | 2;
            assert!(self.until(p, 2, 0, 1000).is_some(), "byte acknowledged (DATA pulled)");
        }
    }

    fn up(p: &mut IecProc) -> Vec<u16> {
        let mut out = Vec::new();
        while p.read(0x2) & 1 == 0 {
            let ctrl = u16::from(p.read(0x7)) << 8;
            out.push(ctrl | u16::from(p.read(0x6)));
        }
        out
    }

    /// LISTEN 11 under ATN reaches the firmware as ATN begin, slot 1 addressed, ATN end; LISTEN 9 is not for us.
    #[test]
    fn the_firmware_microcode_takes_listen_under_atn() {
        let Some(mut p) = firmware_engine(11) else { return };
        p.run_us(20, &mut alone);
        let mut c64 = Host { lines: 0xF & !0x5 };
        c64.run(&mut p, 100);
        c64.send(&mut p, 0x20 | 11);
        c64.lines |= 4;
        c64.run(&mut p, 100);
        assert_eq!(up(&mut p), [0x141, 0x181, 0x142], "CTRL_ATN_BEGIN, CTRL_DEV1, CTRL_ATN_END");
        assert_eq!(p.drivers() & 2, 0, "listening: DATA held until the next byte");

        let Some(mut p) = firmware_engine(11) else { return };
        p.run_us(20, &mut alone);
        let mut c64 = Host { lines: 0xF & !0x5 };
        c64.run(&mut p, 100);
        c64.send(&mut p, 0x20 | 9);
        c64.lines |= 4;
        c64.run(&mut p, 100);
        assert_eq!(up(&mut p), [0x141, 0x142], "another device: no slot addressed");
        assert_eq!(p.drivers(), 0xF, "and the bus released");
    }

    /// The firmware's own program: an ATN edge sends the engine to its handler, which pulls DATA, waits for the
    /// C64's CLK and reports the start of ATN (iec_code.iec `atn_irq_vec`).
    #[test]
    fn the_firmware_microcode_answers_atn() {
        let Some(mut p) = firmware_engine(11) else { return };
        p.run_us(20, &mut alone);
        assert_eq!(p.drivers(), 0xF, "idle: every line released");
        // The C64 pulls ATN, then CLK.
        let mut c64 = |d: u8| d & !0x5;
        p.run_us(30, &mut c64);
        assert_eq!(p.drivers() & 0x2, 0, "DATA pulled: a device is present");
        assert_eq!((p.read(0x2) & 0x80, p.read(0x6)), (0x80, 0x41), "CTRL_ATN_BEGIN");
    }
}
