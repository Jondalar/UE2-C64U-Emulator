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

mod cart;
mod cart_eeprom;
mod clock;
mod drive;
mod keys;
mod reu;
mod sampler;
mod sid;
mod slot;
mod video;

use std::ops::RangeInclusive;
use std::path::Path;

use trx64_core::c64_6510core::{IK_NMI, INTERRUPT_DELAY, INT_SRC_RESTORE};
use trx64_core::cart::CartMapper;
use trx64_core::expansion::Hold;
use trx64_core::keyboard::JoystickState;
use trx64_core::reu::Reu;
use trx64_core::vic::SpeedProfile;
use trx64_core::{AccessCtx, BusKind, CpuHistoryRing, DeltaRing, Machine, Observer};
use ue2_core::c64host::{C64Backend, C64CartSlot, C64Drive, C64Frame, C64Rom, CartSlotInfo, UciEvents};

use cart::{CartHandle, CartLogic, CartProxy, RunHints};
use clock::Clock;
use keys::Keys;
use reu::ReuRam;
use slot::{PhysicalCart, SlotHandle};

pub use slot::FlashDecode;
use video::Palette;

pub use sid::AudioSink;
pub use cart::{CAPAB_COMMAND_INTF, CAPAB_EEPROM, CAPAB_SAMPLER};

/// C64 core config offsets the bridge acts on besides the SID and bus-sharing registers (u64.h:106,134-135). The
/// firmware writes the enable word and the preferred speed, then strobes the update (u64_config.cc:1634-1636).
const CORE_TURBOREGS_EN: u8 = 0x02;
const CORE_SPEED_PREFER: u8 = 0x2D;
const CORE_SPEED_UPDATE: u8 = 0x2E;
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

/// A TRX64 `Machine` driven by the U64 firmware's C64 registers.
pub struct Trx64Backend {
    m: Box<Machine>,
    /// Emulator clock → C64 cycles; anchored by the first `advance_to` and after every reset.
    clock: Option<Clock>,
    /// Emulator clock of the last `advance_to`.
    now: u64,
    stopped: bool,
    reset_held: bool,
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
    palette: Palette,
    /// The sockets and UltiSIDs on reSID, the mixer and the sample stream (S17).
    sid: sid::Sid,
    /// W4-DRIVE: drive A on TRX64's drive 8 (drive.rs).
    drive: drive::DriveA,
    /// REU: guest DDR at `REU_MEMORY_BASE` as the store TRX64's `Reu` holds (reu.rs, Spec 854).
    reu: ReuRam,
    /// C64_REU_SIZE in KiB, which the next attach takes. The register resets to "111" = 16 MB (c64.rs `CART_REGS`),
    /// and the firmware writes it before it writes the enable (c64.cc:315-317).
    reu_size_kb: u32,
    /// Ultimate Audio: UE2's own block, shared with the port device TRX64 holds (sampler.rs, S16).
    sampler: sampler::SamplerHandle,
}

