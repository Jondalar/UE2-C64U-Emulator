//! CARTSLOT: a physical cartridge in the U64's expansion port, beside the internal cartridge logic, on TRX64's one
//! cartridge slot (docs/status/cart-slot.md).
//!
//! [`PhysicalCart`] is the cartridge, built from a CRT file and independent of the internal cartridge the firmware
//! loads into guest DDR. Three models serve it:
//! - `trx64`: TRX64's own mappers for the ROM families (Normal 8K/16K/ULTIMAX, Ocean, Magic Desk (16), GMod4);
//! - `trx64-flash`: the flash families (EasyFlash (XL), GMod2, MegaByter, C64MegaCart) as bridge-side boards whose
//!   register logic follows TRX64's mappers (TRX64 cart.rs `EasyFlashMapper`, `Gmod2Mapper`, `MegabyterMapper`,
//!   `C64MegaCartMapper`) on TRX64's chip models (flash040.rs, m93c86.rs). They exist so that the flash holds exactly
//!   the CRT's bytes (TRX64's EasyFlash swaps an `eapi` block for its own, cart.rs `with_geometry`; a physical board
//!   keeps its own), the command decode can be chosen ([`FlashDecode`]), and bank numbers wrap at the chip size as the
//!   address lines do;
//! - `u64-logic`: for CRT types TRX64 has no mapper for, the ported U64 cartridge logic ([`CartLogic`]) serving the CRT
//!   from the cartridge's own memory, laid out as the firmware's CRT loader lays it out.
//!
//! [`Slot`] joins both cartridges onto the bus. The U64-II top level that connects the expansion port to the C64 core
//! is closed, so the model follows the firmware that drives it and the open cartridge-side VHDL:
//! - C64_BUS_INTERNAL / C64_BUS_EXTERNAL (0x1018002B/2C) choose which side serves IO1 (bit 0), IO2 (bit 1), the ROM
//!   windows and EXROM/GAME (bit 2) and the interrupt lines (bit 3) (c64.cc:1509-1596). The firmware writes them from
//!   "Cartridge Preference" and U64_CART_DETECT at boot (u64_config.cc:911), in `init_cartridge` (c64.cc:1444-1478)
//!   and `start_cartridge` (c64.cc:1154-1224, all internal for a custom cartridge). The register value is taken at
//!   once; a real U64 differs here (docs/status/cart-slot.md, "Auto/External and DMA").
//! - C64_BUS_BRIDGE (0x1018002A) bit 0, "Writes" (c64.cc:62-63, 1598-1601), mirrors IO1/IO2 writes to the expansion
//!   port whatever the sharing says; the other bus modes change nothing a cartridge model sees.
//! - EXROM/GAME of the sides that serve ROM pull the PLA inputs low together (open collector, wired AND). C64_MODE bit 1
//!   forces GAME low and EXROM high whatever a cartridge drives (slot_server_v4.vhd:1083-1098).
//! - A read in a ROM window or in IO1/IO2 is answered by every side that serves it and drives the data bus. When both
//!   drive, the bytes are ANDed (a bus fight where a low bit wins; the firmware enables both only in the Manual "Both"
//!   sharing). When nothing drives, TRX64 reads open bus.
//! - A write reaches every side that serves it. A ROM-window write lands in C64 RAM unless a side consumes it or the
//!   lines are ULTIMAX (no RAM there).
//! - U64_CART_DETECT reads the physical cartridge's own lines, before sharing and the forced ULTIMAX (c64.cc:1514).
//! - The C64's RESET line reaches the physical cartridge when it is asserted and again at the release.
//! - S43, `--trx-io`: TRX64-Ultimate's `slot_port.v` stands between the port and the C64 instead ([`TrxPort`],
//!   docs/specs/S43-trx-resid8-drive-slot.md): the masks take effect only in reset or the pause, the port drives
//!   strobes only while armed, ROML/ROMH reach it in a write cycle only with ROM_WR_EN, and the internal cartridge has
//!   no mask: the port's answer replaces it where the port serves.
//! - TRX64's flash models finish erase steps at absolute C64 cycles (flash040.rs `catch_up_erase`) and TRX64's reset
//!   restarts its cycle counter (c64_6510core.rs:677), so the flash boards count cycles on an epoch that each reset
//!   release advances by the cycles before it.

use std::cell::UnsafeCell;
use std::collections::BTreeSet;
use std::sync::Arc;

use trx64_core::c64re_snapshot::{capture_cart_state, restore_cart_state, CartStateSnapshot};
use trx64_core::cart::{self as tcart, BankInfo, CartLines, CartMapper, CartState, FlashCartState, MapperType};
use trx64_core::flash040::{
    Flash040, Flash040SnapState, Flash040Type, FLASH040B, FLASH040B_XL, FLASH040_160, FLASH040_NORMAL, FLASH800_CB,
};
use trx64_core::m93c86::M93c86;
use ue2_core::c64host::{CartSlotInfo, CART_DETECT_NONE};

use crate::cart::{CartHandle, CartLogic, CartRegs, Layout, RunHints, RAM_SIZE, ROM_SIZE};
use crate::hwlog::{HwLog, Poke, Space};

/// C64_BUS_INTERNAL / C64_BUS_EXTERNAL bits (c64.cc:1539-1592).
pub const BUS_IO1: u8 = 0x01;
pub const BUS_IO2: u8 = 0x02;
pub const BUS_ROM: u8 = 0x04;
pub const BUS_IRQ: u8 = 0x08;
/// C64 core config offsets (u64.h:131-133; docs/hw/10-c64-machine.md, 0x1018002A-2C).
pub const CORE_BUS_BRIDGE: u8 = 0x2A;
pub const CORE_BUS_INTERNAL: u8 = 0x2B;
pub const CORE_BUS_EXTERNAL: u8 = 0x2C;
/// C64_BUS_BRIDGE bit 0: write mirroring (c64.cc:1598-1601).
const BRIDGE_WRITES: u8 = 0x01;
/// Before the firmware writes them (u64_config.cc:911, ahead of any C64 use) both sides serve everything, as a C64
/// with a cartridge plugged in. The top level's reset value is not known.
const BUS_ALL: u8 = BUS_IO1 | BUS_IO2 | BUS_ROM | BUS_IRQ;

const CRT_SIGNATURE: &[u8; 16] = b"C64 CARTRIDGE   ";
/// CHIP packet types: ROM, flash (VICE CRT format; c64_crt.cc:301).
const CHIP_ROM: u16 = 0;
const CHIP_FLASH: u16 = 2;
/// The GMod2 EEPROM chunk the firmware reads and writes (c64_crt.cc:256-280, 757-785): load $DE00, 2 K.
const EEPROM_LOAD: u16 = 0xDE00;
const EEPROM_SIZE: usize = 0x800;
const BANK_8K: usize = 0x2000;
const BANK_16K: usize = 0x4000;

/// Which flash command addresses a physical flash chip decodes (`--cart-slot FILE,flash-decode=11|15|both`).
///
/// The AM29F040B decodes 11 address bits (AA at $555, 55 at $2AA), the older AM29F040 15 (AA at $5555, 55 at $2AAA),
/// which is what TRX64 and VICE give GMod2 (flash040.rs `FLASH040_NORMAL`). The byte-mode MX29F800CB (MegaByter) and
/// M29F160FT (C64MegaCart) decode $AAA/$555 in 12 bits, or $AAAA/$5555 in 16. Every long command address also matches
/// the short decode, so `both` and `11` decode the same addresses; `15` is the strict long decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlashDecode {
    /// 11 bits (12 for the byte-mode chips).
    Short,
    /// 15 bits (16 for the byte-mode chips).
    Long,
    /// Short and long command addresses, the default.
    #[default]
    Both,
}

impl FlashDecode {
    /// `11`, `15` or `both`.
    pub fn parse(s: &str) -> Result<FlashDecode, String> {
        match s {
            "11" => Ok(FlashDecode::Short),
            "15" => Ok(FlashDecode::Long),
            "both" => Ok(FlashDecode::Both),
            other => Err(format!("flash-decode must be 11, 15 or both, not {other:?}")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FlashDecode::Short => "11",
            FlashDecode::Long => "15",
            FlashDecode::Both => "both",
        }
    }

    /// The chip row `t` with this command decode; timing and ids stay the chip's.
    fn apply(self, t: Flash040Type) -> Flash040Type {
        let byte_mode = t.magic1_addr & 0xFFF == 0xAAA;
        let (magic1, magic2, mask) = match (self, byte_mode) {
            (FlashDecode::Long, false) => (0x5555, 0x2AAA, 0x7FFF),
            (FlashDecode::Long, true) => (0xAAAA, 0x5555, 0xFFFF),
            (_, false) => (0x555, 0x2AA, 0x7FF),
            (_, true) => (0xAAA, 0x555, 0xFFF),
        };
        Flash040Type { magic1_addr: magic1, magic2_addr: magic2, magic1_mask: mask, magic2_mask: mask, ..t }
    }
}

/// One CHIP packet of the inserted CRT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chip {
    /// Offset of the packet's data in the CRT.
    data: usize,
    kind: u16,
    bank: u16,
    load: u16,
    size: usize,
}

impl Chip {
    fn is_rom(&self) -> bool {
        matches!(self.kind, CHIP_ROM | CHIP_FLASH) && self.load != EEPROM_LOAD && self.size > 0
    }
}

/// The CRT header fields this module uses and the CHIP packets (VICE CRT format; c64_crt.cc:200-300).
struct Crt {
    hw_type: u16,
    name: String,
    chips: Vec<Chip>,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn parse(crt: &[u8]) -> Result<Crt, String> {
    if crt.len() < 0x40 || &crt[..16] != CRT_SIGNATURE {
        return Err("not a CRT file (no \"C64 CARTRIDGE\" signature)".into());
    }
    let header = (be32(crt, 0x10) as usize).max(0x40);
    let raw_name = &crt[0x20..0x40];
    let end = raw_name.iter().position(|&b| b == 0).unwrap_or(raw_name.len());
    let name = String::from_utf8_lossy(&raw_name[..end]).trim().to_string();
    let mut chips = Vec::new();
    let mut at = header;
    while at + 0x10 <= crt.len() {
        if &crt[at..at + 4] != b"CHIP" {
            return Err(format!("no CHIP packet at offset {at:#x}"));
        }
        let size = usize::from(be16(crt, at + 14));
        let data = at + 0x10;
        if data + size > crt.len() {
            return Err(format!("the CHIP packet at offset {at:#x} runs past the end of the file"));
        }
        chips.push(Chip { data, kind: be16(crt, at + 8), bank: be16(crt, at + 10), load: be16(crt, at + 12), size });
        at += (be32(crt, at + 4) as usize).max(0x10 + size);
    }
    Ok(Crt { hw_type: be16(crt, 0x16), name, chips })
}

/// CRT hardware types TRX64's own mappers serve (TRX64 cart.rs `infer_mapper_type`). TRX64's numbering is used as it
/// is: 87 is GMod4 there, where the firmware has TwoMegabyter; 90 is EasyFlash 3 (TRX64 ef3.rs, its flash in the
/// chip's own layout).
fn trx64_rom_type(hw: u16) -> bool {
    matches!(hw, 0 | 5 | 19 | 85 | 87 | 90)
}

/// The flash families served by the boards below (TRX64's numbering: 88 is C64MegaCart, 232 EasyFlash XL).
fn flash_type(hw: u16) -> bool {
    matches!(hw, 32 | 60 | 86 | 88 | 232)
}

/// C64_CARTRIDGE_TYPE and name for the CRT types the U64 cartridge logic serves and TRX64 does not, as
/// `C64_CRT::configure_cart` picks them (c64_crt.cc:19-106, 466-651; c64.h:115-163). `rom_bytes` is the ROM read.
fn logic_type(hw: u16, rom_bytes: usize) -> Option<(u8, &'static str)> {
    let large = |t: u8| if rom_bytes > 65_536 { t | 0x20 } else { t };
    Some(match hw {
        1 => (0x1B, "Action Replay"),
        2 => (0x1C, "KCS Power Cartridge"),
        3 => (large(0x19), "Final Cartridge III"),
        4 => (0x05, "Simons Basic"),
        8 => (0x0B, "Super Games"),
        9 => (0x5B, "Atomic Power"),
        10 => (0x02, "Epyx Fastload"),
        11 => (0x44, "Westermann"),
        13 => (0x18, "Final Cartridge I"),
        15 => (0x0A, "C64 Game System"),
        18 => (0x0D, "Zaxxon"),
        20 => (large(0x1A), "Super Snapshot 5"),
        21 => (large(0x09), "COMAL 80"),
        36 => (0x3B, "Retro Replay"),
        53 => (0x10, "Pagefox"),
        54 => (0x06, "Kingsoft Business Basic"),
        64 => (0x0C, "Blackbox V8"),
        65 => (0x07, "Blackbox V3"),
        66 => (0x24, "Blackbox V4"),
        71 => (0x0E, "Blackbox V9"),
        _ => return None,
    })
}

fn family(t: MapperType) -> &'static str {
    match t {
        MapperType::Normal8k => "Normal 8K",
        MapperType::Normal16k => "Normal 16K",
        MapperType::Ultimax => "Ultimax",
        MapperType::MagicDesk => "Magic Desk",
        MapperType::MagicDesk16 => "Magic Desk 16",
        MapperType::Ocean => "Ocean type 1",
        MapperType::EasyFlash => "EasyFlash",
        MapperType::EasyFlashXl => "EasyFlash XL",
        MapperType::Gmod2 => "GMod2",
        MapperType::MegaByter => "MegaByter",
        MapperType::C64MegaCart => "C64MegaCart",
        MapperType::Gmod4 => "GMod4",
        MapperType::EasyFlash3 => "EasyFlash 3",
        MapperType::SelfConfig | MapperType::Unsupported => "unsupported",
    }
}

