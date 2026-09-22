//! SD card over SPI, window 0x10060000-0x100600FF: Gideon's `spi_peripheral_io` register model plus an
//! image-backed SDHC card in SPI mode. Spec: docs/specs/S09-sd-card.md, docs/hw/07-sd-card-filesystems.md.
//!
//! The block decodes only address bits 3:2 (spi_peripheral_io.vhd:91,116), so DATA/SPEED/CTRL/CRC occupy
//! four bytes each and repeat every 16 bytes; offsets +1..+3 alias DATA, which is how `SDIO_DATA_32` clocks
//! four transfers (io/sd_card/sdio.h:7-12, 07 §Address map). Every DATA access is one synchronous
//! full-duplex byte: the VHDL `busy` flag only gates the register ack and an activity LED, so there is no
//! status bit to model and no interrupt (07 §Interrupts). Card responses never wait for time to pass,
//! because the firmware polls with interrupts off (07 H7, H8).
//!
//! Without an image the SD block reads 0x00 everywhere, like `io_dummy` (io_dummy.vhd:16): the
//! "SD Card Manager" task sees CD = 0 on its 100 ms poll and never touches DATA (07 H1, 00-memory-map §2 C3).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

/// `SDCARD_BASE` (system/iomap.h:23).
pub const SDCARD_BASE: u32 = 0x1006_0000;
pub const SDCARD_SIZE: u32 = 0x100;
/// `SD_SECTOR_SIZE` (io/sd_card/sd_card.h:23); SDHC block length is fixed.
pub const SECTOR_SIZE: usize = 512;

/// `rate` reset value `g_init_rate` (spi_peripheral_io.vhd:155); SPEED reads back 0xF4.
const RATE_RESET: u16 = 500;

/// R1 bits: in idle state, illegal command, parameter error (sd_card.cc:459-472).
const R1_IDLE: u8 = 0x01;
const R1_ILLEGAL: u8 = 0x04;
const R1_PARAM: u8 = 0x40;
/// Start tokens: single-block read/write and CID/CSD, multi-block write, stop transmission.
const TOKEN_START: u8 = 0xFE;
const TOKEN_START_MULTI: u8 = 0xFC;
const TOKEN_STOP_TRAN: u8 = 0xFD;
/// Data-response tokens `xxx0sss1`: accepted, write error.
const DATA_ACCEPTED: u8 = 0x05;
const DATA_WRITE_ERROR: u8 = 0x0D;
/// Data error tokens `0000eeee`: generic error, out of range.
const ERROR_TOKEN: u8 = 0x01;
const ERROR_TOKEN_RANGE: u8 = 0x08;

/// CID without its CRC: MID 0, OID "UE", PNM "UE2SD", PRV 1.0, PSN 1, MDT 2026-01. Only printed by the
/// firmware (sd_card.cc:606-613).
const CID: [u8; 15] = [0x00, b'U', b'E', b'U', b'E', b'2', b'S', b'D', 0x10, 0x00, 0x00, 0x00, 0x01, 0x01, 0xA1];

/// One CRC7 step over a byte, MSB first, polynomial x^7 + x^3 + 1 (spi.vhd:42-47). The same CRC protects
/// SD command frames and the CID/CSD registers.
fn crc7(mut crc: u8, byte: u8) -> u8 {
    for bit in (0..8).rev() {
        let feedback = ((byte >> bit) ^ (crc >> 6)) & 1;
        crc = ((crc << 1) & 0x7F) ^ (feedback * 0x09);
    }
    crc
}

/// CRC16-CCITT (x^16 + x^12 + x^5 + 1, init 0) sent after a data block. The firmware ignores it
/// (sdio.cc:72-74); a driver that enables CRC checking gets a valid one.
fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
        crc
    })
}

/// A 16-byte CID/CSD register: `body` plus `CRC7 << 1 | 1`.
fn with_crc7(body: [u8; 15]) -> [u8; 16] {
    let mut reg = [0; 16];
    reg[..15].copy_from_slice(&body);
    reg[15] = (body.iter().fold(0, |crc, &b| crc7(crc, b)) << 1) | 1;
    reg
}

/// Where the card is in the MOSI byte stream.
#[derive(Clone, Copy)]
enum Phase {
    /// Between commands; a CMD18 stream may be flowing out.
    Command,
    /// Collecting a 6-byte command frame; the value counts the bytes received.
    Frame(usize),
    /// CMD24/CMD25 accepted, waiting for a start token (or 0xFD for CMD25).
    WriteToken { multi: bool },
    /// Receiving 512 data bytes + 2 CRC bytes; the value counts the bytes received. Bytes 0x40-0x7F are data
    /// here, not command starts (07 §Functional model).
    WriteData { multi: bool, n: usize },
}

