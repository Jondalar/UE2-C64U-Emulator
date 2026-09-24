//! Emulation thread, pacing, headless mode. Spec: docs/specs/S02-core.md

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ue2_core::host::{DisplaySnapshot, HostInput};
use ue2_core::machine::{Machine, MachineConfig, RunExit};

use ue2_vfat::DirSpec;

use crate::audio::{self, AudioOptions, Output};
use crate::cartslot::{CartDone, CartRequest, CartSlot, CartSlotSpec};
use crate::control::{self, ConsoleLog, InputTimeline, TimedInputs};
use crate::gdb::{self, GdbServer};
#[cfg(feature = "trx64")]
use crate::vice::ViceServer;
use crate::net::{self, NetOptions};
use crate::usbdir::{UsbDirs, UsbDone, UsbRequest, EXPLICIT_WAIT_MS};

/// Issue #3: a roms directory whose `chars.bin` is the C64 character ROM draws the whole overlay menu in graphics
/// symbols. Say so once, with the file, instead of leaving the user with a garbled menu.
pub fn warn_on_c64_char_rom(font: &[u8], path: &std::path::Path) {
    if ue2_core::render::looks_like_c64_char_rom(font) {
        eprintln!(
            "overlay: {} is a C64 character ROM, not the firmware's overlay font; the menu will be unreadable. \
             Point --roms at the 1541ultimate roms directory (its chars.bin is 2 KB).",
            path.display()
        );
    }
}

/// Instructions per `Machine::run` slice (4 ms emulated at the default 4 clocks per instruction).
const SLICE_INSNS: u64 = 100_000;
/// Emulated interval between published display snapshots when there is no VIC frame counter to follow (S26 §2).
const DISPLAY_PERIOD_MS: u64 = 20;

/// Whether a display snapshot is due: at every new VIC picture, or every `DISPLAY_PERIOD_MS` without a counting
/// C64 (`frame == 0`), so the C64's row sets the rate under PAL and NTSC alike (S26 §2).
fn display_due(frame: u64, shown_frame: u64, now_ms: u64, next_display_ms: u64) -> bool {
    if frame == 0 { now_ms >= next_display_ms } else { frame != shown_frame }
}
/// Realtime pacing sleeps once emulated time leads wall time by more than this.
const PACE_SLACK_MS: u64 = 2;
/// Realtime pacing drops a larger backlog (host stall, sleep) instead of catching up at full speed.
const PACE_MAX_LAG_MS: u64 = 100;
/// Wall-clock interval over which `EmuHandle::mips` is measured.
const MIPS_INTERVAL: Duration = Duration::from_millis(500);
/// How often a held machine looks for a command (S23 M3). Short enough that `fw go` feels immediate, long enough
/// that a held emulator costs no CPU.
const HELD_POLL: Duration = Duration::from_millis(5);
/// Poll interval of the headless wall-clock wait.
const HEADLESS_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Realtime,
    Max,
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub speed: Speed,
    pub script: Option<PathBuf>,
    pub control: Option<String>,
    pub max_seconds: Option<f64>,
    pub gdb: Option<String>,
    /// VICE binary monitor address (`--vice-monitor`), S23 §8.
    pub vice_monitor: Option<String>,
    /// Wired Ethernet backend (`--net`).
    pub net: Option<NetOptions>,
    /// Attach the TRX64 C64 (`--c64 trx64`); main.rs allows it only with the `trx64` feature.
    pub c64: bool,
    /// SID audio device and WAV file (`--audio`, `--audio-wav`), used with the TRX64 C64 only.
    pub audio: AudioOptions,
    /// `--usb-dir` sticks, on the hub ports after the `--usb` images (docs/status/usb-dir.md).
    pub usb_dirs: Vec<DirSpec>,
    /// `--usb-dir-work`: images, manifests and snapshots of the `--usb-dir` volumes.
    pub usb_dir_work: PathBuf,
    /// `--cart-slot`: a cartridge in the physical expansion port (docs/status/cart-slot.md).
    pub cart_slot: Option<CartSlotSpec>,
    /// `--hold-key`: matrix keys pressed before the first instruction, until a control `release`.
    pub hold_keys: Vec<crate::keymap::MatrixKey>,
}