// ---- the CRT's windows, as TRX64's parse_crt reads them ------------------------------------------------------------

/// `banks` × 8 K of ROML and of ROMH from the CRT, absent banks 0xFF: chips at $8000 are ROML (their second 8 K ROMH),
/// at $A000 and $E000 ROMH; a short chip is padded with 0xFF and a later packet replaces an earlier one; a $A000 chip
/// wins over an $E000 chip of the same bank (TRX64 cart.rs `parse_crt`, `normalize_bank_data`, and the EasyFlash's
/// `romh_a000.or(romh_e000)`).
fn crt_windows(crt: &[u8], chips: &[Chip], banks: usize) -> (Vec<u8>, Vec<u8>) {
    let (mut lo, mut hi) = (vec![0xFF; banks * BANK_8K], vec![0xFF; banks * BANK_8K]);
    let put = |dst: &mut [u8], bank: u16, src: &[u8]| {
        let at = usize::from(bank) * BANK_8K;
        if at + BANK_8K <= dst.len() {
            let n = src.len().min(BANK_8K);
            dst[at..at + n].copy_from_slice(&src[..n]);
            dst[at + n..at + BANK_8K].fill(0xFF);
        }
    };
    let rom = |c: &Chip| &crt[c.data..c.data + c.size];
    for chip in chips.iter().filter(|c| c.is_rom() && c.load == 0xE000) {
        put(&mut hi, chip.bank, rom(chip));
    }
    for chip in chips.iter().filter(|c| c.is_rom()) {
        match chip.load {
            0x8000 => {
                put(&mut lo, chip.bank, rom(chip));
                if chip.size > BANK_8K {
                    put(&mut hi, chip.bank, &rom(chip)[BANK_8K..]);
                }
            }
            0xA000 => put(&mut hi, chip.bank, rom(chip)),
            _ => {}
        }
    }
    (lo, hi)
}

// ---- flash boards (register logic after TRX64 cart.rs, chips from TRX64 flash040.rs / m93c86.rs) -------------------

/// `resolveRelativeOffset`: the offset of `addr` in its 8 K window.
fn window_offset(addr: u16) -> u32 {
    u32::from(addr & 0x1FFF)
}

/// S38: a flash chip's command state for an anchor, without catching the erase alarm up (`CartMapper::get_state` is
/// `&self`); the anchor's capture has caught every erase up before (`PhysicalCart::log_image`).
fn flash_snap(f: &Flash040) -> Flash040SnapState {
    f.snapshot_state_ro()
}

/// The command state of the chips in a `CartState` (S38).
fn flash_state(lo: &Flash040, hi: Option<&Flash040>, eeprom: Option<&M93c86>, io_ram: &[u8]) -> Option<FlashCartState> {
    Some(FlashCartState {
        flash_lo: Some(flash_snap(lo)),
        flash_hi: hi.map(flash_snap),
        eeprom: eeprom.map(M93c86::snapshot_state),
        spi: None,
        easyflash_jumper: 0,
        easyflash_ram: io_ram.to_vec(),
        ef3: None,
    })
}

/// EasyFlash memory configuration per mode register bits 2:0 with the boot jumper open (TRX64 cart.rs
/// `EASYFLASH_MEMCONFIG`, easyflash.c): 0 = 8 K, 1 = 16 K, 2 = off, 3 = ULTIMAX.
const EASYFLASH_MODES: [u8; 8] = [3, 3, 1, 1, 2, 3, 0, 1];

/// EasyFlash (TRX64 cart.rs:1146-1471): bank register $DE00, mode register $DE02, 256 bytes of RAM at $DF00, ROML and
/// ROMH on two AM29F040B, programmed in ULTIMAX at $8000 and $E000. No EAPI block is replaced.
#[derive(Clone)]
struct EasyFlash {
    bank: u8,
    register02: u8,
    io_ram: [u8; 256],
    lo: Flash040,
    hi: Flash040,
    bank_mask: u8,
    kind: MapperType,
}

impl EasyFlash {
    fn new(crt: &[u8], chips: &[Chip], highest: usize, xl: bool, decode: FlashDecode) -> EasyFlash {
        let (capacity, row, bank_mask, kind) = if xl {
            (256, FLASH040B_XL, 0xFF, MapperType::EasyFlashXl)
        } else {
            (64, FLASH040B, 0x3F, MapperType::EasyFlash)
        };
        let (lo, hi) = crt_windows(crt, chips, capacity.max(highest + 1));
        let row = decode.apply(row);
        let mut io_ram = [0u8; 256];
        // easyflash_powerup: FF 00 00 FF FF 00 00 FF ... (TRX64 cart.rs:1205-1209).
        for (i, slot) in io_ram.iter_mut().enumerate() {
            *slot = if ((i + 1) >> 1) & 1 != 0 { 0x00 } else { 0xFF };
        }
        EasyFlash {
            bank: 0,
            register02: 0,
            io_ram,
            lo: Flash040::new(lo, "easyflash-lo", row),
            hi: Flash040::new(hi, "easyflash-hi", row),
            bank_mask,
            kind,
        }
    }

    fn offset(&self, addr: u16) -> u32 {
        (u32::from(self.bank) << 13) | window_offset(addr)
    }

    fn mode(&self) -> u8 {
        EASYFLASH_MODES[usize::from(self.register02 & 7)]
    }
}

impl CartMapper for EasyFlash {
    fn mapper_type(&self) -> MapperType {
        self.kind
    }
    fn get_lines(&self) -> CartLines {
        match self.mode() {
            0 => CartLines { exrom: 0, game: 1 },
            1 => CartLines { exrom: 0, game: 0 },
            3 => CartLines { exrom: 1, game: 0 },
            _ => CartLines { exrom: 1, game: 1 },
        }
    }
    fn read(&mut self, addr: u16, _: &BankInfo, clk: u64) -> Option<u8> {
        let off = self.offset(addr);
        match addr {
            0xDF00..=0xDFFF => Some(self.io_ram[usize::from(addr & 0xFF)]),
            0x8000..=0x9FFF => Some(self.lo.read(off, clk)),
            0xA000..=0xBFFF | 0xE000..=0xFFFF => Some(self.hi.read(off, clk)),
            _ => None,
        }
    }
    /// The registers are write-only on the board: an IO1 read is open bus.
    fn peek(&self, addr: u16, _: &BankInfo) -> Option<u8> {
        let off = self.offset(addr);
        match addr {
            0xDF00..=0xDFFF => Some(self.io_ram[usize::from(addr & 0xFF)]),
            0x8000..=0x9FFF => Some(self.lo.peek(off)),
            0xA000..=0xBFFF | 0xE000..=0xFFFF => Some(self.hi.peek(off)),
            _ => None,
        }
    }
    fn write(&mut self, addr: u16, val: u8, _: &BankInfo, clk: u64) -> bool {
        match addr {
            0xDE00..=0xDEFF if addr & 2 != 0 => self.register02 = val & 0x87,
            0xDE00..=0xDEFF => self.bank = val & self.bank_mask,
            0xDF00..=0xDFFF => self.io_ram[usize::from(addr & 0xFF)] = val,
            0x8000..=0x9FFF if self.mode() == 3 => self.lo.store(self.offset(addr), val, clk),
            0xE000..=0xFFFF if self.mode() == 3 => self.hi.store(self.offset(addr), val, clk),
            _ => return false,
        }
        true
    }
    /// Mode register 0 is ULTIMAX with the jumper open, so $FFFC comes from the cartridge.
    fn reset(&mut self) {
        self.bank = 0;
        self.register02 = 0;
    }
    fn active_bank(&self, _: u16) -> u16 {
        u16::from(self.bank)
    }
    /// S38: the registers, the I/O RAM and both chips' command state.
    fn get_state(&self) -> CartState {
        CartState {
            current_bank: u16::from(self.bank),
            control_register: Some(self.register02),
            flash: flash_state(&self.lo, Some(&self.hi), None, &self.io_ram),
        }
    }
    fn set_state(&mut self, st: CartState) {
        self.bank = st.current_bank as u8 & self.bank_mask;
        self.register02 = st.control_register.unwrap_or(0) & 0x87;
        if let Some(f) = st.flash {
            if f.easyflash_ram.len() == self.io_ram.len() {
                self.io_ram.copy_from_slice(&f.easyflash_ram);
            }
            if let Some(s) = &f.flash_lo {
                self.lo.restore_state(s);
            }
            if let Some(s) = &f.flash_hi {
                self.hi.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.lo.is_dirty() || self.hi.is_dirty()
    }
    fn writable_generation(&self) -> u64 {
        self.lo.writable_generation() + self.hi.writable_generation()
    }
    fn persists_writable_state(&self) -> bool {
        true
    }
    /// ROML chip then ROMH chip, as TRX64's EasyFlash.
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some([self.lo.get_data(clk), self.hi.get_data(clk)].concat())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        let (lo, hi) = bytes.split_at(bytes.len().min(self.lo.data.len()));
        self.lo.load_data(lo);
        self.hi.load_data(hi);
    }
}

/// GMod2 (TRX64 cart.rs:1473-1664): $DE00 bits 5:0 bank, bits 7:6 mode (11 ULTIMAX, 0x 8 K, 10 off), bit 6 EEPROM CS,
/// bit 5 CLK, bit 4 DI, DO in bit 7 of an IO1 read; AM29F040 flash read at $8000 in 8 K mode, programmed in ULTIMAX.
#[derive(Clone)]
struct Gmod2 {
    bank: u16,
    /// 0 = 8 K, 1 = off, 2 = ULTIMAX.
    mode: u8,
    cs: u8,
    flash: Flash040,
    eeprom: M93c86,
}

impl Gmod2 {
    fn new(crt: &[u8], chips: &[Chip], highest: usize, decode: FlashDecode) -> Gmod2 {
        let (lo, _) = crt_windows(crt, chips, 64.max(highest + 1));
        let mut eeprom = M93c86::new();
        if let Some(chip) = chips.iter().find(|c| c.load == EEPROM_LOAD) {
            eeprom.load_data(&crt[chip.data..chip.data + chip.size.min(EEPROM_SIZE)]);
        }
        Gmod2 { bank: 0, mode: 0, cs: 0, flash: Flash040::new(lo, "gmod2", decode.apply(FLASH040_NORMAL)), eeprom }
    }

