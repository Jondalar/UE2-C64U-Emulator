//! Machine: CPU + bus + devices + run loop. Spec: docs/specs/S02-core.md
//!
//! Loop per docs/ARCHITECTURE.md §Execution model. The idle task spins without WFI
//! (docs/hw/01-cpu-boot-memory.md §Functional model "Idle"), so emulated time advances per executed step, and
//! the FreeRTOS tick exists only as an ITU interrupt (01 H8), so `meip` follows `IrqState::line` before every
//! step.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use crate::absent::{AbsentConfig, AbsentMap, Block, Mode, NoAck};
use crate::bus::{Access, SystemBus, IO_BIT, RAM_MASK};
use crate::c64host::{C64Backend, C64CartSlot};
use crate::devices::overlay::{Overlay, REG_TRANSPARENCY};
use crate::devices::{self, c64::C64Port, itu::Itu, u64io::U64Io};
use crate::host::{DisplaySnapshot, HostInput, TrxOutput};
use crate::io::IoDevice;
use crate::loader;
use crate::symbols::Symbols;
use crate::time;

#[derive(Clone, Debug, Default)]
pub struct LogFlags {
    pub unmapped: bool,
    pub io: bool,
    pub irq: bool,
    /// Every access the C64 makes to a cartridge in the expansion port, with the clock the cartridge is given.
    /// The flash families time their erase off that clock, so a stalled one is only visible here.
    pub cart: bool,
}

#[derive(Clone, Debug)]
pub struct MachineConfig {
    /// Firmware image: `ultimate.elf` (with symbols), `ultimate.app` or a `.ue2` update file (loader.rs).
    pub elf: PathBuf,
    /// Firmware roms directory (chars.bin etc.), usually firmware/1541ultimate/roms.
    pub rom_dir: PathBuf,
    /// ITU capability word (big-endian at 0x1000000C). Default 0x34000226: the T0 word plus CAPAB_DRIVE_1541_2, which a
    /// C64 Ultimate has (its REST API lists drive B, 2026-09-23).
    pub capabilities: u32,
    /// Whether `capabilities` came from the command line. An explicit word is the user's, so the frontend does not
    /// OR its own feature bits into it (`--caps` is the only way to model a machine whose FPGA lacks one).
    pub capabilities_explicit: bool,
    /// Emulated 100 MHz clocks per executed instruction.
    pub clocks_per_insn: u64,
    /// Persistent SPI flash image (created erased if missing). None = volatile flash.
    pub flash_image: Option<PathBuf>,
    /// SD card image. None = no card inserted.
    pub sd_image: Option<PathBuf>,
    /// Devices on the USB hub ports (`devices::usb`). Advertising them needs `devices::usb::CAPAB_USB_HOST2`.
    pub usb: devices::usb::UsbConfig,
    /// Which label this board wears: the Bling Board's presence bit, and nothing else (`devices::board::Board`).
    pub board: devices::board::Board,
    /// Seed CFG_USERIF_ITYPE=1 (overlay UI) into blank flash config.
    pub overlay_ui: bool,
    /// Halt on vAssertCalled / exception handler entry / get_mem PANIC loop / illegal instruction.
    pub halt_on_fault: bool,
    /// Record the CPU trace ring ([`TraceRing`]).
    pub trace: bool,
    pub log: LogFlags,
    /// Fast-forward loops that cannot change anything until the next device event (docs/specs/S19-idle-skip.md).
    pub idle_skip: bool,
    /// `.cfg` files whose settings go into the flash config pages before the firmware runs, in order
    /// (docs/specs/S21-settings.md).
    pub settings: Vec<PathBuf>,
    /// S37: TRX64-Ultimate's IO block at 0x101A0000 with the pixel layer (`devices::trx`), on ITU high IRQ 2 in place
    /// of drive B's WD177x. [`Machine::new`] turns `Auto` into on or off from the firmware image; a machine built
    /// from parts takes `Auto` as off.
    pub trx_io: TrxIo,
    /// S41: the blocks this bitstream lacks, how their windows answer, and whether a stall halts
    /// (docs/specs/S41-core-dev-mode.md). Needs the TRX block.
    pub absent: AbsentConfig,
}

/// S37: whether the board carries the TRX bitstream's IO block (`--trx-io[=auto|on|off]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrxIo {
    /// On for TRX firmware ([`trx_firmware`]), off for any other.
    #[default]
    Auto,
    On,
    Off,
}

/// S37: `mode` with auto decided by the image's TRX `marker`, and the line that says which way it went (none for
/// stock firmware with the block off on request).
fn resolve_trx_io(mode: TrxIo, marker: Option<&str>) -> (TrxIo, Option<String>) {
    match (mode, marker) {
        (TrxIo::Auto, Some(why)) => (TrxIo::On, Some(format!("trx-io: on (TRX firmware: {why})"))),
        (TrxIo::Auto, None) => (TrxIo::Off, Some("trx-io: off (stock firmware)".into())),
        (TrxIo::On, _) => (TrxIo::On, Some("trx-io: on (--trx-io=on)".into())),
        (TrxIo::Off, Some(_)) => {
            (TrxIo::Off, Some("trx-io: off (--trx-io=off: TRX firmware on Gideon's bitstream, the ID reads 0)".into()))
        }
        (TrxIo::Off, None) => (TrxIo::Off, None),
    }
}

/// S37: the marker that makes the loaded image TRX firmware, or None for any other. The symbol `trx_lv_open`
/// (trx_lv_port.cc) when the image has symbols, else the console prefix `[TRX UI]` (trx_ui.cc) in a loaded segment.
pub fn trx_firmware(symbols: &Symbols, ram: &[u8], segments: &[(u32, u32)]) -> Option<&'static str> {
    if symbols.syms.iter().any(|s| s.name == "trx_lv_open" || s.name.starts_with("trx_lv_open(")) {
        return Some("trx_lv_open");
    }
    const UI: &[u8] = b"[TRX UI]";
    let found = segments.iter().any(|&(addr, len)| {
        let start = (addr & RAM_MASK) as usize;
        ram.get(start..start.saturating_add(len as usize).min(ram.len()))
            .is_some_and(|seg| seg.windows(UI.len()).any(|w| w == UI))
    });
    found.then_some("\"[TRX UI]\" in the image")
}