pub enum Command {
    Input(HostInput),
    /// A control-language input sequence (`button`, `key`, `type`, `usbkey`, `joy`), timed on the emulation thread by
    /// `control::InputTimeline`; `done` receives `()` once the sequence has ended in emulated time.
    Inputs { seq: TimedInputs, done: Sender<()> },
    /// `usb-sync` / `usb-replug` (`usbdir::UsbDirs::request`); `done` receives the result when it has finished.
    Usb { req: UsbRequest, done: UsbDone },
    /// S33: `usb-plug` / `usb-unplug`: a device onto an empty hub port, or the one on it off (None).
    UsbPlug { port: u8, device: Option<ue2_core::devices::usb::UsbDevice>, done: Sender<Result<String, String>> },
    /// `cart-info` / `cart-save` (`cartslot::CartSlot::request`).
    Cart { req: CartRequest, done: CartDone },
    /// S23: one monitor line, answered with its text. The session lives on the emulation thread, with the machine.
    Monitor { line: String, done: Sender<Result<String, String>> },
    Quit,
}

/// Cloneable view of a running emulator for frontends and control clients.
#[derive(Clone)]
pub struct ControlHandle {
    pub commands: Sender<Command>,
    /// Latest display snapshot, published at every VIC picture, or every 20 ms emulated without a C64 (S26).
    pub display: Arc<Mutex<DisplaySnapshot>>,
    /// Emulated milliseconds since power-on.
    pub now_ms: Arc<AtomicU64>,
    /// False once the emulation thread has stopped.
    pub running: Arc<AtomicBool>,
    /// Firmware console output since power-on, for `expect-console`.
    pub console: Arc<Mutex<ConsoleLog>>,
    /// `MachineConfig::rom_dir`; `png` reads its font (`chars.bin`) there.
    pub rom_dir: PathBuf,
}

pub struct EmuHandle {
    pub ctl: ControlHandle,
    /// Executed MIPS of the last interval: host work, which the idle skip lowers (S19 §5).
    pub mips: Arc<AtomicU64>,
    /// Emulated time against wall time over the last interval, in percent (S19 §5).
    pub speed_pct: Arc<AtomicU64>,
    pub join: JoinHandle<Result<()>>,
    /// The audio device stream; it plays until this is dropped.
    pub audio: Output,
}

