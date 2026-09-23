//! The U64 cartridge logic behind C64_CARTRIDGE_TYPE as a TRX64 cartridge mapper (docs/status/carts.md, S14 §W4-CART).
//!
//! [`CartLogic`] ports `fpga/cart_slot/vhdl_source/all_carts_v5.vhd` with the U64-II generics: ROM banks in DDR at
//! 0x03C00000 (22 cart bits, 4 MB; u2p_riscv_lattice.vhd:603-604), or with firmware before 3.15 at 0x00F00000 (20 cart
//! bits, 1 MB; [`CartRom`], docs/status/carts.md), cart RAM at 0x00EF0000 (64 K), GeoRAM at 0x01000000,
//! the GMOD2 EEPROM ([`Eeprom`], microwire_eeprom.vhd) and the freezer state machine of `freezer.vhd`. It reads and
//! writes that memory in guest DDR, lent by `C64Port` for each access (`C64Backend::lend_ddr`), where the firmware's CRT
//! loader put it (c64_crt.cc:291-358, linker.x:274-287). EXROM/GAME are gated by `cart_en` as `slot_server_v4.vhd`
//! (1083-1098) gates them.
//!
//! [`CartProxy`] is what TRX64 holds as `Machine::cartridge`. It shares the logic with the backend through
//! [`CartHandle`], puts it on the bus beside the physical expansion port's cartridge (CARTSLOT, slot.rs) and adds the
//! firmware's forced ULTIMAX decode (C64_MODE bit 1, c64.cc:451-456).
//!
//! The VHDL is clocked at 50 MHz; here a register changes on the bus access that writes (or reads) it and the outputs
//! follow at once, which no C64 program can tell apart. TRX64 only re-runs its PLA after `$00/$01` writes and consumed
//! `$DE00-$DFFF` writes, so line changes from reads, timers and the freeze button reach it through the backend
//! (`slot::Slot::lines_changed`, [`CartLogic::run_hints`]).

use std::cell::UnsafeCell;
use std::sync::Arc;

use trx64_core::cart::{BankInfo, CartLines, CartMapper, CartState, MapperType};
use trx64_core::expansion::{Access, ExpansionDevice};
use ue2_core::c64host::CartRom;

use crate::cart_eeprom::Eeprom;
use crate::slot::SlotHandle;

/// C64_CARTRIDGE_TYPE bits 4:0 select the logic, bits 7:5 the variant (c64.h:115-163, cart_slot_registers.vhd).
pub const TYPE_MASK: u8 = 0x1F;

/// `cart_logic` values (all_carts_v5.vhd:95-126; c64.h:125-146).
pub const CART_TYPE_NONE: u8 = 0x00;
pub const CART_TYPE_NORMAL: u8 = 0x01;
const EPYX: u8 = 0x02;
const C128: u8 = 0x03;
const WESTERMANN: u8 = 0x04;
const SBASIC: u8 = 0x05;
const BBASIC: u8 = 0x06;
const BLACKBOX_V3: u8 = 0x07;
const OCEAN_8K: u8 = 0x08;
const OCEAN_16K: u8 = 0x09;
const SYSTEM3: u8 = 0x0A;
const SUPERGAMES: u8 = 0x0B;
const BLACKBOX_V8: u8 = 0x0C;
const ZAXXON: u8 = 0x0D;
const BLACKBOX_V9: u8 = 0x0E;
const MEGABYTER: u8 = 0x0F;
const PAGEFOX: u8 = 0x10;
const EASY_FLASH: u8 = 0x11;
const FC: u8 = 0x18;
const FC3: u8 = 0x19;
const SS5: u8 = 0x1A;
const ACTION: u8 = 0x1B;
const KCS: u8 = 0x1C;
const GEORAM: u8 = 0x1F;

/// ITU capability bit for the GMOD2 EEPROM (itu.h:71). Without it the firmware refuses GMOD2 CRTs (c64_crt.cc:214).
pub const CAPAB_EEPROM: u32 = 0x0040_0000;

/// ITU capability bit for the sampler (itu.h:70). Without it the firmware never maps `$DF20-$DFFF`, offers no
/// "Play MOD" (filetype_reu.cc:41-45) and answers `/v1/runners:modplay` with 501 (route_runners.cc:239-243).
pub const CAPAB_SAMPLER: u32 = 0x0020_0000;

/// ITU capability bit for the Ultimate Command Interface (itu.h:67). Without it the firmware starts no "UCI Server"
/// task (command_intf.cc:44) and offers no "Command Interface" setting (c64.cc:311,328).
pub const CAPAB_COMMAND_INTF: u32 = 0x0004_0000;

/// The logic this module models. C128 carts (0x03) serve $8000-$FFFF of a C128 only; the unused codes select nothing.
pub fn modelled(logic: u8) -> bool {
    !matches!(logic, C128 | 0x12..=0x17 | 0x1D | 0x1E)
}

/// DDR placement: `g_rom_base_cart` (3.15 on, [`CartRom::LARGE`]), `g_ram_base_cart`, `g_ram_base_reu`
/// (u2p_riscv_lattice.vhd:603-604; ultimate_logic_32.vhd:37, 849; linker.x:274-287).
#[cfg(test)]
const ROM_BASE: usize = CartRom::LARGE.base;
const RAM_BASE: usize = 0x00EF_0000;
const GEORAM_BASE: usize = 0x0100_0000;
/// ROM the 22 cart bits address, and the 64 K of cart RAM (`g_ram_base_cart`, linker.x:274-287).
pub const ROM_SIZE: usize = 0x40_0000;
pub const RAM_SIZE: usize = 0x1_0000;

/// Where the logic finds its ROM, cart RAM and GeoRAM in the memory it is given (`CartLogic::set_ddr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub rom: usize,
    /// `rom_addr(g_max_cart_bits-1 downto 13) <= bank_bits(...)`: the bank bits the cart ROM's size leaves.
    pub rom_bank_mask: u32,
    pub ram: usize,
    pub geo: usize,
}

impl Layout {
    /// Guest DDR of the U64-II, where the firmware's CRT loader puts the internal cartridge: 3.15 and later.
    pub const GUEST: Layout = Layout::guest(CartRom::LARGE);
    /// A cartridge's own memory (CARTSLOT, slot.rs): ROM from 0 with 22 cart bits, cart RAM after it, no GeoRAM.
    pub const OWN: Layout =
        Layout { rom: 0, rom_bank_mask: bank_mask(ROM_SIZE), ram: ROM_SIZE, geo: ROM_SIZE + RAM_SIZE };

    /// Guest DDR with the cartridge ROM where this firmware puts it (docs/status/carts.md, "Cartridge ROM in DDR").
    pub const fn guest(rom: CartRom) -> Layout {
        Layout { rom: rom.base, rom_bank_mask: bank_mask(rom.size), ram: RAM_BASE, geo: GEORAM_BASE }
    }
}

/// The bank bits (13 and up) of a cart ROM of `size` bytes.
const fn bank_mask(size: usize) -> u32 {
    (size as u32 - 1) & !0x1FFF
}

/// `rom_mode(14 downto 13)`: which of address bits 14 and 13 come from the bus instead of the bank register.
const ROM_8K: u8 = 0b00;
const ROM_16K: u8 = 0b01;
const ROM_32K: u8 = 0b11;