/// SDHC card in SPI mode, backed by a raw image whose length (rounded down to 512) is the capacity.
struct Card {
    file: File,
    sectors: u64,
    /// Image could only be opened read-only.
    write_protect: bool,
    idle: bool,
    /// Previous command was CMD55.
    app: bool,
    phase: Phase,
    frame: [u8; 6],
    /// Next sector of a CMD18 stream or CMD24/25 write.
    lba: u64,
    /// CMD18 stream active: blocks are generated whenever the output queue runs dry.
    streaming: bool,
    block: [u8; SECTOR_SIZE + 2],
    /// MISO bytes; 0xFF when empty.
    out: VecDeque<u8>,
}

impl Card {
    /// Opens the image read-write, or read-only with write protect when writing is not permitted.
    fn open(path: &Path) -> io::Result<Card> {
        let (file, write_protect) = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => (file, false),
            Err(e) if matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem) => {
                (File::open(path)?, true)
            }
            Err(e) => return Err(e),
        };
        let sectors = file.metadata()?.len() / SECTOR_SIZE as u64;
        if sectors == 0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "SD image is smaller than one sector"));
        }
        Ok(Card {
            file,
            sectors,
            write_protect,
            idle: true,
            app: false,
            phase: Phase::Command,
            frame: [0; 6],
            lba: 0,
            streaming: false,
            block: [0; SECTOR_SIZE + 2],
            out: VecDeque::with_capacity(SECTOR_SIZE + 3),
        })
    }

    /// Power-on protocol state.
    fn reset(&mut self) {
        self.idle = true;
        self.app = false;
        self.phase = Phase::Command;
        self.streaming = false;
        self.out.clear();
    }

    /// One SPI byte with SS asserted: MISO is the next queued byte, then MOSI is consumed. A response is
    /// therefore visible on the first transfer after the command's CRC byte (07 H3).
    fn xfer(&mut self, mosi: u8) -> u8 {
        if self.out.is_empty() && self.streaming && matches!(self.phase, Phase::Command) {
            self.queue_sector();
        }
        let miso = self.out.pop_front().unwrap_or(0xFF);
        self.receive(mosi);
        miso
    }

    fn peek(&self) -> u8 {
        self.out.front().copied().unwrap_or(0xFF)
    }

    fn receive(&mut self, mosi: u8) {
        match self.phase {
            Phase::Command | Phase::WriteToken { .. } if mosi & 0xC0 == 0x40 => {
                // A new command drops unread output, e.g. the CID CRC that get_drive_size never clocks
                // (sd_card.cc:600-604), and aborts a pending write.
                self.out.clear();
                self.frame[0] = mosi;
                self.phase = Phase::Frame(1);
            }
            Phase::Command => {}
            Phase::Frame(n) => {
                self.frame[n] = mosi;
                if n == 5 {
                    self.phase = Phase::Command;
                    self.execute();
                } else {
                    self.phase = Phase::Frame(n + 1);
                }
            }
            Phase::WriteToken { multi } => match mosi {
                TOKEN_START if !multi => self.phase = Phase::WriteData { multi, n: 0 },
                TOKEN_START_MULTI if multi => self.phase = Phase::WriteData { multi, n: 0 },
                TOKEN_STOP_TRAN if multi => self.phase = Phase::Command,
                _ => {}
            },
            Phase::WriteData { multi, n } => {
                self.block[n] = mosi;
                if n + 1 < self.block.len() {
                    self.phase = Phase::WriteData { multi, n: n + 1 };
                } else {
                    self.commit_write();
                    self.phase = if multi { Phase::WriteToken { multi } } else { Phase::Command };
                }
            }
        }
    }

    fn execute(&mut self) {
        let cmd = self.frame[0] & 0x3F;
        let arg = u32::from_be_bytes([self.frame[1], self.frame[2], self.frame[3], self.frame[4]]);
        let app = std::mem::take(&mut self.app);
        // Any command ends a CMD18 stream; CMD12 is the one that is supposed to.
        self.streaming = false;
        let idle = if self.idle { R1_IDLE } else { 0 };
        match (app, cmd) {
            (_, 0) => {
                self.idle = true;
                self.out.push_back(R1_IDLE);
            }
            // R7: voltage accepted + check pattern echo; the firmware needs byte 4 == 0xAA (07 H4).
            (false, 8) => self.out.extend([idle, 0x00, 0x00, (arg >> 8) as u8 & 0x0F, arg as u8]),
            (_, 55) => {
                self.app = true;
                self.out.push_back(idle);
            }
            // Ready on the first ACMD41, so the 32001-try loop runs once (07 H5).
            (true, 41) => {
                self.idle = false;
                self.out.push_back(0x00);
            }
            // R3: OCR with power-up done (bit 31) once initialised, CCS = 1 (bit 30), 2.7-3.6 V (07 H6).
            (false, 58) => {
                let ocr_hi = if self.idle { 0x40 } else { 0xC0 };
                self.out.extend([idle, ocr_hi, 0xFF, 0x80, 0x00]);
            }
            (false, 59) => self.out.push_back(idle),
            // In idle state SPI mode accepts only the commands above.
            _ if self.idle => self.out.push_back(R1_IDLE | R1_ILLEGAL),
            (false, 9) => {
                self.out.push_back(0x00);
                self.queue_data(&self.csd());
            }
            (false, 10) => {
                self.out.push_back(0x00);
                self.queue_data(&with_crc7(CID));
            }
            // R1b after a stuff byte; 0xFF suits both drivers that skip it and ones that poll past 0xFF.
            (false, 12) => self.out.extend([0xFF, 0x00]),
            (false, 13) => self.out.extend([0x00, 0x00]),
            (false, 16) => self.out.push_back(if arg as usize == SECTOR_SIZE { 0x00 } else { R1_PARAM }),
            (false, 17 | 18 | 24 | 25) if u64::from(arg) >= self.sectors => self.out.push_back(R1_PARAM),
            (false, 17 | 18) => {
                self.out.push_back(0x00);
                self.lba = u64::from(arg);
                if cmd == 17 {
                    // Token on the first poll after R1 (07 H7).
                    self.queue_sector();
                } else {
                    self.streaming = true;
                }
            }
            (false, 24 | 25) => {
                self.out.push_back(0x00);
                self.lba = u64::from(arg);
                self.phase = Phase::WriteToken { multi: cmd == 25 };
            }
            (true, 23) => self.out.push_back(0x00),
            _ => self.out.push_back(R1_ILLEGAL),
        }
    }

    /// Queues `0xFE`, `data`, CRC16.
    fn queue_data(&mut self, data: &[u8]) {
        self.out.push_back(TOKEN_START);
        self.out.extend(data);
        self.out.extend(crc16(data).to_be_bytes());
    }

    /// Queues the sector at `lba` as a data block, or a data error token.
    fn queue_sector(&mut self) {
        if self.lba >= self.sectors {
            self.streaming = false;
            self.out.push_back(ERROR_TOKEN_RANGE);
            return;
        }
        let mut data = [0; SECTOR_SIZE];
        let read = self
            .file
            .seek(SeekFrom::Start(self.lba * SECTOR_SIZE as u64))
            .and_then(|_| self.file.read_exact(&mut data));
        match read {
            Ok(()) => {
                self.lba += 1;
                self.queue_data(&data);
            }
            Err(_) => {
                self.streaming = false;
                self.out.push_back(ERROR_TOKEN);
            }
        }
    }

    /// Writes the received block straight to the image and queues the data-response token. No busy bytes
    /// follow, so the firmware's 600000-poll wait ends on its first read (07 H8).
    fn commit_write(&mut self) {
        let written = self.lba < self.sectors
            && self
                .file
                .seek(SeekFrom::Start(self.lba * SECTOR_SIZE as u64))
                .and_then(|_| self.file.write_all(&self.block[..SECTOR_SIZE]))
                .is_ok();
        self.lba += 1;
        self.out.push_back(if written { DATA_ACCEPTED } else { DATA_WRITE_ERROR });
    }

    /// CSD v2 (07 §CSD, H9). `C_SIZE = sectors / 1024 - 1`: the firmware reports `(C_SIZE + 1) << 10`
    /// sectors (sd_card.cc:662-670), so an image that is not a multiple of 512 KiB shows slightly smaller,
    /// one under 512 KiB shows as 512 KiB, and above 32 GiB the firmware's 16-bit `c_size` truncates (H10).
    fn csd(&self) -> [u8; 16] {
        let c_size = ((self.sectors >> 10).max(1) - 1).min(0x3F_FFFF) as u32;
        with_crc7([
            0x40,
            0x0E,
            0x00,
            0x32,
            0x5B,
            0x59,
            0x00,
            (c_size >> 16) as u8 & 0x3F,
            (c_size >> 8) as u8,
            c_size as u8,
            0x7F,
            0x80,
            0x0A,
            0x40,
            0x00,
        ])
    }
}

