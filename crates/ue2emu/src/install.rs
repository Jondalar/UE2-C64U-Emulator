//! `ue2emu install`: populate the SPI flash image the way hardware gets its contents, by running a `.ue2` updater
//! (software/application/update_u2p/update_u64ii.cc) inside the emulator. Results: docs/status/install.md.
//!
//! - The updater itself is loaded (`loader::load_updater`, entry 0x03000000) with the normal device set. No
//!   overlay-UI page is seeded, so the flash receives exactly what the updater writes; the next `run` seeds it.
//! - With a C64 present (`C64::exists`, c64.cc:352-360; the cart registers report PHI2) the updater's user
//!   interface is the C64 text screen at $0400 and the CIA1 keyboard (update_common.h:198-222, c64.cc:144-145).
//!   `--c64 trx64` (the default with the `trx64` feature) runs it on TRX64, `--c64 none` on the T0 stub, whose CIA1
//!   port B scans the same keys. Both are read through `C64Port::dma_peek` and fed through `Machine::input`.
//!   Its popups (`UIPopup`, ui_elements.cc:29-126) are read from that screen and answered with their button keys
//!   (userinterface.cc:646-667).
//! - The updater ends by switching the machine off through the ESP32 (`turn_off`, update_common.h:53-76). The run
//!   stops at that request, writes the flash image and checks the application slot against the update file.
//! - The machine is the board model of `--board`, `--caps` and `--flash-chip`, as for `run` (S47 §4).
//! - A MultiCore `.txu` holds no updater: its parts are checked as the host app checks them and written as it writes
//!   them (`ue2_core::txu`, docs/specs/S46-multicore-txu.md §4). No machine runs.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use clap::Args;
use ue2_core::bus::SystemBus;
use ue2_core::devices::{self, c64::C64Port, flash::SpiFlash, wifi::Wifi};
use ue2_core::host::HostInput;
use ue2_core::loader::{self, ImageFormat};
use ue2_core::machine::{Machine, MachineConfig, RunExit};
use ue2_core::symbols::Symbols;
use ue2_core::txu::{self, Txu};

use crate::keymap;

#[derive(Args)]
pub struct InstallArgs {
    /// Update file (update.ue2, Commodore .ue2), the updater's update.elf, which adds symbols, or a MultiCore .txu,
    /// whose parts are written as its host app writes them
    #[arg(long)]
    update: PathBuf,
    /// SPI flash image to write (created erased if missing)
    #[arg(long)]
    flash: PathBuf,
    /// C64 behind the cart/DMA registers [default: trx64 when built with the trx64 feature, else none]
    #[arg(long, value_enum)]
    c64: Option<crate::C64Arg>,
    #[command(flatten)]
    model: crate::ModelArgs,
    /// Firmware roms directory, seeds the TRX64 C64's ROMs [default: firmware/1541ultimate/roms]
    #[arg(long)]
    roms: Option<PathBuf>,
    /// Answer the updater's questions instead of asking on the terminal: Yes, except No to the destructive
    /// "Reformat Flash Disk?" and "Reset Configuration?" (see --reformat-flash, --reset-config)
    #[arg(long)]
    yes: bool,
    /// Answer Yes to "Reformat Flash Disk?": erases everything on /flash (ROMs, carts, web UI, your files)
    #[arg(long)]
    reformat_flash: bool,
    /// Answer Yes to "Reset Configuration?": resets all saved settings
    #[arg(long)]
    reset_config: bool,
    /// Give up after this many emulated seconds without the updater's power-off request [default: 600]
    #[arg(long)]
    timeout: Option<u64>,
    #[command(flatten)]
    c64_roms: crate::c64roms::C64RomsArgs,
    /// A cartridge in the physical expansion port while the updater runs: FILE.crt[,flash-decode=11|15|both]; install
    /// never writes it back (docs/status/cart-slot.md)
    #[arg(long, value_name = "SPEC")]
    cart_slot: Option<crate::cartslot::CartSlotSpec>,
}