/// Retro Replay serving per `mode_bits` (all_carts_v5.vhd:128-129, "11011111" and "10101111", index 0 to 7).
const RR_SERVE_ROM: [bool; 8] = [true, true, false, true, true, true, true, true];
const RR_SERVE_IO: [bool; 8] = [true, false, true, false, true, true, true, true];

/// Epyx FastLoad capacitor: EXROM goes high 512 phi2 ticks after the last IO1/ROML access (slot_slave.vhd:117-131).
const EPYX_CYCLES: u64 = 512;

/// Guest DDR lent by `C64Port` for one access.
#[derive(Clone, Copy)]
struct Ddr {
    ptr: *mut u8,
    len: usize,
}

/// freezer.vhd states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freeze {
    Idle,
    /// Button pushed: the cart pulls NMI (and IRQ) and waits for the 6510's interrupt pushes.
    Triggered,
    /// `freeze_act`: the cart is switched in for its freezer code.
    Active,
    /// Unfrozen; waiting for the button to be released.
    Button,
}

/// What `CartMapper::read`/`write` resolve to (all_carts_v5.vhd `addr_map`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Map {
    Rom,
    Ram,
    Geo,
}

/// The combinational outputs of the `case cart_logic_d` statement for one bus address.
#[derive(Clone, Copy)]
struct Out {
    game_n: bool,
    exrom_n: bool,
    rom: bool,
    io1: bool,
    io2: bool,
    irq_n: bool,
    nmi_n: bool,
    rom_mode: u8,
}

/// How the backend has to run the 6510 for this cart (see [`CartLogic::run_hints`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunHints {
    /// Reads of `$DE00-$DFFF` can change EXROM/GAME: stop at the instruction boundary after one that did.
    pub watch_io: bool,
    /// The lines change by themselves at this C64 cycle (Epyx timeout).
    pub deadline: Option<u64>,
    /// The freeze button is pushed: step instruction by instruction until the 6510 takes the interrupt.
    pub freeze_pending: bool,
}

/// The state of `all_carts_v5` plus its EEPROM and freezer.
pub struct CartLogic {
    /// `cart_logic_d` and `variant`, taken at reset or force.
    logic: u8,
    variant: u8,
    mode: u8,
    /// `bank_bits(21 downto 13)` in place (bit 13 = 0x2000).
    bank: u32,
    /// `ram_bank(15 downto 13)` in place.
    ram_bank: u32,
    georam_bank: u32,
    ef_write: bool,
    cart_en: bool,
    do_io2: bool,
    allow_bank: bool,
    hold_nmi: bool,
    eeprom: Eeprom,
    freeze: Freeze,
    button: bool,
    /// C64 cycle of the last access that discharged the Epyx capacitor.
    epyx_armed: u64,
    /// C64 cycle of the last access (TRX64's `clk`).
    clk: u64,
    ddr: Option<Ddr>,
    /// Where ROM, cart RAM and GeoRAM are in the lent memory.
    layout: Layout,
}

impl Default for CartLogic {
    fn default() -> Self {
        Self::new()
    }
}

impl CartLogic {
    /// No cartridge (type 0), serving from guest DDR.
    pub fn new() -> Self {
        Self::with_layout(Layout::GUEST)
    }

    /// The internal cartridge's ROM moves where this firmware keeps it; cart RAM and GeoRAM stay.
    pub fn set_cart_rom(&mut self, rom: CartRom) {
        self.layout = Layout::guest(rom);
    }

    /// No cartridge (type 0), with ROM, cart RAM and GeoRAM at `layout` in the memory given to `set_ddr`.
    pub fn with_layout(layout: Layout) -> Self {
        CartLogic {
            layout,
            logic: CART_TYPE_NONE,
            variant: 0,
            mode: 0,
            bank: 0,
            ram_bank: 0,
            georam_bank: 0,
            ef_write: false,
            cart_en: false,
            do_io2: true,
            allow_bank: false,
            hold_nmi: false,
            eeprom: Eeprom::new(),
            freeze: Freeze::Idle,
            button: false,
            epyx_armed: 0,
            clk: 0,
            ddr: None,
        }
    }

    /// Guest DDR for the accesses that follow (`None`: taken back). Without it ROM and RAM windows read as not served.
    pub fn set_ddr(&mut self, ddr: Option<&mut [u8]>) {
        self.ddr = ddr.map(|d| Ddr { ptr: d.as_mut_ptr(), len: d.len() });
    }

    /// A logic other than none is selected (it may still be disabled).
    pub fn present(&self) -> bool {
        self.logic != CART_TYPE_NONE
    }

    /// C64_CARTRIDGE_TYPE taken by the reset line (`reset`, `cart_en` on) or by the C64_CARTRIDGE_KILL bit 1 force
    /// (`cart_en` off until a freeze; all_carts_v5.vhd:182-197). `clk` is the C64 cycle.
    pub fn configure(&mut self, type_variant: u8, reset: bool, clk: u64) {
        self.logic = type_variant & TYPE_MASK;
        self.variant = type_variant >> 5;
        self.mode = 0;
        self.bank = if self.logic == BLACKBOX_V9 { 0x4000 } else { 0 };
        self.ram_bank = 0;
        self.georam_bank = 0;
        self.ef_write = false;
        self.allow_bank = false;
        self.do_io2 = true;
        self.cart_en = reset;
        self.hold_nmi = false;
        self.eeprom.set_pins(false, false, false);
        self.clk = clk;
        self.epyx_armed = clk;
        self.settle();
    }

    /// The expansion-port reset TRX64 applies in its warm reset: the reset branch with the type already taken.
    pub fn reset_line(&mut self) {
        self.configure(self.logic | (self.variant << 5), true, 0);
    }

    /// C64_CARTRIDGE_KILL bit 0 (all_carts_v5.vhd:654-658). Types that drive `cart_en` every clock (Ocean) come back.
    pub fn kill(&mut self) {
        self.cart_en = false;
        self.hold_nmi = false;
        self.unfreeze();
        self.settle();
    }

    /// C64_CARTRIDGE_ACTIVE.
    pub fn active(&self) -> bool {
        self.cart_en
    }

    /// EXROM/GAME as the cartridge port sees them. Address-dependent lines (Business Basic's dynamic mode, the Atomic
    /// Power write trick) are taken for a read at $0000; writes that differ are handled in [`Self::bus_write`].
    pub fn lines(&self) -> CartLines {
        let o = self.out(0, false);
        CartLines { exrom: u8::from(!self.cart_en || o.exrom_n), game: u8::from(!self.cart_en || o.game_n) }
    }

    /// NMI asserted by the cartridge (`nmi_n`).
    pub fn nmi(&self) -> bool {
        !self.out(0, false).nmi_n
    }

    /// IRQ asserted by the cartridge (`irq_n`).
    pub fn irq(&self) -> bool {
        !self.out(0, false).irq_n
    }

    /// How the 6510 has to be run: see [`RunHints`].
    pub fn run_hints(&self) -> RunHints {
        let watch_io = self.cart_en && matches!(self.logic, WESTERMANN | SBASIC | BBASIC | BLACKBOX_V9 | KCS | FC)
            || self.logic == EPYX;
        let deadline = (self.logic == EPYX && !self.epyx_timed_out()).then_some(self.epyx_armed + EPYX_CYCLES);
        RunHints { watch_io, deadline, freeze_pending: self.freeze == Freeze::Triggered }
    }

    /// Advance the logic's notion of time (the backend, after running the 6510).
    pub fn set_clk(&mut self, clk: u64) {
        self.clk = clk;
    }