impl MachineConfig {
    pub fn new(elf: PathBuf, rom_dir: PathBuf) -> Self {
        MachineConfig {
            elf,
            rom_dir,
            capabilities: 0x3400_0226,
            capabilities_explicit: false,
            clocks_per_insn: time::DEFAULT_CLOCKS_PER_INSN,
            flash_image: None,
            sd_image: None,
            usb: devices::usb::UsbConfig::default(),
            board: devices::board::Board::default(),
            overlay_ui: true,
            halt_on_fault: true,
            trace: false,
            log: LogFlags::default(),
            idle_skip: true,
            settings: Vec::new(),
            trx_io: TrxIo::Auto,
            absent: AbsentConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// Instruction budget used up.
    Budget,
    /// PC reached a breakpoint (instruction not yet executed).
    Breakpoint(u32),
    /// Firmware fault hook fired, illegal instruction, or an access to an absent block that never completes (S41).
    Halted(String),
}

/// [`FaultHooks`] address of an absent hook. It never equals a PC: every jump target has bits 1:0 cleared
/// (fetch.vhd:58) and the ELF entry is word aligned.
const NO_HOOK: u32 = u32::MAX;

/// `j .` (`jal x0, 0`).
const JUMP_TO_SELF: u32 = 0x0000_006F;

/// Fault-hook addresses, resolved from ELF symbols when `halt_on_fault` (01 §Emulator model tiers, T0
/// diagnostics hooks). An absent hook is [`NO_HOOK`], so the per-step check in [`Machine::run`] is four compares.
#[derive(Clone, Copy, Debug)]
struct FaultHooks {
    /// `vAssertCalled(const char *fileName, uint16_t lineNo)`: prints and loops forever (system/assert.c:23;
    /// FreeRTOSConfig.h:71-72).
    assert: u32,
    /// `C_exception_handler(cause, addr, status, value)`: GURU MEDITATION loop (portable/riscv/riscv_main.c:189-195).
    exception: u32,
    /// crt0 trap vector, active until the scheduler installs its own (crt0.S:75-89,280; 01 H2).
    early_trap: u32,
    /// The `j .` that `get_mem` spins on after printing "** PANIC **" when `pvPortMalloc` returns NULL; `new` and
    /// `new[]` allocate through it (system/memory_wrap.cc:31-40, 47-60; 01 H14).
    panic_loop: u32,
    /// `sp` offset of the `ra` saved by `get_mem`'s prologue, which names the caller in the halt message.
    panic_ra_slot: u32,
}

impl FaultHooks {
    const NONE: FaultHooks =
        FaultHooks { assert: NO_HOOK, exception: NO_HOOK, early_trap: NO_HOOK, panic_loop: NO_HOOK, panic_ra_slot: 0 };

    /// Hook addresses from `symbols`; the `get_mem` loop is located in the code loaded into `ram`.
    fn resolve(symbols: &Symbols, ram: &[u8]) -> Self {
        let addr = |name| symbols.addr_of(name).unwrap_or(NO_HOOK);
        let (panic_loop, panic_ra_slot) = get_mem_panic_loop(symbols, ram).unwrap_or((NO_HOOK, 0));
        FaultHooks {
            assert: addr("vAssertCalled"),
            exception: addr("C_exception_handler"),
            early_trap: addr("__crt0_dummy_trap_handler"),
            panic_loop,
            panic_ra_slot,
        }
    }

    fn armed(&self) -> bool {
        [self.assert, self.exception, self.early_trap, self.panic_loop] != [NO_HOOK; 4]
    }

    /// True when `pc` is a hook address: branch-free compares, inlined into the step loop.
    #[inline(always)]
    fn hit(&self, pc: u32) -> bool {
        (pc == self.assert) | (pc == self.exception) | (pc == self.early_trap) | (pc == self.panic_loop)
    }
}

/// PANIC loop of `get_mem(size_t)` (`_Z7get_memj`) and the `sp` offset of its saved `ra`, found by scanning the loaded
/// function body for its `j .` and its prologue's `sw ra, off(sp)` (ELF @0x43294: `sw ra,28(sp)`, `j .` at
/// 0x432C4). None unless both are there.
fn get_mem_panic_loop(symbols: &Symbols, ram: &[u8]) -> Option<(u32, u32)> {
    let sym = symbols.syms.iter().find(|s| s.mangled == "_Z7get_memj")?;
    let (mut panic_loop, mut ra_slot) = (None, None);
    for pc in (sym.addr..sym.addr.saturating_add(sym.size)).step_by(4) {
        let at = (pc & RAM_MASK) as usize;
        let word = u32::from_le_bytes(ram.get(at..at + 4)?.try_into().ok()?);
        if word == JUMP_TO_SELF {
            panic_loop = Some(pc);
        }
        // SW (opcode 0x23, funct3 2) with rs1 = sp, rs2 = ra; the slot is the sign-extended S-type immediate.
        if word & 0x01FF_F07F == 0x0011_2023 && ra_slot.is_none() {
            ra_slot = Some(((word as i32 >> 25) << 5 | (word >> 7 & 0x1F) as i32) as u32);
        }
    }
    Some((panic_loop?, ra_slot?))
}

/// PCs kept by [`TraceRing`].
pub const TRACE_LEN: usize = 256;

/// CPU trace ring: the PC of each of the last [`TRACE_LEN`] steps (docs/specs/S11-S14-later.md §S11), filled by
/// [`Machine::run`].
///
/// A step that takes an interrupt records the interrupted PC, so an interrupt entry shows up as that PC followed
/// by the trap vector. Recording is one store and one add per step. Measured against the parent commit (20 s
/// `--speed max` boot, 4 rounds): an unconditional push cost 7 % of host MIPS, the push behind
/// `MachineConfig::trace` hoisted out of the loop costs 3 % when on and stays within noise (1 %) when off. A
/// const-generic loop per setting cost 27 % even with the ring off.
#[derive(Clone, Debug)]
pub struct TraceRing {
    pcs: [u32; TRACE_LEN],
    /// Steps recorded since power-on; its low byte indexes the next slot.
    count: u64,
}

impl Default for TraceRing {
    fn default() -> Self {
        TraceRing { pcs: [0; TRACE_LEN], count: 0 }
    }
}

impl TraceRing {
    #[inline(always)]
    fn push(&mut self, pc: u32) {
        self.pcs[usize::from(self.count as u8)] = pc;
        self.count += 1;
    }

    /// Steps recorded since power-on, including those already overwritten.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The recorded PCs, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        let len = self.count.min(TRACE_LEN as u64) as usize;
        let start = usize::from((self.count - len as u64) as u8);
        (0..len).map(move |i| self.pcs[(start + i) % TRACE_LEN])
    }
}

pub struct Machine {
    pub cpu: rv32::Cpu,
    pub bus: SystemBus,
    pub symbols: Symbols,
    /// The firmware image's loaded segments (address, length), for a later scan of its own tables — the settings
    /// definitions (S21) that the monitor's `config` verb reads (S23 §6). Empty for a machine built from parts.
    pub segments: Vec<(u32, u32)>,
    pub cfg: MachineConfig,
    pub breakpoints: Vec<u32>,
    pub trace: TraceRing,
    hooks: FaultHooks,
    /// Earliest device `next_event`; recomputed after IO accesses, ticks and host input.
    next_deadline: u64,
    /// Interrupt entries taken (`Exit::Interrupt`).
    irqs: u64,
    /// PC of the last breakpoint or hook stop. The next `run` executes that instruction instead of stopping again.
    resume_pc: Option<u32>,
    /// The loop the idle skip is watching (S19 §3).
    idle: IdleSlot,
    /// Instructions the idle skip did not execute; `cpu.insns` counts the executed ones (S19 §5).
    pub idle_insns: u64,
    /// The VIC frame counter the UDP stream generator last sent (S24 §4).
    streamed_frame: u64,
    /// Whether the audio stream's tap is installed in the C64 backend.
    streaming_audio: bool,
    /// S41: the registers before the executing instruction, kept while a `noack` stall halts, so a halt leaves the
    /// machine on the instruction that never completes.
    noack_x: [u32; 32],
    /// S41: the end of the console output drained so far, for the last line in a stall report.
    console_tail: Vec<u8>,
}

/// Console bytes [`Machine`] keeps for the last line of a stall report.
const CONSOLE_TAIL: usize = 512;
/// Return addresses a stall report's stack scan lists at most, and the stack bytes it looks at.
const BACKTRACE_DEPTH: usize = 12;
const BACKTRACE_STACK: u32 = 0x1000;

/// One loop head the idle skip watches: the machine state at the last arrival there (S19 §3).
#[derive(Clone, Debug, Default)]
struct IdleSlot {
    head: u32,
    /// `cpu.insns` at the last arrival.
    insns: u64,
    irqs: u64,
    x: [u32; 32],
    csr: rv32::Csrs,
    /// Executed instructions between the last two arrivals; 0 until there have been two.
    period: u64,
}

impl Machine {
    /// Bus with all devices installed, the ELF loaded (`pc = entry`, registers 0) and its symbols. Fails for an
    /// unreadable image and for `clocks_per_insn` 0, which would stop emulated time (ARCHITECTURE §Machine loop 1).
    pub fn new(mut cfg: MachineConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(cfg.clocks_per_insn > 0, "clocks_per_insn must be at least 1, or emulated time stands still");
        let mut bus = SystemBus::new();
        // The image first: whether the TRX block is installed can depend on it (S37).
        let fw = loader::load_firmware(&cfg.elf, &mut bus.ram)?;
        let symbols = match fw.format {
            loader::ImageFormat::Elf => Symbols::from_elf(&cfg.elf)?,
            format => {
                eprintln!(
                    "{}: {format:x?} image, entry {:#010x}: no symbols, logs show raw addresses and fault hooks are off",
                    cfg.elf.display(),
                    fw.entry
                );
                Symbols::empty()
            }
        };
        let (trx_io, why) = resolve_trx_io(cfg.trx_io, trx_firmware(&symbols, &bus.ram, &fw.segments));
        if let Some(why) = why {
            eprintln!("{why}");
        }
        cfg.trx_io = trx_io;
        // S41 §1: the absent blocks are the TRX bitstream's build switches.
        if cfg.absent.is_set() && trx_io != TrxIo::On {
            anyhow::bail!("--absent, --absent-mode and --absent-continue need the TRX block (--trx-io, or TRX firmware with auto)");
        }
        devices::install_all(&mut bus.io, &cfg);
        // The FPGA reads the cartridge ROM where this firmware puts it (docs/status/carts.md, "Cartridge ROM in DDR").
        let cart_rom = crate::fwlayout::cart_rom(&bus.ram, &fw.segments).unwrap_or_else(|| {
            eprintln!("c64: the cartridge ROM address is not in the firmware image; assuming 3.15's 0x03C00000");
            crate::c64host::CartRom::LARGE
        });
        if cart_rom != crate::c64host::CartRom::LARGE {
            let (base, mb) = (cart_rom.base, cart_rom.size >> 20);
            eprintln!("c64: cartridge ROM at {base:#010x}, {mb} MB (firmware before 3.15)");
        }
        if let Some(port) = bus.io.get_mut::<C64Port>() {
            port.set_cart_rom(cart_rom);
        }
        if !cfg.settings.is_empty() {
            // After the overlay seed of `SpiFlash::from_config`, so a file's Interface Type wins (S21 §5).
            let flash = bus.io.get_mut::<devices::flash::SpiFlash>().expect("the flash is always installed");
            crate::settings::apply(&cfg.settings, &bus.ram, &fw.segments, flash)?;
        }
        let mut machine = Self::from_parts(cfg, bus, fw.entry, symbols);
        machine.segments = fw.segments;
        machine.start_absent();
        Ok(machine)
    }

    /// Machine around an already populated bus: registers 0, `pc = entry`, log flags applied to the bus, fault
    /// hooks resolved from `symbols` when `cfg.halt_on_fault`.
    pub fn from_parts(cfg: MachineConfig, mut bus: SystemBus, entry: u32, symbols: Symbols) -> Self {
        bus.trace_io = cfg.log.io;
        bus.log_unmapped = cfg.log.unmapped;
        let hooks = if cfg.halt_on_fault { FaultHooks::resolve(&symbols, &bus.ram) } else { FaultHooks::NONE };
        let next_deadline = bus.next_deadline();
        Machine {
            cpu: rv32::Cpu::new(entry),
            bus,
            symbols,
            segments: Vec::new(),
            cfg,
            breakpoints: Vec::new(),
            trace: TraceRing::default(),
            hooks,
            next_deadline,
            irqs: 0,
            resume_pc: None,
            idle: IdleSlot { head: u32::MAX, ..IdleSlot::default() },
            idle_insns: 0,
            streamed_frame: 0,
            streaming_audio: false,
            noack_x: [0; 32],
            console_tail: Vec::new(),
        }
    }

    /// Run at most `max_insns` instructions (an interrupt entry uses one step of the budget). Instructions the idle
    /// skip fast-forwards count against the budget like executed ones (S19 §4).
    pub fn run(&mut self, max_insns: u64) -> RunExit {
        let hooks = self.hooks;
        let breakpoints = !self.breakpoints.is_empty();
        let check_pc = breakpoints || hooks.armed();
        let trace = self.cfg.trace;
        let idle_skip = self.cfg.idle_skip && !breakpoints && !trace && !self.cfg.log.io;
        // S41: a `noack` stall that halts puts the instruction back, so its registers are kept before each step.
        let noack_halts = self.bus.absent.as_deref().is_some_and(AbsentMap::halts);
        // The host may have changed RAM or devices since the last run (S19 §3).
        self.bus.idle_dirty = true;
        let mut resume = self.resume_pc.take();
        let mut left = max_insns;
        while left > 0 {
            left -= 1;
            if check_pc {
                let pc = self.cpu.pc;
                // `stop_at` only for a hook address or while breakpoints are set; the stop being resumed is skipped.
                if (breakpoints || hooks.hit(pc)) && resume != Some(pc) {
                    if let Some(exit) = self.stop_at(pc) {
                        self.resume_pc = Some(pc);
                        return exit;
                    }
                }
                resume = None;
            }
            if trace {
                self.trace.push(self.cpu.pc);
            }
            let pc_before = self.cpu.pc;
            if noack_halts {
                self.noack_x = self.cpu.x;
            }
            self.pre_step();
            let exit = self.cpu.step(&mut self.bus);
            self.post_step();
            if self.bus.noack.is_some() {
                if let Some(exit) = self.take_noack(pc_before, noack_halts) {
                    return exit;
                }
            }
            // A loop head is where a backward branch or jump lands, `j .` included (S19 §3).
            if idle_skip && exit == rv32::Exit::Stepped && self.cpu.pc <= pc_before {
                left -= self.idle_arrival(left);
            }
            match exit {
                rv32::Exit::Stepped | rv32::Exit::Wfi | rv32::Exit::Ebreak => {}
                rv32::Exit::Interrupt => {
                    self.irqs += 1;
                    if self.cfg.log.irq {
                        self.log_irq_taken();
                    }
                }
                rv32::Exit::Illegal(word) if self.cfg.halt_on_fault => {
                    return RunExit::Halted(format!(
                        "illegal instruction {word:#010x} at {}",
                        self.symbols.format(self.bus.pc)
                    ));
                }
                rv32::Exit::Illegal(_) => {}
            }
        }
        RunExit::Budget
    }

    /// Arrival at a loop head with `left` instructions of budget left. When the loop has reached a fixed point, i.e.
    /// since the last arrival here nothing was written, no IO was accessed, no device ticked, no interrupt was taken
    /// and registers, CSRs and the period are unchanged, every further pass is the same pass until the next device
    /// event: skip whole passes up to it and return the instructions skipped (S19 §3-§4).
    #[inline(never)]
    fn idle_arrival(&mut self, left: u64) -> u64 {
        let clean = !std::mem::replace(&mut self.bus.idle_dirty, false);
        let (pc, insns, irqs) = (self.cpu.pc, self.cpu.insns, self.irqs);
        let slot = &mut self.idle;
        if slot.head != pc {
            *slot = IdleSlot { head: pc, insns, irqs, x: self.cpu.x, csr: self.cpu.csr.clone(), period: 0 };
            return 0;
        }
        let period = insns - slot.insns;
        let fixed = clean
            && irqs == slot.irqs
            && period == slot.period
            && self.cpu.x == slot.x
            && self.cpu.csr == slot.csr;
        (slot.insns, slot.irqs, slot.period) = (insns, irqs, period);
        if !fixed {
            slot.x = self.cpu.x;
            slot.csr = self.cpu.csr.clone();
            return 0;
        }
        let clocks = period * self.cfg.clocks_per_insn;
        let passes = (left / period).min(self.next_deadline.saturating_sub(self.bus.now) / clocks);
        let skipped = passes * period;
        self.bus.now += skipped * self.cfg.clocks_per_insn;
        self.idle_insns += skipped;
        skipped
    }

