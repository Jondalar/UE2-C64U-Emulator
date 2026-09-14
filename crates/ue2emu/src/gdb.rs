//! GDB remote stub (`--gdb ADDR`) and side-effect-free debugger views of the machine.
//! Spec: docs/specs/S11-S14-later.md §S11
//!
//! The stub is the `gdbstub` state machine, driven by the emulation thread between `Machine::run` slices
//! (`runner`), so the runner keeps applying commands and publishing the display while the debugger holds the
//! machine. A reader thread forwards the socket bytes over a channel; replies are written on the emulation thread.
//!
//! - **Attach:** the machine waits at reset (ELF entry, no instruction executed) until a debugger connects and
//!   continues. A debugger that connects after a detach stops the machine where it is.
//! - **Registers:** x0-x31 and pc (`gdbstub_arch` rv32i target description). A write keeps x0 = 0 and clears pc
//!   bits 1:0, as for every rvlite jump target (fetch.vhd:58).
//! - **Memory:** reads decode like the data bus (`ue2_core::bus`): DDR and its mirrors from RAM, IO bytes from
//!   `IoDevice::peek8`, so a read never has side effects; the boot BRAM page and unmapped addresses read 0.
//!   Writes reach DDR only; a write touching any other address fails and changes nothing.
//! - **Breakpoints:** software breakpoints are `Machine::breakpoints`, checked before the instruction; RAM is not
//!   patched. No hardware breakpoints or watchpoints.
//! - **Execution:** step is `Machine::run(1)`, so a pending interrupt is taken as the step (docs/hw/01 H6).
//!   Continue runs slices until a breakpoint, a fault hook, or Ctrl-C (SIGINT). A fault hook stops the debugger
//!   with SIGABRT and prints the halt message and the trace ring instead of ending the run. Signals sent by the
//!   debugger are ignored.
//! - **End:** detach lets the machine run on; kill ends the emulator. A connection that closes without a detach
//!   leaves the machine running once any debugger has resumed it, and stopped before that (so a port probe does
//!   not release the reset hold).
//! - **Monitor:** `monitor tasks` lists the FreeRTOS tasks, `monitor trace` prints the CPU trace ring (`--gdb`
//!   turns it on, main.rs).

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gdbstub::common::Signal;
use gdbstub::stub::state_machine::GdbStubStateMachine;
use gdbstub::stub::{DisconnectReason, GdbStub, GdbStubError, SingleThreadStopReason};
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadResumeOps, SingleThreadSingleStep, SingleThreadSingleStepOps,
};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::breakpoints::{Breakpoints, BreakpointsOps, SwBreakpoint, SwBreakpointOps};
use gdbstub::target::ext::monitor_cmd::{ConsoleOutput, MonitorCmd, MonitorCmdOps};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::riscv::reg::RiscvCoreRegs;
use gdbstub_arch::riscv::Riscv32;
use ue2_core::bus::{SystemBus, BOOT_BRAM_PAGE, IO_BIT, RAM_MASK};
use ue2_core::machine::{Machine, RunExit};

/// Longest wait for debugger input while the machine is held, so the runner keeps serving its commands.
const POLL: Duration = Duration::from_millis(10);
/// `EFAULT`, reported for a memory write outside DDR.
const EFAULT: u8 = 14;
const MONITOR_HELP: &str = "monitor commands: tasks (FreeRTOS task list), trace (CPU trace ring)\n";

/// Listen for a debugger on `addr`.
///
/// The emulation thread builds the [`GdbServer`] around the listener: the stub's target owns the machine, which is
/// not `Send`.
pub fn listen(addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(addr).with_context(|| format!("--gdb: binding {addr}"))?;
    listener.set_nonblocking(true).context("--gdb: non-blocking listener")?;
    let local = listener.local_addr().context("--gdb: listener address")?;
    eprintln!("gdb: listening on {local}; the machine waits at reset until a debugger continues");
    Ok(listener)
}

/// The stub on the emulation thread: serves one debugger at a time and runs the machine on its behalf.
pub struct GdbServer {
    listener: TcpListener,
    session: Option<Session>,
    /// Keep the machine stopped while no debugger is attached; true until a debugger resumes or detaches.
    hold: bool,
    /// Resume action of the attached debugger, kept across slices until the stop is reported.
    action: Action,
}

