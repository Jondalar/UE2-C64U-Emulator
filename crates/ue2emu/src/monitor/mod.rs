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
//! Not here yet: run control (S23 M3, the library's defaults refuse it in one sentence).

mod uci;

use c64_bridge::Trx64Backend;
use trx64_monitor::host::{CpuView, Device, MonitorHost, Reg};
use trx64_monitor::{addr_spans, verbs, MonitorSession};
use ue2_core::devices::c64::C64Port;
use ue2_core::devices::flash::SpiFlash;
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
}

impl<'a> Host<'a> {
    /// A host, if this machine has a C64 (`--c64 trx64`). Without one there is no monitor: every verb of the
    /// library is about `trx64_core::Machine`.
    pub fn new(m: &'a mut Machine) -> Option<Host<'a>> {
        trx64(m).is_some().then_some(Host { m })
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

    fn devices(&self) -> Vec<Device> {
        vec![Device::C64, Device::Drive8, Device::Host(FW)]
    }
}

/// Our own verbs (S23 §5): the Ultimate side, which the library has never seen.
impl Host<'_> {
    /// A line the library declined. `None` means nobody owns it.
    fn ours(&mut self, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
        match verb {
            "fw" => Some(self.verb_fw(args)),
            "clock" => Some(Ok(self.verb_clock())),
            "config" => Some(self.verb_config(args)),
            _ => None,
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
            _ => Err("fw: usage: fw [tasks]".into()),
        }
    }

    /// `config` — the Ultimate's settings, as the menu shows them (S23 §6). Reading only in this cut: `set`,
    /// `write` and `read` need the firmware's own .cfg path and come next.
    fn verb_config(&mut self, args: &[&str]) -> Result<String, String> {
        let flash = self.m.bus.io.get::<SpiFlash>().ok_or("this machine has no flash")?;
        if args == ["flash"] {
            let pages = flash.config_pages();
            let mut out = format!("  {} config pages in use\n", pages.len());
            for page in pages {
                let n = flash.config_page(page).map_or(0, |r| r.len());
                // The id is four ASCII characters, most significant first ("GEN.", "C64 ", `register_store`).
                let name = String::from_utf8_lossy(&page.to_be_bytes().map(|b| if b.is_ascii_graphic() { b } else { b'.' })).into_owned();
                out.push_str(&format!("  {page:08x}  {name}  {n} records\n"));
            }
            return Ok(out);
        }
        if let ["read", rest @ ..] = args {
            return match rest {
                [path] => self.config_read(path),
                _ => Err("config read: usage: config read <path in the emulated machine>".into()),
            };
        }
        if matches!(args.first(), Some(&"set" | &"write")) {
            return Err(format!("config {}: not in this build yet (S23 §6)", args[0]));
        }
        let tables = settings::tables(&self.m.bus.ram, &self.m.segments);
        let stores = settings::stores(&tables);
        Ok(settings::stored(&stores, flash, args.first().copied(), args.get(1).copied()))
    }

    /// `config read <path>` — the menu's "Load Settings", from the monitor: the firmware opens that `.cfg`, applies
    /// the items it knows and effectuates every store they touched (`ControlTarget::load_config`). The path is the
    /// emulated machine's — `/flash/...`, `/Usb0/...`, `/Temp/...`, the SD card — never the host's.
    fn config_read(&mut self, path: &str) -> Result<String, String> {
        if !path.is_ascii() {
            return Err("config read: the firmware's paths are ASCII".into());
        }
        let mut message = vec![uci::TARGET_CONTROL, uci::CTRL_CMD_LOAD_CONFIG];
        message.extend(path.as_bytes());
        // No length field: the command's remainder is a C string, so it needs its terminator
        // (control_target.cc:565-568).
        message.push(0);
        let reply = uci::command(self.m, &message)?;
        // The reply data is the parse log — empty on full success, else the lines the firmware could not apply.
        let log: String = reply.text().lines().map(|l| format!("  {l}\n")).collect();
        if !reply.ok() {
            return Err(format!("config read {path}: {}\n{log}", reply.status).trim_end().to_string());
        }
        Ok(format!("  {path}  {}\n{log}", reply.status))
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
    fn help(&self) -> &'static str {
        "\nthe Ultimate side (S23):\n  fw [tasks]        the firmware's RISC-V: registers, or its FreeRTOS tasks\n  clock             the emulator's clock, the firmware's instructions, the C64's cycle\n  config [cat [item]]  the settings, as stored in flash; `config flash` the raw pages\n  config read PATH  hand a .cfg in the emulated machine to the running firmware\n"
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
pub fn exec(m: &mut Machine, session: &mut MonitorSession, line: &str) -> Result<String, String> {
    let mut host = Host::new(m).ok_or("the monitor needs a C64: start with --c64 trx64")?;
    let verb = line.split_whitespace().next().unwrap_or("").to_owned();
    let answer = match verbs::try_exec(session, &mut host, line) {
        // `help` is the port audit's list on both sides, so ours goes after theirs.
        Some(Ok(text)) if verb == "help" => Ok(text + host.help()),
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
        let run = |m: &mut Machine, s: &mut MonitorSession, line: &str| exec(m, s, line).expect(line);

        let r = run(&mut m, &mut s, "r");
        assert!(r.contains("ADDR"), "the register panel: {r}");
        assert!(!r.contains(",c64,,"), "the marked spans are stripped for a reader: {r}");
        run(&mut m, &mut s, "wr 0400 de ad be ef");
        assert!(run(&mut m, &mut s, "m 0400 0403").contains("de ad be ef"), "the bytes just written");
        assert!(!run(&mut m, &mut s, "d 0400 0400").is_empty(), "a disassembly line");
        assert!(run(&mut m, &mut s, "help").contains("monitor"), "the help text the port audit walks");

        let err = exec(&mut m, &mut s, "nonsense").unwrap_err();
        assert_eq!(err, "unknown monitor command 'nonsense'", "our dispatch takes what the library declines");
    }

    /// S23 §4: `device fw` is the firmware's core — 32-bit, registers and DDR, no flags, no disassembler.
    #[test]
    fn the_firmware_core_is_a_device_of_its_own() {
        let mut m = machine();
        m.cpu.x[10] = 0x1234_5678;
        m.bus.ram[0x100] = 0x42;
        let mut host = Host::new(&mut m).expect("a host");

        assert_eq!(host.devices().len(), 3, "c64, drive8, fw");
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

        let fw = exec(&mut m, &mut s, "fw").expect("fw");
        assert!(fw.contains("sp   80001000"), "the firmware's registers: {fw}");
        assert!(exec(&mut m, &mut s, "fw tasks").is_ok(), "the task list walks an unbooted kernel too");
        assert_eq!(exec(&mut m, &mut s, "fw nonsense").unwrap_err(), "fw: usage: fw [tasks]");

        let clock = exec(&mut m, &mut s, "clock").expect("clock");
        assert!(clock.contains("emulator") && clock.contains("firmware") && clock.contains("c64"), "{clock}");

        let help = exec(&mut m, &mut s, "help").expect("help");
        assert!(help.contains("the Ultimate side (S23)") && help.contains("clock"), "both lists: {help}");
    }

    /// S23 §6: the settings as stored, narrowed by name, and quoted names survive the split.
    #[test]
    fn config_reads_the_stored_settings() {
        assert_eq!(words(r#" "C64 and Cartridge Settings" "REU Size" "#), ["C64 and Cartridge Settings", "REU Size"]);

        let mut m = machine();
        let mut s = MonitorSession::new();
        // A machine built from parts has no firmware image, so no settings tables: the answer says so.
        let out = exec(&mut m, &mut s, "config").expect("config");
        assert_eq!(out, "this firmware has no settings tables\n");
        assert!(exec(&mut m, &mut s, "config flash").expect("config flash").contains("config pages in use"));
        assert_eq!(
            exec(&mut m, &mut s, "config set a b c").unwrap_err(),
            "config set: not in this build yet (S23 §6)"
        );
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

        // A firmware that never enabled the block: the answer says which setting turns it on.
        let err = exec(&mut m, &mut s, "config read /flash/x.cfg").unwrap_err();
        assert!(err.starts_with("the firmware's command interface is off"), "{err}");
        assert!(exec(&mut m, &mut s, "config read").unwrap_err().contains("usage"));
        assert!(exec(&mut m, &mut s, "config read a b").unwrap_err().contains("usage"));

        // With the block on, the command is pushed and the firmware is run for the answer. This machine has no
        // firmware image, so the run faults at once — which is the answer, not a hang.
        enable_uci(&mut m);
        let err = exec(&mut m, &mut s, "config read /flash/x.cfg").unwrap_err();
        assert!(err.starts_with("the firmware stopped with a command in flight"), "{err}");

        let wanted = b"\x04\x50/flash/x.cfg\0";
        let block = trx64(&mut m).expect("a backend").trx64().uci().expect("the block");
        let pushed: Vec<u8> = (0..wanted.len() as u16).map(|i| block.fw_read(0x800 + i)).collect();
        assert_eq!(pushed, wanted, "target 4, CTRL_CMD_LOAD_CONFIG, the path as a C string");
        let s = trx64(&mut m).unwrap().trx64().uci_status().unwrap();
        assert_eq!(s.command_length, wanted.len() as u16);
        assert!(s.new_command, "and the block is telling the firmware about it");
    }

    /// Run control is M3: until then the library's own sentence is the answer, not a panic.
    #[test]
    fn run_control_is_refused_in_one_sentence() {
        let mut m = machine();
        let mut host = Host::new(&mut m).expect("a host");
        let err = host.resume(trx64_monitor::RunUntil::Forever).unwrap_err();
        assert!(err.contains("not available in this host"), "{err}");
        assert!(host.step(1, false).is_err());
    }
}
