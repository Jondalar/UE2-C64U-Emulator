//! S23 M1 — UE2 as the second host of TRX64's monitor (`trx64-monitor`, TRX64 Spec 864).
//!
//! The library knows what a verb means; this file says what our world can answer. The C64 and drive A are the
//! TRX64 machine's own, so they are reached through [`MonitorHost::machine`] exactly as the daemon reaches them.
//! The firmware's RISC-V is a CPU the library has never seen, so it comes as a [`CpuView`] of its own: registers
//! and memory yes, no flag register, no disassembler (S23 §4), and no debug gates — the watch tables are a 6502
//! shape.
//!
//! Our own verbs are S23 M2 and live below, behind the library's dispatch. [`uci`] is what `config` knocks on when
//! a verb has to reach the *running* firmware rather than the bytes it left behind.
//!
//! Run control for both CPUs (S23 M3) is [`run`].

mod config;
mod devices;
pub mod run;
mod uci;

use c64_bridge::Trx64Backend;
use trx64_core::drive::DrivePosition;
use trx64_monitor::host::{CpuView, Device, MonitorHost, Reg};
use trx64_monitor::{addr_spans, verbs, MonitorSession};
use ue2_core::devices::c64::C64Port;
use ue2_core::machine::Machine;
use ue2_core::settings;

use crate::gdb::{peek8, ram_index, task_list};

/// The name `device` takes for the firmware's core.
pub const FW: &str = "fw";

/// RISC-V registers in ABI order, the names the firmware's own build uses (`riscv-abi`). x0 is `zero` and
/// read-only.
const FW_REGS: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7",
    "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

/// Our host: one emulator machine, which carries both CPUs — the RISC-V that runs the firmware, and the C64
/// behind the firmware's registers.
pub struct Host<'a> {
    m: &'a mut Machine,
    state: &'a mut State,
}

impl<'a> Host<'a> {
    /// A host, if this machine has a C64 (`--c64 trx64`). Without one there is no monitor: every verb of the
    /// library is about `trx64_core::Machine`.
    pub fn new(m: &'a mut Machine, state: &'a mut State) -> Option<Host<'a>> {
        trx64(m).is_some().then_some(Host { m, state })
    }
}

/// What our own verbs keep between lines: the items `config set` changed in the running firmware, which
/// `config write` then makes permanent. The library's own state is its `MonitorSession`, beside this one.
#[derive(Default)]
pub struct State {
    staged: Vec<settings::Record>,
    /// Run control (S23 M3): whether the firmware is held, and why.
    pub run: run::State,
}

impl State {
    /// A later `set` of the same item replaces the earlier one, as a `.cfg` with two lines for one item does
    /// (`settings::resolve`).
    fn stage(&mut self, record: settings::Record) {
        self.staged.retain(|r| !(r.page == record.page && r.id == record.id));
        self.staged.push(record);
    }
}

/// The TRX64 backend behind the firmware's C64 registers, through the downcast hook of `C64Backend`.
fn trx64(m: &mut Machine) -> Option<&mut Trx64Backend> {
    m.bus.io.get_mut::<C64Port>()?.backend_mut()?.as_any_mut()?.downcast_mut::<Trx64Backend>()
}

impl MonitorHost for Host<'_> {
    fn machine(&mut self) -> &mut trx64_core::Machine {
        // `new` checked it; a backend cannot leave a port while the host borrows the machine.
        trx64(self.m).expect("the C64 this host was built for").trx64()
    }

    /// Only the firmware's core: the two 6502s are the machine's own and the library reaches them itself.
    fn cpu(&mut self, dev: Device) -> Option<&mut dyn CpuView> {
        (dev == Device::Host(FW)).then_some(self as &mut dyn CpuView)
    }

    /// The C64, every powered drive by its unit (drives A and B, S27), and the firmware's core.
    fn devices(&mut self) -> Vec<Device> {
        let m = self.machine();
        let mut out = vec![Device::C64];
        for pos in [DrivePosition::A, DrivePosition::B] {
            let d = m.drive(pos);
            if d.powered() {
                out.push(Device::Drive(d.unit()));
            }
        }
        out.push(Device::Host(FW));
        out
    }

    /// S23 §3: the C64's stop is the machine's own, through `C64_STOP`.
    fn set_halted(&mut self, halted: bool) -> Result<(), String> {
        run::set_halted(self, halted)
    }

    fn resume(&mut self, until: trx64_monitor::RunUntil) -> Result<trx64_monitor::Resumption, String> {
        run::resume(self, until)
    }

    fn step(&mut self, n: u64, over: bool) -> Result<trx64_monitor::StopInfo, String> {
        run::step(self, n, over)
    }
}