    /// Before a step: tick due devices, then drive `meip` from the ITU line, so an edge raised by a tick is
    /// taken by this very step (ARCHITECTURE §Machine loop 2).
    #[inline(always)]
    fn pre_step(&mut self) {
        if self.bus.now >= self.next_deadline {
            self.bus.tick_due();
            self.next_deadline = self.bus.next_deadline();
        }
        let line = self.bus.irq.line();
        if line != self.cpu.meip && self.cfg.log.irq {
            self.log_irq_edge(line);
        }
        self.cpu.meip = line;
        self.bus.pc = self.cpu.pc;
    }

    /// After a step: advance time, recompute the deadline after IO, print logged accesses (§Machine loop 1, 3).
    #[inline(always)]
    fn post_step(&mut self) {
        self.bus.now += self.cfg.clocks_per_insn + std::mem::take(&mut self.bus.stall);
        if self.bus.io_touched {
            self.bus.io_touched = false;
            self.next_deadline = self.bus.next_deadline();
        }
        if !self.bus.accesses.is_empty() {
            self.flush_accesses();
        }
    }

    /// Breakpoint or fault hook at `pc`, checked before the instruction executes.
    #[cold]
    fn stop_at(&self, pc: u32) -> Option<RunExit> {
        let x = &self.cpu.x;
        let caller = || self.symbols.format(x[1]);
        if pc == self.hooks.panic_loop {
            // Still in get_mem's frame: the prologue's slot holds the return address into its caller, which the
            // console line prints as a raw %p (memory_wrap.cc:36-38).
            let slot = x[2].wrapping_add(self.hooks.panic_ra_slot);
            let ra = u32::from_le_bytes(std::array::from_fn(|i| {
                self.bus.ram[(slot.wrapping_add(i as u32) & RAM_MASK) as usize]
            }));
            return Some(RunExit::Halted(format!(
                "get_mem PANIC: pvPortMalloc returned NULL, get_mem called from {}",
                self.symbols.format(ra)
            )));
        }
        if pc == self.hooks.assert {
            // a0 = fileName, a1 = lineNo (system/assert.c:23).
            return Some(RunExit::Halted(format!(
                "vAssertCalled called from {} ({}:{})",
                caller(),
                self.c_string(x[10]),
                x[11] & 0xFFFF
            )));
        }
        if pc == self.hooks.exception {
            // a0 = cause, a1 = addr, a3 = value (riscv_main.c:189-191).
            return Some(RunExit::Halted(format!(
                "C_exception_handler called from {}: cause {:#010x} at {} value {:#010x}",
                caller(),
                x[10],
                self.symbols.format(x[11]),
                x[13]
            )));
        }
        if pc == self.hooks.early_trap {
            let csr = &self.cpu.csr;
            return Some(RunExit::Halted(format!(
                "__crt0_dummy_trap_handler entered: mcause {:#010x} mepc {}",
                csr.mcause,
                self.symbols.format(csr.mepc)
            )));
        }
        self.breakpoints.contains(&pc).then_some(RunExit::Breakpoint(pc))
    }

    /// NUL-terminated string (at most 128 bytes) from DDR; an IO address is not read because reads have side effects.
    fn c_string(&self, addr: u32) -> String {
        if addr & IO_BIT != 0 {
            return format!("{addr:#010x}");
        }
        let bytes: Vec<u8> = (0..128)
            .map(|i| self.bus.ram[(addr.wrapping_add(i) & RAM_MASK) as usize])
            .take_while(|&b| b != 0)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[cold]
    fn log_irq_edge(&self, line: bool) {
        let irq = &self.bus.irq;
        eprintln!(
            "irq: line {} at {} (global {}, active {:#04x}, high {:#04x})",
            u8::from(line),
            self.symbols.format(self.cpu.pc),
            u8::from(irq.global_en),
            irq.active(),
            irq.high_active()
        );
    }

    #[cold]
    fn log_irq_taken(&self) {
        let irq = &self.bus.irq;
        eprintln!(
            "irq: taken at {} (active {:#04x}, high {:#04x})",
            self.symbols.format(self.cpu.csr.mepc),
            irq.active(),
            irq.high_active()
        );
    }

    /// Print the accesses the bus recorded during the last step, symbolized.
    #[cold]
    fn flush_accesses(&mut self) {
        let mut accesses = std::mem::take(&mut self.bus.accesses);
        let mut err = std::io::stderr().lock();
        for access in accesses.drain(..) {
            if access.trace {
                let _ = writeln!(err, "{}", self.format_io(&access));
            }
            if access.unmapped {
                let _ = writeln!(err, "{}", self.format_unmapped(&access));
            }
        }
        self.bus.accesses = accesses;
    }

    /// `io R/W addr val device+off @pc sym+off t=seconds`, the time in emulated seconds to the 10 ns tick.
    fn format_io(&self, a: &Access) -> String {
        let target = match (self.bus.absent_at(a.addr), self.bus.io.resolve(a.addr)) {
            (Some((block, mode)), _) => format!("absent:{}({})", block.name(), mode.name()),
            (None, Some((dev, off))) => format!("{}+{off:#x}", self.bus.io.devices[dev].name()),
            (None, None) => "unmapped".to_owned(),
        };
        format!(
            "io {} {:#010x} {:#04x} {target} @{:#010x} {} t={}.{:08}",
            rw(a.write),
            a.addr,
            a.val,
            a.pc,
            self.symbols.format(a.pc),
            a.now / time::CLOCK_HZ,
            a.now % time::CLOCK_HZ,
        )
    }

    /// `unmapped R/W addr [val] @pc sym+off`.
    fn format_unmapped(&self, a: &Access) -> String {
        let val = if a.write { format!(" {:#04x}", a.val) } else { String::new() };
        format!("unmapped {} {:#010x}{val} @{:#010x} {}", rw(a.write), a.addr, a.pc, self.symbols.format(a.pc))
    }

    pub fn drain_console(&mut self) -> Vec<u8> {
        let out = std::mem::take(&mut self.bus.console);
        self.console_tail.extend_from_slice(&out);
        if self.console_tail.len() > 2 * CONSOLE_TAIL {
            self.console_tail.drain(..self.console_tail.len() - CONSOLE_TAIL);
        }
        out
    }

    /// Attach a C64 behind the cart/DMA registers (docs/specs/S14-c64-trx64.md §3). Without `devices::c64::C64Port`
    /// installed the backend is dropped.
    pub fn attach_c64(&mut self, backend: Box<dyn C64Backend>) {
        let now = self.bus.now;
        // CARTSLOT: U64_CART_DETECT (U64Io) reads the lines of the backend's physical cartridge (docs/status/cart-slot.md).
        let cart_detect = self.bus.io.get::<U64Io>().map(|io| io.cart_detect.clone());
        let absent = self.backend_absent();
        if let Some(port) = self.bus.io.get_mut::<C64Port>() {
            port.attach(backend, now);
            if let Some(cell) = cart_detect {
                port.set_cart_detect(cell);
            }
            // S41: the C64 side of the blocks this bitstream lacks.
            port.set_absent(&absent);
        }
        self.next_deadline = self.bus.next_deadline();
    }

    /// The cartridge in the attached C64's physical expansion port, if any (docs/status/cart-slot.md).
    pub fn c64_cart_slot(&mut self) -> Option<&mut dyn C64CartSlot> {
        self.bus.io.get_mut::<C64Port>()?.cart_slot()
    }

    /// Route host input to `devices::itu::Itu` / `devices::u64io::U64Io` / `devices::usb::Usb` /
    /// `devices::c64::C64Port`. Matrix keys reach both keyboards, the overlay scanner and the C64 (TRX64 or the T0
    /// stub's CIA1). Input for a device that is not installed is dropped.
    pub fn input(&mut self, ev: HostInput) {
        let now = self.bus.now;
        let io = &mut self.bus.io;
        match ev {
            HostInput::Key { row, col, down } => {
                if let Some(dev) = io.get_mut::<U64Io>() {
                    dev.set_key(row, col, down);
                }
                // S14 §3: the overlay owns the keyboard while TRANSPARENCY bit 6 is set (05 §A, OQ6). Releases always
                // pass, so a key held across the menu does not stick in the C64.
                let own_keyboard = io.get::<Overlay>().is_some_and(|o| o.regs[REG_TRANSPARENCY] & 0x40 != 0);
                if let Some(dev) = io.get_mut::<C64Port>().filter(|_| !(down && own_keyboard)) {
                    dev.set_key(row, col, down);
                }
            }
            HostInput::Joystick(lines) => {
                if let Some(dev) = io.get_mut::<C64Port>() {
                    dev.set_joystick(2, lines);
                }
            }
            HostInput::JoystickPort { port, lines } => {
                if let Some(dev) = io.get_mut::<C64Port>() {
                    dev.set_joystick(port, lines);
                }
            }
            HostInput::MenuButton(pressed) => {
                if let Some(dev) = io.get_mut::<Itu>() {
                    dev.set_menu_button(pressed);
                }
            }
            HostInput::UsbKey { usage, down } => {
                if let Some(dev) = io.get_mut::<devices::usb::Usb>() {
                    dev.key(usage, down);
                }
            }
            HostInput::Restore(held) => {
                if let Some(dev) = io.get_mut::<C64Port>() {
                    dev.set_restore(held);
                }
            }
            HostInput::UsbMouse { dx, dy, wheel, buttons } => {
                if let Some(dev) = io.get_mut::<devices::usb::Usb>() {
                    dev.mouse(dx, dy, wheel, buttons);
                }
            }
            HostInput::UsbPlug { port, connected } => {
                if let Some(dev) = io.get_mut::<devices::usb::Usb>() {
                    dev.set_connected(usize::from(port), connected, now);
                }
            }
        }
        // Input can schedule device events (e.g. a button pulse).
        self.next_deadline = self.bus.next_deadline();
    }

    /// Put USB mass storage on `backend` on the empty hub port `port` (1-based), e.g. a `--usb-dir` volume in a
    /// slot `MachineConfig::usb.storage_slots` left free. Fails without the USB device, for a missing or used port,
    /// and for an empty medium.
    pub fn usb_attach_storage(
        &mut self,
        port: usize,
        backend: Box<dyn devices::usb::block::BlockBackend>,
    ) -> Result<(), String> {
        let usb = self.bus.io.get_mut::<devices::usb::Usb>().ok_or("no USB device installed")?;
        usb.attach_storage(port, backend)?;
        self.next_deadline = self.bus.next_deadline();
        Ok(())
    }

    /// Swap the medium of the USB stick on `port` (1-based) and return the previous one. Meant for an unplugged
    /// stick (`HostInput::UsbPlug`): the firmware reads the new medium when it is plugged back in.
    pub fn usb_replace_backend(
        &mut self,
        port: usize,
        backend: Box<dyn devices::usb::block::BlockBackend>,
    ) -> Result<Box<dyn devices::usb::block::BlockBackend>, String> {
        let usb = self.bus.io.get_mut::<devices::usb::Usb>().ok_or("no USB device installed")?;
        usb.replace_backend(port, backend)
    }

    /// S33: plug `device` into the empty hub port `port` (1-based) while the machine runs.
    pub fn usb_plug(&mut self, port: usize, device: devices::usb::UsbDevice) -> Result<(), String> {
        let now = self.bus.now;
        let usb = self.bus.io.get_mut::<devices::usb::Usb>().ok_or("no USB device installed")?;
        usb.plug(port, device, now)?;
        self.next_deadline = self.bus.next_deadline();
        Ok(())
    }

    /// S33: unplug the device on hub port `port` and take it away; returns what it was.
    pub fn usb_unplug(&mut self, port: usize) -> Result<&'static str, String> {
        let now = self.bus.now;
        let usb = self.bus.io.get_mut::<devices::usb::Usb>().ok_or("no USB device installed")?;
        let kind = usb.unplug(port, now)?;
        self.next_deadline = self.bus.next_deadline();
        Ok(kind)
    }