/// Instructions per slice between checks (4 ms emulated at the default 4 clocks per instruction).
const SLICE_INSNS: u64 = 100_000;
/// Emulated interval between screen scans for popups; `Keyboard_C64::getch` polls every 4 ticks (20 ms,
/// keyboard_c64.cc:312-319).
const SCAN_MS: u64 = 100;
/// Key hold and release gap, as the control language's `key` (control.rs; keyboard_c64.cc:256-258, 286-293).
const KEY_MS: u64 = 80;
const RELEASE_MS: u64 = 40;
/// A popup still on screen this long after its answer gets the key again.
const RETRY_MS: u64 = 2000;
/// The updater's `Screen_MemMappedCharMatrix`: 40 × 25 characters at C64 $0400 (c64.cc:145, c64.h:192).
const SCREEN: u16 = 0x0400;
const COLS: usize = 40;
const ROWS: usize = 25;
/// `flash_buffer_at` programs images page by page from a page boundary (prog_flash.cc:52-54; 256-byte pages,
/// s25fl_l_flash.cc:79-83).
const FLASH_PAGE: usize = 0x100;
/// Xilinx bitstream sync word, near the start of the runtime FPGA image the updater writes at 0.
const XILINX_SYNC: [u8; 4] = [0xAA, 0x99, 0x55, 0x66];

/// Popup buttons in `UserInterface::popup` order with their names and keys (userinterface.cc:648-649).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Ok,
    Yes,
    No,
    All,
    Cancel,
}

impl Button {
    const BUTTONS: [Button; 5] = [Button::Ok, Button::Yes, Button::No, Button::All, Button::Cancel];

    fn name(self) -> &'static str {
        match self {
            Button::Ok => "Ok",
            Button::Yes => "Yes",
            Button::No => "No",
            Button::All => "All",
            Button::Cancel => "Cancel",
        }
    }

    fn key(self) -> char {
        match self {
            Button::Ok => 'o',
            Button::Yes => 'y',
            Button::No => 'n',
            Button::All => 'a',
            Button::Cancel => 'c',
        }
    }
}

/// A popup read from the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Popup {
    /// Message rows, joined with '\n'.
    message: String,
    buttons: Vec<Button>,
}

/// Emulated seconds without the updater's power-off request before `install` gives up.
const TIMEOUT_S: u64 = 600;

/// Run the updater in `args.update` against `args.flash` until it switches the machine off; a `.txu` is written by
/// [`install_txu`].
pub fn install(args: InstallArgs) -> Result<()> {
    let data = std::fs::read(&args.update).with_context(|| format!("reading {}", args.update.display()))?;
    if txu::is_txu(&data) {
        return install_txu(&args, &data);
    }
    let mut machine = build(&args)?;
    let answers = Answers { yes: args.yes, reformat_flash: args.reformat_flash, reset_config: args.reset_config };
    let result = drive(&mut machine, answers, args.timeout.unwrap_or(TIMEOUT_S).saturating_mul(1000));
    eprintln!("{}", machine.stats());
    eprint!("--- C64 screen ---\n{}--- C64 screen ---\n", screen_text(&c64_screen(&machine)?));
    machine.bus.io.get_mut::<SpiFlash>().context("no SPI flash installed")?.flush()?;
    let (capabilities, chip, roms) = (machine.cfg.capabilities, machine.cfg.flash_chip, machine.cfg.rom_dir.clone());
    // Close the image before --c64-roms writes it.
    drop(machine);
    result?;
    eprintln!("install: the updater switched the machine off; flash image {} written", args.flash.display());
    verify(&args.update, &args.flash)?;
    // After the updater, which offers to reformat a /flash that holds files (update_common.h:228-257).
    args.c64_roms.apply(Some(&args.flash), capabilities, chip, &roms)
}

