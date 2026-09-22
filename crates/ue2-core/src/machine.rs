//! Machine: CPU + bus + devices + run loop. Spec: docs/specs/S02-core.md
//!
//! Loop per docs/ARCHITECTURE.md §Execution model. The idle task spins without WFI
//! (docs/hw/01-cpu-boot-memory.md §Functional model "Idle"), so emulated time advances per executed step, and
//! the FreeRTOS tick exists only as an ITU interrupt (01 H8), so `meip` follows `IrqState::line` before every
//! step.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use crate::bus::{Access, SystemBus, IO_BIT, RAM_MASK};
use crate::c64host::{C64Backend, C64CartSlot};
use crate::devices::overlay::{Overlay, REG_TRANSPARENCY};
use crate::devices::{self, c64::C64Port, itu::Itu, u64io::U64Io};
use crate::host::{DisplaySnapshot, HostInput};
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
    /// ITU capability word (big-endian at 0x1000000C). T0 default 0x34000222.
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
}

impl MachineConfig {
    pub fn new(elf: PathBuf, rom_dir: PathBuf) -> Self {
        MachineConfig {
            elf,
            rom_dir,
            capabilities: 0x3400_0222,
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
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// Instruction budget used up.
    Budget,
    /// PC reached a breakpoint (instruction not yet executed).
    Breakpoint(u32),
    /// Firmware fault hook fired or illegal instruction.
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
}

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
    pub fn new(cfg: MachineConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(cfg.clocks_per_insn > 0, "clocks_per_insn must be at least 1, or emulated time stands still");
        let mut bus = SystemBus::new();
        devices::install_all(&mut bus.io, &cfg);
        let fw = loader::load_firmware(&cfg.elf, &mut bus.ram)?;
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
        let mut machine = Self::from_parts(cfg, bus, fw.entry, symbols);
        machine.segments = fw.segments;
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
            self.pre_step();
            let exit = self.cpu.step(&mut self.bus);
            self.post_step();
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

    /// `io R/W addr val device+off @pc sym+off`.
    fn format_io(&self, a: &Access) -> String {
        let target = match self.bus.io.resolve(a.addr) {
            Some((dev, off)) => format!("{}+{off:#x}", self.bus.io.devices[dev].name()),
            None => "unmapped".to_owned(),
        };
        format!(
            "io {} {:#010x} {:#04x} {target} @{:#010x} {}",
            rw(a.write),
            a.addr,
            a.val,
            a.pc,
            self.symbols.format(a.pc)
        )
    }

    /// `unmapped R/W addr [val] @pc sym+off`.
    fn format_unmapped(&self, a: &Access) -> String {
        let val = if a.write { format!(" {:#04x}", a.val) } else { String::new() };
        format!("unmapped {} {:#010x}{val} @{:#010x} {}", rw(a.write), a.addr, a.pc, self.symbols.format(a.pc))
    }

    pub fn drain_console(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bus.console)
    }

    /// Attach a C64 behind the cart/DMA registers (docs/specs/S14-c64-trx64.md §3). Without `devices::c64::C64Port`
    /// installed the backend is dropped.
    pub fn attach_c64(&mut self, backend: Box<dyn C64Backend>) {
        let now = self.bus.now;
        // CARTSLOT: U64_CART_DETECT (U64Io) reads the lines of the backend's physical cartridge (docs/status/cart-slot.md).
        let cart_detect = self.bus.io.get::<U64Io>().map(|io| io.cart_detect.clone());
        if let Some(port) = self.bus.io.get_mut::<C64Port>() {
            port.attach(backend, now);
            if let Some(cell) = cart_detect {
                port.set_cart_detect(cell);
            }
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
                if let Some(dev) = io.get_mut::<U64Io>() {
                    dev.set_joystick(lines);
                }
                if let Some(dev) = io.get_mut::<C64Port>() {
                    dev.set_joystick(lines);
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

    /// State of USB hub port `port` (1-based); None without the USB device or for a port the hub does not have.
    pub fn usb_port(&self, port: usize) -> Option<devices::usb::UsbPortInfo> {
        self.bus.io.get::<devices::usb::Usb>()?.port_info(port)
    }

    /// Overlay snapshot from `devices::overlay::Overlay`, or blank buffers of the sizes documented on
    /// `DisplaySnapshot` when no overlay is installed, plus the frame of an attached C64.
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
            },
        };
        snap.c64 = self.bus.io.get::<C64Port>().and_then(C64Port::frame);
        snap
    }

    /// The UDP stream generators, once per call: a new VIC frame becomes datagrams, and everything waiting goes
    /// back to the caller to put on the wire (S24 §4). Nothing happens while `ETHSTREAM_ENA` has no enabled
    /// stream, so a machine nobody streams from does not build a frame it will not send.
    pub fn stream_pump(&mut self) -> Vec<Vec<u8>> {
        let ena = self.bus.io.get::<devices::u64io::U64Io>().map_or(0, devices::u64io::U64Io::ethstream_ena);
        if ena & 0x01 != 0 {
            let counter = self.bus.io.get::<C64Port>().map_or(0, C64Port::frame_counter);
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
        m.bus.write8(IO_BASE + 7, 3);
        assert_eq!(m.format_io(&m.bus.accesses[0]), "io W 0x10000007 0x03 timer+0x7 @0x00000040 0x00000040");
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
                Call::Nmi(true),
            ],
            "key-down dropped while the overlay owns the keyboard"
        );
        assert_eq!(m.bus.io.get::<U64Io>().unwrap().matrix[7], 0x80, "the overlay still sees the key");
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
}