/// Our own verbs (S23 §5): the Ultimate side, which the library has never seen.
impl Host<'_> {
    /// A line the library declined. `None` means nobody owns it.
    fn ours(&mut self, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
        match verb {
            "fw" => Some(self.verb_fw(args)),
            "c64" => Some(run::c64(self, args)),
            "status" => Some(Ok(run::status(self))),
            "clock" => Some(Ok(self.verb_clock())),
            "config" => Some(config::verb(self, args)),
            _ => run::alias(self, verb, args).or_else(|| devices::verb(self, verb, args)),
        }
    }

    /// `fw` — the firmware's core: its registers, or `fw tasks` for the FreeRTOS task list.
    fn verb_fw(&mut self, args: &[&str]) -> Result<String, String> {
        match args {
            [] => {
                let mut out = format!("  pc  {:08x}  {}\n", self.m.cpu.pc, self.m.symbols.format(self.m.cpu.pc));
                for (i, name) in FW_REGS.iter().enumerate() {
                    out.push_str(&format!("  {name:<4} {:08x}", self.m.cpu.x[i]));
                    if i % 4 == 3 {
                        out.push('\n');
                    }
                }
                Ok(out)
            }
            ["tasks"] => Ok(task_list(self.m)),
            _ => run::firmware(self, args),
        }
    }

    /// `clock` — the two clocks of this machine: the emulator's, which the firmware runs on, and the C64's.
    fn verb_clock(&mut self) -> String {
        let (now_ms, clocks) = (self.m.now_ms(), self.m.bus.now);
        let (insns, idle) = (self.m.cpu.insns, self.m.idle_insns);
        let c64 = self.machine().c64_core.clk;
        format!(
            "  emulator  {now_ms} ms  ({clocks} clocks at {} Hz)\n  firmware  {insns} instructions, {idle} skipped idle\n  c64       cycle {c64}\n",
            ue2_core::time::CLOCK_HZ,
        )
    }

    /// The verbs of §5 for `help`, after the library's own list.
    fn help(&self) -> String {
        concat!(
            "\nthe Ultimate side (S23):\n",
            "  fw [tasks]        the firmware's RISC-V: registers, or its FreeRTOS tasks\n",
            "  fw halt | go | step [n]     run control for the firmware (the C64 stands with it)\n",
            "  c64 [halt | go | step [n]]  run control for the C64, firmware running\n",
            "  g [addr] | x | until ADDR | z | step [n] | n | next [n] | ret\n",
            "                    the same, in TRX64's spelling: they mean the C64\n",
            "  status            where both CPUs stand\n",
            "  clock             the emulator's clock, the firmware's instructions, the C64's cycle\n",
            "  config [cat [item]]         the settings, as stored in flash\n",
            "  config flash                the raw config pages\n",
            "  config set CAT ITEM VALUE   change it in the running firmware\n",
            "  config write [PATH]         the config pages, or a .cfg inside the machine\n",
            "  config read PATH            hand a .cfg inside the machine to the firmware\n",
        )
        .to_string()
            + devices::HELP
    }
}

/// The firmware's RISC-V, as the monitor sees it.
impl CpuView for Host<'_> {
    /// 32: its devices sit at 0x10040000, its cartridge ROM at 0x03C00000 (docs/hw/00-memory-map.md).
    fn addr_bits(&self) -> u8 {
        32
    }

    /// The debugger's view of one data byte, without side effects — the same `peek8` the GDB stub reads with.
    fn read(&mut self, addr: u64) -> Option<u8> {
        Some(peek8(&self.m.bus, addr as u32))
    }

    /// DDR only, as the GDB stub writes: a write into an IO register has side effects the firmware did not ask
    /// for, and a monitor is not the place to spring them.
    fn write(&mut self, addr: u64, value: u8) -> Result<(), String> {
        match ram_index(addr as u32) {
            Some(i) => {
                self.m.bus.ram[i] = value;
                Ok(())
            }
            None => Err(format!("{addr:#010x} is not DDR; the firmware's IO is read-only here")),
        }
    }

    fn registers(&mut self) -> Vec<Reg> {
        let mut regs = vec![Reg { name: "pc", bits: 32, value: u64::from(self.m.cpu.pc) }];
        regs.extend(
            FW_REGS
                .iter()
                .enumerate()
                .map(|(i, &name)| Reg { name, bits: 32, value: u64::from(self.m.cpu.x[i]) }),
        );
        regs
    }

    fn set_register(&mut self, name: &str, value: u64) -> Result<(), String> {
        let value = u32::try_from(value).map_err(|_| format!("{value:#x} does not fit a 32-bit register"))?;
        if name.eq_ignore_ascii_case("pc") {
            self.m.cpu.pc = value & !3;
            return Ok(());
        }
        match FW_REGS.iter().position(|r| r.eq_ignore_ascii_case(name)) {
            Some(0) => Err("zero is hardwired to 0".into()),
            Some(i) => {
                self.m.cpu.x[i] = value;
                Ok(())
            }
            None => Err(format!("{name} is not a register of this device")),
        }
    }
}

