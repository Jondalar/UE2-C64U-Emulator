//! SPI flash controller 0x10060200 + S25FL128L / W25Q128JV NOR model + overlay-UI config seeding.
//! Spec: docs/specs/S06-spi-flash.md, the W25Q128JV docs/specs/S47-c64u-flash-install.md §3. Behaviour:
//! docs/hw/06-spi-flash-config.md (H1-H10), docs/hw/00-memory-map.md §2 C6-C10, C23, C35.
//!
//! - Controller: DATA/RATE/CTRL/CRC selected by address bits 3:2, CS framing per 06 H7.
//! - Chip ([`FlashChip`], `MachineConfig::flash_chip`): the S25FL128L, JEDEC `01 60 18`, which the W25Q and S25FL-K
//!   testers reject and `S25FLxxxL_Flash` claims (06 H1, 00 §3 C8); or the C64U's W25Q128JV, JEDEC `EF 40 18`, which
//!   `W25Q_Flash` claims, with its status registers and block protection. Every operation completes inside the bus
//!   access (06 §Interrupts): SR1 BUSY is 0.
//! - Persistence: `MachineConfig::flash_image`, written back at most ~0.5 s wall after a program/erase and
//!   on drop. The W25Q's status registers are not in the image: every run powers up with them 0 (S47 D3).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;
use crate::settings::Record;
use crate::time::CLOCKS_PER_MS;

/// `FLASH_BASE` (iomap.h:25), one 256-byte window (ultimate_logic_32.vhd:1009-1022).
pub const FLASH_BASE: u32 = 0x1006_0200;
const FLASH_WINDOW: u32 = 0x100;

/// The controller decodes only address bits 3:2 (spi_peripheral_io.vhd:91,116): DATA spans +0..+3, which
/// carries the four bytes of `DATA_32`, and the registers repeat every 0x10 inside the window.
const REG_MASK: u32 = 0x0C;
const REG_DATA: u32 = 0x00;
const REG_RATE: u32 = 0x04;
const REG_CTRL: u32 = 0x08;

/// RATE reads the fixed rate (`g_fixed_rate => true, g_init_rate => 1`, ultimate_logic_32.vhd:1115-1134).
const RATE_FIXED: u8 = 0x01;

/// CTRL bits `SPI_FORCE_SS` / `SPI_LEVEL_SS` (w25q_flash.h:31-32, spi_peripheral_io.vhd:102-104).
const SPI_FORCE_SS: u8 = 0x01;
const SPI_LEVEL_SS: u8 = 0x02;

/// S25FL128L geometry: 4096 sectors of 16 pages of 256 bytes (s25fl_l_flash.cc:79-83, s25fl_l_flash.h:11).
pub const FLASH_SIZE: usize = 16 << 20;
const ADDR_MASK: usize = FLASH_SIZE - 1;
const PAGE_SIZE: usize = 0x100;
const SECTOR_SIZE: usize = 0x1000;
const SECTOR_COUNT: usize = FLASH_SIZE / SECTOR_SIZE;

/// The chip on the controller's chip select (S47 §2: `--flash-chip`, or the board model).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlashChip {
    /// Infineon S25FL128L: ue2emu's chip since S06, for the U64-II whose chip nobody measured.
    #[default]
    S25fl128l,
    /// Winbond W25Q128JV: the C64U's chip, measured on a board (S47 §1).
    W25q128,
}

impl FlashChip {
    /// The name `--flash-chip` takes (lower case) and the start-up line shows.
    pub fn name(self) -> &'static str {
        match self {
            FlashChip::S25fl128l => "S25FL128L",
            FlashChip::W25q128 => "W25Q128",
        }
    }

    /// Manufacturer, type, capacity as `9F` answers them.
    /// - S25FL128L `01 60 18`: the W25Q and S25FL-K testers run first and reject it (w25q_flash.cc:139-144,
    ///   s25fl_flash.cc:51-56); `S25FLxxxL_Flash::tester` accepts it (s25fl_l_flash.cc:65-83). 06 H1, 00 §3 C8.
    /// - W25Q128JV `EF 40 18`: `W25Q_Flash::tester` accepts it as 4096 sectors (w25q_flash.cc:138-174) and names it
    ///   "Winbond W25Q128" (w25q_flash.cc:182-196).
    pub fn jedec_id(self) -> [u8; 3] {
        match self {
            FlashChip::S25fl128l => [0x01, 0x60, 0x18],
            FlashChip::W25q128 => [0xEF, 0x40, 0x18],
        }
    }

    /// Config pages of the chip's driver: 24 (s25fl_l_flash.h:9) or 16 (`W25Q_NUM_CONFIG_PAGES`, w25q_flash.h:8).
    pub fn config_pages(self) -> usize {
        match self {
            FlashChip::S25fl128l => 24,
            FlashChip::W25q128 => 16,
        }
    }

    /// Config page `p` = sector `sector_count - pages + p` (w25q_flash.cc:261-267): 0xFE8000 + p × 4 KiB on the
    /// S25FL128L, 0xFF0000 + p × 4 KiB on the W25Q128JV.
    pub fn config_page_at(self, p: usize) -> usize {
        (SECTOR_COUNT - self.config_pages() + p) * SECTOR_SIZE
    }
}

/// Answer to `4B`. Not all-00/FF, so the MAC (rmii_interface.cc:123-128) and the hostname suffix
/// (product.cc:184-191) are not degenerate. 06 H10, 00 §2 C23.
pub const UID: [u8; 8] = *b"UE2C64U\x01";

/// Logical config page, the part `ConfigPage` reads and writes (w25q_flash.cc:251-254).
const CONFIG_PAGE_SIZE: usize = 512;
/// `CFG_USERIF_STORE_ID` "GEN." (userinterface.h:28).
const USERIF_STORE_ID: u32 = 0x4745_4E2E;
/// `CFG_USERIF_ITYPE` (userinterface.h:38).
const CFG_USERIF_ITYPE: u8 = 0x08;
/// `CFG_TYPE_ENUM` (config.h:75).
const CFG_TYPE_ENUM: u8 = 0x02;
/// ITYPE 1 = "Overlay on HDMI" (userinterface.cc:117,123).
const ITYPE_OVERLAY: u8 = 1;

/// Wall time from the oldest unsaved program/erase to the image write-back (spec: ≤ 1 s).
const FLUSH_DELAY: Duration = Duration::from_millis(500);
/// Emulated interval between wall-clock checks while the image has unsaved sectors.
const FLUSH_POLL_CLOCKS: u64 = 10 * CLOCKS_PER_MS;

/// Opcodes (s25fl_l_flash.h:13-45; 3-byte forms and `C7` from w25q_flash.h:10-25; `66`/`99` from
/// s25fl_l_flash.cc:36-45). The W25Q128JV's own (instruction set table 1 of its datasheet): `01`, `15`, `31`, `11`,
/// `50`, and `35` as read SR2; it has no 4-byte forms and no `07`.
mod cmd {
    pub const WRSR1: u8 = 0x01;
    pub const PP: u8 = 0x02;
    pub const READ: u8 = 0x03;
    pub const WRDI: u8 = 0x04;
    pub const RDSR1: u8 = 0x05;
    pub const WREN: u8 = 0x06;
    pub const RDSR2: u8 = 0x07;
    pub const FAST_READ: u8 = 0x0B;
    pub const FAST_READ4: u8 = 0x0C;
    pub const PP4: u8 = 0x12;
    pub const READ4: u8 = 0x13;
    pub const SE: u8 = 0x20;
    pub const SE4: u8 = 0x21;
    pub const RDCR1: u8 = 0x35;
    pub const RUID: u8 = 0x4B;
    pub const HBE: u8 = 0x52;
    pub const HBE4: u8 = 0x53;
    pub const CE: u8 = 0x60;
    pub const RSTEN: u8 = 0x66;
    pub const RST: u8 = 0x99;
    pub const RDID: u8 = 0x9F;
    pub const CE2: u8 = 0xC7;
    pub const BE: u8 = 0xD8;
    pub const BE4: u8 = 0xDC;
    pub const W_WRSR3: u8 = 0x11;
    pub const W_RDSR3: u8 = 0x15;
    pub const W_WRSR2: u8 = 0x31;
    pub const W_RDSR2: u8 = 0x35;
    pub const W_VWREN: u8 = 0x50;
}

/// Address bytes of an addressed command. The S25FL128L stays in its default 3-byte mode; its 4-byte forms carry
/// their own width (06 §Functional model, H8). The W25Q128JV has only the 3-byte forms.
fn addr_len(chip: FlashChip, op: u8) -> Option<usize> {
    match op {
        cmd::READ | cmd::FAST_READ | cmd::PP | cmd::SE | cmd::HBE | cmd::BE => Some(3),
        cmd::READ4 | cmd::FAST_READ4 | cmd::PP4 | cmd::SE4 | cmd::HBE4 | cmd::BE4 if chip == FlashChip::S25fl128l => {
            Some(4)
        }
        _ => None,
    }
}

/// Block size cleared by an erase command.
fn erase_size(chip: FlashChip, op: u8) -> Option<usize> {
    let s25fl = chip == FlashChip::S25fl128l;
    match op {
        cmd::SE => Some(SECTOR_SIZE),
        cmd::HBE => Some(0x8000),
        cmd::BE => Some(0x1_0000),
        cmd::CE | cmd::CE2 => Some(FLASH_SIZE),
        cmd::SE4 if s25fl => Some(SECTOR_SIZE),
        cmd::HBE4 if s25fl => Some(0x8000),
        cmd::BE4 if s25fl => Some(0x1_0000),
        _ => None,
    }
}

/// W25Q128JV status register bits (datasheet 7.1, figures 4a-4c).
mod sr {
    /// SR2 bit 0: power-supply lock-down.
    pub const SRL: u8 = 0x01;
    /// SR2 bit 6: complement protect.
    pub const CMP: u8 = 0x40;
    /// Bits a write changes (8.2.5): SR1 SRP SEC TB BP2:0 (SRP from 7.1.1's table), SR2 CMP LB3:1 QE SRL,
    /// SR3 DRV1:0 WPS. BUSY, WEL, SUS and the reserved bits are not written.
    pub const WRITABLE: [u8; 3] = [0xFC, 0x7B, 0x64];
    /// SR2 LB3:1: one-time programmable, never back from 1 to 0 (7.1.3, 8.2.5).
    pub const OTP: [u8; 3] = [0x00, 0x38, 0x00];
}

