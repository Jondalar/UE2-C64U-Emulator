//! ue2emu — U64-II / C64 Ultimate firmware emulator.

mod audio;
mod c64roms;
mod cartslot;
mod config;
mod control;
mod gdb;
mod install;
mod keymap;
#[cfg(feature = "trx64")]
mod monitor;
mod net;
mod runner;
mod usb;
mod usbdir;
#[cfg(feature = "trx64")]
mod vice;
mod window;

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use ue2_core::machine::{LogFlags, MachineConfig};

use runner::{RunOptions, Speed};

#[derive(Parser)]
#[command(name = "ue2emu", version, about = "U64-II / C64 Ultimate firmware emulator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Boot the firmware.
    Run(Box<RunArgs>),
    /// Run a .ue2 updater to populate a flash image, as on hardware (docs/status/install.md).
    Install(install::InstallArgs),
    /// Print every setting of a firmware image as a .cfg with its defaults, for `run --settings`
    /// (docs/specs/S21-settings.md).
    Settings(SettingsArgs),
}

#[derive(Args)]
struct SettingsArgs {
    /// Firmware image: ultimate.elf, ultimate.app or a .ue2 update file
    /// [default: firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf]
    #[arg(long = "firmware", visible_alias = "elf")]
    elf: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SpeedArg {
    Realtime,
    Max,
}

/// S37: `--trx-io`, the board's bitstream.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TrxIoArg {
    /// On when the firmware image is TRX firmware
    Auto,
    /// The TRX bitstream
    On,
    /// Gideon's bitstream: 0x101A0000 unmapped, the ID reads 0
    Off,
}

/// S38: `--ring`, TRX64's rings.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum RingArg {
    On,
    Off,
}

/// The C64 behind the cart/DMA registers (docs/specs/S14-c64-trx64.md).
#[derive(Clone, Copy, ValueEnum)]
enum C64Arg {
    /// Register stub only (T0): no C64 picture, DMA loads time out
    None,
    /// TRX64 (needs the `trx64` cargo feature)
    Trx64,
}