/// The SD SPI block (`spi_peripheral_io`, spi_peripheral_io.vhd:71-162) with an optional card.
pub struct SdCard {
    /// None: no image, the window reads as `io_dummy`.
    card: Option<Card>,
    /// SPI clock divider, 9 bits; irrelevant to emulation beyond readback.
    rate: u16,
    force_ss: bool,
    level_ss: bool,
    /// CRC7 over every MOSI bit since the last CRC write (spi.vhd:42-47,118-120).
    crc7: u8,
}

impl SdCard {
    /// No card: every register reads 0x00 and writes are ignored (07 H1, T0).
    pub fn absent() -> Self {
        SdCard { card: None, rate: RATE_RESET, force_ss: false, level_ss: true, crc7: 0 }
    }

    /// A card backed by the raw image at `path`. Writes go straight to the file; an image that can only be
    /// opened read-only reports write protect (07 H2).
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(SdCard { card: Some(Card::open(path)?), ..SdCard::absent() })
    }

    /// Image capacity in 512-byte sectors (0 without a card).
    /// The card, for the monitor's `sd` (S23): its sectors and whether the image is read-only. `None`: no card.
    pub fn card(&self) -> Option<(u64, bool)> {
        self.card.as_ref().map(|c| (c.sectors, c.write_protect))
    }

    pub fn sectors(&self) -> u64 {
        self.card.as_ref().map_or(0, |card| card.sectors)
    }

    /// SSn is low except when forced high (spi.vhd:54,62,100,128-130). A deselected card leaves MISO at 0xFF
    /// and ignores MOSI, e.g. the 100 x 0xFF preamble of sdio_init (sdio.cc:16-23).
    fn selected(&self) -> bool {
        !(self.force_ss && self.level_ss)
    }

    /// SWITCH readback `0000 WP CD level force` (spi_peripheral_io.vhd:127-128); card present means CD = 1.
    fn switches(&self, write_protect: bool) -> u8 {
        (u8::from(write_protect) << 3) | 0x04 | (u8::from(self.level_ss) << 1) | u8::from(self.force_ss)
    }
}

