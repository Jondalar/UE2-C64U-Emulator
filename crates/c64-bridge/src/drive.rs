//! Drive A: TRX64's drive 8 behind the firmware's drive A registers (docs/specs/S14-c64-trx64.md §W4-DRIVE).
//!
//! The firmware owns the disk (docs/hw/11-drives-iec-periph.md §Drive): it converts images to GCR in DDR, and
//! `ue2_core::devices::drives::DriveRegs` hands the drive the bytes of each half-track and takes back what it wrote.
//! TRX64 has no API for a powered-off or held drive, a ROM from memory, the device jumpers or a surface fed from
//! outside, so this works through its public fields (docs/status/drive.md §TRX64 API gaps):
//! - a held drive steps out of `Machine::drive8` and a parked stand-in takes its place, so TRX64's run loop, which
//!   always clocks drive 8, runs nothing and the real drive's clock stands still;
//! - an unpowered drive leaves the IEC bus through TRX64's drive status bits;
//! - the ROM goes through a file, the only way `Drive1541::load_rom` takes one;
//! - the surface is `rotation.image`, and writes are read back from it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use trx64_core::gcr::{GcrImage, GcrTrack};
use trx64_core::iec::{IECBUS_STATUS_DRIVETYPE, IECBUS_STATUS_TRUEDRIVE};
use trx64_core::{Drive1541, Machine};
use ue2_core::c64host::{C64Drive, DriveLines, DriveStatus, DRIVE_HALF_TRACKS};

use crate::Trx64Backend;

/// Drive clock of the stand-in: TRX64's catch-up runs whole instructions while `core.clk < stop_clk` (drive.rs:803).
const PARKED: u64 = 1 << 62;

/// Drive RAM holding the DOS's LISTEN and TALK addresses, which its reset derives from the device jumpers
/// (1541 ROM $EB3A-$EB49, quoted at c1541.cc:359-368). TRX64 wires the jumpers to device 8 (drive.rs:146-148,
/// viacore.rs:2508-2509), so these are the values it leaves for device 8.
const LISTEN: u16 = 0x77;
const TALK: u16 = 0x78;
const LISTEN_8: u8 = 0x28;
const TALK_8: u8 = 0x48;

/// VIA2 port B and direction ($1C00/$1C02): bit 2 spindle motor (via2d store_prb, viacore.rs:2226-2240).
const VIA2_PRB: u16 = 0x1C00;
const VIA2_DDRB: u16 = 0x1C02;
const VIA2_MOTOR: u8 = 0x04;

/// `Drive1541::load_rom` reads the 16 K DOS ROM for $C000-$FFFF from this file (drive.rs:568-576).
const ROM_FILE: &str = "1541.bin";
const ROM_LEN: usize = 0x4000;
const RAM_LEN: usize = 0x800;

/// Numbers the private ROM directory of each drive.
static INSTANCES: AtomicUsize = AtomicUsize::new(0);

/// Drive A's state beside TRX64's `Machine::drive8`.
pub(crate) struct DriveA {
    lines: DriveLines,
    /// The real drive while it is held (off, in reset, frozen or not a 1541); `Machine::drive8` is then a stand-in.
    held: Option<Drive1541>,
    /// The stand-in while the real drive runs.
    spare: Option<Drive1541>,
    /// The reset line was active: the next clocked moment starts the CPU from its ROM.
    start_pending: bool,
    /// Drive 8 on TRX64's IEC bus as last configured; None after an IEC core reset.
    on_bus: Option<bool>,
    /// Half-tracks the drive may have written since `take_written`, bit n = half-track n.
    written: u128,
    /// Head position and TRX64's last written half-track (both 2-based) at the last check.
    last_head: u32,
    last_dirty: u32,
    /// Jumper setting still to be applied to $77/$78 once the DOS's reset has set them.
    device_patch: Option<u8>,
    /// The ROM TRX64 holds for $C000-$FFFF, the directory it was loaded through, and whether loading failed once.
    rom: Vec<u8>,
    rom_dir: PathBuf,
    rom_failed: bool,
    type_noted: bool,
}

