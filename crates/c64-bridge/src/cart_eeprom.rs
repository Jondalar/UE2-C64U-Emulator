//! The GMOD2 serial EEPROM as the U64 FPGA emulates it: `fpga/devices/vhdl_source/microwire_eeprom.vhd`, an ST M93C86
//! with 2 K of memory the firmware reads and writes directly at EEPROM_BASE 0x1004C000 (c64.cc:1640-1660).
//!
//! The VHDL runs at 50 MHz behind two-flop synchronizers; the C64 changes the pins at most once per bus cycle, so every
//! pin change here is one clock (with the clock edge, if any), followed by the clocks the state machine needs until it
//! waits for the next pin change. Memory writes use the address and data the step assigned, as the registered
//! `dpram` sees them one clock later; a read loads its byte before the next clock edge can shift it out.
//! Faithful to the VHDL where it differs from the real chip: no dummy 0 before read data, and WRALL/ERAL fill from
//! address 1 (the `fill` state increments the address in the clock that writes).

/// EEPROM memory size (`g_depth_bits => 11`).
pub const EEPROM_SIZE: usize = 0x800;
/// Offsets inside the firmware window: `io_req.address(11)` selects memory, else the dirty-flag register.
const MEM_BIT: u16 = 0x800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Selected,
    Instruction,
    Decode,
    CollectData,
    WaitDeselect,
    Execute,
    Error,
    Fill,
    Write2,
    Reading,
}

#[derive(Clone)]
pub struct Eeprom {
    mem: Vec<u8>,
    dirty: bool,
    state: State,
    count: u8,
    /// 12 instruction bits after the start bit.
    instr: u16,
    data_word: u16,
    write_enable: bool,
    /// `mem_address`, 11 bits.
    address: u16,
    data_out: bool,
    sel: bool,
    clk: bool,
    din: bool,
    /// A read was requested (`mem_en` in `decode`/`reading`); its byte loads into `data_word` on the next clock.
    load: bool,
}

impl Default for Eeprom {
    fn default() -> Self {
        Self::new()
    }
}

impl Eeprom {
    /// FPGA reset: idle, write disabled, not dirty. The memory powers up as 0xFF here (the BRAM has no init file;
    /// `find_eeprom` or a CRT chunk overwrites it before a GMOD2 cart starts, c64_crt.cc:256-280, 743-785).
    pub fn new() -> Self {
        Eeprom {
            mem: vec![0xFF; EEPROM_SIZE],
            dirty: false,
            state: State::Idle,
            count: 0,
            instr: 0,
            data_word: 0xFFFF,
            write_enable: false,
            address: 0,
            data_out: true,
            sel: false,
            clk: false,
            din: false,
            load: false,
        }
    }

    /// DO, what GMOD2 returns in `$DE00` bit 7 (all_carts_v5.vhd `slot_resp.data(7) <= ee_rdata`).
    pub fn data_out(&self) -> bool {
        self.data_out
    }

    /// The firmware window: memory at +0x800, the dirty flag (bit 0) below.
    pub fn io_read(&self, off: u16) -> u8 {
        if off & MEM_BIT != 0 {
            self.mem[usize::from(off & 0x7FF)]
        } else {
            u8::from(self.dirty)
        }
    }

    /// Memory writes do not set the dirty flag (only the microwire side does); a register write clears it.
    pub fn io_write(&mut self, off: u16, val: u8) {
        if off & MEM_BIT != 0 {
            self.mem[usize::from(off & 0x7FF)] = val;
        } else {
            self.dirty = false;
        }
    }

    /// New CS/CLK/DI levels from a GMOD2 `$DE00` write (bits 6, 5, 4).
    pub fn set_pins(&mut self, sel: bool, clk: bool, din: bool) {
        if (sel, clk, din) == (self.sel, self.clk, self.din) {
            return;
        }
        let rising = clk && !self.clk;
        (self.sel, self.clk, self.din) = (sel, clk, din);
        self.step(rising);
        self.settle();
    }