impl IoDevice for SdCard {
    fn name(&self) -> &'static str {
        "sdcard"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        let selected = self.selected();
        let Some(card) = self.card.as_mut() else { return 0 };
        match off & 0x0C {
            // DATA: a read clocks MOSI = 0xFF and returns MISO (spi_peripheral_io.vhd:117-120,142-147).
            0x0 => {
                self.crc7 = crc7(self.crc7, 0xFF);
                if selected {
                    card.xfer(0xFF)
                } else {
                    0xFF
                }
            }
            0x4 => self.rate as u8,
            0x8 => {
                let write_protect = card.write_protect;
                self.switches(write_protect)
            }
            _ => (self.crc7 << 1) | 1,
        }
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        let selected = self.selected();
        let Some(card) = self.card.as_mut() else { return };
        match off & 0x0C {
            // DATA: MOSI = val; the MISO byte is latched and lost (spi_peripheral_io.vhd:92-94).
            0x0 => {
                self.crc7 = crc7(self.crc7, val);
                if selected {
                    card.xfer(val);
                }
            }
            0x4 => self.rate = u16::from(val) | (u16::from(val & 0x80) << 1),
            0x8 => {
                self.force_ss = val & 0x01 != 0;
                self.level_ss = val & 0x02 != 0;
            }
            _ => self.crc7 = 0,
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        let Some(card) = self.card.as_ref() else { return 0 };
        match off & 0x0C {
            0x0 if self.selected() => card.peek(),
            0x0 => 0xFF,
            0x4 => self.rate as u8,
            0x8 => self.switches(card.write_protect),
            _ => (self.crc7 << 1) | 1,
        }
    }

