//! RTC, trace, RTC timer, GCR codec, ICAP, audio select (T0); the MultiCore host's ICAP loader (S44,
//! docs/specs/S44-multicore-cores.md).
//! Spec: docs/specs/S04-board-t0.md. Registers: docs/hw/07-sd-card-filesystems.md (RTC timer),
//! docs/hw/11-drives-iec-periph.md (GCR codec), docs/hw/12-gaps.md (audio select).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// S44 phase B: what the loader saw that the machine acts on (docs/specs/S44-multicore-cores.md §7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IcapEvent {
    /// The decouple request changed: the core's windows are cut off (true) or given back.
    Decouple(bool),
    /// The enable fell after a stream without an IPROG in it: the bytes, in file order (a partial bitstream).
    Streamed(Vec<u8>),
    /// A CMD write of IPROG: the FPGA reconfigures from this flash address (the last WBSTAR written).
    Iprog(u32),
}

/// Where the configuration logic is in the word stream (UG470 ch. 5, "Configuration Packets").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packet {
    /// Before the sync word: every word is ignored.
    Unsynced,
    /// The next word is a packet header.
    Header,
    /// `left` data words of a write to register `reg`.
    Data { reg: u8, left: u32 },
}

/// The sync word, the registers and the command UE2 acts on (UG470 tables 5-23, 5-24).
const SYNC: u32 = 0xAA99_5566;
const REG_CMD: u8 = 0x04;
const REG_WBSTAR: u8 = 0x10;
const CMD_IPROG: u32 = 0x0F;
const CMD_DESYNC: u32 = 0x0D;

/// S44: the MultiCore host's ICAP loader at 0x10060600 (TRX64U-MultiCore host/ultimate_sys.vhd:1454-1507, T79).
/// +00 reads the ID 0x7A; +10 W a byte of the bitstream while the enable is set, four make a word; +14 b0 the enable
/// (1 after 0 clears the count); +18-+1B R the words, little endian; +20 b0 the decouple request, R b1 the decouple in
/// effect (the request is its only source here); +21 b0 the expansion port release. Gideon's +04 and +08 (W25Q
/// reboot) are inert. Every other offset reads 0. A reset clears the enable and both requests, not the count.
///
/// Phase B: the words go through the configuration logic's packet reader. A write of IPROG to CMD is an
/// [`IcapEvent::Iprog`] with the last WBSTAR; the enable falling after a stream is an [`IcapEvent::Streamed`]; a
/// change of the decouple request is an [`IcapEvent::Decouple`]. Each event raises `stop`, which ends the machine's run
/// slice after this access, so the machine acts before the next instruction.
pub struct Icap {
    en: bool,
    n: u8,
    cnt: u32,
    dc: bool,
    port_rel: bool,
    /// The bytes streamed since the enable last rose, in file order.
    stream: Vec<u8>,
    word: u32,
    packet: Packet,
    /// The last word written to WBSTAR.
    wbstar: u32,
    /// The stream since the enable rose held an IPROG (it is not a partial then).
    iprog: bool,
    events: Vec<IcapEvent>,
    stop: Arc<AtomicBool>,
}

impl Icap {
    pub const ID: u8 = 0x7A;