/// The range BP2:0, TB, SEC and CMP protect on the W25Q128JV (datasheet 7.1.8 for CMP = 0, 7.1.9 for CMP = 1), with
/// WPS = 0. CMP = 1 protects the complement of the CMP = 0 range. SEC = 1 with BP = 110 is in neither table; it is
/// taken as 32 KiB, as BP = 10x.
fn w25q_protected(sr1: u8, sr2: u8) -> Range<usize> {
    let bp = usize::from(sr1 >> 2 & 7);
    let (bottom, sector) = (sr1 & 0x20 != 0, sr1 & 0x40 != 0);
    let base = match bp {
        0 => 0..0,
        7 => 0..FLASH_SIZE,
        _ => {
            let size = if sector { SECTOR_SIZE << (bp - 1).min(3) } else { 0x4_0000 << (bp - 1) };
            if bottom { 0..size } else { FLASH_SIZE - size..FLASH_SIZE }
        }
    };
    if sr2 & sr::CMP == 0 {
        base
    } else if base.is_empty() {
        0..FLASH_SIZE
    } else if base.start == 0 {
        base.end..FLASH_SIZE
    } else {
        0..base.start
    }
}

/// The NOR array and command parser of the chip. A frame is the bytes clocked while CS is low; its first byte is the
/// opcode. Write and erase commands execute when CS rises and finish at once.
struct Nor {
    chip: FlashChip,
    mem: Vec<u8>,
    /// SR1 bit 1: set by `06`; cleared by `04`, a reset, and every executed program/erase.
    wel: bool,
    /// W25Q128JV: SR1-SR3 as read, without WEL and BUSY. A volatile write (after `50`) changes only these.
    sr: [u8; 3],
    /// W25Q128JV: the non-volatile copies, which a reset (`66`/`99`) loads back (8.2.5, 8.2.37). Power-up is 0 (S47 D3).
    sr_nv: [u8; 3],
    /// W25Q128JV: a lone `50` frame makes the next status register write volatile (8.2.2).
    volatile_we: bool,
    /// W25Q128JV: the data bytes of a status register write frame.
    sr_in: [u8; 2],
    /// A lone `66` frame arms the next `99` frame.
    reset_armed: bool,
    /// Opcode of the open frame, `None` until its first byte.
    op: Option<u8>,
    /// Bytes clocked after the opcode.
    count: usize,
    /// Address shifted in MSB first; advances while reading.
    addr: u32,
    /// Page-program data, 0xFF where nothing was sent. Offsets wrap at the page end like the chip's buffer.
    page_buf: [u8; PAGE_SIZE],
    /// Factory unique ID answered by `RUID`: [`UID`] unless [`SpiFlash::set_unique_id`] replaced it.
    uid: [u8; 8],
}

impl Nor {
    fn erased(chip: FlashChip) -> Self {
        Nor {
            chip,
            mem: vec![0xFF; FLASH_SIZE],
            wel: false,
            sr: [0; 3],
            sr_nv: [0; 3],
            volatile_we: false,
            sr_in: [0; 2],
            reset_armed: false,
            op: None,
            count: 0,
            addr: 0,
            page_buf: [0xFF; PAGE_SIZE],
            uid: UID,
        }
    }

    fn w25q(&self) -> bool {
        self.chip == FlashChip::W25q128
    }

    /// SR1 = `SRP0 SEC TB BP2 BP1 BP0 WEL BUSY` (w25q_flash.cc:396-397), never busy (06 H3). The S25FL128L's is
    /// unprotected; the W25Q's carries what its status register writes set.
    fn sr1(&self) -> u8 {
        self.sr[0] & sr::WRITABLE[0] | u8::from(self.wel) << 1
    }

    /// The addresses program and erase commands may not touch: none on the S25FL128L, which is never protected here.
    fn protected(&self) -> Range<usize> {
        if self.w25q() {
            w25q_protected(self.sr[0], self.sr[1])
        } else {
            0..0
        }
    }

    /// "If any Erase or Program command specifies a memory region that contains protected data portion, this command
    /// will be ignored" (datasheet 7.1.8 note 3). The datasheet says nothing of WEL for an ignored command; no cycle
    /// runs, so it stays.
    fn touches_protected(&self, range: &Range<usize>) -> bool {
        let p = self.protected();
        p.start < range.end && range.start < p.end
    }

    /// `01` (with one or two bytes), `31`, `11` on the W25Q128JV (datasheet 8.2.5): after `50` the volatile registers
    /// change, after `06` both copies and WEL clears; neither leaves them alone. SRL = 1 refuses every write until
    /// power-off (7.1.1; SRP needs /WP low to refuse, and the model's /WP is high). LB3:1 never go back to 0.
    fn write_status(&mut self, op: u8, count: usize) {
        let volatile = std::mem::take(&mut self.volatile_we);
        let [b0, b1] = self.sr_in;
        let writes: &[(usize, u8)] = match (op, count) {
            (cmd::WRSR1, 1) => &[(0, b0)],
            (cmd::WRSR1, 2) => &[(0, b0), (1, b1)],
            (cmd::W_WRSR2, 1) => &[(1, b0)],
            (cmd::W_WRSR3, 1) => &[(2, b0)],
            _ => return,
        };
        if self.sr[1] & sr::SRL != 0 || !(volatile || self.wel) {
            return;
        }
        let merge = |old: u8, r: usize, v: u8| old & !sr::WRITABLE[r] | v & sr::WRITABLE[r] | old & sr::OTP[r];
        for &(r, v) in writes {
            self.sr[r] = merge(self.sr[r], r, v);
            if !volatile {
                self.sr_nv[r] = merge(self.sr_nv[r], r, v);
            }
        }
        if !volatile {
            self.wel = false;
        }
    }

    /// `66` then `99`: WEL clears; the W25Q's volatile status registers take their non-volatile values (8.2.37).
    fn reset(&mut self) {
        self.wel = false;
        self.sr = self.sr_nv;
        self.volatile_we = false;
    }

    /// CS falls.
    fn start_frame(&mut self) {
        self.op = None;
        self.count = 0;
        self.addr = 0;
    }

    fn read_next(&mut self) -> u8 {
        let byte = self.mem[self.addr as usize & ADDR_MASK];
        self.addr = self.addr.wrapping_add(1);
        byte
    }

    /// One full-duplex byte inside the open frame. Returns MISO.
    fn xfer(&mut self, mosi: u8) -> u8 {
        let Some(op) = self.op else {
            self.op = Some(mosi);
            return 0xFF;
        };
        let i = self.count;
        self.count += 1;
        let w25q = self.w25q();
        match op {
            cmd::RDID => self.chip.jedec_id().get(i).copied().unwrap_or(0xFF),
            // Repeated on every byte: `wait_ready` polls inside one frame (w25q_flash.cc:492-503).
            cmd::RDSR1 => self.sr1(),
            cmd::W_RDSR2 if w25q => self.sr[1],
            cmd::W_RDSR3 if w25q => self.sr[2],
            cmd::RDSR2 | cmd::RDCR1 if !w25q => 0x00,
            cmd::WRSR1 | cmd::W_WRSR2 | cmd::W_WRSR3 if w25q => {
                if let Some(slot) = self.sr_in.get_mut(i) {
                    *slot = mosi;
                }
                0xFF
            }
            // 4 dummy bytes, then 8 UID bytes (w25q_flash.cc:240-245).
            cmd::RUID => i.checked_sub(4).and_then(|k| self.uid.get(k)).copied().unwrap_or(0xFF),
            _ => match addr_len(self.chip, op) {
                Some(n) if i < n => {
                    self.addr = (self.addr << 8) | u32::from(mosi);
                    0xFF
                }
                Some(n) => self.data(op, i - n, mosi),
                None => 0xFF,
            },
        }
    }

    /// Byte `k` after the address of an addressed command.
    fn data(&mut self, op: u8, k: usize, mosi: u8) -> u8 {
        match op {
            // Read wraps at the end of the array.
            cmd::READ | cmd::READ4 => self.read_next(),
            // One dummy byte (8 latency cycles) before the data.
            cmd::FAST_READ | cmd::FAST_READ4 if k == 0 => 0xFF,
            cmd::FAST_READ | cmd::FAST_READ4 => self.read_next(),
            cmd::PP | cmd::PP4 => {
                if k == 0 {
                    self.page_buf.fill(0xFF);
                }
                self.page_buf[((self.addr as usize & (PAGE_SIZE - 1)) + k) % PAGE_SIZE] = mosi;
                0xFF
            }
            _ => 0xFF,
        }
    }

    /// CS rises: execute a complete write command. Returns the byte range it changed.
    fn end_frame(&mut self) -> Option<Range<usize>> {
        let op = self.op.take()?;
        let count = std::mem::take(&mut self.count);
        let armed = std::mem::take(&mut self.reset_armed);
        let alen = addr_len(self.chip, op);
        let start = self.addr as usize & ADDR_MASK;
        let w25q = self.w25q();
        match op {
            // Single-byte commands only count alone in their frame; `66 99` in one frame is a no-op
            // (s25fl_l_flash.cc:35-38, 06 Q6).
            cmd::WREN | cmd::WRDI | cmd::RSTEN | cmd::RST if count != 0 => {}
            cmd::W_VWREN if w25q && count != 0 => {}
            cmd::WREN => self.wel = true,
            cmd::WRDI => self.wel = false,
            cmd::RSTEN => self.reset_armed = true,
            cmd::RST if armed => self.reset(),
            cmd::W_VWREN if w25q => self.volatile_we = true,
            cmd::WRSR1 | cmd::W_WRSR2 | cmd::W_WRSR3 if w25q => self.write_status(op, count),
            // Program only clears bits (06 §Functional model).
            cmd::PP | cmd::PP4 if self.wel && alen.is_some_and(|n| count > n) => {
                let page = start & !(PAGE_SIZE - 1);
                if self.touches_protected(&(page..page + PAGE_SIZE)) {
                    return None;
                }
                for (cell, byte) in self.mem[page..page + PAGE_SIZE].iter_mut().zip(&self.page_buf) {
                    *cell &= byte;
                }
                self.wel = false;
                return Some(page..page + PAGE_SIZE);
            }
            _ if self.wel && count == alen.unwrap_or(0) => {
                let size = erase_size(self.chip, op)?;
                let base = start & !(size - 1);
                if self.touches_protected(&(base..base + size)) {
                    return None;
                }
                self.mem[base..base + size].fill(0xFF);
                self.wel = false;
                return Some(base..base + size);
            }
            _ => {}
        }
        None
    }
}

/// Host file holding the array; changed sectors are written back debounced.
struct Image {
    file: File,
    path: PathBuf,
    /// One flag per 4 KiB sector.
    dirty: Vec<bool>,
    /// Wall time of the oldest change not yet written.
    dirty_since: Option<Instant>,
    /// Emulated clock of the next wall-clock check while dirty.
    poll_at: u64,
}

fn image_error(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("flash image {}: {e}", path.display()))
}

impl Image {
    fn flush(&mut self, mem: &[u8]) -> io::Result<()> {
        for sector in 0..SECTOR_COUNT {
            if self.dirty[sector] {
                let start = sector * SECTOR_SIZE;
                self.file
                    .seek(SeekFrom::Start(start as u64))
                    .and_then(|_| self.file.write_all(&mem[start..start + SECTOR_SIZE]))
                    .map_err(|e| image_error(&self.path, e))?;
                self.dirty[sector] = false;
            }
        }
        self.dirty_since = None;
        Ok(())
    }
}