    fn offset(&self, addr: u16) -> u32 {
        window_offset(addr) + (u32::from(self.bank) << 13)
    }
}

impl CartMapper for Gmod2 {
    fn mapper_type(&self) -> MapperType {
        MapperType::Gmod2
    }
    fn get_lines(&self) -> CartLines {
        match self.mode {
            0 => CartLines { exrom: 0, game: 1 },
            2 => CartLines { exrom: 1, game: 0 },
            _ => CartLines { exrom: 1, game: 1 },
        }
    }
    fn read(&mut self, addr: u16, info: &BankInfo, clk: u64) -> Option<u8> {
        match addr {
            0xDE00..=0xDEFF if self.cs != 0 => Some(((self.eeprom.read_data() & 1) << 7) | (info.phi1 & 0x7F)),
            0xDE00..=0xDEFF => Some(info.phi1),
            0x8000..=0x9FFF if self.mode == 0 => Some(self.flash.read(self.offset(addr), clk)),
            _ => None,
        }
    }
    /// An IO1 peek would clock the EEPROM's output: open bus.
    fn peek(&self, addr: u16, _: &BankInfo) -> Option<u8> {
        match addr {
            0x8000..=0x9FFF if self.mode == 0 => Some(self.flash.peek(self.offset(addr))),
            _ => None,
        }
    }
    fn write(&mut self, addr: u16, val: u8, _: &BankInfo, clk: u64) -> bool {
        match addr {
            0xDE00..=0xDEFF => {
                self.bank = u16::from(val & 0x3F);
                self.mode = match val & 0xC0 {
                    0xC0 => 2,
                    0x00 | 0x80 => 0,
                    _ => 1,
                };
                self.cs = (val >> 6) & 1;
                self.eeprom.write_select(self.cs);
                if self.cs != 0 {
                    self.eeprom.write_data((val >> 4) & 1);
                    self.eeprom.write_clock((val >> 5) & 1);
                }
            }
            0x8000..=0x9FFF | 0xE000..=0xFFFF if self.mode == 2 => self.flash.store(self.offset(addr), val, clk),
            _ => return false,
        }
        true
    }
    fn reset(&mut self) {
        (self.bank, self.mode, self.cs) = (0, 0, 0);
        self.eeprom.write_select(0);
    }
    fn active_bank(&self, _: u16) -> u16 {
        self.bank
    }
    /// S38: the bank, the mode with the EEPROM's chip select in bit 2, and the chips' state.
    fn get_state(&self) -> CartState {
        CartState {
            current_bank: self.bank,
            control_register: Some(self.mode | (self.cs << 2)),
            flash: flash_state(&self.flash, None, Some(&self.eeprom), &[]),
        }
    }
    fn set_state(&mut self, st: CartState) {
        self.bank = st.current_bank & 0x3F;
        let c = st.control_register.unwrap_or(0);
        (self.mode, self.cs) = (c & 3, (c >> 2) & 1);
        if let Some(f) = st.flash {
            if let Some(s) = &f.flash_lo {
                self.flash.restore_state(s);
            }
            if let Some(s) = &f.eeprom {
                self.eeprom.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty() || self.eeprom.is_dirty()
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation() + self.eeprom.writable_generation()
    }
    fn persists_writable_state(&self) -> bool {
        true
    }
    /// Flash then the 2 K EEPROM, as TRX64's GMod2.
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some([self.flash.get_data(clk), self.eeprom.get_data()].concat())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        let (flash, eeprom) = bytes.split_at(bytes.len().min(self.flash.data.len()));
        self.flash.load_data(flash);
        if !eeprom.is_empty() {
            self.eeprom.load_data(eeprom);
        }
    }
}

/// MegaByter (TRX64 cart.rs:1666-1794): $DE00 (A1 clear) bank, $DE02 (A1 set) mode bits 1:0 (8 K, 16 K, off,
/// ULTIMAX) and LED bit 7; MX29F800CB at ROML, read in every mode, programmed in ULTIMAX.
#[derive(Clone)]
struct MegaByter {
    register00: u8,
    register02: u8,
    flash: Flash040,
}

impl MegaByter {
    fn new(crt: &[u8], chips: &[Chip], highest: usize, decode: FlashDecode) -> MegaByter {
        let (lo, _) = crt_windows(crt, chips, 128.max(highest + 1));
        MegaByter { register00: 0, register02: 0, flash: Flash040::new(lo, "megabyter", decode.apply(FLASH800_CB)) }
    }

    fn offset(&self, addr: u16) -> u32 {
        u32::from(self.register00) * 0x2000 + window_offset(addr)
    }
}

impl CartMapper for MegaByter {
    fn mapper_type(&self) -> MapperType {
        MapperType::MegaByter
    }
    fn get_lines(&self) -> CartLines {
        match self.register02 & 3 {
            0 => CartLines { exrom: 0, game: 1 },
            1 => CartLines { exrom: 0, game: 0 },
            2 => CartLines { exrom: 1, game: 1 },
            _ => CartLines { exrom: 1, game: 0 },
        }
    }
    fn read(&mut self, addr: u16, _: &BankInfo, clk: u64) -> Option<u8> {
        (0x8000..0xA000).contains(&addr).then(|| self.flash.read(self.offset(addr), clk))
    }
    fn peek(&self, addr: u16, _: &BankInfo) -> Option<u8> {
        (0x8000..0xA000).contains(&addr).then(|| self.flash.peek(self.offset(addr)))
    }
    fn write(&mut self, addr: u16, val: u8, _: &BankInfo, clk: u64) -> bool {
        match addr {
            0xDE00..=0xDEFF if addr & 2 != 0 => self.register02 = val & 0x83,
            0xDE00..=0xDEFF => self.register00 = val & 0x7F,
            0x8000..=0x9FFF if self.register02 & 3 == 3 => self.flash.store(self.offset(addr), val, clk),
            _ => return false,
        }
        true
    }
    fn reset(&mut self) {
        (self.register00, self.register02) = (0, 0);
    }
    fn active_bank(&self, _: u16) -> u16 {
        u16::from(self.register00)
    }
    /// S38: both registers and the chip's command state.
    fn get_state(&self) -> CartState {
        CartState {
            current_bank: u16::from(self.register00),
            control_register: Some(self.register02),
            flash: flash_state(&self.flash, None, None, &[]),
        }
    }
    fn set_state(&mut self, st: CartState) {
        self.register00 = st.current_bank as u8 & 0x7F;
        self.register02 = st.control_register.unwrap_or(0) & 0x83;
        if let Some(s) = st.flash.as_ref().and_then(|f| f.flash_lo.as_ref()) {
            self.flash.restore_state(s);
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty()
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation()
    }
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some(self.flash.get_data(clk).to_vec())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        self.flash.load_data(bytes);
    }
}

/// C64MegaCart (TRX64 cart.rs:1796-1956): $DE00 bank bits 7:0, $DF00 bank bits 13:8 and mode bits 7:6 (00 8 K, 80 off,
/// C0 ULTIMAX, 40 unchanged); M29F160FT read at $8000 and $E000, programmed in ULTIMAX at $E000. The bank wraps at the
/// chip size, as the unconnected address lines above it do (TRX64 reads 0xFF there).
#[derive(Clone)]
struct C64MegaCart {
    bank: u16,
    /// 0 = 8 K, 1 = off, 2 = ULTIMAX.
    mode: u8,
    bank_mask: u16,
    flash: Flash040,
}

impl C64MegaCart {
    fn new(crt: &[u8], chips: &[Chip], highest: usize, decode: FlashDecode) -> C64MegaCart {
        let banks = 256.max(highest + 1);
        let (lo, _) = crt_windows(crt, chips, banks);
        let bank_mask = (banks.next_power_of_two() - 1).min(0x3FFF) as u16;
        C64MegaCart { bank: 0, mode: 0, bank_mask, flash: Flash040::new(lo, "c64megacart", decode.apply(FLASH040_160)) }
    }