#[derive(Args)]
struct RunArgs {
    /// Flags from a TOML file: each key a long flag without its dashes, relative paths from the file's directory;
    /// flags on the command line replace the file's (docs/examples/ue2emu.example.toml)
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Firmware image: ultimate.elf (with symbols), ultimate.app or a .ue2 update file
    /// [default: firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf]
    #[arg(long = "firmware", visible_alias = "elf")]
    elf: Option<PathBuf>,
    /// Firmware roms directory [default: firmware/1541ultimate/roms]
    #[arg(long)]
    roms: Option<PathBuf>,
    /// Persistent SPI flash image (created erased if missing)
    #[arg(long)]
    flash: Option<PathBuf>,
    /// SD card image
    #[arg(long)]
    sd: Option<PathBuf>,
    /// C64 behind the cart/DMA registers [default: trx64 when built with the trx64 feature, else none]
    #[arg(long, value_enum)]
    c64: Option<C64Arg>,
    /// ITU capability word, hex [default: 34000226]
    #[arg(long, value_parser = parse_hex)]
    caps: Option<u32>,
    /// Emulated 100 MHz clocks per instruction, at least 1 [default: 4]
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    clocks_per_insn: Option<u64>,
    #[arg(long, value_enum, default_value = "realtime")]
    speed: SpeedArg,
    /// Run without a window
    #[arg(long)]
    headless: bool,
    /// Control script to execute (see docs/specs/S08-frontend-control.md)
    #[arg(long)]
    script: Option<PathBuf>,
    /// Serve the control protocol on ADDR, e.g. 127.0.0.1:6400
    #[arg(long)]
    control: Option<String>,
    /// Stop after this many wall-clock seconds
    #[arg(long)]
    max_seconds: Option<f64>,
    /// GDB remote stub address, e.g. 127.0.0.1:1234; the machine waits at reset until the debugger continues
    #[arg(long)]
    gdb: Option<String>,
    /// VICE binary monitor for the C64, for third-party debuggers (default 127.0.0.1:6502; S23 §8)
    #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:6502")]
    vice_monitor: Option<String>,
    /// Logging: unmapped, io, irq, cart (comma separated)
    #[arg(long, value_delimiter = ',')]
    log: Vec<String>,
    /// Which label the board wears: u64ii (default) or c64u, the same hardware with the Bling Board present
    #[arg(long, value_name = "MODEL", value_parser = ["u64ii", "c64u"])]
    board: Option<String>,
    /// TRX64-Ultimate's IO block at 0x101A0000 and the rest of its bitstream: the ID, the interrupt, the RGB565 pixel
    /// layer over the C64, the reSID8 control, 1541-only drives and the expansion port's slot block. auto: on for TRX
    /// firmware; a bare --trx-io is on (docs/specs/S37-trx-pixel-layer.md, S43-trx-resid8-drive-slot.md)
    #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, default_missing_value = "on", default_value = "auto")]
    trx_io: TrxIoArg,
    /// TRX64's time-travel rings on the C64 (delta ring, CPU history, a checkpoint per frame); they record while the
    /// C64 runs at 1 MHz (docs/specs/S38-trx64-ring.md)
    #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, default_missing_value = "on", default_value = "on")]
    ring: RingArg,
    /// FPGA blocks the bitstream lacks: by their TRX_FEATURES names reu, sampler, uci, jtag, turbo, apply, streams;
    /// resid8 for the two-SID reDIP (TRX_SID=redip2); core for the whole swappable core; none = all present. Needs --trx-io (docs/specs/S41-core-dev-mode.md)
    #[arg(long, value_name = "LIST")]
    absent: Option<String>,
    /// How an absent block's app window answers: noack (the access never completes) or ack0, for all, or per block as
    /// sampler=noack,uci=ack0,core=noack [default: ack0 for all; for the core that is the host's decoupler, noack a host without one]
    #[arg(long, value_name = "MODE")]
    absent_mode: Option<String>,
    /// A noack access is logged and completed as ack0 instead of halting; the end of the run counts the stalls per
    /// address
    #[arg(long)]
    absent_continue: bool,
    /// The directory the monitor's file verbs (asm-file, ringdump, ringload, ls, cd) are confined to [default: the
    /// current directory]
    #[arg(long, value_name = "DIR")]
    monitor_dir: Option<PathBuf>,
    /// Do not seed the overlay user interface into blank flash config
    #[arg(long)]
    no_overlay_ui: bool,
    /// Firmware settings from a .cfg file (the firmware's own format), written into the flash before the firmware
    /// runs, at every start; repeatable, later files win (docs/specs/S21-settings.md)
    #[arg(long, value_name = "FILE.cfg")]
    settings: Vec<PathBuf>,
    /// Keep running when a firmware fault hook fires
    #[arg(long)]
    no_halt: bool,
    /// Record the last 256 PCs, printed when a fault hook halts the machine (implied by --gdb; about 3 % MIPS)
    #[arg(long)]
    trace: bool,
    /// Execute idle loops instruction by instruction instead of fast-forwarding them to the next device event
    /// (docs/specs/S19-idle-skip.md)
    #[arg(long)]
    no_idle_skip: bool,
    #[command(flatten)]
    net: net::NetArgs,
    #[command(flatten)]
    usb: usb::UsbArgs,
    #[command(flatten)]
    audio: audio::AudioArgs,
    #[command(flatten)]
    c64_roms: c64roms::C64RomsArgs,
    /// A cartridge in the physical expansion port, served independently of the internal cartridge:
    /// FILE.crt[,rw|,save=OUT.crt][,flash-decode=11|15|both] (docs/status/cart-slot.md)
    #[arg(long, value_name = "SPEC")]
    cart_slot: Option<cartslot::CartSlotSpec>,
    /// C64 matrix keys held from power-on until the control command `release` lets them go, e.g. `cbm` or
    /// `cbm+z` (the firmware scans the keyboard once at boot, u64_config.cc:965)
    #[arg(long, value_name = "KEYS", value_parser = control::keys_by_names)]
    hold_key: Option<Vec<keymap::MatrixKey>>,
}

fn parse_hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

