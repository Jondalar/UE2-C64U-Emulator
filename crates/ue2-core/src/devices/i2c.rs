//! HW I2C master 0x10100700 (never busy) with the HDMI monitor's EDID EEPROM on bus 0.
//! Spec: docs/specs/S04-board-t0.md. Registers: docs/hw/03-board-init.md §HW I2C master (i2c_master.vhd); transfers
//! and the EDID EEPROM: 03 §Functional model "HW I2C master (T1)" and "HPD / EDID".

use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

/// Register offsets (hw_i2c.h:5-15).
const DATA: u32 = 0x00;
const STATUS: u32 = 0x01;
const REPEATED_START: u32 = 0x02;
const STOP: u32 = 0x03;
const RECEIVE: u32 = 0x04;
const RECEIVE_ACK: u32 = 0x05;
const CHANNEL: u32 = 0x06;
const SOFT_RESET: u32 = 0x07;
const SCAN_ENABLE: u32 = 0x08;

/// HDMI DDC bus (`I2C_CHANNEL_HDMI`, i2c_drv.h:78).
const CHANNEL_HDMI: u8 = 0;
/// EDID EEPROM, 8-bit address form (u64_config.cc:2594, 2608; 03 §I2C device inventory).
const EDID_WRITE: u8 = 0xA0;
const EDID_READ: u8 = EDID_WRITE | 1;
/// E-DDC segment pointer (i2c_drv.cc:362).
const SEGMENT_WRITE: u8 = 0x60;

/// EDID of a 1920×1080@60 HDMI monitor: an EDID 1.3 base block and a CEA-861 extension, checksums filled in by
/// [`edid`]. `U64Config::IsMonitorHDMI` wants the header, an extension count ≥ 1 and a CEA block holding the HDMI
/// VSDB OUI `03 0C 00` (u64_config.cc:2629-2676); then Auto mode writes U64_HDMI_ENABLE = 1 (u64_config.cc:2678-2685,
/// 03 H20).
const EDID: [u8; 256] = edid(
    &[
        // Header; vendor "UEM", product 1, serial 0, week 0 of 2026; EDID 1.3.
        0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x54, 0xAD, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2E,
        0x01, 0x03,
        // Digital input, 53×30 cm, gamma 2.2, RGB colour, preferred timing in the first descriptor; sRGB primaries.
        0x80, 0x35, 0x1E, 0x78, 0x0A, 0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54,
        // Established 640×480@60, 800×600@60, 1024×768@60; standard 1920×1080@60, 1280×720@60, six unused.
        0x21, 0x08, 0x00, 0xD1, 0xC0, 0x81, 0xC0, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01,
        // Preferred timing 1920×1080@60: 148.5 MHz (CEA VIC 16), 531×299 mm, digital separate sync +H +V.
        0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00, 0x13, 0x2B, 0x21, 0x00, 0x00, 0x1E,
        // Range limits: 56-76 Hz vertical, 30-83 kHz horizontal, 170 MHz.
        0x00, 0x00, 0x00, 0xFD, 0x00, 0x38, 0x4C, 0x1E, 0x53, 0x11, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
        // Monitor name "UE2EMU HDMI".
        0x00, 0x00, 0x00, 0xFC, 0x00, 0x55, 0x45, 0x32, 0x45, 0x4D, 0x55, 0x20, 0x48, 0x44, 0x4D, 0x49, 0x0A, 0x20,
        // Dummy descriptor.
        0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // One extension block.
        0x01,
    ],
    &[
        // CEA-861 revision 3, descriptors at 0x1E, basic audio, one native format.
        0x02, 0x03, 0x1E, 0x41,
        // Video data block: VIC 16 (native), 4, 3, 2, 1, 31, 19, 18, 17.
        0x49, 0x90, 0x04, 0x03, 0x02, 0x01, 0x1F, 0x13, 0x12, 0x11,
        // Audio data block: LPCM, 2 channels, 32/44.1/48 kHz, 16/20/24 bit. Speaker allocation: front left/right.
        0x23, 0x09, 0x07, 0x07, 0x83, 0x01, 0x00, 0x00,
        // HDMI VSDB: OUI 00-0C-03, physical address 1.0.0.0, no deep colour, 225 MHz maximum TMDS clock.
        0x67, 0x03, 0x0C, 0x00, 0x10, 0x00, 0x00, 0x2D,
        // 1280×720@60: 74.25 MHz (CEA VIC 4), 531×299 mm, digital separate sync +H +V.
        0x01, 0x1D, 0x00, 0x72, 0x51, 0xD0, 0x1E, 0x20, 0x6E, 0x28, 0x55, 0x00, 0x13, 0x2B, 0x21, 0x00, 0x00, 0x1E,
    ],
);