    /// The 6510 was held by DMA for `cycles`: the Epyx capacitor does not discharge meanwhile (slot_slave.vhd:126).
    pub fn hold_time(&mut self, cycles: u64) {
        if self.logic == EPYX {
            self.epyx_armed += cycles;
        }
    }

    /// The freeze button (MATRIX_KEYB[10]): a push arms a freezer cart (freezer.vhd `idle`).
    pub fn set_button(&mut self, down: bool) {
        if down && !self.button && self.freeze == Freeze::Idle && freezer(self.logic) {
            self.freeze = Freeze::Triggered;
        }
        self.button = down;
        self.settle();
    }

    /// The 6510 is taking the interrupt the trigger pulled: `freeze_act` rises (freezer.vhd `triggered`, which waits
    /// for the three stack pushes) and switches the cart in (all_carts_v5.vhd:174-179).
    pub fn enter_freeze(&mut self) {
        if self.freeze != Freeze::Triggered {
            return;
        }
        self.freeze = Freeze::Active;
        self.bank = 0;
        self.ram_bank = 0;
        self.mode = 0;
        self.cart_en = true;
        self.hold_nmi = true;
        self.settle();
    }

    pub fn eeprom_read(&self, off: u16) -> u8 {
        self.eeprom.io_read(off)
    }

    pub fn eeprom_write(&mut self, off: u16, val: u8) {
        self.eeprom.io_write(off, val);
    }

    /// A 6510 read in a ROM window or `$DE00-$DFFF`, with side effects.
    pub fn bus_read(&mut self, addr: u16, clk: u64) -> Option<u8> {
        self.clk = clk;
        if (0xDE00..0xE000).contains(&addr) {
            return self.io_read(addr);
        }
        let data = self.rom_data(addr);
        if (0x8000..0xA000).contains(&addr) {
            match self.logic {
                // A read of $8000-$8FFF selects bank 0 of ROMH, $9000-$9FFF bank 1 (all_carts_v5.vhd:432-435).
                ZAXXON => self.bank = (self.bank & !0x4000) | (u32::from((addr >> 12) & 1) << 14),
                EPYX => self.epyx_armed = clk,
                _ => {}
            }
        }
        data
    }

    /// Whether the logic writes its RAM in `$8000-$BFFF` by address alone: Action/Retro Replay, SS5 and Pagefox
    /// (all_carts_v5.vhd:688-756; S28).
    pub fn writes_ram_by_address(&self) -> bool {
        matches!(self.logic, ACTION | SS5 | PAGEFOX)
    }

    /// A write to `$8000-$BFFF` whatever the PLA maps there: the FPGA writes cart RAM where `allow_write` says so, with
    /// no look at the lines (slot_slave.vhd:183-199; S28).
    pub fn snoop_ram_write(&mut self, addr: u16, val: u8) {
        if let (Map::Ram, true) = self.map(addr) {
            let off = self.offset(addr, Map::Ram, 0);
            self.ddr_write(off, val);
        }
    }

    /// A 6510 write in a ROM window or `$DE00-$DFFF`. Returns whether the C64 RAM underneath stays unwritten: always
    /// for I/O (so TRX64 re-runs its PLA), for ROM windows only in ULTIMAX, where the PLA selects no RAM there.
    pub fn bus_write(&mut self, addr: u16, val: u8, clk: u64, forced_ultimax: bool) -> bool {
        self.clk = clk;
        if (0xDE00..0xE000).contains(&addr) {
            self.io_write(addr, val);
            return true;
        }
        let (map, allow_write) = self.map(addr);
        if allow_write {
            let off = self.offset(addr, map, self.out(addr, true).rom_mode);
            self.ddr_write(off, val);
        }
        // Atomic Power mode 110 pulls EXROM high for writes to $A000-$BFFF only (all_carts_v5.vhd:538-541).
        let nordic = self.logic == ACTION
            && self.variant & 2 != 0
            && self.mode & 7 == 0b110
            && self.freeze != Freeze::Active
            && self.cart_en
            && (0xA000..0xC000).contains(&addr);
        let l = self.lines();
        forced_ultimax || (l.exrom, l.game) == (1, 0) || nordic
    }

    /// What a read would return, without side effects.
    pub fn peek(&self, addr: u16) -> Option<u8> {
        if (0xDE00..0xE000).contains(&addr) {
            self.io_data(addr)
        } else {
            self.rom_data(addr)
        }
    }

    fn epyx_timed_out(&self) -> bool {
        self.clk >= self.epyx_armed + EPYX_CYCLES
    }

    fn unfreeze(&mut self) {
        if self.freeze == Freeze::Active {
            self.freeze = Freeze::Button;
        }
    }

    /// The assignments the VHDL makes on every clock from the registered state.
    fn settle(&mut self) {
        match self.logic {
            CART_TYPE_NONE => self.cart_en = false,
            OCEAN_8K => self.cart_en = self.mode & 1 == 0,
            SUPERGAMES if self.mode & 3 == 3 => self.cart_en = false,
            ACTION => self.ram_bank = if self.allow_bank { (self.bank >> 1) & 0xE000 } else { 0 },
            KCS if self.freeze == Freeze::Active => self.mode = 0b010,
            _ => {}
        }
        if !freezer(self.logic) {
            self.freeze = Freeze::Idle;
        }
        if self.freeze == Freeze::Button && !self.button {
            self.freeze = Freeze::Idle;
        }
    }