impl DriveA {
    /// Drive A after a hardware reset: off and held in reset (drive_registers.vhd), its surface empty.
    pub(crate) fn new(m: &mut Machine) -> Self {
        let mut real = std::mem::replace(&mut m.drive8, stand_in());
        attach_surface(&mut real, empty_surface());
        let dir = format!("ue2emu-drive-{}-{}", std::process::id(), INSTANCES.fetch_add(1, Ordering::Relaxed));
        let mut drive = DriveA {
            lines: DriveLines::default(),
            held: Some(real),
            spare: None,
            start_pending: true,
            on_bus: None,
            written: 0,
            last_head: 0,
            last_dirty: 0,
            device_patch: None,
            rom: vec![0; ROM_LEN],
            rom_dir: std::env::temp_dir().join(dir),
            rom_failed: false,
            type_noted: false,
        };
        drive.apply_bus(m);
        drive
    }

    /// The firmware's lines, with the 32 K ROM image of the drive area (C64Drive::set_lines).
    pub(crate) fn set_lines(&mut self, m: &mut Machine, lines: DriveLines, rom: &[u8], stopped: bool, reset: bool) {
        self.load_rom(m, rom);
        // The C1541 constructor probes DRIVETYPE with 2 before any drive type is set (c1541.cc:119-120).
        if lines.power && lines.drive_type != 0 && !self.type_noted {
            eprintln!("c64: drive A type {} (1571/1581) is not modelled; drive A stays off", lines.drive_type);
            self.type_noted = true;
        }
        self.lines = lines;
        real_mut(&mut self.held, m).rotation.read_only = i32::from(lines.write_protect);
        self.update(m, stopped, reset);
    }

    /// Hold or clock the drive CPU for the current lines and the C64's stop and reset.
    pub(crate) fn update(&mut self, m: &mut Machine, c64_stopped: bool, c64_reset: bool) {
        let l = self.lines;
        // c1541_timing.vhd `iec_reset_o`: the C64's reset reaches the drive with `use_c64_reset`.
        let reset = l.reset || (l.follow_c64_reset && c64_reset);
        // c1541_timing.vhd: the CPU is clocked only with power and without `drive_stop and stop_on_freeze`.
        let clocked = l.power && l.drive_type == 0 && !(l.stop_on_freeze && c64_stopped);
        self.start_pending |= reset;
        if clocked && !reset {
            if let Some(real) = self.held.take() {
                self.spare = Some(std::mem::replace(&mut m.drive8, real));
            }
            if std::mem::take(&mut self.start_pending) {
                self.start(m);
            }
        } else {
            self.hold(m);
        }
        self.apply_bus(m);
    }

    /// Before `Machine::warm_reset`, which resets drive 8 with the C64 and re-mounts only TRX64's own disk
    /// (lib.rs:1074-1084): the real drive steps aside, and the lines decide whether it resets.
    pub(crate) fn before_c64_reset(&mut self, m: &mut Machine) {
        self.hold(m);
    }

    /// After `Machine::warm_reset`: park the stand-in it reset, and rejoin the new IEC core (lib.rs:1015).
    pub(crate) fn after_c64_reset(&mut self, m: &mut Machine, c64_stopped: bool) {
        park(&mut m.drive8);
        self.on_bus = None;
        self.update(m, c64_stopped, false);
    }

    /// The C64 CPU is stopped or in reset, so TRX64's run loop does not clock drive 8. Do what that loop does after
    /// each instruction (lib.rs:2165-2174); a parked stand-in only follows the clock.
    pub(crate) fn run_with_chips(&mut self, m: &mut Machine) {
        m.drive8.iec_drv_port = m.iec.iecbus.drv_port;
        m.drive8.iec_cpu_bus = m.iec.iecbus.cpu_bus;
        m.drive_c64_ref = m.drive8.catch_up_to(m.c64_core.clk, m.drive_c64_ref);
        m.iec.iec_drive_write((!m.drive8.via1_pb_iec_output()) & 0xff, 0);
        self.after_run(m);
    }