/// Start the emulation thread.
///
/// The machine is built on that thread (device models are not `Send`); construction errors are returned here.
/// The thread runs `Machine::run` slices until `Command::Quit`, all command senders are gone, or the firmware
/// halts, and prints the machine stats (plus the unmapped summary with `--log unmapped`) to stderr when it ends.
pub fn spawn(cfg: MachineConfig, opts: &RunOptions) -> Result<EmuHandle> {
    let gdb = opts.gdb.as_deref().map(gdb::listen).transpose()?;
    let vice = vice_server(opts.vice_monitor.as_deref())?;
    // The audio device is opened on the caller's thread, which keeps its stream in `EmuHandle`; the sink moves to the
    // emulation thread with the C64.
    let (audio, sink) = if opts.c64 {
        audio::start(&opts.audio)?
    } else {
        if opts.audio.wav.is_some() {
            eprintln!("audio: --audio-wav needs the TRX64 C64 (--c64 trx64); no WAV is written");
        }
        (Output::none(), None)
    };
    let (commands, command_rx) = mpsc::channel();
    let ctl = ControlHandle {
        commands,
        display: Arc::default(),
        now_ms: Arc::default(),
        running: Arc::new(AtomicBool::new(true)),
        console: Arc::default(),
        rom_dir: cfg.rom_dir.clone(),
    };
    let mips = Arc::new(AtomicU64::new(0));
    let speed_pct = Arc::new(AtomicU64::new(0));
    let emu = EmuThread {
        display: ctl.display.clone(),
        now_ms: ctl.now_ms.clone(),
        console: ctl.console.clone(),
        mips: mips.clone(),
        speed_pct: speed_pct.clone(),
        speed: opts.speed,
    };
    let running = RunningFlag(ctl.running.clone());
    let (net_opts, c64, armsid) = (opts.net.clone(), opts.c64, opts.audio.armsid);
    let sid_thread = !opts.audio.no_sid_thread;
    let (usb_dirs, usb_dir_work, cart_slot) = (opts.usb_dirs.clone(), opts.usb_dir_work.clone(), opts.cart_slot.clone());
    let hold = control::key_events(&opts.hold_keys, true);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);

    let join = thread::Builder::new()
        .name("emulation".into())
        .spawn(move || {
            let _running = running;
            match build(cfg, net_opts.as_ref(), c64, sink, armsid, sid_thread, &usb_dirs, &usb_dir_work, cart_slot) {
                Ok((mut machine, net, dirs, cart)) => {
                    for (_, ev) in hold {
                        machine.input(ev);
                    }
                    let _ = ready_tx.send(Ok(()));
                    emu.run(machine, net, dirs, cart, &command_rx, gdb.map(GdbServer::new), vice)
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let _ = ready_tx.send(Err(e));
                    Err(anyhow!(msg))
                }
            }
        })
        .context("starting the emulation thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(EmuHandle { ctl, mips, speed_pct, join, audio }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e.context("building the machine"))
        }
        Err(_) => Err(match join.join() {
            Ok(Err(e)) => e,
            _ => anyhow!("the emulation thread panicked while building the machine"),
        }),
    }
}

/// The machine with its C64 and network backends, all built on the emulation thread (none is `Send`), the
/// `--usb-dir` sticks, built (or resumed) and attached before the machine runs, and the `--cart-slot` cartridge.
#[allow(clippy::too_many_arguments)]
fn build(
    cfg: MachineConfig,
    net: Option<&NetOptions>,
    c64: bool,
    sink: Option<audio::Sink>,
    armsid: bool,
    sid_thread: bool,
    usb_dirs: &[DirSpec],
    usb_dir_work: &std::path::Path,
    cart_slot: Option<CartSlotSpec>,
) -> Result<(Machine, Option<net::Backend>, UsbDirs, CartSlot)> {
    let mut machine = Machine::new(cfg)?;
    if c64 {
        attach_trx64_audio(&mut machine, sink, armsid, sid_thread, cart_slot.as_ref())?;
    }
    let cart = CartSlot::new(cart_slot, &mut machine);
    let net = net.map(|opts| net::attach(&mut machine, opts)).transpose()?;
    let first_port = machine.cfg.usb.images.len() + 1;
    let dirs = UsbDirs::attach(&mut machine, usb_dirs, usb_dir_work, first_port)?;
    Ok((machine, net, dirs, cart))
}

/// Attach TRX64 as the C64 (`--c64 trx64`, docs/specs/S14-c64-trx64.md §3), its ROMs seeded from `MachineConfig::rom_dir`,
/// with `cart_slot`'s cartridge in the expansion port. `c64_selected` in main.rs allows it only with the `trx64`
/// feature; without it this does nothing.
pub fn attach_trx64(machine: &mut Machine, cart_slot: Option<&CartSlotSpec>) -> Result<()> {
    attach_trx64_audio(machine, None, false, false, cart_slot)
}