impl GdbServer {
    pub fn new(listener: TcpListener) -> Self {
        GdbServer { listener, session: None, hold: true, action: Action::Continue }
    }

    /// One runner slice of at most `max_insns` instructions on the debugger's terms.
    ///
    /// The machine moves in and out because the `gdbstub` target owns it while packets are handled. Returns None
    /// once the debugger killed the target. A fault halt is returned only while no debugger is attached.
    pub fn run(&mut self, machine: Machine, max_insns: u64) -> (Machine, Option<RunExit>) {
        let mut target = Debuggee { machine, action: self.action };
        let exit = self.slice(&mut target, max_insns);
        self.action = target.action;
        (target.machine, exit)
    }

    fn slice(&mut self, target: &mut Debuggee, max_insns: u64) -> Option<RunExit> {
        if self.session.is_none() {
            self.session = self.accept(target);
        }
        let Some(session) = self.session.take() else {
            if self.hold {
                thread::sleep(POLL);
                return Some(RunExit::Budget);
            }
            return Some(target.machine.run(max_insns));
        };
        match session.pump(target, max_insns) {
            Pumped::Attached(session) => {
                // The first resume releases the hold for good.
                self.hold &= !matches!(session.stub, GdbStubStateMachine::Running(_));
                self.session = Some(session);
            }
            Pumped::Ended(DisconnectReason::Kill) => {
                eprintln!("gdb: killed by the debugger");
                return None;
            }
            Pumped::Ended(_) => {
                eprintln!("gdb: detached at {}; the machine runs on", target.pc_symbol());
                self.hold = false;
            }
            Pumped::Lost if self.hold => eprintln!("gdb: connection closed; the machine waits for a debugger"),
            Pumped::Lost => eprintln!("gdb: connection closed at {}; the machine runs on", target.pc_symbol()),
        }
        Some(RunExit::Budget)
    }

    /// Session of a debugger connecting now; the machine stops where it is.
    fn accept(&self, target: &mut Debuggee) -> Option<Session> {
        let (stream, peer) = match self.listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
            Err(e) => {
                eprintln!("gdb: accept: {e}");
                return None;
            }
        };
        match Session::start(stream, target) {
            Ok(session) => {
                eprintln!("gdb: debugger {peer} attached at {}", target.pc_symbol());
                Some(session)
            }
            Err(e) => {
                eprintln!("gdb: {peer}: {e:#}");
                None
            }
        }
    }
}

type Stub = GdbStubStateMachine<'static, Debuggee, TcpStream>;
type StubError = GdbStubError<Infallible, io::Error>;

/// One debugger connection.
struct Session {
    stub: Stub,
    /// Socket bytes from the reader thread; disconnected once the debugger closed the connection.
    rx: Receiver<Vec<u8>>,
    /// Shutting this clone down also ends the reader thread.
    socket: TcpStream,
}

enum Pumped {
    Attached(Session),
    /// The debugger sent detach or kill.
    Ended(DisconnectReason),
    /// The connection closed or failed without a detach.
    Lost,
}

/// A session that ended without a detach or kill packet.
enum Lost {
    Closed,
    Stub(StubError),
}

impl From<StubError> for Lost {
    fn from(e: StubError) -> Self {
        Lost::Stub(e)
    }
}

impl Session {
    fn start(stream: TcpStream, target: &mut Debuggee) -> Result<Session> {
        // A socket accepted from a non-blocking listener is non-blocking itself on macOS; replies are written
        // blocking.
        stream.set_nonblocking(false).context("blocking socket")?;
        let socket = stream.try_clone().context("cloning the socket")?;
        let mut reader = stream.try_clone().context("cloning the socket")?;
        let stub = GdbStub::new(stream).run_state_machine(target).map_err(|e| anyhow!("{e}"))?;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new().name("gdb-reader".into()).spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        if let Err(e) = spawned {
            let _ = socket.shutdown(Shutdown::Both);
            return Err(e).context("starting the gdb reader thread");
        }
        Ok(Session { stub, rx, socket })
    }