    /// State of USB hub port `port` (1-based); None without the USB device or for a port the hub does not have.
    pub fn usb_port(&self, port: usize) -> Option<devices::usb::UsbPortInfo> {
        self.bus.io.get::<devices::usb::Usb>()?.port_info(port)
    }

    /// Overlay snapshot from `devices::overlay::Overlay`, or blank buffers of the sizes documented on
    /// `DisplaySnapshot` when no overlay is installed, plus the frame of an attached C64 and the TRX output while the
    /// pixel layer is on or the machine pause stands, with the text layer while it is shown (S37).
    pub fn display(&self) -> DisplaySnapshot {
        let now_ms = self.now_ms();
        let mut snap = match self.bus.io.get::<Overlay>() {
            Some(overlay) => overlay.snapshot(now_ms),
            None => DisplaySnapshot {
                regs: [0; 16],
                screen: vec![0; 4096],
                color: vec![0; 4096],
                palette: vec![0; 64],
                hdmi: [0; 0x1E],
                cropper: [0; 4],
                now_ms,
                c64: None,
                trx: None,
            },
        };
        snap.c64 = self.bus.io.get::<C64Port>().and_then(C64Port::frame);
        snap.trx = self.bus.io.get::<devices::trx::Trx>().and_then(|trx| {
            let (layer, paused) = (trx.layer(&self.bus.ram), trx.paused());
            let backend = self.bus.io.get::<C64Port>().and_then(C64Port::backend);
            let text = trx.text().zip(backend).map(|((view, opaque), c64)| devices::trx::text_layer(view, opaque, c64));
            // The TRX bitstream always outputs its 1920 × 1080 frame, layer or not, so the picture keeps one shape.
            Some(TrxOutput { layer, paused, text })
        });
        // S41 §5: without a core the TRX output is the overlay over black.
        if self.core_is_absent() {
            snap.trx = snap.trx.as_ref().map(|_| TrxOutput::default());
        }
        // The text dump shows what the text layer draws while it is shown, not what the paused VIC points at.
        if let (Some(frame), Some(text)) = (snap.c64.as_mut(), snap.trx.as_ref().and_then(|t| t.text.as_ref())) {
            (frame.screen, frame.charset) = (text.screen.clone(), text.charset);
        }
        snap
    }

    /// The attached C64's VIC frame counter, or 0 when there is no C64 or its backend does not count frames.
    pub fn frame_counter(&self) -> u64 {
        self.bus.io.get::<C64Port>().map_or(0, C64Port::frame_counter)
    }

    /// What the picture follows: the VIC frame counter, or with `--trx-io` the TRX output's frame starts, which go on
    /// while the machine pause stands the VIC (S37).
    pub fn picture_counter(&self) -> u64 {
        // S41 §5: without a core nothing counts frames; the display follows emulated time.
        if self.core_is_absent() {
            return 0;
        }
        match self.bus.io.get::<devices::trx::Trx>() {
            Some(trx) => trx.frame_starts(),
            None => self.frame_counter(),
        }
    }

