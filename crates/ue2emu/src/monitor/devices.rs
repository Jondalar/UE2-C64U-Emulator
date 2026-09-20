//! The Ultimate's own hardware, as the firmware's registers stand right now (S23 §5).
//!
//! Every number here is read the way the firmware would read it — through the side-effect-free `peek8` of the GDB
//! stub, or out of the device's own state where the register is write-only — and then decoded with the firmware's
//! own names. Nothing is interpreted beyond that: when a line looks wrong, it is the machine that is wrong.
//!
//! Firmware paths are under firmware/1541ultimate/software; register addresses are docs/hw/00-memory-map.md.

use trx64_monitor::MonitorHost;
use ue2_core::devices::c64::C64Port;
use ue2_core::devices::flash::SpiFlash;
use ue2_core::devices::rmii::Rmii;
use ue2_core::devices::sdcard::SdCard;
use ue2_core::devices::usb::Usb;

use super::Host;
use crate::gdb::peek8;

/// `ITU_BASE` and `C64_CARTREGS_BASE` (iomap.h:10, u64.h).
const ITU: u32 = 0x1000_0000;
const CART: u32 = 0x1004_0000;

/// ITU low interrupt sources, bit 0 first (itu.h:33-40).
const ITU_LOW: [&str; 8] = ["timer", "uart", "usb", "tape", "cmdif", "rmii-rx", "rmii-tx", "reset"];
/// High interrupt sources (itu.h:41-47); 2 has no name in the firmware.
const ITU_HIGH: [&str; 8] = ["acia", "1581", "-", "wifi", "bling", "hdmi", "unlock", "guru"];

/// `CART_TYPE_*` (c64.h:125-151). The variant is the top three bits of the same register and means something
/// different per type, so it is printed as a number.
const CART_TYPES: [&str; 32] = [
    "none", "normal", "epyx", "c128", "westermann/blackbox4", "simons basic", "business basic", "blackbox v3",
    "ocean 8k", "ocean 16k", "system 3", "supergames", "blackbox v8", "zaxxon", "blackbox v9", "megabyter",
    "pagefox", "easyflash", "?", "?", "?", "?", "?", "?", "final cartridge 12", "final cartridge 3",
    "super snapshot 5", "action replay", "kcs", "?", "?", "georam",
];

/// `reu_size[]` (c64.cc:60).
const REU_SIZES: [&str; 8] = ["128 KB", "256 KB", "512 KB", "1 MB", "2 MB", "4 MB", "8 MB", "16 MB"];

/// `C64_CLOCK_DETECT` bits (c64.h:88-95).
const CLOCK_DETECT: [&str; 6] = ["phi2", "vcc", "exrom", "game", "reset", "nmi"];

/// The verbs of this file, or `None` when the line is not one of them.
pub(super) fn verb(host: &mut Host, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
    if !args.is_empty() {
        return match verb {
            "itu" | "cart" | "flash" | "sd" | "usb" | "net" | "audio" => {
                Some(Err(format!("{verb}: takes no arguments")))
            }
            _ => None,
        };
    }
    match verb {
        "itu" => Some(Ok(itu(host))),
        "cart" => Some(Ok(cart(host))),
        "flash" => Some(flash(host)),
        "sd" => Some(sd(host)),
        "usb" => Some(usb(host)),
        "net" => Some(net(host)),
        "audio" => Some(Ok(audio(host))),
        _ => None,
    }
}

/// The verbs of this file for `help`.
pub(super) const HELP: &str = concat!(
    "  itu               the interrupt controller, the timers and the capability word\n",
    "  cart              what the firmware programmed for the C64: mode, cartridge, REU, the command interface\n",
    "  flash             the SPI flash: its image, what is not written out yet, the config pages\n",
    "  sd                the SD card\n",
    "  usb               the hub ports and what is on them\n",
    "  net               the Ethernet MAC: the address, the filter and the queues\n",
    "  audio             the SIDs the firmware built and the windows it routes to them\n",
);