    /// Feed the debugger's bytes to the stub, then run the machine while the stub lets it.
    fn pump(self, target: &mut Debuggee, max_insns: u64) -> Pumped {
        let Session { stub, rx, socket } = self;
        let ended = match advance(stub, &rx, target, max_insns) {
            Ok(GdbStubStateMachine::Disconnected(stub)) => Pumped::Ended(stub.get_reason()),
            Ok(stub) => return Pumped::Attached(Session { stub, rx, socket }),
            Err(Lost::Closed) => Pumped::Lost,
            Err(Lost::Stub(e)) => {
                eprintln!("gdb: {e}");
                Pumped::Lost
            }
        };
        let _ = socket.shutdown(Shutdown::Both);
        ended
    }
}

/// Input first: wait up to [`POLL`] while the machine is stopped, otherwise take only what is queued. Then execute
/// the resume action if the stub is running.
fn advance(mut stub: Stub, rx: &Receiver<Vec<u8>>, target: &mut Debuggee, max_insns: u64) -> Result<Stub, Lost> {
    let mut wait = matches!(stub, GdbStubStateMachine::Idle(_));
    while let Some(bytes) = receive(rx, wait)? {
        wait = false;
        for byte in bytes {
            stub = feed(stub, target, byte)?;
        }
        if matches!(stub, GdbStubStateMachine::Disconnected(_)) {
            return Ok(stub);
        }
    }
    Ok(match stub {
        GdbStubStateMachine::Running(running) => match target.execute(max_insns) {
            Some(stop) => running.report_stop(target, stop)?,
            None => running.into(),
        },
        other => other,
    })
}

fn receive(rx: &Receiver<Vec<u8>>, wait: bool) -> Result<Option<Vec<u8>>, Lost> {
    if wait {
        match rx.recv_timeout(POLL) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Lost::Closed),
        }
    } else {
        match rx.try_recv() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Lost::Closed),
        }
    }
}