/// Two 128-byte EDID blocks from their first 127 bytes (`cea` zero padded), each closed by the checksum byte that
/// makes the block sum to 0 mod 256 (VESA E-EDID).
const fn edid(base: &[u8; 127], cea: &[u8]) -> [u8; 256] {
    let mut out = [0; 256];
    let mut i = 0;
    while i < 127 {
        out[i] = base[i];
        i += 1;
    }
    i = 0;
    while i < cea.len() {
        out[128 + i] = cea[i];
        i += 1;
    }
    let mut block = 0;
    while block < 256 {
        let mut sum = 0u8;
        i = 0;
        while i < 127 {
            sum = sum.wrapping_add(out[block + i]);
            i += 1;
        }
        out[block + 127] = sum.wrapping_neg();
        block += 128;
    }
    out
}

/// Slave selected by the last address byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// No device model. Address and data bytes ACK and received bytes read 0xFF, as at T0 (a NACK would boot too and
    /// only change log lines, 03 H2).
    Unmodelled,
    /// EDID EEPROM write transfer: the first data byte loads the address pointer; further bytes are dropped.
    EdidWrite { pointer_loaded: bool },
    /// EDID EEPROM sequential read from the address pointer.
    EdidRead,
    /// E-DDC segment pointer write transfer.
    Segment,
}

/// The monitor side of the DDC bus: EDID EEPROM at 0xA0 and E-DDC segment pointer at 0x60 (i2c_drv.cc:356-399;
/// 03 §HPD / EDID).
#[derive(Clone, Copy, Debug, Default)]
struct Ddc {
    /// Byte address inside the segment: loaded by a write transfer, +1 per byte read.
    pointer: u8,
    /// Selects EDID bytes `segment * 256 + pointer`. STOP clears it (VESA E-DDC), so only `i2c_read_block_ext`
    /// transfers, which set it and use repeated starts, read beyond segment 0 (i2c_drv.cc:356-399).
    segment: u8,
}

impl Ddc {
    fn read(&mut self) -> u8 {
        let byte = EDID.get(usize::from(self.segment) * 256 + usize::from(self.pointer)).copied().unwrap_or(0xFF);
        self.pointer = self.pointer.wrapping_add(1);
        byte
    }
}

/// The FPGA I2C master. START/TX/repeated-start/STOP/RX strobes (0x700-0x705 writes) finish in zero time, so BUSY
/// never reads 1 (docs/hw/03-board-init.md §Functional model).
pub struct HwI2c {
    /// Selected bus, bits 1:0 (0 HDMI, 1 1V8, 2 3V3; i2c_drv.h:78-80). Only accepted when idle (vhd:192-193),
    /// and the master is always idle.
    pub channel: u8,
    /// Bus handed to the FPGA keyboard scanner (0x10100708 bit0, hw_i2c_drv.cc:59-62).
    pub scan_enable: bool,
    /// STATUS bit0 STARTED: a START went out and no STOP yet (vhd:163-174, 257-265).
    started: bool,
    /// The next transmitted byte is an address byte: after a START or a repeated start (vhd:267-274).
    addressing: bool,
    target: Target,
    /// `data_out` shift register: 0xFF after TX, which shifts in '1's (vhd:216), the received byte after RX
    /// (vhd:237-241, 300-301).
    data_out: u8,
    ddc: Ddc,
}

impl Default for HwI2c {
    fn default() -> Self {
        Self::new()
    }
}

impl HwI2c {
    pub fn new() -> Self {
        HwI2c {
            channel: 0,
            scan_enable: false,
            started: false,
            addressing: false,
            target: Target::Unmodelled,
            data_out: 0xFF,
            ddc: Ddc::default(),
        }
    }

    fn get(&self, off: u32) -> u8 {
        match off {
            DATA | RECEIVE | RECEIVE_ACK => self.data_out,
            // 00 §2 B3/B5/B6, 03 H1: bit7 BUSY is polled with no timeout (hw_i2c_drv.cc:4-5) and must read 0.
            // bit2 ERROR 0 = ACK from every address.
            STATUS => u8::from(self.started),
            CHANNEL => self.channel,
            _ => 0,
        }
    }