    /// The `case cart_logic_d` outputs for a bus cycle at `addr`.
    fn out(&self, addr: u16, write: bool) -> Out {
        let m = |bit: u8| (self.mode >> bit) & 1 != 0;
        let v = |bit: u8| (self.variant >> bit) & 1 != 0;
        let trig = self.freeze == Freeze::Triggered;
        let act = self.freeze == Freeze::Active;
        let mut o =
            Out { game_n: true, exrom_n: true, rom: false, io1: false, io2: false, irq_n: true, nmi_n: true, rom_mode: ROM_16K };
        match self.logic {
            CART_TYPE_NORMAL => {
                (o.game_n, o.exrom_n, o.rom) = (v(1), v(0), true);
            }
            EPYX => {
                (o.exrom_n, o.rom, o.io2) = (self.epyx_timed_out(), true, true);
            }
            WESTERMANN => {
                (o.game_n, o.exrom_n, o.rom) = (m(0), !v(1) && m(0), true);
            }
            SBASIC => {
                (o.game_n, o.exrom_n, o.rom) = (!m(0), false, true);
            }
            BBASIC => {
                if m(0) {
                    (o.game_n, o.exrom_n) = (false, false);
                } else if addr & 0x8000 != 0 && (addr >> 13) & 3 != 0b10 {
                    (o.game_n, o.exrom_n) = (false, true);
                }
                (o.rom, o.io1, o.rom_mode) = (true, true, ROM_32K);
            }
            BLACKBOX_V3 => {
                (o.exrom_n, o.rom) = (m(0), true);
            }
            OCEAN_8K | SYSTEM3 => {
                (o.exrom_n, o.rom, o.rom_mode) = (m(0), true, ROM_8K);
            }
            OCEAN_16K | ZAXXON => {
                (o.game_n, o.exrom_n, o.rom) = (false, false, true);
            }
            MEGABYTER => {
                (o.exrom_n, o.rom) = (m(1), true);
                if v(0) {
                    o.game_n = m(0);
                } else {
                    (o.game_n, o.rom_mode) = (!m(0), ROM_8K);
                }
            }
            SUPERGAMES => {
                (o.game_n, o.exrom_n, o.rom) = (m(0), m(0), true);
            }
            BLACKBOX_V8 => {
                (o.game_n, o.exrom_n, o.rom) = (m(1), m(0), true);
            }
            BLACKBOX_V9 => {
                (o.game_n, o.exrom_n, o.rom, o.io1) = (m(1), !m(0), true, true);
            }
            PAGEFOX => {
                (o.game_n, o.exrom_n, o.rom) = (m(2), m(2), true);
            }
            EASY_FLASH => {
                (o.game_n, o.exrom_n, o.rom, o.io2) = (!m(0) && m(2), !m(1), true, true);
            }
            FC3 => {
                (o.game_n, o.exrom_n) = if act { (false, true) } else { (m(0), m(1)) };
                (o.rom, o.io1, o.io2) = (true, true, true);
                o.nmi_n = !(trig || act || self.hold_nmi);
            }
            ACTION => {
                if act {
                    (o.game_n, o.exrom_n, o.rom) = (false, true, true);
                } else {
                    let i = usize::from(self.mode & 7);
                    (o.io1, o.io2, o.rom) = (RR_SERVE_IO[i], RR_SERVE_IO[i] && self.do_io2, RR_SERVE_ROM[i]);
                    if self.mode & 7 == 0b110 && v(1) {
                        (o.game_n, o.exrom_n) = (false, (0xA000..0xC000).contains(&addr) && write);
                    } else {
                        (o.game_n, o.exrom_n) = (!m(0), m(1));
                    }
                }
                (o.irq_n, o.nmi_n, o.rom_mode) = (!(trig || act), !(trig || act), ROM_8K);
            }
            SS5 => {
                (o.game_n, o.exrom_n, o.io1, o.rom) = (m(0), !m(1), self.cart_en, self.cart_en);
                (o.irq_n, o.nmi_n) = (!(trig || act), !(trig || act));
            }
            KCS => {
                (o.game_n, o.exrom_n, o.io1, o.io2, o.rom) = (m(0), m(1), true, true, true);
                o.nmi_n = !(trig || act);
            }
            FC => {
                (o.game_n, o.exrom_n) = if act { (false, true) } else { (m(0), m(0)) };
                (o.io1, o.io2, o.rom) = (true, true, true);
                o.nmi_n = !(trig || act);
            }
            GEORAM => o.io1 = true,
            _ => {}
        }
        o
    }

    /// `addr_map` and `allow_write` for `addr` (all_carts_v5.vhd:679-761).
    fn map(&self, addr: u16) -> (Map, bool) {
        let m = self.mode;
        let page = addr >> 8;
        match self.logic {
            ACTION if m & 4 != 0 => {
                let (mut map, mut write) = (if addr & 0x2000 == 0 { Map::Ram } else { Map::Rom }, false);
                write |= (0x8000..0xA000).contains(&addr);
                write |= page == 0xDE && (addr >> 1) & 0x7F != 0 && self.variant & 1 != 0;
                write |= page == 0xDF && self.do_io2;
                if m & 3 == 0b10 && self.variant & 2 != 0 {
                    if (0x8000..0xA000).contains(&addr) {
                        (map, write) = (Map::Rom, false);
                    } else if (0xA000..0xC000).contains(&addr) {
                        (map, write) = (Map::Ram, true);
                    }
                }
                (map, write)
            }
            EASY_FLASH if page == 0xDF => (Map::Ram, true),
            // EAPI's writes through $DE09 = $65 in ULTIMAX mode 101 (c64_crt.cc:420-433; eapi.tas).
            EASY_FLASH => (Map::Rom, self.ef_write && m == 0b101 && matches!(addr >> 13, 0b111 | 0b100)),
            SS5 if m & 3 == 0 && (0x8000..0xA000).contains(&addr) => (Map::Ram, true),
            KCS if page == 0xDF => (Map::Ram, true),
            GEORAM if page == 0xDE => (Map::Geo, true),
            PAGEFOX if m & 3 == 0b10 => (Map::Ram, addr >> 14 == 0b10),
            _ => (Map::Rom, false),
        }
    }

    /// The DDR offset of `addr` under `map` (all_carts_v5.vhd:664-676, 682-683, 763-778).
    fn offset(&self, addr: u16, map: Map, rom_mode: u8) -> usize {
        let a = u32::from(addr);
        match map {
            Map::Rom => {
                let mut off = (self.bank & self.layout.rom_bank_mask) | (a & 0x1FFF);
                if rom_mode & 1 != 0 {
                    off = (off & !0x2000) | (a & 0x2000);
                }
                if rom_mode & 2 != 0 {
                    off = (off & !0x4000) | (a & 0x4000);
                }
                self.layout.rom + off as usize
            }
            Map::Ram => {
                let mut ram_bank = self.ram_bank & 0xE000;
                if self.logic == PAGEFOX {
                    ram_bank = (ram_bank & !0x2000) | (a & 0x2000);
                }
                let mut off = ram_bank | (a & 0x1FFF);
                if self.logic == KCS {
                    off &= !0x80;
                }
                self.layout.ram + off as usize
            }
            Map::Geo => self.layout.geo + ((self.georam_bank as usize) << 8) + usize::from(addr & 0xFF),
        }
    }

    fn ddr_read(&self, off: usize) -> Option<u8> {
        let ddr = self.ddr?;
        // SAFETY: `ddr` is `IoCtx::ram`, lent by `C64Port` for the access in progress and taken back before the access
        // returns (`C64Backend::lend_ddr`); nothing else touches guest DDR while the C64 runs inside that access.
        (off < ddr.len).then(|| unsafe { *ddr.ptr.add(off) })
    }

    fn ddr_write(&mut self, off: usize, val: u8) {
        if let Some(ddr) = self.ddr.filter(|d| off < d.len) {
            // SAFETY: as in `ddr_read`.
            unsafe { *ddr.ptr.add(off) = val };
        }
    }

    /// ROML/ROMH: served while `cart_en` (slot_timing.vhd `serve_enable`) and the logic serves ROM.
    fn rom_data(&self, addr: u16) -> Option<u8> {
        let o = self.out(addr, false);
        if !(self.cart_en && o.rom) {
            return None;
        }
        let (map, _) = self.map(addr);
        self.ddr_read(self.offset(addr, map, o.rom_mode))
    }

    /// The 8 KiB this cartridge drives through the ULTIMAX ROMH window `$E000-$FFFF`, as a slice of the lent DDR —
    /// what the VIC-II fetches, not what the 6510 reads (TRX64 `CartMapper::vic_romh`, VICE vicii.c:842-875). Under
    /// ultimax the VIC's fetches at `$3000-$3FFF` of every bank come out of this window instead of RAM, which is the
    /// only place a MAX-machine cartridge can keep its charset: RAM exists at `$0000-$0FFF` alone.
    ///
    /// A slice and not a per-byte read because TRX64 builds one `VicMemView` per cycle and indexes it directly. The
    /// window is a single DDR run: `offset` mixes in address bits 13 and 14 only, and both are the same for every
    /// address in `$E000-$FFFF`, so the bytes are contiguous and their order is the bus's.
    pub(crate) fn vic_romh(&self) -> Option<&[u8]> {
        let o = self.out(0xE000, false);
        if !(self.cart_en && o.rom) {
            return None;
        }
        let (map, _) = self.map(0xE000);
        if map != Map::Rom {
            return None;
        }
        let ddr = self.ddr?;
        let off = self.offset(0xE000, map, o.rom_mode);
        // SAFETY: as in `ddr_read`, `ddr` is the DDR lent for the access in progress. The slice is handed to TRX64's
        // `VicMemView`, which is built and dropped inside one `Machine` cycle, so it never outlives the lend.
        (off + 0x2000 <= ddr.len).then(|| unsafe { std::slice::from_raw_parts(ddr.ptr.add(off), 0x2000) })
    }