    fn offset(&self, addr: u16) -> u32 {
        (u32::from(self.bank & self.bank_mask) << 13) | window_offset(addr)
    }
}

impl CartMapper for C64MegaCart {
    fn mapper_type(&self) -> MapperType {
        MapperType::C64MegaCart
    }
    fn get_lines(&self) -> CartLines {
        match self.mode {
            0 => CartLines { exrom: 0, game: 1 },
            2 => CartLines { exrom: 1, game: 0 },
            _ => CartLines { exrom: 1, game: 1 },
        }
    }
    fn read(&mut self, addr: u16, _: &BankInfo, clk: u64) -> Option<u8> {
        matches!(addr, 0x8000..=0x9FFF | 0xE000..=0xFFFF).then(|| self.flash.read(self.offset(addr), clk))
    }
    fn peek(&self, addr: u16, _: &BankInfo) -> Option<u8> {
        matches!(addr, 0x8000..=0x9FFF | 0xE000..=0xFFFF).then(|| self.flash.peek(self.offset(addr)))
    }
    fn write(&mut self, addr: u16, val: u8, _: &BankInfo, clk: u64) -> bool {
        match addr {
            0xDE00..=0xDEFF => self.bank = (self.bank & 0xFF00) | u16::from(val),
            0xDF00..=0xDFFF => {
                self.bank = (self.bank & 0x00FF) | (u16::from(val & 0x3F) << 8);
                match val & 0xC0 {
                    0xC0 => self.mode = 2,
                    0x00 => self.mode = 0,
                    0x80 => self.mode = 1,
                    _ => {}
                }
            }
            0xE000..=0xFFFF if self.mode == 2 => self.flash.store(self.offset(addr), val, clk),
            _ => return false,
        }
        true
    }
    fn reset(&mut self) {
        (self.bank, self.mode) = (0, 0);
    }
    fn active_bank(&self, _: u16) -> u16 {
        self.bank & self.bank_mask
    }
    /// S38: the bank, the mode and the chip's command state.
    fn get_state(&self) -> CartState {
        CartState {
            current_bank: self.bank,
            control_register: Some(self.mode),
            flash: flash_state(&self.flash, None, None, &[]),
        }
    }
    fn set_state(&mut self, st: CartState) {
        self.bank = st.current_bank & 0x3FFF;
        self.mode = st.control_register.unwrap_or(0).min(2);
        if let Some(s) = st.flash.as_ref().and_then(|f| f.flash_lo.as_ref()) {
            self.flash.restore_state(s);
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty()
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation()
    }
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some(self.flash.get_data(clk).to_vec())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        self.flash.load_data(bytes);
    }
}

// ---- the U64 cart logic with its own memory --------------------------------------------------------------------

/// The ported cartridge logic with its own ROM and cart RAM ([`Layout::OWN`]).
struct LogicCart {
    logic: CartLogic,
    mem: Box<[u8]>,
}

impl LogicCart {
    fn new(type_variant: u8, mem: Box<[u8]>) -> Box<LogicCart> {
        let mut cart = Box::new(LogicCart { logic: CartLogic::with_layout(Layout::OWN), mem });
        let LogicCart { logic, mem } = &mut *cart;
        // The boxed slice never moves or reallocates and lives exactly as long as the logic next to it.
        logic.set_ddr(Some(&mut mem[..]));
        logic.configure(type_variant, true, 0);
        cart
    }
}

/// The ROM as `C64_CRT::read_crt` lays it out for the logic: 16 K banks, chips below 8 K mirrored to 8 K, the banks
/// mirrored up to 64, the per-type copies of `configure_cart`; cart RAM zero (c64_crt.cc:291-358, 473-520; c64.cc:437).
fn logic_memory(crt: &[u8], chips: &[Chip], hw: u16) -> Result<(Box<[u8]>, usize), String> {
    let mut mem = vec![0xFF; ROM_SIZE + RAM_SIZE].into_boxed_slice();
    mem[ROM_SIZE..].fill(0);
    let (mut total, mut highest) = (0, 0);
    for chip in chips.iter().filter(|c| c.is_rom()) {
        if chip.load == 0xC000 || chip.size == 0x8000 {
            return Err("C128 cartridges do not fit a C64 expansion port".into());
        }
        let off = usize::from(chip.bank) * BANK_16K + usize::from(chip.load & 0x2000);
        if off + chip.size > ROM_SIZE {
            return Err(format!("bank {} at ${:04X} lies beyond the 4 MB the cartridge logic addresses", chip.bank, chip.load));
        }
        mem[off..off + chip.size].copy_from_slice(&crt[chip.data..chip.data + chip.size]);
        total += chip.size;
        highest = highest.max(usize::from(chip.bank));
        let mut have = chip.size;
        while have < BANK_8K {
            let n = have.min(ROM_SIZE - (off + have));
            mem.copy_within(off..off + n, off + have);
            have <<= 1;
        }
    }
    let mut banks = 1;
    while banks <= highest {
        banks <<= 1;
    }
    while banks < 64 {
        mem.copy_within(0..banks * BANK_16K, banks * BANK_16K);
        banks <<= 1;
    }
    match hw {
        18 => mem.copy_within(0..0x2000, 0x4000),
        54 => {
            mem.copy_within(0x4000..0x6000, 0x2000);
            mem.copy_within(0x8000..0xA000, 0x6000);
        }
        _ => {}
    }
    Ok((mem, total))
}

enum Device {
    /// A TRX64 mapper or a flash board: a `CartMapper` that counts cycles on the cartridge's epoch.
    Mapper(Box<dyn CartMapper>),
    Logic(Box<LogicCart>),
}

/// A cartridge in the physical expansion port.
pub struct PhysicalCart {
    device: Device,
    name: String,
    hw_type: u16,
    family: String,
    model: &'static str,
    /// The flash command decode, for the flash boards.
    decode: Option<FlashDecode>,
    /// The CRT as inserted: the template for [`PhysicalCart::crt_image`].
    crt: Vec<u8>,
    chips: Vec<Chip>,
    banks: usize,
    /// Cycles before the last C64 reset release, added to TRX64's cycle counter for the mapper (module doc).
    epoch: u64,
    /// `--log cart` (S14): trace every access this cartridge sees.
    trace_on: bool,
    /// S38: which CRT this is (FNV-1a of the file), so an anchor's node is only put back into the cartridge it was
    /// taken from.
    print: u64,
    /// S38: the writable image as the write log last saw it, and the mutation counter it was read at.
    shadow: Option<Vec<u8>>,
    shadow_gen: u64,
    /// S38: images put back by a restore. `generation` counts them, so the write-back of `--cart-slot` sees the change.
    restores: u64,
}

/// FNV-1a, 64 bits.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// S38: a physical cartridge's node in an anchor's `ue2Hw` (docs/specs/S38-trx64-ring.md §4.1).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PhysNode {
    /// The CRT's hardware type and print: the node belongs to this cartridge only.
    hw: u16,
    print: u64,
    /// The cartridge's cycle epoch (module doc): the flash's erase alarm is on it.
    epoch: u64,
    /// A mapper's continuation (TRX64's `cartState` shape), or the U64 logic's registers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mapper: Option<CartStateSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logic: Option<CartRegs>,
}

impl PhysicalCart {
    /// Build the cartridge from a CRT file's bytes; `decode` applies to the flash families.
    pub fn from_crt(crt: &[u8], decode: FlashDecode) -> Result<PhysicalCart, String> {
        let parsed = parse(crt)?;
        let hw = parsed.hw_type;
        let highest = parsed.chips.iter().filter(|c| c.is_rom()).map(|c| usize::from(c.bank)).max();
        let top = highest.unwrap_or(0);
        let chips = &parsed.chips;
        let (device, family, model, banks, decode): (Device, String, &'static str, usize, Option<FlashDecode>) =
            if flash_type(hw) {
                let mapper: Box<dyn CartMapper> = match hw {
                    32 | 232 => Box::new(EasyFlash::new(crt, chips, top, hw == 232, decode)),
                    60 => Box::new(Gmod2::new(crt, chips, top, decode)),
                    86 => Box::new(MegaByter::new(crt, chips, top, decode)),
                    _ => Box::new(C64MegaCart::new(crt, chips, top, decode)),
                };
                let kind = mapper.mapper_type();
                let banks = match kind {
                    MapperType::EasyFlash | MapperType::Gmod2 => 64,
                    MapperType::MegaByter => 128,
                    _ => 256,
                }
                .max(top + 1);
                (Device::Mapper(mapper), family(kind).to_string(), "trx64-flash", banks, Some(decode))
            } else if trx64_rom_type(hw) {
                let (_, mapper) =
                    tcart::load_cartridge_from_bytes(crt, &parsed.name, None).map_err(|e| format!("TRX64: {e}"))?;
                let kind = mapper.mapper_type();
                let banks = match kind {
                    MapperType::Gmod4 => 1024,
                    MapperType::EasyFlash3 => usize::from(trx64_core::ef3::EF3_BANKS),
                    _ => highest.map_or(0, |b| b + 1),
                };
                (Device::Mapper(mapper), family(kind).to_string(), "trx64", banks, None)
            } else {
                let rom_bytes: usize = chips.iter().filter(|c| c.is_rom()).map(|c| c.size).sum();
                let Some((type_variant, name)) = logic_type(hw, rom_bytes) else {
                    return Err(format!(
                        "CRT hardware type {hw} has no physical cartridge model: TRX64 serves 0, 5, 19, 85, 87 and 90, \
                         the flash boards 32, 60, 86, 88 and 232, the U64 cartridge logic 1-4, 8-11, 13, 15, 18, 20, \
                         21, 36, 53, 54, 64-66 and 71"
                    ));
                };
                let (mem, _) = logic_memory(crt, chips, hw)?;
                let banks = highest.map_or(0, |b| b + 1);
                (Device::Logic(LogicCart::new(type_variant, mem)), name.to_string(), "u64-logic", banks, None)
            };
        Ok(PhysicalCart {
            device,
            name: parsed.name,
            hw_type: hw,
            family,
            model,
            decode,
            crt: crt.to_vec(),
            chips: parsed.chips,
            banks,
            epoch: 0,
            trace_on: false,
            print: fnv(crt),
            shadow: None,
            shadow_gen: 0,
            restores: 0,
        })
    }

    /// S38: the U64 logic's writes into the cartridge's own memory go to `log`.
    pub fn set_log(&mut self, log: &HwLog) {
        if let Device::Logic(c) = &mut self.device {
            c.logic.set_log(log.clone(), Space::PhysMem);
        }
    }

    /// S38: what changed in the writable image since the last look goes to `log`, byte by byte. TRX64's flash chips
    /// write their arrays inside the mapper (and an erase completes lazily, `catch_up_erase`), so the bytes are found
    /// by comparing with the image the log last saw, and only when the mutation counter moved. The first look takes
    /// the image as it is. `clk` is TRX64's cycle: a due erase completes first.
    pub fn log_image(&mut self, log: &HwLog, clk: u64) {
        let Device::Mapper(m) = &mut self.device else { return };
        let gen = m.writable_generation();
        if self.shadow.is_some() && gen == self.shadow_gen {
            return;
        }
        let Some(image) = m.writable_image(clk + self.epoch) else { return };
        self.shadow_gen = m.writable_generation();
        if let Some(old) = self.shadow.as_ref().filter(|o| o.len() == image.len()) {
            for (i, (&o, &n)) in old.iter().zip(&image).enumerate() {
                log.note(Space::PhysImage, i, o, n);
            }
        }
        self.shadow = Some(image);
    }

    /// S38: the cartridge's node for an anchor.
    pub fn node(&self) -> PhysNode {
        let (mapper, logic) = match &self.device {
            Device::Mapper(m) => (Some(capture_cart_state(m.as_ref())), None),
            Device::Logic(c) => (None, Some(c.logic.regs())),
        };
        PhysNode { hw: self.hw_type, print: self.print, epoch: self.epoch, mapper, logic }
    }

    /// S38: an anchor's node back, after the write log has put the memories back. Refused, and nothing changes, when
    /// it was taken from another cartridge.
    pub fn set_node(&mut self, n: &PhysNode) -> Result<(), String> {
        if (n.hw, n.print) != (self.hw_type, self.print) {
            return Err(format!("the anchor's expansion-port cartridge is not {}", self.name));
        }
        match (&mut self.device, &n.mapper, &n.logic) {
            (Device::Mapper(m), Some(snap), _) => restore_cart_state(m, snap),
            (Device::Logic(c), _, Some(regs)) => c.logic.set_regs(regs)?,
            _ => return Err("the anchor's expansion-port node does not fit the cartridge".into()),
        }
        self.epoch = n.epoch;
        Ok(())
    }

    /// S38: bytes the write log puts back, in its order: into the logic's own memory, or into the image, which then
    /// goes back into the chips in one piece.
    pub fn poke(&mut self, pokes: &[Poke]) {
        let mut image = false;
        for p in pokes {
            match (p.space, &mut self.device, &mut self.shadow) {
                (Space::PhysMem, Device::Logic(c), _) => {
                    if let Some(b) = c.mem.get_mut(p.addr) {
                        *b = p.value;
                    }
                }
                (Space::PhysImage, Device::Mapper(_), Some(shadow)) => {
                    if let Some(b) = shadow.get_mut(p.addr) {
                        *b = p.value;
                        image = true;
                    }
                }
                _ => {}
            }
        }
        if let (true, Device::Mapper(m), Some(shadow)) = (image, &mut self.device, &self.shadow) {
            m.set_writable_image(shadow);
            self.shadow_gen = m.writable_generation();
            self.restores += 1;
        }
    }

    /// EXROM/GAME as the cartridge drives them, 1 = released.
    pub fn lines(&self) -> CartLines {
        match &self.device {
            Device::Mapper(m) => {
                let l = m.get_lines();
                CartLines { exrom: l.exrom & 1, game: l.game & 1 }
            }
            Device::Logic(c) => c.logic.lines(),
        }
    }

    fn read(&mut self, addr: u16, info: &BankInfo, clk: u64) -> Option<u8> {
        let val = match &mut self.device {
            Device::Mapper(m) => m.read(addr, info, clk + self.epoch),
            Device::Logic(c) => c.logic.bus_read(addr, clk),
        };
        self.trace('R', addr, val, clk);
        val
    }

    /// S39, the lens unit's real cycle: a read of the cartridge at `addr` with its select forced, side effects and
    /// all.
    pub(crate) fn read_forced(&mut self, addr: u16, info: &BankInfo, clk: u64) -> Option<u8> {
        self.read(addr, info, clk)
    }

    fn peek(&self, addr: u16, info: &BankInfo) -> Option<u8> {
        match &self.device {
            Device::Mapper(m) => m.peek(addr, info),
            Device::Logic(c) => c.logic.peek(addr),
        }
    }

    fn write(&mut self, addr: u16, val: u8, info: &BankInfo, clk: u64) -> bool {
        let taken = match &mut self.device {
            Device::Mapper(m) => m.write(addr, val, info, clk + self.epoch),
            Device::Logic(c) => c.logic.bus_write(addr, val, clk, false),
        };
        self.trace('W', addr, Some(val), clk);
        taken
    }

    /// `--log cart`: one line per access to the cartridge in the expansion port.
    ///
    /// The clock is the point: the flash families schedule an erase off the cycle they are handed and finish it
    /// only when a later access carries a cycle at or after the due one (`Flash040::catch_up_erase`, VICE's lazy
    /// alarm). Nothing else in the emulator shows that clock, so a cartridge that seems stuck mid-erase can only
    /// be told apart here -- a chip that is genuinely busy from a chip whose time is not moving. `--` as the byte
    /// is a read the cartridge declined, which is not the same as a read that returned $FF.
    fn trace(&self, dir: char, addr: u16, val: Option<u8>, clk: u64) {
        if !self.trace_on {
            return;
        }
        let byte = val.map_or_else(|| "--".to_string(), |v| format!("{v:02x}"));
        eprintln!("cart {dir} ${addr:04x} {byte}  clk {clk} + epoch {} = {}", self.epoch, clk + self.epoch);
    }

    /// The expansion port's RESET line.
    pub fn reset(&mut self) {
        match &mut self.device {
            Device::Mapper(m) => m.reset(),
            Device::Logic(c) => c.logic.reset_line(),
        }
    }

    fn fake_ultimax(&self) -> bool {
        matches!(&self.device, Device::Mapper(m) if m.fake_ultimax())
    }

    /// The ULTIMAX ROMH window this cartridge drives, for the VIC's fetches (TRX64 cart.rs `vic_romh`).
    fn vic_romh(&self) -> Option<&[u8]> {
        match &self.device {
            Device::Mapper(m) => m.vic_romh(),
            Device::Logic(c) => c.logic.vic_romh(),
        }
    }

    fn interrupts(&self) -> (bool, bool) {
        match &self.device {
            Device::Mapper(_) => (false, false),
            Device::Logic(c) => (c.logic.nmi(), c.logic.irq()),
        }
    }

    fn run_hints(&mut self, clk: u64) -> RunHints {
        match &mut self.device {
            // EasyFlash 3 pulls RESET itself after a `$DE0F` write: its I/O accesses end the run (`Slot::cart_reset`).
            Device::Mapper(m) if m.mapper_type() == MapperType::EasyFlash3 => RunHints { watch_io: true, ..RunHints::default() },
            Device::Mapper(_) => RunHints::default(),
            Device::Logic(c) => {
                c.logic.set_clk(clk);
                c.logic.run_hints()
            }
        }
    }

    /// A reset this cartridge asked for (EasyFlash 3: `$DE0F` = 0 or 7, TRX64 ef3.rs), taken once.
    fn take_reset_request(&mut self) -> bool {
        match &mut self.device {
            Device::Mapper(m) => m.take_reset_request(),
            Device::Logic(_) => false,
        }
    }

    /// The RESET line pulled by the cartridge itself: EasyFlash 3 keeps its mode, slot and bank (TRX64
    /// `CartMapper::reset_generated`).
    fn reset_generated(&mut self) {
        match &mut self.device {
            Device::Mapper(m) => m.reset_generated(),
            Device::Logic(c) => c.logic.reset_line(),
        }
    }

    fn reset_vector_fetched(&mut self) {
        if let Device::Mapper(m) = &mut self.device {
            m.reset_vector_fetched();
        }
    }

    fn set_clk(&mut self, clk: u64) {
        if let Device::Logic(c) = &mut self.device {
            c.logic.set_clk(clk);
        }
    }

    fn hold_time(&mut self, cycles: u64) {
        if let Device::Logic(c) = &mut self.device {
            c.logic.hold_time(cycles);
        }
    }

    /// Flash or EEPROM changed since insertion.
    pub fn dirty(&self) -> bool {
        matches!(&self.device, Device::Mapper(m) if m.is_writable_dirty())
    }

    /// Mutation counter of flash and EEPROM, and of the images a restore put back (S38).
    pub fn generation(&self) -> u64 {
        match &self.device {
            Device::Mapper(m) => m.writable_generation() + self.restores,
            Device::Logic(_) => 0,
        }
    }

    /// Whether a CRT from [`Self::crt_image`] carries flash or EEPROM contents.
    pub fn writable(&self) -> bool {
        matches!(&self.device, Device::Mapper(m) if m.persists_writable_state())
    }

    #[cfg(test)]
    fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    fn hw_type(&self) -> u16 {
        self.hw_type
    }

    #[cfg(test)]
    fn family(&self) -> &str {
        &self.family
    }

    #[cfg(test)]
    fn model(&self) -> &'static str {
        self.model
    }