/// SPI master at `FLASH_BASE` with the S25FL128L or the W25Q128JV on its chip select.
pub struct SpiFlash {
    chip: Nor,
    /// CTRL bit 0, reset 0 (spi_peripheral_io.vhd:157).
    force_ss: bool,
    /// CTRL bit 1, reset 1 (spi_peripheral_io.vhd:158).
    level_ss: bool,
    /// `None` = volatile.
    image: Option<Image>,
}

impl SpiFlash {
    /// Volatile flash, fully erased.
    pub fn volatile(chip: FlashChip) -> Self {
        SpiFlash { chip: Nor::erased(chip), force_ss: false, level_ss: true, image: None }
    }

    /// Flash backed by the image at `path`, created erased (16 MiB of 0xFF) if missing. A shorter file is
    /// padded with 0xFF; a longer one is refused.
    pub fn open(path: &Path, chip: FlashChip) -> io::Result<Self> {
        Self::load(path, chip).map_err(|e| image_error(path, e))
    }

    /// Which chip this is (S47).
    pub fn chip(&self) -> FlashChip {
        self.chip.chip
    }

    /// The W25Q128JV's status registers SR1-SR3 as `05`/`35`/`15` read them (S47 §3); all 0 on the S25FL128L.
    pub fn status(&self) -> [u8; 3] {
        [self.chip.sr1(), self.chip.sr[1], self.chip.sr[2]]
    }

    /// Byte offset of config page `p` of this chip.
    fn config_at(&self, p: usize) -> usize {
        self.chip.chip.config_page_at(p)
    }

    fn config_page_count(&self) -> usize {
        self.chip.chip.config_pages()
    }

    /// Replace the factory unique ID that `RUID` (0x4B) returns. The firmware derives the Ethernet MAC and the
    /// default hostname from it (rmii_interface.cc:119-128, product.cc:184-191), so set it before the machine runs.
    pub fn set_unique_id(&mut self, uid: [u8; 8]) {
        self.chip.uid = uid;
    }

    fn load(path: &Path, chip: FlashChip) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let len = file.metadata()?.len();
        if len > FLASH_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{len} bytes, larger than the {FLASH_SIZE}-byte flash"),
            ));
        }
        let len = len as usize;
        let mut flash = Self::volatile(chip);
        file.read_exact(&mut flash.chip.mem[..len])?;
        if len < FLASH_SIZE {
            file.write_all(&flash.chip.mem[len..])?;
        }
        flash.image = Some(Image {
            file,
            path: path.to_path_buf(),
            dirty: vec![false; SECTOR_COUNT],
            dirty_since: None,
            poll_at: 0,
        });
        Ok(flash)
    }

    /// `cfg.flash_chip`, volatile or image-backed per `cfg.flash_image`; seeded with the overlay-UI page if
    /// `cfg.overlay_ui`.
    pub fn from_config(cfg: &MachineConfig) -> io::Result<Self> {
        let mut flash = match &cfg.flash_image {
            Some(path) => Self::open(path, cfg.flash_chip)?,
            None => Self::volatile(cfg.flash_chip),
        };
        if cfg.overlay_ui && flash.seed_overlay_ui() {
            flash.flush()?;
        }
        Ok(flash)
    }

    /// Write a user-interface config page with `CFG_USERIF_ITYPE = 1` unless one exists (00 §2 C35, Q-D2).
    /// Returns whether a page was written.
    ///
    /// Config page format, derived from the firmware:
    /// - Location: config page `p` is sector `sector_count - pages + p` (w25q_flash.cc:261-267; 4096 sectors
    ///   s25fl_l_flash.cc:79-83, w25q_flash.cc:170-174): `0xFE8000 + p * 0x1000` for the S25FL128L's 24 pages
    ///   (s25fl_l_flash.h:9), `0xFF0000 + p * 0x1000` for the W25Q128JV's 16 (w25q_flash.h:8). The firmware reads it
    ///   with opcode `03` and a 3-byte address (w25q_flash.cc:108-123).
    /// - Size: 512 bytes (w25q_flash.cc:251-254), written as a 4 KiB erase plus two 256-byte programs
    ///   (w25q_flash.cc:269-277).
    /// - Bytes 0..4: page id, native little-endian u32 (config.cc:280). The user-interface store id is
    ///   `CFG_USERIF_STORE_ID = 0x47454E2E` "GEN." (userinterface.h:28), registered only at
    ///   userinterface.cc:156.
    /// - Bytes 4..: items `id, type, len, payload[len]` (config.cc:706-758). ENUM is type 2 with len 1
    ///   (config.cc:730-738, config.h:75). The item list ends with id 0xFF and the rest of the page is 0xFF
    ///   (config.cc:277,322,407).
    /// - Unpack: an item's value is big-endian over `len` bytes; a type mismatch skips the item and an
    ///   out-of-range value takes the default (config.cc:671-692). Items absent from the page keep their
    ///   definition default (config.cc:398-424, 661-669).
    /// - `CFG_USERIF_ITYPE = 0x08` (userinterface.h:38): ENUM 0..=1, default 0 "Freeze", 1 "Overlay on HDMI"
    ///   (userinterface.cc:117,123), read by `getPreferredType` (userinterface.cc:255-258,
    ///   ultimate.cc:182-187).
    /// - Discovery: a store uses the first page whose id matches (config.cc:127-137); otherwise it claims
    ///   the first page reading id 0xFFFFFFFF and writes its defaults there (config.cc:154-167,179-181).
    ///
    /// The seed is `2E 4E 45 47 | 08 02 01 01 | FF...` in the first erased config page, written only when no
    /// page carries the id, so a saved choice is never overwritten. Every other store stays absent and the
    /// firmware claims its own page with defaults.
    pub fn seed_overlay_ui(&mut self) -> bool {
        let pages = self.config_page_count();
        if (0..pages).any(|p| self.page_id(p) == USERIF_STORE_ID) {
            return false;
        }
        let Some(p) = (0..pages).find(|&p| self.page_id(p) == 0xFFFF_FFFF) else {
            return false;
        };
        let base = self.config_at(p);
        let sector = &mut self.chip.mem[base..base + SECTOR_SIZE];
        sector.fill(0xFF);
        sector[..4].copy_from_slice(&USERIF_STORE_ID.to_le_bytes());
        sector[4..8].copy_from_slice(&[CFG_USERIF_ITYPE, CFG_TYPE_ENUM, 1, ITYPE_OVERLAY]);
        self.modified(base..base + SECTOR_SIZE, 0);
        true
    }

    /// The records stored for one config page, as the firmware would unpack them (S23 §6, the monitor's `config`).
    /// `None` when no page carries that id.
    pub fn config_page(&self, page: u32) -> Option<Vec<(u8, u8, Vec<u8>)>> {
        (0..self.config_page_count()).find_map(|p| {
            let at = self.config_at(p);
            (self.page_id(p) == page).then(|| {
                page_records(&self.chip.mem[at..at + CONFIG_PAGE_SIZE])
                    .into_iter()
                    .map(|(id, kind, payload)| (id, kind, payload.to_vec()))
                    .collect()
            })
        })
    }

    /// The image file behind this chip, or `None` for a volatile one (S23, the monitor's `flash`).
    /// The array as the chip holds it (16 MiB), for a reader outside the SPI port: the FPGA's configuration logic at an
    /// IPROG (S44 phase B).
    pub fn array(&self) -> &[u8] {
        &self.chip.mem
    }

    pub fn image(&self) -> Option<&Path> {
        self.image.as_ref().map(|i| i.path.as_path())
    }

    /// Sectors changed since the last write-out, and whether a write-out is pending (S23, `flash`).
    pub fn pending(&self) -> (usize, bool) {
        match &self.image {
            Some(image) => (image.dirty.iter().filter(|&&d| d).count(), image.dirty_since.is_some()),
            None => (0, false),
        }
    }

    /// The ids of the config pages in use, in page order (S23 §6, `config flash`).
    pub fn config_pages(&self) -> Vec<u32> {
        (0..self.config_page_count()).map(|p| self.page_id(p)).filter(|&id| id != 0xFFFF_FFFF).collect()
    }

    /// The id word of config page `p`, native little-endian (config.cc:280).
    fn page_id(&self, p: usize) -> u32 {
        let at = self.config_at(p);
        u32::from_le_bytes(self.chip.mem[at..at + 4].try_into().expect("4 bytes"))
    }

    /// Put settings records into their config pages (docs/specs/S21-settings.md §5): the first page carrying the
    /// page id, else the first erased one, as `register_store` picks (config.cc:127-167). A record of an id being
    /// set is replaced in place and a later duplicate dropped; new ids go before the 0xFF. Only the 512-byte logical
    /// page is written (w25q_flash.cc:251-254), the rest of the sector keeps its bytes.
    pub fn write_settings(&mut self, records: &[Record]) -> anyhow::Result<()> {
        let mut pages: Vec<u32> = records.iter().map(|r| r.page).collect();
        pages.sort_unstable();
        pages.dedup();
        let count = self.config_page_count();
        for page in pages {
            let wanted: Vec<&Record> = records.iter().filter(|r| r.page == page).collect();
            let Some(p) = (0..count)
                .find(|&p| self.page_id(p) == page)
                .or_else(|| (0..count).find(|&p| self.page_id(p) == 0xFFFF_FFFF))
            else {
                anyhow::bail!("page {page:08x}: all {count} config pages are taken");
            };
            let base = self.config_at(p);
            let claimed = self.page_id(p) == page;
            let old = &self.chip.mem[base..base + CONFIG_PAGE_SIZE];
            let mut placed = vec![false; wanted.len()];
            let mut items: Vec<(u8, u8, Vec<u8>)> = Vec::new();
            if claimed {
                for (id, kind, payload) in page_records(old) {
                    match wanted.iter().position(|r| r.id == id) {
                        Some(w) if !placed[w] => {
                            placed[w] = true;
                            items.push((id, wanted[w].kind, wanted[w].payload.clone()));
                        }
                        Some(_) => {}
                        None => items.push((id, kind, payload.to_vec())),
                    }
                }
            }
            for (w, r) in wanted.iter().enumerate() {
                if !placed[w] {
                    items.push((r.id, r.kind, r.payload.clone()));
                }
            }
            let mut new = page.to_le_bytes().to_vec();
            for (id, kind, payload) in &items {
                new.extend_from_slice(&[*id, *kind, payload.len() as u8]);
                new.extend_from_slice(payload);
            }
            new.push(0xFF);
            anyhow::ensure!(
                new.len() <= CONFIG_PAGE_SIZE,
                "page {page:08x}: the settings need {} bytes, a config page holds {CONFIG_PAGE_SIZE}",
                new.len()
            );
            new.resize(CONFIG_PAGE_SIZE, 0xFF);
            if new != old {
                self.chip.mem[base..base + CONFIG_PAGE_SIZE].copy_from_slice(&new);
                self.modified(base..base + CONFIG_PAGE_SIZE, 0);
            }
        }
        Ok(())
    }

    /// Write every sector changed since the last write-back to the image (no-op when volatile).
    pub fn flush(&mut self) -> io::Result<()> {
        match &mut self.image {
            Some(image) => image.flush(&self.chip.mem),
            None => Ok(()),
        }
    }

    /// Record a completed program/erase for the debounced write-back.
    fn modified(&mut self, range: Range<usize>, now: u64) {
        let Some(image) = &mut self.image else {
            return;
        };
        for sector in range.start / SECTOR_SIZE..range.end.div_ceil(SECTOR_SIZE) {
            image.dirty[sector] = true;
        }
        if image.dirty_since.is_none() {
            image.dirty_since = Some(Instant::now());
            image.poll_at = now + FLUSH_POLL_CLOCKS;
        }
    }

    /// Write the image back once `FLUSH_DELAY` of wall time has passed since the oldest unsaved change;
    /// otherwise schedule the next check.
    fn service(&mut self, wall: Instant, now: u64) {
        let Some(image) = &mut self.image else {
            return;
        };
        let Some(since) = image.dirty_since else {
            return;
        };
        if wall.saturating_duration_since(since) >= FLUSH_DELAY {
            match image.flush(&self.chip.mem) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("{e}");
                    image.dirty_since = Some(wall);
                }
            }
        }
        image.poll_at = now + FLUSH_POLL_CLOCKS;
    }

    /// Effective CS between bytes: low only when forced low.
    fn cs_forced_low(&self) -> bool {
        self.force_ss && !self.level_ss
    }

    /// One DATA access; writes send `mosi`, reads send 0xFF (spi_peripheral_io.vhd:92-94,117-120).
    /// Returns MISO.
    fn transfer(&mut self, mosi: u8, now: u64) -> u8 {
        if self.force_ss {
            // CS forced high: the byte shifts out with the chip deselected (06 H7).
            if self.level_ss {
                return 0xFF;
            }
            return self.chip.xfer(mosi);
        }
        // Auto CS: the byte is a frame of its own (spi.vhd:59-62,96-101).
        self.chip.start_frame();
        let miso = self.chip.xfer(mosi);
        self.end_frame(now);
        miso
    }

    fn end_frame(&mut self, now: u64) {
        if let Some(range) = self.chip.end_frame() {
            self.modified(range, now);
        }
    }

    /// A falling effective CS opens a frame, a rising one ends it (spi.vhd:128-130, 06 H7).
    fn write_ctrl(&mut self, val: u8, now: u64) {
        let was_low = self.cs_forced_low();
        self.force_ss = val & SPI_FORCE_SS != 0;
        self.level_ss = val & SPI_LEVEL_SS != 0;
        match (was_low, self.cs_forced_low()) {
            (false, true) => self.chip.start_frame(),
            (true, false) => self.end_frame(now),
            _ => {}
        }
    }
}