    /// IO1/IO2 read data: a register (`slot_resp.reg_output`), served memory, or open bus (slot_slave.vhd:292-301).
    fn io_data(&self, addr: u16) -> Option<u8> {
        let io1 = addr & 0x100 == 0;
        match self.logic {
            ACTION if self.variant & 2 != 0 && addr & 0x1FE == 0 => {
                let b = |bit: u32| u8::from(self.bank >> bit & 1 != 0);
                return Some(b(16) << 7 | 0x40 | b(15) << 4 | b(14) << 3 | u8::from(self.allow_bank) << 1);
            }
            OCEAN_8K if self.variant & 3 == 2 && io1 => return Some(u8::from(self.eeprom.data_out()) << 7),
            _ => {}
        }
        let o = self.out(addr, false);
        if !(self.cart_en && if io1 { o.io1 } else { o.io2 }) {
            return None;
        }
        let (map, _) = self.map(addr);
        self.ddr_read(self.offset(addr, map, o.rom_mode))
    }

    fn io_read(&mut self, addr: u16) -> Option<u8> {
        let data = self.io_data(addr);
        let io = addr & 0x1FF;
        let io1 = io & 0x100 == 0;
        match self.logic {
            EPYX if io1 => self.epyx_armed = self.clk,
            WESTERMANN => {
                if !io1 {
                    self.mode |= 1;
                } else if self.variant & 1 != 0 {
                    self.mode &= !1;
                }
            }
            SBASIC if io1 => self.mode &= !1,
            BBASIC if io1 => self.mode |= 1,
            // The VHDL loads the data bus of the read; nothing drives IO1 for this type, so bank 0 (as VICE).
            SYSTEM3 if io1 => {
                self.bank = 0;
                self.mode &= !1;
            }
            BLACKBOX_V9 if io1 => {
                self.bank = (self.bank & !0x4000) | (u32::from((io >> 7) & 1) << 14);
                self.mode = (self.mode & !3) | (((io & 1) as u8) << 1) | u8::from(io & 0x40 == 0);
            }
            KCS if io1 => self.mode = 1 | (((io >> 1) & 1) as u8) << 1,
            KCS if io & 0x180 == 0x180 => self.unfreeze(),
            FC => {
                if io1 {
                    self.mode |= 1;
                } else {
                    self.mode &= !1;
                }
                self.unfreeze();
            }
            _ => {}
        }
        self.settle();
        data
    }

    fn io_write(&mut self, addr: u16, v: u8) {
        let (map, allow_write) = self.map(addr);
        if allow_write {
            let off = self.offset(addr, map, self.out(addr, true).rom_mode);
            self.ddr_write(off, v);
        }
        let io = addr & 0x1FF;
        let io1 = io & 0x100 == 0;
        match self.logic {
            CART_TYPE_NORMAL if io == 0x1FF && self.cart_en && v & 0xC0 == 0x40 => self.cart_en = false,
            SBASIC if io1 => self.mode |= 1,
            BBASIC if io1 => self.mode &= !1,
            BLACKBOX_V3 => {
                if io1 {
                    self.mode |= 1;
                } else {
                    self.mode &= !1;
                }
            }
            OCEAN_8K if io1 => {
                self.bank = u32::from(v & 0x3F) << 14;
                match self.variant & 3 {
                    1 => self.mode = (self.mode & !1) | (v >> 7),
                    2 => {
                        self.mode = (self.mode & !3) | ((v >> 7) << 1) | ((v >> 6) & 1);
                        self.eeprom.set_pins(v & 0x40 != 0, v & 0x20 != 0, v & 0x10 != 0);
                    }
                    _ => {}
                }
            }
            OCEAN_16K if io1 => self.bank = u32::from(v) << 14,
            SYSTEM3 if io1 => {
                self.bank = u32::from(v) << 14;
                self.mode &= !1;
            }
            MEGABYTER if io1 => {
                if io & 2 == 0 {
                    self.bank = u32::from(v) << 14;
                } else {
                    self.mode = (self.mode & 4) | (v & 3);
                }
            }
            SUPERGAMES if !io1 && self.mode & 2 == 0 => {
                self.bank = (self.bank & !0xC000) | (u32::from(v & 3) << 14);
                self.mode = (self.mode & 4) | ((v >> 2) & 3);
            }
            BLACKBOX_V8 if !io1 => {
                self.bank = (self.bank & !0xC000) | (u32::from((v >> 2) & 3) << 14);
                self.mode = (self.mode & 4) | (v & 3);
            }
            BLACKBOX_V9 if io1 => {
                self.bank = (self.bank & !0x4000) | (u32::from(!(io >> 7) & 1) << 14);
                self.mode = (self.mode & !3) | (((io & 1) as u8) << 1) | u8::from(io & 0x40 == 0);
            }
            PAGEFOX if io & 0x180 == 0x080 => {
                self.mode = (v >> 2) & 7;
                self.bank = (self.bank & !0xC000) | (u32::from((v >> 1) & 3) << 14);
                self.ram_bank = (self.ram_bank & !0x4000) | (u32::from((v >> 1) & 1) << 14);
            }
            EASY_FLASH if io1 && self.cart_en => {
                self.ef_write = false;
                match io & 0xF {
                    0x0 => self.bank = u32::from(v) << 14,
                    0x2 => self.mode = v & 7,
                    0x9 => self.ef_write = v == 0x65,
                    _ => {}
                }
            }
            FC3 if io == 0x1FF && self.cart_en => {
                self.bank = (self.bank & !0xC000) | (u32::from(v & 3) << 14);
                if self.variant & 1 != 0 {
                    self.bank = (self.bank & !0x3_0000) | (u32::from((v >> 2) & 3) << 16);
                }
                self.mode = (((v >> 4) & 1) << 1) | ((v >> 5) & 1);
                self.unfreeze();
                self.cart_en = v & 0x80 == 0;
                self.hold_nmi = v & 0x40 == 0;
            }
            ACTION if io1 && self.cart_en && (io & 0x1FE == 0 || self.variant & 1 == 0) => {
                if io & 1 == 0 || self.variant & 1 == 0 {
                    self.bank = (self.bank & !0x1_C000) | (u32::from(v >> 7) << 16) | (u32::from((v >> 3) & 3) << 14);
                    self.mode = (((v >> 5) & 1) << 2) | (v & 3);
                    if v & 0x40 != 0 {
                        self.unfreeze();
                    }
                    self.cart_en = v & 0x04 == 0;
                } else {
                    // Retro Replay's second register at $DE01.
                    if v & 0x40 != 0 {
                        self.do_io2 = false;
                    }
                    if v & 0x02 != 0 {
                        self.allow_bank = true;
                    }
                }
            }
            SS5 if io1 && self.cart_en => {
                self.bank = (self.bank & !0xC000) | (u32::from((v >> 4) & 1) << 15) | (u32::from((v >> 2) & 1) << 14);
                if self.variant & 1 != 0 {
                    self.bank = (self.bank & !0x1_0000) | (u32::from((v >> 5) & 1) << 16);
                }
                self.mode = (((v >> 3) & 1) << 2) | (((v >> 1) & 1) << 1) | (v & 1);
                if v & 1 == 0 {
                    self.unfreeze();
                }
                self.cart_en = v & 0x08 == 0;
            }
            KCS if io & 0x180 == 0x080 => self.mode = 0,
            KCS if io & 0x180 == 0x000 => {
                if self.mode == 0 {
                    self.mode = 0b110;
                } else if self.mode == 0b010 || self.mode == 0b111 {
                    self.mode = ((io >> 1) & 1) as u8;
                }
            }
            FC => {
                if io1 {
                    self.mode |= 1;
                } else {
                    self.mode &= !1;
                }
                self.unfreeze();
            }
            GEORAM if io & 0x180 == 0x180 => {
                if io & 1 == 0 {
                    self.georam_bank = (self.georam_bank & !0xC03F) | u32::from(v & 0x3F) | (u32::from(v >> 6) << 14);
                } else {
                    self.georam_bank = (self.georam_bank & !0x3FC0) | (u32::from(v) << 6);
                }
            }
            _ => {}
        }
        self.settle();
    }
}