/// `itu` — the interrupt controller as the ISR sees it, and the timers the firmware runs on.
fn itu(host: &mut Host) -> String {
    let byte = |host: &mut Host, off: u32| peek8(&host.m.bus, ITU + off);
    let capabilities = host.m.cfg.capabilities;
    let version = byte(host, 0x0B);
    let timer = byte(host, 0x06);
    let (timer_en, timer_hi, timer_lo) = (byte(host, 0x07), byte(host, 0x08), byte(host, 0x09));
    let ms = u32::from(byte(host, 0x22)) << 8 | u32::from(byte(host, 0x23));
    let irq = &host.m.bus.irq;
    let mut out = String::new();
    out.push_str(&format!("  capabilities  {capabilities:#010x}\n"));
    out.push_str(&format!("  fpga version  {version:#04x}\n"));
    out.push_str(&format!("  ms timer      {ms} (16 bits, wraps)\n"));
    out.push_str(&format!("  us timer      {timer} (counts down in 5 us steps)\n"));
    out.push_str(&format!(
        "  irq timer     {}, reload {}\n",
        if timer_en & 1 != 0 { "on" } else { "off" },
        u32::from(timer_hi) << 8 | u32::from(timer_lo),
    ));
    out.push_str(&format!("  irq           global {}\n", on(irq.global_en)));
    out.push_str(&format!("  low  enabled  {}\n", bits(irq.mask, &ITU_LOW)));
    out.push_str(&format!("  low  edge     {}\n", bits(irq.edge_mask, &ITU_LOW)));
    out.push_str(&format!("  low  pending  {}\n", bits(irq.flags | irq.level, &ITU_LOW)));
    out.push_str(&format!("  high enabled  {}\n", bits(irq.high_en, &ITU_HIGH)));
    out.push_str(&format!("  high pending  {}\n", bits(irq.high_src & irq.high_en, &ITU_HIGH)));
    out
}

/// `cart` — the C64's side of the machine, as the firmware left it: the registers `c64.cc` writes when a cartridge
/// is chosen, the REU, the sampler and the command interface.
fn cart(host: &mut Host) -> String {
    let byte = |host: &mut Host, off: u32| peek8(&host.m.bus, CART + off);
    let mode = byte(host, 0x00);
    let (stop, stop_mode) = (byte(host, 0x01), byte(host, 0x02));
    let detect = byte(host, 0x03);
    let kind = byte(host, 0x05);
    let (active, kernal) = (byte(host, 0x06), byte(host, 0x07));
    let (reu, reu_size) = (byte(host, 0x08), byte(host, 0x09));
    let (serve, sampler) = (byte(host, 0x0D), byte(host, 0x0E));

    let mut out = String::new();
    out.push_str(&format!(
        "  mode          {}\n",
        bits(mode, &["-", "ultimax", "reset", "unreset", "nmi", "-", "-", "-"]),
    ));
    out.push_str(&format!(
        "  stop          {}, {}  (condition {})\n",
        if stop & 0x01 != 0 { "requested" } else { "not requested" },
        if stop & 0x02 != 0 { "stopped" } else { "running" },
        ["badline", "r/w sequence", "force", "3"][usize::from(stop_mode & 3)],
    ));
    out.push_str(&format!(
        "  cartridge     type {:#04x} variant {}  {}, {}\n",
        kind & 0x1F,
        kind >> 5,
        CART_TYPES[usize::from(kind & 0x1F)],
        if active & 1 != 0 { "active" } else { "not active" },
    ));
    let rom = host.m.bus.io.get::<C64Port>().map(C64Port::cart_rom);
    if let Some(rom) = rom {
        out.push_str(&format!("  cart rom      {:#010x}, {} MiB\n", rom.base, rom.size / (1024 * 1024)));
    }
    out.push_str(&format!("  kernal        {}\n", on(kernal & 1 != 0)));
    out.push_str(&format!(
        "  reu           {}, {}\n",
        on(reu & 1 != 0),
        REU_SIZES[usize::from(reu_size & 7)],
    ));
    out.push_str(&format!("  sampler       {}\n", on(sampler & 1 != 0)));
    out.push_str(&format!("  serve         while stopped: {}\n", yes(serve & 1 != 0)));
    out.push_str(&format!("  clock detect  {}\n", bits(detect, &CLOCK_DETECT)));
    match host.machine().uci_status() {
        None => out.push_str("  command intf  this C64 carries no block\n"),
        Some(u) => out.push_str(&format!(
            "  command intf  {} at {:#06x}, bus id {}, state {}, irq {}{}\n",
            on(u.enabled),
            u.window,
            u.bus_id,
            u.state,
            yes(u.irq),
            if u.error { ", error" } else { "" },
        )),
    }
    out
}