/// [`attach_trx64`], with the SID's samples going to `sink` when given and an ARMSID in socket 1 when `armsid`
/// (S14 §W4-SID).
fn attach_trx64_audio(
    machine: &mut Machine,
    sink: Option<audio::Sink>,
    armsid: bool,
    sid_thread: bool,
    cart_slot: Option<&CartSlotSpec>,
) -> Result<()> {
    #[cfg(feature = "trx64")]
    {
        let mut c64 = c64_bridge::Trx64Backend::new(&machine.cfg.rom_dir);
        c64.set_sid_socket1(armsid);
        if let Some(sink) = sink {
            // S20: with a device listening the engines, the mixing and the sink move to the SID worker.
            let rate = sink.rate();
            if sid_thread && sink.has_device() {
                c64.set_audio_threaded(rate, Box::new(sink));
            } else {
                c64.set_audio(rate, Box::new(sink));
            }
        }
        if let Some(spec) = cart_slot {
            let decode = c64_bridge::FlashDecode::parse(&spec.flash_decode).map_err(|e| anyhow!("--cart-slot: {e}"))?;
            let crt = spec.read()?;
            let info = c64.insert_cart(&crt, decode).map_err(|e| anyhow!("--cart-slot {}: {e}", spec.path.display()))?;
            eprintln!(
                "cart-slot: {} {:?} ({}, CRT type {}, {} banks, model {}), mode {}",
                spec.path.display(),
                info.name,
                info.family,
                info.hw_type,
                info.banks,
                info.model,
                spec.mode()
            );
            c64.set_log_cart(machine.cfg.log.cart);
        }
        // S15: TRX64 carries the UCI block on its `u64` profile, so the firmware may start its UCI task.
        let has_uci = ue2_core::c64host::C64Backend::has_uci(&c64);
        // S16: Ultimate Audio is UE2's own block, not TRX64's, but it is reached through the C64 bus, so it needs the
        // backend all the same.
        let has_sampler = ue2_core::c64host::C64Backend::has_sampler(&c64);
        machine.attach_c64(Box::new(c64));
        // W4-CART: the bridge emulates the GMOD2 EEPROM behind 0x1004C000, which the firmware only uses with
        // CAPAB_EEPROM (itu.h:71; c64_crt.cc:214, 272, 746).
        // A word given with `--caps` is the user's: the bits below are not OR'd into it, so a machine whose FPGA
        // lacks one of these features can be modelled (and a bit can be forced on by naming it in the word).
        let own_caps = !machine.cfg.capabilities_explicit;
        if let Some(itu) = machine.bus.io.get_mut::<ue2_core::devices::itu::Itu>().filter(|_| own_caps) {
            itu.capabilities |= c64_bridge::CAPAB_EEPROM;
            // Without CAPAB_COMMAND_INTF the firmware starts no "UCI Server" task (command_intf.cc:44) and offers no
            // "Command Interface" setting (c64.cc:311,328). `--c64 none` keeps the capability word it had.
            if has_uci {
                itu.capabilities |= c64_bridge::CAPAB_COMMAND_INTF;
            }
            // Without CAPAB_SAMPLER the firmware never maps `$DF20-$DFFF` (c64.cc:319-326), offers no "Play MOD"
            // (filetype_reu.cc:41-45) and answers `/v1/runners:modplay` with 501 (docs/specs/S16-ultimate-audio.md).
            if has_sampler {
                itu.capabilities |= c64_bridge::CAPAB_SAMPLER;
            }
        }
    }
    #[cfg(not(feature = "trx64"))]
    let _ = (machine, sink, armsid, sid_thread, cart_slot);
    Ok(())
}

/// Run without a window: script, control server, or plain run for `max_seconds`.
///
/// The emulation thread prints the machine stats when it ends; this adds the host MIPS of the last interval.
pub fn run_headless(cfg: MachineConfig, opts: RunOptions) -> Result<()> {
    let emu = spawn(cfg, &opts)?;
    let driven = drive(&emu.ctl, &opts);
    let _ = emu.ctl.commands.send(Command::Quit);
    let finished = emu.join.join().map_err(|_| anyhow!("the emulation thread panicked"))?;
    eprintln!(
        "{} MIPS, {} % of realtime (last interval)",
        emu.mips.load(Ordering::Relaxed),
        emu.speed_pct.load(Ordering::Relaxed)
    );
    finished.and(driven)
}