/// Whether to attach TRX64 as the C64: `--c64`, else the build's default (`run` and `install`).
fn c64_selected(arg: Option<C64Arg>) -> Result<bool> {
    Ok(match arg {
        None => cfg!(feature = "trx64"),
        Some(C64Arg::None) => false,
        Some(C64Arg::Trx64) if cfg!(feature = "trx64") => true,
        Some(C64Arg::Trx64) => bail!("--c64 trx64: this ue2emu was built without the trx64 feature"),
    })
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn main() -> Result<()> {
    let argv = config::with_file(Cli::command(), std::env::args_os().collect())?;
    match Cli::parse_from(argv).cmd {
        Cmd::Run(args) => run(*args),
        Cmd::Install(args) => install::install(args),
        Cmd::Settings(args) => settings(args),
    }
}

fn default_firmware() -> PathBuf {
    repo_root().join("firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf")
}

/// `ue2emu settings`: the image's settings as a .cfg on stdout.
fn settings(a: SettingsArgs) -> Result<()> {
    let elf = a.elf.unwrap_or_else(default_firmware);
    let mut ram = vec![0; ue2_core::bus::RAM_SIZE];
    let fw = ue2_core::loader::load_firmware(&elf, &mut ram)?;
    let tables = ue2_core::settings::tables(&ram, &fw.segments);
    let stores = ue2_core::settings::stores(&tables);
    print!("{}", ue2_core::settings::template(&stores, &elf.display().to_string()));
    Ok(())
}

fn run(a: RunArgs) -> Result<()> {
    let elf = a.elf.unwrap_or_else(default_firmware);
    let roms = a.roms.unwrap_or_else(|| repo_root().join("firmware/1541ultimate/roms"));

    let mut cfg = MachineConfig::new(elf, roms);
    if let Some(caps) = a.caps {
        cfg.capabilities = caps;
        cfg.capabilities_explicit = true;
    }
    if let Some(c) = a.clocks_per_insn {
        cfg.clocks_per_insn = c;
    }
    cfg.flash_image = a.flash;
    cfg.sd_image = a.sd;
    cfg.overlay_ui = !a.no_overlay_ui;
    if a.board.as_deref() == Some("c64u") {
        cfg.board = ue2_core::devices::board::Board::C64U;
    }
    cfg.trx_io = match a.trx_io {
        TrxIoArg::Auto => ue2_core::TrxIo::Auto,
        TrxIoArg::On => ue2_core::TrxIo::On,
        TrxIoArg::Off => ue2_core::TrxIo::Off,
    };
    cfg.absent = ue2_core::absent::AbsentConfig::parse(a.absent.as_deref(), a.absent_mode.as_deref(), a.absent_continue)
        .map_err(|e| anyhow::anyhow!(e))?;
    cfg.halt_on_fault = !a.no_halt;
    cfg.trace = a.trace || a.gdb.is_some();
    cfg.idle_skip = !a.no_idle_skip;
    cfg.settings = a.settings;
    let mut log = LogFlags::default();
    for flag in &a.log {
        match flag.as_str() {
            "unmapped" => log.unmapped = true,
            "io" => log.io = true,
            "irq" => log.irq = true,
            "cart" => log.cart = true,
            other => bail!("unknown --log flag '{other}' (expected unmapped, io, irq, cart)"),
        }
    }
    cfg.log = log;
    let net = net::configure(a.net, &mut cfg)?;
    let (usb_dirs, usb_dir_work) = usb::configure(a.usb, &mut cfg)?;
    let c64 = c64_selected(a.c64)?;
    if a.cart_slot.is_some() && !c64 {
        bail!("--cart-slot needs the TRX64 C64 (--c64 trx64)");
    }
    let audio = audio::configure(a.audio, a.headless);
    // Before the machine opens the image.
    a.c64_roms.apply(cfg.flash_image.as_deref(), cfg.capabilities, &cfg.rom_dir)?;

    let opts = RunOptions {
        speed: match a.speed {
            SpeedArg::Realtime => Speed::Realtime,
            SpeedArg::Max => Speed::Max,
        },
        script: a.script,
        control: a.control,
        max_seconds: a.max_seconds,
        gdb: a.gdb,
        vice_monitor: a.vice_monitor,
        net,
        c64,
        audio,
        usb_dirs,
        usb_dir_work,
        cart_slot: a.cart_slot,
        hold_keys: a.hold_key.unwrap_or_default(),
        ring: matches!(a.ring, RingArg::On),
        monitor_dir: match a.monitor_dir {
            Some(dir) => {
                if !dir.is_dir() {
                    bail!("--monitor-dir {}: not a directory", dir.display());
                }
                dir
            }
            None => std::env::current_dir()?,
        },
    };

    if a.headless {
        runner::run_headless(cfg, opts)
    } else {
        window::run_window(cfg, opts)
    }
}