/// S46 §4: a `.txu` into the flash image as the host app's `.txu` flow leaves it. The checks of `txu_stage` come
/// first; if one fails, nothing is written. The written regions are read back.
fn install_txu(args: &InstallArgs, data: &[u8]) -> Result<()> {
    let name = args.update.display();
    let package = Txu::parse(data).with_context(|| format!("{name}"))?;
    let fpga = package.fpga;
    let roms = args.roms.clone().unwrap_or_else(|| crate::repo_root().join("firmware/1541ultimate/roms"));
    let mut cfg = MachineConfig::new(args.update.clone(), roms);
    // Without --caps or --board the FPGA is the package's.
    if !args.model.names_fpga() {
        cfg.capabilities = cfg.capabilities & !crate::CAPAB_FPGA_TYPE | fpga.fpga_type << 28;
    }
    eprintln!("install: {}", args.model.apply(&mut cfg));
    let machine_type = cfg.capabilities >> 28 & 3;
    ensure!(
        machine_type == fpga.fpga_type,
        "{name} is for the {} (FPGA type {}); this machine's FPGA type is {machine_type}{}",
        fpga.name,
        fpga.fpga_type,
        txu::fpga(machine_type).map_or(String::new(), |f| format!(", the {}", f.name))
    );
    let unused: Vec<&str> = [
        (args.c64.is_some(), "--c64"),
        (args.yes, "--yes"),
        (args.reformat_flash, "--reformat-flash"),
        (args.reset_config, "--reset-config"),
        (args.timeout.is_some(), "--timeout"),
        (args.cart_slot.is_some(), "--cart-slot"),
    ]
    .into_iter()
    .filter_map(|(given, flag)| given.then_some(flag))
    .collect();
    if !unused.is_empty() {
        let verb = if unused.len() == 1 { "does" } else { "do" };
        eprintln!("install: {} {verb} nothing for a .txu: no updater runs", unused.join(", "));
    }
    let parts: Vec<String> = package
        .parts
        .iter()
        .map(|p| match p.kind {
            txu::KIND_HOST => format!("host bitstream {} bytes, key {:08x}", p.size, package.host_key),
            _ => format!("app {} bytes", p.size),
        })
        .collect();
    eprintln!("install: {name}: TRX update {} ({}) for the {}: {}", package.version, package.commit, fpga.name, parts.join(", "));
    // txu_check compares these with the running system; here none runs.
    eprintln!(
        "install: host interface: provides {}, needs {} (0: no such part; not checked against the flash)",
        package.iface_provided, package.iface_needed
    );
    package.check(data).with_context(|| format!("{name}: nothing written"))?;
    let (mut image, full_size) = crate::c64roms::read_image(&args.flash)?;
    let original = image.clone();
    package.write_into(data, &mut image);
    let changed = crate::c64roms::write_image(&args.flash, &image, &original, full_size)?;
    eprintln!("install: flash image {}: {changed} changed 4 KiB sector(s) written", args.flash.display());
    let (written, _) = crate::c64roms::read_image(&args.flash)?;
    for part in &package.parts {
        let (addr, bytes) = (part.flash_addr as usize, part.bytes(data));
        ensure!(written[addr..addr + bytes.len()] == *bytes, "flash {addr:#08x}: the read-back differs from {name}");
        match part.kind {
            txu::KIND_HOST => {
                ensure!(
                    written[bytes.len()..fpga.bit_slot as usize].iter().all(|&b| b == 0xFF),
                    "flash: the rest of the bitstream slot is not blank"
                );
                eprintln!(
                    "install: flash {addr:#08x}: host bitstream, {} bytes, key {:08x}; the slot erased up to {:#08x}",
                    bytes.len(),
                    package.host_key,
                    fpga.bit_slot
                );
            }
            _ => eprintln!("install: flash {addr:#08x}: app, {} bytes, identical to {name}", bytes.len()),
        }
    }
    args.c64_roms.apply(Some(&args.flash), cfg.capabilities, cfg.flash_chip, &cfg.rom_dir)
}

/// The normal device set with the updater loaded and no overlay-UI seed.
fn build(args: &InstallArgs) -> Result<Machine> {
    let c64 = crate::c64_selected(args.c64)?;
    if args.cart_slot.is_some() && !c64 {
        bail!("--cart-slot needs the TRX64 C64 (--c64 trx64)");
    }
    // `rom_dir` only seeds the TRX64 ROMs here; the window and `png` do not run.
    let roms = args.roms.clone().unwrap_or_else(|| crate::repo_root().join("firmware/1541ultimate/roms"));
    let mut cfg = MachineConfig::new(args.update.clone(), roms);
    eprintln!("install: {}", args.model.apply(&mut cfg));
    cfg.flash_image = Some(args.flash.clone());
    cfg.overlay_ui = false;
    let mut bus = SystemBus::new();
    devices::install_all(&mut bus.io, &cfg);
    let fw = loader::load_updater(&cfg.elf, &mut bus.ram)?;
    let symbols = match fw.format {
        ImageFormat::Elf => Symbols::from_elf(&cfg.elf)?,
        _ => Symbols::empty(),
    };
    let c64_name = if c64 { "trx64" } else { "none" };
    eprintln!("install: {} ({:?}), entry {:#010x}, C64 {c64_name}", cfg.elf.display(), fw.format, fw.entry);
    let mut machine = Machine::from_parts(cfg, bus, fw.entry, symbols);
    if c64 {
        crate::runner::attach_trx64(&mut machine, args.cart_slot.as_ref())?;
    }
    Ok(machine)
}