/// Headless driver: the control server when requested, then the script or the wall-clock wait.
fn drive(ctl: &ControlHandle, opts: &RunOptions) -> Result<()> {
    let _server = opts.control.as_deref().map(|addr| control::serve(ctl.clone(), addr)).transpose()?;
    if let Some(script) = &opts.script {
        return control::run_script(ctl, script);
    }
    let limit = opts.max_seconds.map(Duration::try_from_secs_f64).transpose().context("--max-seconds")?;
    let start = Instant::now();
    while ctl.running.load(Ordering::SeqCst) && limit.is_none_or(|limit| start.elapsed() < limit) {
        thread::sleep(HEADLESS_POLL);
    }
    Ok(())
}

/// Clears `ControlHandle::running` when the emulation thread ends, including by panic.
struct RunningFlag(Arc<AtomicBool>);

impl Drop for RunningFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The emulation thread's side of the shared handle state.
struct EmuThread {
    display: Arc<Mutex<DisplaySnapshot>>,
    now_ms: Arc<AtomicU64>,
    console: Arc<Mutex<ConsoleLog>>,
    mips: Arc<AtomicU64>,
    speed_pct: Arc<AtomicU64>,
    speed: Speed,
}

impl EmuThread {
    fn run(
        &self,
        mut machine: Machine,
        mut net: Option<net::Backend>,
        mut usb_dirs: UsbDirs,
        mut cart: CartSlot,
        commands: &Receiver<Command>,
        mut gdb: Option<GdbServer>,
        #[allow(unused_mut, unused_variables)] mut vice: Option<ViceServer>,
    ) -> Result<()> {
        let mut pacer = Pacer::new(machine.now_ms());
        let mut next_display_ms = 0;
        let mut shown_frame = 0;
        let mut mips_mark = (Instant::now(), machine.cpu.insns, machine.now_ms());
        let mut stdout = std::io::stdout();
        let mut inputs = InputTimeline::default();
        let mut monitor = MonitorState::default();
        // Emulated ms of the quit request: the machine keeps running while a --usb-dir guest write is recent.
        let mut quit_at: Option<u64> = None;
        loop {
            // A slice ends at the next timed input, so holds last exactly their emulated length.
            // S23 M3: `fw halt` holds the firmware, and with it the whole machine — the C64 only advances while
            // the firmware drives it. Commands are still served, or there would be no way back.
            if monitor.firmware_held() {
                #[cfg(feature = "trx64")]
                if let Some(server) = vice.as_mut() {
                    server.poll(&mut machine, &mut monitor.ours);
                }
                if apply_commands(&mut machine, commands, &mut inputs, &mut usb_dirs, &mut cart, &mut monitor) {
                    usb_dirs.finish(&mut machine, true);
                    cart.finish(&mut machine);
                    report(&machine);
                    return Ok(());
                }
                thread::sleep(HELD_POLL);
                continue;
            }
            let slice = inputs.slice_insns(machine.bus.now, machine.cfg.clocks_per_insn, SLICE_INSNS);
            let exit = match gdb.as_mut() {
                None => machine.run(slice),
                Some(server) => {
                    let (debugged, exit) = server.run(machine, slice);
                    machine = debugged;
                    let Some(exit) = exit else {
                        usb_dirs.finish(&mut machine, true);
                        cart.finish(&mut machine);
                        report(&machine);
                        return Ok(());
                    };
                    exit
                }
            };
            if let Some(net) = &mut net {
                net::pump(&mut machine, net);
            }

            let mut console = machine.drain_console();
            if !console.is_empty() {
                // small_printf.cc:236-243 sends "\r\n"; the host terminal wants "\n".
                console.retain(|&b| b != b'\r');
                stdout.write_all(&console).and_then(|()| stdout.flush()).context("writing the console to stdout")?;
                self.console.lock().unwrap_or_else(PoisonError::into_inner).push(&console);
            }

            let quit = apply_commands(&mut machine, commands, &mut inputs, &mut usb_dirs, &mut cart, &mut monitor)
                || quit_at.is_some();
            usb_dirs.poll(&mut machine);

            let now_ms = machine.now_ms();
            cart.poll(&mut machine, now_ms);
            let frame = machine.frame_counter();
            if display_due(frame, shown_frame, now_ms, next_display_ms) {
                *self.display.lock().unwrap_or_else(PoisonError::into_inner) = machine.display();
                shown_frame = frame;
                next_display_ms = now_ms + DISPLAY_PERIOD_MS;
            }
            self.now_ms.store(now_ms, Ordering::Relaxed);

            #[cfg(feature = "trx64")]
            if let Some(server) = vice.as_mut() {
                server.poll(&mut machine, &mut monitor.ours);
            }
            if let RunExit::Breakpoint(pc) = exit {
                monitor.note_stop(format!("breakpoint at {:08x} {}", pc, machine.symbols.format(pc)));
            }
            if let RunExit::Halted(msg) = exit {
                eprintln!("halted: {msg}");
                eprint!("{}", machine.trace_report());
                usb_dirs.finish(&mut machine, false);
                report(&machine);
                bail!("halted: {msg}");
            }
            if quit {
                let since = *quit_at.get_or_insert(now_ms);
                let writing = usb_dirs.writing(now_ms);
                if writing && since == now_ms {
                    eprintln!("usb-dir: waiting for the guest to finish writing before the last sync");
                }
                if !writing || now_ms >= since + EXPLICIT_WAIT_MS {
                    usb_dirs.finish(&mut machine, true);
                    cart.finish(&mut machine);
                    report(&machine);
                    return Ok(());
                }
            }

            if self.speed == Speed::Realtime {
                if let Some(ahead) = pacer.delay(now_ms) {
                    thread::sleep(ahead);
                }
            }

            let elapsed = mips_mark.0.elapsed();
            if elapsed >= MIPS_INTERVAL {
                let (insns, now_ms) = (machine.cpu.insns, machine.now_ms());
                let mips = (insns - mips_mark.1) as f64 / elapsed.as_secs_f64() / 1e6;
                self.mips.store(mips.round() as u64, Ordering::Relaxed);
                let pct = (now_ms - mips_mark.2) as f64 / elapsed.as_secs_f64() / 10.0;
                self.speed_pct.store(pct.round() as u64, Ordering::Relaxed);
                mips_mark = (Instant::now(), insns, now_ms);
            }
        }
    }
}