    /// DATA write: START unless started, then send the byte (vhd:163-174). The byte after a (repeated) start
    /// addresses a slave on the selected bus.
    fn transmit(&mut self, byte: u8) {
        if !self.started {
            self.started = true;
            self.addressing = true;
        }
        if self.addressing {
            self.addressing = false;
            self.target = match (self.channel, byte) {
                (CHANNEL_HDMI, EDID_WRITE) => Target::EdidWrite { pointer_loaded: false },
                (CHANNEL_HDMI, EDID_READ) => Target::EdidRead,
                (CHANNEL_HDMI, SEGMENT_WRITE) => Target::Segment,
                _ => Target::Unmodelled,
            };
        } else {
            match self.target {
                Target::EdidWrite { pointer_loaded: false } => {
                    self.ddc.pointer = byte;
                    self.target = Target::EdidWrite { pointer_loaded: true };
                }
                Target::Segment => self.ddc.segment = byte,
                Target::EdidWrite { pointer_loaded: true } | Target::EdidRead | Target::Unmodelled => {}
            }
        }
        self.data_out = 0xFF;
    }

    /// RECEIVE/RECEIVE_ACK strobe: the addressed slave drives the next byte; with none, SDA stays high (vhd:237-241).
    fn receive(&mut self) {
        self.data_out = match self.target {
            Target::EdidRead => self.ddc.read(),
            _ => 0xFF,
        };
    }
}