    /// After the drive ran: note the half-tracks it may have written, and set the device number once the DOS has
    /// read its jumpers.
    pub(crate) fn after_run(&mut self, m: &mut Machine) {
        if self.held.is_some() {
            return;
        }
        let d = &mut m.drive8;
        let r = &mut d.rotation;
        let head = r.current_half_track;
        // The drive writes only under the head (rotation.rs:346-391). A step flushes TRX64's dirty flag without a
        // write-back target (rotation.rs:1036-1068), so the track stepped off counts too; DriveRegs keeps only the
        // tracks whose bytes differ from the disk's.
        let mut written = 0;
        if !r.read_write_mode || r.gcr_dirty_track != 0 {
            written |= bit(head) | bit(r.dirty_half_track);
        }
        if r.dirty_half_track != self.last_dirty {
            written |= bit(r.dirty_half_track);
        }
        if head != self.last_head {
            written |= bit(self.last_head);
        }
        r.gcr_dirty_track = 0;
        (self.last_head, self.last_dirty) = (head, r.dirty_half_track);
        self.written |= written;
        if let Some(device) = self.device_patch {
            if d.drive_ram_read(TALK) == TALK_8 && d.drive_ram_read(LISTEN) == LISTEN_8 {
                d.drive_ram_write(TALK, TALK_8 + device);
                d.drive_ram_write(LISTEN, LISTEN_8 + device);
                self.device_patch = None;
            }
        }
    }

    pub(crate) fn set_track(&mut self, m: &mut Machine, half_track: u8, gcr: &[u8]) {
        let r = &mut real_mut(&mut self.held, m).rotation;
        let ht = usize::from(half_track);
        let Some(track) = r.image.as_mut().and_then(|i| i.tracks.get_mut(ht)) else { return };
        track.data.clear();
        track.data.extend_from_slice(gcr);
        track.size = gcr.len();
        if r.current_half_track as usize == ht + 2 {
            // The track under the head changed size: re-select it (rotation.rs:978-1013).
            let current = r.current_half_track;
            r.set_half_track(current);
        }
    }

    pub(crate) fn track<'a>(&'a self, m: &'a Machine, half_track: u8) -> &'a [u8] {
        let image = real(&self.held, m).rotation.image.as_ref();
        image.and_then(|i| i.tracks.get(usize::from(half_track))).map_or(&[], |t| &t.data[..t.size.min(t.data.len())])
    }

    pub(crate) fn take_written(&mut self) -> u128 {
        std::mem::take(&mut self.written)
    }

    pub(crate) fn status(&self, m: &Machine) -> DriveStatus {
        let d = real(&self.held, m);
        DriveStatus {
            half_track: (d.rotation.current_half_track.saturating_sub(2) as usize).min(DRIVE_HALF_TRACKS - 1) as u8,
            motor: d.drive_peek(VIA2_PRB) & d.drive_peek(VIA2_DDRB) & VIA2_MOTOR != 0,
            writing: !d.rotation.read_write_mode,
            led: d.led_on(),
        }
    }

    pub(crate) fn read_ram(&self, m: &Machine, out: &mut [u8]) {
        let d = real(&self.held, m);
        for (addr, byte) in out.iter_mut().take(RAM_LEN).enumerate() {
            *byte = d.drive_ram_read(addr as u16);
        }
    }

    fn hold(&mut self, m: &mut Machine) {
        if self.held.is_none() {
            let stand_in = self.spare.take().unwrap_or_else(stand_in);
            self.held = Some(std::mem::replace(&mut m.drive8, stand_in));
        }
    }

    /// Start the CPU from its ROM as a released reset does: CPU and VIAs reset, RAM kept, the head at track 1
    /// (floppy_stream.vhd `p_move`: reset clears `track_i`).
    fn start(&mut self, m: &mut Machine) {
        let d = &mut m.drive8;
        let surface = d.rotation.image.take().unwrap_or_else(empty_surface);
        d.cold_reset();
        d.rotation.current_half_track = 2;
        attach_surface(d, surface);
        d.rotation.read_only = i32::from(self.lines.write_protect);
        (self.last_head, self.last_dirty) = (2, 0);
        self.device_patch = (self.lines.device != 0).then(|| {
            d.drive_ram_write(TALK, 0);
            d.drive_ram_write(LISTEN, 0);
            self.lines.device
        });
    }