/// Run until the ESP32 stub reports a power request, answering popups on the way.
fn drive(machine: &mut Machine, answers: Answers, timeout_ms: u64) -> Result<()> {
    let mut answered: Option<(Popup, u64)> = None;
    let mut next_scan = 0;
    loop {
        slice(machine)?;
        let ctrl = &mut machine.bus.io.get_mut::<Wifi>().context("no WiFi UART installed")?.ctrl;
        if let Some(event) = ctrl.power_event.take() {
            eprintln!("install: power request {event:?} after {:.3} s emulated", machine.now_ms() as f64 / 1000.0);
            return Ok(());
        }
        let now = machine.now_ms();
        ensure!(now < timeout_ms, "no power request from the updater within {} s emulated", timeout_ms / 1000);
        if now < next_scan {
            continue;
        }
        next_scan = now + SCAN_MS;
        let Some(popup) = find_popup(&c64_screen(machine)?) else {
            answered = None;
            continue;
        };
        if answered.as_ref().is_some_and(|(seen, at)| *seen == popup && now < at + RETRY_MS) {
            continue;
        }
        let button = choose(&popup, answers)?;
        eprintln!("install: updater asks {:?} -> {}", popup.message, button.name());
        press(machine, button.key())?;
        answered = Some((popup, machine.now_ms()));
    }
}

/// One run slice; the firmware console goes to stdout and a fault hook ends the install.
fn slice(machine: &mut Machine) -> Result<()> {
    if let RunExit::Halted(msg) = machine.run(SLICE_INSNS) {
        eprint!("{}", machine.trace_report());
        bail!("halted: {msg}");
    }
    let mut console = machine.drain_console();
    if !console.is_empty() {
        // small_printf.cc:236-243 sends "\r\n"; the host terminal wants "\n".
        console.retain(|&b| b != b'\r');
        let mut out = std::io::stdout().lock();
        out.write_all(&console).and_then(|()| out.flush()).context("writing the console to stdout")?;
    }
    Ok(())
}

/// Run for `ms` emulated milliseconds.
fn run_ms(machine: &mut Machine, ms: u64) -> Result<()> {
    let until = machine.now_ms() + ms;
    while machine.now_ms() < until {
        slice(machine)?;
    }
    Ok(())
}

/// Tap the key that types `c` on the keyboard both CIA1 and the overlay scanner see.
fn press(machine: &mut Machine, c: char) -> Result<()> {
    let key = keymap::key_for_char(c).ok_or_else(|| anyhow!("no matrix key types {c:?}"))?;
    machine.input(HostInput::Key { row: key.row, col: key.col, down: true });
    run_ms(machine, KEY_MS)?;
    machine.input(HostInput::Key { row: key.row, col: key.col, down: false });
    run_ms(machine, RELEASE_MS)
}

/// How to answer the updater's questions (`--yes`, `--reformat-flash`, `--reset-config`).
#[derive(Clone, Copy, Debug, Default)]
struct Answers {
    yes: bool,
    reformat_flash: bool,
    reset_config: bool,
}

/// Explicit answer to a destructive question (update_common.h:244-253 reformat, the updater's config reset), if any:
/// Yes only with its own flag; with `--yes` alone, No.
fn destructive_answer(popup: &Popup, answers: Answers) -> Option<Button> {
    let flag = if popup.message.contains("Reformat Flash") {
        answers.reformat_flash
    } else if popup.message.contains("Reset Configuration") {
        answers.reset_config
    } else {
        return None;
    };
    let want = if flag { Button::Yes } else if answers.yes { Button::No } else { return None };
    popup.buttons.contains(&want).then_some(want)
}