impl IoDevice for SpiFlash {
    fn name(&self) -> &'static str {
        "flash"
    }

    fn read8(&mut self, off: u32, ctx: &mut IoCtx) -> u8 {
        if off & REG_MASK == REG_DATA {
            self.transfer(0xFF, ctx.now)
        } else {
            self.peek8(off)
        }
    }

    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        match off & REG_MASK {
            REG_DATA => {
                self.transfer(val, ctx.now);
            }
            REG_CTRL => self.write_ctrl(val, ctx.now),
            // RATE is fixed; clearing the absent CRC (`g_crc => false`) has no visible effect.
            _ => {}
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        match off & REG_MASK {
            REG_DATA => 0xFF,
            REG_RATE => RATE_FIXED,
            // `0000 & !WRPROTn & !DETECTn & level & force` with WRPROTn = 1, DETECTn = 0
            // (spi_peripheral_io.vhd:127-128, ultimate_logic_32.vhd:1128-1129).
            REG_CTRL => 0x04 | (u8::from(self.level_ss) << 1) | u8::from(self.force_ss),
            // CRC reads 0 with `g_crc => false` (spi.vhd:134).
            _ => 0x00,
        }
    }

    fn next_event(&self) -> Option<u64> {
        self.image.as_ref().filter(|image| image.dirty_since.is_some()).map(|image| image.poll_at)
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.service(Instant::now(), ctx.now);
    }

    fn reset(&mut self) {
        // Controller reset state (spi_peripheral_io.vhd:153-160). An open frame is dropped unexecuted; the
        // array, WEL and the W25Q's status registers (a lock-down too) belong to the chip, which keeps power over an
        // IPROG (S47 §3).
        self.force_ss = false;
        self.level_ss = true;
        self.chip.start_frame();
    }

    crate::impl_as_any!();
}

impl Drop for SpiFlash {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("{e}");
        }
    }
}

/// The records of a config page as `ConfigStore::unpack` walks them (config.cc:398-424): from offset 4 up to id
/// 0xFF, a length that does not fit ends the walk.
fn page_records(page: &[u8]) -> Vec<(u8, u8, &[u8])> {
    let mut records = Vec::new();
    let mut index = 4;
    while index + 3 <= page.len() && page[index] != 0xFF {
        let len = usize::from(page[index + 2]);
        if len > page.len() - index - 3 {
            break;
        }
        records.push((page[index], page[index + 1], &page[index + 3..index + 3 + len]));
        index += len + 3;
    }
    records
}