/// One byte into the stub. A Ctrl-C is answered at once with SIGINT, so `CtrlCInterrupt` never outlives a call;
/// bytes after a disconnect are dropped.
fn feed(stub: Stub, target: &mut Debuggee, byte: u8) -> Result<Stub, StubError> {
    let stub = match stub {
        GdbStubStateMachine::Idle(idle) => idle.incoming_data(target, byte)?,
        GdbStubStateMachine::Running(running) => running.incoming_data(target, byte)?,
        other => other,
    };
    match stub {
        GdbStubStateMachine::CtrlCInterrupt(interrupt) => {
            interrupt.interrupt_handled(target, Some(SingleThreadStopReason::<u32>::Signal(Signal::SIGINT)))
        }
        other => Ok(other),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Step,
    Continue,
}

/// The `gdbstub` target: the machine plus the pending resume action.
struct Debuggee {
    machine: Machine,
    action: Action,
}

impl Debuggee {
    fn pc_symbol(&self) -> String {
        self.machine.symbols.format(self.machine.cpu.pc)
    }

    /// Run the resume action: the stop to report, or None while a continue is still running.
    fn execute(&mut self, max_insns: u64) -> Option<SingleThreadStopReason<u32>> {
        let step = self.action == Action::Step;
        match self.machine.run(if step { 1 } else { max_insns }) {
            RunExit::Budget if step => Some(SingleThreadStopReason::DoneStep),
            RunExit::Budget => None,
            RunExit::Breakpoint(_) => Some(SingleThreadStopReason::SwBreak(())),
            RunExit::Halted(msg) => {
                eprintln!("halted: {msg}");
                eprint!("{}", self.machine.trace_report());
                Some(SingleThreadStopReason::Signal(Signal::SIGABRT))
            }
        }
    }
}

impl Target for Debuggee {
    type Arch = Riscv32;
    type Error = Infallible;

    #[inline(always)]
    fn base_ops(&mut self) -> BaseOps<'_, Riscv32, Infallible> {
        BaseOps::SingleThread(self)
    }

    #[inline(always)]
    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_monitor_cmd(&mut self) -> Option<MonitorCmdOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadBase for Debuggee {
    fn read_registers(&mut self, regs: &mut RiscvCoreRegs<u32>) -> TargetResult<(), Self> {
        regs.x = self.machine.cpu.x;
        regs.pc = self.machine.cpu.pc;
        Ok(())
    }

    fn write_registers(&mut self, regs: &RiscvCoreRegs<u32>) -> TargetResult<(), Self> {
        let cpu = &mut self.machine.cpu;
        cpu.x = regs.x;
        cpu.x[0] = 0;
        cpu.pc = regs.pc & !3;
        Ok(())
    }

    fn read_addrs(&mut self, start: u32, data: &mut [u8]) -> TargetResult<usize, Self> {
        for (i, byte) in (0u32..).zip(data.iter_mut()) {
            *byte = peek8(&self.machine.bus, start.wrapping_add(i));
        }
        Ok(data.len())
    }

    fn write_addrs(&mut self, start: u32, data: &[u8]) -> TargetResult<(), Self> {
        let indices: Option<Vec<usize>> = (0u32..).zip(data).map(|(i, _)| ram_index(start.wrapping_add(i))).collect();
        let indices = indices.ok_or(TargetError::Errno(EFAULT))?;
        for (i, &byte) in indices.into_iter().zip(data) {
            self.machine.bus.ram[i] = byte;
        }
        Ok(())
    }

    #[inline(always)]
    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for Debuggee {
    fn resume(&mut self, _signal: Option<Signal>) -> Result<(), Infallible> {
        self.action = Action::Continue;
        Ok(())
    }

    #[inline(always)]
    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for Debuggee {
    fn step(&mut self, _signal: Option<Signal>) -> Result<(), Infallible> {
        self.action = Action::Step;
        Ok(())
    }
}

impl Breakpoints for Debuggee {
    #[inline(always)]
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for Debuggee {
    fn add_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        if !self.machine.breakpoints.contains(&addr) {
            self.machine.breakpoints.push(addr);
        }
        Ok(true)
    }

    fn remove_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        let breakpoints = &mut self.machine.breakpoints;
        let before = breakpoints.len();
        breakpoints.retain(|&bp| bp != addr);
        Ok(breakpoints.len() != before)
    }
}

impl MonitorCmd for Debuggee {
    fn handle_monitor_cmd(&mut self, cmd: &[u8], mut out: ConsoleOutput<'_>) -> Result<(), Infallible> {
        let text = match String::from_utf8_lossy(cmd).trim() {
            "tasks" => task_list(&self.machine),
            "trace" => self.machine.trace_report(),
            "" | "help" => MONITOR_HELP.to_owned(),
            other => format!("unknown monitor command '{other}'\n{MONITOR_HELP}"),
        };
        let _ = out.write_str(&text);
        Ok(())
    }
}

/// DDR index of a data address: bit 28 clear and outside the boot BRAM page (`ue2_core::bus` decode).
fn ram_index(addr: u32) -> Option<usize> {
    (addr & IO_BIT == 0 && addr >> 16 != BOOT_BRAM_PAGE).then_some((addr & RAM_MASK) as usize)
}

/// Debugger view of one data-bus byte, without side effects: DDR (and its mirrors) from RAM, IO bytes from
/// `IoDevice::peek8`, 0 for the boot BRAM page and unmapped addresses.
fn peek8(bus: &SystemBus, addr: u32) -> u8 {
    match ram_index(addr) {
        Some(i) => bus.ram[i],
        None => bus.io.resolve(addr).map_or(0, |(dev, off)| bus.io.devices[dev].peek8(off)),
    }
}

fn peek32(bus: &SystemBus, addr: u32) -> u32 {
    u32::from_le_bytes(std::array::from_fn(|i| peek8(bus, addr.wrapping_add(i as u32))))
}

// FreeRTOS kernel layout of this firmware build (tasks.c:252-264, list.h:140-170), offsets as `ptype/o` shows them
// in the ELF's DWARF.
/// `TCB_t.xEventListItem` (tasks.c:261).
const TCB_EVENT_ITEM: u32 = 24;
/// `TCB_t.uxPriority` (tasks.c:262).
const TCB_PRIORITY: u32 = 44;
/// `TCB_t.pcTaskName`, `configMAX_TASK_NAME_LEN` bytes (tasks.c:264; FreeRTOSConfig.h:25).
const TCB_NAME: u32 = 52;
const TASK_NAME_LEN: u32 = 16;
/// `List_t.xListEnd` (list.h:164-169).
const LIST_END: u32 = 8;
/// `sizeof(List_t)`; `pxReadyTasksLists` holds `configMAX_PRIORITIES` of them (tasks.c:340; FreeRTOSConfig.h:22).
const LIST_SIZE: u32 = 20;
const MAX_PRIORITIES: u32 = 5;
/// `ListItem_t.pxNext`, `.pvOwner` and `.pxContainer` (list.h:140-148).
const ITEM_NEXT: u32 = 4;
const ITEM_OWNER: u32 = 12;
const ITEM_CONTAINER: u32 = 16;
/// Bound on one list walk, so corrupt links cannot loop forever.
const MAX_LIST_ITEMS: usize = 256;

/// `monitor tasks`: every task on the kernel's state lists (tasks.c:334-356), read through [`peek8`].
///
/// States follow `eTaskGetState` (tasks.c:1378-1459): `pxCurrentTCB` is running, a task on the suspended list with
/// an event list is blocked; a suspended task waiting on a notification is shown as suspended.
fn task_list(machine: &Machine) -> String {
    let bus = &machine.bus;
    let symbol = |name| machine.symbols.addr_of(name);
    let (Some(current), Some(ready)) = (symbol("pxCurrentTCB"), symbol("pxReadyTasksLists")) else {
        return "no FreeRTOS symbols (pxCurrentTCB, pxReadyTasksLists) in the ELF\n".to_owned();
    };
    let current = peek32(bus, current);
    let mut lists: Vec<(&str, u32)> = (0..MAX_PRIORITIES).map(|p| ("ready", ready + p * LIST_SIZE)).collect();
    for (state, name) in [
        ("ready", "xPendingReadyList"),
        ("blocked", "xDelayedTaskList1"),
        ("blocked", "xDelayedTaskList2"),
        ("suspended", "xSuspendedTaskList"),
        ("deleted", "xTasksWaitingTermination"),
    ] {
        lists.extend(symbol(name).map(|addr| (state, addr)));
    }

    let mut out = String::from("TCB        pri  state     name\n");
    let mut tasks = 0;
    for (state, list) in lists {
        for tcb in list_owners(bus, list) {
            let state = if tcb == current {
                "running"
            } else if state == "suspended" && peek32(bus, tcb.wrapping_add(TCB_EVENT_ITEM + ITEM_CONTAINER)) != 0 {
                "blocked"
            } else {
                state
            };
            let name: String = (0..TASK_NAME_LEN)
                .map(|i| peek8(bus, tcb.wrapping_add(TCB_NAME + i)))
                .take_while(|&b| b != 0)
                .map(char::from)
                .collect();
            let priority = peek32(bus, tcb.wrapping_add(TCB_PRIORITY));
            let _ = writeln!(out, "{tcb:#010x} {priority:>3}  {state:<9} {name}");
            tasks += 1;
        }
    }
    if tasks == 0 {
        return "no FreeRTOS tasks yet\n".to_owned();
    }
    out
}

/// `pvOwner` of each item of the `List_t` at `list`, in list order. A zero link (list not initialised yet) ends the
/// walk like the list end marker.
fn list_owners(bus: &SystemBus, list: u32) -> Vec<u32> {
    let end = list.wrapping_add(LIST_END);
    let mut owners = Vec::new();
    let mut item = peek32(bus, end.wrapping_add(ITEM_NEXT));
    while item != end && item != 0 && owners.len() < MAX_LIST_ITEMS {
        owners.push(peek32(bus, item.wrapping_add(ITEM_OWNER)));
        item = peek32(bus, item.wrapping_add(ITEM_NEXT));
    }
    owners
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::thread::JoinHandle;
    use std::time::Instant;

    use ue2_core::io::{IoCtx, IoDevice, IO_BASE};
    use ue2_core::machine::MachineConfig;
    use ue2_core::symbols::{Symbol, Symbols};

    use super::*;

    /// IO device whose reads count and return `0xA0 | off`; `peek8` returns `0x50 | off`.
    struct Probe {
        reads: u32,
    }

    impl IoDevice for Probe {
        fn name(&self) -> &'static str {
            "probe"
        }

        fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
            self.reads += 1;
            0xA0 | off as u8
        }

        fn write8(&mut self, _off: u32, _val: u8, _ctx: &mut IoCtx) {}

        fn peek8(&self, off: u32) -> u8 {
            0x50 | off as u8
        }

        ue2_core::impl_as_any!();
    }

    /// `addi x5,x5,1; addi x6,x6,2; addi x7,x7,3; j 0x30000` at the entry 0x30000.
    const LOOP: [u32; 4] = [0x0012_8293, 0x0023_0313, 0x0033_8393, 0xFF5F_F06F];

    fn sym(addr: u32, size: u32, name: &str) -> Symbol {
        Symbol { addr, size, name: name.to_owned(), mangled: name.to_owned() }
    }

    fn put32(machine: &mut Machine, addr: u32, val: u32) {
        machine.bus.ram[addr as usize..][..4].copy_from_slice(&val.to_le_bytes());
    }

    /// Machine running [`LOOP`], a [`Probe`] at IO_BASE.
    fn machine(symbols: Symbols) -> Machine {
        let mut bus = SystemBus::new();
        bus.io.add(IO_BASE, 0x100, Box::new(Probe { reads: 0 }));
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        cfg.trace = true;
        let mut machine = Machine::from_parts(cfg, bus, 0x30000, symbols);
        for (addr, word) in (0x30000..).step_by(4).zip(LOOP) {
            put32(&mut machine, addr, word);
        }
        machine
    }

    fn probe_reads(machine: &Machine) -> u32 {
        machine.bus.io.devices[0].as_any().downcast_ref::<Probe>().unwrap().reads
    }

    fn debuggee() -> Debuggee {
        Debuggee { machine: machine(Symbols::empty()), action: Action::Continue }
    }

    #[test]
    fn memory_reads_peek_io_without_side_effects() {
        let mut target = debuggee();
        put32(&mut target.machine, 0x1000, 0xCAFE_F00D);
        let mut buf = [0; 4];
        for (addr, want) in [
            (0x1000, [0x0D, 0xF0, 0xFE, 0xCA]),
            (0x0400_1000, [0x0D, 0xF0, 0xFE, 0xCA]),
            (IO_BASE + 4, [0x54, 0x55, 0x56, 0x57]),
            (0x10FF_0000, [0; 4]),
            (0x8000_1000, [0; 4]),
            (0x3000_1000, [0; 4]),
        ] {
            assert_eq!(target.read_addrs(addr, &mut buf).ok(), Some(4));
            assert_eq!(buf, want, "{addr:#010x}");
        }
        assert_eq!(target.read_addrs(0xFFFF_FFFE, &mut buf).ok(), Some(4), "wraps around the address space");
        assert_eq!(probe_reads(&target.machine), 0);
        assert!(!target.machine.bus.io_touched);
    }

    #[test]
    fn memory_writes_reach_ram_only() {
        let mut target = debuggee();
        assert!(target.write_addrs(0x0400_2000, &[1, 2, 3]).is_ok());
        assert_eq!(target.machine.bus.ram[0x2000..0x2003], [1, 2, 3], "64 MB mirror");
        assert!(matches!(target.write_addrs(IO_BASE - 1, &[9, 9]), Err(TargetError::Errno(EFAULT))));
        assert_eq!(target.machine.bus.ram[RAM_MASK as usize], 0, "nothing written when a byte is not RAM");
        assert!(matches!(target.write_addrs(0x8000_0000, &[9]), Err(TargetError::Errno(EFAULT))));
        assert_eq!(probe_reads(&target.machine), 0);
    }

    #[test]
    fn registers_keep_x0_zero_and_pc_aligned() {
        let mut target = debuggee();
        let mut regs = RiscvCoreRegs::<u32>::default();
        target.machine.cpu.x[7] = 0x77;
        assert!(target.read_registers(&mut regs).is_ok());
        assert_eq!((regs.x[7], regs.pc), (0x77, 0x30000));
        regs.x = [0xFFFF_FFFF; 32];
        regs.pc = 0x0003_0107;
        assert!(target.write_registers(&regs).is_ok());
        assert_eq!((target.machine.cpu.x[0], target.machine.cpu.x[31], target.machine.cpu.pc), (0, 0xFFFF_FFFF, 0x30104));
    }

    #[test]
    fn software_breakpoints_are_machine_breakpoints() {
        let mut target = debuggee();
        assert_eq!(target.add_sw_breakpoint(0x30008, 4).ok(), Some(true));
        assert_eq!(target.add_sw_breakpoint(0x30008, 4).ok(), Some(true));
        assert_eq!(target.machine.breakpoints, [0x30008]);
        assert_eq!(target.machine.run(100), RunExit::Breakpoint(0x30008));
        assert_eq!(target.remove_sw_breakpoint(0x30008, 4).ok(), Some(true));
        assert_eq!(target.remove_sw_breakpoint(0x30008, 4).ok(), Some(false));
        assert!(target.machine.breakpoints.is_empty());
    }

    #[test]
    fn task_list_walks_the_kernel_lists() {
        let symbols = Symbols::from_symbols(vec![
            sym(0x1000, 4, "pxCurrentTCB"),
            sym(0x2000, 100, "pxReadyTasksLists"),
            sym(0x3000, 20, "xDelayedTaskList1"),
            sym(0x3100, 20, "xSuspendedTaskList"),
        ]);
        let mut m = machine(symbols);
        assert_eq!(task_list(&m), "no FreeRTOS tasks yet\n");

        // TCB at `tcb` with name and priority, linked after `prev` (an item, or a list end marker).
        let mut task = |tcb: u32, prio: u32, name: &[u8], prev: u32, end: u32| {
            m.bus.ram[(tcb + TCB_NAME) as usize..][..name.len()].copy_from_slice(name);
            put32(&mut m, tcb + TCB_PRIORITY, prio);
            put32(&mut m, prev + ITEM_NEXT, tcb + 4);
            put32(&mut m, tcb + 4 + ITEM_NEXT, end);
            put32(&mut m, tcb + 4 + ITEM_OWNER, tcb);
        };
        task(0x5000, 1, b"main", 0x2014 + LIST_END, 0x2014 + LIST_END);
        task(0x6000, 2, b"Tmr Svc", 0x3000 + LIST_END, 0x3000 + LIST_END);
        task(0x7000, 3, b"usb", 0x3100 + LIST_END, 0x8004);
        task(0x8000, 4, b"sixteen-chars-xx", 0x7004, 0x3100 + LIST_END);
        put32(&mut m, 0x7000 + TCB_EVENT_ITEM + ITEM_CONTAINER, 0x4000);
        put32(&mut m, 0x1000, 0x5000);
        assert_eq!(
            task_list(&m),
            "TCB        pri  state     name\n\
             0x00005000   1  running   main\n\
             0x00006000   2  blocked   Tmr Svc\n\
             0x00007000   3  blocked   usb\n\
             0x00008000   4  suspended sixteen-chars-xx\n"
        );
        assert_eq!(task_list(&machine(Symbols::empty())), "no FreeRTOS symbols (pxCurrentTCB, pxReadyTasksLists) in the ELF\n");
    }

    /// Minimal RSP client: `$data#cs` packets, acknowledges replies, expands run-length encoding.
    struct Client {
        stream: TcpStream,
    }

    impl Client {
        fn connect(addr: SocketAddr) -> Client {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            Client { stream }
        }

        fn send(&mut self, data: &str) {
            let sum = data.bytes().fold(0u8, u8::wrapping_add);
            write!(self.stream, "${data}#{sum:02x}").unwrap();
        }

        fn byte(&mut self) -> u8 {
            let mut b = [0];
            self.stream.read_exact(&mut b).unwrap();
            b[0]
        }

        fn recv(&mut self) -> String {
            while self.byte() != b'$' {}
            let mut data = Vec::new();
            loop {
                match self.byte() {
                    b'#' => break,
                    b'*' => {
                        let last = *data.last().unwrap();
                        let n = self.byte() - 29;
                        data.extend(std::iter::repeat_n(last, n.into()));
                    }
                    b => data.push(b),
                }
            }
            self.byte();
            self.byte();
            self.stream.write_all(b"+").unwrap();
            String::from_utf8(data).unwrap()
        }

        fn request(&mut self, data: &str) -> String {
            self.send(data);
            self.recv()
        }

        /// Register `n` (x0-x31, 32 = pc) from a `g` reply.
        fn register(&mut self, n: usize) -> u32 {
            let g = self.request("g");
            u32::from_str_radix(&g[n * 8..n * 8 + 8], 16).unwrap().swap_bytes()
        }
    }

    /// Run `server` like the runner does until the debugger kills the target or 30 s pass; then join the client.
    fn serve(mut server: GdbServer, mut machine: Machine, client: JoinHandle<()>) -> (GdbServer, Machine, bool) {
        let start = Instant::now();
        let mut killed = false;
        while start.elapsed() < Duration::from_secs(30) {
            let (m, exit) = server.run(machine, 1000);
            machine = m;
            if exit.is_none() {
                killed = true;
                break;
            }
            if client.is_finished() && server.session.is_none() {
                break;
            }
        }
        client.join().unwrap();
        (server, machine, killed)
    }

    fn server() -> (GdbServer, SocketAddr) {
        let listener = listen("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        (GdbServer::new(listener), addr)
    }

    #[test]
    fn holds_at_reset_until_a_debugger_continues() {
        let (mut server, addr) = server();
        let mut m = machine(Symbols::empty());
        drop(TcpStream::connect(addr).unwrap());
        let start = Instant::now();
        for slice in 0.. {
            let (next, exit) = server.run(m, 1000);
            m = next;
            assert_eq!(exit, Some(RunExit::Budget));
            if slice >= 20 && server.session.is_none() || start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        assert!(server.hold && server.session.is_none(), "a connection without packets keeps the hold");
        assert_eq!((m.cpu.pc, m.cpu.insns), (0x30000, 0));
    }

    #[test]
    fn rsp_session_breakpoint_step_interrupt_monitor_kill() {
        let (server, addr) = server();
        let client = thread::spawn(move || {
            let mut gdb = Client::connect(addr);
            assert!(gdb.request("?").starts_with("T05"));
            assert_eq!(gdb.register(32), 0x30000, "held at reset");
            assert_eq!(gdb.request("m10000004,4"), "54555657", "IO through peek8");
            assert_eq!(gdb.request("M1000,4:deadbeef"), "OK");
            assert_eq!(gdb.request("m1000,4"), "deadbeef");
            assert_eq!(gdb.request("M10000000,1:ff"), "E0e");

            assert_eq!(gdb.request("Z0,30008,4"), "OK");
            assert!(gdb.request("c").starts_with("T05"));
            assert_eq!((gdb.register(32), gdb.register(5), gdb.register(6)), (0x30008, 1, 2));
            assert_eq!(gdb.request("z0,30008,4"), "OK");
            assert_eq!(gdb.request("s"), "S05");
            assert_eq!((gdb.register(32), gdb.register(7)), (0x3000C, 3));

            gdb.send("c");
            thread::sleep(Duration::from_millis(50));
            gdb.stream.write_all(&[0x03]).unwrap();
            assert_eq!(gdb.recv(), "S02", "Ctrl-C stops with SIGINT");

            assert!(gdb.monitor("trace").starts_with("trace: last 256 of "));
            assert_eq!(gdb.monitor("tasks"), "no FreeRTOS symbols (pxCurrentTCB, pxReadyTasksLists) in the ELF\n");
            assert_eq!(gdb.monitor("bogus"), format!("unknown monitor command 'bogus'\n{MONITOR_HELP}"));
            gdb.send("k");
        });
        let (_, m, killed) = serve(server, machine(Symbols::empty()), client);
        assert!(killed);
        assert!(m.cpu.insns > 3);
        assert_eq!(probe_reads(&m), 0);
    }

    impl Client {
        /// Console text of `monitor cmd` (`qRcmd`: `O` packets, then `OK`).
        fn monitor(&mut self, cmd: &str) -> String {
            let hex: String = cmd.bytes().map(|b| format!("{b:02x}")).collect();
            self.send(&format!("qRcmd,{hex}"));
            let mut console = String::new();
            loop {
                let reply = self.recv();
                if reply == "OK" {
                    return console;
                }
                let bytes = (1..reply.len()).step_by(2).map(|i| u8::from_str_radix(&reply[i..i + 2], 16).unwrap());
                console.extend(bytes.map(char::from));
            }
        }
    }

    #[test]
    fn detach_lets_the_machine_run_and_a_new_debugger_stops_it() {
        let (server, addr) = server();
        let client = thread::spawn(move || {
            let mut gdb = Client::connect(addr);
            assert_eq!(gdb.request("D"), "OK");
        });
        let (mut server, mut m, killed) = serve(server, machine(Symbols::empty()), client);
        assert!(!killed && !server.hold);
        let before = m.cpu.insns;
        let (next, exit) = server.run(m, 1000);
        m = next;
        assert_eq!((exit, m.cpu.insns - before), (Some(RunExit::Budget), 1000), "runs without a debugger after detach");

        let client = thread::spawn(move || {
            let mut gdb = Client::connect(addr);
            let pc = gdb.register(32);
            assert!((0x30000..0x30010).contains(&pc));
            gdb.send("k");
        });
        let (_, m, killed) = serve(server, m, client);
        assert!(killed);
        assert!(m.cpu.insns >= 1000);
    }
}
