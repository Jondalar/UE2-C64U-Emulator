//! ue2emu — U64-II / C64 Ultimate firmware emulator.

mod audio;
mod c64roms;
mod cartslot;
mod config;
mod control;
mod gdb;
mod install;
mod keymap;
mod net;
mod runner;
mod usb;
mod usbdir;
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
    /// ITU capability word, hex [default: 34000222]
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
    /// Logging: unmapped, io, irq (comma separated)
    #[arg(long, value_delimiter = ',')]
    log: Vec<String>,
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
            other => bail!("unknown --log flag '{other}' (expected unmapped, io, irq)"),
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
        net,
        c64,
        audio,
        usb_dirs,
        usb_dir_work,
        cart_slot: a.cart_slot,
    };

    if a.headless {
        runner::run_headless(cfg, opts)
    } else {
        window::run_window(cfg, opts)
    }
}
