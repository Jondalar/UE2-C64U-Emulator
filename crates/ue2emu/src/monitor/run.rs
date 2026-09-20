//! Run control for both CPUs of this machine (S23 M3).
//!
//! A debug monitor for an Ultimate has to stop two things: the runtime the Ultimate's own application runs on —
//! the firmware's RISC-V — and the C64 behind it. They are not symmetric, and the asymmetry is the machine's, not
//! a choice made here:
//!
//! - **Stopping the firmware stops everything.** The C64 only advances because `run_cpu` in the bridge drives it,
//!   so a held RISC-V holds the C64 with it. On hardware the C64 has its own crystal and would run on; here it
//!   does not. That makes a firmware breakpoint a consistent snapshot of both, and it makes timing bugs at the
//!   seam between the two clocks (issue #2 was one) invisible while stopped. They are only visible running.
//! - **Stopping the C64 leaves the firmware running**, which is what a monitor wants when the question is about
//!   the C64.
//!
//! Stopping the C64 **is** the stop the machine already has. `Hold::Cpu` in TRX64 is documented as "DMA / freeze /
//! C64_STOP" — one mechanism, and the firmware reaches it by writing `C64_STOP`. So does the monitor, through the
//! same register and the same side effects (`C64Port`, c64.cc). Two consequences follow and are deliberate: the
//! last writer wins, so the firmware releasing its DMA stop releases ours too, and the halt is visible where the
//! firmware looks — which is why every answer here reports the state it *reads*, never the state it set.

use trx64_monitor::session::{FlowKind, StepClass};
use trx64_monitor::{MonitorHost, Resumption, RunUntil, StopInfo};
use ue2_core::machine::RunExit;
use ue2_core::symbols::Symbols;

use super::Host;
use crate::gdb::peek8;

/// `C64_CARTREGS_BASE + 1`, the register the firmware stops the C64 with (c64.h:56).
const C64_STOP: u32 = 0x1004_0001;
/// `C64_DO_STOP` and `C64_HAS_STOPPED` (c64.h:85-86).
const DO_STOP: u8 = 0x01;
const HAS_STOPPED: u8 = 0x02;

/// Instructions one `fw step` retires at most while looking for the next one that counts.
const STEP_BUDGET: u64 = 1;
/// How far a bounded run chases a PC before it gives up and says so. A monitor must answer, not hang.
const UNTIL_INSNS: u64 = 5_000_000;

/// What run control keeps between lines.
#[derive(Default)]
pub struct State {
    /// The firmware is held: the run loop stops advancing the machine until this clears.
    pub firmware_held: bool,
    /// Why it stopped, for `status` and for the line that reports it.
    pub firmware_reason: Option<String>,
}

/// The run-control verbs in TRX64's own spelling, on the C64 — which is what they mean there: `RunUntil::Pc` is
/// 16 bits, `StopInfo::pc` is 16 bits.
///
/// They live here because `trx64-monitor` does not carry them yet: the library owns `bk`, but nothing in it calls
/// `MonitorHost::resume` or `::step`, so `g` and the stepping verbs are still in TRX64's daemon. Our dispatch only
/// ever sees a line the library declined, so when they land upstream the library answers first and these fall away
/// on their own — no collision, and nobody has to wait for them in the meantime.
pub(super) fn alias(host: &mut Host, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
    let count = |args: &[&str]| -> Result<u64, String> {
        match args {
            [] => Ok(1),
            [n] => n.parse().map_err(|_| format!("{verb}: {n} is not a number")),
            _ => Err(format!("{verb}: usage: {verb} [count]")),
        }
    };
    match verb {
        "g" | "x" => Some(match args {
            [] => c64(host, &["go"]),
            [addr] => match parse_addr(addr) {
                Some(pc) => {
                    host.machine().c64_core.reg_pc = pc;
                    c64(host, &["go"])
                }
                None => Err(format!("{verb}: {addr} is not an address")),
            },
            _ => Err(format!("{verb}: usage: {verb} [addr]")),
        }),
        "z" | "step" => Some(count(args).and_then(|n| c64_step(host, n))),
        "n" | "next" => Some(count(args).and_then(|n| step_over(host, n))),
        "ret" | "return" => Some(until_return(host).map(|()| c64_state(host))),
        "until" => Some(match args {
            [addr] => match parse_addr(addr) {
                Some(pc) => run_until_pc(host, pc),
                None => Err(format!("until: {addr} is not an address")),
            },
            _ => Err("until: usage: until <addr>".into()),
        }),
        _ => None,
    }
}

