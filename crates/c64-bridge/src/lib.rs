//! TRX64 (`trx64-core`) as the C64 behind the U64-II cart/DMA registers: [`Trx64Backend`] implements
//! `ue2_core::c64host::C64Backend`. Spec: docs/specs/S14-c64-trx64.md.
//!
//! TRX64 has no API for a reset line, a forced ULTIMAX decode or a key-matrix bitmap, so the bridge works through its
//! public internals: the cartridge slot with `memconfig_table`/`pla_index` for ULTIMAX and the cartridges (§5.2, §8,
//! docs/status/carts.md), key names for the matrix (§6). Cargo.toml pins the TRX64 commit these internals were checked
//! against.
//!
//! The machine is built on TRX64's `u64` profile (Spec 851), which brings the turbo registers and the UCI block
//! (Spec 852) the firmware serves at 0x10044000 (docs/specs/S15-uci.md). A held 6510 is TRX64's own `Machine::set_hold`
//! (Spec 850 D7), and the cartridges' IRQ/NMI sit on its expansion-port source (D6).

mod board;
mod cart;
mod cart_eeprom;
mod clock;
mod drive;
mod hwlog;
pub mod iec_proc;
mod keys;
mod reu;
mod ring;
mod sampler;
mod sid;
mod slot;
mod video;
pub mod wd177x;

/// S30: the IEC processor's bus slot (TRX64 Spec 874: 4-7 are folded and belong to no unit).
const IEC_SLOT: u8 = 4;

use std::ops::RangeInclusive;
use std::path::Path;

use trx64_core::c64_6510core::{IK_NMI, INTERRUPT_DELAY, INT_SRC_RESTORE};
use trx64_core::cart::CartMapper;
use trx64_core::expansion::Hold;
use trx64_core::RunStop;
use trx64_core::keyboard::JoystickState;
use trx64_core::model::{self, C64Model};
use trx64_core::reu::Reu;
use trx64_core::vic::SpeedProfile;
use trx64_core::{AccessCtx, BusKind, CpuHistoryRing, DeltaRing, Machine, Observer};
use trx64_core::c64cia::{cia1_read_pa, cia1_read_pb};
use trx64_core::ciacore::CiaPins;
use ue2_core::c64host::{C64Backend, C64CartSlot, C64Drive, C64Frame, C64Rom, CartPeek, CartRom, CartSlotInfo, UciEvents};

use cart::{CartHandle, CartLogic, CartProxy, RunHints};
use clock::Clock;
use hwlog::HwLog;
use keys::Keys;
use reu::ReuRam;
use slot::{PhysicalCart, SlotHandle};

pub use board::{check_board_ring, BoardCheck, BOARD_MAX_ENTRIES, BOARD_NODE};
pub use ring::{BoardDumpInfo, Mode as RingMode, Ring, UE2_NODE};
pub use slot::FlashDecode;
use video::Palette;

pub use sid::AudioSink;
pub use cart::{CAPAB_COMMAND_INTF, CAPAB_EEPROM, CAPAB_SAMPLER};

/// C64 core config offsets the bridge acts on besides the SID and bus-sharing registers (u64.h:106,134-135). The
/// firmware writes the enable word and the preferred speed, then strobes the update (u64_config.cc:1634-1636).
const CORE_TURBOREGS_EN: u8 = 0x02;
const CORE_SPEED_PREFER: u8 = 0x2D;
const CORE_SPEED_UPDATE: u8 = 0x2E;
/// C64_VIDEOFORMAT and its cycles-per-line field (u64.h:105,167-169): 63 is PAL timing, 65 NTSC; the colour encoding
/// and the other bits change nothing the C64 core produces (S25 §1).
const CORE_VIDEOFORMAT: u8 = 0x01;
/// S32: C64_PADDLE_1_X .. C64_MOUSE_EN_2 (u64.h:139-144): x/y of port 1, x/y of port 2, the two mouse enables.
const CORE_PADDLE_1_X: u8 = 0x32;
const CORE_MOUSE_EN_2: u8 = 0x37;
const VIDEOFORMAT_CYCLES: u8 = 0x30;
const VIDEOFORMAT_CYCLES_63: u8 = 0x00;
const VIDEOFORMAT_CYCLES_65: u8 = 0x20;
/// Cycles past the computed frame end a switching run aims at: TRX64 reports line 0 from the cycle after the frame
/// boundary, and the switch has to land early in line 0 (S25 §2).
pub(crate) const BOUNDARY_SLACK: u64 = 2;
/// C64_BUS_INTERNAL bits that gate the internal IO1 and IO2 ranges, and with them the UCI window (c64.cc:1536-1590;
/// u64_config.cc:1015-1016; Spec 852 §5).
const BUS_IO1: u8 = 0x01;
const BUS_IO2: u8 = 0x02;

/// TRX64's switch for its always-on reverse-debug rings, read in `Machine::new` (cpu_history.rs:98,
/// delta_ring.rs:285).
const CPUHISTORY_ENV: &str = "TRX64_CPUHISTORY";
/// Depth of the delta ring in seconds, read in `Machine::new` (delta_ring.rs:270-278). Switched-off rings are still
/// allocated at their depth.
const REVERSE_SECONDS_ENV: &str = "TRX64_REVERSE_SECONDS";
/// `TRX64_REVERSE_SECONDS` as the user gave it, read before [`Trx64Backend::new`] sets its own (S38 `set_ring`).
static USER_REVERSE_SECONDS: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// ROM images seeded from the firmware roms directory (S14 §5.5). The firmware has no BASIC fallback (c64.cc:1103).
const BASIC_ROM: &str = "basic.bin";
const KERNAL_ROM: &str = "kernal.901227-03.bin";
const CHAR_ROM: &str = "characters.901225-01.bin";

/// DMA reads here that no SID decodes return 0 while I/O is mapped: an empty socket's detector reports "none" (S14
/// §W4-SID; doc 10 H12/H13).
const SID_WINDOW: RangeInclusive<u16> = 0xD400..=0xD7FF;

/// TRX64 access-watch table of the cartridge I/O `$DE00-$DFFF`, for carts whose lines change on reads.
static IO_WATCH: [u8; 0x1_0000] = {
    let mut table = [0; 0x1_0000];
    let mut addr = 0xDE00;
    while addr < 0xE000 {
        table[addr] = 1;
        addr += 1;
    }
    table
};

/// TRX64 access-watch table of the UCI control register at each slot base the firmware sets: $DF1C, $DFFC and $DE1C
/// (`CMD_IF_SLOT_BASE` 0x47, 0x7F, 0x07; c64.cc:1305-1320).
static UCI_WATCH: [u8; 0x1_0000] = {
    let mut table = [0; 0x1_0000];
    table[0xDF1C] = 1;
    table[0xDFFC] = 1;
    table[0xDE1C] = 1;
    table
};

/// The UCI control register addresses of [`UCI_WATCH`].
fn is_uci_control(addr: u16) -> bool {
    matches!(addr, 0xDF1C | 0xDFFC | 0xDE1C)
}

/// How long the C64 waits for the firmware after a UCI event (PUSH_CMD, DATA_ACC, ABORT), in firmware clocks: 10 ms.
/// The C64 runs in batches of up to `SYNC_PERIOD` behind the firmware (S14 §4), so without the wait the firmware saw an
/// event up to a millisecond late, while hardware answers within microseconds. A program that aborts and then sends
/// at once (`uii_detect()` then a command, at 16 MHz) had its command bytes wiped by the late HANDSHAKE_RESET
/// (issue #2, `Null command.` on the console). A command the firmware takes longer for ends the wait after 10 ms,
/// and the C64 polls on as it would on hardware (docs/specs/S15-uci.md).
const UCI_EVENT_WAIT: u64 = 10 * ue2_core::time::CLOCKS_PER_MS;
/// The status byte's event bits the firmware clears once it has handled them: b0 new command, b1 data accepted, b2
/// abort (TRX64 `uci.rs` `handshake_in`).
const UCI_EVENTS: u8 = 0x07;

/// The TRX64 observer for runs without cartridge hints.
///
/// Two jobs in one, because a run has one observer: it halts after a C64 write to the UCI control register, and it
/// halts on a debugger's checkpoint (S23 §8). Both arrive through `on_access`, which is the only hook that can end
/// a run, and both are gated by the watch table so an untouched address costs nothing.
#[derive(Default)]
struct UciObserver {
    /// Checkpoint ranges the frontend set, as (start, end, operations) with `MEMORY_OP` bits: 1 load, 2 store,
    /// 4 exec. Empty while no debugger is attached.
    checkpoints: Vec<(u16, u16, u8)>,
    /// The address of the first checkpoint this run hit.
    hit: Option<u16>,
}

/// Whether this access is a load/store checkpoint. Both observers ask this, because a checkpoint has to fire
/// whatever the run is doing.
fn is_checkpoint(checkpoints: &[(u16, u16, u8)], kind: BusKind, addr: u16, _ctx: &AccessCtx) -> bool {
    // Load and store only: an instruction fetch never arrives here, so the exec bit is `exec_watch`'s job.
    let op = if kind == BusKind::Write { 0x02 } else { 0x01 };
    checkpoints.iter().any(|&(start, end, ops)| start <= addr && addr <= end && ops & op != 0)
}

impl Observer for UciObserver {
    #[inline(always)]
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

    #[inline(always)]
    fn on_bus(&mut self, _: BusKind, _: u16, _: u8, _: u16, _: u64, _: u8) {}

    #[inline(always)]
    fn on_interrupt(&mut self, _: u16, _: u64) {}

    fn on_access(&mut self, kind: BusKind, addr: u16, _: u8, ctx: AccessCtx) -> bool {
        if self.hit.is_none() && is_checkpoint(&self.checkpoints, kind, addr, &ctx) {
            self.hit = Some(addr);
            return true;
        }
        kind == BusKind::Write && is_uci_control(addr)
    }
}

/// A TRX64 `Machine` driven by the U64 firmware's C64 registers.
/// How long the REU stays on the bus after C64_REU_ENABLE goes to 0, in firmware clocks: 10 ms. The firmware writes
/// 0 then 1 within microseconds on every cartridge change (`set_emulation_flags`, c64.cc:297-305), while a program may
/// already be using the REU; on hardware that is a short decode gap a program rarely hits, here it is none. A setting
/// that switches the REU off takes effect 10 ms later (docs/status/reu.md, "Enable toggle").
const REU_OFF_GRACE: u64 = 10 * ue2_core::time::CLOCKS_PER_MS;

/// The same grace for the UCI: `set_emulation_flags` writes CMD_IF_SLOT_ENABLE 0 then 1 (c64.cc:311-331) after a DMA
/// load has already started the program, and a program that sends its first command at once (at 16 MHz) wrote its
/// bytes into the gap and got an empty reply (issue #2, `Null command.` on the console). A setting that switches the
/// command interface off takes effect 10 ms later (docs/specs/S15-uci.md).
const UCI_OFF_GRACE: u64 = REU_OFF_GRACE;
/// CMD_IF_SLOT_ENABLE, a firmware register of the UCI block (command_protocol.vhd; TRX64 `uci.rs`): bit 7 clear
/// writes the enable (bit 0), bit 7 set the C64 bus ID. The registers repeat every 16 bytes below the RAM at 0x800.
const UCI_SLOT_ENABLE: u16 = 0x1;
const UCI_FW_RAM: u16 = 0x800;

pub struct Trx64Backend {
    m: Box<Machine>,
    /// S24 M3: the UDP audio stream's tap while that stream is enabled.
    stream_tap: Option<std::sync::Arc<std::sync::Mutex<Vec<i16>>>>,
    /// Emulator clock → C64 cycles; anchored by the first `advance_to` and after every reset.
    clock: Option<Clock>,
    /// Emulator clock of the last `advance_to`.
    now: u64,
    /// The row C64_VIDEOFORMAT asks for, until the VIC reaches a frame boundary to switch at (S25 §2).
    pending_model: Option<&'static C64Model>,
    /// C64_VIDEOFORMAT asked for a cycle count no row has; noticed once.
    videoformat_noted: bool,
    stopped: bool,
    /// S37 phase B: TRX64-Ultimate's machine pause: no C64 cycle runs (`C64Backend::set_paused`).
    paused: bool,
    reset_held: bool,
    /// S28: `cart::RamSnoop` is on the port.
    ram_snoop: bool,
    ultimax: bool,
    /// The cartridge logic, shared with the mapper TRX64 holds (cart.rs).
    cart: CartHandle,
    /// CARTSLOT: the physical expansion port and the bus sharing, shared with the mapper TRX64 holds (slot.rs).
    slot: SlotHandle,
    /// Cartridge types already reported as not modelled.
    unsupported: Vec<u8>,
    keys: Keys,
    /// Joystick port 1 and 2 lines, active low.
    joysticks: [u8; 2],
    /// NMI level from the port (C64_MODE bit 4, MATRIX_KEYB[9], host RESTORE).
    nmi: bool,
    /// The port's NMI as last given to TRX64's RESTORE source, which carries nothing else now (Spec 850 D6).
    nmi_line: bool,
    /// C64_TURBOREGS_EN (0x02) and C64_SPEED_PREFER (0x2D), latched until the C64_SPEED_UPDATE strobe (0x2E) hands
    /// them to TRX64 (Spec 851 §6). They start at the profile's own values — "U64 Turbo Registers" at 1 MHz with
    /// badline timing (Spec 851 D2) — so a strobe before any write changes nothing.
    turbo_regs_en: u8,
    speed_prefer: u8,
    /// The enable word and speed last handed to TRX64, and whether the C64 was reset since. `set_u64_turbo` is the
    /// menu (TRX64 Spec 890), but the firmware's reset task re-runs `setCpuSpeed` after every C64 reset
    /// (u64_config.cc:1035-1045), and on a C64 Ultimate that strobe leaves the reset state alone: `$D031` reads `$00`
    /// in registers mode, `$D030` `$FE` in TurboEnable mode. So the first strobe after a reset that repeats the
    /// applied values is not a menu set; one with new values is.
    turbo_applied: Option<(u8, u8)>,
    reset_since_turbo: bool,
    palette: Palette,
    /// The sockets and UltiSIDs on reSID, the mixer and the sample stream (S17).
    sid: sid::Sid,
    /// S27: drives A and B on TRX64's two drive positions (drive.rs).
    drives: [drive::DriveSlot; 2],
    /// The drive `C64Backend::drive` handed out last; `C64Drive` calls go to it.
    drive_sel: usize,
    /// REU: guest DDR at `REU_MEMORY_BASE` as the store TRX64's `Reu` holds (reu.rs, Spec 854).
    reu: ReuRam,
    /// C64_REU_SIZE in KiB, which the next attach takes. The register resets to "111" = 16 MB (c64.rs `CART_REGS`),
    /// and the firmware writes it before it writes the enable (c64.cc:315-317).
    reu_size_kb: u32,
    /// The REU while C64_REU_ENABLE is 0, kept with its REC registers. The FPGA's enable only gates the decode, and
    /// the firmware toggles it 0 → 1 on every cartridge change (`set_emulation_flags`, c64.cc:297-305), also while a
    /// program already runs and uses the REU (docs/status/reu.md, "Enable toggle").
    parked_reu: Option<Box<dyn trx64_core::expansion::ExpansionDevice>>,
    /// When C64_REU_ENABLE went to 0 with the REU still on the bus ([`REU_OFF_GRACE`]).
    reu_off_since: Option<u64>,
    /// When CMD_IF_SLOT_ENABLE went to 0 with the UCI block still answering at $DF1C ([`UCI_OFF_GRACE`]).
    uci_off_since: Option<u64>,
    /// The C64 waits for the firmware to handle a UCI event: since when, and which event bits ([`UCI_EVENT_WAIT`]).
    uci_wait: Option<(u64, u8)>,
    /// A debugger's checkpoints (S23 §8) as (start, end, `MEMORY_OP` bits), and the watch table they are gated by:
    /// [`UCI_WATCH`] with their addresses added, rebuilt whenever they change.
    checkpoints: Vec<(u16, u16, u8)>,
    /// Load/store checkpoints, gating `on_access`.
    checkpoint_watch: Option<Box<[u8; 0x1_0000]>>,
    /// Exec checkpoints. These are a different door: an instruction FETCH never reaches `on_access` — that hook
    /// is the load/store path (`full_sc.rs` `loadRead`/`store`) — so an exec breakpoint goes through TRX64's own
    /// `exec_watch`, which is checked at the instruction boundary and ends the run with `RunStop::Breakpoint(pc)`
    /// before the instruction runs.
    checkpoint_exec: Option<Box<[u8; 0x1_0000]>>,
    /// The address of the checkpoint the last run stopped at, waiting to be picked up.
    checkpoint_hit: Option<u16>,
    /// Ultimate Audio: UE2's own block, shared with the port device TRX64 holds (sampler.rs, S16).
    sampler: sampler::SamplerHandle,
    /// S32: the core's paddle registers 0x32-0x37 as last written.
    paddles: [u8; 6],
    /// S30: the IEC processor, shared with the device TRX64 holds on the IEC bus at slot 4 (Spec 874).
    iec: iec_proc::IecHandle,
    /// S38: the checkpoint ring and its transport while TRX64's rings are on (`set_ring`).
    ring: Option<Box<Ring>>,
    /// S38: UE2's write log of the memories around the C64 (hwlog.rs), shared with the cartridge logic and the REU's
    /// store. It takes records while the rings are on.
    hwlog: HwLog,
}

impl Trx64Backend {
    /// TRX64's machine, for the monitor host of S23 (`ue2emu::monitor`). The C64 and drive A are reached through
    /// it, exactly as the daemon reaches its own.
    pub fn trx64(&mut self) -> &mut Machine {
        &mut self.m
    }