impl IoDevice for HwI2c {
    fn name(&self) -> &'static str {
        "hw-i2c"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.get(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        match off {
            DATA => self.transmit(val),
            // Any value; STARTED stays 1 (vhd:176-177, 267-274).
            REPEATED_START => self.addressing = true,
            // Any value (vhd:179-180, 257-265).
            STOP => {
                self.started = false;
                self.target = Target::Unmodelled;
                self.ddc.segment = 0;
            }
            RECEIVE | RECEIVE_ACK => self.receive(),
            CHANNEL => self.channel = val & 0x03,
            // bit0 resets the master: channel 0, scanner off, no transfer (vhd:136, 282-283). The monitor is untouched.
            SOFT_RESET if val & 1 != 0 => *self = HwI2c { ddc: self.ddc, ..HwI2c::new() },
            SCAN_ENABLE => self.scan_enable = val & 1 != 0,
            _ => {}
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        self.get(off)
    }

    fn reset(&mut self) {
        *self = HwI2c::new();
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(0x1010_0700, 0x100, Box::new(HwI2c::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::board::rig::Rig;

    /// `tx(b)` of hw_i2c_drv.cc:31-37: write, then poll bit7; bit2 is the NACK.
    fn tx(rig: &mut Rig, b: u8) -> u8 {
        rig.w8(0x1010_0700, b);
        rig.r8(0x1010_0701)
    }

    /// `rx(ack)` of hw_i2c_drv.cc:39-48.
    fn rx(rig: &mut Rig, ack: bool) -> u8 {
        rig.w8(if ack { 0x1010_0705 } else { 0x1010_0704 }, 1);
        assert_eq!(rig.r8(0x1010_0701) & 0x80, 0);
        rig.r8(0x1010_0700)
    }

    /// `i2c_read_block(dev, reg, data, len)` (i2c_drv.cc:318-354) on the current channel; None on a NACK.
    fn read_block(rig: &mut Rig, dev: u8, reg: u8, len: usize) -> Option<Vec<u8>> {
        for (strobe, byte) in [(None, dev), (None, reg), (Some(0x1010_0702), dev | 1)] {
            if let Some(addr) = strobe {
                rig.w8(addr, 1);
            }
            if tx(rig, byte) & 0x84 != 0 {
                return None;
            }
        }
        let data = (0..len).map(|i| rx(rig, i + 1 < len)).collect();
        rig.w8(0x1010_0703, 1);
        Some(data)
    }

    #[test]
    fn b3_i2c_not_busy() {
        let mut rig = Rig::new(install);
        // Hw_I2C_Driver ctor, then nau8822_init on channel 1 (u64ii_init.cc:116-120).
        rig.w8(0x1010_0708, 0);
        rig.w8(0x1010_0706, 1);
        assert_eq!(rig.r8(0x1010_0701), 0);
        for b in [0x34, 0x00, 0x00] {
            assert_eq!(tx(&mut rig, b), 0x01, "not busy, ACK, started");
        }
        rig.w8(0x1010_0703, 1);
        assert_eq!(rig.r8(0x1010_0701), 0);
        // A read from a device without a model: rx(ack) then rx(nack) return 0xFF.
        assert_eq!(read_block(&mut rig, 0xA0, 0x00, 2), Some(vec![0xFF, 0xFF]), "0xA0 on channel 1 is no EDID");
        assert_eq!(rig.r8(0x1010_0704), 0xFF);
        assert_eq!(rig.r8(0x1010_0706), 1);
        rig.w8(0x1010_0708, 1);
        assert!(rig.map.get::<HwI2c>().unwrap().scan_enable);
        rig.w8(0x1010_0707, 1);
        assert_eq!(rig.r8(0x1010_0706), 0);
        assert!(!rig.map.get::<HwI2c>().unwrap().scan_enable);
    }

    #[test]
    fn edid_blocks_are_valid_and_describe_an_hdmi_1080p60_monitor() {
        assert_eq!(EDID[..8], [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        for block in EDID.chunks(128) {
            assert_eq!(block.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)), 0, "checksum");
        }
        assert_eq!(EDID[126], 1, "extension count");
        // Preferred timing: pixel clock in 10 kHz, active and blanking sizes as 12-bit fields.
        let dtd = &EDID[54..72];
        let active_h = u16::from(dtd[4] >> 4) << 8 | u16::from(dtd[2]);
        let active_v = u16::from(dtd[7] >> 4) << 8 | u16::from(dtd[5]);
        let total_h = active_h + (u16::from(dtd[4] & 0x0F) << 8 | u16::from(dtd[3]));
        let total_v = active_v + (u16::from(dtd[7] & 0x0F) << 8 | u16::from(dtd[6]));
        let clock_hz = u64::from(u16::from_le_bytes([dtd[0], dtd[1]])) * 10_000;
        assert_eq!((active_h, active_v, clock_hz), (1920, 1080, 148_500_000));
        assert_eq!(clock_hz / (u64::from(total_h) * u64::from(total_v)), 60);
        // IsMonitorHDMI's walk over the CEA data blocks (u64_config.cc:2645-2673).
        let cea = &EDID[128..];
        assert_eq!(cea[0], 0x02);
        let (mut i, end) = (4, usize::from(cea[2]));
        let mut hdmi = false;
        while i < end {
            let (tag, len) = (cea[i] >> 5, usize::from(cea[i] & 0x1F));
            hdmi |= tag == 3 && cea[i + 1..i + 4] == [0x03, 0x0C, 0x00];
            i += 1 + len;
        }
        assert_eq!(i, end, "data blocks end at the descriptor offset");
        assert!(hdmi, "HDMI VSDB present");
    }

    #[test]
    fn read_edid_gets_both_blocks_on_the_hdmi_bus() {
        let mut rig = Rig::new(install);
        // U64Config::read_edid (u64_config.cc:2594-2614): channel 0, block 0 from 0x00, then block 1 from 0x80.
        rig.w8(0x1010_0706, 0);
        assert_eq!(read_block(&mut rig, 0xA0, 0x00, 128).as_deref(), Some(&EDID[..128]));
        assert_eq!(read_block(&mut rig, 0xA0, 0x80, 128).as_deref(), Some(&EDID[128..]));
        assert_eq!(rig.r8(0x1010_0701), 0, "stopped");
        // A read transfer without a pointer write continues sequentially and wraps inside the segment.
        assert_eq!(read_block(&mut rig, 0xA0, 0xFF, 1), Some(vec![EDID[255]]));
        rig.w8(0x1010_0700, 0xA1);
        assert_eq!(rx(&mut rig, false), EDID[0]);
        rig.w8(0x1010_0703, 1);
    }

    #[test]
    fn segment_pointer_selects_edid_pages_until_stop() {
        let mut rig = Rig::new(install);
        // i2c_read_block_ext(page 1, 0xA0, 0x00, ...) (i2c_drv.cc:356-399): segment 1 is past this EDID.
        let restart = Some(0x1010_0702);
        for (strobe, byte) in [(None, 0x60), (None, 1), (restart, 0xA0), (None, 0x00), (restart, 0xA1)] {
            if let Some(addr) = strobe {
                rig.w8(addr, 1);
            }
            assert_eq!(tx(&mut rig, byte), 0x01);
        }
        assert_eq!([rx(&mut rig, true), rx(&mut rig, false)], [0xFF, 0xFF]);
        rig.w8(0x1010_0703, 1);
        assert_eq!(read_block(&mut rig, 0xA0, 0x08, 2), Some(vec![EDID[8], EDID[9]]), "STOP cleared the segment");
        // Soft reset ends the transfer but keeps the monitor's pointer.
        rig.w8(0x1010_0700, 0xA1);
        rig.w8(0x1010_0707, 1);
        assert_eq!(rig.r8(0x1010_0701), 0);
        rig.w8(0x1010_0700, 0xA1);
        assert_eq!(rx(&mut rig, false), EDID[10]);
    }
}