    /// The UDP stream generators, once per call: a new VIC frame becomes datagrams, and everything waiting goes
    /// back to the caller to put on the wire (S24 §4). Nothing happens while `ETHSTREAM_ENA` has no enabled
    /// stream, so a machine nobody streams from does not build a frame it will not send.
    pub fn stream_pump(&mut self) -> Vec<Vec<u8>> {
        let ena = self.bus.io.get::<devices::u64io::U64Io>().map_or(0, devices::u64io::U64Io::ethstream_ena);
        if ena & 0x01 != 0 {
            let counter = self.frame_counter();
            if counter != self.streamed_frame {
                self.streamed_frame = counter;
                if let Some(frame) = self.bus.io.get::<C64Port>().and_then(C64Port::frame) {
                    if let Some(streams) = self.bus.io.get_mut::<devices::streams::Streams>() {
                        streams.send_frame(&frame);
                    }
                }
            }
        }
        if (ena & 0x02 != 0) != self.streaming_audio {
            self.streaming_audio = ena & 0x02 != 0;
            let on = self.streaming_audio;
            if let Some(port) = self.bus.io.get_mut::<C64Port>() {
                port.set_stream_audio(on);
            }
        }
        if self.streaming_audio {
            let pcm = self.bus.io.get_mut::<C64Port>().map_or_else(Vec::new, C64Port::take_stream_audio);
            if !pcm.is_empty() {
                if let Some(streams) = self.bus.io.get_mut::<devices::streams::Streams>() {
                    streams.send_audio(&pcm);
                }
            }
        }
        match self.bus.io.get_mut::<devices::streams::Streams>() {
            Some(streams) => streams.take(),
            None => Vec::new(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        time::clocks_to_ms(self.bus.now)
    }

    /// One-line run statistics.
    pub fn stats(&self) -> String {
        format!(
            "{} instructions, {} skipped idle, {:.3} s emulated, {} IRQs taken, pc {}",
            self.cpu.insns,
            self.idle_insns,
            self.bus.now as f64 / time::CLOCK_HZ as f64,
            self.irqs,
            self.symbols.format(self.cpu.pc)
        )
    }

    /// The trace ring, oldest first, one symbolized PC per line, for a fault halt or the debugger.
    pub fn trace_report(&self) -> String {
        if !self.cfg.trace {
            return format!("trace: not recorded (--trace or --gdb record the last {TRACE_LEN} PCs)\n");
        }
        let total = self.trace.count();
        let mut out = format!("trace: last {} of {total} steps, oldest first\n", total.min(TRACE_LEN as u64));
        for pc in self.trace.iter() {
            let _ = match self.symbols.lookup(pc) {
                Some((name, 0)) => writeln!(out, "  {pc:#010x} {name}"),
                Some((name, off)) => writeln!(out, "  {pc:#010x} {name}+{off:#x}"),
                None => writeln!(out, "  {pc:#010x}"),
            };
        }
        out
    }

    /// Table of unmapped accesses (address, read and write counts) for the end of a run; None unless
    /// `--log unmapped`.
    pub fn unmapped_summary(&self) -> Option<String> {
        if !self.bus.log_unmapped {
            return None;
        }
        let counts = &self.bus.unmapped;
        let mut out = format!("unmapped summary: {} addresses\n", counts.len());
        if !counts.is_empty() {
            out.push_str("  address        reads    writes\n");
        }
        for (addr, count) in counts {
            let _ = writeln!(out, "  {addr:#010x} {:>9} {:>9}", count.reads, count.writes);
        }
        Some(out)
    }
}

/// S41: absent blocks (docs/specs/S41-core-dev-mode.md).
impl Machine {
    /// The blocks absent from power-on whose C64 side goes with the backend (`C64Backend::set_absent`).
    fn backend_absent(&self) -> Vec<Block> {
        self.cfg.absent.blocks.iter().copied().filter(|&b| b != Block::Core).collect()
    }

    /// §1: the absent blocks from power-on: the bus's map, the TRX block's APPLY, one line each on what it changes, and
    /// `--absent core` as a `core absent` before the first instruction. The C64 side follows with the backend
    /// ([`Machine::attach_c64`]).
    fn start_absent(&mut self) {
        if !self.cfg.absent.is_set() {
            return;
        }
        let cfg = self.cfg.absent.clone();
        let mut map = AbsentMap::new(&cfg);
        // The core goes through `core_absent`, which also stops the C64 and the TRX block.
        map.set(Block::Core, false);
        self.bus.absent = Some(Box::new(map));
        if let Some(trx) = self.bus.io.get_mut::<devices::trx::Trx>() {
            trx.set_apply_absent(cfg.has(Block::Apply));
        }
        for &block in &cfg.blocks {
            let what = match block {
                Block::Sampler | Block::Uci => {
                    let (start, end) = block.windows()[0];
                    let c64 = if block == Block::Sampler {
                        "C64 $DF20-$DFFF not mapped, IRQ 0, no DDR fetch"
                    } else {
                        "nothing on the C64 bus, firmware and C64 IRQ 0"
                    };
                    format!("{} {start:#010x}-{:#010x}; {c64}", cfg.mode(block).name(), end - 1)
                }
                Block::Reu => "C64 $DF00-$DFFF reads $FF while REU_ENABLE is set, no DMA, IRQ 0".into(),
                Block::Turbo => "the TRX model has no turbo engine (S40 §0), so nothing changes".into(),
                Block::Jtag => "no bus effect".into(),
                Block::Streams => "no UDP stream senders; the template writes are still acked and the enables do nothing, \
                                   no bus difference"
                    .into(),
                Block::Apply => "AP_* writes ignored, AP_CMD never busy, CAPS2 b5 clear".into(),
                Block::Core => continue,
            };
            eprintln!("absent: {}: {what}", block.name());
        }
        if cfg.keep_going {
            eprintln!("absent: --absent-continue: a noack stall is logged and completed as ack0");
        }
        if cfg.has(Block::Core) {
            if let Ok(line) = self.core_absent() {
                eprintln!("{line}");
            }
        }
    }

    /// Whether the swappable core is absent (§5).
    pub fn core_is_absent(&self) -> bool {
        self.bus.absent.as_deref().is_some_and(|a| a.is_absent(Block::Core))
    }

    /// §5 `core absent`: every register window of the core answers as absent, the C64 stops where it is (no clock, no
    /// picture, no sound), the core's interrupts into the ITU drop. The host runs on.
    pub fn core_absent(&mut self) -> Result<String, String> {
        if self.cfg.trx_io != TrxIo::On {
            return Err("core absent: the core's windows are the TRX bitstream's; this machine has no TRX block (--trx-io)".into());
        }
        let cfg = &self.cfg.absent;
        let map = self.bus.absent.get_or_insert_with(|| Box::new(AbsentMap::new(cfg)));
        if map.is_absent(Block::Core) {
            return Err("core absent: the core is already absent".into());
        }
        map.set(Block::Core, true);
        let mode = map.mode(Block::Core);
        let now = self.bus.now;
        let (io, irq) = (&mut self.bus.io, &mut self.bus.irq);
        if let Some(port) = io.get_mut::<C64Port>() {
            // ack0 is the host's decoupler, which resets the C64 side's registers once; noack is a host without one.
            port.set_core_absent(now, irq, mode == Mode::Ack0);
        }
        if let Some(trx) = io.get_mut::<devices::trx::Trx>() {
            trx.set_core_absent(true, now, irq);
        }
        self.next_deadline = self.bus.next_deadline();
        Ok(format!("core absent ({}): its windows answer {}, the C64 stopped at {}", mode.name(), mode.name(), self.time_text()))
    }

    /// The backend of the core about to be replaced, for its physical cartridge (§5 `core present`).
    pub fn take_c64_backend(&mut self) -> Option<Box<dyn C64Backend>> {
        self.bus.io.get_mut::<C64Port>()?.take_backend()
    }

    /// §5 `core present`: a fresh core image from power-up, as a reconfiguration loads it: the core's windows answer
    /// again, every register of the C64 port, the TRX block and the GCR codec at its power-on value, `backend` (a new
    /// C64, or None for none) attached now. Blocks absent from the bitstream stay absent in the new core.
    pub fn core_present(&mut self, backend: Option<Box<dyn C64Backend>>) -> Result<String, String> {
        if !self.core_is_absent() {
            return Err("core present: the core is present".into());
        }
        if let Some(map) = self.bus.absent.as_deref_mut() {
            map.set(Block::Core, false);
        }
        let now = self.bus.now;
        let absent = self.backend_absent();
        let apply_absent = self.cfg.absent.has(Block::Apply);
        let cart_detect = self.bus.io.get::<U64Io>().map(|io| io.cart_detect.clone());
        let (io, irq, ram) = (&mut self.bus.io, &mut self.bus.irq, &self.bus.ram);
        if let Some(trx) = io.get_mut::<devices::trx::Trx>() {
            IoDevice::reset(trx);
            trx.set_apply_absent(apply_absent);
            trx.set_core_absent(false, now, irq);
        }
        if let Some(gcr) = io.get_mut::<devices::misc::GcrCodec>() {
            IoDevice::reset(gcr);
        }
        if let Some(port) = io.get_mut::<C64Port>() {
            port.power_up(backend, now, ram);
            if let Some(cell) = cart_detect {
                port.set_cart_detect(cell);
            }
            port.set_absent(&absent);
        }
        self.next_deadline = self.bus.next_deadline();
        Ok(format!("core present: a fresh core from power-up at {}", self.time_text()))
    }

    /// `t=seconds` of emulated time since power-on.
    fn time_text(&self) -> String {
        format!("t={}.{:08}", self.bus.now / time::CLOCK_HZ, self.bus.now % time::CLOCK_HZ)
    }

    /// The `noack` access the last step made: a halt with the instruction put back (its registers, its PC, its count),
    /// or with `--absent-continue` a stall completed as `ack0`, logged the first time at its site and counted.
    #[cold]
    fn take_noack(&mut self, pc: u32, halts: bool) -> Option<RunExit> {
        let s = self.bus.noack.take()?;
        if halts {
            self.cpu.pc = pc;
            self.cpu.x = self.noack_x;
            self.cpu.insns -= 1;
            return Some(RunExit::Halted(self.noack_report(&s)));
        }
        if self.bus.absent.as_deref_mut().is_some_and(|m| m.count(&s)) {
            let width = self.access_width(s.pc).map_or(String::new(), |w| w.to_string());
            let val = if s.write { format!(" {:#04x}", s.val) } else { String::new() };
            eprintln!(
                "absent: {} noack {}{width} {:#010x}{val} @{:#010x} {} t={}.{:08} (completed as ack0)",
                s.block.name(),
                rw(s.write),
                s.addr,
                s.pc,
                self.symbols.format(s.pc),
                s.now / time::CLOCK_HZ,
                s.now % time::CLOCK_HZ,
            );
        }
        None
    }

    /// The width in bits of the load or store at `pc`, from the instruction in DDR.
    fn access_width(&self, pc: u32) -> Option<u32> {
        let insn = self.ram_word(pc)?;
        match (insn & 0x7F, insn >> 12 & 7) {
            (0x03, 0 | 4) | (0x23, 0) => Some(8),
            (0x03, 1 | 5) | (0x23, 1) => Some(16),
            (0x03, 2) | (0x23, 2) => Some(32),
            _ => None,
        }
    }

    /// A word of DDR, without side effects; None for an IO address.
    fn ram_word(&self, addr: u32) -> Option<u32> {
        if addr & IO_BIT != 0 {
            return None;
        }
        let at = (addr & RAM_MASK) as usize;
        Some(u32::from_le_bytes(self.bus.ram.get(at..at + 4)?.try_into().ok()?))
    }

    /// §3: what a halt on a `noack` access reports: the access, the PC, a backtrace, the last console line and the
    /// time since boot.
    fn noack_report(&self, s: &NoAck) -> String {
        let width = self.access_width(s.pc).map_or("a".to_owned(), |w| format!("{w}-bit"));
        let what = if s.write { format!("write ({:#04x}) to", s.val) } else { "read of".to_owned() };
        let mut out = format!(
            "absent {} ({}): {width} {what} {:#010x} never completes\n",
            s.block.name(),
            Mode::NoAck.name(),
            s.addr
        );
        let x = &self.cpu.x;
        let _ = writeln!(out, "  pc {:#010x} {}", s.pc, self.symbols.format(s.pc));
        let _ = writeln!(out, "  ra {:#010x} {}", x[1], self.symbols.format(x[1]));
        let frames = self.stack_return_addresses();
        if frames.is_empty() {
            let _ = writeln!(out, "  backtrace: no return address found on the stack (sp {:#010x})", x[2]);
        } else {
            let _ = writeln!(out, "  backtrace (return addresses on the stack from sp {:#010x}, innermost first):", x[2]);
            for ra in frames {
                let _ = writeln!(out, "    {ra:#010x} {}", self.symbols.format(ra));
            }
        }
        let _ = writeln!(out, "  last console line: {:?}", self.last_console_line());
        let _ = write!(
            out,
            "  {} s emulated since boot, {} instructions",
            format_args!("{}.{:08}", s.now / time::CLOCK_HZ, s.now % time::CLOCK_HZ),
            self.cpu.insns + self.idle_insns
        );
        out
    }

    /// Return addresses on the stack, innermost first: words above `sp` that point into a symbol right behind a call
    /// (`jal ra` or `jalr ra`). The firmware keeps no frame pointer, so this is a scan, and a stale slot of a returned
    /// call can show up in it.
    fn stack_return_addresses(&self) -> Vec<u32> {
        let sp = self.cpu.x[2] & !3;
        let mut out = Vec::new();
        let mut at = sp;
        while at < sp.saturating_add(BACKTRACE_STACK) && out.len() < BACKTRACE_DEPTH {
            let Some(word) = self.ram_word(at) else { break };
            let call = word.checked_sub(4).and_then(|a| self.ram_word(a));
            let is_call = call.is_some_and(|c| (c & 0x7F == 0x6F || c & 0x707F == 0x67) && c >> 7 & 0x1F == 1);
            if word & 3 == 0 && is_call && self.symbols.lookup(word).is_some() {
                out.push(word);
            }
            at += 4;
        }
        out
    }

    /// The last non-empty line of the console output, drained or not.
    fn last_console_line(&self) -> String {
        let bytes: Vec<u8> = self.console_tail.iter().chain(&self.bus.console).copied().filter(|&b| b != b'\r').collect();
        let text = String::from_utf8_lossy(&bytes);
        text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_owned()
    }

    /// §3: the stalls `--absent-continue` completed, per address, for the end of a run; None without it.
    pub fn absent_summary(&self) -> Option<String> {
        let map = self.bus.absent.as_deref().filter(|m| m.keep_going)?;
        let mut out = format!("absent summary: {} addresses stalled (noack, completed as ack0)\n", map.stalls.len());
        if !map.stalls.is_empty() {
            out.push_str("  address    block        reads    writes  first pc\n");
        }
        for (addr, c) in &map.stalls {
            let block = c.block.map_or("?", Block::name);
            let _ = writeln!(
                out,
                "  {addr:#010x} {block:<8} {:>9} {:>9}  {:#010x} {}",
                c.reads,
                c.writes,
                c.first_pc,
                self.symbols.format(c.first_pc)
            );
        }
        Some(out)
    }
}

fn rw(write: bool) -> char {
    if write {
        'W'
    } else {
        'R'
    }
}

#[cfg(test)]
mod tests {
    use rv32::Bus;

    use super::*;
    use crate::io::{IoCtx, IoDevice, IO_BASE};
    use crate::loader::tests::{firmware_root, FIRMWARE_ELF};
    use crate::symbols::Symbol;

    /// Fake periodic source: a write of `n` arms it `n` clocks after the access; a tick records its clock, pushes
    /// `id` to the console, pulses ITU bit 0 and re-arms after `period`.
    struct Timer {
        id: u8,
        period: u64,
        due: Option<u64>,
        ticks: Vec<u64>,
    }

    impl IoDevice for Timer {
        fn name(&self) -> &'static str {
            "timer"
        }

        fn read8(&mut self, _off: u32, _ctx: &mut IoCtx) -> u8 {
            0
        }

        fn write8(&mut self, _off: u32, val: u8, ctx: &mut IoCtx) {
            self.due = Some(ctx.now + u64::from(val));
        }

        fn next_event(&self) -> Option<u64> {
            self.due
        }

        fn tick(&mut self, ctx: &mut IoCtx) {
            self.ticks.push(ctx.now);
            ctx.console.push(self.id);
            ctx.irq.pulse(0);
            self.due = Some(ctx.now + self.period);
        }

        crate::impl_as_any!();
    }

    fn timer(id: u8, period: u64, due: Option<u64>) -> Box<Timer> {
        Box::new(Timer { id, period, due, ticks: Vec::new() })
    }

    fn sym(addr: u32, size: u32, name: &str) -> Symbol {
        Symbol { addr, size, name: name.to_owned(), mangled: name.to_owned() }
    }

    /// Machine without an ELF: devices at IO_BASE + i·0x100, pc 0x30000.
    fn machine(cfg: MachineConfig, devices: Vec<Box<dyn IoDevice>>, symbols: Symbols) -> Machine {
        let mut bus = SystemBus::new();
        for (i, dev) in (0u32..).zip(devices) {
            bus.io.add(IO_BASE + i * 0x100, 0x100, dev);
        }
        Machine::from_parts(cfg, bus, 0x30000, symbols)
    }

    fn cfg() -> MachineConfig {
        MachineConfig::new(PathBuf::new(), PathBuf::new())
    }

    fn timer_at(m: &Machine, idx: usize) -> &Timer {
        m.bus.io.devices[idx].as_any().downcast_ref::<Timer>().unwrap()
    }

    /// The loop bookkeeping around one instruction, with the IO write that instruction would perform.
    fn step(m: &mut Machine, io_write: Option<(u32, u8)>) {
        m.pre_step();
        if let Some((addr, val)) = io_write {
            m.bus.write8(addr, val);
        }
        m.post_step();
    }

    #[test]
    fn io_access_recomputes_deadline_and_tick_precedes_meip() {
        let mut m = machine(cfg(), vec![timer(1, 20, None)], Symbols::empty());
        m.bus.irq.global_en = true;
        m.bus.irq.mask = 0x01;
        assert_eq!(m.next_deadline, u64::MAX);

        step(&mut m, Some((IO_BASE, 12))); // at clock 0: arm for clock 12
        assert_eq!((m.bus.now, m.next_deadline), (4, 12));
        assert!(!m.bus.io_touched);
        step(&mut m, None);
        step(&mut m, None);
        assert!(timer_at(&m, 0).ticks.is_empty() && !m.cpu.meip);

        m.pre_step(); // clock 12: due
        assert_eq!(timer_at(&m, 0).ticks, [12]);
        assert!(m.cpu.meip, "the tick's edge reaches meip before the same step");
        assert_eq!(m.next_deadline, 32);
        m.post_step();
        assert_eq!(m.bus.now, 16);
    }

    #[test]
    fn due_devices_tick_in_install_order_and_time_uses_clocks_per_insn() {
        let mut config = cfg();
        config.clocks_per_insn = 10;
        let mut m = machine(config, vec![timer(1, 30, Some(10)), timer(2, 5, None), timer(3, 50, Some(10))], Symbols::empty());
        assert_eq!(m.next_deadline, 10);
        step(&mut m, None);
        assert!(m.bus.console.is_empty());
        step(&mut m, None); // clock 10
        assert_eq!(m.bus.console, [1, 3]);
        assert_eq!((m.bus.now, m.next_deadline), (20, 40));
        step(&mut m, Some((IO_BASE + 0x100, 5))); // clock 20: arm device 2 for 25
        assert_eq!(m.next_deadline, 25);
        step(&mut m, None); // clock 30
        assert_eq!(m.bus.console, [1, 3, 2]);
        assert_eq!(timer_at(&m, 1).ticks, [30]);
    }

    #[test]
    fn meip_follows_irq_line() {
        let mut m = machine(cfg(), Vec::new(), Symbols::empty());
        m.cpu.pc = 0x1234;
        m.bus.irq.pulse(0);
        m.pre_step();
        assert!(!m.cpu.meip, "global enable off");
        assert_eq!(m.bus.pc, 0x1234);
        m.bus.irq.global_en = true;
        m.pre_step();
        assert!(!m.cpu.meip, "masked");
        m.bus.irq.mask = 0x01;
        m.pre_step();
        assert!(m.cpu.meip);
        m.bus.irq.clear(0x01);
        m.pre_step();
        assert!(!m.cpu.meip);
        m.bus.irq.high_en = 0x08;
        m.bus.irq.set_high(3, true);
        m.pre_step();
        assert!(m.cpu.meip);
        m.bus.irq.set_high(3, false);
        m.pre_step();
        assert!(!m.cpu.meip);
    }

    #[test]
    fn breakpoint_stops_before_the_instruction() {
        let mut m = machine(cfg(), Vec::new(), Symbols::empty());
        m.breakpoints.push(0x30000);
        assert_eq!(m.run(10), RunExit::Breakpoint(0x30000));
        assert_eq!((m.bus.now, m.resume_pc, m.trace.count()), (0, Some(0x30000), 0));
        assert_eq!(m.run(0), RunExit::Budget);
    }

    #[test]
    fn run_traces_each_step_and_the_report_is_symbolized() {
        // 0x30000: addi x5,x5,1; 0x30004: addi x6,x6,2; 0x30008: j 0x30000
        let program = |m: &mut Machine| {
            for (addr, word) in [(0x30000, 0x0012_8293u32), (0x30004, 0x0023_0313), (0x30008, 0xFF9F_F06F)] {
                m.bus.ram[addr..addr + 4].copy_from_slice(&word.to_le_bytes());
            }
        };
        let mut untraced = machine(cfg(), Vec::new(), Symbols::empty());
        program(&mut untraced);
        assert_eq!(untraced.run(4), RunExit::Budget);
        assert_eq!((untraced.cpu.x[6], untraced.trace.count()), (2, 0));
        assert_eq!(untraced.trace_report(), "trace: not recorded (--trace or --gdb record the last 256 PCs)\n");

        let mut config = cfg();
        config.trace = true;
        let mut m = machine(config, Vec::new(), Symbols::from_symbols(vec![sym(0x30000, 4, "top")]));
        program(&mut m);
        assert_eq!(m.trace.iter().count(), 0);
        assert_eq!(m.run(4), RunExit::Budget);
        assert_eq!(
            m.trace_report(),
            "trace: last 4 of 4 steps, oldest first\n  0x00030000 top\n  0x00030004\n  0x00030008\n  0x00030000 top\n"
        );
        m.symbols = Symbols::from_symbols(vec![sym(0x30000, 12, "top")]);
        assert!(m.trace_report().ends_with("  0x00030008 top+0x8\n  0x00030000 top\n"));

        // 301 steps in total: the ring holds steps 45..=300, oldest first, across several run slices.
        m.breakpoints.push(0x30004);
        assert_eq!(m.run(1000), RunExit::Breakpoint(0x30004), "a stop is not recorded");
        assert_eq!(m.trace.count(), 4);
        m.breakpoints.clear();
        assert_eq!(m.run(297), RunExit::Budget);
        let pcs: Vec<_> = m.trace.iter().collect();
        let want: Vec<u32> = (45..301).map(|step| 0x30000 + step % 3 * 4).collect();
        assert_eq!((m.trace.count(), pcs), (301, want));
    }

    #[test]
    fn fault_hooks_halt_with_symbolized_context() {
        let symbols = Symbols::from_symbols(vec![
            sym(0x33D8C, 52, "vAssertCalled"),
            sym(0x3E00C, 32, "C_exception_handler"),
            sym(0x30178, 0, "__crt0_dummy_trap_handler"),
            sym(0x3DFC4, 0x40, "vPortSetupTimerInterrupt"),
        ]);
        let mut m = machine(cfg(), Vec::new(), symbols.clone());
        m.bus.ram[0x2000..0x2007].copy_from_slice(b"port.c\0");
        m.cpu.pc = 0x33D8C;
        m.cpu.x[1] = 0x3DFD4;
        m.cpu.x[10] = 0x2000;
        m.cpu.x[11] = 161;
        assert_eq!(
            m.run(1),
            RunExit::Halted("vAssertCalled called from vPortSetupTimerInterrupt+0x10 (port.c:161)".into())
        );

        m.cpu.pc = 0x3E00C;
        (m.cpu.x[10], m.cpu.x[11], m.cpu.x[13]) = (2, 0x3DFC8, 0xDEAD);
        assert_eq!(
            m.run(1),
            RunExit::Halted(
                "C_exception_handler called from vPortSetupTimerInterrupt+0x10: cause 0x00000002 at \
                 vPortSetupTimerInterrupt+0x4 value 0x0000dead"
                    .into()
            )
        );

        m.cpu.pc = 0x30178;
        (m.cpu.csr.mcause, m.cpu.csr.mepc) = (11, 0x3DFC4);
        assert_eq!(
            m.run(1),
            RunExit::Halted("__crt0_dummy_trap_handler entered: mcause 0x0000000b mepc vPortSetupTimerInterrupt".into())
        );

        let mut quiet = cfg();
        quiet.halt_on_fault = false;
        assert!(!machine(quiet, Vec::new(), symbols).hooks.armed());
    }

    #[test]
    fn get_mem_panic_loop_halts_with_the_caller() {
        let symbols = Symbols::from_symbols(vec![
            sym(0x40000, 0x40, "_Z7get_memj"),
            sym(0x3DFC4, 0x40, "vPortSetupTimerInterrupt"),
        ]);
        let load = |words: &[(usize, u32)]| {
            let mut bus = SystemBus::new();
            for &(addr, word) in words {
                bus.ram[addr..addr + 4].copy_from_slice(&word.to_le_bytes());
            }
            bus
        };
        // get_mem as in the ELF: addi sp,sp,-32; sw ra,28(sp); ...; j . at +0x30.
        let prologue: [(usize, u32); 2] = [(0x40000, 0xFE01_0113), (0x40004, 0x0011_2E23)];
        let mut bus = load(&[prologue[0], prologue[1], (0x40030, JUMP_TO_SELF)]);
        bus.ram[0x201C..0x2020].copy_from_slice(&0x3DFD4u32.to_le_bytes());
        let mut m = Machine::from_parts(cfg(), bus, 0x40030, symbols.clone());
        assert_eq!((m.hooks.panic_loop, m.hooks.panic_ra_slot), (0x40030, 28));
        m.cpu.x[2] = 0x2000;
        assert_eq!(
            m.run(10),
            RunExit::Halted(
                "get_mem PANIC: pvPortMalloc returned NULL, get_mem called from vPortSetupTimerInterrupt+0x10".into()
            )
        );
        assert_eq!(m.bus.now, 0, "halted before the loop executes");

        let no_loop = Machine::from_parts(cfg(), load(&prologue), 0x30000, symbols);
        assert!(!no_loop.hooks.armed(), "no `j .` in get_mem, no hook");
    }

    #[test]
    fn new_rejects_zero_clocks_per_insn() {
        let mut config = cfg();
        config.clocks_per_insn = 0;
        let err = Machine::new(config).err().expect("rejected before loading");
        assert!(err.to_string().contains("clocks_per_insn must be at least 1"), "{err}");
    }

    #[test]
    fn unmapped_log_lines_and_summary() {
        let mut config = cfg();
        config.log.unmapped = true;
        let mut m = machine(config, Vec::new(), Symbols::from_symbols(vec![sym(0x1000, 0x100, "poll")]));
        assert_eq!(m.unmapped_summary().unwrap(), "unmapped summary: 0 addresses\n");
        m.bus.pc = 0x1010;
        m.bus.read8(0x10FF_0000);
        m.bus.write8(0x10FF_0000, 0x5A);
        assert_eq!(m.format_unmapped(&m.bus.accesses[0]), "unmapped R 0x10ff0000 @0x00001010 poll+0x10");
        assert_eq!(m.format_unmapped(&m.bus.accesses[1]), "unmapped W 0x10ff0000 0x5a @0x00001010 poll+0x10");
        m.post_step();
        assert!(m.bus.accesses.is_empty(), "flushed after the step");
        assert_eq!(
            m.unmapped_summary().unwrap(),
            "unmapped summary: 1 addresses\n  address        reads    writes\n  0x10ff0000         1         1\n"
        );
        assert_eq!(machine(cfg(), Vec::new(), Symbols::empty()).unmapped_summary(), None);
    }

    #[test]
    fn io_trace_line_names_the_device() {
        let mut config = cfg();
        config.log.io = true;
        let mut m = machine(config, vec![timer(1, 1, None)], Symbols::empty());
        m.bus.pc = 0x40;
        m.bus.now = 123_456_789;
        m.bus.write8(IO_BASE + 7, 3);
        assert_eq!(
            m.format_io(&m.bus.accesses[0]),
            "io W 0x10000007 0x03 timer+0x7 @0x00000040 0x00000040 t=1.23456789",
            "the time is the emulated clock of the access, in seconds to the 10 ns tick"
        );
    }

    #[test]
    fn stats_and_default_display() {
        let mut m = machine(cfg(), Vec::new(), Symbols::empty());
        m.bus.now = 3 * time::CLOCK_HZ / 2;
        m.irqs = 2;
        assert_eq!(m.stats(), "0 instructions, 0 skipped idle, 1.500 s emulated, 2 IRQs taken, pc 0x00030000");
        m.input(HostInput::MenuButton(true));
        let snap = m.display();
        assert_eq!((snap.screen.len(), snap.color.len(), snap.palette.len(), snap.now_ms), (4096, 4096, 64, 1500));
    }

    #[test]
    fn c64_input_routing_and_frame() {
        use crate::c64host::mock::{Call, Mock};

        let mut bus = SystemBus::new();
        devices::install_all(&mut bus.io, &cfg());
        let mut m = Machine::from_parts(cfg(), bus, 0x30000, Symbols::empty());
        assert_eq!(m.display().c64, None, "no C64 attached");
        m.bus.now = 1234;
        let mock = Mock::default();
        m.attach_c64(Box::new(mock.clone()));
        assert_eq!(m.next_deadline, 1234 + 100_000, "the C64 sync is scheduled");

        m.input(HostInput::Key { row: 1, col: 2, down: true });
        m.bus.io.get_mut::<Overlay>().unwrap().regs[REG_TRANSPARENCY] = 0xC0;
        m.input(HostInput::Key { row: 7, col: 7, down: true });
        m.input(HostInput::Key { row: 1, col: 2, down: false });
        m.input(HostInput::Joystick(0xEF));
        m.input(HostInput::JoystickPort { port: 1, lines: 0xFE });
        m.input(HostInput::Restore(true));
        assert_eq!(
            mock.take(),
            [
                Call::CartRom(crate::c64host::CartRom::LARGE),
                Call::Advance(1234),
                Call::Key(1, 2, true),
                Call::Key(1, 2, false),
                Call::Joystick(1, 0xFF),
                Call::Joystick(2, 0xEF),
                Call::Joystick(1, 0xFE),
                Call::Joystick(2, 0xEF),
                Call::Nmi(true),
            ],
            "key-down dropped while the overlay owns the keyboard"
        );
        assert_eq!(m.bus.io.get::<U64Io>().unwrap().matrix[7], 0x80, "the overlay still sees the key");
        // S36: U64II_KEYB_JOY reads the port the select picks, through the lines install_all shares.
        let keyb_joy = |m: &mut Machine| m.bus.io.get_mut::<U64Io>().unwrap().peek8(0x06);
        assert_eq!(keyb_joy(&mut m), 0xEF, "select 0: port 2");
        m.bus.io.get_mut::<U64Io>().unwrap().latch[0x06] = 1;
        assert_eq!(keyb_joy(&mut m), 0xFE, "select 1: port 1");
        // CARTSLOT: U64_CART_DETECT follows the backend's physical cartridge after the next C64 access.
        let cart_detect = |m: &mut Machine| m.bus.io.get_mut::<U64Io>().unwrap().peek8(0x03);
        assert_eq!(cart_detect(&mut m), 0x03, "empty expansion port");
        *mock.detect.borrow_mut() = Some(0x02);
        m.input(HostInput::Restore(false));
        assert_eq!(cart_detect(&mut m), 0x03, "no C64 access yet");
        // addi x0,x0,0 at the entry, so one step runs the device tick that is due.
        m.bus.ram[0x30000..0x30004].copy_from_slice(&0x0000_0013u32.to_le_bytes());
        m.bus.now += 100_000;
        m.run(1);
        assert_eq!(cart_detect(&mut m), 0x02, "the 1 ms sync refreshed it");
        assert!(m.c64_cart_slot().is_none(), "the mock has no cartridge slot");
        mock.take();
        assert_eq!(m.display().c64.map(|f| f.width), Some(2));
    }

    #[test]
    fn new_loads_the_firmware_and_resolves_hooks() {
        let Some(root) = firmware_root() else { return };
        let m = Machine::new(MachineConfig::new(root.join(FIRMWARE_ELF), root.join("roms"))).unwrap();
        assert_eq!(m.cpu.pc, 0x30000);
        assert!(m.cpu.x.iter().all(|&r| r == 0));
        assert_ne!(u32::from_le_bytes(m.bus.ram[0x30000..0x30004].try_into().unwrap()), 0);
        assert!(m.hooks.assert != NO_HOOK && m.hooks.exception != NO_HOOK);
        assert_eq!(m.hooks.early_trap, 0x30178);
        // get_mem(unsigned int) @0x43294: `sw ra,28(sp)`, `j .` at 0x432C4.
        assert_eq!((m.hooks.panic_loop, m.hooks.panic_ra_slot), (0x432C4, 28));
        assert_eq!(m.symbols.format(0x33700), "freertos_risc_v_trap_handler");
    }

    // ---- S19 idle skip ----

    const J_BACK_4: u32 = 0xFFDF_F06F; // jal x0, -4
    const J_SELF: u32 = 0x0000_006F; // jal x0, 0

    /// A machine running `program` at 0x30000, the skip on or off, with a timer ticking every 1000 clocks.
    fn idle_machine(program: &[u32], skip: bool) -> Machine {
        let mut config = cfg();
        config.idle_skip = skip;
        let mut m = machine(config, vec![timer(1, 1000, Some(1000))], Symbols::empty());
        for (i, word) in (0u32..).zip(program) {
            m.bus.write32(0x30000 + 4 * i, *word);
        }
        m
    }

    /// Everything a skip must leave as it was, and the instructions counted either way.
    fn idle_state(m: &Machine) -> (u32, [u32; 32], rv32::Csrs, u64, u64, u64, Vec<u64>) {
        let executed_or_skipped = m.cpu.insns + m.idle_insns;
        (m.cpu.pc, m.cpu.x, m.cpu.csr.clone(), m.bus.now, m.irqs, executed_or_skipped, timer_at(m, 0).ticks.clone())
    }

    #[test]
    fn idle_skip_fast_forwards_a_fixed_loop_to_the_same_state() {
        // addi a1,x0,5 ; j -4
        let program = [0x0050_0593, J_BACK_4];
        let (mut on, mut off) = (idle_machine(&program, true), idle_machine(&program, false));
        for _ in 0..7 {
            assert_eq!(on.run(3_333), RunExit::Budget);
            assert_eq!(off.run(3_333), RunExit::Budget);
            assert_eq!(idle_state(&on), idle_state(&off));
        }
        assert_eq!(on.cpu.insns + on.idle_insns, 7 * 3_333, "skipped instructions count against the budget");
        assert!(on.idle_insns > on.cpu.insns, "most of the loop was skipped: {} of {}", on.idle_insns, on.cpu.insns);
        assert_eq!(off.idle_insns, 0);
        assert_eq!(timer_at(&on, 0).ticks.len(), 93, "the timer ticked at every deadline");
    }

    #[test]
    fn idle_skip_takes_an_interrupt_at_the_same_clock_and_pc() {
        // 0x30000: addi a1,x0,5 ; j -4        0x30100 (mtvec): j .
        let mut program = vec![0u32; 0x41];
        (program[0], program[1], program[0x40]) = (0x0050_0593, J_BACK_4, J_SELF);
        let (mut on, mut off) = (idle_machine(&program, true), idle_machine(&program, false));
        for m in [&mut on, &mut off] {
            m.bus.irq.global_en = true;
            m.bus.irq.mask = 0x01;
            m.cpu.csr.mtvec = 0x30100;
            m.cpu.csr.mie = rv32::csr::MIE_MEIE;
            m.cpu.csr.mstatus = rv32::csr::MSTATUS_MIE;
        }
        assert_eq!(on.run(5_000), RunExit::Budget);
        assert_eq!(off.run(5_000), RunExit::Budget);
        assert_eq!(on.irqs, 1);
        assert_eq!(idle_state(&on), idle_state(&off), "same clock, mepc and handler loop");
        assert!(on.idle_insns > 0, "the handler's `j .` is skipped too");
    }

    #[test]
    fn idle_skip_leaves_loops_alone_that_change_something() {
        let io_base_lui = 0x1000_0737; // lui a4, 0x10000
        let loops: [(&str, Vec<u32>); 3] = [
            ("writes RAM", vec![0x0004_0637, 0x00B6_2023, J_BACK_4]), // lui a2,0x40 ; sw a1,0(a2) ; j -4
            ("reads IO", vec![io_base_lui, 0x0007_2683, J_BACK_4]), // lui a4,0x10000 ; lw a3,0(a4) ; j -4
            ("counts", vec![0x0015_8593, J_BACK_4]),              // addi a1,a1,1 ; j -4
        ];
        for (what, program) in loops {
            let mut m = idle_machine(&program, true);
            assert_eq!(m.run(20_000), RunExit::Budget);
            assert_eq!(m.idle_insns, 0, "a loop that {what}");
        }
    }

    #[test]
    fn idle_skip_is_off_for_debugging_and_when_disabled() {
        let program = [0x0050_0593, J_BACK_4];
        let cases: [(&str, fn(&mut Machine)); 4] = [
            ("disabled", |m| m.cfg.idle_skip = false),
            ("breakpoints", |m| m.breakpoints.push(0x50000)),
            ("trace", |m| m.cfg.trace = true),
            ("--log io", |m| m.cfg.log.io = true),
        ];
        for (what, set) in cases {
            let mut m = idle_machine(&program, true);
            set(&mut m);
            assert_eq!(m.run(20_000), RunExit::Budget);
            assert_eq!((m.idle_insns, m.cpu.insns), (0, 20_000), "{what}");
        }
    }

    /// S19 §8.3: the firmware runs to the same state with the skip on and off. `UE2_IDLE_SKIP_SECONDS` sets the
    /// emulated length (default 3).
    #[test]
    fn idle_skip_leaves_the_firmware_run_unchanged() {
        let Some(root) = firmware_root() else { return };
        let secs: u64 = std::env::var("UE2_IDLE_SKIP_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
        let run = |skip: bool| {
            let mut config = MachineConfig::new(root.join(FIRMWARE_ELF), root.join("roms"));
            config.idle_skip = skip;
            let mut m = Machine::new(config).unwrap();
            // The RTC counts host seconds; hold it so both runs read the same time (byte 0 locks, misc.rs).
            for off in 0..3 {
                m.bus.write8(0x1006_0400 + off, 0);
            }
            let mut console = Vec::new();
            let mut left = secs * time::CLOCK_HZ / m.cfg.clocks_per_insn;
            while left > 0 {
                let slice = left.min(100_000);
                assert_eq!(m.run(slice), RunExit::Budget);
                console.extend(m.drain_console());
                left -= slice;
            }
            (m, console)
        };
        let ((on, on_console), (off, off_console)) = (run(true), run(false));
        assert_eq!(String::from_utf8_lossy(&on_console), String::from_utf8_lossy(&off_console));
        assert_eq!((on.cpu.pc, on.cpu.x, &on.cpu.csr), (off.cpu.pc, off.cpu.x, &off.cpu.csr));
        assert_eq!((on.bus.now, on.irqs, on.cpu.insns + on.idle_insns), (off.bus.now, off.irqs, off.cpu.insns));
        assert!(on.bus.ram == off.bus.ram, "DDR differs");
        eprintln!(
            "{secs} s emulated: {} executed, {} skipped ({:.1} %)",
            on.cpu.insns,
            on.idle_insns,
            100.0 * on.idle_insns as f64 / off.cpu.insns as f64
        );
    }
    /// S37: the TRX marker, synthetic: the symbol, else the string inside a loaded segment.
    #[test]
    fn trx_firmware_is_found_by_symbol_or_string() {
        let sym = |name: &str| Symbol { addr: 0x30000, size: 4, name: name.into(), mangled: String::new() };
        let mut ram = vec![0u8; 0x1000];
        let none = Symbols::empty();
        assert_eq!(trx_firmware(&Symbols::from_symbols(vec![sym("trx_lv_open()")]), &ram, &[]), Some("trx_lv_open"));
        assert_eq!(trx_firmware(&Symbols::from_symbols(vec![sym("trx_lv_close()")]), &ram, &[]), None);
        ram[0x800..0x808].copy_from_slice(b"[TRX UI]");
        assert_eq!(trx_firmware(&none, &ram, &[(0x100, 0x700)]), None, "outside the segments");
        assert!(trx_firmware(&none, &ram, &[(0x100, 0x710)]).is_some());
    }

    /// S37: auto follows the marker, an explicit mode wins.
    #[test]
    fn trx_io_auto_follows_the_firmware_and_explicit_wins() {
        let mode = |m, marker| resolve_trx_io(m, marker).0;
        assert_eq!(mode(TrxIo::Auto, Some("trx_lv_open")), TrxIo::On);
        assert_eq!(mode(TrxIo::Auto, None), TrxIo::Off);
        assert_eq!(mode(TrxIo::On, None), TrxIo::On);
        assert_eq!(mode(TrxIo::Off, Some("trx_lv_open")), TrxIo::Off);
        assert_eq!(resolve_trx_io(TrxIo::Auto, Some("trx_lv_open")).1.unwrap(), "trx-io: on (TRX firmware: trx_lv_open)");
    }

    /// S37: the real images. The stock ELF has no marker; TRX64-Ultimate's `trx64/main` build (`UE2_TRX_FIRMWARE`, or
    /// software/ultimate-app beside this repo) has the symbol in its ELF and the string in its .app.
    #[test]
    fn trx_firmware_on_the_real_images() {
        if let Some(root) = firmware_root() {
            let elf = root.join(FIRMWARE_ELF);
            let mut ram = vec![0; crate::bus::RAM_SIZE];
            let fw = loader::load_firmware(&elf, &mut ram).unwrap();
            assert_eq!(trx_firmware(&Symbols::from_elf(&elf).unwrap(), &ram, &fw.segments), None, "stock");
        }
        let app = std::env::var_os("UE2_TRX_FIRMWARE").map(PathBuf::from).unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../TRX64-Ultimate/software/ultimate-app")
        });
        let elf = app.join(FIRMWARE_ELF);
        if !elf.is_file() {
            eprintln!("skipping: TRX firmware ELF not found at {}", elf.display());
            return;
        }
        let mut ram = vec![0; crate::bus::RAM_SIZE];
        let fw = loader::load_firmware(&elf, &mut ram).unwrap();
        assert_eq!(trx_firmware(&Symbols::from_elf(&elf).unwrap(), &ram, &fw.segments), Some("trx_lv_open"));
        let image = app.join("u64ii/ultimate.app");
        if image.is_file() {
            let mut ram = vec![0; crate::bus::RAM_SIZE];
            let fw = loader::load_firmware(&image, &mut ram).unwrap();
            assert_eq!(trx_firmware(&Symbols::empty(), &ram, &fw.segments), Some("\"[TRX UI]\" in the image"));
        }
    }

