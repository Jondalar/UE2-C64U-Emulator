//! RTC, trace, RTC timer, GCR codec, ICAP, audio select (T0).
//! Spec: docs/specs/S04-board-t0.md. Registers: docs/hw/07-sd-card-filesystems.md (RTC timer),
//! docs/hw/11-drives-iec-periph.md (GCR codec), docs/hw/12-gaps.md (audio select).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::devices::board::add_table;
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

fn host_utc_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// RTC seconds timer 0x10060400 (real_time_clock.vhd): a 32-bit LE seconds counter at +0..+3, other offsets
/// read 0. Counts host UTC seconds. 00 §2 C21, 07 H12: FatFs timestamps and the UI clock read it
/// (rtc_dummy.cc:25,119-123); 0 would only give 1970 dates.
pub struct RtcTimer {
    /// Guest seconds minus host UTC seconds, set by firmware writes (rtc_dummy.cc:109,114).
    offset: i64,
    /// Counter while a write holds the lock: byte 0 locks, byte 3 unlocks (real_time_clock.vhd).
    locked: Option<u32>,
    /// Counter sampled by the byte-0 read; bytes 1-3 of the same 32-bit load return it, so a second
    /// boundary between the four byte reads cannot tear the value.
    sample: u32,
}

impl Default for RtcTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl RtcTimer {
    pub fn new() -> Self {
        RtcTimer { offset: 0, locked: None, sample: 0 }
    }

    fn current(&self) -> u32 {
        self.locked.unwrap_or_else(|| (host_utc_secs() + self.offset) as u32)
    }
}

impl IoDevice for RtcTimer {
    fn name(&self) -> &'static str {
        "rtc-timer"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        match off & 0x0F {
            0 => {
                self.sample = self.current();
                self.sample as u8
            }
            r @ 1..=3 => (self.sample >> (8 * r)) as u8,
            _ => 0,
        }
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        let r = off & 0x0F;
        if r > 3 {
            return;
        }
        let v = (self.current() & !(0xFF << (8 * r))) | u32::from(val) << (8 * r);
        match r {
            0 => self.locked = Some(v),
            3 => {
                self.offset = i64::from(v) - host_utc_secs();
                self.locked = None;
            }
            _ if self.locked.is_some() => self.locked = Some(v),
            _ => self.offset = i64::from(v) - host_utc_secs(),
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        match off & 0x0F {
            r @ 0..=3 => (self.current() >> (8 * r)) as u8,
            _ => 0,
        }
    }

    fn reset(&mut self) {
        *self = RtcTimer::new();
    }

    crate::impl_as_any!();
}

/// 4-bit value → 5-bit GCR code (bin2gcr.vhd). gcr2bin.vhd is its inverse; other codes flag an error.
const GCR: [u8; 16] = [0x0A, 0x0B, 0x12, 0x13, 0x0E, 0x0F, 0x16, 0x17, 0x09, 0x19, 0x1A, 0x1B, 0x0D, 0x1D, 0x1E, 0x15];

/// GCR codec 0x10060500 (gcr_codec.vhd). Not used at boot, but a RAZ codec would turn D64 mounts into all-zero
/// GCR and decode zeros into the user's image on write-back (11 H17), so it is exact.
/// One 40-bit shift register (VHDL bit 0 = bit 39 here) takes every byte write. Reads (`address(3:0)`):
/// +0..+3 decode the 8 five-bit codes, +4..+7 error bits (bit 7-i = code i invalid), +8..+C encode the
/// low 32 bits.
pub struct GcrCodec {
    shift: u64,
}

impl Default for GcrCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl GcrCodec {
    pub fn new() -> Self {
        GcrCodec { shift: 0x55_5555_5555 }
    }

    fn encoded(&self) -> u64 {
        (0..8).fold(0, |acc, i| acc << 5 | u64::from(GCR[(self.shift >> (28 - 4 * i) & 0xF) as usize]))
    }

    /// (decoded bytes, error bits).
    fn decoded(&self) -> (u32, u8) {
        (0..8).fold((0, 0), |(bin, err), i| {
            let code = (self.shift >> (35 - 5 * i) & 0x1F) as u8;
            match GCR.iter().position(|&c| c == code) {
                Some(nibble) => (bin << 4 | nibble as u32, err << 1),
                None => (bin << 4, err << 1 | 1),
            }
        })
    }

    fn get(&self, off: u32) -> u8 {
        match off & 0x0F {
            r @ 0..=3 => (self.decoded().0 >> (24 - 8 * r)) as u8,
            4..=7 => self.decoded().1,
            r @ 8..=0x0C => (self.encoded() >> (32 - 8 * (r - 8))) as u8,
            _ => 0,
        }
    }
}