/// Logic with a freezer (`freezer_ena`).
fn freezer(logic: u8) -> bool {
    matches!(logic, FC | FC3 | SS5 | ACTION | KCS)
}

/// [`CartLogic`] shared by the backend and the mapper TRX64 holds.
#[derive(Clone, Default)]
pub struct CartHandle(Arc<SharedCell>);

#[derive(Default)]
struct SharedCell(UnsafeCell<CartLogic>);

// SAFETY: the backend, TRX64's `Machine` and every copy of the handle live on the emulation thread; `Send` is only
// required because `CartMapper: Send`.
unsafe impl Send for SharedCell {}
unsafe impl Sync for SharedCell {}

impl CartHandle {
    /// Run `f` on the logic. Calls do not nest: the backend never holds one across a call into TRX64, and the mapper
    /// and observer hold one only inside a single bus access or callback.
    pub fn with<R>(&self, f: impl FnOnce(&mut CartLogic) -> R) -> R {
        // SAFETY: single thread, and no two borrows are live at once (see above).
        f(unsafe { &mut *self.0 .0.get() })
    }

    /// The logic behind the handle, for a borrow that has to outlive the call: `CartMapper::vic_romh` hands TRX64 a
    /// slice of the ROMH window, which `with` cannot express (its closure's argument has its own lifetime).
    pub fn view(&self) -> &CartLogic {
        // SAFETY: as `with`, and read-only: the caller holds no other borrow while the reference is live.
        unsafe { &*self.0 .0.get() }
    }
}

/// The cartridge in TRX64's slot: the shared internal logic and the physical cartridge of the expansion port
/// (CARTSLOT, slot.rs), under the firmware's forced ULTIMAX decode when `forced_ultimax`.
#[derive(Clone)]
pub struct CartProxy {
    cart: CartHandle,
    slot: SlotHandle,
    forced_ultimax: bool,
}

impl CartProxy {
    /// The internal logic alone, with an empty expansion port.
    #[cfg(test)]
    pub fn new(cart: CartHandle, forced_ultimax: bool) -> Self {
        Self::with_slot(cart, SlotHandle::default(), forced_ultimax)
    }

    /// The internal logic and the expansion port `slot`.
    pub fn with_slot(cart: CartHandle, slot: SlotHandle, forced_ultimax: bool) -> Self {
        CartProxy { cart, slot, forced_ultimax }
    }
}

/// S28: the port device that hands every write to `$8000-$BFFF` to the cartridge RAM, banked in or not. On the port
/// only while a cartridge that needs it is in (`Trx64Backend::install_cart`).
pub struct RamSnoop {
    cart: CartHandle,
    slot: SlotHandle,
    addrs: Vec<u16>,
}

impl RamSnoop {
    pub fn new(cart: CartHandle, slot: SlotHandle) -> Self {
        RamSnoop { cart, slot, addrs: (0x8000..0xC000).collect() }
    }
}

impl ExpansionDevice for RamSnoop {
    fn read(&mut self, _: Access, _: Option<u8>) -> Option<u8> {
        None
    }

    fn peek(&self, _: u16, _: Option<u8>) -> Option<u8> {
        None
    }

    fn write(&mut self, _: Access, _: u8) {}

    fn snoop_addresses(&self) -> &[u16] {
        &self.addrs
    }

    fn snoop_write(&mut self, a: Access, value: u8) {
        self.slot.with(|s| s.snoop_ram_write(&self.cart, a.addr, value));
    }
}

impl CartMapper for CartProxy {
    fn mapper_type(&self) -> MapperType {
        match self.get_lines() {
            CartLines { exrom: 0, game: 0 } => MapperType::Normal16k,
            CartLines { exrom: 1, game: 0 } => MapperType::Ultimax,
            _ => MapperType::Normal8k,
        }
    }

    /// The forced decode pulls GAME low with EXROM high whatever the carts drive (slot_server_v4.vhd:1087-1088); else
    /// the wired AND of both cartridges (slot.rs).
    fn get_lines(&self) -> CartLines {
        self.slot.with(|s| s.lines(&self.cart, self.forced_ultimax))
    }

    fn read(&mut self, address: u16, bank_info: &BankInfo, clk: u64) -> Option<u8> {
        self.slot.with(|s| s.read(&self.cart, address, bank_info, clk))
    }

    fn peek(&self, address: u16, bank_info: &BankInfo) -> Option<u8> {
        self.slot.with(|s| s.peek(&self.cart, address, bank_info))
    }

    fn write(&mut self, address: u16, value: u8, bank_info: &BankInfo, clk: u64) -> bool {
        let forced = self.forced_ultimax;
        self.slot.with(|s| s.write(&self.cart, address, value, bank_info, clk, forced))
    }

    fn reset(&mut self) {
        self.slot.with(|s| s.reset(&self.cart));
    }

    /// GMod4 in the expansion port holds ULTIMAX and resolves the windows itself (TRX64 cart.rs `fake_ultimax`).
    fn fake_ultimax(&self) -> bool {
        self.slot.with(|s| s.fake_ultimax())
    }

    /// The ULTIMAX ROMH window the VIC-II fetches `$3000-$3FFF` from, from whichever side of the bus serves it.
    /// TRX64 asks only under a real ultimax board, so the mode is not re-checked here.
    fn vic_romh(&self) -> Option<&[u8]> {
        self.slot.view().vic_romh(&self.cart)
    }

    fn active_bank(&self, _addr: u16) -> u16 {
        0
    }

    fn get_state(&self) -> CartState {
        CartState::default()
    }

    fn set_state(&mut self, _state: CartState) {}

    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DDR as large as the guest's, with bank `b` of the cart ROM filled with `0x40 + b` for ROML and `0x80 + b` for
    /// ROMH, and cart RAM zero.
    pub(crate) fn ddr() -> Vec<u8> {
        let mut ddr = vec![0u8; 0x0400_0000];
        for bank in 0..64 {
            let base = ROM_BASE + bank * 0x4000;
            ddr[base..base + 0x2000].fill(0x40 + bank as u8);
            ddr[base + 0x2000..base + 0x4000].fill(0x80 + bank as u8);
        }
        ddr
    }