/// The monitor's own state between lines: the cursors, the selected device and the assemble mode (S23; without
/// the `trx64` feature there is no C64 and no monitor).
#[derive(Default)]
pub struct MonitorState {
    #[cfg(feature = "trx64")]
    session: trx64_monitor::MonitorSession,
    /// What our own verbs keep between lines (S23 §6: `config set` before `config write`).
    #[cfg(feature = "trx64")]
    ours: crate::monitor::State,
}

impl MonitorState {
    /// S23 M3: the firmware is held, so the run loop must not advance the machine.
    #[cfg(feature = "trx64")]
    fn firmware_held(&self) -> bool {
        crate::monitor::run::firmware_held(&self.ours)
    }

    #[cfg(not(feature = "trx64"))]
    fn firmware_held(&self) -> bool {
        false
    }

    /// A stop the run loop took by itself, so the next `status` says why.
    #[cfg(feature = "trx64")]
    fn note_stop(&mut self, reason: String) {
        crate::monitor::run::note_stop(&mut self.ours, reason);
    }

    #[cfg(not(feature = "trx64"))]
    fn note_stop(&mut self, _reason: String) {}

    /// One line, answered as the monitor prints it.
    #[cfg(feature = "trx64")]
    fn exec(&mut self, machine: &mut Machine, line: &str) -> Result<String, String> {
        crate::monitor::exec(machine, &mut self.session, &mut self.ours, line)
    }