    /// Clocks without an edge until the state machine waits for a pin change. `fill` needs up to 2048.
    fn settle(&mut self) {
        for _ in 0..4 * EEPROM_SIZE {
            let before = self.state;
            self.step(false);
            let busy = matches!(self.state, State::Decode | State::Execute | State::Fill | State::Write2);
            if !busy && !self.load && self.state == before {
                break;
            }
        }
    }

    /// One 50 MHz clock of the `process(clock)` state machine, with `rising` the synchronized CLK edge.
    fn step(&mut self, rising: bool) {
        let (sel, din) = (self.sel, u16::from(self.din));
        let mut write: Option<u8> = None;
        if self.load && self.state == State::Reading {
            self.data_word = (self.data_word & 0xFF00) | u16::from(self.mem[usize::from(self.address)]);
            self.count = 7;
        }
        self.load = false;
        self.state = match self.state {
            State::Idle => {
                self.count = 11;
                self.data_out = true;
                self.data_word = 0xFFFF;
                if sel {
                    State::Selected
                } else {
                    State::Idle
                }
            }
            State::Selected if !sel => State::Idle,
            State::Selected if rising => {
                if din != 0 {
                    State::Instruction
                } else {
                    State::Error
                }
            }
            State::Instruction if !sel => State::Idle,
            State::Instruction if rising => {
                self.instr = ((self.instr << 1) | din) & 0xFFF;
                if self.count == 0 {
                    State::Decode
                } else {
                    self.count -= 1;
                    State::Instruction
                }
            }
            State::Decode => {
                self.count = 15;
                match self.instr {
                    i if i >> 10 == 0b01 || i >> 8 == 0b0001 => State::CollectData,
                    i if i >> 10 == 0b10 => {
                        self.address = (i & 0x3FF) << 1;
                        self.load = true;
                        State::Reading
                    }
                    _ => State::WaitDeselect,
                }
            }
            State::CollectData if !sel => State::Idle,
            State::CollectData if rising => {
                self.data_word = (self.data_word << 1) | din;
                if self.count == 0 {
                    State::WaitDeselect
                } else {
                    self.count -= 1;
                    State::CollectData
                }
            }
            State::WaitDeselect if rising => State::Error,
            State::WaitDeselect if !sel => State::Execute,
            State::Execute => {
                self.data_out = false;
                if self.instr & 0x400 != 0 {
                    // WRITE or ERASE of one word: high byte now, low byte in write2.
                    self.address = (self.instr & 0x3FF) << 1;
                    write = Some((self.data_word >> 8) as u8);
                    State::Write2
                } else {
                    match self.instr >> 8 {
                        0b0011 => {
                            self.write_enable = true;
                            State::Idle
                        }
                        0b0000 => {
                            self.write_enable = false;
                            State::Idle
                        }
                        0b0010 | 0b0001 => {
                            self.address = 0;
                            State::Fill
                        }
                        _ => State::Idle,
                    }
                }
            }
            State::Error if !sel => State::Idle,
            State::Fill => {
                write = Some(if self.address & 1 != 0 { self.data_word as u8 } else { (self.data_word >> 8) as u8 });
                if self.address == 0x7FF {
                    State::Idle
                } else {
                    self.address += 1;
                    State::Fill
                }
            }
            State::Write2 => {
                write = Some(self.data_word as u8);
                self.address |= 1;
                State::Idle
            }
            State::Reading if !sel => State::Idle,
            State::Reading if rising => {
                self.data_out = (self.data_word >> self.count) & 1 != 0;
                if self.count == 0 {
                    self.address = (self.address + 1) & 0x7FF;
                    self.load = true;
                } else {
                    self.count -= 1;
                }
                State::Reading
            }
            s => s,
        };
        if let Some(byte) = write.filter(|_| self.write_enable) {
            self.mem[usize::from(self.address)] = byte;
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clock `bits` (MSB first, `n` of them) in with CS high, as GMOD2 software does: data with CLK low, then CLK high.
    fn send(ee: &mut Eeprom, bits: u32, n: u32) {
        for i in (0..n).rev() {
            let d = (bits >> i) & 1 != 0;
            ee.set_pins(true, false, d);
            ee.set_pins(true, true, d);
        }
        ee.set_pins(true, false, false);
    }

    fn deselect(ee: &mut Eeprom) {
        ee.set_pins(false, false, false);
    }

    /// Start bit + 2-bit opcode + 10-bit address.
    fn command(ee: &mut Eeprom, opcode: u32, addr: u32) {
        send(ee, (1 << 12) | (opcode << 10) | (addr & 0x3FF), 13);
    }

    fn read_bits(ee: &mut Eeprom, n: u32) -> u32 {
        (0..n).fold(0, |acc, _| {
            ee.set_pins(true, true, false);
            let bit = u32::from(ee.data_out());
            ee.set_pins(true, false, false);
            (acc << 1) | bit
        })
    }

    #[test]
    fn write_needs_ewen_and_read_streams_bytes() {
        let mut ee = Eeprom::new();
        command(&mut ee, 0b01, 0x012);
        send(&mut ee, 0x1234, 16);
        deselect(&mut ee);
        assert_eq!((ee.io_read(0x800 + 0x24), ee.io_read(0)), (0xFF, 0), "write disabled after reset");

        command(&mut ee, 0b00, 0b11 << 8); // EWEN
        deselect(&mut ee);
        command(&mut ee, 0b01, 0x012);
        send(&mut ee, 0xA55A, 16);
        deselect(&mut ee);
        assert_eq!([ee.io_read(0x824), ee.io_read(0x825), ee.io_read(0)], [0xA5, 0x5A, 1], "word at 2*addr, dirty");
        ee.io_write(0, 1);
        assert_eq!(ee.io_read(0), 0, "a register write clears dirty");

        ee.io_write(0x826, 0xC3);
        assert_eq!(ee.io_read(0), 0, "firmware writes do not set dirty");
        command(&mut ee, 0b10, 0x012);
        assert_eq!(read_bits(&mut ee, 24), 0xA55AC3, "continuous read across words");
        deselect(&mut ee);
        assert!(ee.data_out(), "idle drives ready");
    }

    #[test]
    fn erase_eral_and_wrdis() {
        let mut ee = Eeprom::new();
        ee.mem.fill(0);
        command(&mut ee, 0b00, 0b11 << 8);
        deselect(&mut ee);
        command(&mut ee, 0b11, 0x001); // ERASE word 1
        deselect(&mut ee);
        assert_eq!([ee.io_read(0x802), ee.io_read(0x803), ee.io_read(0x804)], [0xFF, 0xFF, 0x00]);
        command(&mut ee, 0b00, 0b10 << 8); // ERAL
        deselect(&mut ee);
        assert_eq!((ee.io_read(0x800), ee.io_read(0x801), ee.io_read(0xFFF)), (0x00, 0xFF, 0xFF), "fill starts at 1");
        command(&mut ee, 0b00, 0b00 << 8); // WRDIS
        deselect(&mut ee);
        ee.io_write(0x810, 0x11);
        command(&mut ee, 0b01, 0x008);
        send(&mut ee, 0x0000, 16);
        deselect(&mut ee);
        assert_eq!(ee.io_read(0x810), 0x11);
    }

    #[test]
    fn a_zero_start_bit_is_an_error_until_deselect() {
        let mut ee = Eeprom::new();
        send(&mut ee, 0x0800, 13); // start bit 0, READ, address 0
        assert_eq!(ee.state, State::Error);
        deselect(&mut ee);
        assert_eq!(ee.state, State::Idle);
    }
}