    fn logic(type_variant: u8, ddr: &mut [u8]) -> CartLogic {
        let mut c = CartLogic::new();
        c.set_ddr(Some(ddr));
        c.configure(type_variant, true, 0);
        c
    }

    fn lines(c: &CartLogic) -> (u8, u8) {
        let l = c.lines();
        (l.exrom, l.game)
    }

    #[test]
    fn normal_variants_kill_and_force() {
        let mut ddr = ddr();
        let mut c = logic(0x41, &mut ddr);
        assert_eq!((lines(&c), c.bus_read(0x8000, 0), c.bus_read(0xA000, 0)), ((0, 1), Some(0x40), Some(0x80)));
        assert_eq!(lines(&logic(0x01, &mut ddr)), (0, 0), "16K");
        assert_eq!(lines(&logic(0xA1, &mut ddr)), (1, 0), "ULTIMAX serving the VIC");
        assert_eq!(lines(&logic(0x61, &mut ddr)), (1, 1), "off");
        c.bus_write(0xDFFF, 0xC0, 0, false);
        assert!(c.active(), "bits 7:6 must be 01");
        assert!(!c.bus_write(0x8000, 0x12, 0, false), "8K: the write lands in RAM");
        assert!(c.bus_write(0xDFFF, 0x40, 0, false));
        assert_eq!((c.active(), lines(&c), c.bus_read(0x8000, 0)), (false, (1, 1), None));
        c.reset_line();
        assert!(c.active(), "the reset line enables it again");
        c.configure(0x41, false, 0);
        assert!(!c.active(), "a force without reset leaves the cart off");
        c.configure(0x00, true, 0);
        assert!(!c.active() && !c.present(), "type 0");
        c.set_ddr(None);
        c.configure(0x41, true, 0);
        assert_eq!(c.bus_read(0x8000, 0), None, "no DDR lent");
    }

    #[test]
    fn ocean_magic_desk_and_gmod2_bank_in_8k() {
        let mut ddr = ddr();
        let mut c = logic(0x08, &mut ddr);
        assert!(c.bus_write(0xDE00, 0x85, 0, false));
        assert_eq!((c.bus_read(0x8123, 0), lines(&c), c.active()), (Some(0x45), (0, 1), true), "bank 5, bit 7 ignored");
        let mut md = logic(0x28, &mut ddr);
        md.bus_write(0xDE00, 0x03, 0, false);
        assert_eq!(md.bus_read(0x9FFF, 0), Some(0x43));
        md.bus_write(0xDE00, 0x80, 0, false);
        assert_eq!((lines(&md), md.active()), ((1, 1), false), "Magic Desk bit 7 switches it off");
        md.kill();
        md.bus_write(0xDE00, 0x01, 0, false);
        assert_eq!((md.active(), md.bus_read(0x8000, 0)), (true, Some(0x41)));
        let mut g = logic(0x48, &mut ddr);
        g.bus_write(0xDE00, 0x07, 0, false);
        assert_eq!(g.bus_read(0x8000, 0), Some(0x47));
        assert_eq!(g.bus_read(0xDE00, 0), Some(0x80), "EEPROM DO idles high in bit 7");
        g.bus_write(0xDE00, 0x40, 0, false);
        assert_eq!(lines(&g), (1, 1), "CS high disables the ROM");
    }

    /// Firmware before 3.15 (docs/status/carts.md, "Cartridge ROM in DDR"): the ROM is 1 MB at 0x00F00000 and the bank
    /// bits above it wrap, as 20 cart bits do.
    #[test]
    fn a_small_cart_rom_is_served_from_its_own_place() {
        let masks = (Layout::GUEST.rom_bank_mask, Layout::guest(CartRom::SMALL).rom_bank_mask);
        assert_eq!(masks, (0x003F_E000, 0x000F_E000));
        let mut ddr = vec![0u8; 0x0400_0000];
        for bank in 0..8 {
            let base = CartRom::SMALL.base + bank * 0x4000;
            ddr[base..base + 0x2000].fill(0x60 + bank as u8);
        }
        let mut c = CartLogic::new();
        c.set_cart_rom(CartRom::SMALL);
        c.set_ddr(Some(&mut ddr));
        c.configure(0x08, true, 0);
        c.bus_write(0xDE00, 0x05, 0, false);
        assert_eq!(c.bus_read(0x8123, 0), Some(0x65), "Ocean bank 5 at 0x00F00000");
        c.bank = 0x10_4000;
        assert_eq!(c.bus_read(0x8123, 0), Some(0x61), "bit 20 is beyond 20 cart bits");
    }

    #[test]
    fn easyflash_modes_ram_and_ultimax_writes_land_in_ddr() {
        let mut ddr = ddr();
        let mut c = logic(0x11, &mut ddr);
        assert_eq!((lines(&c), c.bus_read(0xE000, 0)), ((1, 0), Some(0x80)), "boots ULTIMAX, bank 0 ROMH at $E000");
        c.bus_write(0xDE00, 0x02, 0, false);
        c.bus_write(0xDE02, 0x07, 0, false);
        assert_eq!((lines(&c), c.bus_read(0x8000, 0), c.bus_read(0xA000, 0)), ((0, 0), Some(0x42), Some(0x82)));
        c.bus_write(0xDE02, 0x06, 0, false);
        assert_eq!(lines(&c), (0, 1), "8K");
        c.bus_write(0xDE02, 0x04, 0, false);
        assert_eq!(lines(&c), (1, 1), "off");
        assert!(c.bus_write(0xDF10, 0x5A, 0, false));
        assert_eq!((c.bus_read(0xDF10, 0), c.peek(0xDE00)), (Some(0x5A), None), "IO2 RAM; IO1 is write-only");
        c.bus_write(0xDE02, 0x05, 0, false);
        c.bus_write(0xDE09, 0x65, 0, false);
        assert!(c.bus_write(0x8123, 0xA5, 0, false), "ULTIMAX write: no C64 RAM");
        assert!(c.bus_write(0xF000, 0x3C, 0, false));
        c.bus_write(0xDE02, 0x07, 0, false);
        assert_eq!((c.bus_read(0x8123, 0), c.bus_read(0xB000, 0)), (Some(0xA5), Some(0x3C)), "written to bank 2");
        c.bus_write(0xDE02, 0x05, 0, false);
        c.bus_write(0x8124, 0x11, 0, false);
        c.set_ddr(None);
        assert_eq!(
            [ddr[ROM_BASE + 2 * 0x4000 + 0x123], ddr[ROM_BASE + 2 * 0x4000 + 0x3000], ddr[ROM_BASE + 2 * 0x4000 + 0x124]],
            [0xA5, 0x3C, 0x42],
            "a $DE02 write clears the write key"
        );
        assert_eq!(ddr[RAM_BASE + 0x1F10], 0x5A);
    }