    /// A powered-on C64 (S14 §7) with BASIC, KERNAL and CHAR ROM seeded from `rom_dir` where present.
    ///
    /// Unless `TRX64_CPUHISTORY` is set, TRX64's reverse-debug rings are switched off (a per-instruction cost) and
    /// kept at one entry instead of their default depth (about 110 MB); S38's [`Self::set_ring`] turns them on. An
    /// explicit `TRX64_CPUHISTORY` leaves both to TRX64 until then.
    pub fn new(rom_dir: &Path) -> Self {
        let rings_off = std::env::var_os(CPUHISTORY_ENV).is_none();
        // S38: the depth the user asked for, before the line below may set its own.
        USER_REVERSE_SECONDS.get_or_init(|| std::env::var(REVERSE_SECONDS_ENV).ok());
        if rings_off {
            std::env::set_var(CPUHISTORY_ENV, "0");
            if std::env::var_os(REVERSE_SECONDS_ENV).is_none() {
                // Machine::new allocates the switched-off rings at this depth before they are replaced below.
                std::env::set_var(REVERSE_SECONDS_ENV, "1");
            }
        }
        let mut m = Box::new(Machine::new());
        // Spec 851 D1: this is an Ultimate, not a plain C64. The profile brings the turbo registers and the UCI block
        // (Spec 852), and it has to be in before the power-on reset below — a cartridge probes in its boot stub.
        m.set_machine_profile(SpeedProfile::U64);
        if rings_off {
            m.cpu_history = CpuHistoryRing::with_capacity(1);
            m.delta_ring = DeltaRing::with_capacity(1, 1);
        }
        m.fill_power_on_ram();
        let mm = &mut *m;
        for (name, rom) in [(BASIC_ROM, &mut mm.basic_rom[..]), (KERNAL_ROM, &mut mm.kernal_rom[..]), (CHAR_ROM, &mut mm.char_rom[..])] {
            match std::fs::read(rom_dir.join(name)) {
                Ok(image) if image.len() == rom.len() => rom.copy_from_slice(&image),
                _ => eprintln!("c64: no {name} in {}; the C64 has only what the firmware uploads", rom_dir.display()),
            }
        }
        m.full_assembled = true;
        m.cold_reset();
        // S17: TRX64's SID write trace, host door and decode table (Spec 855); the machine keeps them across resets.
        let mut sid = sid::Sid::new();
        sid.install(&mut m);
        let drives = [
            drive::DriveSlot::new(&mut m, trx64_core::drive::DrivePosition::A),
            drive::DriveSlot::new(&mut m, trx64_core::drive::DrivePosition::B),
        ];
        // S16: the sampler is on the port for the life of the machine, as it is in the FPGA; `C64_SAMPLER_ENABLE`
        // gates the window, not the block's existence. Attaching it before any REU also settles who answers
        // `$DF20-$DFFF`: TRX64's REU mirrors its registers there, the U64's does not.
        let sampler = sampler::SamplerHandle::default();
        m.attach_expansion_also(Box::new(sampler.clone()));
        // S30: the IEC processor is on the bus for the life of the machine, as in the FPGA; RESET_ENABLE decides
        // whether it runs. Slot 4 is TRX64's home for a device with no fixed unit (Spec 874 §4).
        let iec = iec_proc::IecHandle::default();
        // S38: the cartridge logic's memory writes (cart ROM, cart RAM, GeoRAM, the EEPROM) and the REU's go to one log.
        let hwlog = HwLog::default();
        let cart = CartHandle::default();
        cart.with(|c| c.set_log(hwlog.clone(), hwlog::Space::Ddr));
        let hz = u64::from(m.timing().cpu_hz);
        if let Err(e) = m.attach_iec_device(IEC_SLOT, Box::new(iec_proc::IecBusDevice::new(iec.clone(), hz))) {
            eprintln!("c64: {e}");
        }
        Trx64Backend {
            stream_tap: None,
            m,
            clock: None,
            now: 0,
            pending_model: None,
            videoformat_noted: false,
            stopped: false,
            paused: false,
            reset_held: false,
            ram_snoop: false,
            ultimax: false,
            cart,
            slot: SlotHandle::default(),
            unsupported: Vec::new(),
            keys: Keys::default(),
            joysticks: [0xFF; 2],
            nmi: false,
            nmi_line: false,
            turbo_regs_en: 0x01,
            speed_prefer: 0x80,
            turbo_applied: None,
            reset_since_turbo: false,
            palette: Palette::default(),
            sid,
            drives,
            drive_sel: 0,
            reu: ReuRam::with_log(hwlog.clone()),
            reu_size_kb: reu::DEFAULT_SIZE_KB,
            parked_reu: None,
            reu_off_since: None,
            uci_off_since: None,
            uci_wait: None,
            checkpoints: Vec::new(),
            checkpoint_watch: None,
            checkpoint_exec: None,
            checkpoint_hit: None,
            sampler,
            paddles: [0; 6],
            iec,
            ring: None,
            hwlog,
        }
    }

    /// Send the SID's mono samples at `sample_rate` Hz to `sink`, following emulated time (S14 §W4-SID).
    ///
    /// S16: the sampler's voices join them on the way out. `Sid` keeps pushing whole blocks and keeps owning the
    /// clock; the mixer renders the same number of samples from the voices and forwards the sum.
    pub fn set_audio(&mut self, sample_rate: u32, sink: Box<dyn AudioSink>) {
        let mixed = self.mix_with_sampler(sample_rate, sink);
        self.sid.set_audio(sample_rate, mixed, self.m.c64_core.clk);
    }

    /// [`Trx64Backend::set_audio`] with the reSID engines, the mixing and the sink on their own thread (S20). For a
    /// live device: the sink must be `Send`, and reads of a SID register cost a round trip.
    pub fn set_audio_threaded(&mut self, sample_rate: u32, sink: Box<dyn AudioSink + Send>) {
        let mixed = self.mix_with_sampler(sample_rate, sink);
        self.sid.set_audio_threaded(sample_rate, mixed, self.m.c64_core.clk);
    }

    /// S24 M3: the UDP audio stream's tap, while the firmware has the audio stream enabled.
    pub fn set_stream_audio(&mut self, on: bool) {
        if on == self.stream_tap.is_some() {
            return;
        }
        let tap = on.then(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        self.stream_tap = tap.clone();
        self.sid.set_stream_tap(tap, self.stream_rate(), self.m.c64_core.clk);
    }

    /// Samples the tap has collected, interleaved stereo, and the tap is empty afterwards.
    pub fn take_stream_audio(&mut self) -> Vec<i16> {
        let Some(tap) = &self.stream_tap else { return Vec::new() };
        match tap.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        }
    }

    /// The sampler's voices in front of `sink` (S16 §3.4).
    fn mix_with_sampler<S: AudioSink>(&mut self, sample_rate: u32, sink: S) -> Box<sampler::SamplerMix<S>> {
        self.sampler.with(|s| s.set_sample_rate(sample_rate));
        let queue = self.sampler.with(|s| s.queue());
        Box::new(sampler::SamplerMix::new(queue, sink))
    }

    /// Fit the ARMSID in SID socket 1 (`true`) or leave the socket empty, the default (S14 §W4-SID).
    /// The SID routing as text, for the monitor's `audio` verb (S23 §5).
    pub fn sid_summary(&self) -> String {
        self.sid.summary()
    }

    pub fn set_sid_socket1(&mut self, fitted: bool) {
        self.sid.set_socket1(fitted);
        self.update_sid_map();
    }

    /// S17: hand TRX64 the SID decode table when the decode changed it (855 D2).
    fn update_sid_map(&mut self) {
        if let Some(map) = self.sid.take_map() {
            self.m.set_sid_map(map);
        }
    }

    /// Plug the cartridge, under the forced ULTIMAX decode when set, into TRX64 and re-run its PLA. Without a cartridge
    /// type and without the forced decode the slot stays empty, so TRX64 runs without any cartridge call.
    fn install_cart(&mut self) {
        // CARTSLOT: a cartridge in the physical expansion port attaches the proxy too (slot.rs).
        let attached = self.ultimax || self.cart.with(|c| c.present()) || self.slot.with(|s| s.physical().is_some());
        self.m.cartridge = attached.then(|| {
            Box::new(CartProxy::with_slot(self.cart.clone(), self.slot.clone(), self.ultimax)) as Box<dyn CartMapper>
        });
        // S28: cart RAM that the FPGA writes by address alone hears every write to $8000-$BFFF.
        let snoop = self.slot.with(|s| s.writes_ram_by_address(&self.cart));
        if snoop != self.ram_snoop {
            self.ram_snoop = snoop;
            if snoop {
                self.m.attach_expansion_also(Box::new(cart::RamSnoop::new(self.cart.clone(), self.slot.clone())));
            } else {
                self.m.detach_expansion_device::<cart::RamSnoop>();
            }
        }
        self.update_pla();
    }

    /// TRX64 recomputes its memconfig only on $00/$01 and consumed cart I/O writes (full.rs `pla_config_changed`).
    fn update_pla(&mut self) {
        let m = &mut *self.m;
        m.memconfig = m.memconfig_table[m.pla_index()];
        let (cart, ultimax) = (&self.cart, self.ultimax);
        self.slot.with(|s| s.lines_changed(cart, ultimax));
    }

    /// CARTSLOT: put a cartridge built from `crt` (the bytes of a .crt file) into the physical expansion port, in place of
    /// any cartridge there (docs/status/cart-slot.md). The C64 sees it at once; U64_CART_DETECT reports its lines.
    /// `decode` is the flash command decode of a flash cartridge.
    pub fn insert_cart(&mut self, crt: &[u8], decode: FlashDecode) -> Result<CartSlotInfo, String> {
        let mut cart = PhysicalCart::from_crt(crt, decode)?;
        cart.set_log(&self.hwlog);
        // S38: another cartridge is another machine; the anchors describe the one before (TRX64-Ultimate T20 §4.5).
        self.ring_discard();
        self.slot.with(|s| s.insert(cart));
        self.install_cart();
        self.apply_interrupts();
        Ok(self.slot.with(|s| s.info()).expect("inserted above"))
    }

    /// `--log cart` (S14): trace every access the C64 makes to the cartridge in the expansion port.
    pub fn set_log_cart(&mut self, on: bool) {
        self.slot.with(|s| s.set_trace(on));
    }

    /// CARTSLOT: take the physical cartridge out of the expansion port.
    pub fn eject_cart(&mut self) {
        if self.slot.with(|s| s.eject()).is_some() {
            // S38: as an insert, an eject replaces the machine.
            self.ring_discard();
            self.install_cart();
        }
    }

    /// After anything that may have changed the cartridge's lines or interrupt outputs.
    fn cart_changed(&mut self) {
        self.update_pla();
        self.apply_interrupts();
    }

    /// The port's NMI on TRX64's RESTORE source — which carries only RESTORE again — and the cartridges' IRQ and NMI
    /// on the expansion port (Spec 850 D6). TRX64 samples the port's lines per cycle and refreshes its source at each
    /// instruction boundary, and the UCI block's own IRQ is ORed in there.
    fn apply_interrupts(&mut self) {
        // CARTSLOT: C64_BUS_INTERNAL/EXTERNAL bit 3 route each cartridge's interrupt lines (c64.cc:1539-1592).
        let cart = &self.cart;
        let (cart_nmi, cart_irq) = self.slot.with(|s| s.interrupts(cart));
        if self.nmi != self.nmi_line {
            self.nmi_line = self.nmi;
            let clk = self.m.c64_core.clk;
            self.m.c64_int.set_nmi(INT_SRC_RESTORE, self.nmi, clk);
        }
        // Two stores the run loop samples at the next boundary; unlike the RESTORE source there is no clk to stamp,
        // so they are written unconditionally and a warm reset needs no re-arming.
        self.m.set_expansion_lines(cart_irq, cart_nmi);
    }

    /// Whether the 6510 takes an NMI at the start of its next instruction: `interrupt_check_nmi_delay`
    /// (c64_6510core.rs:411-420) on the state `do_interrupt` sees. TRX64 keeps the check private.
    fn nmi_due(&self) -> bool {
        const OPINFO_DELAYS_INTERRUPT: u32 = 1 << 8;
        let (int, opinfo) = (&self.m.c64_int, self.m.c64_core.last_opcode_info);
        let delay = INTERRUPT_DELAY + u64::from(opinfo & OPINFO_DELAYS_INTERRUPT != 0);
        int.global_pending_int & IK_NMI != 0 && opinfo & 0xFF != 0 && int.nmi_delay_cycles >= delay
    }

    /// Whole 6510 instructions up to cycle `target`. Without a cartridge this is one TRX64 run; with one the run ends
    /// where the cart's EXROM/GAME can change behind TRX64's PLA: after an I/O access that changed them, at a timed
    /// change, and instruction by instruction while a freeze waits for the interrupt (docs/status/carts.md).
    fn run_cpu(&mut self, target: u64) {
        // W4-SID: the SID tap records the CPU's SID writes with their cycle in every run.
        let plain = |m: &mut Machine, budget| m.run_for_full_capped(budget, u64::MAX, &mut sid::SidTap, |_, _, _, _, _, _, _| {});
        // UCI: with the block on the bus, a run halts after a C64 write to its control register, so the C64 can wait
        // for the firmware to handle the event ([`UCI_EVENT_WAIT`]).
        let uci = self.m.uci().is_some_and(|u| u.status().enabled);
        // A debugger's checkpoints need the same door: the watch table gates which accesses reach the observer,
        // and the observer is the only hook that can end a run (S23 §8).
        let watching = !self.checkpoints.is_empty();
        if self.m.cartridge.is_none() && !uci && !watching {
            let clk = self.m.c64_core.clk;
            plain(&mut self.m, target - clk);
            return;
        }
        if self.m.cartridge.is_none() {
            let clk = self.m.c64_core.clk;
            let events = self.uci_events();
            self.run_watched(target.saturating_sub(clk).max(1));
            self.wait_for_uci(events);
            return;
        }
        let with_uci = |m: &mut Machine, budget| {
            let mut obs = UciObserver::default();
            m.run_for_full_capped_dbg(budget, u64::MAX, None, None, Some(&UCI_WATCH), &mut obs, |_, _, _, _, _, _, _| {});
        };
        loop {
            let clk = self.m.c64_core.clk;
            if clk >= target || self.uci_wait.is_some() {
                break;
            }
            let events = self.uci_events();
            // CARTSLOT: the hints of the internal and the physical cartridge (slot.rs).
            let cart = &self.cart;
            let hints = self.slot.with(|s| s.run_hints(cart, clk));
            if hints == RunHints::default() {
                if uci {
                    with_uci(&mut self.m, target - clk);
                } else {
                    plain(&mut self.m, target - clk);
                }
            } else {
                // freezer.vhd switches the cart in after the three stack pushes, before the vector fetch.
                if hints.freeze_pending && self.nmi_due() {
                    self.cart.with(CartLogic::enter_freeze);
                    self.cart_changed();
                }
                let end = hints.deadline.map_or(target, |d| d.min(target));
                let max = if hints.freeze_pending { 1 } else { u64::MAX };
                let mut obs = CartObserver {
                    cart: self.cart.clone(),
                    slot: self.slot.clone(),
                    forced_ultimax: self.ultimax,
                    vector: None,
                    sid: self.sid.tap(),
                    checkpoints: self.checkpoints.clone(),
                    hit: None,
                };
                // With checkpoints the merged table covers this run's own needs too, so the gate never costs the
                // cart path its watch.
                let watch = match (&self.checkpoint_watch, hints.watch_io) {
                    (Some(watch), _) => Some(&**watch),
                    (None, true) => Some(&IO_WATCH),
                    (None, false) => uci.then_some(&UCI_WATCH),
                };
                let budget = end.saturating_sub(clk).max(1);
                let exec = self.checkpoint_exec.as_deref();
                let stop =
                    self.m.run_for_full_capped_dbg(budget, max, None, exec, watch, &mut obs, |_, _, _, _, _, _, _| {});
                self.note_checkpoint(stop, obs.hit);
                if let Some(vector) = obs.vector.filter(|_| self.cart.with(|c| c.run_hints().freeze_pending)) {
                    self.freeze_after_vector(vector);
                }
            }
            let (cart, clk) = (&self.cart, self.m.c64_core.clk);
            self.slot.with(|s| s.set_clk(cart, clk));
            self.cart_changed();
            self.wait_for_uci(events);
        }
    }

    /// The UCI event bits waiting for the firmware ([`UCI_EVENTS`]).
    fn uci_events(&self) -> u8 {
        self.m.uci().map_or(0, |u| u.status().status_byte & UCI_EVENTS)
    }

    /// After a run: an event the C64 raised in it (`before` = the bits pending at its start) makes the C64 wait for the
    /// firmware. Only new bits count, so a command the firmware is still working on after the wait does not hold the C64
    /// again.
    fn wait_for_uci(&mut self, before: u8) {
        let new = self.uci_events() & !before;
        if new != 0 {
            self.uci_wait = Some((self.now, new));
        }
    }

    /// The 6510 took an interrupt through the KERNAL vector before the freeze could switch the cart in (an IRQ, or an
    /// NMI `nmi_due` did not foresee): switch it in now and continue at the cart's vector. The handler's first
    /// KERNAL instruction has run.
    fn freeze_after_vector(&mut self, vector: u16) {
        self.cart.with(CartLogic::enter_freeze);
        self.cart_changed();
        let (lo, hi) = self.cart.with(|c| (c.peek(vector), c.peek(vector.wrapping_add(1))));
        if let (Some(lo), Some(hi)) = (lo, hi) {
            self.m.c64_core.reg_pc = u16::from_le_bytes([lo, hi]);
        }
    }

    /// Up to cycle `target`: the chips alone while the 6510 is held, else whole instructions, unless it waits for the
    /// firmware (UCI).
    fn run_to(&mut self, target: u64) {
        if self.stopped || self.reset_held {
            self.run_held(target);
        } else if self.uci_wait.is_none() {
            self.run_cpu(target);
            // W4-DRIVE: note what drive A wrote.
            for d in &mut self.drives {
                d.after_run(&mut self.m);
            }
        }
    }

    /// The C64's clock in Hz, as its row has it.
    fn cpu_hz(&self) -> u64 {
        u64::from(self.m.timing().cpu_hz)
    }

    /// Cycles until the VIC starts the next frame.
    fn cycles_to_frame_end(&self) -> u64 {
        let t = self.m.timing();
        let (line, cycle) = (u64::from(self.m.vic.raster_line), u64::from(self.m.vic.raster_cycle));
        (u64::from(t.lines_per_frame) * u64::from(t.cycles_per_line))
            .saturating_sub(line * u64::from(t.cycles_per_line) + cycle)
            .max(1)
    }

    /// C64_VIDEOFORMAT was written: the row its cycles field names becomes pending, or nothing is pending when the
    /// machine is on it already (S25 §1).
    fn request_model(&mut self, val: u8) {
        let name = match val & VIDEOFORMAT_CYCLES {
            VIDEOFORMAT_CYCLES_63 => "c64-pal",
            VIDEOFORMAT_CYCLES_65 => "c64-ntsc",
            _ => {
                if !self.videoformat_noted {
                    eprintln!("c64: C64_VIDEOFORMAT {val:#04x} asks for 64 cycles per line; no row has them");
                    self.videoformat_noted = true;
                }
                return;
            }
        };
        let model = model::find(name).expect("TRX64 has the PAL and NTSC rows");
        self.pending_model = (!std::ptr::eq(self.m.model(), model)).then_some(model);
    }

    /// At a frame boundary: put the machine on `model`, and move the clock and reSID to its rate (S25 §2-§3). A VIC
    /// that is not in line 0 (the run was held for the firmware, or overshot) leaves the switch pending. Whether it
    /// happened is returned.
    /// S32: each port's POT byte from its paddle registers. With the port's mouse enable set the value is a 1351's
    /// position, which a 1351 puts into POT bits 1-6 with bit 0 as noise; UE2 takes `position << 1`, the reading
    /// GEOS's 1351 driver expects (how the U64 core converts it is closed and not measured yet, S32 §4). Without it
    /// the byte is the paddle's own, as the firmware's extra fire buttons write it (0x80 released, 0x00 pressed,
    /// joystick_output.cc:26-37).
    fn apply_pots(&mut self) {
        for port in 0..2 {
            let (x, y, mouse) = (self.paddles[2 * port], self.paddles[2 * port + 1], self.paddles[4 + port] & 1 != 0);
            let pot = |v: u8| if mouse { (v & 0x7F) << 1 } else { v };
            let _ = self.m.set_pot(port as u8 + 1, pot(x), pot(y));
        }
    }

    /// S34: the audio stream's rate, derived from the video clock: PAL or NTSC (`tests/e2e/lib/streams.py:661-662`).
    fn stream_rate(&self) -> u32 {
        if self.cpu_hz() > 1_000_000 {
            47_940
        } else {
            47_983
        }
    }

    fn switch_model(&mut self, model: &'static C64Model) -> bool {
        if self.m.vic.raster_line != 0 {
            return false;
        }
        if let Err(why) = self.m.switch_model(model) {
            eprintln!("c64: switching to {}: {why}", model.name);
            return false;
        }
        self.pending_model = None;
        let clk = self.m.c64_core.clk;
        // The emulator time of this cycle on the old clock anchors the new one.
        let at = self.clock.map_or(self.now, |c| c.time_of(clk));
        self.clock = Some(Clock::new(at, clk, self.cpu_hz()));
        self.sid.set_clock(self.cpu_hz(), clk);
        // S34: the audio stream follows the video clock.
        if let Some(tap) = self.stream_tap.clone() {
            self.sid.set_stream_tap(Some(tap), self.stream_rate(), clk);
        }
        true
    }