    /// 0x30000: addi x5,x0,7; 0x30004: lw x5,0(x6); 0x30008: j . — with x6 at the sampler's window.
    fn noack_machine(absent: &str, mode: &str, keep_going: bool) -> Machine {
        let mut m = machine(cfg(), Vec::new(), Symbols::from_symbols(vec![sym(0x30000, 12, "probe")]));
        for (addr, word) in [(0x30000, 0x0070_0293u32), (0x30004, 0x0003_2283), (0x30008, 0x0000_006F)] {
            m.bus.ram[addr..addr + 4].copy_from_slice(&word.to_le_bytes());
        }
        m.cpu.x[6] = 0x1004_8000;
        let cfg = AbsentConfig::parse(Some(absent), Some(mode), keep_going).unwrap();
        m.bus.absent = Some(Box::new(AbsentMap::new(&cfg)));
        m
    }

    /// S41 §3: a `noack` load halts on its instruction, which is put back as if it never ran, and the report names the
    /// block, the access, the PC and the time; `ack0` and `--absent-continue` run on, the latter counting the stall.
    #[test]
    fn a_noack_access_halts_on_its_instruction() {
        let mut m = noack_machine("sampler", "sampler=noack", false);
        m.bus.console.extend_from_slice(b"boot\r\nSampler init\r\n");
        let RunExit::Halted(msg) = m.run(10) else { panic!("no halt") };
        assert!(msg.starts_with("absent sampler (noack): 32-bit read of 0x10048000 never completes\n"), "{msg}");
        assert!(msg.contains("  pc 0x00030004 probe+0x4\n") && msg.contains("last console line: \"Sampler init\""), "{msg}");
        assert!(msg.contains(" s emulated since boot, 1 instructions"), "{msg}");
        assert_eq!((m.cpu.pc, m.cpu.x[5], m.cpu.insns), (0x30004, 7, 1), "the load never completed");
        assert!(m.bus.noack.is_none());

        let mut m = noack_machine("sampler", "sampler=ack0", false);
        assert_eq!(m.run(10), RunExit::Budget);
        assert_eq!(m.cpu.x[5], 0, "ack0 reads 0");
        assert!(m.absent_summary().is_none());

        let mut m = noack_machine("sampler", "sampler=noack", true);
        assert_eq!(m.run(10), RunExit::Budget);
        assert_eq!((m.cpu.x[5], m.cpu.pc), (0, 0x30008), "completed as ack0");
        let summary = m.absent_summary().unwrap();
        assert!(summary.starts_with("absent summary: 1 addresses stalled"), "{summary}");
        assert!(summary.contains("0x10048000 sampler          1         0  0x00030004 probe+0x4"), "{summary}");
    }