    #[cfg(not(feature = "trx64"))]
    fn exec(&mut self, _machine: &mut Machine, _line: &str) -> Result<String, String> {
        Err("this build has no C64, so no monitor (build with the trx64 feature)".into())
    }
}

/// Apply queued commands, then the timed inputs now due; true when the thread should stop (`Quit`, or every
/// sender dropped).
fn apply_commands(
    machine: &mut Machine,
    commands: &Receiver<Command>,
    inputs: &mut InputTimeline,
    usb_dirs: &mut UsbDirs,
    cart: &mut CartSlot,
    monitor: &mut MonitorState,
) -> bool {
    let quit = loop {
        match commands.try_recv() {
            Ok(Command::Input(ev)) => machine.input(ev),
            Ok(Command::Inputs { seq, done }) => inputs.push(seq, done, machine.now_ms()),
            Ok(Command::Usb { req, done }) => usb_dirs.request(machine, req, done),
            Ok(Command::Cart { req, done }) => cart.request(machine, req, done),
            Ok(Command::UsbPlug { port, device, done }) => {
                let port_n = usize::from(port);
                let result = if usb_dirs.owns(port_n) {
                    Err(format!("hub port {port} is a --usb-dir stick; usb-replug unplugs and plugs it"))
                } else {
                    match device {
                        Some(device) => {
                            let what = match &device {
                                ue2_core::devices::usb::UsbDevice::Image(path) => format!("image {}", path.display()),
                                ue2_core::devices::usb::UsbDevice::Keyboard => "keyboard".into(),
                                ue2_core::devices::usb::UsbDevice::Mouse => "mouse".into(),
                            };
                            machine.usb_plug(port_n, device).map(|()| format!("hub port {port}: {what} plugged in"))
                        }
                        None => machine.usb_unplug(port_n).map(|kind| format!("hub port {port}: {kind} unplugged")),
                    }
                };
                let _ = done.send(result);
            }
            Ok(Command::Monitor { line, done }) => {
                let _ = done.send(monitor.exec(machine, &line));
            }
            Ok(Command::Quit) | Err(TryRecvError::Disconnected) => break true,
            Err(TryRecvError::Empty) => break false,
        }
    };
    let now_ms = machine.now_ms();
    inputs.advance(now_ms, |ev| machine.input(ev));
    quit
}

/// `--vice-monitor [ADDR]`: the VICE binary monitor server, or None (S23 §8).
#[cfg(feature = "trx64")]
fn vice_server(addr: Option<&str>) -> Result<Option<ViceServer>> {
    let Some(addr) = addr else { return Ok(None) };
    let parsed = addr.parse().with_context(|| format!("--vice-monitor {addr}: not an address"))?;
    ViceServer::bind(parsed).map(Some)
}

#[cfg(not(feature = "trx64"))]
fn vice_server(addr: Option<&str>) -> Result<Option<ViceServer>> {
    if addr.is_some() {
        anyhow::bail!("--vice-monitor needs a C64: this build has none (build with the trx64 feature)");
    }
    Ok(None)
}

/// Without the C64 there is no VICE monitor, and the option is refused rather than ignored.
#[cfg(not(feature = "trx64"))]
pub struct ViceServer;

/// Final machine report on stderr; stdout carries the firmware console.
fn report(machine: &Machine) {
    eprintln!("{}", machine.stats());
    if let Some(summary) = machine.unmapped_summary() {
        eprint!("{summary}");
    }
}

/// Realtime pacing: keeps emulated time at most `PACE_SLACK_MS` ahead of the wall clock since the anchor.
struct Pacer {
    anchor: Instant,
    anchor_emu_ms: u64,
}

impl Pacer {
    fn new(emu_ms: u64) -> Self {
        Pacer { anchor: Instant::now(), anchor_emu_ms: emu_ms }
    }