    pub fn new() -> Self {
        Icap {
            en: false,
            n: 0,
            cnt: 0,
            dc: false,
            port_rel: false,
            stream: Vec::new(),
            word: 0,
            packet: Packet::Unsynced,
            wbstar: 0,
            iprog: false,
            events: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The decouple request in effect (the core's windows are cut off on the board).
    pub fn decoupled(&self) -> bool {
        self.dc
    }

    pub fn port_released(&self) -> bool {
        self.port_rel
    }

    pub fn words(&self) -> u32 {
        self.cnt
    }

    /// Set while events wait; the machine ends its run slice on it.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// The events since the last call, in order.
    pub fn take_events(&mut self) -> Vec<IcapEvent> {
        self.stop.store(false, Ordering::Relaxed);
        std::mem::take(&mut self.events)
    }

    fn event(&mut self, e: IcapEvent) {
        self.events.push(e);
        self.stop.store(true, Ordering::Relaxed);
    }

    /// One configuration word, first byte in bits 31:24 (the file's order).
    fn config_word(&mut self, w: u32) {
        match self.packet {
            Packet::Unsynced => {
                if w == SYNC {
                    self.packet = Packet::Header;
                }
            }
            Packet::Header => {
                let count = match w >> 29 {
                    1 => w & 0x7FF,
                    2 => w & 0x07FF_FFFF,
                    _ => 0,
                };
                // Type 1 names its register; a type 2 continues the type 1 before it (a zero-count FDRI header):
                // UE2 acts on no register a type 2 reaches, so its words are only skipped.
                let reg = if w >> 29 == 1 { ((w >> 13) & 0x1F) as u8 } else { 0xFF };
                if (w >> 27) & 3 == 2 && count > 0 {
                    self.packet = Packet::Data { reg, left: count };
                }
            }
            Packet::Data { reg, left } => {
                self.packet = if left > 1 { Packet::Data { reg, left: left - 1 } } else { Packet::Header };
                match (reg, w & 0x1F) {
                    (REG_WBSTAR, _) => self.wbstar = w,
                    (REG_CMD, CMD_IPROG) => {
                        self.iprog = true;
                        self.packet = Packet::Unsynced;
                        let at = self.wbstar;
                        self.event(IcapEvent::Iprog(at));
                    }
                    (REG_CMD, CMD_DESYNC) => self.packet = Packet::Unsynced,
                    _ => {}
                }
            }
        }
    }
}

impl Default for Icap {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for Icap {
    fn name(&self) -> &'static str {
        "icap"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        match off & 0xFF {
            0x10 if self.en => {
                self.stream.push(val);
                self.word = self.word << 8 | u32::from(val);
                self.n = (self.n + 1) & 3;
                if self.n == 0 {
                    self.cnt = self.cnt.wrapping_add(1);
                    let w = self.word;
                    self.config_word(w);
                }
            }
            0x14 => {
                let on = val & 1 != 0;
                if on && !self.en {
                    self.cnt = 0;
                    self.stream.clear();
                    self.iprog = false;
                    self.packet = Packet::Unsynced;
                } else if !on && self.en && !self.iprog && !self.stream.is_empty() {
                    let bytes = std::mem::take(&mut self.stream);
                    self.event(IcapEvent::Streamed(bytes));
                }
                self.en = on;
                self.n = 0;
            }
            0x20 => {
                let dc = val & 1 != 0;
                if dc != self.dc {
                    self.dc = dc;
                    self.event(IcapEvent::Decouple(dc));
                }
            }
            0x21 => self.port_rel = val & 1 != 0,
            _ => {}
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        match off & 0xFF {
            0x00 => Self::ID,
            0x14 => u8::from(self.en),
            0x18..=0x1B => (self.cnt >> (8 * (off & 3))) as u8,
            0x20 => u8::from(self.dc) << 1 | u8::from(self.dc),
            0x21 => u8::from(self.port_rel),
            _ => 0,
        }
    }

    /// The reconfiguration's power-on state: no enable, no requests, the packet reader unsynced; the count stays.
    fn reset(&mut self) {
        (self.en, self.n, self.dc, self.port_rel, self.iprog) = (false, 0, false, false, false);
        self.packet = Packet::Unsynced;
        self.stream.clear();
        self.events.clear();
        self.stop.store(false, Ordering::Relaxed);
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    // RTC_BASE (I2C RTC chip): no compiled user, the build links rtc_dummy.cc (07).
    add_table(map, 0x1006_0100, 0x100, "rtc-i2c", &[]);
    // TRACE_BASE: PROFILER_SUB/PROFILER_TASK 0x10060304/05, written on every context switch.
    add_table(map, 0x1006_0300, 0x100, "trace", &[]);
    map.add(0x1006_0400, 0x100, Box::new(RtcTimer::new()));
    map.add(0x1006_0500, 0x100, Box::new(GcrCodec::new()));
    // ICAP: the MultiCore host's loader with `--icap` (S44); else Gideon's PULSE 0x10060604 / WRITE 0x10060608,
    // accepted, the IPROG reboot is T1 (06), and the rest reads 0.
    if cfg.icap {
        map.add(0x1006_0600, 0x100, Box::new(Icap::new()));
    } else {
        add_table(map, 0x1006_0600, 0x100, "icap", &[]);
    }
    // AUDIO_SEL_BASE: written only by "Play MOD" under CAPAB_SAMPLER, never read (12 Region B).
    add_table(map, 0x1006_0700, 0x100, "audio-select", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::board::rig::Rig;

    const RTC: u32 = 0x1006_0400;
    const GCR_BASE: u32 = 0x1006_0500;

    /// S44: coreboot.cc's core_swap sequence against the loader: ID, decouple, stream, count.
    #[test]
    fn icap_loader_as_core_swap_drives_it() {
        let mut cfg = MachineConfig::new("fw".into(), "roms".into());
        cfg.icap = true;
        let mut rig = Rig::with_cfg(install, &cfg);
        const I: u32 = 0x1006_0600;
        assert_eq!(rig.r8(I), 0x7A);
        rig.w8(I + 0x20, 1);
        assert_eq!(rig.r8(I + 0x20), 3, "b1 the decouple in effect");
        rig.w8(I + 0x10, 0x55);
        assert_eq!(rig.r32(I + 0x18), 0, "no byte before the enable");
        rig.w8(I + 0x14, 1);
        for b in 0..10u8 {
            rig.w8(I + 0x10, b);
        }
        rig.w8(I + 0x14, 0);
        assert_eq!(rig.r32(I + 0x18), 2, "ten bytes are two words");
        assert_eq!(rig.map.get_mut::<Icap>().unwrap().take_events(), vec![
            IcapEvent::Decouple(true),
            IcapEvent::Streamed((0..10).collect())
        ]);
        rig.w8(I + 0x20, 0);
        rig.w8(I + 0x21, 1);
        assert_eq!((rig.r8(I + 0x20), rig.r8(I + 0x21)), (0, 1));
        rig.w8(I + 0x14, 1);
        assert_eq!(rig.r32(I + 0x18), 0, "the rising enable clears the count");
    }

    fn send(rig: &mut Rig, bytes: &[u8]) {
        const I: u32 = 0x1006_0600;
        for &b in bytes {
            rig.w8(I + 0x10, b);
        }
    }

    /// The IPROG sequence of hw_return_to_host / cfw_iprog_seq (WBSTAR, then CMD 0x0F), the decouple and a partial's
    /// stream as events; an IPROG pattern inside FDRI data is no command.
    #[test]
    fn icap_events_iprog_partial_and_decouple() {
        let mut cfg = MachineConfig::new("fw".into(), "roms".into());
        cfg.icap = true;
        let mut rig = Rig::with_cfg(install, &cfg);
        const I: u32 = 0x1006_0600;
        let take = |rig: &mut Rig| rig.map.get_mut::<Icap>().unwrap().take_events();
        let stop = rig.map.get::<Icap>().unwrap().stop_flag();
        rig.w8(I + 0x20, 1);
        rig.w8(I + 0x20, 1);
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(take(&mut rig), vec![IcapEvent::Decouple(true)], "one event per change");
        assert!(!stop.load(Ordering::Relaxed));
        // A partial: sync, a type-1 FDRI header with count 0, a type-2 header with 4 words that look like CMD IPROG.
        let mut partial = vec![0xFF; 4];
        partial.extend([0xAA, 0x99, 0x55, 0x66, 0x30, 0x00, 0x40, 0x00, 0x50, 0x00, 0x00, 0x04]);
        partial.extend([0x30, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x30, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, 0x0F]);
        partial.extend([0x30, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, 0x0D]);
        rig.w8(I + 0x14, 1);
        send(&mut rig, &partial);
        assert!(take(&mut rig).is_empty(), "FDRI data is not read as a command");
        rig.w8(I + 0x14, 0);
        assert_eq!(take(&mut rig), vec![IcapEvent::Streamed(partial.clone())]);
        let iprog = |a: u32| {
            let a = a.to_be_bytes();
            vec![
                0xFF, 0xFF, 0xFF, 0xFF, 0xAA, 0x99, 0x55, 0x66, 0x20, 0x00, 0x00, 0x00, 0x30, 0x02, 0x00, 0x01, a[0], a[1],
                a[2], a[3], 0x30, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x20, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00,
            ]
        };
        rig.w8(I + 0x14, 1);
        send(&mut rig, &iprog(0x0080_0000));
        assert_eq!(take(&mut rig), vec![IcapEvent::Iprog(0x0080_0000)], "acts on the word, the enable stays up");
        rig.w8(I + 0x14, 0);
        assert!(take(&mut rig).is_empty(), "a stream with an IPROG is no partial");
        rig.w8(I + 0x14, 1);
        send(&mut rig, &iprog(0));
        assert_eq!(take(&mut rig), vec![IcapEvent::Iprog(0)]);
    }

    #[test]
    fn icap_off_reads_zero() {
        let mut rig = Rig::new(install);
        assert_eq!(rig.r8(0x1006_0600), 0);
    }

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