    #[cfg(test)]
    fn banks(&self) -> usize {
        self.banks
    }

    /// The cartridge as a CRT at C64 cycle `clk` (a pending flash erase completes first): the inserted header and chip
    /// packets with the data the cartridge holds now, then a packet for every flash bank the inserted CRT lacks that
    /// is not erased (ROML at $8000, EasyFlash ROMH at $A000, chip type 2) and the GMod2 EEPROM if the CRT had none.
    pub fn crt_image(&mut self, clk: u64) -> Vec<u8> {
        let mut out = self.crt.clone();
        let Device::Mapper(m) = &mut self.device else {
            return out; // the logic's ROM is read-only
        };
        let kind = m.mapper_type();
        if kind == MapperType::EasyFlash3 {
            // TRX64 re-packs the chip itself: a bank's ROML and ROMH halves lie 64 KB apart in it (ef3.rs).
            return m.crt_image(clk + self.epoch).unwrap_or(out);
        }
        let Some(image) = m.writable_image(clk + self.epoch) else {
            return out;
        };
        let (lo, hi, eeprom) = match kind {
            MapperType::EasyFlash | MapperType::EasyFlashXl => {
                let (lo, hi) = image.split_at(image.len() / 2);
                (lo, hi, &[][..])
            }
            MapperType::Gmod2 => {
                let (flash, eeprom) = image.split_at(image.len() - EEPROM_SIZE);
                (flash, &[][..], eeprom)
            }
            _ => (&image[..], &[][..], &[][..]),
        };
        let (mut have_lo, mut have_hi, mut have_eeprom) = (BTreeSet::new(), BTreeSet::new(), false);
        for chip in &self.chips {
            let dst = &mut out[chip.data..chip.data + chip.size];
            let bank = usize::from(chip.bank) * BANK_8K;
            if chip.load == EEPROM_LOAD {
                have_eeprom = true;
                copy_in(dst, eeprom, 0);
            } else if !chip.is_rom() {
            } else if chip.load == 0x8000 {
                have_lo.insert(chip.bank);
                let (first, second) = dst.split_at_mut(chip.size.min(BANK_8K));
                copy_in(first, lo, bank);
                if !second.is_empty() {
                    have_hi.insert(chip.bank);
                    copy_in(second, hi, bank);
                }
            } else if matches!(chip.load, 0xA000 | 0xE000) {
                have_hi.insert(chip.bank);
                copy_in(dst, hi, bank);
            }
        }
        for (flash, have, load) in [(lo, &have_lo, 0x8000u16), (hi, &have_hi, 0xA000)] {
            for (bank, data) in flash.chunks_exact(BANK_8K).enumerate() {
                let bank = bank as u16;
                if !have.contains(&bank) && data.iter().any(|&b| b != 0xFF) {
                    push_chip(&mut out, CHIP_FLASH, bank, load, data);
                }
            }
        }
        if !have_eeprom && eeprom.iter().any(|&b| b != 0xFF) {
            push_chip(&mut out, CHIP_ROM, 0, EEPROM_LOAD, eeprom);
        }
        out
    }
}

/// Fill `dst` from `src[at..]` where `src` has the bytes; the rest of `dst` keeps what it had.
fn copy_in(dst: &mut [u8], src: &[u8], at: usize) {
    if let Some(src) = src.get(at..) {
        let n = dst.len().min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
    }
}

fn push_chip(out: &mut Vec<u8>, kind: u16, bank: u16, load: u16, data: &[u8]) {
    out.extend_from_slice(b"CHIP");
    out.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
    for field in [kind, bank, load, data.len() as u16] {
        out.extend_from_slice(&field.to_be_bytes());
    }
    out.extend_from_slice(data);
}

/// Whether the side with sharing bits `bits` takes part in a bus access at `addr`.
fn serves(bits: u8, addr: u16) -> bool {
    match addr {
        0xDE00..=0xDEFF => bits & BUS_IO1 != 0,
        0xDF00..=0xDFFF => bits & BUS_IO2 != 0,
        _ => bits & BUS_ROM != 0,
    }
}

/// Two drivers on the data bus.
fn wired(a: Option<u8>, b: Option<u8>) -> Option<u8> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a & b),
        (a, None) => a,
        (None, b) => b,
    }
}

/// S43: TRX64-Ultimate's `slot_port.v` (designs/c64_basic, c64_core.v:598-660), what decides the port's share of the
/// bus. The fault guard has nothing to catch: no modelled cartridge pulls DMA and there are no transceivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrxPort {
    /// The masks in effect (`m_int`, `m_ext`): C64_BUS_INTERNAL/EXTERNAL as they stood the last time the CPU was in
    /// reset or the machine paused (`if (cpu_reset || halted)`).
    pub int: u8,
    pub ext: u8,
    /// The CPU is in reset or the machine is paused: the masks follow the registers.
    take: bool,
    /// `armed`: the port takes part in the C64's cycles; set from `present` at every cycle that runs, app DMA cycles
    /// in the pause included.
    pub armed: bool,
    /// SLOT_CTRL ROM_WR_EN (`roml_go`, `romh_go`).
    pub wr_en: bool,
    /// The app's Ultimax (C64_MODE b1): the port does not serve ROM (`rom_port`).
    ultimax: bool,
}

impl TrxPort {
    /// The block at configuration: INT 0xF, EXT 0x0 (ultimate_sys.vhd:453-454, slot_port.v `m_int`/`m_ext`).
    const POWER_ON: TrxPort = TrxPort { int: BUS_ALL, ext: 0, take: false, armed: false, wr_en: false, ultimax: false };

    /// `rom_sel`: EXT ROM and not INT ROM (Both = Internal).
    fn rom_sel(&self) -> bool {
        self.ext & BUS_ROM != 0 && self.int & BUS_ROM == 0
    }

    /// `rom_port`: GAME/EXROM and ROM reads from the port.
    fn rom_port(&self) -> bool {
        self.rom_sel() && !self.ultimax
    }

    /// `present`: a resource is routed to the port.
    fn present(&self) -> bool {
        self.rom_sel() || self.ext & (BUS_IO1 | BUS_IO2) != 0
    }

    /// The port's strobe for an access at `addr`: IO1/IO2 with their EXT bit, ROM with `rom_port`, and for a write
    /// cycle in a ROM window only with ROM_WR_EN. Only an armed port drives one.
    fn strobe(&self, addr: u16, write: bool) -> bool {
        self.armed
            && match addr {
                0xDE00..=0xDEFF => self.ext & BUS_IO1 != 0,
                0xDF00..=0xDFFF => self.ext & BUS_IO2 != 0,
                _ => self.rom_port() && (!write || self.wr_en),
            }
    }

    /// Whether the port's data answers a read at `addr` (`io1_rd`, `io2_rd`, `rom_port`); `crt`: a CRT is configured
    /// on the internal cartridge (`cart_int`).
    pub(crate) fn answers(&self, addr: u16, crt: bool) -> bool {
        let io = |bit: u8| self.armed && self.ext & bit != 0 && !(self.int & bit != 0 && crt);
        match addr {
            0xDE00..=0xDEFF => io(BUS_IO1),
            0xDF00..=0xDFFF => io(BUS_IO2),
            _ => self.rom_port(),
        }
    }
}

/// The expansion-port side of the bus: the physical cartridge and the sharing registers.
pub struct Slot {
    physical: Option<PhysicalCart>,
    bus_internal: u8,
    bus_external: u8,
    bus_bridge: u8,
    /// EXROM/GAME as TRX64's PLA last took them.
    shown: (u8, u8),
    /// The physical cartridge pulled the RESET line (EasyFlash 3): the bridge resets the C64 after the run.
    cart_reset: bool,
    /// S43: with `--trx-io`, TRX64-Ultimate's port logic.
    trx: Option<TrxPort>,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            physical: None,
            bus_internal: BUS_ALL,
            bus_external: BUS_ALL,
            bus_bridge: 0,
            shown: (1, 1),
            cart_reset: false,
            trx: None,
        }
    }
}

impl Slot {
    pub fn insert(&mut self, cart: PhysicalCart) {
        self.physical = Some(cart);
    }

    /// `--log cart`: trace the expansion port's cartridge.
    pub fn set_trace(&mut self, on: bool) {
        if let Some(p) = &mut self.physical {
            p.trace_on = on;
        }
    }

    pub fn eject(&mut self) -> Option<PhysicalCart> {
        self.physical.take()
    }

    pub fn physical(&self) -> Option<&PhysicalCart> {
        self.physical.as_ref()
    }

    pub fn physical_mut(&mut self) -> Option<&mut PhysicalCart> {
        self.physical.as_mut()
    }

    /// A C64 core config write; true when it changed the bus sharing.
    pub fn core_config(&mut self, off: u8, val: u8) -> bool {
        let (reg, val) = match off {
            CORE_BUS_BRIDGE => (&mut self.bus_bridge, val),
            CORE_BUS_INTERNAL => (&mut self.bus_internal, val & BUS_ALL),
            CORE_BUS_EXTERNAL => (&mut self.bus_external, val & BUS_ALL),
            _ => return false,
        };
        let changed = std::mem::replace(reg, val) != val;
        if self.trx.is_some() {
            return self.trx_take();
        }
        changed
    }

    /// S43: `--trx-io` on or off. On, the port starts as the block configures it, and with the bus registers' values
    /// once the CPU is in reset or the machine paused.
    pub fn set_trx(&mut self, on: bool) {
        self.trx = on.then_some(TrxPort::POWER_ON);
    }

    /// S43: the port logic, with `--trx-io`.
    pub fn trx(&self) -> Option<&TrxPort> {
        self.trx.as_ref()
    }

    /// S43: the CPU is in reset or the machine paused (`cpu_reset || halted`), or neither; while it stands the masks
    /// follow the registers. True when the masks in effect changed.
    pub fn set_trx_take(&mut self, take: bool) -> bool {
        match &mut self.trx {
            Some(t) => {
                t.take = take;
                self.trx_take()
            }
            None => false,
        }
    }

    /// The masks in effect take the registers while `take` stands; true when they changed.
    fn trx_take(&mut self) -> bool {
        let (int, ext) = (self.bus_internal, self.bus_external);
        let Some(t) = self.trx.as_mut().filter(|t| t.take) else { return false };
        let changed = (t.int, t.ext) != (int, ext);
        (t.int, t.ext) = (int, ext);
        changed
    }

    /// S43: a C64 cycle runs (the CPU's, also in reset, or an app DMA cycle in the pause): `armed` follows `present`.
    pub fn trx_cycle(&mut self) {
        if let Some(t) = &mut self.trx {
            t.armed = t.present();
        }
    }

    /// S43: SLOT_CTRL ROM_WR_EN.
    pub fn set_trx_wr_en(&mut self, on: bool) {
        if let Some(t) = &mut self.trx {
            t.wr_en = on;
        }
    }

    /// S43: the app's Ultimax (C64_MODE b1).
    pub fn set_trx_ultimax(&mut self, on: bool) {
        if let Some(t) = &mut self.trx {
            t.ultimax = on;
        }
    }

    /// S43: SLOT_STATUS (slot_port.v `status`) without b18 and b31, which the block adds; `reset_low`: the port's RESET
    /// line is low (our own reset with its stretch drives it). DMA, IRQ and NMI read high: no modelled cartridge drives
    /// them low on the pins. The SID sockets (b24-26) are not on the port in UE2.
    pub fn trx_status(&self, reset_low: bool) -> u32 {
        let Some(t) = &self.trx else { return 0 };
        let l = self.physical.as_ref().map_or(CartLines { exrom: 1, game: 1 }, PhysicalCart::lines);
        let (nmi, irq) = self.physical.as_ref().map_or((false, false), PhysicalCart::interrupts);
        u32::from(t.armed) << 16
            | u32::from(t.int) << 12
            | u32::from(t.ext) << 8
            | u32::from(reset_low) << 5
            | u32::from(!nmi) << 4
            | u32::from(!irq) << 3
            | 1 << 2
            | u32::from(l.exrom & 1) << 1
            | u32::from(l.game & 1)
    }

    /// EXROM/GAME at the PLA: the forced ULTIMAX decode, else the wired AND of the sides that serve the ROM windows.
    pub fn lines(&self, cart: &CartHandle, forced_ultimax: bool) -> CartLines {
        if forced_ultimax {
            return CartLines { exrom: 1, game: 0 };
        }
        // S43: the port's pins where it serves ROM, else the internal cartridge's (c64_core.v:598).
        if let Some(t) = &self.trx {
            return match &self.physical {
                Some(p) if t.rom_port() => p.lines(),
                None if t.rom_port() => CartLines { exrom: 1, game: 1 },
                _ => cart.with(|c| c.lines()),
            };
        }
        let mut lines = CartLines { exrom: 1, game: 1 };
        if self.bus_internal & BUS_ROM != 0 {
            let l = cart.with(|c| c.lines());
            (lines.exrom, lines.game) = (lines.exrom & l.exrom, lines.game & l.game);
        }
        if let Some(p) = self.physical.as_ref().filter(|_| self.bus_external & BUS_ROM != 0) {
            let l = p.lines();
            (lines.exrom, lines.game) = (lines.exrom & l.exrom, lines.game & l.game);
        }
        lines
    }