    /// Sleep needed now that emulation reached `emu_ms`.
    fn delay(&mut self, emu_ms: u64) -> Option<Duration> {
        let wall = self.anchor.elapsed();
        self.delay_at(emu_ms, wall)
    }

    /// Sleep needed at `wall` since the anchor. Re-anchors when emulation lags by more than `PACE_MAX_LAG_MS`.
    fn delay_at(&mut self, emu_ms: u64, wall: Duration) -> Option<Duration> {
        let emu = emu_ms - self.anchor_emu_ms;
        let wall_ms = wall.as_millis() as u64;
        if emu > wall_ms + PACE_SLACK_MS {
            return Some(Duration::from_millis(emu - wall_ms));
        }
        if wall_ms > emu + PACE_MAX_LAG_MS {
            *self = Pacer::new(emu_ms);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_follows_the_vic_frame_counter() {
        // A counting C64: a new picture publishes, the same one does not, whatever the time.
        assert!(display_due(7, 6, 0, 100));
        assert!(!display_due(7, 7, 500, 100));
        // No counter (`--c64 none`): every DISPLAY_PERIOD_MS.
        assert!(!display_due(0, 0, 99, 100));
        assert!(display_due(0, 0, 100, 100));
    }

    #[test]
    fn pacer_sleeps_when_ahead_and_reanchors_when_far_behind() {
        let mut pacer = Pacer::new(0);
        assert_eq!(pacer.delay_at(10, Duration::from_millis(9)), None, "within the slack");
        assert_eq!(pacer.delay_at(10, Duration::from_millis(5)), Some(Duration::from_millis(5)));
        assert_eq!(pacer.delay_at(10, Duration::from_millis(50)), None);
        assert_eq!(pacer.anchor_emu_ms, 0);
        assert_eq!(pacer.delay_at(10, Duration::from_millis(500)), None);
        assert_eq!(pacer.anchor_emu_ms, 10, "backlog dropped");
    }

    /// S19 §8.4: with TRX64 attached the firmware and the C64 run to the same state with the idle skip on and off.
    /// `UE2_IDLE_SKIP_SECONDS` sets the emulated length (default 3).
    #[cfg(feature = "trx64")]
    #[test]
    fn idle_skip_leaves_a_run_with_the_c64_unchanged() {
        use ue2_core::devices::c64::C64Port;

        let root = std::env::var_os("UE2_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/1541ultimate"));
        let elf = root.join("target/u64ii/riscv/ultimate/result/ultimate.elf");
        if !elf.is_file() || !root.join("roms").is_dir() {
            eprintln!("skipping: firmware ELF or roms not found under {}", root.display());
            return;
        }
        let secs: u64 = std::env::var("UE2_IDLE_SKIP_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
        let run = |skip: bool| {
            let mut cfg = MachineConfig::new(elf.clone(), root.join("roms"));
            cfg.idle_skip = skip;
            let mut m = Machine::new(cfg).unwrap();
            attach_trx64(&mut m, None).unwrap();
            // The RTC counts host seconds; hold it so both runs read the same time (byte 0 locks).
            for off in 0..3 {
                rv32::Bus::write8(&mut m.bus, 0x1006_0400 + off, 0);
            }
            let mut console = Vec::new();
            let mut left = secs * 1000 * 100_000 / m.cfg.clocks_per_insn;
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
        assert_eq!((on.bus.now, on.cpu.insns + on.idle_insns), (off.bus.now, off.cpu.insns));
        assert!(on.bus.ram == off.bus.ram, "DDR differs");
        let frame = |m: &Machine| m.bus.io.get::<C64Port>().and_then(C64Port::frame);
        assert!(frame(&on).is_some() && frame(&on) == frame(&off), "C64 frame differs");
        eprintln!("{secs} s emulated with TRX64: {} executed, {} skipped", on.cpu.insns, on.idle_insns);
    }
}