/// The button to press: the only one, the destructive-question policy, Yes with `--yes`, otherwise the answer typed on
/// the terminal.
fn choose(popup: &Popup, answers: Answers) -> Result<Button> {
    if let [only] = popup.buttons[..] {
        return Ok(only);
    }
    if let Some(button) = destructive_answer(popup, answers) {
        return Ok(button);
    }
    if answers.yes {
        return popup
            .buttons
            .iter()
            .copied()
            .find(|&b| b == Button::Yes)
            .ok_or_else(|| anyhow!("--yes cannot answer {:?}: it has no Yes button", popup.message));
    }
    let keys: Vec<String> = popup.buttons.iter().map(|b| b.key().to_string()).collect();
    let stdin = std::io::stdin();
    loop {
        eprint!("updater asks {:?} [{}] ", popup.message, keys.join("/"));
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            bail!("stdin closed while the updater asks {:?} (--yes answers Yes)", popup.message);
        }
        let answer = line.trim().to_ascii_lowercase();
        let pick = popup.buttons.iter().copied().find(|b| {
            answer.chars().eq([b.key()]) || answer == b.name().to_ascii_lowercase()
        });
        if let Some(button) = pick {
            return Ok(button);
        }
    }
}

/// The C64 text screen, one row of character codes per line, reverse video (bit 7, screen.cc:291-294) removed.
fn c64_screen(machine: &Machine) -> Result<Vec<[u8; COLS]>> {
    let port = machine.bus.io.get::<C64Port>().context("no C64 port installed")?;
    Ok((0..ROWS).map(|y| std::array::from_fn(|x| port.dma_peek(SCREEN + (y * COLS + x) as u16) & 0x7F)).collect())
}

/// Printable ASCII as is (the updater writes ASCII codes, screen.cc:291-294); line drawing and other codes as '#'.
fn screen_text(screen: &[[u8; COLS]]) -> String {
    screen
        .iter()
        .map(|row| {
            let line: String = row.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '#' }).collect();
            format!("{}\n", line.trim_end())
        })
        .collect()
}

/// The popup on the screen, found by its layout (`UIPopup::init` / `draw_buttons`, ui_elements.cc:65-123;
/// `Window::draw_border`, screen.cc:463-484): a row holding only button names between two equal vertical border
/// characters, a blank row inside the border above it, and above that the message rows up to the top corners.
fn find_popup(screen: &[[u8; COLS]]) -> Option<Popup> {
    (2..screen.len()).find_map(|y| popup_at(screen, y))
}

/// The popup whose button row is row `y`, trying every column as its left border. Text left of the window may touch
/// the border, so the row is not split into words.
fn popup_at(screen: &[[u8; COLS]], y: usize) -> Option<Popup> {
    (0..COLS).find_map(|x1| {
        let (buttons, x2) = button_row(&screen[y], x1)?;
        let border = screen[y][x1];
        let bordered = |yy: usize| screen[yy][x1] == border && screen[yy][x2] == border;
        let inner = |yy: usize| screen_text(&[screen[yy]])[x1 + 1..x2].trim().to_owned();
        if !bordered(y - 1) || !inner(y - 1).is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut top = y - 2;
        while bordered(top) {
            lines.push(inner(top));
            top = top.checked_sub(1)?;
        }
        if lines.is_empty() || screen[top][x1] == b' ' {
            return None;
        }
        lines.reverse();
        Some(Popup { message: lines.join("\n"), buttons })
    })
}

/// Button names separated by spaces between the border character at `x1` and its next copy: (buttons, x2).
fn button_row(row: &[u8; COLS], x1: usize) -> Option<(Vec<Button>, usize)> {
    let border = row[x1];
    if border == b' ' {
        return None;
    }
    let mut buttons = Vec::new();
    let mut x = x1 + 1;
    loop {
        x += row.get(x..)?.iter().take_while(|&&b| b == b' ').count();
        match row.get(x)? {
            &b if b == border => return (!buttons.is_empty()).then_some((buttons, x)),
            _ => {
                let len = row[x..].iter().take_while(|&&b| b != b' ' && b != border).count();
                let word = &row[x..x + len];
                buttons.push(Button::BUTTONS.into_iter().find(|b| b.name().as_bytes() == word)?);
                x += len;
            }
        }
    }
}