impl Trx64Backend {
    /// A powered-on C64 (S14 §7) with BASIC, KERNAL and CHAR ROM seeded from `rom_dir` where present.
    ///
    /// Unless `TRX64_CPUHISTORY` is set, TRX64's reverse-debug rings are switched off (a per-instruction cost) and
    /// kept at one entry instead of their default depth (about 110 MB). An explicit `TRX64_CPUHISTORY` leaves both to
    /// TRX64.
    pub fn new(rom_dir: &Path) -> Self {
        let rings_off = std::env::var_os(CPUHISTORY_ENV).is_none();
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
        let drive = drive::DriveA::new(&mut m);
        // S16: the sampler is on the port for the life of the machine, as it is in the FPGA; `C64_SAMPLER_ENABLE`
        // gates the window, not the block's existence. Attaching it before any REU also settles who answers
        // `$DF20-$DFFF`: TRX64's REU mirrors its registers there, the U64's does not.
        let sampler = sampler::SamplerHandle::default();
        m.attach_expansion_also(Box::new(sampler.clone()));
        Trx64Backend {
            m,
            clock: None,
            now: 0,
            stopped: false,
            reset_held: false,
            ultimax: false,
            cart: CartHandle::default(),
            slot: SlotHandle::default(),
            unsupported: Vec::new(),
            keys: Keys::default(),
            joysticks: [0xFF; 2],
            nmi: false,
            nmi_line: false,
            turbo_regs_en: 0x01,
            speed_prefer: 0x80,
            palette: Palette::default(),
            sid,
            drive,
            reu: ReuRam::default(),
            reu_size_kb: reu::DEFAULT_SIZE_KB,
            sampler,
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

    /// The sampler's voices in front of `sink` (S16 §3.4).
    fn mix_with_sampler<S: AudioSink>(&mut self, sample_rate: u32, sink: S) -> Box<sampler::SamplerMix<S>> {
        self.sampler.with(|s| s.set_sample_rate(sample_rate));
        let queue = self.sampler.with(|s| s.queue());
        Box::new(sampler::SamplerMix::new(queue, sink))
    }

    /// Fit the ARMSID in SID socket 1 (`true`) or leave the socket empty, the default (S14 §W4-SID).
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
        let cart = PhysicalCart::from_crt(crt, decode)?;
        self.slot.with(|s| s.insert(cart));
        self.install_cart();
        self.apply_interrupts();
        Ok(self.slot.with(|s| s.info()).expect("inserted above"))
    }

    /// CARTSLOT: take the physical cartridge out of the expansion port.
    pub fn eject_cart(&mut self) {
        if self.slot.with(|s| s.eject()).is_some() {
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
        if self.m.cartridge.is_none() {
            let clk = self.m.c64_core.clk;
            plain(&mut self.m, target - clk);
            return;
        }
        loop {
            let clk = self.m.c64_core.clk;
            if clk >= target {
                break;
            }
            // CARTSLOT: the hints of the internal and the physical cartridge (slot.rs).
            let cart = &self.cart;
            let hints = self.slot.with(|s| s.run_hints(cart, clk));
            if hints == RunHints::default() {
                plain(&mut self.m, target - clk);
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
                };
                let watch = hints.watch_io.then_some(&IO_WATCH);
                let budget = end.saturating_sub(clk).max(1);
                self.m.run_for_full_capped_dbg(budget, max, None, None, watch, &mut obs, |_, _, _, _, _, _, _| {});
                if let Some(vector) = obs.vector.filter(|_| self.cart.with(|c| c.run_hints().freeze_pending)) {
                    self.freeze_after_vector(vector);
                }
            }
            let (cart, clk) = (&self.cart, self.m.c64_core.clk);
            self.slot.with(|s| s.set_clk(cart, clk));
            self.cart_changed();
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
        // W4-DRIVE: `Hold::Cpu` clocked drive 8 already, so only the write-back check is left. `Hold::Reset` leaves
        // the drive standing and moves its reference along, but on the U64 the drive's own RESET bit 1
        // (`use_c64_reset`, drive_registers.vhd) decides whether the C64's reset reaches it, so it is clocked here
        // from the reference the hold skipped, exactly as before.
        if self.reset_held {
            self.m.drive_c64_ref = drive_ref;
            self.drive.run_with_chips(&mut self.m);
        } else {
            self.drive.after_run(&mut self.m);
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
}

impl Observer for CartObserver {
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        self.sid.on_bus(kind, addr, value, pc, clk, old);
    }

    fn on_interrupt(&mut self, vector: u16, _: u64) {
        self.vector = Some(vector);
    }

    fn on_access(&mut self, _: BusKind, _: u16, _: u8, _: AccessCtx) -> bool {
        let (cart, forced) = (&self.cart, self.forced_ultimax);
        self.slot.with(|s| s.lines_changed(cart, forced))
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
        self.now = now;
        let clk = self.m.c64_core.clk;
        let target = self.clock.get_or_insert(Clock::new(now, clk)).cycles(now);
        if target <= clk {
            return;
        }
        if self.stopped || self.reset_held {
            self.run_held(target);
        } else {
            self.run_cpu(target);
            // W4-DRIVE: note what drive A wrote.
            self.drive.after_run(&mut self.m);
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
        if held {
            self.sid.reset();
            // S16: the C64 reset clears the sampler's IRQ latches and nothing else — its register file has no reset
            // branch, which is why the firmware clears the voices in software on every reset (`sampler2.vhd:229-232`).
            self.sampler.with(|s| s.c64_reset());
            // W4-DRIVE: drive A may follow the C64's reset.
            self.drive.update(&mut self.m, self.stopped, true);
            // CARTSLOT: the expansion port's RESET line holds the physical cartridge in its reset state, so
            // U64_CART_DETECT shows its boot lines while the firmware decides (c64.cc:1449-1464).
            self.slot.with(|s| s.reset_physical());
            self.update_pla();
            return;
        }
        self.install_cart();
        // W4-DRIVE: TRX64's warm reset also resets drive 8; drive A's lines decide instead.
        self.drive.before_c64_reset(&mut self.m);
        // CARTSLOT: TRX64's reset restarts its cycle counter; the physical cartridge's flash timers go on (slot.rs).
        let clk = self.m.c64_core.clk.max(self.m.clk);
        self.slot.with(|s| s.reset_release(clk));
        // REU: the reset carries the port's /RESET line to the device itself (`ExpansionDevice::reset`, TRX64 0.7.1),
        // so the REC goes to power-on while the DDR keeps every byte. The bridge used to do this after the call; it
        // does not any more.
        self.m.warm_reset();
        self.m.clk = self.m.c64_core.clk;
        self.sid.reanchor(self.m.c64_core.clk);
        self.drive.after_c64_reset(&mut self.m, self.stopped);
        self.clock = Some(Clock::new(self.now, self.m.c64_core.clk));
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
        self.drive.update(&mut self.m, stopped, self.reset_held);
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
        self.cart.with(|c| c.configure(type_variant, reset, clk));
        self.install_cart();
        self.apply_interrupts();
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

    /// S17 §2.5: the SID channels weight reSID; the sampler, drive and tape channels are not applied.
    fn mixer_write(&mut self, off: u8, val: u8) {
        self.sid.mixer_write(off, val);
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
            CORE_SPEED_UPDATE => self.m.set_u64_turbo(self.turbo_regs_en, self.speed_prefer),
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

    fn lend_ddr(&mut self, mut ddr: Option<&mut [u8]>) {
        // REU: TRX64 HOLDS the store for the life of the device, while the DDR is only LENT for this access, so the
        // store cannot be the borrow — it is a second view of the very same lease (Spec 854 D3, reu.rs).
        self.reu.set_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
        // S16: the voices read the same DDR, on the same lease — but not through the REU's store, which is gated by
        // `C64_REU_SIZE`; a voice plays with no REU fitted at all.
        self.sampler.with(|s| s.ram()).set_ddr(ddr.as_mut().map(|d| (d.as_mut_ptr(), d.len())));
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
            (true, false) => {
                self.reu.set_size_kb(self.reu_size_kb);
                // The store is guest DDR, so an REU that comes back finds the image the firmware preloaded there
                // (reu_preloader.cc:104) and everything the last one wrote.
                match Reu::new_with_store(self.reu_size_kb, Box::new(self.reu.clone())) {
                    Some(reu) => self.m.attach_expansion_also(Box::new(reu)),
                    // `128 << n` is always a size TRX64 builds, so the register cannot reach this.
                    None => eprintln!("c64: TRX64 builds no REU of {} KB", self.reu_size_kb),
                }
            }
            (false, true) => {
                self.m.detach_reu();
            }
            _ => {}
        }
    }

    /// C64_REU_SIZE. Spec 854 D4: only how much DRAM the REC reports as fitted changes. The store is DDR and keeps
    /// its contents, so the firmware moving the setting drops neither a preloaded image nor what the C64 stashed.
    fn set_reu_size_kb(&mut self, size_kb: u32) {
        self.reu_size_kb = size_kb;
        self.reu.set_size_kb(size_kb);
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

    // W4-DRIVE: drive A is TRX64's drive 8 (drive.rs); there is no drive B.
    fn drive(&mut self, unit: u8) -> Option<&mut dyn C64Drive> {
        if unit == 0 {
            Some(self)
        } else {
            None
        }
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

    fn has_uci(&self) -> bool {
        self.m.uci().is_some()
    }

    fn uci_read(&self, off: u16) -> u8 {
        self.m.uci().map_or(0, |u| u.fw_read(off))
    }

    fn uci_write(&mut self, off: u16, val: u8) {
        if let Some(u) = self.m.uci_mut() {
            u.fw_write(off, val);
        }
    }

    fn uci_irq(&self) -> bool {
        self.m.uci().is_some_and(|u| u.fw_irq())
    }

    /// `Machine::uci_mut` hands the block any C64 reset since the last call first, so `take_events` reports it.
    fn uci_take_events(&mut self) -> UciEvents {
        let events = self.m.uci_mut().map(|u| u.take_events()).unwrap_or_default();
        UciEvents { c64_reset: events.c64_reset, unlock: events.unlock }
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

    impl AudioSink for Collect {
        fn samples(&mut self, pcm: &[i16]) {
            self.0.borrow_mut().extend_from_slice(pcm);
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
}