    /// A TRX machine of parts with the C64 port, the TRX block and the bitstream's `absent` and `modes`, the mock C64
    /// attached.
    fn swap_machine(absent: &str, modes: Option<&str>) -> (Machine, crate::c64host::mock::Mock) {
        let mut config = cfg();
        config.trx_io = TrxIo::On;
        config.absent = AbsentConfig::parse(Some(absent), modes, true).unwrap();
        let mut bus = SystemBus::new();
        devices::install_all(&mut bus.io, &config);
        let mut m = Machine::from_parts(config, bus, 0x30000, Symbols::empty());
        m.start_absent();
        let old = crate::c64host::mock::Mock::default();
        m.attach_c64(Box::new(old.clone()));
        (m, old)
    }

    /// The host's registers: REU_SIZE and JOY1_SWOUT (cart page 0, core page 3), MATRIX_KEYB row 0 (page 1),
    /// ETHSTREAM_ENA (page 2), C64_PLD_ACC (page 4), U64_DEBUG_REGISTER (page 5).
    const PAGES: [u32; 6] = [0x1004_0009, 0x1018_0030, 0x1010_0300, 0x1010_040F, 0x1018_1000, 0x1018_1800];

    fn pages(m: &mut Machine) -> [u8; 6] {
        PAGES.map(|addr| rv32::Bus::read8(&mut m.bus, addr))
    }