/// Maps the controller window. An unusable flash image aborts machine construction.
pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    let flash = SpiFlash::from_config(cfg).unwrap_or_else(|e| panic!("{e}"));
    map.add(FLASH_BASE, FLASH_WINDOW, Box::new(flash));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::IrqState;

    /// `SPI_FLASH_DATA` / `SPI_FLASH_CTRL` (w25q_flash.h:27-29).
    const DATA: u32 = FLASH_BASE;
    const CTRL: u32 = FLASH_BASE + 8;
    /// The S25FL128L's identity and config pages, which the tests below that name no chip use.
    const JEDEC_ID: [u8; 3] = [0x01, 0x60, 0x18];
    const CONFIG_BASE: usize = 0xFE_8000;
    const CONFIG_PAGES: usize = 24;

    fn cfg(flash_image: Option<PathBuf>, overlay_ui: bool) -> MachineConfig {
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        cfg.flash_image = flash_image;
        cfg.overlay_ui = overlay_ui;
        cfg
    }

    fn w25q_cfg(flash_image: Option<PathBuf>, overlay_ui: bool) -> MachineConfig {
        let mut cfg = cfg(flash_image, overlay_ui);
        cfg.flash_chip = FlashChip::W25q128;
        cfg
    }

    /// 256 bytes with set and cleared bits.
    fn pattern(seed: u8) -> Vec<u8> {
        (0..=255u8).map(|i| i.wrapping_mul(37) ^ seed).collect()
    }

    /// Item walk of `ConfigStore::unpack` (config.cc:398-424): `(id, type, payload)` up to id 0xFF.
    fn unpack(page: &[u8]) -> Vec<(u8, u8, Vec<u8>)> {
        let mut items = Vec::new();
        let mut index = 4;
        while index < page.len() && page[index] != 0xFF {
            let len = usize::from(page[index + 2]);
            assert!(len <= page.len() - index - 3, "item length does not fit (config.cc:411)");
            items.push((page[index], page[index + 1], page[index + 3..index + 3 + len].to_vec()));
            index += len + 3;
        }
        items
    }

    /// Stand-in for `SystemBus`: the installed window plus an access context.
    struct Rig {
        map: IoMap,
        ram: Vec<u8>,
        irq: IrqState,
        console: Vec<u8>,
        now: u64,
    }

    impl Rig {
        fn new(cfg: &MachineConfig) -> Self {
            let mut map = IoMap::new();
            install(&mut map, cfg);
            Rig { map, ram: Vec::new(), irq: IrqState::new(), console: Vec::new(), now: 0 }
        }

        fn flash(&mut self) -> &mut SpiFlash {
            self.map.get_mut::<SpiFlash>().expect("flash installed")
        }

        fn read8(&mut self, addr: u32) -> u8 {
            let (dev, off) = self.map.resolve(addr).expect("flash window mapped");
            let mut ctx =
                IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram[..], irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].read8(off, &mut ctx)
        }

        fn write8(&mut self, addr: u32, val: u8) {
            let (dev, off) = self.map.resolve(addr).expect("flash window mapped");
            let mut ctx =
                IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram[..], irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].write8(off, val, &mut ctx);
        }

        /// A 32-bit IO access is four byte accesses at +0..+3, bits 7:0 first (bus_converter.vhd:56,160-184).
        fn read32(&mut self, addr: u32) -> u32 {
            (0..4).fold(0, |word, i| word | (u32::from(self.read8(addr + i)) << (8 * i)))
        }

        fn write32(&mut self, addr: u32, val: u32) {
            for (i, byte) in (0u32..).zip(val.to_le_bytes()) {
                self.write8(addr + i, byte);
            }
        }

        fn cs_high(&mut self) {
            self.write8(CTRL, SPI_FORCE_SS | SPI_LEVEL_SS);
        }

        /// CTRL = 01, opcode, `width` address bytes MSB first.
        fn command(&mut self, op: u8, addr: u32, width: u32) {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, op);
            for i in (0..width).rev() {
                self.write8(DATA, (addr >> (8 * i)) as u8);
            }
        }

        /// A single-byte command as the firmware sends WREN/WRDI: its own CTRL = 00 frame
        /// (s25fl_l_flash.cc:140-141).
        fn single(&mut self, op: u8) {
            self.write8(CTRL, 0x00);
            self.write8(DATA, op);
        }

        fn sr1(&mut self) -> u8 {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, cmd::RDSR1);
            let sr1 = self.read8(DATA);
            self.cs_high();
            sr1
        }

        /// `W25Q_Flash::tester` (w25q_flash.cc:127-136); `S25FL_Flash::tester` is identical (s25fl_flash.cc:35-45).
        fn tester_w25q(&mut self) -> [u8; 3] {
            self.cs_high();
            self.write8(DATA, 0xFF);
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, cmd::RDID);
            let id = std::array::from_fn(|_| self.read8(DATA));
            self.cs_high();
            id
        }

        /// `S25FLxxxL_Flash::tester` (s25fl_l_flash.cc:34-60); its `wait_ms(1)` calls only touch the ITU.
        fn tester_s25fl_l(&mut self) -> [u8; 3] {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, 0x66);
            self.write8(DATA, 0x99);
            self.cs_high();
            for op in [0x66, 0x99, 0xFF] {
                self.write8(CTRL, SPI_FORCE_SS);
                self.write8(DATA, op);
                self.cs_high();
            }
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, cmd::RDID);
            let id = std::array::from_fn(|_| self.read8(DATA));
            self.cs_high();
            id
        }

        /// `W25Q_Flash::read_dev_addr` (w25q_flash.cc:108-123): `03`, 3-byte address, 8-bit reads.
        fn read_dev_addr(&mut self, addr: u32, len: usize) -> Vec<u8> {
            self.command(cmd::READ, addr, 3);
            let bytes = (0..len).map(|_| self.read8(DATA)).collect();
            self.cs_high();
            bytes
        }

        /// `read_page`: `03` + 3-byte address (w25q_flash.cc:286-305) or `13` + 4-byte address
        /// (s25fl_l_flash.cc:111-131), 64 `DATA_32` reads stored little-endian.
        fn read_page(&mut self, op: u8, width: u32, page: u32) -> Vec<u8> {
            self.command(op, page << 8, width);
            let bytes = (0..64).flat_map(|_| self.read32(DATA).to_le_bytes()).collect();
            self.cs_high();
            bytes
        }

        /// `wait_ready` (w25q_flash.cc:489-506) with a poll budget in place of the ms timer.
        fn wait_ready(&mut self) -> bool {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, cmd::RDSR1);
            let ready = (0..16).any(|_| self.read8(DATA) & 0x01 == 0);
            self.cs_high();
            ready
        }

        /// `write_page` word-aligned path: `02` (w25q_flash.cc:307-341) or `12` (s25fl_l_flash.cc:133-168).
        fn write_page(&mut self, op: u8, width: u32, page: u32, data: &[u8]) -> bool {
            self.single(cmd::WREN);
            self.command(op, page << 8, width);
            for word in data.chunks_exact(4) {
                self.write32(DATA, u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            self.cs_high();
            let ok = self.wait_ready();
            self.single(cmd::WRDI);
            self.cs_high();
            ok
        }

        /// `erase_sector`: `20` (w25q_flash.cc:343-363) or `21` (s25fl_l_flash.cc:170-191).
        fn erase_sector(&mut self, op: u8, width: u32, sector: u32) -> bool {
            self.single(cmd::WREN);
            self.command(op, sector << 12, width);
            self.cs_high();
            let ok = self.wait_ready();
            self.single(cmd::WRDI);
            self.cs_high();
            ok
        }

        /// `W25Q_Flash::read_serial` (w25q_flash.cc:236-249).
        fn read_serial(&mut self) -> [u8; 8] {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, cmd::RUID);
            self.write32(DATA, 0);
            let uid = std::array::from_fn(|_| self.read8(DATA));
            self.cs_high();
            uid
        }
    }

    #[test]
    fn flash_testers_end_with_s25fl_l_claiming_the_chip() {
        let mut rig = Rig::new(&cfg(None, false));
        // `get_flash()` runs all testers in `.init_array` order on every call (flash.cc:16-22, 00 §3 C8).
        for _ in 0..3 {
            let w25q = rig.tester_w25q();
            assert_ne!(w25q[0], 0xEF, "W25Q tester must reject (w25q_flash.cc:139)");
            let s25fl_k = rig.tester_w25q();
            assert!(s25fl_k[0] != 0x01 || s25fl_k[1] != 0x40, "S25FL-K tester must reject (s25fl_flash.cc:51-56)");
            assert_eq!(rig.tester_s25fl_l(), JEDEC_ID);
        }
    }

    #[test]
    fn flash_ctrl_framing_and_register_decode() {
        let mut rig = Rig::new(&cfg(None, false));
        assert_eq!(rig.read8(CTRL), 0x06, "reset: force 0, level 1");
        // CTRL = 03: bytes shift with the chip deselected.
        rig.cs_high();
        rig.write8(DATA, cmd::WREN);
        assert_eq!(rig.read8(DATA), 0xFF);
        assert_eq!(rig.sr1(), 0x00);
        // CTRL = 00: each byte is a frame, so WREN latches.
        rig.single(cmd::WREN);
        assert_eq!(rig.sr1(), 0x02);
        // A single-byte command followed by more bytes in its frame does nothing.
        rig.write8(CTRL, SPI_FORCE_SS);
        rig.write8(DATA, cmd::WRDI);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        assert_eq!(rig.sr1(), 0x02);
        // `66 99` in one frame is no reset; `66` then `99` in separate frames is.
        rig.command(0x66, 0x99, 1);
        rig.cs_high();
        assert_eq!(rig.sr1(), 0x02);
        rig.single(0x66);
        rig.single(0x99);
        assert_eq!(rig.sr1(), 0x00);
        // RATE, CTRL readback, CRC.
        assert_eq!(rig.read8(FLASH_BASE + 4), RATE_FIXED);
        rig.cs_high();
        assert_eq!(rig.read8(CTRL), 0x07);
        rig.write8(CTRL, SPI_FORCE_SS);
        assert_eq!(rig.read8(CTRL), 0x05);
        rig.cs_high();
        assert_eq!(rig.read8(FLASH_BASE + 0x0C), 0x00);
        // Registers repeat every 0x10; DATA spans +0..+3.
        rig.write8(FLASH_BASE + 0x18, SPI_FORCE_SS);
        rig.write8(FLASH_BASE + 0x13, cmd::RDID);
        assert_eq!([rig.read8(FLASH_BASE + 0x21), rig.read8(FLASH_BASE + 0xF2), rig.read8(DATA)], JEDEC_ID);
        rig.write8(FLASH_BASE + 0xFB, SPI_FORCE_SS | SPI_LEVEL_SS);
        assert_eq!(rig.flash().peek8(8), 0x07);
    }

    #[test]
    fn flash_status_never_busy_and_wel_gates_writes() {
        let mut rig = Rig::new(&cfg(None, false));
        let data = pattern(0x0F);
        // SR1 repeats on every byte of the frame, BUSY always 0.
        rig.single(cmd::WREN);
        rig.write8(CTRL, SPI_FORCE_SS);
        rig.write8(DATA, cmd::RDSR1);
        for _ in 0..8 {
            assert_eq!(rig.read8(DATA), 0x02);
        }
        rig.cs_high();
        rig.single(cmd::WRDI);
        // Program without WREN is ignored.
        rig.command(cmd::PP4, 0x58_0000, 4);
        for &byte in &data {
            rig.write8(DATA, byte);
        }
        rig.cs_high();
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5800), vec![0xFF; PAGE_SIZE]);
        // With WREN it programs and clears WEL.
        assert!(rig.write_page(cmd::PP4, 4, 0x5800, &data));
        rig.single(cmd::WREN);
        rig.command(cmd::PP4, 0x58_0000, 4);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        assert_eq!(rig.sr1(), 0x00, "program clears WEL");
        let mut expected = data.clone();
        expected[0] = 0x00;
        // Erase without WREN is ignored.
        rig.command(cmd::SE4, 0x58_0000, 4);
        rig.cs_high();
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5800), expected);
        // An erase frame with extra bytes is ignored even with WEL set.
        rig.single(cmd::WREN);
        rig.command(cmd::SE4, 0x58_0000, 4);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        assert_eq!(rig.sr1(), 0x02);
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5800), expected);
    }

    #[test]
    fn flash_program_erase_read_4byte() {
        let mut rig = Rig::new(&cfg(None, false));
        let (a, b) = (pattern(0x11), pattern(0xA5));
        // FAT sector 0 on the 100T layout (w25q_flash.cc:61).
        assert!(rig.erase_sector(cmd::SE4, 4, 0x580));
        assert!(rig.write_page(cmd::PP4, 4, 0x5801, &a));
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5801), a);
        // NOR programming only clears bits.
        assert!(rig.write_page(cmd::PP4, 4, 0x5801, &b));
        let and: Vec<u8> = a.iter().zip(&b).map(|(x, y)| x & y).collect();
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5801), and);
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5800), vec![0xFF; PAGE_SIZE]);
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5802), vec![0xFF; PAGE_SIZE]);
        // The 3-byte read sees the same array; the 4-byte address's top byte is beyond 16 MiB and wraps.
        assert_eq!(rig.read_dev_addr(0x58_0100, 4), and[..4]);
        rig.command(cmd::READ4, 0x0158_0100, 4);
        assert_eq!(rig.read32(DATA).to_le_bytes(), and[..4]);
        rig.cs_high();
        assert!(rig.erase_sector(cmd::SE4, 4, 0x580));
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5801), vec![0xFF; PAGE_SIZE]);
        // Fast read has one dummy byte.
        assert!(rig.write_page(cmd::PP4, 4, 0x5801, &a));
        rig.command(cmd::FAST_READ4, 0x58_0100, 4);
        rig.read8(DATA);
        assert_eq!([rig.read8(DATA), rig.read8(DATA)], a[..2]);
        rig.cs_high();
    }

    #[test]
    fn flash_program_erase_read_3byte() {
        let mut rig = Rig::new(&cfg(None, false));
        let (a, b) = (pattern(0x3C), pattern(0x77));
        // `write_config_page` for config page 5: erase, then two pages (w25q_flash.cc:269-277).
        assert!(rig.erase_sector(cmd::SE, 3, 0xFED));
        assert!(rig.write_page(cmd::PP, 3, 0xFED0, &a));
        assert!(rig.write_page(cmd::PP, 3, 0xFED1, &b));
        let page = rig.read_dev_addr(0xFE_D000, 512);
        assert_eq!(page[..256], a[..]);
        assert_eq!(page[256..], b[..]);
        assert_eq!(rig.read_page(cmd::READ, 3, 0xFED1), b);
        // Sequential reads wrap at the end of the array.
        assert!(rig.write_page(cmd::PP, 3, 0, &a));
        assert!(rig.write_page(cmd::PP, 3, 0xFFFF, &b));
        assert_eq!(rig.read_dev_addr(0xFF_FFFE, 4), [b[254], b[255], a[0], a[1]]);
        assert!(rig.erase_sector(cmd::SE, 3, 0xFED));
        assert_eq!(rig.read_dev_addr(0xFE_D000, 512), vec![0xFF; 512]);
        // Other erase sizes.
        rig.single(cmd::WREN);
        rig.command(cmd::BE, 0x00_8000, 3);
        rig.cs_high();
        assert_eq!(rig.read_page(cmd::READ, 3, 0), vec![0xFF; PAGE_SIZE]);
        assert_eq!(rig.read_dev_addr(0xFF_FFFF, 1), [b[255]]);
        rig.single(cmd::WREN);
        rig.single(cmd::CE);
        assert_eq!(rig.read_dev_addr(0xFF_FF00, 256), vec![0xFF; PAGE_SIZE]);
    }

    #[test]
    fn flash_page_program_wraps_inside_its_page() {
        let mut rig = Rig::new(&cfg(None, false));
        rig.single(cmd::WREN);
        rig.command(cmd::PP4, 0x1_00FE, 4);
        for byte in [0x01, 0x02, 0x03, 0x04] {
            rig.write8(DATA, byte);
        }
        rig.cs_high();
        let page = rig.read_page(cmd::READ4, 4, 0x100);
        assert_eq!([page[0xFE], page[0xFF], page[0], page[1], page[2]], [0x01, 0x02, 0x03, 0x04, 0xFF]);
        assert_eq!(rig.read_dev_addr(0x1_0100, 1), [0xFF]);
    }

    #[test]
    fn flash_data32_is_lsb_first() {
        let mut rig = Rig::new(&cfg(None, false));
        // Unaligned `write_page` buffers program with 8-bit writes (s25fl_l_flash.cc:150-155).
        rig.single(cmd::WREN);
        rig.command(cmd::PP4, 0x1_0000, 4);
        for byte in [0x11, 0x22, 0x33, 0x44] {
            rig.write8(DATA, byte);
        }
        rig.cs_high();
        rig.command(cmd::READ4, 0x1_0000, 4);
        assert_eq!(rig.read32(DATA), 0x4433_2211);
        rig.cs_high();
        // A 32-bit write sends bits 7:0 first (06 H9).
        rig.single(cmd::WREN);
        rig.command(cmd::PP4, 0x1_0100, 4);
        rig.write32(DATA, 0xDDCC_BBAA);
        rig.cs_high();
        assert_eq!(rig.read_dev_addr(0x1_0100, 4), [0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn flash_uid_is_stable() {
        let mut rig = Rig::new(&cfg(None, false));
        let uid = rig.read_serial();
        assert_eq!(uid, UID);
        assert_eq!(rig.read_serial(), uid);
        assert_eq!(Rig::new(&cfg(None, false)).read_serial(), uid);
        // MAC octets 3..5 (rmii_interface.cc:126-128) are not all zero.
        assert_ne!([uid[1] ^ uid[5], uid[2] ^ uid[6], uid[3] ^ uid[7]], [0; 3]);
    }

    #[test]
    fn flash_uid_can_be_replaced() {
        let mut rig = Rig::new(&cfg(None, false));
        let uid = *b"U\x12\x34\x56\0\0\0\0";
        rig.map.get_mut::<SpiFlash>().unwrap().set_unique_id(uid);
        assert_eq!(rig.read_serial(), uid);
    }

    #[test]
    fn flash_image_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let (fat, config) = (pattern(0x42), pattern(0x99));
        {
            let mut rig = Rig::new(&cfg(Some(path.clone()), false));
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(bytes.len(), FLASH_SIZE, "missing image is created at full size");
            assert!(bytes.iter().all(|&b| b == 0xFF), "and erased");
            assert!(rig.write_page(cmd::PP4, 4, 0x5800, &fat));
            assert!(rig.erase_sector(cmd::SE, 3, 0xFE8));
            assert!(rig.write_page(cmd::PP, 3, 0xFE80, &config));
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[0x58_0000..0x58_0100], fat[..]);
        assert_eq!(bytes[CONFIG_BASE..CONFIG_BASE + PAGE_SIZE], config[..]);
        let mut rig = Rig::new(&cfg(Some(path.clone()), false));
        assert_eq!(rig.read_page(cmd::READ4, 4, 0x5800), fat);
        assert_eq!(rig.read_dev_addr(CONFIG_BASE as u32, PAGE_SIZE), config);
    }

    #[test]
    fn flash_image_write_back_is_debounced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let on_disk = |path: &Path| std::fs::read(path).unwrap()[0x58_0000..0x58_0100].to_vec();
        let mut rig = Rig::new(&cfg(Some(path.clone()), false));
        assert_eq!(rig.flash().next_event(), None);
        rig.now = 1_000;
        let data = pattern(0x33);
        assert!(rig.write_page(cmd::PP4, 4, 0x5800, &data));
        let poll = rig.flash().next_event().expect("a program schedules the write-back");
        assert_eq!(poll, 1_000 + FLUSH_POLL_CLOCKS);
        rig.flash().service(Instant::now(), poll);
        assert_eq!(on_disk(&path), vec![0xFF; PAGE_SIZE], "not written before the debounce delay");
        assert_eq!(rig.flash().next_event(), Some(poll + FLUSH_POLL_CLOCKS));
        rig.flash().service(Instant::now() + FLUSH_DELAY, poll + FLUSH_POLL_CLOCKS);
        assert_eq!(on_disk(&path), data);
        assert_eq!(rig.flash().next_event(), None);
    }

    #[test]
    fn flash_image_short_file_is_padded_and_oversized_refused() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.bin");
        std::fs::write(&short, [0x12, 0x34]).unwrap();
        let flash = SpiFlash::open(&short, FlashChip::S25fl128l).unwrap();
        assert_eq!(flash.chip.mem[..3], [0x12, 0x34, 0xFF]);
        drop(flash);
        let bytes = std::fs::read(&short).unwrap();
        assert_eq!(bytes.len(), FLASH_SIZE);
        assert_eq!(bytes[..3], [0x12, 0x34, 0xFF]);
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![0u8; FLASH_SIZE + 1]).unwrap();
        assert!(SpiFlash::open(&big, FlashChip::S25fl128l).is_err());
    }

    #[test]
    fn flash_overlay_ui_seed_parses_back() {
        let mut rig = Rig::new(&cfg(None, true));
        // `ConfigManager::register_store` compares each page's id (config.cc:127-137).
        let ids: Vec<u32> = (0..CONFIG_PAGES as u32)
            .map(|p| {
                let id = rig.read_dev_addr(CONFIG_BASE as u32 + p * 0x1000, 4);
                u32::from_le_bytes([id[0], id[1], id[2], id[3]])
            })
            .collect();
        assert_eq!(ids[0], USERIF_STORE_ID);
        assert!(ids[1..].iter().all(|&id| id == 0xFFFF_FFFF), "other stores stay absent: {ids:08x?}");
        // `ConfigPage::read` takes the 512-byte page (config.cc:448-449, w25q_flash.cc:251-254).
        let page = rig.read_dev_addr(CONFIG_BASE as u32, 512);
        assert_eq!(page[..4], *b".NEG");
        let items = unpack(&page);
        assert_eq!(items, vec![(CFG_USERIF_ITYPE, CFG_TYPE_ENUM, vec![1])]);
        // `ConfigItem::unpack`: big-endian over len, inside min..=max = 0..=1 (config.cc:683-691,
        // userinterface.cc:123).
        let value = items[0].2.iter().fold(0u32, |v, &b| (v << 8) | u32::from(b));
        assert_eq!(value, 1);
        assert!(page[8..].iter().all(|&b| b == 0xFF));

        let mut plain = Rig::new(&cfg(None, false));
        for p in 0..CONFIG_PAGES as u32 {
            assert_eq!(plain.read_dev_addr(CONFIG_BASE as u32 + p * 0x1000, 4), [0xFF; 4]);
        }
    }

    #[test]
    fn flash_overlay_ui_seed_keeps_existing_pages() {
        let mut flash = SpiFlash::volatile(FlashChip::S25fl128l);
        // Page 0 already claimed by the U64 stores (u64_config.cc:99).
        flash.chip.mem[CONFIG_BASE..CONFIG_BASE + 4].copy_from_slice(&0x5536_3443u32.to_le_bytes());
        assert!(flash.seed_overlay_ui());
        let page1 = CONFIG_BASE + SECTOR_SIZE;
        assert_eq!(flash.chip.mem[page1..page1 + 8], [0x2E, 0x4E, 0x45, 0x47, 0x08, 0x02, 0x01, 0x01]);
        // A saved "Freeze" choice is not overwritten.
        flash.chip.mem[page1 + 7] = 0x00;
        assert!(!flash.seed_overlay_ui());
        assert_eq!(flash.chip.mem[page1 + 7], 0x00);
        // No erased page left: nothing to claim.
        let mut full = SpiFlash::volatile(FlashChip::S25fl128l);
        for p in 0..CONFIG_PAGES {
            let at = CONFIG_BASE + p * SECTOR_SIZE;
            full.chip.mem[at..at + 4].copy_from_slice(&(0x100 + p as u32).to_le_bytes());
        }
        assert!(!full.seed_overlay_ui());
    }

    #[test]
    fn flash_overlay_ui_seed_keeps_the_firmware_written_store() {
        // Page 0 of run/flash.bin after scripts/smoke-flash-1.ctl saved Color Scheme = C128 Style: the firmware
        // packs every item in definition order (config.cc:305-325, userinterface.cc:121-140), `07 03 00` being the
        // empty Home Directory string. docs/status/storage.md.
        #[rustfmt::skip]
        let saved = [
            0x2E, 0x4E, 0x45, 0x47,
            0x08, 0x02, 0x01, 0x01, 0x0D, 0x02, 0x01, 0x00, 0x0E, 0x02, 0x01, 0x02, 0x06, 0x02, 0x01, 0x00,
            0x07, 0x03, 0x00, 0x0A, 0x02, 0x01, 0x01, 0x0B, 0x02, 0x01, 0x01, 0x0C, 0x02, 0x01, 0x00,
            0x0F, 0x02, 0x01, 0x01, 0x10, 0x02, 0x01, 0x01, 0xFF,
        ];
        let mut flash = SpiFlash::volatile(FlashChip::S25fl128l);
        flash.chip.mem[CONFIG_BASE..CONFIG_BASE + saved.len()].copy_from_slice(&saved);
        let before = flash.chip.mem.clone();
        assert!(!flash.seed_overlay_ui(), "a saved user-interface store is never reseeded");
        assert!(flash.chip.mem == before);

        let page = &flash.chip.mem[CONFIG_BASE..CONFIG_BASE + 512];
        let items = unpack(page);
        assert_eq!(items.len(), 10);
        let value = |id: u8| items.iter().find(|item| item.0 == id).map(|item| item.2.clone());
        assert_eq!(value(CFG_USERIF_ITYPE), Some(vec![ITYPE_OVERLAY]), "overlay UI kept");
        // `CFG_USERIF_COLORSCHEME` 0x0E (userinterface.h:44) = 2, "C128 Style" (userinterface.cc:114,126).
        assert_eq!(value(0x0E), Some(vec![2]));
        assert_eq!(value(0x07), Some(vec![]), "empty string item");
    }

    #[test]
    fn flash_overlay_ui_seed_is_written_to_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let rig = Rig::new(&cfg(Some(path.clone()), true));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[CONFIG_BASE..CONFIG_BASE + 8], [0x2E, 0x4E, 0x45, 0x47, 0x08, 0x02, 0x01, 0x01]);
        drop(rig);
        let mut rig = Rig::new(&cfg(Some(path), true));
        assert_eq!(rig.read_dev_addr(CONFIG_BASE as u32 + 0x1000, 4), [0xFF; 4], "seeded only once");
    }

    fn record(page: u32, id: u8, kind: u8, payload: &[u8]) -> Record {
        Record { page, id, kind, payload: payload.to_vec(), text: String::new() }
    }

    fn config_page(flash: &SpiFlash, p: usize) -> &[u8] {
        &flash.chip.mem[CONFIG_BASE + p * SECTOR_SIZE..CONFIG_BASE + p * SECTOR_SIZE + CONFIG_PAGE_SIZE]
    }

    /// S21 §5: records land in the page with the id, replacing in place and dropping a later duplicate; new ids go
    /// before the end marker; a store without a page gets the first erased one.
    #[test]
    fn settings_edit_the_existing_page_and_claim_an_erased_one() {
        let mut flash = SpiFlash::volatile(FlashChip::S25fl128l);
        assert!(flash.seed_overlay_ui());
        // A C64 page as the firmware writes it, with a duplicate REU record at the end.
        let c64 = 0x4336_3420u32;
        let base = CONFIG_BASE + SECTOR_SIZE;
        let mut page = c64.to_le_bytes().to_vec();
        page.extend_from_slice(&[0xC3, 0x02, 0x01, 0x00, 0xE1, 0x07, 0x03, b'a', b'b', b'c']);
        page.extend_from_slice(&[0xC3, 0x02, 0x01, 0x00, 0xFF]);
        flash.chip.mem[base..base + page.len()].copy_from_slice(&page);
        flash.chip.mem[base + 0x800] = 0x5A; // beyond the logical page
        flash
            .write_settings(&[
                record(c64, 0xC3, 0x02, &[1]),
                record(c64, 0x71, 0x02, &[1]),
                record(USERIF_STORE_ID, CFG_USERIF_ITYPE, CFG_TYPE_ENUM, &[0]),
                record(0x4E45_5400, 0x10, 0x03, b"host"),
            ])
            .unwrap();
        assert_eq!(
            unpack(config_page(&flash, 1)),
            vec![(0xC3, 0x02, vec![1]), (0xE1, 0x07, b"abc".to_vec()), (0x71, 0x02, vec![1])]
        );
        assert_eq!(flash.chip.mem[base + 0x800], 0x5A, "the rest of the sector is kept");
        assert_eq!(unpack(config_page(&flash, 0)), vec![(CFG_USERIF_ITYPE, CFG_TYPE_ENUM, vec![0])], "file beats seed");
        let net = config_page(&flash, 2);
        assert_eq!(net[..4], 0x4E45_5400u32.to_le_bytes());
        assert_eq!(unpack(net), vec![(0x10, 0x03, b"host".to_vec())]);
        assert!(net[4 + 7..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn settings_that_do_not_fit_are_refused() {
        let mut flash = SpiFlash::volatile(FlashChip::S25fl128l);
        let long = vec![b'x'; 255];
        let records: Vec<Record> = (0..2).map(|i| record(0x4E45_5400, i, 0x03, &long)).collect();
        let err = flash.write_settings(&records).unwrap_err().to_string();
        assert!(err.contains("a config page holds 512"), "{err}");

        for p in 0..CONFIG_PAGES {
            flash.chip.mem[CONFIG_BASE + p * SECTOR_SIZE] = p as u8;
        }
        let err = flash.write_settings(&[record(0x4E45_5400, 0x10, 0x03, b"h")]).unwrap_err().to_string();
        assert!(err.contains("all 24 config pages are taken"), "{err}");
    }

    #[test]
    fn settings_written_to_the_image_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let mut flash = SpiFlash::open(&path, FlashChip::S25fl128l).unwrap();
        flash.write_settings(&[record(0x4336_3420, 0xC3, 0x02, &[1])]).unwrap();
        flash.flush().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[CONFIG_BASE..CONFIG_BASE + 9], [0x20, 0x34, 0x36, 0x43, 0xC3, 0x02, 0x01, 0x01, 0xFF]);
        // Writing the same again changes nothing.
        let mut again = SpiFlash::open(&path, FlashChip::S25fl128l).unwrap();
        again.write_settings(&[record(0x4336_3420, 0xC3, 0x02, &[1])]).unwrap();
        assert!(again.image.as_ref().unwrap().dirty.iter().all(|&d| !d));
    }

    // ---- S47: the W25Q128JV ----

    impl Rig {
        /// `05`, `35` or `15`, one byte.
        fn read_sr(&mut self, op: u8) -> u8 {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, op);
            let v = self.read8(DATA);
            self.cs_high();
            v
        }

        /// A status register write as flashlock.cc sends it: the enable (`06` or `50`) in a frame of its own, then
        /// the opcode and its bytes in one frame, never reading the data register in it.
        fn write_sr(&mut self, enable: u8, op: u8, bytes: &[u8]) {
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, enable);
            self.cs_high();
            self.write8(CTRL, SPI_FORCE_SS);
            self.write8(DATA, op);
            for &b in bytes {
                self.write8(DATA, b);
            }
            self.cs_high();
        }

        fn status(&mut self) -> [u8; 3] {
            [self.read_sr(cmd::RDSR1), self.read_sr(cmd::W_RDSR2), self.read_sr(cmd::W_RDSR3)]
        }
    }

    /// T1: `W25Q_Flash::tester` takes the chip; the S25FL-K and S25FL-L testers, which run after it, would reject it.
    #[test]
    fn w25q_identity_for_the_testers() {
        let mut rig = Rig::new(&w25q_cfg(None, false));
        let id = rig.tester_w25q();
        assert_eq!(id, [0xEF, 0x40, 0x18], "w25q_flash.cc:138-174: Winbond, type 0x40, 128 Mbit");
        assert!(id[0] != 0x01, "S25FL-K tester rejects (s25fl_flash.cc:51-56)");
        assert_ne!(rig.tester_s25fl_l(), [0x01, 0x60, 0x18], "S25FL-L tester rejects");
        assert_eq!(rig.read_serial(), UID);
        assert_eq!(rig.flash().chip(), FlashChip::W25q128);
        // And the other way round: the W25Q tester rejects the S25FL128L.
        assert_ne!(Rig::new(&cfg(None, false)).tester_w25q()[0], 0xEF);
    }

    /// T2: the commands the W25Q driver sends (w25q_flash.cc), 3-byte addresses only.
    #[test]
    fn w25q_read_program_erase() {
        let mut rig = Rig::new(&w25q_cfg(None, false));
        let (a, b) = (pattern(0x21), pattern(0x84));
        // `write_config_page` for config page 3: erase, then two pages (w25q_flash.cc:269-277).
        assert!(rig.erase_sector(cmd::SE, 3, 0xFF3));
        assert!(rig.write_page(cmd::PP, 3, 0xFF30, &a));
        assert!(rig.write_page(cmd::PP, 3, 0xFF31, &b));
        assert_eq!(rig.read_page(cmd::READ, 3, 0xFF30), a);
        assert_eq!(rig.read_dev_addr(0xFF_3100, 256), b);
        // Fast read: one dummy byte.
        rig.command(cmd::FAST_READ, 0xFF_3000, 3);
        rig.read8(DATA);
        assert_eq!([rig.read8(DATA), rig.read8(DATA)], a[..2]);
        rig.cs_high();
        // 32 KiB and 64 KiB erases.
        rig.single(cmd::WREN);
        rig.command(cmd::HBE, 0xFF_0000, 3);
        rig.cs_high();
        assert_eq!(rig.read_page(cmd::READ, 3, 0xFF30), vec![0xFF; PAGE_SIZE]);
        assert!(rig.write_page(cmd::PP, 3, 0x0010, &a));
        assert!(rig.write_page(cmd::PP, 3, 0x0100, &b));
        rig.single(cmd::WREN);
        rig.command(cmd::BE, 0x00_8000, 3);
        rig.cs_high();
        assert_eq!(rig.read_page(cmd::READ, 3, 0x0010), vec![0xFF; PAGE_SIZE], "block 0x000000-0x00FFFF");
        assert_eq!(rig.read_page(cmd::READ, 3, 0x0100), b, "0x010000 is the next block");
        // Both chip erases.
        for ce in [cmd::CE, cmd::CE2] {
            assert!(rig.write_page(cmd::PP, 3, 0x0100, &a));
            rig.single(cmd::WREN);
            rig.single(ce);
            assert_eq!(rig.read_page(cmd::READ, 3, 0x0100), vec![0xFF; PAGE_SIZE]);
            assert_eq!(rig.sr1(), 0x00, "chip erase clears WEL");
        }
        // The S25FL128L's 4-byte forms are no W25Q commands: nothing is written or erased.
        assert!(rig.write_page(cmd::PP, 3, 0x0100, &a));
        rig.single(cmd::WREN);
        rig.command(cmd::PP4, 0x0000_0200, 4);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        rig.command(cmd::SE4, 0x0000_0000, 4);
        rig.cs_high();
        assert_eq!(rig.sr1(), 0x02, "WEL still set: neither frame was a command");
        assert_eq!(rig.read_page(cmd::READ, 3, 0x0100), a);
        assert_eq!(rig.read_dev_addr(0x200, 1), [0xFF]);
    }

    /// The datasheet's table 7.1.8 (WPS = 0, CMP = 0): (SEC, TB, BP) → protected range.
    #[rustfmt::skip]
    const CMP0: [(u8, u8, u8, usize, usize); 22] = [
        (0, 0, 0b001, 0xFC_0000, 0x100_0000), (0, 0, 0b010, 0xF8_0000, 0x100_0000), (0, 0, 0b011, 0xF0_0000, 0x100_0000),
        (0, 0, 0b100, 0xE0_0000, 0x100_0000), (0, 0, 0b101, 0xC0_0000, 0x100_0000), (0, 0, 0b110, 0x80_0000, 0x100_0000),
        (0, 1, 0b001, 0, 0x04_0000), (0, 1, 0b010, 0, 0x08_0000), (0, 1, 0b011, 0, 0x10_0000),
        (0, 1, 0b100, 0, 0x20_0000), (0, 1, 0b101, 0, 0x40_0000), (0, 1, 0b110, 0, 0x80_0000),
        (1, 0, 0b001, 0xFF_F000, 0x100_0000), (1, 0, 0b010, 0xFF_E000, 0x100_0000), (1, 0, 0b011, 0xFF_C000, 0x100_0000),
        (1, 0, 0b100, 0xFF_8000, 0x100_0000), (1, 0, 0b101, 0xFF_8000, 0x100_0000),
        (1, 1, 0b001, 0, 0x1000), (1, 1, 0b010, 0, 0x2000), (1, 1, 0b011, 0, 0x4000),
        (1, 1, 0b100, 0, 0x8000), (1, 1, 0b101, 0, 0x8000),
    ];

    /// T3: every row of tables 7.1.8 and 7.1.9 (CMP = 1 protects the complement: e.g. SEC 0 TB 1 BP 101 →
    /// 0x400000-0xFFFFFF, SEC 1 TB 0 BP 001 → 0x000000-0xFFEFFF).
    #[test]
    fn w25q_protection_table() {
        let sr1 = |sec: u8, tb: u8, bp: u8| sec << 6 | tb << 5 | bp << 2;
        for sec in 0..2 {
            for tb in 0..2 {
                assert_eq!(w25q_protected(sr1(sec, tb, 0), 0), 0..0, "BP 000: none");
                assert_eq!(w25q_protected(sr1(sec, tb, 7), 0), 0..FLASH_SIZE, "BP 111: all");
                assert_eq!(w25q_protected(sr1(sec, tb, 0), sr::CMP), 0..FLASH_SIZE, "CMP 1, BP 000: all");
                assert!(w25q_protected(sr1(sec, tb, 7), sr::CMP).is_empty(), "CMP 1, BP 111: none");
            }
        }
        for (sec, tb, bp, start, end) in CMP0 {
            assert_eq!(w25q_protected(sr1(sec, tb, bp), 0), start..end, "SEC {sec} TB {tb} BP {bp:03b}");
            let complement = if start == 0 { end..FLASH_SIZE } else { 0..start };
            assert_eq!(w25q_protected(sr1(sec, tb, bp), sr::CMP), complement, "CMP 1, SEC {sec} TB {tb} BP {bp:03b}");
        }
        assert_eq!(w25q_protected(sr1(0, 1, 0b101), sr::CMP), 0x40_0000..FLASH_SIZE);
        assert_eq!(w25q_protected(sr1(1, 0, 0b001), sr::CMP), 0..0xFF_F000);
        // SEC 1 with BP 110 is in neither table: 32 KiB like BP 10x.
        assert_eq!(w25q_protected(sr1(1, 1, 0b110), 0), 0..0x8000);
    }

    /// T3: status register writes after `06` (both copies, WEL cleared) and after `50` (the volatile copy only, WEL
    /// untouched), read-only bits, the one-time LB bits, a reset back to the non-volatile copy, and a protected
    /// program and erase.
    #[test]
    fn w25q_status_registers_and_protection() {
        let mut rig = Rig::new(&w25q_cfg(None, false));
        assert_eq!(rig.status(), [0, 0, 0], "power-up: all 0 (S47 D3)");
        // Without an enable nothing changes.
        rig.write8(CTRL, SPI_FORCE_SS);
        rig.write8(DATA, cmd::WRSR1);
        rig.write8(DATA, 0x34);
        rig.cs_high();
        assert_eq!(rig.status(), [0, 0, 0]);
        // Non-volatile: SR1, then SR1 and SR2 in one `01`, SR3; read-only and reserved bits stay 0.
        rig.write_sr(cmd::WREN, cmd::WRSR1, &[0xFF]);
        assert_eq!(rig.status(), [0xFC, 0, 0], "SRP SEC TB BP2:0 written, WEL cleared, BUSY 0");
        rig.write_sr(cmd::WREN, cmd::WRSR1, &[0x00, 0xFE]);
        assert_eq!(rig.status(), [0x00, 0x7A, 0], "SR2: CMP LB3:1 QE written, SUS and S10 not (SRL left 0 here)");
        rig.write_sr(cmd::WREN, cmd::W_WRSR2, &[0x00]);
        assert_eq!(rig.read_sr(cmd::W_RDSR2), 0x38, "LB3:1 are one-time: they stay 1");
        rig.write_sr(cmd::WREN, cmd::W_WRSR3, &[0xFF]);
        assert_eq!(rig.read_sr(cmd::W_RDSR3), 0x64, "DRV1:0 WPS");
        // A frame of the wrong length is not executed.
        rig.write_sr(cmd::WREN, cmd::W_WRSR3, &[0x00, 0x00]);
        assert_eq!(rig.read_sr(cmd::W_RDSR3), 0x64);
        assert_eq!(rig.sr1() & 0x02, 0x02, "and WEL stays");
        rig.single(cmd::WRDI);
        // Volatile: after `50`, WEL stays 0 and a reset brings the non-volatile value back.
        rig.write_sr(cmd::W_VWREN, cmd::WRSR1, &[0x34]);
        assert_eq!(rig.read_sr(cmd::RDSR1), 0x34);
        rig.write_sr(cmd::W_VWREN, cmd::W_WRSR3, &[0x00]);
        rig.single(cmd::RSTEN);
        rig.single(cmd::RST);
        assert_eq!(rig.status(), [0x00, 0x38, 0x64], "reset: the non-volatile copies");
        // `50` is good for one write only.
        rig.write_sr(cmd::W_VWREN, cmd::WRSR1, &[0x34]);
        rig.write8(CTRL, SPI_FORCE_SS);
        rig.write8(DATA, cmd::WRSR1);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        assert_eq!(rig.read_sr(cmd::RDSR1), 0x34);
        // TB 1, BP 101: 0x000000-0x3FFFFF. Program and erase there are ignored; outside they work.
        let data = pattern(0x5A);
        assert!(rig.write_page(cmd::PP, 3, 0x1000, &data));
        assert_eq!(rig.read_page(cmd::READ, 3, 0x1000), vec![0xFF; PAGE_SIZE]);
        rig.single(cmd::WREN);
        rig.command(cmd::PP, 0x3F_FF00, 3);
        rig.write8(DATA, 0x00);
        rig.cs_high();
        assert_eq!(rig.read_dev_addr(0x3F_FF00, 1), [0xFF]);
        assert_eq!(rig.sr1(), 0x36, "an ignored program leaves WEL set");
        rig.single(cmd::WRDI);
        assert!(rig.write_page(cmd::PP, 3, 0x4000, &data));
        assert_eq!(rig.read_page(cmd::READ, 3, 0x4000), data);
        // A 64 KiB erase at 0x3F0000 and the chip erase touch the protected range; a sector erase at 0x400000 not.
        rig.single(cmd::WREN);
        rig.command(cmd::BE, 0x3F_0000, 3);
        rig.cs_high();
        rig.single(cmd::WREN);
        rig.single(cmd::CE);
        assert_eq!(rig.read_page(cmd::READ, 3, 0x4000), data);
        rig.single(cmd::WRDI);
        assert!(rig.erase_sector(cmd::SE, 3, 0x400));
        assert_eq!(rig.read_page(cmd::READ, 3, 0x4000), vec![0xFF; PAGE_SIZE]);
    }

    /// T3: MultiCore's T152 lock (flashlock.cc `flashlock_apply`): a volatile SR1 write of TB + BP 101, then a
    /// volatile SR2 write with SRL. It reads back `34 01`, a program at 0x100000 is ignored, and no status register
    /// write gets through any more until a new machine; an IPROG's controller reset does not lift it.
    #[test]
    fn w25q_flashlock_holds_until_power_off() {
        let mut rig = Rig::new(&w25q_cfg(None, false));
        let cur1 = rig.read_sr(cmd::RDSR1);
        rig.write_sr(cmd::W_VWREN, cmd::WRSR1, &[(cur1 & !0x7C | 0x34) & !0x80]);
        assert_eq!(rig.read_sr(cmd::RDSR1) & 0x80, 0, "SRP clear");
        let cur2 = rig.read_sr(cmd::W_RDSR2);
        rig.write_sr(cmd::W_VWREN, cmd::W_WRSR2, &[cur2 & !0x40 | 0x01]);
        assert_eq!([rig.read_sr(cmd::RDSR1), rig.read_sr(cmd::W_RDSR2)], [0x34, 0x01], "\"status 34 01\"");
        assert!(rig.write_page(cmd::PP, 3, 0x1000, &pattern(1)));
        assert_eq!(rig.read_page(cmd::READ, 3, 0x1000), vec![0xFF; PAGE_SIZE]);
        // Gideon's protect_disable (w25q_flash.cc:390-424): `06 01 00`, `06 31 00` change nothing now.
        rig.write_sr(cmd::WREN, cmd::WRSR1, &[0x00]);
        rig.write_sr(cmd::WREN, cmd::W_WRSR2, &[0x00]);
        rig.write_sr(cmd::W_VWREN, cmd::WRSR1, &[0x00]);
        assert_eq!([rig.read_sr(cmd::RDSR1) & 0xFC, rig.read_sr(cmd::W_RDSR2)], [0x34, 0x01]);
        rig.flash().reset();
        assert_eq!([rig.read_sr(cmd::RDSR1) & 0xFC, rig.read_sr(cmd::W_RDSR2)], [0x34, 0x01], "IPROG: the chip keeps power");
        // Power-off: a new machine starts at 0.
        assert_eq!(Rig::new(&w25q_cfg(None, false)).status(), [0, 0, 0]);
    }

    /// T4: the W25Q driver's 16 config pages at 0xFF0000 (w25q_flash.cc:256-267): the overlay seed, the page list,
    /// the monitor's records and the settings writer use them, and leave 0xFE8000 alone.
    #[test]
    fn w25q_config_pages_at_ff0000() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let mut rig = Rig::new(&w25q_cfg(Some(path.clone()), true));
        assert_eq!(rig.read_dev_addr(0xFF_0000, 8), [0x2E, 0x4E, 0x45, 0x47, 0x08, 0x02, 0x01, 0x01]);
        assert_eq!(rig.read_dev_addr(CONFIG_BASE as u32, 4), [0xFF; 4], "not the S25FL128L's page 0");
        let flash = rig.flash();
        assert_eq!(flash.config_pages(), vec![USERIF_STORE_ID]);
        assert_eq!(flash.config_page(USERIF_STORE_ID), Some(vec![(CFG_USERIF_ITYPE, CFG_TYPE_ENUM, vec![1])]));
        flash.write_settings(&[record(0x4336_3420, 0xC3, 0x02, &[1])]).unwrap();
        assert_eq!(flash.config_pages(), vec![USERIF_STORE_ID, 0x4336_3420]);
        assert_eq!(flash.chip.mem[0xFF_1000..0xFF_1009], [0x20, 0x34, 0x36, 0x43, 0xC3, 0x02, 0x01, 0x01, 0xFF]);
        // 16 pages: the 17th store finds none.
        for p in 2..16 {
            let at = FlashChip::W25q128.config_page_at(p);
            flash.chip.mem[at..at + 4].copy_from_slice(&(0x100 + p as u32).to_le_bytes());
        }
        let err = flash.write_settings(&[record(0x4E45_5400, 0x10, 0x03, b"h")]).unwrap_err().to_string();
        assert!(err.contains("all 16 config pages are taken"), "{err}");
        assert!(flash.chip.mem[0xFE_8000..0xFF_0000].iter().all(|&b| b == 0xFF));
        assert_eq!((FlashChip::W25q128.config_page_at(15), FlashChip::S25fl128l.config_page_at(23)), (0xFF_F000, 0xFF_F000));
    }
}