/// Run one monitor line against this machine, and return the text as a reader sees it.
///
/// The library sees every line first, because it owns the modal state (`a`, a pending prompt). A line it does not
/// own comes back as `None`, and that is where our own verbs will go (S23 M2). The marked address spans
/// (TRX64 Spec 804) stay on the library's side of this call and are stripped here, so a later caller that wants
/// them can ask for them instead (S23 §7).
pub fn exec(
    m: &mut Machine,
    session: &mut MonitorSession,
    state: &mut State,
    line: &str,
) -> Result<String, String> {
    let mut host = Host::new(m, state).ok_or("the monitor needs a C64: start with --c64 trx64")?;
    let verb = line.split_whitespace().next().unwrap_or("").to_owned();
    let answer = match verbs::try_exec(session, &mut host, line) {
        // `help` is the port audit's list on both sides, so ours goes after theirs.
        Some(Ok(text)) if verb == "help" => Ok(text + &host.help()),
        Some(answer) => answer,
        None => {
            let args = words(line.trim_start().strip_prefix(&verb).unwrap_or(""));
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            host.ours(&verb, &args).unwrap_or(Err(format!("unknown monitor command '{verb}'")))
        }
    }?;
    Ok(addr_spans::plain(&answer))
}

/// Split a verb's arguments, honouring double quotes: store and item names have spaces ("C64 and Cartridge
/// Settings", "REU Size").
fn words(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in rest.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

// ── the VICE port's door into this host (S23 §8) ─────────────────────────────────────────────────────────────
//
// `crates/ue2emu/src/vice.rs` speaks someone else's protocol and must not learn our internals, so everything it
// needs is one function here, each taking the machine and our state and building a [`Host`] the same way `exec`
// does. Every one of them is about the C64: the RISC-V is not reachable over that protocol at all.

/// The C64's registers as the protocol numbers them (montypes.h:52-107): PC, A, X, Y, SP, FL, LIN, CYC.
pub fn c64_registers(m: &mut Machine, state: &mut State) -> Vec<(u8, u16)> {
    let Some(mut host) = Host::new(m, state) else { return Vec::new() };
    let machine = host.machine();
    let core = &machine.c64_core;
    let (a, x, y) = (core.reg_a, core.reg_x, core.reg_y);
    let (sp, status, pc) = (core.reg_sp, core.status(), core.reg_pc);
    let (line, cycle) = (machine.vic.raster_line, machine.vic.raster_cycle);
    vec![
        (0x03, pc),
        (0x00, u16::from(a)),
        (0x01, u16::from(x)),
        (0x02, u16::from(y)),
        (0x04, u16::from(sp)),
        (0x05, u16::from(status)),
        (0x35, line),
        (0x36, cycle),
    ]
}

/// One register, by the protocol's id. An id this CPU does not have is ignored, as VICE ignores it.
pub fn set_c64_register(m: &mut Machine, state: &mut State, reg: u8, value: u16) {
    let Some(mut host) = Host::new(m, state) else { return };
    let core = &mut host.machine().c64_core;
    match reg {
        0x03 => core.reg_pc = value,
        0x00 => core.reg_a = value as u8,
        0x01 => core.reg_x = value as u8,
        0x02 => core.reg_y = value as u8,
        0x04 => core.reg_sp = value as u8,
        0x05 => core.set_status_composite(value as u8),
        _ => {}
    }
}

/// `length` bytes from `start` through one bank lens, without side effects.
pub fn c64_read(m: &mut Machine, state: &mut State, start: u16, length: usize, lens: &str) -> Vec<u8> {
    let Some(mut host) = Host::new(m, state) else { return vec![0; length] };
    let machine = host.machine();
    (0..length).map(|i| machine.peek_lens(start.wrapping_add(i as u16), lens)).collect()
}

/// Bytes into the machine through one bank lens, the library's own write so both hosts agree what a lens means.
pub fn c64_write(m: &mut Machine, state: &mut State, start: u16, bytes: &[u8], lens: &str) {
    let Some(mut host) = Host::new(m, state) else { return };
    trx64_monitor::verbs::monitor_write(&mut host, start, bytes, lens);
}

/// Stop or release the C64 (S23 §3: the machine's own stop).
pub fn halt_c64(m: &mut Machine, state: &mut State, halted: bool) {
    if let Some(mut host) = Host::new(m, state) {
        let _ = run::set_halted(&mut host, halted);
    }
}

/// `n` instructions, `over` running a `JSR` to its return address.
pub fn step_c64(m: &mut Machine, state: &mut State, n: u64, over: bool) -> Result<(), String> {
    let mut host = Host::new(m, state).ok_or("this machine has no C64")?;
    run::step(&mut host, n, over).map(|_| ())
}

/// Run until the current subroutine returns: the address under the stack pointer is where it goes.
pub fn c64_until_return(m: &mut Machine, state: &mut State) -> Result<(), String> {
    let mut host = Host::new(m, state).ok_or("this machine has no C64")?;
    run::until_return(&mut host)
}

/// `KEYBOARD_FEED`, as VICE's own `kbdbuf_feed` does it: into the KERNAL's keyboard buffer at $0277, with the
/// count at $00C6 (10 bytes, kernal.s). Nothing else would work while the C64 is stopped.
pub fn feed_keyboard(m: &mut Machine, text: &str) {
    let mut state = State::default();
    let Some(mut host) = Host::new(m, &mut state) else { return };
    let machine = host.machine();
    for (i, byte) in text.bytes().take(10).enumerate() {
        machine.write_full(0x0277 + i as u16, byte);
    }
    machine.write_full(0x00C6, text.len().min(10) as u8);
}

/// Reset the C64 through the register the firmware uses for it (S23 §3), not around the firmware.
pub fn reset_c64(m: &mut Machine, state: &mut State, hard: bool) -> Result<(), String> {
    let mut host = Host::new(m, state).ok_or("this machine has no C64")?;
    run::reset(&mut host, hard)
}

/// The checkpoints the VICE port set, handed to the bridge so its runs stop on them (S23 §8).
pub fn set_c64_checkpoints(m: &mut Machine, ranges: &[(u16, u16, u8)]) {
    if let Some(backend) = trx64(m) {
        backend.set_checkpoints(ranges);
    }
}

/// The checkpoint the last run stopped at, once.
pub fn take_c64_checkpoint_hit(m: &mut Machine) -> Option<u16> {
    trx64(m).and_then(Trx64Backend::take_checkpoint_hit)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ue2_core::bus::SystemBus;
    use ue2_core::devices::c64;
    use ue2_core::machine::MachineConfig;
    use ue2_core::symbols::Symbols;

    use super::*;

    /// A machine with the C64 port installed and a TRX64 backend attached, as `runner::attach_trx64` builds it.
    fn machine() -> Machine {
        let cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        let mut bus = SystemBus::new();
        c64::install(&mut bus.io, &cfg);
        // `config` reads the flash, as it does on a real machine (volatile here).
        ue2_core::devices::flash::install(&mut bus.io, &cfg);
        let mut m = Machine::from_parts(cfg, bus, 0x30000, Symbols::empty());
        m.attach_c64(Box::new(Trx64Backend::new(Path::new("/nonexistent"))));
        m
    }

    /// S23 §9.2: the library's verbs answer against our host, on the machine's own C64.
    #[test]
    fn the_librarys_verbs_run_against_our_host() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let run = |m: &mut Machine, s: &mut MonitorSession, line: &str| {
            exec(m, s, &mut State::default(), line).expect(line)
        };

        let r = run(&mut m, &mut s, "r");
        assert!(r.contains("ADDR"), "the register panel: {r}");
        assert!(!r.contains(",c64,,"), "the marked spans are stripped for a reader: {r}");
        run(&mut m, &mut s, "wr 0400 de ad be ef");
        assert!(run(&mut m, &mut s, "m 0400 0403").contains("de ad be ef"), "the bytes just written");
        assert!(!run(&mut m, &mut s, "d 0400 0400").is_empty(), "a disassembly line");
        assert!(run(&mut m, &mut s, "help").contains("monitor"), "the help text the port audit walks");

        let err = exec(&mut m, &mut s, &mut st, "nonsense").unwrap_err();
        assert_eq!(err, "unknown monitor command 'nonsense'", "our dispatch takes what the library declines");
    }

    /// S23 §9.1, TRX64 Spec 864 item 13: the port audit. Every verb `help` lists answers on our host — drift from
    /// the daemon shows up here, on our side, rather than in someone's session.
    #[test]
    fn every_verb_in_help_answers_on_this_host() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let help = exec(&mut m, &mut s, &mut st, "help").expect("help");

        // Both help sections lay a verb out in its own column: an indented line, the verb, then two or more
        // spaces before the description. That is what tells a verb from the prose around it.
        let verbs: Vec<String> = help
            .lines()
            .filter(|line| line.starts_with(' '))
            .map(str::trim_start)
            .filter_map(|line| line.split_once("  "))
            .map(|(head, _)| head.split_whitespace().next().unwrap_or(""))
            .filter(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert!(verbs.len() > 30, "the audit must actually walk the list, not a handful: {verbs:?}");

        // Run control and the verbs that hand work to the firmware would drive the machine, and this machine has
        // no firmware to drive; they are covered by their own tests above.
        let drives_the_machine = ["g", "x", "z", "n", "next", "step", "ret", "return", "until", "dir", "reset"];
        let mut missing = Vec::new();
        let mut answered_now = Vec::new();
        for verb in &verbs {
            if drives_the_machine.contains(&verb.as_str()) {
                continue;
            }
            let answer = exec(&mut m, &mut s, &mut State::default(), verb);
            let text = match &answer {
                Ok(text) => text.clone(),
                Err(e) => e.clone(),
            };
            match text.starts_with("unknown monitor command") {
                true => missing.push(verb.as_str()),
                false if STILL_THE_DAEMONS.contains(&verb.as_str()) => answered_now.push(verb.as_str()),
                false => {}
            }
        }
        let unexpected: Vec<&str> = missing.iter().copied().filter(|v| !STILL_THE_DAEMONS.contains(v)).collect();
        assert!(unexpected.is_empty(), "in help but nothing answers, and not a known gap: {unexpected:?}");
        assert!(
            answered_now.is_empty(),
            "these answer now — take them out of STILL_THE_DAEMONS: {answered_now:?}"
        );
    }

    /// The verbs `monitor_help_text()` lists that `verbs::try_exec` does not dispatch at rev 638175d.
    ///
    /// The library carries the DAEMON's help text while Spec 864's extraction is under way, so a second host is
    /// offered a list it cannot honour. That is upstream's to close — either the verbs move into the library or
    /// the help text narrows to what it dispatches — and this array is how we watch it happen: a verb that starts
    /// answering fails the test until it is taken out, and a new one that does not answer fails it too.
    ///
    /// Most of these are a host's own business (`mount`, `eject`, `savecrt`, `drivepower`, `turbo`, `warp`,
    /// `window`, `frame`, `play`, `pause`), which is exactly why they cannot live in help as if they were the
    /// library's. We do not invent our own meanings for them: S23 §5 keeps `reu`, `uci` and `turbo` out on purpose.
    const STILL_THE_DAEMONS: [&str; 45] = [
        "bitmap", "bload", "bsave", "cadence", "chis", "diff", "drive", "drivepower", "eject", "frame", "goto",
        "identify", "load", "log", "map", "mark", "marks", "mkdir", "model", "mount", "pause", "play", "pwd",
        "rawframe", "recent", "rewind", "ringdump", "ringload", "rstep", "run", "save", "savecrt", "sf",
        "swapcrt", "swimlane", "taint", "trace", "tracedb", "traceindex", "tracering", "traprules", "turbo",
        "unmark", "warp", "window",
    ];

    /// S23 §4: `device fw` is the firmware's core — 32-bit, registers and DDR, no flags, no disassembler.
    #[test]
    fn the_firmware_core_is_a_device_of_its_own() {
        let mut m = machine();
        m.cpu.x[10] = 0x1234_5678;
        m.bus.ram[0x100] = 0x42;
        let mut st = State::default();
        let mut host = Host::new(&mut m, &mut st).expect("a host");

        assert_eq!(host.devices(), vec![Device::C64, Device::Host(FW)], "both drives are off at power-on (S27)");
        assert!(host.cpu(Device::C64).is_none(), "the 6502s are the machine's own");
        let view = host.cpu(Device::Host(FW)).expect("the firmware core");
        assert_eq!(view.addr_bits(), 32);
        assert!(view.flags().is_none(), "the RISC-V has no flag register");
        assert!(view.disasm(0x30000).is_none(), "and no disassembler here");
        assert!(!view.supports_debug_gates(), "the watch tables are a 6502 shape");
        assert_eq!(view.read(0x100), Some(0x42), "DDR through peek8");
        assert_eq!(view.registers().iter().find(|r| r.name == "a0").map(|r| r.value), Some(0x1234_5678));

        view.write(0x100, 0x43).expect("DDR is writable");
        assert_eq!(view.read(0x100), Some(0x43));
        assert!(view.write(0x1004_0000, 0).is_err(), "the firmware's IO is not");

        view.set_register("a0", 7).expect("a0");
        assert_eq!(view.registers().iter().find(|r| r.name == "a0").map(|r| r.value), Some(7));
        assert!(view.set_register("zero", 1).is_err(), "x0 is hardwired");
        assert!(view.set_register("nope", 1).is_err());
    }

    /// S23 §5: the verbs the library declines are ours, and `help` carries both lists.
    #[test]
    fn our_own_verbs_take_what_the_library_declines() {
        let mut m = machine();
        m.cpu.x[2] = 0x8000_1000;
        let mut s = MonitorSession::new();
        let mut st = State::default();

        let fw = exec(&mut m, &mut s, &mut st, "fw").expect("fw");
        assert!(fw.contains("sp   80001000"), "the firmware's registers: {fw}");
        assert!(exec(&mut m, &mut s, &mut st, "fw tasks").is_ok(), "the task list walks an unbooted kernel too");
        assert_eq!(
            exec(&mut m, &mut s, &mut st, "fw nonsense").unwrap_err(),
            "fw: usage: fw [tasks | halt | go | step [n]]"
        );

        let clock = exec(&mut m, &mut s, &mut st, "clock").expect("clock");
        assert!(clock.contains("emulator") && clock.contains("firmware") && clock.contains("c64"), "{clock}");

        let help = exec(&mut m, &mut s, &mut st, "help").expect("help");
        assert!(help.contains("the Ultimate side (S23)") && help.contains("clock"), "both lists: {help}");
    }

    /// S23 §6: the settings as stored, narrowed by name, and quoted names survive the split.
    #[test]
    fn config_reads_the_stored_settings() {
        assert_eq!(words(r#" "C64 and Cartridge Settings" "REU Size" "#), ["C64 and Cartridge Settings", "REU Size"]);

        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        // A machine built from parts has no firmware image, so no settings tables: the answer says so.
        let out = exec(&mut m, &mut s, &mut st, "config").expect("config");
        assert_eq!(out, "this firmware has no settings tables\n");
        assert!(exec(&mut m, &mut s, &mut st, "config flash").expect("config flash").contains("config pages in use"));
    }

    /// S23 §6: what `config set` and `config write` refuse, and why — the rule is `set` before `write`.
    #[test]
    fn config_set_and_write_say_what_they_will_not_do() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let line = |m: &mut Machine, st: &mut State, l: &str| exec(m, &mut MonitorSession::new(), st, l).unwrap_err();

        assert!(exec(&mut m, &mut s, &mut st, "config set a b").unwrap_err().contains("usage"));
        assert_eq!(
            line(&mut m, &mut st, "config set a b c"),
            "config set: [a] is not a store UE2 can set (S21 §3); \"b\" skipped",
            "the value is checked against the firmware's own definition before the firmware sees it"
        );
        assert!(
            line(&mut m, &mut st, "config write").starts_with("config write: nothing was set in this session"),
            "a page the firmware does not know about is overwritten from its own copy"
        );
        // A path on any medium is the firmware's to write; this machine has no settings to put there.
        assert!(line(&mut m, &mut st, "config write /Usb0/mine.cfg").contains("no settings tables"));
    }

    /// The firmware registers the block sits behind (command_if_pkg.vhd:7-25), as `CommandInterface`'s constructor
    /// writes them: the window at $DF18 and the block on.
    fn enable_uci(m: &mut Machine) {
        let block = trx64(m).expect("a backend").trx64().uci_mut().expect("the u64 profile carries the block");
        block.fw_write(0x0, 0x47);
        block.fw_write(0x1, 1);
    }

    /// S23 §6: `config read` hands the path to the running firmware through the command interface.
    #[test]
    fn config_read_goes_through_the_command_interface() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();

        // A firmware that never enabled the block: the answer says which setting turns it on.
        let err = exec(&mut m, &mut s, &mut st, "config read /flash/x.cfg").unwrap_err();
        assert!(err.starts_with("the firmware's command interface is off"), "{err}");
        assert!(exec(&mut m, &mut s, &mut st, "config read").unwrap_err().contains("usage"));
        assert!(exec(&mut m, &mut s, &mut st, "config read a b").unwrap_err().contains("usage"));

        // With the block on, the command is pushed and the firmware is run for the answer. This machine has no
        // firmware image, so the run faults at once — which is the answer, not a hang.
        enable_uci(&mut m);
        let err = exec(&mut m, &mut s, &mut st, "config read /flash/x.cfg").unwrap_err();
        assert!(err.starts_with("the firmware stopped with a command in flight"), "{err}");

        let wanted = b"\x04\x50/flash/x.cfg\0";
        let block = trx64(&mut m).expect("a backend").trx64().uci().expect("the block");
        let pushed: Vec<u8> = (0..wanted.len() as u16).map(|i| block.fw_read(0x800 + i)).collect();
        assert_eq!(pushed, wanted, "target 4, CTRL_CMD_LOAD_CONFIG, the path as a C string");
        let s = trx64(&mut m).unwrap().trx64().uci_status().unwrap();
        assert_eq!(s.command_length, wanted.len() as u16);
        assert!(s.new_command, "and the block is telling the firmware about it");
    }

    /// S23 §5: the Ultimate's own hardware, decoded from the registers the firmware reads.
    #[test]
    fn the_device_verbs_read_the_firmwares_registers() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let run = |m: &mut Machine, st: &mut State, l: &str| {
            exec(m, &mut MonitorSession::new(), st, l).unwrap_or_else(|e| panic!("{l}: {e}"))
        };

        let itu = run(&mut m, &mut st, "itu");
        assert!(itu.contains("capabilities  0x34000226"), "the capability word this machine advertises: {itu}");
        assert!(itu.contains("low  enabled"), "the interrupt controller as the ISR sees it: {itu}");

        let cart = run(&mut m, &mut st, "cart");
        assert!(cart.contains("cartridge     type 0x00 variant 0  none"), "{cart}");
        assert!(cart.contains("reu           off, 16 MB"), "the reset value of C64_REU_SIZE is 7: {cart}");
        assert!(cart.contains("cart rom      0x03c00000"), "where this firmware keeps it: {cart}");
        assert!(cart.contains("command intf  off"), "the firmware never enabled the block: {cart}");

        assert!(run(&mut m, &mut st, "flash").contains("volatile"), "the test machine's flash has no image");
        // The test machine installs neither, and each says which one is missing.
        assert_eq!(exec(&mut m, &mut s, &mut st, "sd").unwrap_err(), "this machine has no SD slot");
        assert_eq!(exec(&mut m, &mut s, &mut st, "usb").unwrap_err(), "this machine has no USB host");
        assert_eq!(exec(&mut m, &mut s, &mut st, "cart x").unwrap_err(), "cart: takes no arguments");

        // `dir` is the firmware's own listing, so on a machine with no firmware it says the door is shut.
        assert!(exec(&mut m, &mut s, &mut st, "dir a b").unwrap_err().contains("usage"));
        assert!(
            exec(&mut m, &mut s, &mut st, "dir /flash").unwrap_err().starts_with("the firmware's command interface"),
            "the listing goes through the command interface"
        );

        let audio = run(&mut m, &mut st, "audio");
        assert!(audio.contains("socket 1      empty"), "no ARMSID in this machine: {audio}");
        // The firmware programs one 32-byte window per mirror; a run that reaches one chip is one line.
        assert!(audio.contains("window        $d400-$d7ff -> chip 0"), "the C64's own SID: {audio}");
        assert_eq!(exec(&mut m, &mut s, &mut st, "net").unwrap_err(), "this machine has no Ethernet MAC");

        let help = exec(&mut m, &mut s, &mut st, "help").expect("help");
        for verb in ["itu", "cart", "joy", "flash", "sd", "usb", "net", "audio"] {
            assert!(help.contains(&format!("\n  {verb} ")), "{verb} is in help: {help}");
        }
    }

    /// S23 M3: both CPUs have run control, and they are not symmetric.
    #[test]
    fn run_control_holds_the_firmware_and_the_c64() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let run = |m: &mut Machine, st: &mut State, l: &str| {
            exec(m, &mut MonitorSession::new(), st, l).unwrap_or_else(|e| panic!("{l}: {e}"))
        };

        // The C64's halt is the machine's own stop, so it reads back out of the register the firmware uses.
        assert!(run(&mut m, &mut st, "c64").contains("running"));
        let halted = run(&mut m, &mut st, "c64 halt");
        assert!(halted.contains("c64  stopped"), "read back from C64_STOP, not from what we wrote: {halted}");
        assert_eq!(super::super::gdb::peek8(&m.bus, 0x1004_0001) & 1, 1, "the register itself carries the request");
        assert!(run(&mut m, &mut st, "c64 go").contains("c64  running"));

        // Holding the firmware holds everything, and the answer says so.
        assert!(run(&mut m, &mut st, "fw").contains("pc  "), "`fw` alone is still the register panel");
        let held = run(&mut m, &mut st, "fw halt");
        assert!(held.contains("fw   held") && held.contains("the C64 stands with it"), "{held}");
        assert!(run::firmware_held(&st), "and the run loop is told");
        assert!(run(&mut m, &mut st, "status").contains("fw   held"), "status carries both");
        assert!(run(&mut m, &mut st, "fw go").contains("fw   running"));
        assert!(!run::firmware_held(&st));

        assert!(exec(&mut m, &mut s, &mut st, "c64 step 0").unwrap_err().contains("not a step"));
        assert!(exec(&mut m, &mut s, &mut st, "c64 nonsense").unwrap_err().contains("usage"));
        assert!(exec(&mut m, &mut s, &mut st, "fw nonsense").unwrap_err().contains("usage"));
    }

    /// S23 M3: the run-control verbs in TRX64's spelling answer on the C64 while the library does not carry them.
    #[test]
    fn the_trx64_spelled_run_verbs_answer_on_the_c64() {
        let mut m = machine();
        let mut s = MonitorSession::new();
        let mut st = State::default();
        let run = |m: &mut Machine, st: &mut State, l: &str| {
            exec(m, &mut MonitorSession::new(), st, l).unwrap_or_else(|e| panic!("{l}: {e}"))
        };

        // `bk` is the library's own and still answers; the run verbs are ours until they land upstream.
        assert!(exec(&mut m, &mut s, &mut st, "bk").is_ok(), "the library owns the breakpoint verb");

        assert!(run(&mut m, &mut st, "g").contains("c64  running"));
        assert!(run(&mut m, &mut st, "g c000").contains("c64  running"));
        assert_eq!(
            trx64(&mut m).unwrap().trx64().c64_core.reg_pc,
            0xc000,
            "`g addr` sets the PC before it lets go"
        );
        assert!(run(&mut m, &mut st, "z").contains("instruction(s)"));
        assert!(run(&mut m, &mut st, "step 3").contains("3 instruction(s)"));
        assert!(run(&mut m, &mut st, "n").contains("instruction(s)"));
        assert!(run(&mut m, &mut st, "ret").contains("c64  "));

        assert!(exec(&mut m, &mut s, &mut st, "g zz").unwrap_err().contains("not an address"));
        assert!(exec(&mut m, &mut s, &mut st, "until").unwrap_err().contains("usage"));

        let help = exec(&mut m, &mut s, &mut st, "help").expect("help");
        assert!(help.contains("in TRX64's spelling"), "and they are in help: {help}");
    }

    /// The host trait's own run control, which the library's verbs call once TRX64 has moved them.
    #[test]
    fn the_host_trait_can_halt_and_step_the_c64() {
        let mut m = machine();
        let mut st = State::default();
        let mut host = Host::new(&mut m, &mut st).expect("a host");

        host.set_halted(true).expect("halt");
        assert!(run::halted(&mut host), "through the same register as the firmware's own stop");
        host.set_halted(false).expect("go");
        assert!(!run::halted(&mut host));

        // A C64 that is driven by the firmware cannot finish an open-ended resume itself.
        match host.resume(trx64_monitor::RunUntil::Forever).expect("resume") {
            trx64_monitor::Resumption::Resumed { .. } => {}
            other => panic!("the firmware drives this C64: {other:?}"),
        }
        let stop = host.step(2, false).expect("step");
        assert_eq!(stop.reason, "step");
        assert_eq!(stop.steps.len(), 2, "one entry per retired instruction, for the library's flow tracker");
    }
}