    fn write_pages(m: &mut Machine, vals: [u8; 6]) {
        for (addr, val) in PAGES.into_iter().zip(vals) {
            rv32::Bus::write8(&mut m.bus, addr, val);
        }
    }

    /// S41 §5, the decoupler (ultimate_sys.vhd 155353d): `core absent` stops the C64 (no tick, no frame, no backend for
    /// the monitor), blanks the TRX output to the overlay, resets pages 0 and 3 once with the C64's reset held and
    /// raises the reset sense; then pages 0 and 3 take writes, the core's windows ack 0, the DMA window's SID range too.
    /// `core present` attaches a fresh backend that is handed the host's registers as they stand, with no reset event;
    /// pages 1, 2, 4 and 5 are never touched.
    #[test]
    fn core_absent_and_present_swap_the_c64() {
        use crate::c64host::mock::Call;
        let (mut m, old) = swap_machine("sampler", None);
        assert!(old.take().contains(&Call::Absent(Block::Sampler)));
        assert!(m.display().c64.is_some() && !m.core_is_absent());
        assert_eq!(m.core_present(None), Err("core present: the core is present".into()));
        write_pages(&mut m, [0x02, 0x7F, 0x55, 0x21, 0x66, 0x77]);
        assert_eq!(pages(&mut m), [0x02, 0x7F, 0x55, 0x21, 0x66, 0x77]);
        m.bus.irq.mask = 0x80;

        assert!(m.core_absent().unwrap().starts_with("core absent (ack0)"));
        assert_eq!(pages(&mut m), [0x07, 0xFF, 0x55, 0x21, 0x66, 0x77], "pages 0 and 3 back at power-up, once");
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1004_0000) & 0x04, 0x04, "C64_MODE: the reset is held");
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1004_0003), 0x11, "C64_CLOCK_DETECT: the reset sensed");
        assert_eq!(m.bus.irq.active(), 0x80, "the reset sense, ITU low bit 7");
        m.bus.irq.clear(0x80);
        write_pages(&mut m, [0x03, 0x7E, 0x54, 0x20, 0x67, 0x76]);
        assert_eq!(pages(&mut m), [0x03, 0x7E, 0x54, 0x20, 0x67, 0x76], "writes stick while decoupled");
        assert_eq!(m.core_absent(), Err("core absent: the core is already absent".into()));
        let port = m.bus.io.get::<C64Port>().unwrap();
        assert!(port.next_event().is_none() && port.backend().is_none());
        let snap = m.display();
        assert_eq!((snap.c64, snap.trx), (None, Some(TrxOutput::default())));
        assert_eq!(m.picture_counter(), 0);
        assert_eq!(m.bus.absent_at(0x1002_0000), Some((Block::Core, Mode::Ack0)));
        assert_eq!(m.bus.absent_at(0x1004_4000), None, "the UCI is the host's");
        assert_eq!(m.bus.absent_at(0x1004_0000), None, "the cart registers are the host's");
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1005_D400), 0, "the decoupler acks the DMA window's SID range with 0");
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1002_0000), 0);
        assert!(m.bus.noack.is_none());

        old.take();
        assert!(m.take_c64_backend().is_some());
        let new = crate::c64host::mock::Mock::default();
        assert!(m.core_present(Some(Box::new(new.clone()))).unwrap().starts_with("core present"));
        assert_eq!(pages(&mut m), [0x03, 0x7E, 0x54, 0x20, 0x67, 0x76], "the host's registers as they were");
        assert_eq!(m.bus.irq.active(), 0, "the release raises nothing");
        let calls = new.take();
        for call in [Call::Matrix([0x54, 0, 0, 0, 0, 0, 0, 0]), Call::Reset(true), Call::ReuSize(1024), Call::Absent(Block::Sampler)] {
            assert!(calls.contains(&call), "{call:?} in {calls:?}");
        }
        assert!(new.core.borrow().contains(&(0x30, 0x7E)), "the written core config");
        assert!(!m.core_is_absent() && m.bus.absent_at(0x1002_0000).is_none());
        assert!(m.bus.io.get::<C64Port>().unwrap().next_event().is_some());
        assert!(m.display().c64.is_some());
        assert!(old.take().is_empty(), "the old core is gone");
    }

    /// S41 §5, a host without the decoupler (`core=noack`): nothing is reset and no reset is sensed on entry, the
    /// core's windows never answer, ultimate_sys answers the DMA window's SID range itself with $FF unless MEMONLY.
    #[test]
    fn without_the_decoupler_nothing_resets_and_the_core_stalls() {
        let (mut m, _old) = swap_machine("sampler", Some("core=noack"));
        write_pages(&mut m, [0x02, 0x7F, 0x55, 0x21, 0x66, 0x77]);
        m.bus.irq.mask = 0x80;
        assert!(m.core_absent().unwrap().starts_with("core absent (noack)"));
        assert_eq!(pages(&mut m), [0x02, 0x7F, 0x55, 0x21, 0x66, 0x77]);
        assert_eq!(m.bus.irq.active(), 0);
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1005_D400), 0xFF);
        assert!(m.bus.noack.is_none());
        rv32::Bus::write8(&mut m.bus, 0x1018_0003, 1);
        assert_eq!(rv32::Bus::read8(&mut m.bus, 0x1005_D400), 0);
        assert!(m.bus.noack.take().is_some(), "MEMONLY: the byte goes to the core, which is not there");
    }
}