    /// Drive 8 is on the IEC bus only with power: mm_drive_cpu.vhd:747-750 releases CLK, DATA and ATN without it.
    /// TRX64 folds drive 8 into the bus unless both its status bits are clear, which selects the no-drive callbacks
    /// (iec.rs:645-667, 674-699).
    fn apply_bus(&mut self, m: &mut Machine) {
        let on = self.lines.power && self.lines.drive_type == 0;
        if self.on_bus == Some(on) {
            return;
        }
        m.iec.iecbus_status_set(IECBUS_STATUS_TRUEDRIVE, 8, u8::from(on));
        m.iec.iecbus_status_set(IECBUS_STATUS_DRIVETYPE, 8, u8::from(on));
        if on {
            // The no-drive write callback does not follow the C64's lines (iec.rs:449-451): take them from CIA2.
            m.iec.iec_update_cpu_bus(!m.cia2_pa_out);
            m.iec.iec_old_atn = m.iec.iecbus.cpu_bus & 0x10;
            m.iec.iec_update_ports();
        }
        self.on_bus = Some(on);
    }

    /// The DOS ROM for $C000-$FFFF: the upper half of the drive area's ROM image. `Drive1541::load_rom` only reads a
    /// file, so a changed image goes through one in a private temporary directory. $8000-$BFFF stays TRX64's zeros.
    fn load_rom(&mut self, m: &mut Machine, rom: &[u8]) {
        let Some(upper) = rom.len().checked_sub(ROM_LEN).map(|at| &rom[at..]) else { return };
        if upper == self.rom.as_slice() {
            return;
        }
        let file = self.rom_dir.join(ROM_FILE);
        let loaded = std::fs::create_dir_all(&self.rom_dir)
            .and_then(|()| std::fs::write(&file, upper))
            .map_err(|e| e.to_string())
            .and_then(|()| real_mut(&mut self.held, m).load_rom(&self.rom_dir).map_err(|e| format!("{e:?}")));
        match loaded {
            Ok(()) => self.rom.copy_from_slice(upper),
            Err(e) if !self.rom_failed => {
                eprintln!("c64: drive A ROM not loaded through {}: {e}", file.display());
                self.rom_failed = true;
            }
            Err(_) => {}
        }
    }
}

impl Drop for DriveA {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.rom_dir);
    }
}

fn real<'a>(held: &'a Option<Drive1541>, m: &'a Machine) -> &'a Drive1541 {
    held.as_ref().unwrap_or(&m.drive8)
}

fn real_mut<'a>(held: &'a mut Option<Drive1541>, m: &'a mut Machine) -> &'a mut Drive1541 {
    held.as_mut().unwrap_or(&mut m.drive8)
}

/// Bit of a 2-based TRX64 half-track in a written mask (bit n = firmware half-track n).
fn bit(half_track: u32) -> u128 {
    match half_track.checked_sub(2) {
        Some(n) if (n as usize) < DRIVE_HALF_TRACKS => 1 << n,
        _ => 0,
    }
}

fn empty_surface() -> GcrImage {
    GcrImage { tracks: (0..DRIVE_HALF_TRACKS).map(|_| GcrTrack { data: Vec::new(), size: 0 }).collect() }
}

/// Mount a surface without TRX64's attach delay (rotation.rs:1095-1145): the firmware's own sensor sequence tells the
/// DOS about a disk change (c1541.cc:456-514). The simple rotation engine is VICE's for dxx images; the firmware's
/// GCR is standard.
fn attach_surface(d: &mut Drive1541, surface: GcrImage) {
    let r = &mut d.rotation;
    r.image = Some(surface);
    r.gcr_image_loaded = 1;
    r.complicated_image_loaded = 0;
    (r.attach_clk, r.attach_detach_clk) = (0, 0);
    (r.gcr_current_track_size, r.gcr_head_offset) = (0, 0);
    let current = r.current_half_track;
    r.set_half_track(current);
}

/// A drive 8 that TRX64 clocks without running anything.
fn stand_in() -> Drive1541 {
    let mut d = Drive1541::new();
    d.cold_reset();
    park(&mut d);
    d
}