/// A hex address, with or without the `$` the monitor's own verbs take.
fn parse_addr(text: &str) -> Option<u16> {
    u32::from_str_radix(text.trim_start_matches(['$', '+']), 16).ok().map(|v| v as u16)
}

/// `n`/`next`: like a step, but a `JSR` runs to its return address.
fn step_over(host: &mut Host, n: u64) -> Result<String, String> {
    let stop = step(host, n, true)?;
    Ok(format!("{}  {} instruction(s)\n", c64_state(host), stop.steps.len()))
}

/// `until <addr>`: run the C64 until its PC is there.
fn run_until_pc(host: &mut Host, pc: u16) -> Result<String, String> {
    let stop = match resume(host, RunUntil::Pc(pc))? {
        Resumption::Stopped(stop) => stop,
        Resumption::Resumed { .. } => return Err("until: the C64 did not stop".into()),
    };
    Ok(format!("{}  {}\n", c64_state(host), stop.reason))
}

/// `c64 [halt|go|step [n]]` — the C64's own run control.
pub(super) fn c64(host: &mut Host, args: &[&str]) -> Result<String, String> {
    match args {
        [] => Ok(c64_state(host)),
        ["halt"] => {
            set_stop(host, true);
            Ok(c64_state(host))
        }
        ["go"] => {
            set_stop(host, false);
            Ok(c64_state(host))
        }
        ["step"] => c64_step(host, 1),
        ["step", n] => c64_step(host, n.parse().map_err(|_| format!("c64 step: {n} is not a number"))?),
        _ => Err("c64: usage: c64 [halt | go | step [n]]".into()),
    }
}

/// One line for what the C64 is doing, read back from the register — never from what we just wrote.
fn c64_state(host: &mut Host) -> String {
    let stop = peek8(&host.m.bus, C64_STOP);
    let pc = host.machine().c64_core.reg_pc;
    let cycle = host.machine().c64_core.clk;
    format!(
        "  c64  {} at ${pc:04x}, cycle {cycle}{}\n",
        if stop & HAS_STOPPED != 0 { "stopped" } else { "running" },
        if stop & DO_STOP != 0 { "" } else { " (nobody is asking it to stop)" },
    )
}

/// Write `C64_STOP` as the firmware writes it, with the side effects that belong to it.
fn set_stop(host: &mut Host, stop: bool) {
    host.m.bus.poke_io8(C64_STOP, u8::from(stop) * DO_STOP);
}

/// `c64 step [n]` — the C64 alone, `n` instructions, whatever the firmware is doing.
///
/// Stepping releases the stop for the length of the step and puts it back, so a step from a halted C64 leaves it
/// halted. The firmware's clock does not move: this is the C64's own core, run out of band.
fn c64_step(host: &mut Host, n: u64) -> Result<String, String> {
    if n == 0 {
        return Err("c64 step: 0 instructions is not a step".into());
    }
    let held = peek8(&host.m.bus, C64_STOP) & HAS_STOPPED != 0;
    set_stop(host, false);
    let before = host.machine().c64_core.clk;
    host.machine().run_for_full_capped(n * 64, n, &mut trx64_core::NullSink, |_, _, _, _, _, _, _| {});
    let cycles = host.machine().c64_core.clk - before;
    if held {
        set_stop(host, true);
    }
    Ok(format!("{}  {n} instruction(s), {cycles} cycles\n", c64_state(host)))
}

/// `fw [halt|go|step [n]]` — the firmware's RISC-V, on top of what the GDB stub already does.
pub(super) fn firmware(host: &mut Host, args: &[&str]) -> Result<String, String> {
    match args {
        ["halt"] => {
            host.state.run.firmware_held = true;
            host.state.run.firmware_reason = Some("the monitor asked".into());
            Ok(fw_state(host))
        }
        ["go"] => {
            host.state.run.firmware_held = false;
            host.state.run.firmware_reason = None;
            Ok(fw_state(host))
        }
        ["step"] => fw_step(host, 1),
        ["step", n] => fw_step(host, n.parse().map_err(|_| format!("fw step: {n} is not a number"))?),
        _ => Err("fw: usage: fw [tasks | halt | go | step [n]]".into()),
    }
}