    fn reset(&mut self) {
        self.rate = RATE_RESET;
        self.force_ss = false;
        self.level_ss = true;
        self.crc7 = 0;
        if let Some(card) = self.card.as_mut() {
            card.reset();
        }
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    let dev = match &cfg.sd_image {
        None => SdCard::absent(),
        Some(path) => SdCard::open(path).unwrap_or_else(|e| {
            eprintln!("sdcard: cannot use image {}: {e}; no card inserted", path.display());
            SdCard::absent()
        }),
    };
    if dev.sectors() >> 10 > 0x1_0000 {
        eprintln!("sdcard: image is larger than 32 GiB; the firmware truncates its size (docs/hw/07 H10)");
    }
    map.add(SDCARD_BASE, SDCARD_SIZE, Box::new(dev));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::irq::IrqState;

    const DATA: u32 = SDCARD_BASE;
    const SPEED: u32 = SDCARD_BASE + 0x04;
    const CTRL: u32 = SDCARD_BASE + 0x08;
    const CRC: u32 = SDCARD_BASE + 0x0C;

    fn image(sectors: u64) -> NamedTempFile {
        let img = NamedTempFile::new().unwrap();
        img.as_file().set_len(sectors * SECTOR_SIZE as u64).unwrap();
        img
    }

    fn pattern(seed: u64) -> [u8; SECTOR_SIZE] {
        std::array::from_fn(|i| (i as u64 * 7 + seed * 13) as u8)
    }

    fn put_sector(img: &NamedTempFile, lba: u64, data: &[u8; SECTOR_SIZE]) {
        let mut file = img.as_file();
        file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64)).unwrap();
        file.write_all(data).unwrap();
    }

    fn file_sector(path: &Path, lba: u64) -> [u8; SECTOR_SIZE] {
        let mut file = File::open(path).unwrap();
        file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64)).unwrap();
        let mut data = [0; SECTOR_SIZE];
        file.read_exact(&mut data).unwrap();
        data
    }

    /// Firmware `get_drive_size` view of a CSD v2: 16-bit `c_size` (sd_card.cc:581,662-670).
    fn fw_sectors(csd: &[u8; 16]) -> u32 {
        let c_size = (u32::from(csd[9]) + (u32::from(csd[8]) << 8) + (u32::from(csd[7] & 0x3F) << 16)) as u16;
        (u32::from(c_size) + 1) << 10
    }

    /// Drives the window through `install` + `IoMap` decode, the way the firmware does.
    struct Host {
        map: IoMap,
        irq: IrqState,
        console: Vec<u8>,
    }

    impl Host {
        fn new(image: Option<&Path>) -> Host {
            let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
            cfg.sd_image = image.map(Path::to_path_buf);
            let mut map = IoMap::new();
            install(&mut map, &cfg);
            Host { map, irq: IrqState::new(), console: Vec::new() }
        }

        fn r(&mut self, addr: u32) -> u8 {
            let (dev, off) = self.map.resolve(addr).expect("SD window mapped");
            let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut [], irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].read8(off, &mut ctx)
        }

        fn w(&mut self, addr: u32, val: u8) {
            let (dev, off) = self.map.resolve(addr).expect("SD window mapped");
            let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut [], irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].write8(off, val, &mut ctx);
        }

        fn reset_all(&mut self) {
            for dev in &mut self.map.devices {
                dev.reset();
            }
        }

        /// `SDIO_DATA_32` read: DATA transfers at +0..+3, the first byte in bits 7:0 (07 §Address map).
        fn r32(&mut self) -> u32 {
            (0..4).fold(0, |word, i| word | (u32::from(self.r(DATA + i)) << (8 * i)))
        }

        fn w32(&mut self, word: u32) {
            for i in 0..4 {
                self.w(DATA + i, (word >> (8 * i)) as u8);
            }
        }

        /// `sdio_send_command` (io/sd_card/sdio.cc:26-37).
        fn cmd(&mut self, n: u8, x: u16, y: u16) {
            self.w(DATA, 0xFF);
            self.w(CRC, 0);
            for b in [0x40 | n, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8] {
                self.w(DATA, b);
            }
            let crc = self.r(CRC);
            self.w(DATA, crc);
        }

        /// `SdCard::Resp8b` (io/sd_card/sd_card.cc:409-424).
        fn resp8b(&mut self) -> u8 {
            let mut resp = 0xFF;
            for _ in 0..8 {
                resp = self.r(DATA);
                if resp != 0xFF {
                    break;
                }
            }
            resp
        }

        /// `SdCard::init` with `sdio_init` (sd_card.cc:91-208, sdio.cc:14-24). Ok(sdhc).
        fn fw_init(&mut self) -> Result<bool, &'static str> {
            self.w(SPEED, 254);
            self.w(CTRL, 0x03);
            for _ in 0..100 {
                self.w(DATA, 0xFF);
            }
            self.w(CTRL, 0x00);
            self.w(SPEED, 200);
            let mut resp = 0xFF;
            for _ in 0..101 {
                self.cmd(0, 0, 0);
                resp = self.resp8b();
                if resp == 0x01 {
                    break;
                }
            }
            if resp != 0x01 {
                return Err("CMD0");
            }
            self.cmd(8, 0, 0x01AA);
            let v2 = self.resp8b() & 0x04 == 0;
            if v2 {
                self.r(DATA);
                self.w(DATA, 0xFF);
                self.r(DATA);
                if self.r(DATA) != 0xAA {
                    return Err("CMD8 check pattern");
                }
            }
            let mut tries = 0;
            loop {
                self.cmd(55, 0, 0);
                self.resp8b();
                self.cmd(41, 0x4000, 0);
                resp = self.resp8b();
                tries += 1;
                if resp != 0x01 || tries == 32001 {
                    break;
                }
            }
            if resp != 0x00 {
                return Err("ACMD41");
            }
            assert_eq!(tries, 1, "H5: ready on the first ACMD41");
            let mut sdhc = false;
            if v2 {
                self.cmd(58, 0, 0);
                self.resp8b();
                sdhc = self.r(DATA) & 0x40 != 0;
                for _ in 0..3 {
                    self.w(DATA, 0xFF);
                }
            }
            self.w(SPEED, 1);
            Ok(sdhc)
        }

        /// `SdCard::read` + `sdio_read_block` for one SDHC sector (sd_card.cc:254-280, sdio.cc:44-77).
        /// `wide` takes the word-aligned `SDIO_DATA_32` loop, otherwise the byte loop.
        fn fw_read(&mut self, lba: u32, wide: bool) -> Result<[u8; SECTOR_SIZE], u8> {
            self.cmd(17, (lba >> 16) as u16, lba as u16);
            let r1 = self.resp8b();
            if r1 != 0x00 {
                return Err(r1);
            }
            let token = self.r(DATA);
            assert_ne!(token, 0xFF, "H7: token on the first poll");
            if token != TOKEN_START {
                return Err(token);
            }
            let mut buf = [0; SECTOR_SIZE];
            if wide {
                for word in buf.chunks_exact_mut(4) {
                    word.copy_from_slice(&self.r32().to_le_bytes());
                }
            } else {
                for b in buf.iter_mut() {
                    *b = self.r(DATA);
                }
            }
            self.w(DATA, 0xFF);
            self.w(DATA, 0xFF);
            Ok(buf)
        }

        /// `SdCard::write` + `sdio_write_block` for one SDHC sector (sd_card.cc:332-351, sdio.cc:79-115).
        /// Returns R1.
        fn fw_write(&mut self, lba: u32, data: &[u8; SECTOR_SIZE], wide: bool) -> u8 {
            self.cmd(24, (lba >> 16) as u16, lba as u16);
            let r1 = self.resp8b();
            self.w(DATA, 0xFE);
            if wide {
                for word in data.chunks_exact(4) {
                    self.w32(u32::from_le_bytes(word.try_into().unwrap()));
                }
            } else {
                for &b in data {
                    self.w(DATA, b);
                }
            }
            for _ in 0..3 {
                self.w(DATA, 0xFF);
            }
            let polls = (1..=600_000).find(|_| self.r(DATA) == 0xFF);
            assert_eq!(polls, Some(1), "H8: not busy on the first poll");
            r1
        }

        /// CID/CSD read of `get_drive_size`: up to 200 `Resp8b` for 0xFE, then 16 bytes (sd_card.cc:591-603).
        fn fw_register(&mut self, cmd: u8) -> Option<[u8; 16]> {
            self.cmd(cmd, 0, 0);
            let found = (0..200).any(|_| self.resp8b() == TOKEN_START);
            found.then(|| std::array::from_fn(|_| self.r(DATA)))
        }

        /// `get_drive_size` bus sequence: CID (no trailing CRC clocks), CSD, 2 x `W FF` (sd_card.cc:590-633).
        fn fw_drive_size(&mut self) -> Result<([u8; 16], [u8; 16]), &'static str> {
            let cid = self.fw_register(10).ok_or("CID")?;
            let csd = self.fw_register(9).ok_or("CSD")?;
            self.w(DATA, 0xFF);
            self.w(DATA, 0xFF);
            Ok((cid, csd))
        }

        /// Generic SPI-mode command (ChaN mmc_spi `send_cmd` shape): 32-bit argument, real CRC7, stuff byte
        /// skipped after CMD12, R1 within 10 bytes.
        fn send_cmd(&mut self, cmd: u8, arg: u32) -> u8 {
            self.w(DATA, 0xFF);
            let frame = [0x40 | cmd, (arg >> 24) as u8, (arg >> 16) as u8, (arg >> 8) as u8, arg as u8];
            for b in frame {
                self.w(DATA, b);
            }
            self.w(DATA, (frame.iter().fold(0, |crc, &b| crc7(crc, b)) << 1) | 1);
            if cmd == 12 {
                self.r(DATA);
            }
            let mut r1 = 0xFF;
            for _ in 0..10 {
                r1 = self.r(DATA);
                if r1 & 0x80 == 0 {
                    break;
                }
            }
            r1
        }
    }

    #[test]
    fn no_image_reads_no_card() {
        let mut host = Host::new(None);
        assert_eq!(host.r(CTRL), 0x00, "T0 poll value: CD = 0 (07 H1)");
        host.w(CTRL, 0x03);
        host.w(DATA, 0x40);
        for off in [0x00, 0x03, 0x04, 0x08, 0x0C, 0xFF] {
            assert_eq!(host.r(SDCARD_BASE + off), 0x00, "offset {off:#x}");
        }
        assert!(host.map.resolve(SDCARD_BASE + SDCARD_SIZE).is_none(), "window is 256 bytes");

        let mut host = Host::new(Some(Path::new("/nonexistent/ue2-sd.img")));
        assert_eq!(host.r(CTRL), 0x00, "unusable image: no card");
    }

    #[test]
    fn firmware_init_is_ready_and_csd_reports_64mib() {
        let img = image((64 << 20) / SECTOR_SIZE as u64);
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.r(CTRL), 0x06, "CD + VHDL reset level_ss");
        assert_eq!(host.fw_init(), Ok(true), "SDHC (H6)");
        assert_eq!(host.r(CTRL), 0x04, "CD, SS automatic");
        assert_eq!(host.r(SPEED), 1, "RW_SPEED (sd_card.cc:186)");

        let (cid, csd) = host.fw_drive_size().expect("H9: CID and CSD found");
        assert_eq!(&cid[3..8], b"UE2SD");
        assert_eq!(csd[0] >> 6, 1, "CSD v2");
        assert_eq!(fw_sectors(&csd), 131_072);
        for reg in [cid, csd] {
            assert_eq!(reg, with_crc7(reg[..15].try_into().unwrap()), "register CRC7");
        }
        // The unclocked CID CRC did not desynchronise the next command.
        assert_eq!(host.fw_read(0, true), Ok([0; SECTOR_SIZE]));
    }

    #[test]
    fn firmware_read_block_returns_image_pattern() {
        let img = image(64);
        for lba in [0, 5, 63] {
            put_sector(&img, lba, &pattern(lba));
        }
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.fw_init(), Ok(true));
        assert_eq!(host.fw_read(5, true), Ok(pattern(5)));
        assert_eq!(host.fw_read(63, false), Ok(pattern(63)));
        assert_eq!(host.fw_read(0, true), Ok(pattern(0)));
        assert_eq!(host.fw_read(64, true), Err(R1_PARAM), "LBA past the image");
        assert_eq!(host.fw_read(5, false), Ok(pattern(5)), "recovers after an error");
    }

    #[test]
    fn firmware_write_block_goes_to_file() {
        let img = image(64);
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.fw_init(), Ok(true));
        assert_eq!(host.fw_write(7, &pattern(7), true), 0x00);
        assert_eq!(host.fw_write(8, &pattern(8), false), 0x00);
        assert_eq!(file_sector(img.path(), 7), pattern(7));
        assert_eq!(file_sector(img.path(), 8), pattern(8));
        assert_eq!(file_sector(img.path(), 6), [0; SECTOR_SIZE], "neighbour untouched");
        assert_eq!(file_sector(img.path(), 9), [0; SECTOR_SIZE], "neighbour untouched");
        assert_eq!(host.fw_read(7, false), Ok(pattern(7)));
        assert_eq!(host.fw_read(8, true), Ok(pattern(8)));
    }

    #[test]
    fn multi_block_write_and_read_with_stop() {
        let img = image(64);
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.fw_init(), Ok(true));

        assert_eq!(host.send_cmd(25, 10), 0x00);
        for lba in 10..13 {
            let data = pattern(lba);
            host.w(DATA, 0xFF);
            host.w(DATA, TOKEN_START_MULTI);
            for &b in &data {
                host.w(DATA, b);
            }
            for b in crc16(&data).to_be_bytes() {
                host.w(DATA, b);
            }
            assert_eq!(host.r(DATA) & 0x1F, DATA_ACCEPTED, "block {lba}");
            assert_eq!(host.r(DATA), 0xFF, "not busy");
        }
        host.w(DATA, TOKEN_STOP_TRAN);
        host.r(DATA);
        assert_eq!(host.r(DATA), 0xFF, "not busy after stop tran");
        for lba in 10..13 {
            assert_eq!(file_sector(img.path(), lba), pattern(lba));
        }
        assert_eq!(file_sector(img.path(), 13), [0; SECTOR_SIZE]);

        assert_eq!(host.send_cmd(18, 10), 0x00);
        for lba in 10..14 {
            let mut token = 0xFF;
            for _ in 0..10 {
                token = host.r(DATA);
                if token != 0xFF {
                    break;
                }
            }
            assert_eq!(token, TOKEN_START, "block {lba}");
            let data: [u8; SECTOR_SIZE] = std::array::from_fn(|_| host.r(DATA));
            let expected = if lba < 13 { pattern(lba) } else { [0; SECTOR_SIZE] };
            assert_eq!(data, expected, "block {lba}");
            let crc = u16::from_be_bytes([host.r(DATA), host.r(DATA)]);
            assert_eq!(crc, crc16(&data), "block {lba} CRC16");
        }
        assert_eq!(host.send_cmd(12, 0), 0x00, "stop transmission");
        assert_eq!(host.send_cmd(13, 0), 0x00, "R2");
        assert_eq!(host.r(DATA), 0x00, "R2 status byte");
        assert_eq!(host.r(DATA), 0xFF, "stream stopped");

        assert_eq!(host.send_cmd(18, 63), 0x00);
        assert_eq!(host.r(DATA), TOKEN_START);
        for _ in 0..SECTOR_SIZE + 2 {
            host.r(DATA);
        }
        assert_eq!(host.r(DATA), ERROR_TOKEN_RANGE, "stream runs past the image");
        assert_eq!(host.send_cmd(12, 0), 0x00);
    }

    #[test]
    fn registers_alias_and_crc7_frames_commands() {
        let img = image(8);
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.r(SPEED), 0xF4, "rate reset 500");
        host.w(SDCARD_BASE + 0x15, 200);
        assert_eq!(host.r(SPEED + 3), 200, "SPEED aliases every 16 bytes");
        host.w(CTRL + 0x10, 0x01);
        assert_eq!(host.r(CTRL + 0xF1), 0x05, "CTRL readback + CD");
        host.w(CTRL, 0x00);

        host.w(CRC, 0);
        for b in [0x40, 0x00, 0x00, 0x00, 0x00] {
            host.w(DATA + 1, b);
        }
        assert_eq!(host.r(CRC + 2), 0x95, "CMD0 CRC (07 §Functional model)");
        host.w(DATA + 2, 0x95);
        assert_eq!(host.r(DATA + 3), R1_IDLE, "H3: R1 on the first read after the CRC byte");

        host.w(CRC, 0);
        for b in [0x48, 0x00, 0x00, 0x01, 0xAA] {
            host.w(DATA, b);
        }
        assert_eq!(host.r(CRC), 0x87, "CMD8 CRC");
        host.w(DATA, 0x87);
        let r7: Vec<u8> = (0..5).map(|_| host.r(DATA)).collect();
        assert_eq!(r7, [0x01, 0x00, 0x00, 0x01, 0xAA], "H4");

        host.reset_all();
        assert_eq!(host.r(SPEED), 0xF4);
        assert_eq!(host.r(CTRL), 0x06);
        assert_eq!(host.r(CRC), 0x01);
    }

    #[test]
    fn idle_rejects_data_commands_and_deselect_ignores_mosi() {
        let img = image(8);
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.send_cmd(0, 0), R1_IDLE);
        assert_eq!(host.send_cmd(17, 0), R1_IDLE | R1_ILLEGAL, "not initialised");
        assert_eq!(host.fw_init(), Ok(true));
        assert_eq!(host.send_cmd(24, 8), R1_PARAM, "LBA past the image");
        assert_eq!(host.send_cmd(16, 512), 0x00);
        assert_eq!(host.send_cmd(63, 0), R1_ILLEGAL);

        host.w(CTRL, 0x03);
        assert_eq!(host.send_cmd(0, 0), 0xFF, "SS forced high");
        host.w(CTRL, 0x01);
        assert_eq!(host.send_cmd(13, 0), 0x00, "SS forced low; CMD0 above was ignored");
        assert_eq!(host.r(DATA), 0x00);
    }

    #[test]
    fn read_only_image_reports_write_protect() {
        let img = image(8);
        let mut perms = fs::metadata(img.path()).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(img.path(), perms).unwrap();
        if OpenOptions::new().write(true).open(img.path()).is_ok() {
            eprintln!("skipping: read-only permissions not enforced for this user");
            return;
        }
        let mut host = Host::new(Some(img.path()));
        assert_eq!(host.fw_init(), Ok(true));
        assert_eq!(host.r(CTRL), 0x0C, "CD + WP (07 H2)");
        assert_eq!(host.send_cmd(24, 2), 0x00);
        host.w(DATA, TOKEN_START);
        for _ in 0..SECTOR_SIZE + 2 {
            host.w(DATA, 0x55);
        }
        assert_eq!(host.r(DATA) & 0x1F, DATA_WRITE_ERROR);
        assert_eq!(file_sector(img.path(), 2), [0; SECTOR_SIZE]);
    }
}