/// Consume the pending power-on reset, which the catch-up loop runs whatever the clock (drive.rs:803), then park the
/// CPU clock. A reset VIA has no alarm armed (viacore.rs `viacore_reset`), so an ATN edge stamped at the parked clock
/// (full.rs:596-606) dispatches nothing.
fn park(d: &mut Drive1541) {
    d.run_cycles(0);
    d.core.clk = PARKED;
}

impl C64Drive for Trx64Backend {
    fn set_lines(&mut self, lines: DriveLines, rom: &[u8]) {
        let (stopped, reset) = (self.stopped, self.reset_held);
        self.drive.set_lines(&mut self.m, lines, rom, stopped, reset);
    }

    fn set_track(&mut self, half_track: u8, gcr: &[u8]) {
        self.drive.set_track(&mut self.m, half_track, gcr);
    }

    fn track(&self, half_track: u8) -> &[u8] {
        self.drive.track(&self.m, half_track)
    }

    fn take_written(&mut self) -> u128 {
        self.drive.take_written()
    }

    fn status(&self) -> DriveStatus {
        self.drive.status(&self.m)
    }

    fn read_ram(&self, out: &mut [u8]) {
        self.drive.read_ram(&self.m, out);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use trx64_core::gcr::{gcr_read_sector, CBMDOS_FDC_ERR_OK};
    use trx64_core::iec::IecbusCallback;
    use ue2_core::c64host::C64Backend;
    use ue2_core::time::CLOCKS_PER_MS;

    use super::*;

    /// Firmware roms directory with the C64 ROMs and 1541.bin, or None (tests skip).
    fn roms() -> Option<PathBuf> {
        let root = std::env::var_os("UE2_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/1541ultimate"));
        let dir = root.join("roms");
        if dir.join("basic.bin").is_file() && dir.join(ROM_FILE).is_file() {
            Some(dir)
        } else {
            eprintln!("skipping: no C64 and 1541 ROMs in {} (set UE2_FIRMWARE)", dir.display());
            None
        }
    }

    fn run_ms(c64: &mut Trx64Backend, now: &mut u64, ms: u64) {
        for _ in 0..ms {
            *now += CLOCKS_PER_MS;
            c64.advance_to(*now);
        }
    }

    /// How often `text` (upper case and punctuation) is on the C64 screen, reverse video included (the directory
    /// header is printed in reverse).
    fn on_screen(c64: &Trx64Backend, text: &str) -> usize {
        let codes: Vec<u8> = text.bytes().map(|b| if b.is_ascii_uppercase() { b - b'@' } else { b }).collect();
        let screen = c64.frame().screen;
        screen.windows(codes.len()).filter(|w| w.iter().zip(&codes).all(|(s, c)| s & 0x7F == *c)).count()
    }

    /// Run until `text` is on the screen `count` times; false after `ms`.
    fn run_until(c64: &mut Trx64Backend, now: &mut u64, text: &str, count: usize, ms: u64) -> bool {
        (0..ms / 10).any(|_| {
            run_ms(c64, now, 10);
            on_screen(c64, text) >= count
        })
    }

    /// Type up to 10 PETSCII characters through the KERNAL keyboard buffer ($0277, count at $C6).
    fn type_keys(c64: &mut Trx64Backend, keys: &str) {
        for (i, key) in keys.bytes().enumerate() {
            c64.dma_write(0x0277 + i as u16, key, true);
        }
        c64.dma_write(0x00C6, keys.len() as u8, true);
    }

    /// 10 PRINT"HI THERE", loaded at $0801.
    fn hi_prg() -> Vec<u8> {
        let mut prg = vec![0x01, 0x08, 0x11, 0x08, 10, 0, 0x99, b'"'];
        prg.extend_from_slice(b"HI THERE\"\0\0\0");
        prg
    }

    fn name16(name: &[u8]) -> Vec<u8> {
        let mut padded = name.to_vec();
        padded.resize(16, 0xA0);
        padded
    }

    fn sectors(track: usize) -> usize {
        match track {
            1..=17 => 21,
            18..=24 => 19,
            25..=30 => 18,
            _ => 17,
        }
    }

    fn sector_offset(track: usize, sector: usize) -> usize {
        ((1..track).map(sectors).sum::<usize>() + sector) * 256
    }