/// `flash` — the chip the firmware boots from and keeps its settings in.
fn flash(host: &mut Host) -> Result<String, String> {
    let flash = host.m.bus.io.get::<SpiFlash>().ok_or("this machine has no flash")?;
    let (dirty, writing) = flash.pending();
    let pages = flash.config_pages().len();
    let mut out = String::new();
    match flash.image() {
        Some(path) => out.push_str(&format!("  image         {}\n", path.display())),
        None => out.push_str("  image         none: volatile, this run only\n"),
    }
    out.push_str(&format!(
        "  not written   {dirty} sector(s) of 4 KiB{}\n",
        if writing { ", a write-out is pending" } else { "" },
    ));
    out.push_str(&format!("  config pages  {pages} in use  (`config flash` lists them)\n"));
    Ok(out)
}

/// `sd` — the card in the slot.
fn sd(host: &mut Host) -> Result<String, String> {
    let sd = host.m.bus.io.get::<SdCard>().ok_or("this machine has no SD slot")?;
    Ok(match sd.card() {
        None => "  card          none in the slot\n".to_string(),
        Some((sectors, read_only)) => format!(
            "  card          {} sectors, {} MiB{}\n",
            sectors,
            sectors * 512 / (1024 * 1024),
            if read_only { ", write protected (the image is read-only)" } else { "" },
        ),
    })
}

/// `usb` — the hub the firmware's nano program enumerates, port by port.
fn usb(host: &mut Host) -> Result<String, String> {
    let usb = host.m.bus.io.get::<Usb>().ok_or("this machine has no USB host")?;
    let mut out = String::new();
    for port in 1.. {
        let Some(info) = usb.port_info(port) else { break };
        // An empty port's connect line is the hub's own business; there is nothing on it either way.
        let state = match (info.device, info.connected, info.enabled, info.plug_pending) {
            (None, ..) => String::new(),
            (Some(_), _, _, true) => "waiting to be plugged in".to_string(),
            (Some(_), false, ..) => "unplugged".to_string(),
            (Some(_), true, false, _) => "plugged in, not enumerated".to_string(),
            (Some(_), true, true, _) => "in use".to_string(),
        };
        let line = format!("  port {port}        {:<9} {state}", info.device.unwrap_or("empty"));
        out.push_str(line.trim_end());
        out.push('\n');
    }
    match out.is_empty() {
        true => Err("this machine's USB host has no hub".into()),
        false => Ok(out),
    }
}

/// `net` — the Ethernet MAC, as the firmware programmed and left it.
fn net(host: &mut Host) -> Result<String, String> {
    let rmii = host.m.bus.io.get::<Rmii>().ok_or("this machine has no Ethernet MAC")?;
    Ok(rmii.summary())
}

/// `audio` — the SIDs behind the C64's register windows. The mixer's own gain registers are write-only on the
/// hardware and are not part of what the machine keeps, so they are not here (docs/hw/00-memory-map.md).
fn audio(host: &mut Host) -> String {
    match super::trx64(host.m) {
        Some(backend) => backend.sid_summary(),
        None => "  no C64: the SIDs are the C64's, and this machine has none\n".to_string(),
    }
}

/// The set bits of `value` by name, or the word for none.
fn bits(value: u8, names: &[&str]) -> String {
    let set: Vec<&str> = names.iter().enumerate().filter(|(i, _)| value >> i & 1 != 0).map(|(_, &n)| n).collect();
    match set.is_empty() {
        true => format!("{value:#04x}  none"),
        false => format!("{value:#04x}  {}", set.join(" ")),
    }
}

fn on(state: bool) -> &'static str {
    match state {
        true => "on",
        false => "off",
    }
}

fn yes(state: bool) -> &'static str {
    match state {
        true => "yes",
        false => "no",
    }
}