    /// Run up to cycle `target` with the 6510 held: TRX64 clocks the chips itself (Spec 850 D7), `Hold::Cpu` with the
    /// VIC, CIAs, SID and drive 8, `Hold::Reset` with the VIC alone — the 6569 has no reset pin (S14 §4).
    /// [`Self::apply_hold`] has put the hold in; the run then spends the whole budget without executing.
    fn run_held(&mut self, target: u64) {
        let start = self.m.c64_core.clk;
        let drive_ref = self.m.drive_c64_ref;
        self.m.run_for_full_capped(target - start, u64::MAX, &mut sid::SidTap, |_, _, _, _, _, _, _| {});
        let clk = self.m.c64_core.clk;
        // DMA holds the Epyx capacitor while the 6510 is stopped (slot_slave.vhd:126); CARTSLOT: in both cartridges.
        let cart = &self.cart;
        self.slot.with(|s| s.hold_time(cart, clk - start));
        // S27 §2: `Hold::Cpu` clocked both drives already, so only the write-back check is left. `Hold::Reset` leaves
        // them standing and moves their reference along, but on the U64 each drive's own RESET bit 1
        // (`use_c64_reset`, drive_registers.vhd) decides whether the C64's reset reaches it, so they are clocked
        // here from the reference the hold skipped.
        if self.reset_held {
            self.m.drive_c64_ref = drive_ref;
            drive::run_with_chips(&mut self.m);
        }
        for d in &mut self.drives {
            d.after_run(&mut self.m);
        }
    }

    /// One run through the watch table: the UCI's control register, plus a debugger's checkpoints when there are
    /// any. The table is [`UCI_WATCH`] unless checkpoints widened it.
    fn run_watched(&mut self, budget: u64) {
        let mut obs = UciObserver { checkpoints: self.checkpoints.clone(), hit: None };
        let table: &[u8; 0x1_0000] = match &self.checkpoint_watch {
            Some(watch) => watch,
            None => &UCI_WATCH,
        };
        let exec = self.checkpoint_exec.as_deref();
        let stop =
            self.m.run_for_full_capped_dbg(budget, u64::MAX, None, exec, Some(table), &mut obs, |_, _, _, _, _, _, _| {});
        self.note_checkpoint(stop, obs.hit);
    }

    /// What a stopped run means for a checkpoint.
    ///
    /// `exec_watch` ends the run with `RunStop::Observer` and not `Breakpoint` — only the `HashSet` form gives
    /// that — and `Observer` is also what an access watch returns, the UCI's own control-register halt included.
    /// So an exec hit is a stop whose boundary PC is in the exec table, and nothing else.
    fn note_checkpoint(&mut self, stop: RunStop, access_hit: Option<u16>) {
        if access_hit.is_some() {
            self.checkpoint_hit = access_hit;
            return;
        }
        let pc = self.m.c64_core.reg_pc;
        let exec_hit = matches!(stop, RunStop::Observer | RunStop::Breakpoint(_))
            && self.checkpoint_exec.as_ref().is_some_and(|w| w[pc as usize] != 0);
        if exec_hit {
            self.checkpoint_hit = Some(pc);
        }
    }

    /// The checkpoints a debugger set (S23 §8), as (start, end, `MEMORY_OP` bits). An empty list takes the gate
    /// away again, so a machine nobody is debugging runs exactly as it did.
    ///
    /// Both observers carry the gate, so a checkpoint fires whatever the run is doing — with a cartridge on the
    /// bus as well as without one.
    pub fn set_checkpoints(&mut self, ranges: &[(u16, u16, u8)]) {
        self.checkpoints = ranges.to_vec();
        self.checkpoint_exec = ranges.iter().any(|&(_, _, ops)| ops & 0x04 != 0).then(|| {
            let mut watch = Box::new([0u8; 0x1_0000]);
            for &(start, end, ops) in ranges.iter().filter(|(_, _, ops)| ops & 0x04 != 0) {
                let _ = ops;
                for addr in start..=end {
                    watch[usize::from(addr)] = 1;
                }
            }
            watch
        });
        self.checkpoint_watch = (!ranges.is_empty()).then(|| {
            // Every base a run of this machine could otherwise have used, so one table serves all of them: the
            // UCI's control register, the cartridge's I/O window, and the checkpoints themselves. Watching more
            // than a run needs costs observer calls that answer false, never correctness.
            let mut watch = Box::new(UCI_WATCH);
            for (addr, byte) in IO_WATCH.iter().enumerate() {
                watch[addr] |= byte;
            }
            for &(start, end, _) in ranges {
                for addr in start..=end {
                    watch[usize::from(addr)] = 1;
                }
            }
            watch
        });
    }

    /// The checkpoint the last run stopped at, once.
    pub fn take_checkpoint_hit(&mut self) -> Option<u16> {
        self.checkpoint_hit.take()
    }

    /// C64_SPEED_UPDATE: hand the latched turbo settings to TRX64 as a menu set, unless this is the reset task's
    /// repeat of the applied values (see `turbo_applied`).
    fn speed_update(&mut self) {
        let set = (self.turbo_regs_en, self.speed_prefer);
        let repeat = std::mem::take(&mut self.reset_since_turbo) && self.turbo_applied == Some(set);
        if !repeat {
            self.m.set_u64_turbo(set.0, set.1);
            self.turbo_applied = Some(set);
        }
    }

    /// Spec 850 D7 — the hold TRX64 runs under. The reset line wins over C64_STOP: with CPU, CIAs and SID in reset
    /// only the VIC is clocked (S14 §4).
    fn apply_hold(&mut self) {
        self.m.set_hold(match (self.reset_held, self.stopped) {
            (true, _) => Some(Hold::Reset),
            (false, true) => Some(Hold::Cpu),
            (false, false) => None,
        });
    }

    fn apply_joysticks(&mut self) {
        let [port1, port2] = self.joysticks.map(joystick);
        (self.m.joystick1, self.m.joystick2) = (port1, port2);
    }

    fn rom(&self, rom: C64Rom) -> &[u8] {
        match rom {
            C64Rom::Basic => &self.m.basic_rom[..],
            C64Rom::Kernal => &self.m.kernal_rom[..],
            C64Rom::Char => &self.m.char_rom[..],
        }
    }
}

/// The TRX64 observer for cartridge runs: halts after an instruction whose watched I/O access changed the cart's lines,
/// and notes the vector of an interrupt taken. SID writes go on to the SID tap (W4-SID).
struct CartObserver {
    cart: CartHandle,
    /// CARTSLOT: the lines TRX64's PLA sees are both cartridges' (slot.rs).
    slot: SlotHandle,
    forced_ultimax: bool,
    vector: Option<u16>,
    sid: sid::SidTap,
    /// A debugger's checkpoints (S23 §8). A cartridge run is split by the cart's own hints and carries this
    /// observer instead of [`UciObserver`], so the gate has to be here too or a checkpoint would not fire while a
    /// cartridge is active.
    checkpoints: Vec<(u16, u16, u8)>,
    hit: Option<u16>,
}

impl Observer for CartObserver {
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        self.sid.on_bus(kind, addr, value, pc, clk, old);
    }

    fn on_interrupt(&mut self, vector: u16, _: u64) {
        self.vector = Some(vector);
    }

    fn on_access(&mut self, kind: BusKind, addr: u16, _: u8, ctx: AccessCtx) -> bool {
        if self.hit.is_none() && is_checkpoint(&self.checkpoints, kind, addr, &ctx) {
            self.hit = Some(addr);
            return true;
        }
        let (cart, forced) = (&self.cart, self.forced_ultimax);
        let lines = self.slot.with(|s| s.lines_changed(cart, forced));
        lines || (kind == BusKind::Write && is_uci_control(addr))
    }
}

/// Active-low port lines (bit 0 up, 1 down, 2 left, 3 right, 4 fire) as TRX64 joystick state (keyboard.rs:270-290).
fn joystick(lines: u8) -> JoystickState {
    let pressed = |bit: u8| lines & (1 << bit) == 0;
    JoystickState { up: pressed(0), down: pressed(1), left: pressed(2), right: pressed(3), fire: pressed(4) }
}

impl C64Backend for Trx64Backend {
    /// Whole 6510 instructions up to the target cycle; the overshoot carries into the next call (S14 §4).
    fn advance_to(&mut self, now: u64) {
        let since = std::mem::replace(&mut self.now, now);
        if self.reu_off_since.is_some_and(|since| now.saturating_sub(since) >= REU_OFF_GRACE) {
            self.reu_off_since = None;
            self.parked_reu = self.m.detach_reu();
        }
        if self.uci_off_since.is_some_and(|since| now.saturating_sub(since) >= UCI_OFF_GRACE) {
            self.uci_off_since = None;
            if let Some(u) = self.m.uci_mut() {
                u.fw_write(UCI_SLOT_ENABLE, 0);
            }
        }
        // UCI: the C64 stands still until the firmware has handled the event it raised, or the wait is over; then it
        // catches up.
        if let Some((since, bits)) = self.uci_wait {
            let pending = self.uci_events() & bits != 0;
            if !pending || now.saturating_sub(since) >= UCI_EVENT_WAIT || self.stopped || self.reset_held {
                self.uci_wait = None;
            }
        }
        // S37: while the machine pause stands no C64 cycle runs, so the CPU, VIC, CIAs, SID and drives stand and the
        // SID makes no sample of its own. The sampler is the FPGA's and runs on. S38: the same while the transport
        // holds the C64 on an anchor.
        if self.paused || self.ring_holds() {
            self.sampler.with(|s| s.advance_to(now));
            // The pause mutes (top.v:1059), and the output goes on at its rate: the stretch is zeros, not missing.
            if self.paused {
                self.sid.pause_gap(now.saturating_sub(since));
            }
            return;
        }
        let (clk, hz) = (self.m.c64_core.clk, self.cpu_hz());
        let mut target = self.clock.get_or_insert(Clock::new(now, clk, hz)).cycles(now);
        if target <= clk {
            return;
        }
        // S25 §2: a row switch waits for the frame boundary, so the slice stops there first.
        if let Some(model) = self.pending_model {
            let boundary = clk + self.cycles_to_frame_end() + BOUNDARY_SLACK;
            if boundary <= target {
                self.run_to(boundary);
                if self.switch_model(model) {
                    target = self.clock.map_or(target, |c| c.cycles(now));
                }
            }
        }
        // S38: TRX64's rings record this slice only if the C64 runs at 1 MHz with nothing holding it, and then the
        // slice stops at each frame boundary for the checkpoint ring.
        self.arm_rings();
        if self.ring_recording() {
            self.run_recording(target);
        } else {
            self.run_to(target);
        }
        self.sid.advance(self.m.c64_core.clk);
        // S16: the voices are FPGA-clocked, so they follow the emulator's clock rather than the C64's — and they run
        // whether or not anyone is listening, because a finished voice sets the status bit `audio_detect()` polls.
        self.sampler.with(|s| s.advance_to(now));
    }

    /// A release warm-resets through the cartridge and ULTIMAX state (S14 §7). TRX64's reset restarts its cycle
    /// counter, its interrupt state and drops keys and joysticks, so the clock is re-anchored and the inputs re-applied.
    /// A held NMI is not: the 6510 only takes a new edge. The cartridge's IRQ is level-triggered and is.
    fn set_reset(&mut self, held: bool) {
        self.reset_held = held;
        self.apply_hold();
        self.uci_wait = None;
        if held {
            // S38: a reset replaces the machine, so its anchors go.
            self.ring_discard();
            self.reset_since_turbo = true;
            self.sid.reset();
            // S16: the C64 reset clears the sampler's IRQ latches and nothing else — its register file has no reset
            // branch, which is why the firmware clears the voices in software on every reset (`sampler2.vhd:229-232`).
            self.sampler.with(|s| s.c64_reset());
            // S27: a drive may follow the C64's reset.
            for d in &mut self.drives {
                d.update(&mut self.m, self.stopped, true);
            }
            // CARTSLOT: the expansion port's RESET line holds the physical cartridge in its reset state, so
            // U64_CART_DETECT shows its boot lines while the firmware decides (c64.cc:1449-1464).
            self.slot.with(|s| s.reset_physical());
            self.update_pla();
            return;
        }
        self.install_cart();
        // CARTSLOT: TRX64's reset restarts its cycle counter; the physical cartridge's flash timers go on (slot.rs).
        let clk = self.m.c64_core.clk.max(self.m.clk);
        self.slot.with(|s| s.reset_release(clk));
        // REU: the reset carries the port's /RESET line to the device itself (`ExpansionDevice::reset`, TRX64 0.7.1),
        // so the REC goes to power-on while the DDR keeps every byte. The bridge used to do this after the call; it
        // does not any more.
        self.m.warm_reset();
        // A parked REU is on the same /RESET line.
        if let Some(reu) = self.parked_reu.as_mut() {
            reu.reset();
        }
        self.m.clk = self.m.c64_core.clk;
        self.sid.reanchor(self.m.c64_core.clk);
        // S27: the drives' RESET line is cut in TRX64, so `warm_reset` left them alone; their lines decide.
        for d in &mut self.drives {
            d.update(&mut self.m, self.stopped, false);
        }
        self.clock = Some(Clock::new(self.now, self.m.c64_core.clk, self.cpu_hz()));
        self.keys.cleared();
        self.keys.apply(&mut self.m.keyboard);
        self.apply_joysticks();
        // A held NMI is not re-applied: the 6510 only takes a new edge. The cartridges' IRQ and NMI are levels on the
        // expansion port, which `apply_interrupts` writes again whatever they were.
        self.nmi_line = self.nmi;
        self.apply_interrupts();
    }

    fn set_stopped(&mut self, stopped: bool) {
        self.stopped = stopped;
        self.apply_hold();
        // W4-DRIVE: drive A may stop with the C64.
        for d in &mut self.drives {
            d.update(&mut self.m, stopped, self.reset_held);
        }
    }

    fn set_ultimax(&mut self, on: bool) {
        self.ultimax = on;
        self.install_cart();
    }

    /// RESTORE-key NMI source, which TRX64's run loop leaves alone (c64_6510core.rs:155, 306).
    fn set_nmi(&mut self, level: bool) {
        self.nmi = level;
        self.apply_interrupts();
    }

    /// Through TRX64's live PLA with side effects (lib.rs:1198), RAM alone with `mem_only` (S14 §5.2).
    fn dma_read(&mut self, addr: u16, mem_only: bool) -> u8 {
        if mem_only {
            self.m.ram[usize::from(addr)]
        } else {
            if self.m.memconfig.io && sid::Sid::window(addr) {
                // Socket 1 / UltiSID 1 per the SID decode; else 0 in the SID range, the cartridge at $DE00-$DFFF.
                let clk = self.m.c64_core.clk;
                match self.sid.read(addr, clk) {
                    Some(v) => return v,
                    None if SID_WINDOW.contains(&addr) => return 0,
                    None => {}
                }
            }
            let val = self.m.read_full_live(addr);
            if self.m.cartridge.is_some() {
                self.cart_changed();
            }
            val
        }
    }

    /// Through TRX64's live bus. A SID write there reaches TRX64's register files and its write trace, whose records
    /// reach reSID at the C64's current cycle (S17 §2.3).
    fn dma_write(&mut self, addr: u16, val: u8, mem_only: bool) {
        if mem_only {
            self.m.ram[usize::from(addr)] = val;
        } else {
            self.m.write_full(addr, val);
            self.sid.dma_written(self.m.c64_core.clk);
            if self.m.cartridge.is_some() {
                self.cart_changed();
            }
        }
    }

    /// Without side effects; cartridge ROM and RAM read as not served, because no DDR is lent here.
    fn dma_peek(&self, addr: u16) -> u8 {
        let io = self.m.memconfig.io;
        match self.sid.peek(addr).filter(|_| io) {
            Some(v) => v,
            None if io && SID_WINDOW.contains(&addr) => 0,
            None => self.m.read_full(addr),
        }
    }

    fn rom_write(&mut self, rom: C64Rom, off: u16, val: u8) {
        let m = &mut *self.m;
        let image = match rom {
            C64Rom::Basic => &mut m.basic_rom[..],
            C64Rom::Kernal => &mut m.kernal_rom[..],
            C64Rom::Char => &mut m.char_rom[..],
        };
        if let Some(byte) = image.get_mut(usize::from(off)) {
            *byte = val;
        }
    }

    fn rom_read(&self, rom: C64Rom, off: u16) -> u8 {
        self.rom(rom).get(usize::from(off)).copied().unwrap_or(0)
    }

    /// The type takes effect with the reset line held (the port calls this right before a release) or as the KILL
    /// bit 1 force otherwise. The ROM is read from the lent DDR, not from `_rom`.
    fn set_cart(&mut self, type_variant: u8, _rom: &[u8]) {
        let type_variant = if cart::modelled(type_variant & cart::TYPE_MASK) {
            type_variant
        } else {
            if !self.unsupported.contains(&type_variant) {
                self.unsupported.push(type_variant);
                eprintln!("c64: cartridge type {type_variant:#04x} is not modelled (docs/status/carts.md); no cartridge");
            }
            cart::CART_TYPE_NONE
        };
        let (reset, clk) = (self.reset_held, self.m.c64_core.clk);
        // S38: another cartridge type is another machine; the anchors describe the one before (T20 §4.5).
        if self.cart.with(|c| c.type_variant()) != type_variant {
            self.ring_discard();
        }
        self.cart.with(|c| c.configure(type_variant, reset, clk));
        self.install_cart();
        self.apply_interrupts();
    }

    /// Before 3.15 the internal cartridge's ROM is 1 MB at 0x00F00000 (docs/status/carts.md, "Cartridge ROM in DDR").
    fn set_cart_rom(&mut self, rom: CartRom) {
        self.cart.with(|c| c.set_cart_rom(rom));
    }

    fn kill_cart(&mut self) {
        self.cart.with(CartLogic::kill);
        self.cart_changed();
    }

    fn cart_active(&self) -> bool {
        self.cart.with(|c| c.active())
    }

    fn set_palette_byte(&mut self, off: u8, val: u8) {
        self.palette.set_byte(off, val);
    }

    /// S17 §2.5, S29: the SID channels weight reSID, the sampler channels the sampler's pair; drive and tape have no
    /// source here.
    fn mixer_write(&mut self, off: u8, val: u8) {
        self.sid.mixer_write(off, val);
        self.sampler.with(|s| s.mixer_write(off, val));
    }

    /// The SID decode and UltiSID settings (S17), and CARTSLOT's C64_BUS_BRIDGE / C64_BUS_INTERNAL /
    /// C64_BUS_EXTERNAL, which route each cartridge onto the bus from the next access on (slot.rs).
    fn core_config_write(&mut self, off: u8, val: u8) {
        self.sid.core_config(off, val);
        self.update_sid_map();
        match off {
            // Spec 851 §6: the enable word and the preferred speed are latched and take effect on the update strobe,
            // as `setCpuSpeed` writes them (u64_config.cc:1634-1636).
            CORE_TURBOREGS_EN => self.turbo_regs_en = val,
            CORE_SPEED_PREFER => self.speed_prefer = val,
            CORE_SPEED_UPDATE => self.speed_update(),
            CORE_VIDEOFORMAT => self.request_model(val),
            // S32: the paddle values and the mouse enables of both control ports (u64.h:139-144) reach SID POTX/POTY
            // through TRX64's POT lines (Spec 876).
            CORE_PADDLE_1_X..=CORE_MOUSE_EN_2 => {
                self.paddles[usize::from(off - CORE_PADDLE_1_X)] = val;
                self.apply_pots();
            }
            // Spec 852 §5: the U64 bus multiplexer decides whether the internal IO1/IO2 range — and with it the UCI
            // window — reaches the C64 bus. `unlock_irq` sets bit 1 for exactly that (u64_config.cc:1015-1016).
            slot::CORE_BUS_INTERNAL => {
                if let Some(uci) = self.m.uci_mut() {
                    uci.set_routed(val & BUS_IO1 != 0, val & BUS_IO2 != 0);
                }
            }
            _ => {}
        }
        if self.slot.with(|s| s.core_config(off, val)) {
            self.install_cart();
            self.apply_interrupts();
        }
    }

    fn set_key(&mut self, row: u8, col: u8, down: bool) {
        self.keys.set_host(row, col, down);
        self.keys.apply(&mut self.m.keyboard);
    }