/// Where the firmware stands, with the symbol the GDB stub would print.
fn fw_state(host: &mut Host) -> String {
    let pc = host.m.cpu.pc;
    let held = host.state.run.firmware_held;
    let why = host.state.run.firmware_reason.clone().unwrap_or_default();
    format!(
        "  fw   {} at {pc:08x}  {}{}\n{}",
        if held { "held" } else { "running" },
        host.m.symbols.format(pc),
        if why.is_empty() { String::new() } else { format!("  ({why})") },
        // The C64 only moves while the firmware drives it, so say so rather than let someone wonder.
        if held { "  the C64 stands with it: it only advances while the firmware runs\n" } else { "" },
    )
}

/// `fw step [n]` — `n` firmware instructions, out of band. The C64 advances with them, as it does in any run.
fn fw_step(host: &mut Host, n: u64) -> Result<String, String> {
    if n == 0 {
        return Err("fw step: 0 instructions is not a step".into());
    }
    let before = host.m.cpu.insns;
    for _ in 0..n {
        match host.m.run(STEP_BUDGET) {
            RunExit::Budget => {}
            RunExit::Breakpoint(pc) => {
                host.state.run.firmware_held = true;
                host.state.run.firmware_reason = Some(format!("breakpoint at {}", symbol(&host.m.symbols, pc)));
                break;
            }
            RunExit::Halted(why) => {
                host.state.run.firmware_held = true;
                host.state.run.firmware_reason = Some(why.clone());
                return Err(format!("fw step: the firmware stopped: {why}\n{}", fw_state(host)));
            }
        }
    }
    let retired = host.m.cpu.insns - before;
    Ok(format!("{}  {retired} instruction(s)\n", fw_state(host)))
}

fn symbol(symbols: &Symbols, pc: u32) -> String {
    format!("{pc:08x} {}", symbols.format(pc))
}

/// Both CPUs in one answer, for `status` and for anyone who lost track.
pub(super) fn status(host: &mut Host) -> String {
    format!("{}{}", fw_state(host), c64_state(host))
}

/// The run loop asks this before every slice: while the firmware is held, the machine does not advance and only
/// commands are served (`runner::run`).
pub fn firmware_held(state: &super::State) -> bool {
    state.run.firmware_held
}

/// A stop the run loop saw by itself — a breakpoint the GDB stub did not take — becomes the monitor's held state,
/// so the next `fw` or `status` says why.
pub fn note_stop(state: &mut super::State, reason: String) {
    state.run.firmware_held = true;
    state.run.firmware_reason = Some(reason);
}

/// The C64's stop as the host trait sees it (S23 §3). The library's run-control verbs call these once TRX64 has
/// moved them into `trx64-monitor`; until then our own `c64` verb is the way in, and both go through the same
/// register.
pub(super) fn set_halted(host: &mut Host, halted: bool) -> Result<(), String> {
    set_stop(host, halted);
    Ok(())
}

/// Whether the C64 is stopped right now, read back from the register.
pub(super) fn halted(host: &mut Host) -> bool {
    peek8(&host.m.bus, C64_STOP) & HAS_STOPPED != 0
}

/// `resume` for the host trait.
///
/// `Forever` is the one case that is not ours to finish: the C64 runs because the firmware drives it, so we let
/// go of the stop and answer `Resumed`. The stop, when it comes, arrives asynchronously — which is why the
/// library's own `on_stop` exists. The bounded forms we can finish here, out of band, because a bound is a
/// question about the C64 alone.
pub(super) fn resume(host: &mut Host, until: RunUntil) -> Result<Resumption, String> {
    match until {
        RunUntil::Forever => {
            set_stop(host, false);
            Ok(Resumption::Resumed { until })
        }
        RunUntil::Cycles(n) => {
            let (_, cycle) = pc_cycle(host);
            run_c64(host, n, u64::MAX);
            let (pc, now) = pc_cycle(host);
            Ok(Resumption::Stopped(StopInfo::at(pc, now, format!("{} cycles", now - cycle))))
        }
        RunUntil::Pc(target) => {
            let held = halted(host);
            set_stop(host, false);
            let mut reason = "budget";
            for _ in 0..UNTIL_INSNS {
                if pc_cycle(host).0 == target {
                    reason = "pc";
                    break;
                }
                run_c64(host, 64, 1);
            }
            if held {
                set_stop(host, true);
            }
            let (pc, cycle) = pc_cycle(host);
            Ok(Resumption::Stopped(StopInfo::at(pc, cycle, reason)))
        }
    }
}