/// Check the flash image against the update file: a flash page holds the embedded `ultimate.app` and the runtime
/// FPGA image at 0 carries the Xilinx sync word. The slot is searched, not assumed: the upstream updater writes the
/// application to 0x3C0000 on the XC7A100T layout (update_u64ii.cc:179-180), the Commodore 1.1.0 updater to 0x220000.
fn verify(update: &Path, flash: &Path) -> Result<()> {
    let image = std::fs::read(flash).with_context(|| format!("reading {}", flash.display()))?;
    let data = std::fs::read(update).with_context(|| format!("reading {}", update.display()))?;
    let app = loader::embedded_app(&data).context("the update file embeds no ultimate.app to compare")?;
    let slot = (0..image.len().saturating_sub(app.len()))
        .step_by(FLASH_PAGE)
        .find(|&at| image[at..at + app.len()] == *app)
        .context("no flash page starts the update file's ultimate.app")?;
    eprintln!("install: flash {slot:#08x}: ultimate.app, {} bytes, identical to the update file", app.len());
    let sync = image[..0x1000].windows(4).position(|w| w == XILINX_SYNC);
    let sync = sync.context("flash 0x000000: no FPGA bitstream sync word in the first 4 KiB")?;
    eprintln!("install: flash 0x000000: FPGA bitstream, sync word at {sync:#x}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-ins for the charset's line-drawing codes (CHR_VERTICAL_LINE, CHR_HORIZONTAL_LINE, BORD_* corners).
    const VERTICAL: u8 = 0x1D;
    const HORIZONTAL: u8 = 0x1E;
    const CORNER: u8 = 0x1F;

    fn blank() -> Vec<[u8; COLS]> {
        vec![[b' '; COLS]; ROWS]
    }

    fn put(screen: &mut [[u8; COLS]], x: usize, y: usize, text: &[u8]) {
        screen[y][x..x + text.len()].copy_from_slice(text);
    }

    /// Draw a popup with the geometry of `UIPopup::init` / `draw_buttons` (ui_elements.cc:65-123) and
    /// `Window::draw_border` (screen.cc:463-484).
    fn draw(screen: &mut [[u8; COLS]], message: &str, buttons: &[Button]) {
        let lines: Vec<&str> = message.split('\n').collect();
        let names: String = buttons.iter().map(|b| format!(" {} ", b.name())).collect();
        let width = lines.iter().map(|l| l.len()).max().unwrap().max(names.len());
        let height = lines.len() + 4;
        let (x1, y1) = ((COLS - width) / 2, (ROWS - height) / 2);
        for row in &mut screen[y1..y1 + height] {
            row[x1..x1 + width + 2].fill(b' ');
            row[x1] = VERTICAL;
            row[x1 + width + 1] = VERTICAL;
        }
        for y in [y1, y1 + height - 1] {
            screen[y][x1 + 1..x1 + width + 1].fill(HORIZONTAL);
            (screen[y][x1], screen[y][x1 + width + 1]) = (CORNER, CORNER);
        }
        for (row, line) in lines.iter().enumerate() {
            put(screen, x1 + 1 + (width - line.len()) / 2, y1 + 1 + row, line.as_bytes());
        }
        put(screen, x1 + 1 + (width - names.len()) / 2, y1 + 1 + lines.len() + 1, names.as_bytes());
    }

    /// `ue2emu install ARGS`.
    fn install_args(args: &[&str]) -> InstallArgs {
        #[derive(clap::Parser)]
        struct Cmd {
            #[command(flatten)]
            install: InstallArgs,
        }
        <Cmd as clap::Parser>::try_parse_from(["install"].iter().chain(args)).unwrap().install
    }

    /// A `.bit` file with `idcode` and the AXSS stamp `key` (the walks of cfw.cc and flashfw.cc).
    fn bit(idcode: u32, key: u32) -> Vec<u8> {
        let write = |reg: u32| (1u32 << 29 | 2 << 27 | reg << 13 | 1).to_be_bytes();
        let mut config = vec![0xFF; 8];
        config.extend_from_slice(&0xAA99_5566u32.to_be_bytes());
        for (reg, value) in [(0x0C, idcode), (0x0D, key), (0x04, 0x0D)] {
            config.extend_from_slice(&write(reg));
            config.extend_from_slice(&u32::to_be_bytes(value));
        }
        let mut out = vec![0x00, 0x09, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x00, 0x00, 0x01];
        for key in [b'a', b'b', b'c', b'd'] {
            out.extend_from_slice(&[key, 0, 2, b'x', 0]);
        }
        out.push(b'e');
        out.extend_from_slice(&(config.len() as u32).to_be_bytes());
        out.extend_from_slice(&config);
        out
    }

    /// A one-record `.app` at 0x30000 with `n` data bytes.
    fn app(n: usize) -> Vec<u8> {
        let mut out: Vec<u8> = [0x3_0000u32, n as u32, 0x3_0000].iter().flat_map(|w| w.to_le_bytes()).collect();
        out.extend((0..n).map(|i| i as u8));
        out
    }

    fn package(idcode: u32, app_len: Option<usize>) -> Vec<u8> {
        let host = bit(idcode, 0xC0DE_0001);
        let app = app_len.map(app);
        txu::pack(
            Some(txu::HostIn { bit: &host, iface_provided: 1 }),
            app.as_deref().map(|app| txu::AppIn { app, iface_needed: 1 }),
            "t239.1",
            "b4730f57",
        )
    }

    /// S46 T3: `install --update X.txu` writes the parts as `fw_write_parts` does, into a missing (erased) flash and over
    /// a filled one; a failing check writes nothing; the FPGA is the package's unless --caps or --board say otherwise.
    #[test]
    fn install_writes_a_txu_like_the_host_app() {
        let tmp = tempfile::tempdir().unwrap();
        let path = |name: &str| tmp.path().join(name).to_str().unwrap().to_owned();
        let data = package(0x0362_C093, Some(4096 * 2 + 12));
        std::fs::write(path("a.txu"), &data).unwrap();
        let parsed = Txu::parse(&data).unwrap();
        let (host, app) = (parsed.host().unwrap().bytes(&data).to_vec(), parsed.app().unwrap().bytes(&data).to_vec());

        install(install_args(&["--update", &path("a.txu"), "--flash", &path("new.bin"), "--yes"])).unwrap();
        let image = std::fs::read(path("new.bin")).unwrap();
        assert_eq!(image.len(), 16 << 20);
        assert_eq!((&image[..host.len()], &image[0x22_0000..0x22_0000 + app.len()]), (&host[..], &app[..]));
        assert!(image[host.len()..0x22_0000].iter().all(|&b| b == 0xFF));
        assert!(image[0x22_0000 + app.len()..].iter().all(|&b| b == 0xFF));

        // Over a filled flash: an app-only package leaves the bitstream slot and the rest of the app slot.
        let only_app = txu::pack(None, Some(txu::AppIn { app: &app[..], iface_needed: 1 }), "v", "c");
        std::fs::write(path("b.txu"), &only_app).unwrap();
        std::fs::write(path("full.bin"), vec![0u8; 16 << 20]).unwrap();
        install(install_args(&["--update", &path("b.txu"), "--flash", &path("full.bin"), "--board", "c64u"])).unwrap();
        let image = std::fs::read(path("full.bin")).unwrap();
        assert!(image[..0x22_0000].iter().all(|&b| b == 0));
        assert_eq!(image[0x22_0000..0x22_0000 + app.len()], app[..]);
        assert!(image[0x22_0000 + app.len()..0x22_3000].iter().all(|&b| b == 0xFF), "the sectors the app covers");
        assert!(image[0x22_3000..].iter().all(|&b| b == 0));

        // A check that fails writes nothing, not even a new image file.
        std::fs::write(path("c.txu"), package(0x0363_1093, Some(8))).unwrap();
        let err = install(install_args(&["--update", &path("c.txu"), "--flash", &path("none.bin")])).unwrap_err();
        assert!(format!("{err:#}").contains("nothing written: the host part is not a bitstream for this FPGA"), "{err:#}");
        assert!(!tmp.path().join("none.bin").exists());
        // A TXU1 package is the XC7A50T's: an XC7A100T machine is refused.
        let err = install(install_args(&["--update", &path("a.txu"), "--flash", &path("none.bin"), "--caps", "34000226"]));
        let err = format!("{:#}", err.unwrap_err());
        assert!(err.contains("is for the XC7A50T (FPGA type 2); this machine's FPGA type is 3, the XC7A100T"), "{err}");
    }

    /// S47 T5: `install` takes the model options as `run` does.
    #[test]
    fn install_takes_the_model() {
        let args = install_args(&["--update", "u.ue2", "--flash", "f.bin", "--board", "c64u"]);
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        assert_eq!(args.model.apply(&mut cfg), "flash: W25Q128 (--board c64u), FPGA type 2");
        let args = install_args(&["--update", "u.ue2", "--flash", "f.bin", "--caps", "24E40226", "--flash-chip", "w25q128"]);
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        args.model.apply(&mut cfg);
        assert_eq!((cfg.capabilities, cfg.flash_chip), (0x24E4_0226, ue2_core::devices::flash::FlashChip::W25q128));
    }

    #[test]
    fn finds_popups_by_border_and_buttons() {
        let mut screen = blank();
        put(&mut screen, 0, 11, b"Yes  No are just words here");
        put(&mut screen, 0, 13, b"Writing 1541.rom to /flash: No error");
        assert_eq!(find_popup(&screen), None, "unbordered text is no popup");

        draw(&mut screen, "About to update. Continue?", &[Button::Yes, Button::No]);
        let popup = Popup { message: "About to update. Continue?".into(), buttons: vec![Button::Yes, Button::No] };
        assert_eq!(find_popup(&screen), Some(popup));

        let mut screen = blank();
        put(&mut screen, 0, 12, b"Background left and right of the window");
        draw(&mut screen, "Flashing ESP32\nSuccess!", &[Button::Ok]);
        let popup = Popup { message: "Flashing ESP32\nSuccess!".into(), buttons: vec![Button::Ok] };
        assert_eq!(find_popup(&screen), Some(popup));

        let mut screen = blank();
        draw(&mut screen, "Go?", &[Button::Yes, Button::No, Button::Cancel]);
        assert_eq!(find_popup(&screen).map(|p| (p.message, p.buttons.len())), Some(("Go?".into(), 3)));
    }

    #[test]
    fn screen_text_shows_ascii_and_marks_other_codes() {
        let mut screen = blank();
        put(&mut screen, 0, 0, b"OK!");
        screen[1][0] = VERTICAL;
        let text = screen_text(&screen[..2]);
        assert_eq!(text, "OK!\n#\n");
    }

    #[test]
    fn choose_takes_the_only_button_and_yes_with_yes() {
        let yes = Answers { yes: true, ..Answers::default() };
        let ok = Popup { message: "Flashing ESP32 Failed!".into(), buttons: vec![Button::Ok] };
        assert_eq!(choose(&ok, Answers::default()).unwrap(), Button::Ok);
        let update = Popup { message: "About to update. Continue?".into(), buttons: vec![Button::Yes, Button::No] };
        assert_eq!(choose(&update, yes).unwrap(), Button::Yes);
        let odd = Popup { message: "Pick".into(), buttons: vec![Button::All, Button::Cancel] };
        assert!(choose(&odd, yes).is_err());
        let reset =
            Popup { message: "Reset Configuration? (Recommended)".into(), buttons: vec![Button::Yes, Button::No] };
        let reformat = Popup { message: "Reformat Flash Disk?".into(), buttons: vec![Button::Yes, Button::No] };
        assert_eq!(choose(&reset, yes).unwrap(), Button::No, "--yes keeps the settings");
        assert_eq!(choose(&reformat, yes).unwrap(), Button::No, "--yes keeps /flash");
        let destroy = Answers { yes: true, reformat_flash: true, reset_config: true };
        assert_eq!(choose(&reset, destroy).unwrap(), Button::Yes);
        assert_eq!(choose(&reformat, destroy).unwrap(), Button::Yes);
        let only_reformat = Answers { reformat_flash: true, ..Answers::default() };
        assert_eq!(choose(&reformat, only_reformat).unwrap(), Button::Yes, "flag answers without --yes");
        for button in Button::BUTTONS {
            assert!(keymap::key_for_char(button.key()).is_some_and(|key| !key.shift), "{button:?}");
        }
    }
}