    fn set_matrix_keyb(&mut self, rows: [u8; 8]) {
        self.keys.set_matrix(rows);
        self.keys.apply(&mut self.m.keyboard);
    }

    fn set_joystick(&mut self, port: u8, lines: u8) {
        if let 1 | 2 = port {
            self.joysticks[usize::from(port - 1)] = lines;
            self.apply_joysticks();
        }
    }

    fn frame(&self) -> C64Frame {
        video::frame(&self.m, &self.palette)
    }

    /// TRX64's own frame counter (`vic.frame`), which advances once per frame whether anyone looks or not, so
    /// the UDP stream generator sends one burst per picture (S24 §4) and the window shows each one (S26).
    fn frame_counter(&self) -> u64 {
        self.m.vic.frame
    }

    fn set_stream_audio(&mut self, on: bool) {
        Trx64Backend::set_stream_audio(self, on);
    }

    fn take_stream_audio(&mut self) -> Vec<i16> {
        Trx64Backend::take_stream_audio(self)
    }

    fn lend_ddr(&mut self, mut ddr: Option<&mut [u8]>) {
        // REU: TRX64 HOLDS the store for the life of the device, while the DDR is only LENT for this access, so the
        // store cannot be the borrow — it is a second view of the very same lease (Spec 854 D3, reu.rs).
        self.reu.set_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
        // S38: the write log puts guest DDR bytes back on the same lease.
        self.hwlog.set_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
        // S16: the voices read the same DDR, on the same lease — but not through the REU's store, which is gated by
        // `C64_REU_SIZE`; a voice plays with no REU fitted at all.
        self.sampler.with(|s| s.ram()).set_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
        // S31: a 1581 controller's DMA reaches the firmware's sector buffer on the same lease.
        for d in &mut self.drives {
            d.lend_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
        }
        self.cart.with(|c| c.set_ddr(ddr));
    }

    fn eeprom_read(&self, off: u16) -> u8 {
        self.cart.with(|c| c.eeprom_read(off))
    }

    fn eeprom_write(&mut self, off: u16, val: u8) {
        self.cart.with(|c| c.eeprom_write(off, val));
    }

    fn set_freeze_button(&mut self, down: bool) {
        self.cart.with(|c| c.set_button(down));
        self.cart_changed();
    }

    // ---- REU: the RAM Expansion Unit (docs/status/reu.md) ----

    /// C64_REU_ENABLE. `attach_expansion_also` rather than `attach_expansion`, so the REU joins whatever else is on
    /// the port instead of replacing it, and `detach_reu` takes the REU back off and leaves the rest (Spec 854 §7).
    /// Neither touches the UCI block, which is the machine profile's own device in a slot of its own (Spec 852), nor
    /// the cartridge, which is `Machine::cartridge`.
    fn set_reu_enabled(&mut self, on: bool) {
        match (on, self.m.reu().is_some()) {
            // Switched back on within the grace time: it never left.
            (true, true) => self.reu_off_since = None,
            (true, false) => {
                self.reu.set_size_kb(self.reu_size_kb);
                // The REU that was switched off comes back as it was: its REC registers too, so a transfer the C64
                // was setting up when the firmware toggled the enable goes on (the "REU too small" of UltimateDemo2026
                // at 16 MHz). The store is guest DDR either way, so the bytes, a preloaded image among them
                // (reu_preloader.cc:104), were never lost.
                if let Some(reu) = self.parked_reu.take() {
                    self.m.attach_expansion_also(reu);
                    if let Some(reu) = self.m.reu_mut() {
                        reu.set_size_kb(self.reu_size_kb);
                    }
                    return;
                }
                match Reu::new_with_store(self.reu_size_kb, Box::new(self.reu.clone())) {
                    Some(reu) => self.m.attach_expansion_also(Box::new(reu)),
                    // `128 << n` is always a size TRX64 builds, so the register cannot reach this.
                    None => eprintln!("c64: TRX64 builds no REU of {} KB", self.reu_size_kb),
                }
            }
            (false, true) => {
                self.reu_off_since.get_or_insert(self.now);
            }
            _ => {}
        }
    }

    /// C64_REU_SIZE. Spec 854 D4: only how much DRAM the REC reports as fitted changes. The store is DDR and keeps
    /// its contents, so the firmware moving the setting drops neither a preloaded image nor what the C64 stashed.
    fn set_reu_size_kb(&mut self, size_kb: u32) {
        self.reu_size_kb = size_kb;
        self.reu.set_size_kb(size_kb);
        self.cart.with(|c| c.set_georam_size_kb(size_kb));
        if let Some(reu) = self.m.reu_mut() {
            reu.set_size_kb(size_kb);
        }
    }

    fn reu_attached(&self) -> bool {
        self.m.reu().is_some()
    }

    // ---- Ultimate Audio: the sampler (docs/specs/S16-ultimate-audio.md) ----

    fn has_sampler(&self) -> bool {
        true
    }

    fn sampler_read(&self, off: u16) -> u8 {
        self.sampler.with(|s| s.read(off))
    }

    fn sampler_write(&mut self, off: u16, val: u8) {
        self.sampler.with(|s| s.write(off, val));
    }

    /// `C64_SAMPLER_ENABLE`. The block stays on the port either way — this opens or closes its `$DF20-$DFFF` window
    /// and, with it, the IRQ into the C64 (`slot_to_io_bridge.vhd:51,85-88`).
    fn set_sampler_enabled(&mut self, on: bool) {
        self.sampler.with(|s| s.set_enabled(on));
    }

    // S27: drive A is TRX64's position A, drive B position B (drive.rs).
    fn drive(&mut self, unit: u8) -> Option<&mut dyn C64Drive> {
        self.drive_sel = usize::from(unit);
        (self.drive_sel < self.drives.len()).then_some(self as &mut dyn C64Drive)
    }

    /// CARTSLOT: GAME (bit 0) and EXROM (bit 1) of the physical cartridge, 0x03 without one (slot.rs).
    fn cart_detect(&self) -> u8 {
        self.slot.with(|s| s.detect())
    }

    fn cart_slot(&mut self) -> Option<&mut dyn C64CartSlot> {
        if self.slot.with(|s| s.physical().is_some()) {
            Some(self)
        } else {
            None
        }
    }

    // ---- UCI: TRX64's own block, part of the `u64` profile (Spec 852; docs/specs/S15-uci.md) ----

    /// S23: the monitor host reaches this backend, and through it TRX64's machine.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn has_uci(&self) -> bool {
        self.m.uci().is_some()
    }

    fn has_iec(&self) -> bool {
        true
    }

    fn iec_read(&mut self, off: u16) -> u8 {
        self.iec.with(|p| p.read(off))
    }

    fn iec_peek(&self, off: u16) -> u8 {
        self.iec.with(|p| p.peek(off))
    }

    fn iec_write(&mut self, off: u16, val: u8) {
        self.iec.with(|p| p.write(off, val));
    }

    /// During the grace time the firmware reads back the 0 it wrote to CMD_IF_SLOT_ENABLE.
    fn uci_read(&self, off: u16) -> u8 {
        if off < UCI_FW_RAM && off & 0xf == UCI_SLOT_ENABLE && self.uci_off_since.is_some() {
            return 0;
        }
        self.m.uci().map_or(0, |u| u.fw_read(off))
    }

    /// Switching the block off waits [`UCI_OFF_GRACE`]; switching it back on within that time means it never left.
    fn uci_write(&mut self, off: u16, val: u8) {
        let Some(u) = self.m.uci_mut() else { return };
        if off < UCI_FW_RAM && off & 0xf == UCI_SLOT_ENABLE && val & 0x80 == 0 {
            if val & 0x01 == 0 {
                if u.status().enabled {
                    self.uci_off_since.get_or_insert(self.now);
                }
                return;
            }
            self.uci_off_since = None;
        }
        u.fw_write(off, val);
    }

    fn uci_irq(&self) -> bool {
        self.m.uci().is_some_and(|u| u.fw_irq())
    }

    /// `Machine::uci_mut` hands the block any C64 reset since the last call first, so `take_events` reports it.
    fn uci_take_events(&mut self) -> UciEvents {
        let events = self.m.uci_mut().map(|u| u.take_events()).unwrap_or_default();
        UciEvents { c64_reset: events.c64_reset, unlock: events.unlock }
    }

    // ---- S37 phase B: TRX64-Ultimate's machine pause and freeze session (docs/specs/S37-trx-pixel-layer.md §5) ----

    /// TRX64 has no hold for the whole machine (`Hold::Cpu` keeps the chips running), so the pause is here:
    /// `advance_to` runs no cycle while it stands, and the release re-anchors the clock at the C64's cycle, as a reset
    /// release does.
    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if !paused {
            self.clock = Some(Clock::new(self.now, self.m.c64_core.clk, self.cpu_hz()));
        }
    }

    fn set_muted(&mut self, muted: bool) {
        self.sid.set_mute(muted, self.m.c64_core.clk);
    }

    fn frame_clocks(&self) -> u64 {
        let t = self.m.timing();
        u64::from(t.lines_per_frame) * u64::from(t.cycles_per_line) * ue2_core::time::CLOCK_HZ / self.cpu_hz()
    }

    fn frame_lines(&self) -> usize {
        usize::from(self.m.timing().lines_per_frame)
    }

    /// TRX64's I/O peek: no IRQ latch, collision or ICR cleared.
    fn io_peek(&self, addr: u16) -> u8 {
        self.m.peek_lens(addr, "io")
    }

    fn ram_peek(&self, addr: u16) -> u8 {
        self.m.ram[usize::from(addr)]
    }

    fn cpu_port(&self) -> (u8, u8) {
        (self.m.port_dir, self.m.port_data)
    }

    /// The internal and the physical cartridge's lines as the bus sharing routes them, without the forced Ultimax.
    fn cart_lines(&self) -> (bool, bool, bool) {
        let present = self.cart.with(|c| c.present()) || self.slot.with(|s| s.physical().is_some());
        let cart = &self.cart;
        let l = self.slot.with(|s| s.lines(cart, false));
        (present, l.exrom != 0, l.game != 0)
    }

    fn cia_regs(&self, cia: u8) -> [u8; 16] {
        if cia == 0 {
            self.m.cia1.c_cia
        } else {
            self.m.cia2.c_cia
        }
    }

    /// TRX64's own port function (VICE `read_ciapa`/`read_ciapb`) over CIA 1 with these port registers.
    fn cia1_ports(&self, pra: u8, ddra: u8, prb: u8, ddrb: u8) -> (u8, u8) {
        let mut c_cia = self.m.cia1.c_cia;
        (c_cia[0], c_cia[1], c_cia[2], c_cia[3]) = (pra, prb, ddra, ddrb);
        let pins = CiaPins { c_cia, old_pa: pra | !ddra, old_pb: prb | !ddrb };
        let (kb, clk, j1, j2) = (&self.m.keyboard, self.m.clk, &self.m.joystick1, &self.m.joystick2);
        (cia1_read_pa(&pins, kb, clk, j1, j2), cia1_read_pb(&pins, kb, clk, j1, j2))
    }

    /// The cartridge logic's peek, and in IO1/IO2 the port's devices (the UCI, the REU, the sampler) before it, as
    /// TRX64's `port_peek` asks them, without its open-bus fallback. A cartridge on the physical port is not read.
    fn cart_peek(&self, addr: u16) -> CartPeek {
        if self.slot.with(|s| s.physical().is_some()) {
            return CartPeek::External;
        }
        let cart = self.cart.with(|c| c.peek(addr));
        let device = if (0xDE00..0xE000).contains(&addr) {
            let profile = self.m.port_profile.as_ref().and_then(|d| d.peek(addr, cart));
            profile.or_else(|| self.m.expansion.as_ref().and_then(|d| d.peek(addr, cart)))
        } else {
            None
        };
        device.or(cart).map_or(CartPeek::Miss, CartPeek::Hit)
    }

    /// TRX64's banked write, with the forced Ultimax decode put in for the writes when I/O is not mapped (TRX64 has
    /// no host write to I/O whatever the PLA says).
    fn io_commit(&mut self, writes: &[(u16, u8)]) {
        if writes.is_empty() {
            return;
        }
        let forced = !self.m.memconfig.io;
        let ultimax = self.ultimax;
        if forced {
            self.ultimax = true;
            self.install_cart();
        }
        for &(addr, val) in writes {
            self.m.write_full(addr, val);
        }
        self.sid.dma_written(self.m.c64_core.clk);
        if forced {
            self.ultimax = ultimax;
            self.install_cart();
        }
        if self.m.cartridge.is_some() {
            self.cart_changed();
        }
    }
}

/// CARTSLOT: `cart-info`, `cart-save` and the write-back of `--cart-slot` (docs/status/cart-slot.md).
impl C64CartSlot for Trx64Backend {
    fn info(&self) -> CartSlotInfo {
        self.slot.with(|s| s.info()).unwrap_or_default()
    }

    /// At the C64's current cycle, so an erase that is due completes first.
    fn crt_image(&mut self) -> Result<Vec<u8>, String> {
        let clk = self.m.c64_core.clk.max(self.m.clk);
        self.slot.with(|s| s.crt_image(clk)).ok_or_else(|| "no cartridge in the expansion port".to_string())
    }

    fn generation(&self) -> u64 {
        self.slot.with(|s| s.physical().map_or(0, PhysicalCart::generation))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use ue2_core::time::CLOCKS_PER_MS;

    use super::*;

    /// Firmware roms directory with the seed images, or None (tests skip).
    fn roms() -> Option<PathBuf> {
        let root = std::env::var_os("UE2_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/1541ultimate"));
        let dir = root.join("roms");
        if dir.join(BASIC_ROM).is_file() {
            Some(dir)
        } else {
            eprintln!("skipping: {} not found (set UE2_FIRMWARE)", dir.join(BASIC_ROM).display());
            None
        }
    }

    /// Screen codes of `text` (upper case and punctuation).
    fn screen_codes(text: &str) -> Vec<u8> {
        text.bytes().map(|b| if b.is_ascii_uppercase() { b - b'@' } else { b }).collect()
    }

    fn screen_has(c64: &Trx64Backend, text: &str) -> bool {
        c64.frame().screen.windows(text.len()).any(|w| w == screen_codes(text))
    }

    /// Advance `ms` emulated milliseconds in 1 ms steps, as `C64Port::tick` does.
    fn run_ms(c64: &mut Trx64Backend, now: &mut u64, ms: u64) {
        for _ in 0..ms {
            *now += CLOCKS_PER_MS;
            c64.advance_to(*now);
        }
    }

    /// Reset released at 0, run until `READY.` (at most 3 s emulated).
    fn booted() -> Option<(Trx64Backend, u64)> {
        let mut c64 = Trx64Backend::new(&roms()?);
        let mut now = 0;
        c64.advance_to(now);
        c64.set_reset(true);
        run_ms(&mut c64, &mut now, 20);
        c64.set_reset(false);
        for _ in 0..3000 {
            run_ms(&mut c64, &mut now, 100);
            if screen_has(&c64, "READY.") {
                return Some((c64, now));
            }
        }
        panic!("no READY. after 3 s: {:?}", c64.frame().screen);
    }

    #[test]
    fn reset_release_boots_to_ready() {
        let Some((c64, now)) = booted() else { return };
        assert!(now <= 3000 * CLOCKS_PER_MS, "{now}");
        assert!(screen_has(&c64, "**** COMMODORE 64 BASIC V2 ****"));
        assert!(screen_has(&c64, "38911 BASIC BYTES FREE"));
        let frame = c64.frame();
        assert_eq!((frame.width, frame.height), (384, 272));
        assert_eq!((frame.indices[4 * 384 + 4], frame.indices[220 * 384 + 340]), (14, 6), "border and background");
    }

    /// S25: System Mode NTSC (C64_VIDEOFORMAT 0x2b, the firmware's default) boots an NTSC C64, and PAL (0x00) puts it
    /// back: the KERNAL's own detection answers, and the canvas follows the row.
    #[test]
    fn videoformat_picks_the_standard_the_kernal_detects() {
        let Some(roms) = roms() else { return };
        for (format, pal, height) in [(0x2B, 0, 247), (0x00, 1, 272)] {
            let mut c64 = Trx64Backend::new(&roms);
            let mut now = 0;
            c64.advance_to(now);
            c64.set_reset(true);
            c64.core_config_write(CORE_VIDEOFORMAT, format);
            run_ms(&mut c64, &mut now, 20);
            c64.set_reset(false);
            run_ms(&mut c64, &mut now, 3000);
            assert!(screen_has(&c64, "READY."), "{format:#04x}");
            assert_eq!(c64.m.ram[0x02A6], pal, "{format:#04x}: $02A6, 1 = PAL");
            assert_eq!(c64.frame().height, height, "{format:#04x}");
        }
    }