/// `step` for the host trait: `n` instructions, `over` running a `JSR` to its return address.
pub(super) fn step(host: &mut Host, n: u64, over: bool) -> Result<StopInfo, String> {
    if n == 0 {
        return Err("a step of 0 instructions is not a step".into());
    }
    let held = halted(host);
    set_stop(host, false);
    let mut steps = Vec::new();
    for _ in 0..n {
        let (pc0, cycle) = pc_cycle(host);
        // `JSR abs` is $20; stepping over it means running until the byte after it (c64_6510core).
        let jsr = over && host.machine().read_full(pc0) == 0x20;
        match jsr {
            true => {
                let back = pc0.wrapping_add(3);
                for _ in 0..UNTIL_INSNS {
                    run_c64(host, 64, 1);
                    if pc_cycle(host).0 == back {
                        break;
                    }
                }
            }
            false => run_c64(host, 64, 1),
        }
        let (pc1, after) = pc_cycle(host);
        steps.push(StepClass { is_int: false, is_rti: false, flow: FlowKind::Main, pc0, pc1, cycle_abs: cycle });
        let _ = after;
    }
    if held {
        set_stop(host, true);
    }
    let (pc, cycle) = pc_cycle(host);
    Ok(StopInfo { pc, cycle, reason: "step".into(), steps })
}

/// Run until the subroutine the C64 is in returns: the address under the stack pointer is where it goes back to,
/// and `RTS` leaves the PC one past it (`EXECUTE_UNTIL_RETURN`, S23 §8).
pub(super) fn until_return(host: &mut Host) -> Result<(), String> {
    let held = halted(host);
    set_stop(host, false);
    let machine = host.machine();
    let sp = machine.c64_core.reg_sp;
    let lo = machine.read_full(0x0100 + u16::from(sp.wrapping_add(1)));
    let hi = machine.read_full(0x0100 + u16::from(sp.wrapping_add(2)));
    let back = u16::from_le_bytes([lo, hi]).wrapping_add(1);
    for _ in 0..UNTIL_INSNS {
        run_c64(host, 64, 1);
        if pc_cycle(host).0 == back {
            break;
        }
    }
    if held {
        set_stop(host, true);
    }
    Ok(())
}

/// Reset the C64 through `C64_MODE`, the register the firmware resets it with (c64.h:70-73). Warm and cold are the
/// same line here; the difference on hardware is what the firmware does around it, which is the firmware's.
pub(super) fn reset(host: &mut Host, _hard: bool) -> Result<(), String> {
    const C64_MODE: u32 = 0x1004_0000;
    const MODE_RESET: u8 = 0x04;
    const MODE_UNRESET: u8 = 0x08;
    let mode = peek8(&host.m.bus, C64_MODE);
    host.m.bus.poke_io8(C64_MODE, (mode & !MODE_UNRESET) | MODE_RESET);
    run_c64(host, 64, 1);
    host.m.bus.poke_io8(C64_MODE, (mode & !MODE_RESET) | MODE_UNRESET);
    Ok(())
}

/// The C64's PC and cycle.
fn pc_cycle(host: &mut Host) -> (u16, u64) {
    let c64 = host.machine();
    (c64.c64_core.reg_pc, c64.c64_core.clk)
}

/// Run the C64's own core out of band: the firmware's clock does not move.
fn run_c64(host: &mut Host, cycles: u64, insns: u64) {
    host.machine().run_for_full_capped(cycles, insns, &mut trx64_core::NullSink, |_, _, _, _, _, _, _| {});
}