    /// A 35-track D64 "UE2 TEST", ID U2, holding the PRG "HI" at 17/0.
    fn d64() -> Vec<u8> {
        let mut d = vec![0; sector_offset(36, 0)];
        let bam = sector_offset(18, 0);
        d[bam..bam + 4].copy_from_slice(&[18, 1, 0x41, 0]);
        for track in 1..=35 {
            let used: &[usize] = match track {
                17 => &[0],
                18 => &[0, 1],
                _ => &[],
            };
            let free = (0..sectors(track)).filter(|s| !used.contains(s)).fold(0u32, |bits, s| bits | 1 << s);
            let count = (sectors(track) - used.len()) as u8;
            d[bam + 4 * track..bam + 4 * track + 4].copy_from_slice(&[count, free as u8, (free >> 8) as u8, (free >> 16) as u8]);
        }
        d[bam + 0x90..bam + 0xA0].copy_from_slice(&name16(b"UE2 TEST"));
        d[bam + 0xA0..bam + 0xAB].copy_from_slice(b"\xA0\xA0U2\xA02A\xA0\xA0\xA0\xA0");
        let dir = sector_offset(18, 1);
        d[dir..dir + 5].copy_from_slice(&[0, 0xFF, 0x82, 17, 0]);
        d[dir + 5..dir + 21].copy_from_slice(&name16(b"HI"));
        d[dir + 30] = 1;
        let prg = hi_prg();
        let file = sector_offset(17, 0);
        d[file..file + 2].copy_from_slice(&[0, prg.len() as u8 + 1]);
        d[file + 2..file + 2 + prg.len()].copy_from_slice(&prg);
        d
    }

    /// The drive area's ROM image: the 16 K DOS mirrored at $8000 and $C000 (c1541.cc:937-940).
    fn rom_image(dir: &Path) -> Vec<u8> {
        let dos = std::fs::read(dir.join(ROM_FILE)).expect("1541 ROM");
        [dos.clone(), dos].concat()
    }

    /// A C64 at READY. whose drive A holds [`d64`] and runs with `lines`: the firmware's insert (c1541.cc:456-514)
    /// and power-on (c1541.cc:324-349) reduced to surfaces and lines.
    fn with_drive(lines: DriveLines) -> Option<(Trx64Backend, u64)> {
        let dir = roms()?;
        let mut c64 = Trx64Backend::new(&dir);
        let rom = rom_image(&dir);
        let image = GcrImage::from_d64(&d64());
        for (half_track, track) in image.tracks.iter().enumerate() {
            c64.set_track(half_track as u8, &track.data[..track.size]);
        }
        let mut now = 0;
        c64.advance_to(now);
        c64.set_reset(true);
        run_ms(&mut c64, &mut now, 20);
        c64.set_reset(false);
        c64.set_lines(DriveLines { reset: true, ..lines }, &rom);
        run_ms(&mut c64, &mut now, 1);
        c64.set_lines(lines, &rom);
        assert!(run_until(&mut c64, &mut now, "READY.", 1, 3000), "no READY.");
        Some((c64, now))
    }

    fn running(device: u8) -> DriveLines {
        DriveLines { power: true, reset: false, write_protect: false, device, ..DriveLines::default() }
    }