    /// Whether the lines differ from what TRX64's PLA last took; records them as taken.
    pub fn lines_changed(&mut self, cart: &CartHandle, forced_ultimax: bool) -> bool {
        let l = self.lines(cart, forced_ultimax);
        let changed = (l.exrom, l.game) != self.shown;
        self.shown = (l.exrom, l.game);
        changed
    }

    /// NMI and IRQ at the C64: each side's lines where the sharing routes them (bit 3).
    pub fn interrupts(&self, cart: &CartHandle) -> (bool, bool) {
        let (mut nmi, mut irq) = (false, false);
        // S43: the internal cartridge's always, the port's with EXT b3 (`irq_ext`).
        if let Some(t) = &self.trx {
            (nmi, irq) = cart.with(|c| (c.nmi(), c.irq()));
            if let Some(p) = self.physical.as_ref().filter(|_| t.ext & BUS_IRQ != 0) {
                let (n, i) = p.interrupts();
                (nmi, irq) = (nmi || n, irq || i);
            }
            return (nmi, irq);
        }
        if self.bus_internal & BUS_IRQ != 0 {
            (nmi, irq) = cart.with(|c| (c.nmi(), c.irq()));
        }
        if let Some(p) = self.physical.as_ref().filter(|_| self.bus_external & BUS_IRQ != 0) {
            let (n, i) = p.interrupts();
            (nmi, irq) = (nmi || n, irq || i);
        }
        (nmi, irq)
    }

    /// U64_CART_DETECT: bit 0 GAME, bit 1 EXROM of the physical cartridge.
    pub fn detect(&self) -> u8 {
        self.physical.as_ref().map_or(CART_DETECT_NONE, |p| {
            let l = p.lines();
            l.game | (l.exrom << 1)
        })
    }

    pub fn read(&mut self, cart: &CartHandle, addr: u16, info: &BankInfo, clk: u64) -> Option<u8> {
        // S43: the internal cartridge sees every access; the port's strobe goes out when it drives one, and its data
        // replaces the internal cartridge's where it answers (`port_rd`).
        if let Some(t) = self.trx {
            let internal = cart.with(|c| c.bus_read(addr, clk));
            let external = match self.physical.as_mut() {
                Some(p) if t.strobe(addr, false) => p.read(addr, info, clk),
                _ => None,
            };
            let crt = cart.with(|c| c.present());
            return if t.answers(addr, crt) { external } else { internal };
        }
        let internal = if serves(self.bus_internal, addr) { cart.with(|c| c.bus_read(addr, clk)) } else { None };
        let external = match self.physical.as_mut() {
            Some(p) if serves(self.bus_external, addr) => p.read(addr, info, clk),
            _ => None,
        };
        wired(internal, external)
    }

    pub fn peek(&self, cart: &CartHandle, addr: u16, info: &BankInfo) -> Option<u8> {
        if let Some(t) = self.trx {
            if t.answers(addr, cart.with(|c| c.present())) {
                return self.physical.as_ref().filter(|_| t.strobe(addr, false)).and_then(|p| p.peek(addr, info));
            }
            return cart.with(|c| c.peek(addr));
        }
        let internal = if serves(self.bus_internal, addr) { cart.with(|c| c.peek(addr)) } else { None };
        let external = match self.physical.as_ref() {
            Some(p) if serves(self.bus_external, addr) => p.peek(addr, info),
            _ => None,
        };
        wired(internal, external)
    }

    /// A write on the bus. Returns whether C64 RAM stays unwritten: always for `$DE00-$DFFF` (so TRX64 re-runs its PLA),
    /// for a ROM window when a side consumed it or the lines are ULTIMAX.
    pub fn write(&mut self, cart: &CartHandle, addr: u16, val: u8, info: &BankInfo, clk: u64, forced_ultimax: bool) -> bool {
        let io = (0xDE00..0xE000).contains(&addr);
        // S43: no mask on the internal cartridge; the port takes the write when it drives the strobe, and C64_BUS_BRIDGE
        // is Gideon's.
        let trx = self.trx;
        let internal = (trx.is_some() || serves(self.bus_internal, addr))
            && cart.with(|c| c.bus_write(addr, val, clk, forced_ultimax));
        let mirrored = trx.is_none() && io && self.bus_bridge & BRIDGE_WRITES != 0;
        let to_port = match trx {
            Some(t) => t.strobe(addr, true),
            None => serves(self.bus_external, addr) || mirrored,
        };
        let external = match self.physical.as_mut() {
            Some(p) if to_port => {
                let taken = p.write(addr, val, info, clk);
                // S43: a pulled RESET resets the C64 only while a resource is routed to the port (`reset_req`).
                let pulled = p.take_reset_request();
                self.cart_reset |= pulled && trx.is_none_or(|t| t.present());
                taken
            }
            _ => false,
        };
        if io {
            return true;
        }
        let l = self.lines(cart, forced_ultimax);
        internal || external || (l.exrom, l.game) == (1, 0)
    }

    /// Whether a cartridge on either side writes its RAM by address alone (S28).
    pub fn writes_ram_by_address(&self, cart: &CartHandle) -> bool {
        let logic = |p: &PhysicalCart| matches!(&p.device, Device::Logic(c) if c.logic.writes_ram_by_address());
        cart.with(|c| c.writes_ram_by_address()) || self.physical.as_ref().is_some_and(logic)
    }

    /// A write to `$8000-$BFFF` whatever the PLA maps, for each side that takes part in ROM-window accesses (S28).
    pub fn snoop_ram_write(&mut self, cart: &CartHandle, addr: u16, val: u8) {
        // S43: an armed port carries address and data of every cycle, whatever the strobes.
        let (int, ext) = match self.trx {
            Some(t) => (true, t.armed),
            None => (serves(self.bus_internal, addr), serves(self.bus_external, addr)),
        };
        if int {
            cart.with(|c| c.snoop_ram_write(addr, val));
        }
        if let Some(p) = self.physical.as_mut().filter(|_| ext) {
            if let Device::Logic(c) = &mut p.device {
                c.logic.snoop_ram_write(addr, val);
            }
        }
    }

    /// The C64's RESET line reaches both cartridges.
    pub fn reset(&mut self, cart: &CartHandle) {
        cart.with(CartLogic::reset_line);
        self.reset_physical();
    }

    /// The physical cartridge pulled the RESET line since the last call (EasyFlash 3).
    pub fn cart_reset_pending(&self) -> bool {
        self.cart_reset
    }

    pub fn take_cart_reset(&mut self) -> bool {
        std::mem::take(&mut self.cart_reset)
    }

    /// The RESET line pulled by the physical cartridge: the internal cartridge sees the line, the physical one knows
    /// it pulled it.
    pub fn reset_generated(&mut self, cart: &CartHandle) {
        cart.with(CartLogic::reset_line);
        if let Some(p) = &mut self.physical {
            p.reset_generated();
        }
    }

    /// TRX64 fetched the reset vector through an Ultimax ROMH (EasyFlash 3 lets GAME go here).
    pub fn reset_vector_fetched(&mut self) {
        if let Some(p) = &mut self.physical {
            p.reset_vector_fetched();
        }
    }

    /// The RESET line asserted: the physical cartridge resets while it is held.
    pub fn reset_physical(&mut self) {
        if let Some(p) = &mut self.physical {
            p.reset();
        }
    }

    /// TRX64's cycle counter restarts at a reset release after `clk` cycles: carry them into the cartridge's epoch.
    pub fn reset_release(&mut self, clk: u64) {
        if let Some(p) = &mut self.physical {
            p.epoch += clk;
        }
    }

    /// How the 6510 has to run for both cartridges at C64 cycle `clk`.
    pub fn run_hints(&mut self, cart: &CartHandle, clk: u64) -> RunHints {
        let mut hints = cart.with(|c| {
            c.set_clk(clk);
            c.run_hints()
        });
        if let Some(p) = &mut self.physical {
            let own = p.run_hints(clk);
            hints.watch_io |= own.watch_io;
            hints.deadline = match (hints.deadline, own.deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        hints
    }

    pub fn set_clk(&mut self, cart: &CartHandle, clk: u64) {
        cart.with(|c| c.set_clk(clk));
        if let Some(p) = &mut self.physical {
            p.set_clk(clk);
        }
    }

    /// The 6510 was held for `cycles` (Epyx capacitor, slot_slave.vhd:126).
    pub fn hold_time(&mut self, cart: &CartHandle, cycles: u64) {
        cart.with(|c| c.hold_time(cycles));
        if let Some(p) = &mut self.physical {
            p.hold_time(cycles);
        }
    }

    pub fn fake_ultimax(&self) -> bool {
        let rom = self.trx.map_or(self.bus_external & BUS_ROM != 0, |t| t.rom_port());
        self.physical.as_ref().is_some_and(|p| rom && p.fake_ultimax())
    }

    /// The ULTIMAX ROMH window the VIC fetches from, resolved the way `read` resolves a byte: the internal cartridge
    /// first, the expansion port's when the internal side does not serve `$E000` (S14; `CartMapper::vic_romh`).
    pub fn vic_romh<'a>(&'a self, cart: &'a CartHandle) -> Option<&'a [u8]> {
        // S43: the VIC's Ultimax fetch from the port needs it armed (`vu_now`), else the internal cartridge's.
        if let Some(t) = self.trx {
            if t.rom_port() {
                return self.physical.as_ref().filter(|_| t.armed).and_then(|p| p.vic_romh());
            }
            return cart.view().vic_romh();
        }
        if serves(self.bus_internal, 0xE000) {
            if let Some(window) = cart.view().vic_romh() {
                return Some(window);
            }
        }
        match self.physical.as_ref() {
            Some(p) if serves(self.bus_external, 0xE000) => p.vic_romh(),
            _ => None,
        }
    }

    /// The physical cartridge as a CRT at TRX64 cycle `clk`.
    pub fn crt_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        self.physical.as_mut().map(|p| p.crt_image(clk))
    }

    /// `cart-info` for the physical cartridge.
    pub fn info(&self) -> Option<CartSlotInfo> {
        let p = self.physical.as_ref()?;
        let l = p.lines();
        Some(CartSlotInfo {
            name: p.name.clone(),
            hw_type: p.hw_type,
            family: p.family.clone(),
            model: p.model.to_string(),
            banks: p.banks,
            exrom: l.exrom,
            game: l.game,
            bus_internal: self.bus_internal,
            bus_external: self.bus_external,
            bus_bridge: self.bus_bridge,
            dirty: p.dirty(),
            generation: p.generation(),
            writable: p.writable(),
            flash_decode: p.decode.map(|d| d.name().to_string()),
        })
    }
}

/// [`Slot`] shared by the backend and the mapper TRX64 holds, like `CartHandle`.
#[derive(Clone, Default)]
pub struct SlotHandle(Arc<SlotCell>);

#[derive(Default)]
struct SlotCell(UnsafeCell<Slot>);

// SAFETY: as `cart::SharedCell`: everything lives on the emulation thread; `Send` is only required because
// `CartMapper: Send`.
unsafe impl Send for SlotCell {}
unsafe impl Sync for SlotCell {}

impl SlotHandle {
    /// Run `f` on the slot. Calls do not nest (as `CartHandle::with`); `f` may use the internal `CartHandle`.
    /// The slot behind the handle, for a borrow that has to outlive the call (as `CartHandle::view`).
    pub fn view(&self) -> &Slot {
        // SAFETY: as `with`, and read-only.
        unsafe { &*self.0 .0.get() }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Slot) -> R) -> R {
        // SAFETY: single thread, no two borrows of the slot live at once.
        f(unsafe { &mut *self.0 .0.get() })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A CRT with `chips` of (bank, load, data, chip type).
    pub(crate) fn crt(hw: u16, exrom: u8, game: u8, chips: &[(u16, u16, Vec<u8>, u16)]) -> Vec<u8> {
        let mut out = b"C64 CARTRIDGE   ".to_vec();
        out.extend_from_slice(&0x40u32.to_be_bytes());
        out.extend_from_slice(&[0x01, 0x00]);
        out.extend_from_slice(&hw.to_be_bytes());
        out.extend_from_slice(&[exrom, game, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&b"SLOT TEST".iter().copied().chain(std::iter::repeat(0)).take(32).collect::<Vec<_>>());
        for (bank, load, data, kind) in chips {
            push_chip(&mut out, *kind, *bank, *load, data);
        }
        out
    }

    /// 8 K of `base + bank`, with the offset in the low bytes so every byte differs from its neighbours' banks.
    pub(crate) fn fill(bank: u16, base: u8) -> Vec<u8> {
        (0..BANK_8K).map(|i| if i < 0x100 { base.wrapping_add(bank as u8) } else { (i as u8) ^ base ^ bank as u8 }).collect()
    }

    pub(crate) fn easyflash(banks: u16) -> Vec<u8> {
        let chips: Vec<_> =
            (0..banks).flat_map(|b| [(b, 0x8000, fill(b, 0x10), CHIP_FLASH), (b, 0xA000, fill(b, 0x90), CHIP_FLASH)]).collect();
        crt(32, 1, 0, &chips)
    }

    pub(crate) fn info() -> BankInfo {
        BankInfo {
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
        }
    }

    fn lines(slot: &Slot, cart: &CartHandle) -> (u8, u8) {
        let l = slot.lines(cart, false);
        (l.exrom, l.game)
    }

    fn slot_with(crt: &[u8], decode: FlashDecode) -> Slot {
        let mut slot = Slot::default();
        slot.insert(PhysicalCart::from_crt(crt, decode).unwrap());
        slot
    }

    /// `Slot::write` without the forced ULTIMAX decode.
    trait Write {
        fn wr(&mut self, cart: &CartHandle, addr: u16, val: u8, info: &BankInfo, clk: u64) -> bool;
    }

    impl Write for Slot {
        fn wr(&mut self, cart: &CartHandle, addr: u16, val: u8, info: &BankInfo, clk: u64) -> bool {
            self.write(cart, addr, val, info, clk, false)
        }
    }

    #[test]
    fn crt_types_pick_their_model_and_bad_files_are_refused() {
        let ef = PhysicalCart::from_crt(&easyflash(4), FlashDecode::Both).unwrap();
        assert_eq!((ef.family(), ef.model(), ef.banks(), ef.hw_type(), ef.name()), ("EasyFlash", "trx64-flash", 64, 32, "SLOT TEST"));
        let ocean = PhysicalCart::from_crt(&crt(5, 0, 0, &[(0, 0x8000, fill(0, 1), 0), (1, 0x8000, fill(1, 1), 0)]), FlashDecode::Both).unwrap();
        assert_eq!((ocean.family(), ocean.model(), ocean.banks(), ocean.decode), ("Ocean type 1", "trx64", 2, None));
        let sg = PhysicalCart::from_crt(&crt(8, 0, 0, &[(0, 0x8000, [fill(0, 1), fill(0, 2)].concat(), 0)]), FlashDecode::Both).unwrap();
        assert_eq!((sg.family(), sg.model(), sg.banks()), ("Super Games", "u64-logic", 1));
        assert!(PhysicalCart::from_crt(&crt(6, 0, 0, &[(0, 0x8000, fill(0, 1), 0)]), FlashDecode::Both).err().unwrap().contains("type 6"));
        assert!(PhysicalCart::from_crt(b"not a cartridge", FlashDecode::Both).is_err());
        let mut short = easyflash(1);
        short.truncate(short.len() - 1);
        assert!(PhysicalCart::from_crt(&short, FlashDecode::Both).err().unwrap().contains("past the end"));
        assert_eq!(FlashDecode::parse("15"), Ok(FlashDecode::Long));
        assert!(FlashDecode::parse("16").is_err());
    }

    #[test]
    fn easyflash_registers_windows_and_cart_detect() {
        let cart = CartHandle::default();
        let mut slot = slot_with(&easyflash(4), FlashDecode::Both);
        let bi = info();
        assert_eq!((lines(&slot, &cart), slot.detect()), ((1, 0), 0x02), "boots ULTIMAX: GAME low");
        assert!(slot.wr(&cart, 0xDE00, 2, &bi, 0) && slot.wr(&cart, 0xDE02, 7, &bi, 0));
        assert_eq!((lines(&slot, &cart), slot.detect()), ((0, 0), 0x00), "16K");
        assert_eq!((slot.read(&cart, 0x8000, &bi, 0), slot.read(&cart, 0xA000, &bi, 0)), (Some(0x12), Some(0x92)));
        assert_eq!(slot.read(&cart, 0xDE00, &bi, 0), None, "IO1 is write-only: open bus");
        assert_eq!(slot.lines(&cart, true).game, 0, "forced ULTIMAX");
        assert_eq!(slot.detect(), 0x00, "CART_DETECT sees the cartridge's own lines");
        assert!(slot.wr(&cart, 0xDF10, 0x5A, &bi, 0));
        assert_eq!(slot.read(&cart, 0xDF10, &bi, 0), Some(0x5A), "IO2 RAM");
        slot.wr(&cart, 0xDE00, 64 + 3, &bi, 0);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x13), "the bank register keeps 6 bits");
        slot.reset(&cart);
        assert_eq!(lines(&slot, &cart), (1, 0), "RESET: ULTIMAX again");
    }