    /// TRX64 Spec 890: the reset task's repeat of the applied turbo settings leaves the reset state alone ($D031 reads
    /// $00 in registers mode, as on a C64 Ultimate); a menu change after the reset applies.
    #[test]
    fn the_reset_tasks_speed_strobe_is_not_a_menu_set() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        let strobe = |c64: &mut Trx64Backend, prefer: u8| {
            c64.core_config_write(CORE_TURBOREGS_EN, 0x01);
            c64.core_config_write(CORE_SPEED_PREFER, prefer);
            c64.core_config_write(CORE_SPEED_UPDATE, 1);
        };
        strobe(&mut c64, 0x89);
        run_ms(&mut c64, &mut now, 5);
        assert_ne!(c64.m.peek_lens(0xD031, "io"), 0x00, "the menu set applies");
        c64.set_reset(true);
        run_ms(&mut c64, &mut now, 5);
        c64.set_reset(false);
        strobe(&mut c64, 0x89);
        run_ms(&mut c64, &mut now, 5);
        assert_eq!(c64.m.peek_lens(0xD031, "io"), 0x00, "the reset task's strobe");
        strobe(&mut c64, 0x89);
        assert_ne!(c64.m.peek_lens(0xD031, "io"), 0x00, "a later set with the same values is the menu again");
        c64.set_reset(true);
        c64.set_reset(false);
        strobe(&mut c64, 0x8C);
        assert_ne!(c64.m.peek_lens(0xD031, "io"), 0x00, "new values after a reset are a menu change");
    }

    /// S25 §2-§3: a switch while the C64 runs lands at a frame boundary, and from there the clock counts at the new row's
    /// rate. PAL-60 (65 cycles, PAL encoding) is NTSC timing; a 64-cycle format names no row and changes nothing.
    #[test]
    fn a_running_switch_lands_at_a_frame_boundary_on_the_new_clock() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        run_ms(&mut c64, &mut now, 7);
        c64.core_config_write(CORE_VIDEOFORMAT, 0x2A);
        assert!(c64.pending_model.is_some(), "not at a frame boundary yet");
        run_ms(&mut c64, &mut now, 25);
        assert_eq!(c64.m.model().name, "c64-ntsc");
        assert!(c64.pending_model.is_none());
        let clk = c64.m.c64_core.clk;
        run_ms(&mut c64, &mut now, 1000);
        let second = c64.m.c64_core.clk - clk;
        assert!(second.abs_diff(1_022_730) < 100, "one second at the NTSC clock: {second}");

        c64.core_config_write(CORE_VIDEOFORMAT, 0x10);
        assert!(c64.pending_model.is_none(), "64 cycles: no row");
        c64.core_config_write(CORE_VIDEOFORMAT, 0x2A);
        assert!(c64.pending_model.is_none(), "already NTSC");
    }

    /// S37 phase B: the machine pause stands the whole C64 — cycle count, VIC frame, CIA timer, the drives'
    /// reference — while C64_STOP alone (`Hold::Cpu`) keeps the chips running; after the release the C64 runs on from
    /// the cycle it stood at.
    #[test]
    fn the_pause_stands_the_whole_machine_and_resumes_from_the_same_cycle() {
        let Some((mut c64, mut now)) = booted() else { return };
        let state = |c: &Trx64Backend| (c.m.c64_core.clk, c.m.vic.frame, c.m.peek_lens(0xDC04, "io"), c.m.drive_c64_ref);
        c64.set_stopped(true);
        let before = state(&c64);
        run_ms(&mut c64, &mut now, 50);
        let held = state(&c64);
        assert!(held.0 > before.0 && held.1 > before.1, "C64_STOP alone: the chips run");
        c64.set_paused(true);
        run_ms(&mut c64, &mut now, 100);
        assert_eq!(state(&c64), held, "paused: nothing moves");
        assert_eq!(c64.frame_clocks(), 312 * 63 * ue2_core::time::CLOCK_HZ / c64.cpu_hz(), "one PAL frame");
        c64.set_paused(false);
        c64.set_stopped(false);
        run_ms(&mut c64, &mut now, 10);
        let ran = c64.m.c64_core.clk - held.0;
        assert!(ran.abs_diff(9_852) < 100, "10 ms from the cycle it stood at: {ran}");
    }

    /// S37 phase B: what the session reads and writes of the C64: the commit's writes reach I/O whatever the CPU port
    /// maps, and CIA 1's ports follow the keys with the port registers the session holds.
    #[test]
    fn the_session_reaches_io_through_any_banking_and_scans_the_keys() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        c64.advance_to(0);
        c64.dma_write(0x0001, 0x34, false);
        assert!(!c64.m.memconfig.io, "all RAM");
        c64.io_commit(&[(0xD020, 0x02), (0xD800, 0x05)]);
        assert_eq!((c64.m.vic.regs[0x20] & 0x0F, c64.io_peek(0xD800)), (0x02, 0xF5));
        assert_eq!((c64.m.ram[0xD020], c64.m.memconfig.io), (0, false), "the RAM below and the banking as they were");
        assert_eq!(c64.cpu_port().1, 0x34);
        assert_eq!(c64.cart_lines(), (false, true, true), "no cartridge");
        assert_eq!(c64.cart_peek(0x8000), CartPeek::Miss);
        let scan = |c64: &Trx64Backend| (0..8).map(|c| c64.cia1_ports(!(1u8 << c), 0xFF, 0xFF, 0x00).1).collect::<Vec<_>>();
        assert!(scan(&c64).iter().all(|&pb| pb == 0xFF), "no key");
        c64.set_key(1, 2, true);
        assert_eq!(scan(&c64).iter().filter(|&&pb| pb != 0xFF).count(), 1, "one column sees the key");
    }

    /// S37 phase B (spec §5.8): a raster interrupt frozen and resumed ten times through the session, with Gideon's
    /// freeze protocol under PROTO and a menu between, keeps every interrupt's line and cycle and misses none.
    #[test]
    fn a_raster_irq_keeps_its_line_and_cycle_through_ten_freezes() {
        use ue2_core::devices::trx_session::{Lines, Session, SH_PROTO};
        let Some((mut c64, mut now)) = booted() else { return };
        // $C000: CIA 1 interrupts off, a raster interrupt at line $80, CIA 1 timer A free-running over one PAL frame
        // (19,656 cycles); the handler stores timer A's low byte and $D012 at an index and acknowledges.
        #[rustfmt::skip]
        let program: &[u8] = &[
            0x78,                   // sei
            0xA9, 0x7F, 0x8D, 0x0D, 0xDC, 0xAD, 0x0D, 0xDC, // CIA 1 interrupts off
            0xA9, 0x35, 0x8D, 0x14, 0x03, 0xA9, 0xC0, 0x8D, 0x15, 0x03, // ($0314) = $C035
            0xA9, 0x1B, 0x8D, 0x11, 0xD0, 0xA9, 0x80, 0x8D, 0x12, 0xD0, // line $80
            0xA9, 0x01, 0x8D, 0x1A, 0xD0, // raster interrupt on
            0xA9, 0xC7, 0x8D, 0x04, 0xDC, 0xA9, 0x4C, 0x8D, 0x05, 0xDC, // timer A latch $4CC7
            0xA9, 0x11, 0x8D, 0x0E, 0xDC, // start, force load, continuous
            0x58,                   // cli
            0x4C, 0x32, 0xC0,       // $C032: jmp $C032
            // $C035: the handler
            0xAD, 0x04, 0xDC, 0xAE, 0x00, 0xC1, 0x9D, 0x00, 0xC2, // timer A low to $C200,x
            0xAD, 0x12, 0xD0, 0x9D, 0x00, 0xC3, // $D012 to $C300,x
            0xEE, 0x00, 0xC1,       // the index
        ];
        let tail: [u8; 8] = [0xA9, 0x01, 0x8D, 0x19, 0xD0, 0x4C, 0x81, 0xEA]; // ack, jmp $EA81
        for (i, &b) in program.iter().chain(&tail).enumerate() {
            c64.dma_write(0xC000 + i as u16, b, true);
        }
        c64.dma_write(0xC100, 0, true);
        c64.m.c64_core.reg_pc = 0xC000;
        let first = c64.m.vic.frame;
        run_ms(&mut c64, &mut now, 100);
        let lines = |stop, ultimax| Lines { frz: true, stop, ultimax, reset: false };
        let mut session = Session::default();
        for _ in 0..10 {
            run_ms(&mut c64, &mut now, 37);
            // trx_video_freeze_enter: PROTO, then FRZ; stop(): C64_STOP pauses; goUltimax() starts the session.
            session.write_ctrl(1 | SH_PROTO);
            c64.set_stopped(true);
            c64.set_paused(true);
            c64.set_ultimax(true);
            session.step(lines(true, true), &mut c64);
            assert!(session.active);
            // stop() and determine_d012(): the registers, the raster interrupt acknowledged and polled for.
            for addr in [0xD012, 0xD011, 0xD01A, 0xD019] {
                session.read(addr, lines(true, true), &mut c64);
            }
            session.write(0xD01A, 0x01, lines(true, true), &mut c64);
            session.write(0xD019, 0x81, lines(true, true), &mut c64);
            session.read(0xD019, lines(true, true), &mut c64);
            // init_io().
            for (addr, val) in [(0xDD02, 0x3C), (0xDC03, 0x00), (0xDC02, 0xFF), (0xD018, 0x12), (0xD011, 0x1B), (0xD012, 0xF8), (0xD020, 0x00)] {
                session.write(addr, val, lines(true, true), &mut c64);
            }
            // The menu, listed: the keyboard scan and the screen.
            session.write_ctrl(1);
            for row in [0xFE, 0xFD, 0xFB, 0x00] {
                session.write(0xDC00, row, lines(true, true), &mut c64);
                session.read(0xDC01, lines(true, true), &mut c64);
            }
            session.write(0x0400, 0x01, lines(true, true), &mut c64);
            run_ms(&mut c64, &mut now, 20);
            // restore_io() and resume(), Gideon's protocol again: everything put back.
            session.write_ctrl(1 | SH_PROTO);
            for (addr, val) in [(0xDC00, 0x7F), (0xDC02, 0xFF), (0xDC03, 0x00), (0xDD02, 0x3F), (0xD018, 0x15), (0xD011, 0x1B), (0xD012, 0x80), (0xD020, 0x0E), (0xD019, 0x81), (0xD01A, 0x01)] {
                session.write(addr, val, lines(true, true), &mut c64);
            }
            c64.dma_write(0x0400, 0x20, true);
            c64.set_ultimax(false);
            session.step(lines(true, false), &mut c64);
            session.read(0xD000, lines(true, false), &mut c64);
            c64.set_stopped(false);
            c64.set_paused(false);
            session.step(lines(false, false), &mut c64);
            assert!(!session.active);
        }
        run_ms(&mut c64, &mut now, 100);
        // The first entry is the flag the raster had latched before `cli`.
        let n = usize::from(c64.m.ram[0xC100]);
        let frames = (c64.m.vic.frame - first) as usize;
        assert!((n - 1).abs_diff(frames) <= 1 && n > 25, "{n} interrupts in {frames} frames: none missed");
        let timer = &c64.m.ram[0xC201..0xC200 + n];
        let line = &c64.m.ram[0xC301..0xC300 + n];
        assert_eq!(line[0], 0x80);
        assert!(line.iter().all(|&l| l == line[0]), "every interrupt on the same line: {line:?}");
        assert!(timer.iter().all(|&t| t == timer[0]), "every interrupt at the same cycle: {timer:?}");
        assert_eq!(session.last_commit, 0, "the menu's writes were put back: nothing to commit");
    }

    #[test]
    fn dma_ram_io_memonly_and_sid() {
        let Some((mut c64, _)) = booted() else { return };
        c64.dma_write(0xC000, 0x5A, false);
        assert_eq!((c64.dma_read(0xC000, false), c64.dma_read(0xC000, true), c64.dma_peek(0xC000)), (0x5A, 0x5A, 0x5A));
        c64.dma_write(0xD020, 0x02, false);
        assert_eq!(c64.dma_read(0xD020, false) & 0x0F, 0x02, "border reaches the VIC");
        assert_eq!(c64.m.vic.read_reg(0x20) & 0x0F, 0x02);
        c64.dma_write(0xD021, 0x07, true);
        assert_eq!((c64.m.ram[0xD021], c64.m.vic.read_reg(0x21) & 0x0F), (0x07, 6), "MEMONLY: RAM under I/O");
        c64.dma_write(0x0001, 0x55, true);
        assert_eq!((c64.m.port_data, c64.dma_read(0x0001, true)), (0x37, 0x55), "MEMONLY: RAM under the port");
        c64.dma_write(0xD418, 0x0F, false);
        assert_eq!(
            (c64.dma_read(0xD400, false), c64.dma_read(0xD41B, false), c64.dma_peek(0xD41C)),
            (0x0F, 0, 0),
            "UltiSID 1 (default map) on reSID: bus value of the last write, OSC3, ENV3"
        );
        assert_eq!(c64.m.sid_chip_regs(0).map(|r| r[0x18]), Some(0x0F), "and TRX64's own register file");
        c64.core_config_write(0x0A, 0x01);
        assert_eq!(c64.dma_read(0xD400, false), 0x0F, "UltiSID 2 is at $D400 too");
        c64.core_config_write(0x0B, 0x01);
        assert_eq!(c64.dma_read(0xD400, false), 0, "no SID decodes $D400");
        c64.dma_write(0x0001, 0x34, false);
        c64.m.ram[0xD400] = 0x99;
        assert_eq!(c64.dma_read(0xD400, false), 0x99, "RAM with I/O banked out");
        c64.dma_write(0x0001, 0x37, false);
    }

    struct Collect(Rc<RefCell<Vec<i16>>>);

    /// Keeps the left side of each stereo frame (S29): the tests measure length, tone and level of a centred SID.
    impl AudioSink for Collect {
        fn samples(&mut self, pcm: &[i16]) {
            self.0.borrow_mut().extend(pcm.iter().step_by(2));
        }
    }

    /// W4-SID: a 6510 program's SID writes reach reSID at their cycle; the sink gets emulated time's worth of samples.
    #[test]
    fn cpu_sid_writes_play_through_resid() {
        let Some((mut c64, mut now)) = booted() else { return };
        let pcm = Rc::new(RefCell::new(Vec::new()));
        c64.set_audio(44_100, Box::new(Collect(Rc::clone(&pcm))));
        // $C000: LDA #v / STA $D4rr for volume 15, AD 0, SR $F0, 1000 Hz (F = 17029 at PAL), sawtooth + gate; JMP *.
        let mut code = Vec::new();
        for (reg, val) in [(0x18, 15), (0x05, 0), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x04, 0x21)] {
            code.extend([0xA9, val, 0x8D, reg, 0xD4]);
        }
        let [lo, hi] = (0xC000 + code.len() as u16).to_le_bytes();
        code.extend([0x4C, lo, hi]);
        for (i, &b) in code.iter().enumerate() {
            c64.dma_write(0xC000 + i as u16, b, false);
        }
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms(&mut c64, &mut now, 600);
        let pcm = pcm.borrow();
        assert!((26_440..=26_480).contains(&pcm.len()), "600 ms at 44.1 kHz: {}", pcm.len());
        let tail = &pcm[pcm.len() - 22_050..];
        let mean = tail.iter().map(|&s| f64::from(s)).sum::<f64>() / tail.len() as f64;
        let rising = tail.windows(2).filter(|w| f64::from(w[0]) < mean && f64::from(w[1]) >= mean).count();
        assert!((495..=505).contains(&rising), "{rising} periods in 0.5 s");
    }

    /// S37: TRX64-Ultimate's `c64_mute` (top.v:1059). C64_STOP alone cuts the output to zeros while the SID runs on
    /// beneath; the machine pause keeps the output going at its rate with zeros while the SID stands; each release
    /// brings the tone back at once.
    #[test]
    fn c64_mute_cuts_the_output_and_the_release_brings_the_tone_back() {
        let Some((mut c64, mut now)) = booted() else { return };
        let pcm = Rc::new(RefCell::new(Vec::new()));
        c64.set_audio(44_100, Box::new(Collect(Rc::clone(&pcm))));
        let mut code = Vec::new();
        for (reg, val) in [(0x18, 15), (0x05, 0), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x04, 0x21)] {
            code.extend([0xA9, val, 0x8D, reg, 0xD4]);
        }
        let [lo, hi] = (0xC000 + code.len() as u16).to_le_bytes();
        code.extend([0x4C, lo, hi]);
        for (i, &b) in code.iter().enumerate() {
            c64.dma_write(0xC000 + i as u16, b, false);
        }
        c64.m.c64_core.reg_pc = 0xC000;
        let mut stretch = |c64: &mut Trx64Backend, now: &mut u64| {
            pcm.borrow_mut().clear();
            run_ms(c64, now, 100);
            let got = std::mem::take(&mut *pcm.borrow_mut());
            let swing = got.iter().max().copied().unwrap_or(0) as i32 - got.iter().min().copied().unwrap_or(0) as i32;
            (got.len(), swing)
        };
        let (_, tone) = stretch(&mut c64, &mut now);
        assert!(tone > 1000, "the tone plays: {tone}");
        // C64_STOP alone: the chips run, the output is zeros.
        c64.set_stopped(true);
        c64.set_muted(true);
        let clk = c64.m.c64_core.clk;
        let (n, swing) = stretch(&mut c64, &mut now);
        assert!(n.abs_diff(4_410) <= 2 && swing == 0, "100 ms of zeros: {n} samples, swing {swing}");
        assert!(c64.m.c64_core.clk > clk, "the SID ran on");
        c64.set_muted(false);
        c64.set_stopped(false);
        assert!(stretch(&mut c64, &mut now).1 > 1000, "the release: the tone again");
        // The machine pause: no C64 cycle, and still 100 ms of zeros.
        c64.set_stopped(true);
        c64.set_muted(true);
        c64.set_paused(true);
        let clk = c64.m.c64_core.clk;
        let (n, swing) = stretch(&mut c64, &mut now);
        assert!(n.abs_diff(4_410) <= 2 && swing == 0, "paused: 100 ms of zeros: {n} samples, swing {swing}");
        assert_eq!(c64.m.c64_core.clk, clk, "the SID stood");
        c64.set_paused(false);
        c64.set_muted(false);
        c64.set_stopped(false);
        assert!(stretch(&mut c64, &mut now).1 > 1000, "the release: the tone from where it stood");
    }

    /// S17 §4.3: the SID player's two-SID map, UltiSID 2 at $D420. A 6510 program's tone there plays through UltiSID 2's
    /// reSID, and chip 0 — TRX64's own SID and UltiSID 1 — stays silent.
    #[test]
    fn ultisid_2_at_d420_plays_a_cpu_tone() {
        let Some((mut c64, mut now)) = booted() else { return };
        let pcm = Rc::new(RefCell::new(Vec::new()));
        c64.set_audio(44_100, Box::new(Collect(Rc::clone(&pcm))));
        // unmapAllSids, then SetSidAddress for UltiSID 1 and 2 (u64_config.cc:1886-1920, 2031).
        for (off, val) in [(0x08, 0x01), (0x09, 0x01), (0x0A, 0x40), (0x0B, 0x42)] {
            c64.core_config_write(off, val);
            c64.core_config_write(off + 4, 0xFE);
        }
        assert_eq!(c64.m.sid_map()[1].chip, 1, "UltiSID 2 is chip 1");
        let mut code = Vec::new();
        for (reg, val) in [(0x18, 15), (0x05, 0), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x04, 0x21)] {
            code.extend([0xA9, val, 0x8D, 0x20 + reg, 0xD4]);
        }
        let [lo, hi] = (0xC000 + code.len() as u16).to_le_bytes();
        code.extend([0x4C, lo, hi]);
        for (i, &b) in code.iter().enumerate() {
            c64.dma_write(0xC000 + i as u16, b, false);
        }
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms(&mut c64, &mut now, 600);
        let pcm = pcm.borrow();
        assert!((26_440..=26_480).contains(&pcm.len()), "600 ms at 44.1 kHz: {}", pcm.len());
        let tail = &pcm[pcm.len() - 22_050..];
        let mean = tail.iter().map(|&s| f64::from(s)).sum::<f64>() / tail.len() as f64;
        let rising = tail.windows(2).filter(|w| f64::from(w[0]) < mean && f64::from(w[1]) >= mean).count();
        assert!((495..=505).contains(&rising), "{rising} periods in 0.5 s");
        assert_eq!(c64.m.sid_chip_regs(1).map(|r| (r[0x18], r[0x04])), Some((15, 0x21)), "TRX64's chip 1");
        assert_eq!(c64.m.sid_chip_regs(0).map(|r| (r[0x18], r[0x04])), Some((0, 0)), "chip 0 untouched");
        assert_eq!(c64.sid.ultisid_regs(1, 0).map(|r| (r[0x18], r[0x01], r[0x00])), Some((15, 66, 133)));
        assert!(c64.sid.ultisid_regs(0, 0).is_none_or(|r| r == [0; 0x19]), "UltiSID 1 never got a register");
    }

    #[test]
    fn ultimax_lens() {
        let Some((mut c64, _)) = booted() else { return };
        c64.dma_write(0x1000, 0x11, false);
        c64.dma_write(0x0400, 0x22, false);
        c64.set_ultimax(true);
        assert_eq!(c64.dma_read(0x1000, false), 0xFF, "open bus");
        assert_eq!(c64.dma_read(0x0400, false), 0x22, "RAM below $1000");
        c64.dma_write(0xD020, 0x05, false);
        assert_eq!(c64.dma_read(0xD020, false) & 0x0F, 0x05, "I/O");
        c64.dma_write(0x0001, 0x30, false);
        assert_eq!(c64.dma_read(0xD020, false) & 0x0F, 0x05, "I/O whatever $01 says");
        c64.dma_write(0x0001, 0x37, false);
        c64.set_ultimax(false);
        assert_eq!(c64.dma_read(0x1000, false), 0x11);
    }

    #[test]
    fn stopped_cpu_holds_while_the_raster_runs() {
        let Some((mut c64, mut now)) = booted() else { return };
        c64.set_stopped(true);
        let (pc, clk) = (c64.m.c64_core.reg_pc, c64.m.c64_core.clk);
        c64.dma_write(0x0002, 0x77, false);
        let mut lines = Vec::new();
        for _ in 0..25 {
            run_ms(&mut c64, &mut now, 1);
            lines.push(c64.dma_read(0xD012, false));
        }
        assert_eq!(c64.m.c64_core.reg_pc, pc, "6510 held");
        assert!(c64.m.c64_core.clk >= clk + 24_000, "chips clocked");
        assert!(lines.windows(2).any(|w| w[1] < w[0]), "raster wraps within 25 ms: {lines:?}");
        assert!((0..20_000).any(|_| {
            now += 101;
            c64.advance_to(now);
            c64.dma_read(0xD012, false) == 0xFF
        }), "DetectSidImpl's $D012 poll ends (u64_config.cc:2239)");
        c64.set_stopped(false);
        run_ms(&mut c64, &mut now, 20);
        assert_ne!(c64.m.c64_core.reg_pc, pc, "runs again");
        assert_eq!(c64.dma_read(0x0002, true), 0x77);
    }

    /// DDR of guest size with cart ROM bank `b` filled with `0x40 + b` (ROML) and `0x80 + b` (ROMH).
    fn ddr() -> Vec<u8> {
        cart::tests::ddr()
    }

    const CART_ROM: usize = 0x03C0_0000;

    /// S32: the paddle registers reach TRX64's POT lines; with the mouse enable the 7-bit position is shifted into
    /// bits 1-7 as a 1351's.
    #[test]
    fn paddle_and_mouse_registers_set_the_pots() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        assert_eq!(c64.m.pot(1), None, "nothing set: TRX64's open line");
        c64.core_config_write(0x32, 0x80);
        c64.core_config_write(0x33, 0x00);
        assert_eq!(c64.m.pot(1), Some((0x80, 0x00)), "paddle bytes as written");
        c64.core_config_write(0x36, 1);
        c64.core_config_write(0x32, 0x14);
        c64.core_config_write(0x33, 0x7F);
        assert_eq!(c64.m.pot(1), Some((0x28, 0xFE)), "mouse: position << 1");
        c64.core_config_write(0x34, 0x40);
        assert_eq!(c64.m.pot(2), Some((0x40, 0x00)), "port 2 has no mouse enable");
    }

    /// S28: an Action Replay in RAM mode keeps its RAM written with ROML banked out, as the FPGA does.
    #[test]
    fn cart_ram_takes_writes_with_its_window_banked_out() {
        const CART_RAM: usize = 0x00EF_0000;
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut ddr = ddr();
        c64.lend_ddr(Some(&mut ddr));
        c64.set_reset(true);
        c64.set_cart(0x1B, &[]);
        assert!(c64.ram_snoop);
        c64.dma_write(0xDE00, 0x20, false);
        c64.dma_write(0x0001, 0x35, false);
        assert!(!c64.m.memconfig.ultimax && c64.dma_read(0x8123, false) == c64.m.ram[0x8123], "ROML banked out");
        c64.dma_write(0x8123, 0x5A, false);
        assert_eq!(c64.m.ram[0x8123], 0x5A, "C64 RAM underneath");
        c64.set_cart(0x41, &[]);
        assert!(!c64.ram_snoop, "a normal cartridge takes the snoop off");
        c64.dma_write(0x8124, 0x6B, false);
        c64.lend_ddr(None);
        assert_eq!([ddr[CART_RAM + 0x123], ddr[CART_RAM + 0x124]], [0x5A, 0]);
    }

    #[test]
    fn boot_cart_runs_and_kills_itself() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut ddr = ddr();
        ddr[CART_ROM..CART_ROM + 2].copy_from_slice(&[0x09, 0x80]);
        ddr[CART_ROM + 0x3FFC..CART_ROM + 0x4000].copy_from_slice(&[0x00, 0xE0, 0x00, 0x00]);
        c64.lend_ddr(Some(&mut ddr));
        c64.set_reset(true);
        c64.set_cart(0x41, &[]);
        assert!(c64.cart_active());
        assert_eq!(c64.dma_read(0x8000, false), 0x09, "ROML from DDR");
        c64.dma_write(0xDFFF, 0xC0, false);
        assert!(c64.cart_active(), "bits 7:6 = 11 does not kill");
        c64.dma_write(0xDFFF, 0x40, false);
        assert!(!c64.cart_active(), "boot cart `sta $dfff` with $40");
        assert_eq!(c64.dma_read(0x8000, false), c64.m.ram[0x8000], "RAM again");
        c64.set_cart(0x21, &[]);
        assert_eq!((c64.dma_read(0xE000, false), c64.dma_read(0xFFFD, false)), (0x80, 0xE0), "ULTIMAX ROMH at $E000");
        c64.kill_cart();
        assert!(!c64.cart_active());
        assert_eq!(c64.dma_read(0xE000, false), c64.m.kernal_rom[0], "KERNAL again");
        c64.set_cart(0x13, &[]);
        assert!(!c64.cart_active() && c64.m.cartridge.is_none(), "unused codes attach nothing");
        c64.set_cart(0x41, &[]);
        c64.set_reset(false);
        c64.set_cart(0x41, &[]);
        assert!(!c64.cart_active(), "KILL bit 1 without reset: forced off");
        c64.set_cart(0x00, &[]);
        assert!(c64.m.cartridge.is_none());
        c64.lend_ddr(None);
    }

    /// A C64 without ROMs running `code` at $C000 from RAM, with cartridge `type_variant` configured by a reset.
    fn running_code(type_variant: u8, code: &[u8], ddr: &mut [u8]) -> (Trx64Backend, u64) {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        c64.lend_ddr(Some(ddr));
        c64.set_reset(true);
        c64.set_cart(type_variant, &[]);
        c64.set_reset(false);
        c64.m.ram[0xC000..0xC000 + code.len()].copy_from_slice(code);
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms(&mut c64, &mut now, 1);
        (c64, now)
    }

    #[test]
    fn a_read_that_switches_the_cart_reaches_the_pla_at_once() {
        let mut ddr = ddr();
        // LDA $DE00 (KCS: 16K -> 8K), LDA $A000, STA $0400, JMP $C009.
        let code = [0xAD, 0x00, 0xDE, 0xAD, 0x00, 0xA0, 0x8D, 0x00, 0x04, 0x4C, 0x09, 0xC0];
        let (mut c64, mut now) = running_code(0x1C, &code, &mut ddr);
        run_ms(&mut c64, &mut now, 2);
        assert_eq!(c64.m.ram[0x0400], c64.m.basic_rom[0], "BASIC at $A000 in 8K mode, not ROMH $80");
        c64.lend_ddr(None);
    }

    #[test]
    fn freeze_button_enters_the_cart_through_its_nmi_vector() {
        let mut ddr = ddr();
        let romh = CART_ROM + 0x2000;
        // Handler at $E010 in ROMH: LDA #$42, STA $0400, JMP $E015. NMI vector $FFFA -> $E010.
        ddr[romh + 0x10..romh + 0x18].copy_from_slice(&[0xA9, 0x42, 0x8D, 0x00, 0x04, 0x4C, 0x15, 0xE0]);
        ddr[romh + 0x1FFA..romh + 0x1FFC].copy_from_slice(&[0x10, 0xE0]);
        // FC3 off (bit 7) with NMI released (bit 6), then a loop in RAM: LDA #$C0, STA $DFFF, JMP $C005.
        let code = [0xA9, 0xC0, 0x8D, 0xFF, 0xDF, 0x4C, 0x05, 0xC0];
        let (mut c64, mut now) = running_code(0x19, &code, &mut ddr);
        assert!(!c64.cart_active());
        c64.set_freeze_button(true);
        run_ms(&mut c64, &mut now, 2);
        assert_eq!(c64.m.ram[0x0400], 0x42, "handler ran from the cart");
        let sp = usize::from(c64.m.c64_core.reg_sp);
        let pushed = u16::from_le_bytes([c64.m.ram[0x100 + sp + 2], c64.m.ram[0x100 + sp + 3]]);
        assert!((0xC005..=0xC007).contains(&pushed), "interrupted the RAM loop: {pushed:#06x}");
        assert!(c64.cart_active() && c64.m.memconfig.ultimax);
        c64.lend_ddr(None);
    }

    /// CARTSLOT: a physical EasyFlash reached through DMA as the fork's dumper reaches it: its bus setup
    /// (app_api_impl.cc `api_c64_cart_setup_bus`), U64_CART_DETECT, the sharing registers, MEMONLY past the cartridge,
    /// the forced ULTIMAX lens, and a sector erase issued over DMA that completes while the 6510 is stopped.
    #[test]
    fn physical_easyflash_over_dma_while_stopped() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        let chips: Vec<_> = (0..4u16)
            .flat_map(|b| [(b, 0x8000, slot::tests::fill(b, 0x10), 2), (b, 0xA000, slot::tests::fill(b, 0x90), 2)])
            .collect();
        let info = c64.insert_cart(&slot::tests::crt(32, 1, 0, &chips), FlashDecode::Both).unwrap();
        assert_eq!((info.family.as_str(), info.model.as_str(), info.banks), ("EasyFlash", "trx64-flash", 64));
        assert_eq!(c64.cart_detect(), 0x02, "boots ULTIMAX: GAME low");
        assert!(c64.cart_slot().is_some());
        c64.set_stopped(true);
        for (off, val) in [(0x2A, 0x03), (0x2B, 0x00), (0x2C, 0x0F)] {
            c64.core_config_write(off, val);
        }
        c64.dma_write(0x0000, 0x2F, false);
        c64.dma_write(0x0001, 0x37, false);
        c64.dma_write(0xDE00, 2, false);
        c64.dma_write(0xDE02, 7, false);
        assert_eq!((c64.dma_read(0x8000, false), c64.dma_read(0xA000, false), c64.cart_detect()), (0x12, 0x92, 0x00), "16K bank 2");
        c64.m.ram[0x8000] = 0x77;
        assert_eq!(c64.dma_read(0x8000, true), 0x77, "MEMONLY: the RAM under ROML");
        c64.core_config_write(0x2C, 0x00);
        assert_eq!(c64.dma_read(0x8000, false), 0x77, "C64_BUS_EXTERNAL 0: the cartridge is off the bus");
        c64.core_config_write(0x2C, 0x0F);
        c64.dma_write(0xDE02, 0x04, false);
        c64.set_ultimax(true);
        c64.dma_write(0xDE00, 1, false);
        assert_eq!(c64.dma_read(0x8000, false), 0x11, "the cartridge switched off still answers ROML under the lens");
        c64.set_ultimax(false);

        c64.dma_write(0xDE02, 0x05, false);
        c64.dma_write(0xDE00, 0, false);
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0x80), (0x8555, 0xAA), (0x82AA, 0x55), (0x8000, 0x30)] {
            c64.dma_write(addr, val, false);
        }
        let mut ms = 0;
        while c64.dma_read(0x8000, false) != 0xFF {
            now += CLOCKS_PER_MS;
            c64.advance_to(now);
            ms += 1;
            assert!(ms < 2000, "the erase never finished");
        }
        assert!((1000..1100).contains(&ms), "50 + 1 000 000 cycles are about 1015 ms: {ms}");
        assert!(c64.generation() > 0 && C64CartSlot::info(&c64).dirty);
        let image = c64.crt_image().unwrap();
        assert!(PhysicalCart::from_crt(&image, FlashDecode::Both).is_ok(), "the saved CRT loads again");
        let bank0 = &image[0x40 + 0x10..0x40 + 0x10 + 0x2000];
        assert!(bank0.iter().all(|&b| b == 0xFF), "the saved CRT's first packet, ROML bank 0, is erased");
    }

    #[test]
    fn keys_joysticks_and_palette_reach_trx64() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        c64.set_key(7, 4, true);
        c64.set_matrix_keyb([0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(c64.m.keyboard.pressed_keys(), ["SPACE", "RETURN"]);
        c64.set_joystick(2, 0xEF);
        c64.set_joystick(1, 0xFE);
        c64.set_joystick(3, 0x00);
        assert_eq!((c64.m.joystick2, c64.m.joystick1), (joystick(0xEF), joystick(0xFE)));
        assert!(c64.m.joystick2.fire && !c64.m.joystick2.up && c64.m.joystick1.up);
        c64.advance_to(0);
        c64.set_reset(true);
        c64.set_reset(false);
        assert_eq!(c64.m.keyboard.pressed_keys(), ["RETURN", "SPACE"], "re-applied after the reset");
        assert!(c64.m.joystick2.fire);
        c64.set_palette_byte(0, 0x12);
        assert_eq!(c64.frame().palette[0], 0x0012_0000);
        c64.rom_write(C64Rom::Char, 0xFFF, 0xAB);
        assert_eq!((c64.rom_read(C64Rom::Char, 0xFFF), c64.m.char_rom[0xFFF]), (0xAB, 0xAB));
    }

    // ---- REU: the RAM Expansion Unit (TRX64 Spec 854, docs/status/reu.md) ----

    use reu::{DEFAULT_SIZE_KB, REU_BASE};

    /// What the `Reu` answers for an address its store says nothing backs: its own `floating_bus`, whose power-on
    /// value is 0xFF (trx64-core `reu.rs`). The store returns `None` and the device picks the byte — which is the
    /// whole point of `ExpansionRam::read` being an `Option` (854 D3).
    const FLOATING_BUS: u8 = 0xFF;

    /// `C64Port::tick`'s shape: DDR lent around the run and taken back after it. That window is the only time the
    /// REU's store has anything behind it.
    fn run_ms_lent(c64: &mut Trx64Backend, now: &mut u64, ms: u64, ddr: &mut [u8]) {
        for _ in 0..ms {
            *now += CLOCKS_PER_MS;
            c64.lend_ddr(Some(ddr));
            c64.advance_to(*now);
            c64.lend_ddr(None);
        }
    }

    /// A 6510 program for $C000 that runs one REU transfer and then loops: `cmd` is the command byte ($90 stash
    /// C64→REU, $91 fetch REU→C64, both with the $FF00 trigger disabled), between `c64_addr` and `reu_addr`, `len`
    /// bytes.
    fn reu_program(cmd: u8, c64_addr: u16, reu_addr: u32, len: u16) -> Vec<u8> {
        let ([c_lo, c_hi], [l_lo, l_hi]) = (c64_addr.to_le_bytes(), len.to_le_bytes());
        let mut code = Vec::new();
        let regs = [
            (0x02, c_lo),
            (0x03, c_hi),
            (0x04, reu_addr as u8),
            (0x05, (reu_addr >> 8) as u8),
            (0x06, (reu_addr >> 16) as u8),
            (0x07, l_lo),
            (0x08, l_hi),
            // Last: writing the command with bit 7 set starts the transfer.
            (0x01, cmd),
        ];
        for (reg, val) in regs {
            code.extend([0xA9, val, 0x8D, reg, 0xDF]);
        }
        let [lo, hi] = (0xC000 + code.len() as u16).to_le_bytes();
        code.extend([0x4C, lo, hi]);
        code
    }

    fn load(c64: &mut Trx64Backend, at: u16, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            c64.dma_write(at + i as u16, b, false);
        }
    }

    /// Run `code` from $C000 with DDR lent, as an access that reaches the C64 bus does.
    fn run_program(c64: &mut Trx64Backend, now: &mut u64, code: &[u8], ddr: &mut [u8]) {
        load(c64, 0xC000, code);
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms_lent(c64, now, 20, ddr);
    }

    /// C64_REU_ENABLE puts the REU on the port and takes it off while the machine runs, and C64_REU_SIZE moves under
    /// it — without disturbing the UCI block or the cartridge.
    #[test]
    fn enable_and_size_move_at_runtime() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut ddr = ddr();
        c64.lend_ddr(Some(&mut ddr));
        c64.set_reset(true);
        c64.set_cart(0x41, &[]);
        assert!(c64.has_uci() && c64.cart_active(), "the UCI block and a cartridge are there first");
        assert!(!c64.reu_attached());
        let ff00_before = c64.m.expansion_snoop_registered(0xFF00);

        c64.set_reu_enabled(true);
        assert_eq!(
            c64.m.reu().map(|r| (r.size_kb(), r.ram_len(), r.ram_is_owned())),
            Some((DEFAULT_SIZE_KB, DEFAULT_SIZE_KB * 1024, false)),
            "the register's reset value, over a store the host owns (854 D7)"
        );
        c64.set_reu_size_kb(512);
        assert_eq!(c64.m.reu().map(|r| (r.size_kb(), r.ram_len())), Some((512, 512 * 1024)));
        assert!(c64.reu_attached() && c64.has_uci() && c64.cart_active(), "neither the UCI nor the cartridge moved");
        assert!(c64.m.expansion_snoop_registered(0xFF00), "the REU's $FF00 trigger is snooped");

        // 854 D4: the fitted size moves without rebuilding the device or touching a byte of DDR.
        ddr[REU_BASE + 0x100] = 0x3C;
        ddr[REU_BASE + 0x10_0000] = 0x5A;
        assert_eq!(c64.m.reu().map(|r| r.ram_byte(0x10_0000)), Some(FLOATING_BUS), "above 512 KB no DRAM is fitted");
        c64.set_reu_size_kb(16384);
        assert_eq!(c64.m.reu().map(|r| r.ram_byte(0x10_0000)), Some(0x5A), "and the boundary moved");
        assert_eq!(c64.m.reu().map(|r| r.ram_byte(0x100)), Some(0x3C), "what was below it is still there");

        c64.set_reu_enabled(false);
        assert!(c64.reu_attached(), "for the grace time it stays on the bus");
        c64.advance_to(c64.now + REU_OFF_GRACE);
        assert!(!c64.reu_attached() && c64.m.reu().is_none());
        assert!(c64.has_uci() && c64.cart_active(), "detaching took only the REU (854 §7)");
        assert_eq!(c64.m.expansion_snoop_registered(0xFF00), ff00_before, "and its snoop with it");

        // It comes back onto the DDR it left: the bytes were never the device's.
        c64.set_reu_enabled(true);
        assert_eq!(c64.m.reu().map(|r| (r.size_kb(), r.ram_byte(0x100))), Some((16384, 0x3C)));
        c64.lend_ddr(None);
    }

    /// The transfer that matters: the C64 stashes into the REU and the bytes land in the firmware's DDR at
    /// `REU_MEMORY_BASE`, then it fetches them back — including a byte the host changed in between, as a preload
    /// does (reu_preloader.cc:104). One copy, and it is the firmware's.
    #[test]
    fn a_transfer_moves_bytes_between_c64_ram_and_the_firmwares_ddr() {
        let Some((mut c64, mut now)) = booted() else { return };
        let (mut ddr, pattern) = (ddr(), (0..16u8).map(|i| 0xA0 + i).collect::<Vec<_>>());
        load(&mut c64, 0x0400, &pattern);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);

        run_program(&mut c64, &mut now, &reu_program(0x90, 0x0400, 0, 16), &mut ddr);
        assert_eq!(&ddr[REU_BASE..REU_BASE + 16], &pattern[..], "the stash landed in DDR at REU_MEMORY_BASE");

        // The firmware's CPU writes that region whenever it likes; the C64 must read what it wrote.
        ddr[REU_BASE + 15] = 0x5A;
        run_program(&mut c64, &mut now, &reu_program(0x91, 0x0500, 0, 16), &mut ddr);
        let back: Vec<u8> = (0..16).map(|i| c64.dma_read(0x0500 + i, false)).collect();
        assert_eq!(back[..15], pattern[..15], "the fetch brought the stash back");
        assert_eq!(back[15], 0x5A, "and the host's own byte with it, not a copy of the REU's");
    }

    /// 854 D3 — a transfer with nothing lent completes, reads the device's floating bus, writes nothing and does not
    /// panic. Between the accesses that lend, this is the normal state.
    #[test]
    fn a_transfer_with_nothing_lent_reads_the_floating_bus_and_drops_writes() {
        let Some((mut c64, mut now)) = booted() else { return };
        let (ddr, pattern) = (ddr(), (0..16u8).map(|i| 0xA0 + i).collect::<Vec<_>>());
        load(&mut c64, 0x0400, &pattern);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);

        // `run_ms`, not `run_ms_lent`: the C64 runs with no DDR under the store.
        load(&mut c64, 0xC000, &reu_program(0x90, 0x0400, 0, 16));
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms(&mut c64, &mut now, 20);
        assert!(ddr[REU_BASE..REU_BASE + 16].iter().all(|&b| b == 0), "the stash wrote nothing");

        load(&mut c64, 0xC000, &reu_program(0x91, 0x0500, 0, 16));
        c64.m.c64_core.reg_pc = 0xC000;
        run_ms(&mut c64, &mut now, 20);
        let back: Vec<u8> = (0..16).map(|i| c64.dma_read(0x0500 + i, false)).collect();
        // The floating bus is a LATCH, not a constant: `dma_host_to_reu` ends with
        // `self.floating_bus = value`, the last byte it drove (trx64-core `reu.rs`). The stash above was dropped for
        // want of DDR, but it still drove 0xA0..0xAF onto the bus, so the latch holds 0xAF and the fetch reads that.
        // Until the store returned `None` this read 0xFF, because the store invented that byte and the device's own
        // latch never got a word in — the exact silent disagreement 854 D3's `Option` was asked for.
        let last_stashed = pattern[15];
        assert!(back.iter().all(|&b| b == last_stashed), "the fetch read the floating-bus latch: {back:?}");
        assert!(c64.reu_attached(), "the REU is still on the port and the machine ran on");
    }

    /// The expansion port's RESET line resets the REC and leaves the RAM alone (reu.c:602-615). TRX64 0.7.1 carries
    /// the line to the device itself (`ExpansionDevice::reset`), so the bridge no longer resets the REU by hand —
    /// this pins that the behaviour survived the removal, DDR contents included.
    #[test]
    fn a_c64_reset_resets_the_rec_and_keeps_the_ram() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut ddr = ddr();
        c64.lend_ddr(Some(&mut ddr));
        c64.advance_to(0);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);
        c64.m.reu_mut().unwrap().restore_registers(&[0u8; 16]);
        ddr[REU_BASE + 0x100] = 0x77;
        let fresh = Reu::new(512).unwrap().snapshot_registers();
        assert_ne!(c64.m.reu().unwrap().snapshot_registers(), fresh, "the REC is away from its reset state");

        c64.set_reset(true);
        c64.set_reset(false);
        assert_eq!(c64.m.reu().unwrap().snapshot_registers(), fresh, "the RESET line reset the REC");
        assert_eq!(c64.m.reu().unwrap().ram_byte(0x100), 0x77, "and left the RAM, which is the firmware's DDR");
        c64.lend_ddr(None);
    }

    /// The firmware's enable toggle during a cartridge change leaves the REC as it was, as the FPGA's decode gate
    /// does; a C64 reset while it is off still resets it.
    #[test]
    fn an_enable_toggle_keeps_the_rec_and_a_reset_still_clears_it() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut ddr = ddr();
        c64.lend_ddr(Some(&mut ddr));
        c64.advance_to(0);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);
        // A transfer being set up: C64 address $C000, REU address $1234, the rest as the REC has it.
        let mut regs = c64.m.reu().unwrap().snapshot_registers();
        regs[2..6].copy_from_slice(&[0x00, 0xC0, 0x34, 0x12]);
        c64.m.reu_mut().unwrap().restore_registers(&regs);
        let set_up = c64.m.reu().unwrap().snapshot_registers();

        // The firmware's 0 then 1 within the grace time: the REU never leaves the bus.
        c64.set_reu_enabled(false);
        c64.advance_to(c64.now + REU_OFF_GRACE / 2);
        assert!(c64.m.reu().is_some(), "a short pulse leaves it on the bus");
        c64.set_reu_enabled(true);
        c64.advance_to(c64.now + REU_OFF_GRACE);
        assert_eq!(c64.m.reu().unwrap().snapshot_registers(), set_up, "and the REC as it was");

        // Switched off for longer it leaves the bus, and comes back with the REC it had.
        c64.set_reu_enabled(false);
        c64.advance_to(c64.now + REU_OFF_GRACE);
        assert!(c64.m.reu().is_none(), "switched off, the REU is off the bus");
        c64.set_reu_enabled(true);
        assert_eq!(c64.m.reu().unwrap().snapshot_registers(), set_up, "the REC kept the transfer being set up");

        c64.set_reu_enabled(false);
        c64.advance_to(c64.now + REU_OFF_GRACE);
        c64.set_reu_size_kb(16384);
        c64.set_reu_enabled(true);
        assert_eq!(c64.m.reu().unwrap().size_kb(), 16384, "a size written while it was off applies");

        c64.set_reu_enabled(false);
        c64.advance_to(c64.now + REU_OFF_GRACE);
        c64.set_reset(true);
        c64.set_reset(false);
        c64.set_reu_enabled(true);
        let fresh = Reu::new(16384).unwrap().snapshot_registers();
        assert_eq!(c64.m.reu().unwrap().snapshot_registers(), fresh, "a reset while it was off reset the REC");
        c64.lend_ddr(None);
    }

    /// Issue #2: the firmware's 0-then-1 on CMD_IF_SLOT_ENABLE during a cartridge change leaves the block answering
    /// at $DF1C; a longer 0 switches it off; the bus ID write (bit 7) is not an enable.
    #[test]
    fn a_uci_enable_toggle_keeps_the_block_on_the_bus() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        c64.advance_to(0);
        let enabled = |c64: &Trx64Backend| c64.m.uci().unwrap().status().enabled;
        c64.uci_write(UCI_SLOT_ENABLE, 1);
        assert!(enabled(&c64));

        c64.uci_write(UCI_SLOT_ENABLE, 0);
        assert_eq!(c64.uci_read(UCI_SLOT_ENABLE), 0, "the firmware reads back what it wrote");
        c64.advance_to(c64.now + UCI_OFF_GRACE / 2);
        assert!(enabled(&c64), "a short pulse leaves it on the bus");
        c64.uci_write(UCI_SLOT_ENABLE, 1);
        c64.advance_to(c64.now + UCI_OFF_GRACE);
        assert!(enabled(&c64), "switched back on in time, it never left");
        assert_eq!(c64.uci_read(UCI_SLOT_ENABLE), 1);

        c64.uci_write(UCI_SLOT_ENABLE, 0x80 | 8);
        assert!(enabled(&c64), "a bus ID write is no enable");
        assert_eq!(c64.m.uci().unwrap().status().bus_id, 8);

        c64.uci_write(UCI_SLOT_ENABLE, 0);
        c64.advance_to(c64.now + UCI_OFF_GRACE);
        assert!(!enabled(&c64), "switched off for longer, it is off");
        assert_eq!(c64.uci_read(UCI_SLOT_ENABLE), 0);
        c64.uci_write(UCI_SLOT_ENABLE, 1);
        assert!(enabled(&c64));
    }

    // ---- S38: TRX64's rings (docs/specs/S38-trx64-ring.md §7) ----

    /// A booted C64 with the rings on, and the anchors it holds.
    fn ringed() -> Option<(Trx64Backend, u64)> {
        let (mut c64, now) = booted()?;
        c64.set_ring(true);
        Some((c64, now))
    }

    fn anchors(c64: &Trx64Backend) -> usize {
        c64.ring().map_or(0, |r| r.anchors.list().len())
    }

    /// At 1 MHz all three rings record, one anchor per frame; in turbo, with the 6510 stopped and in reset they
    /// stand, and they go on afterwards. A reset drops what they held.
    #[test]
    fn the_ring_records_at_1_mhz_and_stands_in_turbo_and_while_held() {
        let Some((mut c64, mut now)) = ringed() else { return };
        // Past the post-reset hold, in which the C64 is a 1 MHz 6510 whatever is set (TRX64 BUG-061).
        while c64.m.vic.u64_reset_hold > 0 {
            run_ms(&mut c64, &mut now, 100);
        }
        let n0 = anchors(&c64);
        run_ms(&mut c64, &mut now, 200);
        let (n, d) = (anchors(&c64), c64.m.delta_ring.len());
        assert!((9..=11).contains(&(n - n0)), "10 PAL frames, one anchor each: {n0} -> {n}");
        assert!(d > 10_000 && c64.m.cpu_history.len() > 10_000, "per instruction: {d}");

        // Turbo: the U64 turbo registers are on after a reset (Spec 851 D2), so $D031 selects the speed.
        c64.m.write_full(0xD031, 0x0F);
        assert!(c64.m.turbo_engaged());
        run_ms(&mut c64, &mut now, 100);
        assert_eq!((anchors(&c64), c64.m.delta_ring.len()), (n, d), "turbo: nothing recorded");
        c64.m.write_full(0xD031, 0x00);
        assert!(!c64.m.turbo_engaged());
        run_ms(&mut c64, &mut now, 100);
        let (n, d) = (anchors(&c64), c64.m.delta_ring.len());
        assert!(n > n0 + 14 && d > 0, "1 MHz again: {n}");

        c64.set_stopped(true);
        run_ms(&mut c64, &mut now, 100);
        assert_eq!((anchors(&c64), c64.m.delta_ring.len()), (n, d), "C64_STOP: nothing recorded");
        c64.set_stopped(false);
        c64.set_paused(true);
        run_ms(&mut c64, &mut now, 100);
        assert_eq!((anchors(&c64), c64.m.delta_ring.len()), (n, d), "machine pause: nothing recorded");
        c64.set_paused(false);
        run_ms(&mut c64, &mut now, 100);
        assert!(anchors(&c64) > n);

        c64.set_reset(true);
        assert_eq!((anchors(&c64), c64.m.delta_ring.len(), c64.m.cpu_history.len()), (0, 0, 0), "a reset drops them");
        run_ms(&mut c64, &mut now, 50);
        assert_eq!(anchors(&c64), 0, "held in reset: nothing recorded");
        c64.set_reset(false);
        run_ms(&mut c64, &mut now, 100);
        assert!(anchors(&c64) > 0);
    }

    /// `--ring=off`: nothing is recorded, as before S38.
    #[test]
    fn with_the_ring_off_nothing_is_recorded() {
        let Some((mut c64, mut now)) = booted() else { return };
        run_ms(&mut c64, &mut now, 100);
        assert!(c64.ring().is_none());
        assert_eq!((c64.m.delta_ring.len(), c64.m.cpu_history.len()), (0, 0));
        assert!(c64.ring_dump().is_err());
    }

    /// `goto` puts the C64 back on an anchor — RAM, registers, cycle — and holds it there while the firmware's time
    /// runs on; the release continues from the anchor and cuts the anchors after it, marks excepted.
    #[test]
    fn goto_restores_the_c64_and_holds_it_until_the_release() {
        let Some((mut c64, mut now)) = ringed() else { return };
        c64.m.ram[0xC000] = 0x11;
        run_ms(&mut c64, &mut now, 100);
        let ids = c64.ring().unwrap().ids();
        let (back, _, cycles) = ids[ids.len() - 3].clone();
        let marked = ids[ids.len() - 2].0.clone();
        c64.ring_mut().unwrap().anchors.set_mark(&marked, "keep").unwrap();
        c64.m.ram[0xC000] = 0x22;
        run_ms(&mut c64, &mut now, 100);
        let total = anchors(&c64);

        let a = c64.ring_goto(&back, RingMode::StepBack).unwrap();
        assert_eq!(a.cycles, cycles);
        assert_eq!(c64.m.c64_core.clk, cycles, "the anchor's cycle");
        assert_eq!(c64.m.ram[0xC000], 0x11, "the anchor's RAM");
        assert!(c64.ring_holds());
        let regs = |c: &Trx64Backend| (c.m.c64_core.reg_pc, c.m.c64_core.reg_a, c.m.c64_core.reg_sp, c.m.c64_core.clk);
        let held = regs(&c64);
        run_ms(&mut c64, &mut now, 50);
        assert_eq!(regs(&c64), held, "held: no C64 cycle runs");
        assert_eq!(anchors(&c64), total, "held: nothing recorded, nothing cut");

        // Twice onto the same anchor gives the same machine: a restore, not a replay.
        c64.ring_goto(&ids[ids.len() - 1].0, RingMode::StepFwd).unwrap();
        c64.ring_goto(&back, RingMode::StepBack).unwrap();
        assert_eq!(regs(&c64), held);

        let dropped = c64.ring_release();
        let left = c64.ring().unwrap().ids();
        assert_eq!(left.iter().position(|(i, _, _)| *i == back), Some(left.len() - 2), "cut after the anchor");
        assert_eq!(left.last().unwrap().0, marked, "the mark survives the cut");
        assert_eq!(dropped as usize, total - left.len());
        run_ms(&mut c64, &mut now, 10);
        let ran = c64.m.c64_core.clk - cycles;
        assert!(ran.abs_diff(9_852) < 200, "10 ms from the anchor's cycle: {ran}");
        assert!(screen_has(&c64, "READY."));
    }

    /// `goto` shows the anchor's picture at once: the frame the VIC showed when the anchor was taken, not the last one
    /// drawn live. Showing it runs nothing: the C64's state, its cycle, UE2's memories and the UCI block are what the
    /// restore left.
    #[test]
    fn goto_shows_the_anchors_picture_without_running_anything() {
        let Some((mut c64, mut now)) = ringed() else { return };
        // No cursor blink, so the screen stands still between frames.
        c64.m.ram[0xCC] = 1;
        run_ms(&mut c64, &mut now, 100);
        let shown = c64.frame().indices;
        let id = c64.ring().unwrap().ids().last().unwrap().0.clone();
        let cycles = c64.ring().unwrap().anchors.get(&id).unwrap().cycles;
        c64.m.write_full(0xD020, 2);
        run_ms(&mut c64, &mut now, 100);
        let live = c64.frame().indices;
        assert_ne!(live, shown, "the border went red");
        let (distinct, _) = c64.ring().unwrap().pictures();
        assert!(distinct >= 2, "a changed picture is a new one: {distinct}");

        c64.ring_goto(&id, RingMode::StepBack).unwrap();
        assert_eq!(c64.frame().indices, shown, "the anchor's picture");
        let notes = &c64.ring().unwrap().notes;
        assert!(!notes.iter().any(|n| n.contains("picture")), "the session kept it: {notes:?}");
        assert_eq!(c64.m.c64_core.clk, cycles);

        // Showing it again, from the kept picture and drawn from the registers, changes nothing but the picture.
        let state = |c: &Trx64Backend| {
            let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint_with(
                &c.m,
                "",
                "",
                None,
                None,
                None,
                None,
                trx64_core::c64re_snapshot::CaptureOpts { omit_framebuffer: true, omit_expansion_ram: true },
            );
            (cp, c.ue2_log(), c.cart.with(|l| l.regs()), c.m.uci().map(|u| u.status()))
        };
        let before = state(&c64);
        let note = c64.show_picture(None).expect("drawn from the registers says so");
        assert!(note.contains("no raster splits"), "{note}");
        let drawn = c64.frame().indices;
        assert_eq!((drawn[4 * 384 + 4], drawn[220 * 384 + 340]), (14, 6), "the anchor's border and background");
        assert!(state(&c64) == before, "drawing changed the machine");
        c64.ring_goto(&id, RingMode::StepBack).unwrap();
        assert_eq!(c64.frame().indices, shown);

        // The release runs on from the anchor, and the live frames follow.
        c64.ring_release();
        run_ms(&mut c64, &mut now, 100);
        assert_eq!(c64.frame().indices, shown, "the border was blue at the anchor and stays so");
    }

    /// The firmware's parts stay live through a restore: the UCI block stays enabled, the turbo enable word stays
    /// the firmware's, and the anchor's model must be the one the firmware runs.
    #[test]
    fn a_restore_keeps_the_firmwares_side() {
        let Some((mut c64, mut now)) = ringed() else { return };
        run_ms(&mut c64, &mut now, 60);
        let first = c64.ring().unwrap().ids()[0].0.clone();
        c64.uci_write(UCI_SLOT_ENABLE, 1);
        let enabled = |c: &Trx64Backend| c.m.uci().is_some_and(|u| u.status().enabled);
        assert!(enabled(&c64));
        let regs_en = c64.m.vic.u64_regs_en;
        c64.ring_goto(&first, RingMode::StepBack).unwrap();
        assert!(enabled(&c64), "the UCI block is the firmware's");
        assert_eq!(c64.m.vic.u64_regs_en, regs_en);
        c64.ring_release();

        // An anchor of another model is refused, and nothing changes.
        let mut cp = c64.ring().unwrap().anchors.restore_snapshot(&first).unwrap();
        let ntsc = model::find("c64-ntsc").unwrap();
        cp["vic"]["model"] = serde_json::json!(ntsc.vicii_id);
        let clk = c64.m.c64_core.clk;
        let err = c64.restore_c64(&mut cp, None).unwrap_err();
        assert!(err.contains("firmware runs"), "{err}");
        assert_eq!(c64.m.c64_core.clk, clk);
    }

    /// ringdump and ringload round-trip the three rings with their marks, and the load stands the C64 on the file's
    /// current anchor.
    #[test]
    fn ringdump_and_ringload_round_trip() {
        let Some((mut c64, mut now)) = ringed() else { return };
        c64.m.ram[0xC000] = 0x33;
        c64.m.ram[0xCC] = 1;
        run_ms(&mut c64, &mut now, 100);
        let ids = c64.ring().unwrap().ids();
        c64.ring_mut().unwrap().anchors.set_mark(&ids[2].0, "here").unwrap();
        let (bytes, info) = c64.ring_dump().unwrap();
        let shown = c64.frame().indices;
        assert_eq!(&bytes[..8], b"C64RERNG");
        assert_eq!(info.anchors, ids.len() as u64);
        let (head, delta, chis) = (ids.last().unwrap().clone(), c64.m.delta_ring.len(), c64.m.cpu_history.len());

        c64.m.ram[0xC000] = 0x44;
        c64.m.write_full(0xD020, 2);
        run_ms(&mut c64, &mut now, 100);
        let loaded = c64.ring_load(&bytes).unwrap();
        assert_eq!(c64.frame().indices, shown, "this session's picture of the file's anchor");
        let notes = &c64.ring().unwrap().notes;
        assert!(!notes.iter().any(|n| n.contains("picture")), "{notes:?}");
        assert_eq!(loaded.anchors, ids.len() as u64);
        assert_eq!(c64.ring().unwrap().ids(), ids);
        assert_eq!(c64.ring().unwrap().anchors.mark_id("here"), Some(ids[2].0.clone()));
        assert_eq!((c64.m.delta_ring.len(), c64.m.cpu_history.len()), (delta, chis));
        assert_eq!(c64.m.c64_core.clk, head.2, "on the current anchor");
        assert_eq!(c64.m.ram[0xC000], 0x33);
        assert!(c64.ring_holds());
    }

    // ---- S38 §4.1: UE2's part around the C64 rewinds with TRX64 ----

    /// A C64 without ROMs running `code` at `at` with cartridge `type_variant`, the rings on and guest DDR lent around
    /// every slice; `now` after 100 ms.
    fn ringed_at(type_variant: u8, at: u16, code: &[u8], ddr: &mut [u8]) -> (Trx64Backend, u64) {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        c64.lend_ddr(Some(ddr));
        c64.set_reset(true);
        c64.set_cart(type_variant, &[]);
        c64.set_reset(false);
        c64.lend_ddr(None);
        c64.m.ram[usize::from(at)..usize::from(at) + code.len()].copy_from_slice(code);
        c64.m.c64_core.reg_pc = at;
        c64.set_ring(true);
        run_ms_lent(&mut c64, &mut now, 100, ddr);
        (c64, now)
    }

    /// The newest anchor's id.
    fn newest(c64: &Trx64Backend) -> String {
        c64.ring().unwrap().ids().last().unwrap().0.clone()
    }

    /// `goto` with guest DDR lent, as the monitor does it; the notes of the restore.
    fn goto_lent(c64: &mut Trx64Backend, id: &str, ddr: &mut [u8]) -> Vec<String> {
        c64.lend_ddr(Some(ddr));
        c64.ring_goto(id, RingMode::StepBack).unwrap();
        c64.lend_ddr(None);
        c64.ring().unwrap().notes.clone()
    }

    /// Going back across a bank switch puts the bank back: the logic's registers come from the anchor, and the C64
    /// reads the old bank, before and after it runs on.
    #[test]
    fn goto_across_a_bank_switch_restores_the_bank() {
        let mut ddr = ddr();
        // Ocean 8K: LDA $C100, STA $DE00, LDA $8000, STA $0400, JMP $C000.
        let code = [0xAD, 0x00, 0xC1, 0x8D, 0x00, 0xDE, 0xAD, 0x00, 0x80, 0x8D, 0x00, 0x04, 0x4C, 0x00, 0xC0];
        let (mut c64, mut now) = {
            let mut code = code.to_vec();
            code.resize(0x101, 0);
            code[0x100] = 1;
            ringed_at(0x08, 0xC000, &code, &mut ddr)
        };
        assert_eq!(c64.m.ram[0x0400], 0x41, "bank 1");
        let a = newest(&c64);
        c64.m.ram[0xC100] = 5;
        run_ms_lent(&mut c64, &mut now, 100, &mut ddr);
        assert_eq!(c64.m.ram[0x0400], 0x45, "bank 5");

        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        c64.lend_ddr(Some(&mut ddr));
        assert_eq!(c64.dma_read(0x8000, false), 0x41, "the C64 reads the anchor's bank");
        c64.lend_ddr(None);
        c64.m.ram[0x0400] = 0;
        c64.ring_release();
        run_ms_lent(&mut c64, &mut now, 20, &mut ddr);
        assert_eq!(c64.m.ram[0x0400], 0x41, "and runs on in it");
    }

    /// Going back across a REU STASH puts the REU's bytes back from the write log, and forward again redoes them.
    #[test]
    fn goto_across_a_reu_stash_restores_the_reu_bytes() {
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_at(0x00, 0xC000, &[0x4C, 0x00, 0xC0], &mut ddr);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);
        let a_bytes: Vec<u8> = (0..16).map(|i| 0xA0 + i).collect();
        load(&mut c64, 0x0400, &a_bytes);
        run_program(&mut c64, &mut now, &reu_program(0x90, 0x0400, 0x100, 16), &mut ddr);
        assert_eq!(&ddr[REU_BASE + 0x100..REU_BASE + 0x110], &a_bytes[..]);
        let a = newest(&c64);
        let b_bytes: Vec<u8> = (0..16).map(|i| 0x50 + i).collect();
        load(&mut c64, 0x0400, &b_bytes);
        run_program(&mut c64, &mut now, &reu_program(0x90, 0x0400, 0x100, 16), &mut ddr);
        assert_eq!(&ddr[REU_BASE + 0x100..REU_BASE + 0x110], &b_bytes[..]);
        let b = newest(&c64);
        assert!(c64.ue2_log().0 >= 32, "both stashes logged: {:?}", c64.ue2_log());

        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(&ddr[REU_BASE + 0x100..REU_BASE + 0x110], &a_bytes[..], "the stash after the anchor is undone");
        c64.lend_ddr(Some(&mut ddr));
        assert_eq!(c64.m.reu().map(|r| r.ram_byte(0x100)), Some(0xA0), "and the REU reads it");
        c64.lend_ddr(None);
        goto_lent(&mut c64, &b, &mut ddr);
        assert_eq!(&ddr[REU_BASE + 0x100..REU_BASE + 0x110], &b_bytes[..], "forward redoes it");
        goto_lent(&mut c64, &a, &mut ddr);
        c64.ring_release();
        run_ms_lent(&mut c64, &mut now, 20, &mut ddr);
        assert_eq!(&ddr[REU_BASE + 0x100..REU_BASE + 0x110], &a_bytes[..], "the program at the anchor stashed A again");
    }

    /// Going back across an EasyFlash program (EAPI's `$DE09 = $65` in mode 101) puts the cart ROM's byte back in DDR.
    #[test]
    fn goto_across_an_easyflash_program_restores_the_flash_byte() {
        let mut ddr = ddr();
        // At $0800, visible in ULTIMAX: wait for $02, then mode 101, $DE09 = $65, STA $8000, $02 = 0, loop.
        let code = [
            0xA5, 0x02, 0xF0, 0xFC, 0xA9, 0x05, 0x8D, 0x02, 0xDE, 0xA9, 0x65, 0x8D, 0x09, 0xDE, 0xA5, 0x02, 0x8D, 0x00,
            0x80, 0xA9, 0x00, 0x85, 0x02, 0x4C, 0x00, 0x08,
        ];
        let (mut c64, mut now) = ringed_at(0x11, 0x0800, &code, &mut ddr);
        assert_eq!(ddr[CART_ROM], 0x40);
        let a = newest(&c64);
        c64.m.ram[0x02] = 0x5A;
        run_ms_lent(&mut c64, &mut now, 100, &mut ddr);
        assert_eq!((ddr[CART_ROM], c64.m.ram[0x02]), (0x5A, 0), "programmed");
        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(ddr[CART_ROM], 0x40, "the flash byte is the anchor's");
        c64.lend_ddr(Some(&mut ddr));
        assert_eq!(c64.dma_read(0x8000, false), 0x40, "and the C64 reads it");
        c64.lend_ddr(None);
    }

    /// GeoRAM: the page registers come from the anchor, the page's bytes from the write log.
    #[test]
    fn goto_restores_the_georam_page_and_its_bytes() {
        const GEORAM: usize = 0x0100_0000;
        let mut ddr = ddr();
        // LDA $C100, STA $DFFE, LDA $C101, STA $DE00, JMP $C000.
        let mut code = vec![0xAD, 0x00, 0xC1, 0x8D, 0xFE, 0xDF, 0xAD, 0x01, 0xC1, 0x8D, 0x00, 0xDE, 0x4C, 0x00, 0xC0];
        code.resize(0x102, 0);
        (code[0x100], code[0x101]) = (1, 0x11);
        let (mut c64, mut now) = ringed_at(0x1F, 0xC000, &code, &mut ddr);
        assert_eq!(ddr[GEORAM + 0x100], 0x11, "page 1");
        let a = newest(&c64);
        (c64.m.ram[0xC100], c64.m.ram[0xC101]) = (2, 0x22);
        run_ms_lent(&mut c64, &mut now, 100, &mut ddr);
        assert_eq!(ddr[GEORAM + 0x200], 0x22, "page 2");
        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(ddr[GEORAM + 0x200], 0, "page 2's byte undone");
        c64.lend_ddr(Some(&mut ddr));
        assert_eq!(c64.dma_read(0xDE00, false), 0x11, "the window shows page 1 again");
        c64.lend_ddr(None);
    }

    /// The UCI block comes back when neither side has a command in flight: here the C64 had written two command
    /// bytes at the anchor. With a command in flight it stays live, and the restore says so.
    #[test]
    fn goto_restores_an_idle_uci_and_keeps_a_command_in_flight_live() {
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_at(0x00, 0xC000, &[0x4C, 0x00, 0xC0], &mut ddr);
        c64.uci_write(0, 0x47);
        c64.uci_write(UCI_SLOT_ENABLE, 1);
        // The firmware has taken the reset event, as its UCI task does at once.
        c64.uci_take_events();
        c64.dma_write(0xDF1D, 0x01, false);
        c64.dma_write(0xDF1D, 0x02, false);
        run_ms_lent(&mut c64, &mut now, 60, &mut ddr);
        let len = |c: &Trx64Backend| c.m.uci().unwrap().status().command_length;
        assert_eq!(len(&c64), 2);
        let a = newest(&c64);
        c64.dma_write(0xDF1D, 0x03, false);
        run_ms_lent(&mut c64, &mut now, 60, &mut ddr);
        assert_eq!(len(&c64), 3);
        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(len(&c64), 2, "the anchor's command buffer");
        c64.ring_release();

        // PUSH_CMD: the command is the firmware's now.
        c64.dma_write(0xDF1C, 0x01, false);
        assert!(c64.m.uci().unwrap().status().new_command);
        run_ms_lent(&mut c64, &mut now, 60, &mut ddr);
        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.iter().any(|n| n.contains("UCI kept live")), "{notes:?}");
        assert!(c64.m.uci().unwrap().status().new_command, "the live block, command and all");
    }

    /// A physical EasyFlash: a byte programmed and a bank switched after the anchor are both undone, and the
    /// write-back of `--cart-slot` sees the change.
    #[test]
    fn goto_restores_a_physical_easyflash() {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        let chips: Vec<_> = (0..4u16)
            .flat_map(|b| [(b, 0x8000, slot::tests::fill(b, 0x10), 2), (b, 0xA000, slot::tests::fill(b, 0x90), 2)])
            .collect();
        c64.insert_cart(&slot::tests::crt(32, 1, 0, &chips), FlashDecode::Both).unwrap();
        c64.m.ram[0x0800..0x0803].copy_from_slice(&[0x4C, 0x00, 0x08]);
        c64.m.c64_core.reg_pc = 0x0800;
        c64.set_ring(true);
        run_ms(&mut c64, &mut now, 100);
        let a = newest(&c64);
        assert_eq!(c64.dma_read(0x8100, false), 0x10);
        for (addr, val) in [(0x8555, 0xAA), (0x82AA, 0x55), (0x8555, 0xA0), (0x8100, 0x00)] {
            c64.dma_write(addr, val, false);
        }
        c64.dma_write(0xDE00, 2, false);
        run_ms(&mut c64, &mut now, 100);
        assert_eq!(c64.dma_read(0x8000, false), 0x12, "bank 2");
        c64.dma_write(0xDE00, 0, false);
        assert_eq!(c64.dma_read(0x8100, false), 0x00, "programmed");
        c64.dma_write(0xDE00, 2, false);
        let gen = c64.generation();

        let mut ddr = ddr();
        let notes = goto_lent(&mut c64, &a, &mut ddr);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(c64.dma_read(0x8000, false), 0x10, "bank 0 again");
        assert_eq!(c64.dma_read(0x8100, false), 0x10, "the programmed byte is the anchor's");
        assert!(c64.generation() > gen, "the write-back sees a new image");
    }

    /// Another cartridge is another machine (TRX64-Ultimate T20 §4.5): a new cartridge type, or a cartridge put into
    /// or taken out of the expansion port, drops the anchors; the same type again does not.
    #[test]
    fn a_new_cartridge_drops_the_anchors() {
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_at(0x08, 0xC000, &[0x4C, 0x00, 0xC0], &mut ddr);
        assert!(anchors(&c64) > 0);
        c64.set_cart(0x08, &[]);
        assert!(anchors(&c64) > 0, "the same type");
        c64.set_cart(0x09, &[]);
        assert_eq!(anchors(&c64), 0, "another type");
        run_ms_lent(&mut c64, &mut now, 60, &mut ddr);
        assert!(anchors(&c64) > 0);
        let chips = [(0u16, 0x8000u16, slot::tests::fill(0, 0x10), 0u16)];
        c64.insert_cart(&slot::tests::crt(0, 0, 1, &chips), FlashDecode::Both).unwrap();
        assert_eq!(anchors(&c64), 0, "an insert");
        run_ms_lent(&mut c64, &mut now, 60, &mut ddr);
        c64.eject_cart();
        assert_eq!(anchors(&c64), 0, "an eject");
    }

    /// The UE2 node rides the `.c64rering`: TRX64's own loader reads the file and restores an anchor of it, and a
    /// load in the same session puts the memories back through the log.
    #[test]
    fn ringdump_carries_the_ue2_node_and_trx64_still_loads_it() {
        use trx64_core::c64re_snapshot::restore_runtime_checkpoint;
        use trx64_core::checkpoint_ring::RuntimeCheckpointRing;
        use trx64_core::ring_dump::read_ring_dump;
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_at(0x00, 0xC000, &[0x4C, 0x00, 0xC0], &mut ddr);
        c64.set_reu_size_kb(512);
        c64.set_reu_enabled(true);
        load(&mut c64, 0x0400, &[0x77; 16]);
        run_program(&mut c64, &mut now, &reu_program(0x90, 0x0400, 0, 16), &mut ddr);
        let (bytes, _) = c64.ring_dump().unwrap();
        load(&mut c64, 0x0400, &[0x99; 16]);
        run_program(&mut c64, &mut now, &reu_program(0x90, 0x0400, 0, 16), &mut ddr);

        let dump = read_ring_dump(&bytes).unwrap();
        let last = dump.checkpoint_ring.entries.last().unwrap();
        assert!(last.payload.get(UE2_NODE).is_some_and(|n| n.get("cart").is_some()), "the node is in the file");
        let ring = RuntimeCheckpointRing::from_dump(&dump.checkpoint_ring);
        let cp = ring.restore_snapshot(&last.id).unwrap();
        let mut m = Machine::new();
        assert!(restore_runtime_checkpoint(&mut m, &cp).is_ok(), "TRX64 restores an anchor with the node");

        c64.lend_ddr(Some(&mut ddr));
        c64.ring_load(&bytes).unwrap();
        c64.lend_ddr(None);
        assert!(c64.ring().unwrap().notes.is_empty(), "{:?}", c64.ring().unwrap().notes);
        assert_eq!(ddr[REU_BASE], 0x77, "same session: the log took the REU back");
    }

    // ---- S38 §7: the board's files ----

    /// A PAL C64 without ROMs (the board file is PAL), the rings on, a loop at $C000 run for 100 ms.
    fn ringed_pal(ddr: &mut [u8]) -> (Trx64Backend, u64) {
        let mut c64 = Trx64Backend::new(Path::new("/nonexistent"));
        let mut now = 0;
        c64.advance_to(now);
        c64.set_reset(true);
        c64.core_config_write(CORE_VIDEOFORMAT, 0x00);
        c64.set_reset(false);
        c64.m.ram[0xC000..0xC003].copy_from_slice(&[0x4C, 0x00, 0xC0]);
        c64.m.c64_core.reg_pc = 0xC000;
        c64.set_ring(true);
        run_ms_lent(&mut c64, &mut now, 100, ddr);
        (c64, now)
    }

    /// `ring_load` with guest DDR lent, as the monitor does it.
    fn load_lent(c64: &mut Trx64Backend, bytes: &[u8], ddr: &mut [u8]) -> trx64_core::ring_dump::RingDumpInfo {
        c64.lend_ddr(Some(ddr));
        let info = c64.ring_load(bytes).unwrap();
        c64.lend_ddr(None);
        info
    }

    /// The board's own file loads: its 9 anchors, the C64 on the current one with the RAM under the I/O from the
    /// board's node, the picture drawn from its VIC registers, notes for what the file lacks, the firmware's turbo
    /// settings live; `goto` reaches the others, and after a release the ring records as far back as before.
    #[test]
    fn a_board_file_loads_and_goes_to_its_anchors() {
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_pal(&mut ddr);
        c64.m.write_full(0xD020, 2);
        c64.m.write_full(0xD021, 0);
        run_ms_lent(&mut c64, &mut now, 40, &mut ddr);
        let prefer = c64.m.vic.u64_speed_prefer;
        let cap = c64.ring().unwrap().anchors.max_entries();

        let info = load_lent(&mut c64, &board::tests::board_file(), &mut ddr);
        assert_eq!((info.anchors, info.delta_entries, info.cpu_history), (9, 0, 0));
        assert_eq!(info.current_id.as_deref(), Some("cp_97660_97662"));
        let ids = c64.ring().unwrap().ids();
        assert_eq!(ids.len(), 9);
        assert_eq!((c64.m.c64_core.clk, c64.m.c64_core.reg_pc), (1_919_624_615, 0xE5D4));
        assert!(screen_has(&c64, "READY."), "the board's screen");
        assert_ne!(c64.m.ram[0xD800..0xDC00].iter().filter(|b| **b >= 0x10).count(), 0, "RAM under the I/O, not colour");
        let frame = c64.frame();
        assert_eq!((frame.indices[4 * 384 + 4], frame.indices[220 * 384 + 340]), (14, 6), "the board's border and background");
        let notes = c64.ring().unwrap().notes.clone();
        for want in ["a board anchor (TRX64-Ultimate)", "raster compare", "no ue2Hw node", "picture drawn", "cia1: converted", "no turbo node"] {
            assert!(notes.iter().any(|n| n.contains(want)), "{want}: {notes:?}");
        }
        assert_eq!((c64.m.vic.regs[0x30], c64.m.vic.regs[0x31], c64.m.vic.u64_speed_prefer), (0, 0, prefer));
        assert!(c64.ring().unwrap().anchors.max_entries() >= cap, "the board's cap of 9 is not the ring's");

        c64.lend_ddr(Some(&mut ddr));
        c64.ring_goto(&ids[3].0, RingMode::StepBack).unwrap();
        c64.lend_ddr(None);
        assert_eq!(c64.m.c64_core.clk, ids[3].2);
        assert_eq!(c64.m.c64_core.reg_pc, 0xE5D4);
        c64.ring_release();
        run_ms_lent(&mut c64, &mut now, 400, &mut ddr);
        assert!(anchors(&c64) > 9, "records on after the board's anchors: {}", anchors(&c64));
        assert!(c64.m.c64_core.clk > ids[3].2);
    }

    /// With the ROMs, the C64 runs on from a board anchor at READY: it takes a line typed after the release.
    #[test]
    fn a_board_anchor_runs_on_at_ready() {
        let Some(roms) = roms() else { return };
        let mut ddr = ddr();
        let mut c64 = Trx64Backend::new(&roms);
        let mut now = 0;
        c64.advance_to(now);
        c64.set_reset(true);
        c64.core_config_write(CORE_VIDEOFORMAT, 0x00);
        run_ms(&mut c64, &mut now, 20);
        c64.set_reset(false);
        run_ms(&mut c64, &mut now, 3000);
        c64.set_ring(true);
        load_lent(&mut c64, &board::tests::board_file(), &mut ddr);
        c64.ring_release();
        // POKE 1024,1 through the keyboard buffer.
        let line = b"POKE1024,1\r";
        c64.m.ram[0x0277..0x0277 + line.len()].copy_from_slice(line);
        c64.m.ram[0xC6] = line.len() as u8;
        run_ms_lent(&mut c64, &mut now, 500, &mut ddr);
        assert_eq!(c64.m.ram[0x0400], 1, "the line ran");
        assert!(screen_has(&c64, "READY."));
    }

    /// The board's format back: the anchors the board file brought (not UE2's own), stored-block gzip by the board's
    /// rules, the anchor the transport stands on as current; UE2 loads that file again. A ring without board anchors
    /// refuses.
    #[test]
    fn ringdump_board_writes_what_the_board_loads() {
        let mut ddr = ddr();
        let (mut c64, mut now) = ringed_pal(&mut ddr);
        assert!(c64.ring_dump_board().unwrap_err().contains("trx64Hw"), "UE2's own anchors only");

        load_lent(&mut c64, &board::tests::board_file(), &mut ddr);
        let ids = c64.ring().unwrap().ids();
        c64.ring_mut().unwrap().anchors.set_mark(&ids[1].0, "early").unwrap();
        c64.lend_ddr(Some(&mut ddr));
        c64.ring_goto(&ids[5].0, RingMode::StepBack).unwrap();
        c64.lend_ddr(None);
        let (bytes, info) = c64.ring_dump_board().unwrap();
        assert_eq!((info.anchors, info.marks, info.over_limit, info.not_board), (9, 1, 0, 0));
        assert_eq!(info.current_id, ids[5].0);
        let check = check_board_ring(&bytes).unwrap();
        assert_eq!((check.entries, check.current_id.as_str()), (9, ids[5].0.as_str()));

        // Run on from the anchor: UE2's anchors join the ring and stay out of the board's file.
        c64.ring_release();
        run_ms_lent(&mut c64, &mut now, 200, &mut ddr);
        let (_, info) = c64.ring_dump_board().unwrap();
        assert_eq!((info.anchors, info.marks), (6, 1), "ids 0-5 are left after the cut");
        assert!(info.not_board > 0);
        assert_eq!(info.current_id, ids[5].0, "the newest board anchor");

        // And back into UE2.
        let loaded = load_lent(&mut c64, &bytes, &mut ddr);
        assert_eq!((loaded.anchors, loaded.current_id.as_deref()), (9, Some(ids[5].0.as_str())));
        assert_eq!(c64.m.c64_core.clk, ids[5].2);
        assert_eq!(c64.ring().unwrap().anchors.mark_id("early"), Some(ids[1].0.clone()));
    }
}