    #[test]
    fn action_replay_retro_replay_and_fc3() {
        let mut ddr = ddr();
        let mut ar = logic(0x1B, &mut ddr);
        assert_eq!((lines(&ar), ar.bus_read(0x8000, 0)), ((0, 1), Some(0x40)), "8K bank 0");
        ar.bus_write(0xDE00, 0x08 | 0x20, 0, false);
        assert_eq!(ar.bus_read(0x8000, 0), Some(0), "RAM mode reads cart RAM");
        assert!(!ar.bus_write(0x8001, 0x77, 0, false), "8K RAM write also reaches C64 RAM");
        ar.bus_write(0xDE00, 0x08, 0, false);
        assert_eq!(ar.bus_read(0x8001, 0), Some(0x41), "bank 1 ROM again");
        ar.bus_write(0xDE00, 0x04, 0, false);
        assert!(!ar.active(), "bit 2 disables");
        let mut rr = logic(0x3B, &mut ddr);
        rr.bus_write(0xDE01, 0x02, 0, false);
        rr.bus_write(0xDE00, 0x98 | 0x20, 0, false);
        rr.bus_write(0x8000, 0x99, 0, false);
        assert_eq!(rr.bus_read(0x8000, 0), Some(0x99));
        rr.set_ddr(None);
        assert_eq!(ddr[RAM_BASE + (0b111 << 13)], 0x99, "RR allow_bank: RAM bank 7");
        let mut fc3 = logic(0x19, &mut ddr);
        assert_eq!((lines(&fc3), fc3.bus_read(0xA000, 0), fc3.nmi()), ((0, 0), Some(0x80), false));
        fc3.bus_write(0xDFFF, 0x03 | 0x40 | 0x10, 0, false);
        assert_eq!((lines(&fc3), fc3.bus_read(0x8000, 0)), ((1, 0), Some(0x43)), "EXROM high: ULTIMAX, bank 3");
        fc3.bus_write(0xDFFF, 0x00, 0, false);
        assert!(fc3.nmi(), "bit 6 low holds NMI");
        fc3.bus_write(0xDFFF, 0x80 | 0x40, 0, false);
        assert!(!fc3.active());
    }

    #[test]
    fn snooped_writes_reach_cart_ram_whatever_the_lines() {
        let mut ddr = ddr();
        let mut ar = logic(0x1B, &mut ddr);
        ar.snoop_ram_write(0x8010, 0x11);
        ar.bus_write(0xDE00, 0x20, 0, false);
        ar.snoop_ram_write(0x8011, 0x22);
        ar.snoop_ram_write(0xA011, 0x23);
        let mut ss5 = logic(0x1A, &mut ddr);
        ss5.bus_write(0xDE00, 0x01, 0, false);
        ss5.snoop_ram_write(0x8020, 0x33);
        ss5.bus_write(0xDE00, 0x00, 0, false);
        ss5.snoop_ram_write(0x8021, 0x44);
        let mut pagefox = logic(0x10, &mut ddr);
        pagefox.bus_write(0xDE80, 0x18, 0, false);
        assert_eq!(lines(&pagefox), (1, 1), "lines off, RAM still written");
        pagefox.snoop_ram_write(0xA030, 0x55);
        pagefox.snoop_ram_write(0xC030, 0x66);
        pagefox.set_ddr(None);
        assert_eq!([ddr[RAM_BASE + 0x10], ddr[RAM_BASE + 0x11], ddr[RAM_BASE + 0x11 + 0x2000]], [0, 0x22, 0]);
        assert_eq!([ddr[RAM_BASE + 0x20], ddr[RAM_BASE + 0x21]], [0, 0x44], "SS5 RAM in mode 00 only");
        assert_eq!([ddr[RAM_BASE + 0x2030], ddr[RAM_BASE + 0x0030]], [0x55, 0], "Pagefox $A000 half, not $C000");
    }

    #[test]
    fn freeze_button_arms_enters_and_unfreezes() {
        let mut ddr = ddr();
        let mut fc3 = logic(0x19, &mut ddr);
        fc3.bus_write(0xDFFF, 0x80 | 0x40, 0, false);
        fc3.set_button(true);
        assert!(fc3.nmi() && fc3.run_hints().freeze_pending);
        fc3.enter_freeze();
        assert_eq!((fc3.active(), lines(&fc3), fc3.bus_read(0xFFFA, 0)), (true, (1, 0), Some(0x80)));
        fc3.bus_write(0xDFFF, 0x40, 0, false);
        assert_eq!((lines(&fc3), fc3.nmi(), fc3.freeze), ((0, 0), false, Freeze::Button));
        fc3.set_button(false);
        assert_eq!(fc3.freeze, Freeze::Idle);
        let mut normal = logic(0x41, &mut ddr);
        normal.set_button(true);
        assert!(!normal.run_hints().freeze_pending, "no freezer");
    }

    #[test]
    fn read_triggered_and_timed_lines() {
        let mut ddr = ddr();
        let mut kcs = logic(0x1C, &mut ddr);
        assert!(kcs.run_hints().watch_io);
        assert_eq!(lines(&kcs), (0, 0));
        kcs.bus_read(0xDE02, 0);
        assert_eq!(lines(&kcs), (1, 1), "read with bit 1: off");
        kcs.bus_read(0xDE00, 0);
        assert_eq!(lines(&kcs), (0, 1), "8K");
        let mut epyx = logic(0x02, &mut ddr);
        assert_eq!((lines(&epyx), epyx.run_hints().deadline), ((0, 1), Some(EPYX_CYCLES)));
        epyx.bus_read(0x8000, 300);
        epyx.set_clk(811);
        assert_eq!(lines(&epyx), (0, 1), "ROML read discharged at 300");
        epyx.set_clk(812);
        assert_eq!((lines(&epyx), epyx.run_hints().deadline), ((1, 1), None));
        assert_eq!((lines(&epyx), epyx.bus_read(0xDF00, 900)), ((1, 1), Some(0x40)), "IO2 ROM stays visible");
        epyx.bus_read(0xDE00, 1000);
        assert_eq!(lines(&epyx), (0, 1), "IO1 read switches it on");
    }

    #[test]
    fn proxy_forced_ultimax_and_georam() {
        let mut ddr = ddr();
        let cart = CartHandle::default();
        cart.with(|c| {
            c.set_ddr(Some(&mut ddr));
            c.configure(0x41, true, 0);
        });
        let mut lens = CartProxy::new(cart.clone(), true);
        let info = BankInfo {
            cpu_port_direction: 0x2F,
            cpu_port_value: 0x37,
            basic_visible: true,
            kernal_visible: true,
            io_visible: true,
            char_visible: false,
            cartridge_attached: true,
            cartridge_exrom: None,
            cartridge_game: None,
            phi1: 0xFF,
        };
        assert_eq!((lens.get_lines().exrom, lens.get_lines().game, lens.mapper_type()), (1, 0, MapperType::Ultimax));
        assert!(lens.write(0x8000, 1, &info, 0), "forced ULTIMAX: no C64 RAM");
        assert!(lens.write(0xDFFF, 0x40, &info, 0));
        assert_eq!((cart.with(|c| c.active()), lens.read(0x8000, &info, 0)), (false, None));
        lens.reset();
        assert_eq!(CartProxy::new(cart.clone(), false).mapper_type(), MapperType::Normal8k);
        cart.with(|c| c.configure(0x1F, true, 0));
        assert!(lens.write(0xDFFE, 0x05, &info, 0) && lens.write(0xDFFF, 0x02, &info, 0));
        assert!(lens.write(0xDE10, 0xEE, &info, 0));
        assert_eq!(lens.read(0xDE10, &info, 0), Some(0xEE));
        cart.with(|c| c.set_ddr(None));
        assert_eq!(ddr[GEORAM_BASE + (0x85 << 8) + 0x10], 0xEE, "GeoRAM page 0x85");
    }
}