    #[test]
    fn bus_sharing_gates_each_side_and_both_sides_wire_and() {
        let mut ddr = crate::cart::tests::ddr();
        let cart = CartHandle::default();
        cart.with(|c| {
            c.set_ddr(Some(&mut ddr));
            c.configure(0x41, true, 0);
        });
        let mut slot = slot_with(&easyflash(4), FlashDecode::Both);
        let bi = info();
        slot.wr(&cart, 0xDE02, 7, &bi, 0);
        slot.wr(&cart, 0xDE00, 3, &bi, 0);
        assert_eq!(lines(&slot, &cart), (0, 0), "internal 8K AND external 16K");
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x40 & 0x13), "both drive ROML");
        assert!(slot.core_config(CORE_BUS_INTERNAL, BUS_IO1 | BUS_IO2 | BUS_IRQ));
        assert!(!slot.core_config(CORE_BUS_INTERNAL, BUS_IO1 | BUS_IO2 | BUS_IRQ), "unchanged");
        assert_eq!((lines(&slot, &cart), slot.read(&cart, 0x8000, &bi, 0)), ((0, 0), Some(0x13)), "internal ROM off");
        slot.core_config(CORE_BUS_INTERNAL, BUS_ALL);
        slot.core_config(CORE_BUS_EXTERNAL, 0);
        assert_eq!((lines(&slot, &cart), slot.read(&cart, 0x8000, &bi, 0)), ((0, 1), Some(0x40)), "external off");
        slot.wr(&cart, 0xDE00, 1, &bi, 0);
        slot.core_config(CORE_BUS_EXTERNAL, BUS_ROM);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x40 & 0x13), "the $DE00 write did not reach the EasyFlash");
        slot.core_config(CORE_BUS_BRIDGE, BRIDGE_WRITES);
        slot.wr(&cart, 0xDE00, 1, &bi, 0);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x40 & 0x11), "write mirroring reaches it");
        assert_eq!(slot.detect(), 0x00, "CART_DETECT ignores the sharing");
        assert!(!slot.wr(&cart, 0x8123, 0, &bi, 0), "a 16K ROML write lands in RAM");
        cart.with(|c| c.set_ddr(None));
    }

    fn trx_slot(crt: &[u8]) -> Slot {
        let mut slot = slot_with(crt, FlashDecode::Both);
        slot.set_trx(true);
        slot
    }

    /// The masks in effect C64_BUS_INTERNAL/EXTERNAL taken in reset or the pause, then a cycle that arms the port.
    fn trx_route(slot: &mut Slot, int: u8, ext: u8) {
        slot.set_trx_take(true);
        slot.core_config(CORE_BUS_INTERNAL, int);
        slot.core_config(CORE_BUS_EXTERNAL, ext);
        slot.set_trx_take(false);
        slot.trx_cycle();
    }

    /// S43 S6, S7, S8: SLOT_STATUS's bits; the masks take the registers only in reset or the pause, `armed` follows
    /// `present` at a cycle and stands in the pause until one comes.
    #[test]
    fn trx_masks_take_in_reset_or_pause_and_arm_at_a_cycle() {
        let mut slot = trx_slot(&easyflash(4));
        assert_eq!(slot.trx_status(false), 0x0000_F01E, "INT 0xF, EXT 0x0; the EasyFlash boots ULTIMAX: GAME low");
        assert!(!slot.core_config(CORE_BUS_EXTERNAL, 0x07) && !slot.core_config(CORE_BUS_INTERNAL, 0), "running: no take");
        assert_eq!(slot.trx_status(true), 0x0000_F03E, "b5: the port's RESET low");
        assert!(slot.set_trx_take(true), "reset: the masks take the registers");
        assert_eq!(slot.trx_status(false), 0x0000_071E, "not armed before a cycle");
        slot.trx_cycle();
        assert_eq!(slot.trx_status(false), 0x0001_071E, "armed");
        slot.set_trx_take(false);
        assert!(!slot.core_config(CORE_BUS_EXTERNAL, 0));
        assert_eq!(slot.trx_status(false) & 0x1_FF00, 0x1_0700, "running: unchanged");
        assert!(slot.set_trx_take(true), "the pause takes EXT 0");
        assert_eq!(slot.trx_status(false) & 0x1_FF00, 0x1_0000, "armed until a cycle comes");
        slot.trx_cycle();
        assert_eq!(slot.trx_status(false) & 0x1_0000, 0, "nothing routed: not armed");
        assert_eq!(Slot::default().trx_status(false), 0, "without --trx-io no status");
    }

    /// S43 S9: ROML/ROMH reach the port in a write cycle only with ROM_WR_EN; IO1 writes always.
    #[test]
    fn trx_rom_writes_need_rom_wr_en() {
        let cart = CartHandle::default();
        let mut slot = trx_slot(&easyflash(4));
        trx_route(&mut slot, 0, BUS_ROM | BUS_IO1 | BUS_IO2);
        let bi = info();
        let program = |slot: &mut Slot| {
            for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0xA0), (0x8010, 0x00)] {
                slot.wr(&cart, addr, val, &bi, 100);
            }
        };
        program(&mut slot);
        assert_eq!(slot.read(&cart, 0x8010, &bi, 1000), Some(0x10), "the flash never saw the sequence");
        slot.wr(&cart, 0xDE00, 1, &bi, 1000);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 1000), Some(0x11), "the IO1 bank register took it");
        slot.wr(&cart, 0xDE00, 0, &bi, 1000);
        slot.set_trx_wr_en(true);
        program(&mut slot);
        assert_eq!(slot.read(&cart, 0x8010, &bi, 1000), Some(0x00), "programmed");
        assert!(slot.physical().unwrap().dirty());
    }

    /// S43 S10: the internal cartridge has no mask; where the port serves, its pins and data replace the internal
    /// cartridge's (Both = Internal for ROM); IO strobes go out with EXT, IO data needs not (INT and a CRT).
    #[test]
    fn trx_port_beside_the_internal_cartridge() {
        let mut ddr = crate::cart::tests::ddr();
        let cart = CartHandle::default();
        cart.with(|c| {
            c.set_ddr(Some(&mut ddr));
            c.configure(0x41, true, 0);
        });
        let mut slot = trx_slot(&easyflash(4));
        let bi = info();
        trx_route(&mut slot, 0, BUS_ROM | BUS_IO1 | BUS_IO2);
        slot.wr(&cart, 0xDE02, 7, &bi, 0);
        slot.wr(&cart, 0xDE00, 3, &bi, 0);
        assert_eq!((lines(&slot, &cart), slot.read(&cart, 0x8000, &bi, 0)), ((0, 0), Some(0x13)), "the port's 16K");
        trx_route(&mut slot, 0, 0);
        assert_eq!((lines(&slot, &cart), slot.read(&cart, 0x8000, &bi, 0)), ((0, 1), Some(0x40)), "the internal 8K, unmasked");
        trx_route(&mut slot, BUS_ROM, BUS_ROM | BUS_IO2);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x40), "Both: internal");
        slot.wr(&cart, 0xDF10, 0x5A, &bi, 0);
        assert_eq!(slot.read(&cart, 0xDF10, &bi, 0), Some(0x5A), "IO2 from the port");
        assert_eq!(slot.lines(&cart, true).game, 0, "the app's Ultimax");
        trx_route(&mut slot, BUS_IO2, BUS_IO2);
        assert_eq!(slot.read(&cart, 0xDF10, &bi, 0), None, "INT IO2 and a CRT: the internal cartridge answers");
        trx_route(&mut slot, 0, BUS_IO2);
        assert_eq!(slot.read(&cart, 0xDF10, &bi, 0), Some(0x5A));
        trx_route(&mut slot, 0, BUS_ROM);
        slot.set_trx_ultimax(true);
        assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(0x40), "Ultimax: ROM stays internal");
        cart.with(|c| c.set_ddr(None));
    }

    /// S43: an EasyFlash 3's `$DE0F` reset counts only while a resource is routed to the port (slot_port.v `reset_req`).
    #[test]
    fn trx_cart_reset_needs_a_routed_port() {
        let cart = CartHandle::default();
        let crt = crt(90, 1, 0, &[(0, 0x8000, vec![0x11; 0x2000], 2), (0, 0xE000, vec![0xEA; 0x2000], 2)]);
        let mut slot = trx_slot(&crt);
        let bi = info();
        trx_route(&mut slot, 0, BUS_IO1);
        slot.wr(&cart, 0xDE0F, 0, &bi, 0);
        assert!(slot.take_cart_reset(), "routed: the C64 resets");
        trx_route(&mut slot, 0xF, 0);
        slot.trx = Some(TrxPort { armed: true, ..*slot.trx().unwrap() });
        slot.wr(&cart, 0xDE0F, 0, &bi, 0);
        assert!(!slot.take_cart_reset(), "nothing routed");
    }

    /// Sector erase and byte program through the bus, timed by the C64 cycle the accesses carry (TRX64 flash040.rs).
    #[test]
    fn easyflash_flash_erase_program_and_crt_image() {
        let cart = CartHandle::default();
        let inserted = easyflash(10);
        let mut slot = slot_with(&inserted, FlashDecode::Both);
        assert_eq!(slot.crt_image(0).unwrap(), inserted, "unchanged: the inserted CRT");
        let bi = info();
        let w = |slot: &mut Slot, addr: u16, val: u8, clk: u64| slot.wr(&cart, addr, val, &bi, clk);
        w(&mut slot, 0xDE02, 5, 0);
        w(&mut slot, 0xDE00, 8, 0);
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0x80), (0x8555, 0xAA), (0x82AA, 0x55), (0x8000, 0x30)] {
            assert!(w(&mut slot, addr, val, 100), "ULTIMAX: no RAM");
        }
        let a = slot.read(&cart, 0x8000, &bi, 200);
        let b = slot.read(&cart, 0x8000, &bi, 201);
        assert_ne!(a, b, "DQ6 toggles while the sector erases");
        assert_eq!(slot.read(&cart, 0x8000, &bi, 1_100_000), Some(0xFF), "erased once 50 + 1 000 000 cycles have passed");
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0xA0), (0x8010, 0x5A)] {
            w(&mut slot, addr, val, 1_100_010);
        }
        // ROMH bank 8 is not erased: a program only clears bits, so 0x88 into its 0x98.
        for (addr, val) in [(0xE555, 0xAA), (0xE2AA, 0x55), (0xE555, 0xA0), (0xE020, 0x88)] {
            w(&mut slot, addr, val, 1_100_020);
        }
        let p = slot.physical_mut().unwrap();
        assert!(p.dirty() && p.generation() > 0 && p.writable());
        let image = slot.crt_image(1_100_030).unwrap();
        let saved = PhysicalCart::from_crt(&image, FlashDecode::Both).unwrap();
        let chips = parse(&image).unwrap().chips;
        let chip = |bank: u16, load: u16| chips.iter().find(|c| c.bank == bank && c.load == load).map(|c| &image[c.data..c.data + c.size]);
        let bank8 = chip(8, 0x8000).unwrap();
        assert_eq!((bank8[0x10], bank8[0x11], bank8[0x1FFF]), (0x5A, 0xFF, 0xFF), "sector 1 erased, one byte programmed");
        assert_eq!(chip(9, 0x8000).unwrap(), &[0xFF; BANK_8K][..], "bank 9 is in sector 1");
        assert_eq!(chip(8, 0xA000).unwrap()[0x20], 0x98 & 0x88, "ROMH programmed through $E000");
        assert_eq!(chip(7, 0x8000).unwrap(), &fill(7, 0x10)[..], "sector 0 untouched");
        assert_eq!(saved.banks(), 64);
        assert_eq!(chips.len(), 20, "no new packets: banks 10-15 are erased, the rest was never written");
    }

    /// An erase pending over a C64 reset completes: TRX64's cycle counter restarts, the cartridge's epoch does not.
    #[test]
    fn a_reset_release_keeps_flash_time_running() {
        let cart = CartHandle::default();
        let mut slot = slot_with(&easyflash(1), FlashDecode::Both);
        let bi = info();
        slot.wr(&cart, 0xDE02, 5, &bi, 0);
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0x80), (0x8555, 0xAA), (0x82AA, 0x55), (0x8000, 0x30)] {
            slot.wr(&cart, addr, val, &bi, 900_000);
        }
        slot.reset_release(950_000);
        slot.reset(&cart);
        slot.wr(&cart, 0xDE02, 5, &bi, 0);
        assert_ne!(slot.read(&cart, 0x8000, &bi, 10), Some(0xFF), "still erasing just after the reset");
        assert_eq!(slot.read(&cart, 0x8000, &bi, 960_000), Some(0xFF), "900 000 + 1 000 050 cycles later");
    }

    /// `flash-decode`: the AM29F040 family takes 11-bit command addresses with `11` and `both`, only 15-bit ones with
    /// `15`; every 15-bit address also matches the 11-bit decode.
    #[test]
    fn flash_decode_selects_the_command_addresses() {
        let cart = CartHandle::default();
        let bi = info();
        let gmod2 = crt(60, 0, 1, &[(0, 0x8000, fill(0, 0x30), 0)]);
        // AA 55 90 in ULTIMAX ($DE00 = $C0 | bank), the ids read back in 8 K mode (GMod2 reads its flash only there),
        // then F0.
        let autoselect = |slot: &mut Slot, cmds: &[(u8, u16, u8)]| {
            for &(bank, addr, val) in cmds {
                slot.wr(&cart, 0xDE00, 0xC0 | bank, &bi, 0);
                slot.wr(&cart, addr, val, &bi, 0);
            }
            slot.wr(&cart, 0xDE00, 0x00, &bi, 0);
            let ids = (slot.read(&cart, 0x8000, &bi, 0), slot.read(&cart, 0x8001, &bi, 0));
            slot.wr(&cart, 0xDE00, 0xC0, &bi, 0);
            slot.wr(&cart, 0x8000, 0xF0, &bi, 0);
            ids
        };
        let short = [(0, 0xE555, 0xAA), (0, 0xE2AA, 0x55), (0, 0xE555, 0x90)];
        let long = [(2, 0x9555, 0xAA), (1, 0x8AAA, 0x55), (2, 0x9555, 0x90)];
        let ids = (Some(0x01), Some(0xA4));
        let mut both = slot_with(&gmod2, FlashDecode::Both);
        assert_eq!((autoselect(&mut both, &short), autoselect(&mut both, &long)), (ids, ids));
        let mut strict = slot_with(&gmod2, FlashDecode::Long);
        assert_eq!(autoselect(&mut strict, &long), ids);
        assert_ne!(autoselect(&mut strict, &short), ids, "the cartlib unlock at $E555/$E2AA does nothing on a 15-bit chip");
        let mut ef = slot_with(&easyflash(4), FlashDecode::Long);
        ef.wr(&cart, 0xDE02, 5, &bi, 0);
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0x90)] {
            ef.wr(&cart, addr, val, &bi, 0);
        }
        assert_eq!(ef.read(&cart, 0x8000, &bi, 0), Some(0x10), "11-bit unlock refused: array data");
        let mb = crt(86, 0, 1, &[(0, 0x8000, fill(0, 0x20), 0)]);
        let mut mb = slot_with(&mb, FlashDecode::Long);
        mb.wr(&cart, 0xDE02, 3, &bi, 0);
        for (bank, addr, val) in [(5u8, 0x8AAA, 0xAA), (2, 0x9555, 0x55), (5, 0x8AAA, 0x90)] {
            mb.wr(&cart, 0xDE00, bank, &bi, 0);
            mb.wr(&cart, addr, val, &bi, 0);
        }
        assert_eq!((mb.read(&cart, 0x8000, &bi, 0), mb.read(&cart, 0x8001, &bi, 0)), (Some(0xC2), Some(0x58)), "16-bit $AAAA/$5555");
    }

    #[test]
    fn eapi_block_and_c64megacart_banks_are_as_on_the_board() {
        let cart = CartHandle::default();
        let bi = info();
        let mut romh0 = fill(0, 0x90);
        romh0[0x1800..0x1804].copy_from_slice(b"eapi");
        let ef = crt(32, 1, 0, &[(0, 0x8000, fill(0, 0x10), 2), (0, 0xA000, romh0.clone(), 2)]);
        let mut slot = slot_with(&ef, FlashDecode::Both);
        slot.wr(&cart, 0xDE02, 7, &bi, 0);
        let romh: Vec<u8> = (0xA000..0xC000u16).map(|a| slot.read(&cart, a, &bi, 0).unwrap()).collect();
        assert_eq!(romh, romh0, "the CRT's own EAPI block, not TRX64's");

        let mc = crt(88, 0, 1, &[(0, 0x8000, fill(0, 0x50), 2), (255, 0x8000, fill(255, 0x50), 2)]);
        let mut slot = slot_with(&mc, FlashDecode::Both);
        // fill(255, 0x50) starts with 0x50 + 255, wrapped: 0x4F.
        for (high, low, want) in [(0u8, 255u8, 0x4Fu8), (1, 0, 0x50), (3, 255, 0x4F)] {
            slot.wr(&cart, 0xDF00, high, &bi, 0);
            slot.wr(&cart, 0xDE00, low, &bi, 0);
            assert_eq!(slot.read(&cart, 0x8000, &bi, 0), Some(want), "bank {}", (u16::from(high) << 8) | u16::from(low));
        }
    }

    #[test]
    fn flash_outside_the_crt_gets_new_packets_and_eeprom_round_trips() {
        let cart = CartHandle::default();
        let bi = info();
        let mut slot = slot_with(&crt(86, 0, 1, &[(0, 0x8000, fill(0, 0x20), 0)]), FlashDecode::Both);
        slot.wr(&cart, 0xDE02, 3, &bi, 0);
        slot.wr(&cart, 0xDE00, 100, &bi, 0);
        for (addr, val) in [(0x8AAA, 0xAA), (0x8555, 0x55), (0x8AAA, 0xA0), (0x8001, 0x42)] {
            slot.wr(&cart, addr, val, &bi, 10);
        }
        let image = slot.crt_image(20).unwrap();
        let chips = parse(&image).unwrap().chips;
        assert_eq!(chips.len(), 2);
        assert_eq!((chips[1].bank, chips[1].load, chips[1].kind, image[chips[1].data + 1]), (100, 0x8000, CHIP_FLASH, 0x42));

        let mut eeprom = vec![0xFF; EEPROM_SIZE];
        eeprom[..4].copy_from_slice(b"UE2E");
        let gmod2 = crt(60, 0, 1, &[(0, 0x8000, fill(0, 0x30), 0), (0, EEPROM_LOAD, eeprom, 0)]);
        let mut p = PhysicalCart::from_crt(&gmod2, FlashDecode::Both).unwrap();
        assert_eq!(p.crt_image(0), gmod2, "flash and EEPROM chunk as inserted");
        // cartlib_internal_eeprom_read_word(0) over the GMod2 register.
        let mut slot = slot_with(&gmod2, FlashDecode::Both);
        slot.wr(&cart, 0xDE00, 0x40, &bi, 0);
        let cmd = 0x6u16 << 10;
        for bit in (0..13).rev() {
            let v = 0x40 | if (cmd >> bit) & 1 != 0 { 0x10 } else { 0 };
            for val in [v, v | 0x20, v] {
                slot.wr(&cart, 0xDE00, val, &bi, 0);
            }
        }
        let mut word = 0u16;
        for bit in (0..16).rev() {
            slot.wr(&cart, 0xDE00, 0x40, &bi, 0);
            slot.wr(&cart, 0xDE00, 0x60, &bi, 0);
            if slot.read(&cart, 0xDE00, &bi, 0).unwrap() & 0x80 != 0 {
                word |= 1 << bit;
            }
        }
        assert_eq!(word, u16::from_be_bytes(*b"UE"));
    }

    #[test]
    fn logic_cartridge_serves_its_own_memory() {
        let cart = CartHandle::default();
        let bi = info();
        let chips: Vec<_> = (0..4).map(|b| (b, 0x8000, [fill(b, 0x40), fill(b, 0xC0)].concat(), 0)).collect();
        let inserted = crt(8, 0, 0, &chips);
        let mut slot = slot_with(&inserted, FlashDecode::Both);
        assert_eq!((lines(&slot, &cart), slot.read(&cart, 0x8000, &bi, 0)), ((0, 0), Some(0x40)), "Super Games: 16K bank 0");
        slot.wr(&cart, 0xDF00, 2, &bi, 0);
        assert_eq!((slot.read(&cart, 0x8000, &bi, 0), slot.read(&cart, 0xA000, &bi, 0)), (Some(0x42), Some(0xC2)));
        slot.wr(&cart, 0xDF00, 4, &bi, 0);
        assert_eq!((lines(&slot, &cart), slot.detect()), ((1, 1), 0x03), "off: the port reads empty");
        assert_eq!(slot.crt_image(0).unwrap(), inserted);
    }
}