    #[test]
    fn held_or_unpowered_drive_runs_nothing() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let off = |c64: &Trx64Backend| matches!(c64.m.iec.iecbus_callback, IecbusCallback::Conf0);
        let parked = |c64: &Trx64Backend| c64.drive.held.is_some() && c64.m.drive8.core.clk == PARKED;
        assert!(off(&c64) && parked(&c64), "power-on: off and in reset");
        c64.advance_to(0);
        c64.set_reset(true);
        c64.set_reset(false);
        assert!(off(&c64) && parked(&c64), "a C64 reset leaves an unpowered drive alone");
        let rom = vec![0; 0x8000];
        c64.set_lines(DriveLines { power: true, ..DriveLines::default() }, &rom);
        assert!(!off(&c64) && parked(&c64), "powered, still in reset");
        c64.set_lines(running(0), &rom);
        assert!(c64.drive.held.is_none() && c64.m.drive8.core.clk < PARKED, "released");
        c64.set_stopped(true);
        assert!(parked(&c64), "stops with the C64 (RESET bit 2)");
        c64.set_stopped(false);
        c64.set_lines(DriveLines { stop_on_freeze: false, ..running(0) }, &rom);
        c64.set_stopped(true);
        assert!(c64.drive.held.is_none(), "runs on while the C64 is frozen");
        c64.set_stopped(false);
        c64.set_reset(true);
        assert!(parked(&c64), "follows the C64's reset (RESET bit 1)");
        c64.set_reset(false);
        assert!(c64.drive.held.is_none() && !off(&c64));
        c64.set_lines(DriveLines { power: false, ..running(0) }, &rom);
        assert!(off(&c64) && parked(&c64), "power off releases the bus");
    }

    #[test]
    fn dos_lists_loads_and_saves() {
        let Some((mut c64, mut now)) = with_drive(running(0)) else { return };
        type_keys(&mut c64, "LOAD\"$\",8\r");
        assert!(run_until(&mut c64, &mut now, "READY.", 2, 10_000), "directory not loaded: {:?}", c64.frame().screen);
        assert_eq!(c64.status().half_track, 34, "head on track 18");
        type_keys(&mut c64, "LIST\r");
        assert!(run_until(&mut c64, &mut now, "BLOCKS FREE.", 1, 2000));
        assert_eq!((on_screen(&c64, "\"UE2 TEST        \" U2 2A"), on_screen(&c64, "\"HI\"")), (1, 1));

        // CLR/HOME first, so the READY. count is not cut short by the screen scrolling.
        let clear = |c64: &mut Trx64Backend, now: &mut u64| {
            type_keys(c64, "\u{93}");
            assert!(run_until(c64, now, "READY.", 0, 100));
            run_ms(c64, now, 100);
            assert_eq!(on_screen(c64, "READY."), 0, "screen cleared");
        };
        clear(&mut c64, &mut now);
        type_keys(&mut c64, "LOAD\"HI\",8\r");
        assert!(run_until(&mut c64, &mut now, "READY.", 1, 10_000), "HI not loaded");
        type_keys(&mut c64, "RUN\r");
        assert!(run_until(&mut c64, &mut now, "HI THERE", 1, 2000));

        clear(&mut c64, &mut now);
        c64.take_written();
        type_keys(&mut c64, "SAVE\"N\",8\r");
        assert!(run_until(&mut c64, &mut now, "READY.", 1, 20_000), "SAVE did not finish");
        run_ms(&mut c64, &mut now, 500);
        let written = c64.take_written();
        assert_ne!(written & 1 << 34, 0, "directory track written: {written:#x}");
        // Decode what the DOS wrote: the directory entry N on 18/1, then its first block.
        let read = |c64: &Trx64Backend, track: u8, sector: u8| {
            let data = c64.track((track - 1) * 2).to_vec();
            let mut block = [0; 256];
            assert_eq!(gcr_read_sector(&GcrTrack { size: data.len(), data }, &mut block, sector), CBMDOS_FDC_ERR_OK);
            block
        };
        let dir = read(&c64, 18, 1);
        let entry = (0..8).map(|i| &dir[2 + 32 * i..34 + 32 * i]).find(|e| e[0] == 0x82 && e[3..19] == name16(b"N")[..]);
        let entry = entry.expect("N in the directory");
        assert_ne!(written & 1 << ((entry[1] - 1) * 2), 0, "file track written");
        let block = read(&c64, entry[1], entry[2]);
        assert_eq!((block[0], &block[2..usize::from(block[1]) + 1]), (0, &hi_prg()[..]), "N holds the program");
    }

    #[test]
    fn device_jumpers_move_the_dos_address() {
        let Some((mut c64, mut now)) = with_drive(running(1)) else { return };
        let listen_talk = |c64: &Trx64Backend| (c64.m.drive8.drive_ram_read(LISTEN), c64.m.drive8.drive_ram_read(TALK));
        assert!((0..300).any(|_| {
            run_ms(&mut c64, &mut now, 10);
            listen_talk(&c64) == (LISTEN_8 + 1, TALK_8 + 1)
        }));
        let mut ram = [0; RAM_LEN];
        c64.read_ram(&mut ram);
        assert_eq!(ram[usize::from(TALK)], TALK_8 + 1, "device 9");
    }
}