impl IoDevice for GcrCodec {
    fn name(&self) -> &'static str {
        "gcr-codec"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.get(off)
    }

    fn write8(&mut self, _off: u32, val: u8, _ctx: &mut IoCtx) {
        self.shift = (self.shift << 8 | u64::from(val)) & 0xFF_FFFF_FFFF;
    }

    fn peek8(&self, off: u32) -> u8 {
        self.get(off)
    }

    fn reset(&mut self) {
        *self = GcrCodec::new();
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    // RTC_BASE (I2C RTC chip): no compiled user, the build links rtc_dummy.cc (07).
    add_table(map, 0x1006_0100, 0x100, "rtc-i2c", &[]);
    // TRACE_BASE: PROFILER_SUB/PROFILER_TASK 0x10060304/05, written on every context switch.
    add_table(map, 0x1006_0300, 0x100, "trace", &[]);
    map.add(0x1006_0400, 0x100, Box::new(RtcTimer::new()));
    map.add(0x1006_0500, 0x100, Box::new(GcrCodec::new()));
    // ICAP PULSE 0x10060604 / WRITE 0x10060608: accepted, IPROG reboot is T1 (06).
    add_table(map, 0x1006_0600, 0x100, "icap", &[]);
    // AUDIO_SEL_BASE: written only by "Play MOD" under CAPAB_SAMPLER, never read (12 Region B).
    add_table(map, 0x1006_0700, 0x100, "audio-select", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::board::rig::Rig;

    const RTC: u32 = 0x1006_0400;
    const GCR_BASE: u32 = 0x1006_0500;

    #[test]
    fn c21_rtc_epoch() {
        let mut rig = Rig::new(install);
        let host = host_utc_secs();
        let guest = i64::from(rig.r32(RTC));
        assert!((host..=host + 2).contains(&guest), "guest {guest} host {host}");
        assert_eq!(rig.r8(RTC + 4), 0);
        // Rtc::set_time_utc (rtc_dummy.cc:112-115): one LE 32-bit store.
        rig.w32(RTC, 1_000_000_000);
        let t = rig.r32(RTC);
        assert!((1_000_000_000..=1_000_000_002).contains(&t), "{t}");
        // Byte 0 locks the counter until byte 3 is written.
        rig.w8(RTC, 0x44);
        assert_eq!(rig.r8(RTC), 0x44);
        assert_eq!(rig.map.get::<RtcTimer>().unwrap().peek8(0), 0x44);
        rig.w8(RTC + 3, 0x7F);
        assert_eq!(rig.r32(RTC) >> 24, 0x7F);
    }

    #[test]
    fn h17_gcr_codec() {
        let mut rig = Rig::new(install);
        // GcrImage encode: one 32-bit store, then read +8..+C (disk_image.cc:218-237).
        rig.w32(GCR_BASE + 8, 0);
        assert_eq!([8, 9, 10, 11, 12].map(|o| rig.r8(GCR_BASE + o)), [0x52, 0x94, 0xA5, 0x29, 0x4A]);
        rig.w32(GCR_BASE + 8, 0xFFFF_FFFF);
        assert_eq!([8, 9, 10, 11, 12].map(|o| rig.r8(GCR_BASE + o)), [0xAD, 0x6B, 0x5A, 0xD6, 0xB5]);
        // Encode bytes 01 02 03 04, then decode the 5 GCR bytes back (disk_image.cc:382-397).
        rig.w32(GCR_BASE + 8, 0x0403_0201);
        let gcr = [8, 9, 10, 11, 12].map(|o| rig.r8(GCR_BASE + o));
        for b in gcr {
            rig.w8(GCR_BASE, b);
        }
        assert_eq!([0, 1, 2, 3].map(|o| rig.r8(GCR_BASE + o)), [1, 2, 3, 4]);
        assert_eq!(rig.r8(GCR_BASE + 4), 0);
        // 00000 is not a GCR code: all 8 error bits.
        for _ in 0..5 {
            rig.w8(GCR_BASE, 0);
        }
        assert_eq!((rig.r8(GCR_BASE + 7), rig.r8(GCR_BASE + 0x0D)), (0xFF, 0));
    }

    #[test]
    fn misc_sinks_accept_writes() {
        let mut rig = Rig::new(install);
        rig.w8(0x1006_0304, 15);
        rig.w8(0x1006_0604, 1);
        rig.w8(0x1006_0700, 6);
        assert_eq!([0x1006_0100, 0x1006_0304, 0x1006_0604, 0x1006_0700].map(|a| rig.r8(a)), [0; 4]);
    }
}
