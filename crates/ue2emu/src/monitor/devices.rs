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
use ue2_core::devices::trx::{self, Trx};
use ue2_core::io::IoDevice;
use ue2_core::devices::usb::Usb;

use super::{uci, Host};
use crate::gdb::peek8;

/// `ITU_BASE` and `C64_CARTREGS_BASE` (iomap.h:10, u64.h).
const ITU: u32 = 0x1000_0000;
const CART: u32 = 0x1004_0000;
/// `C64_IO_BASE` (u64.h:49): the core registers of the C64 side.
const CORE: u32 = 0x1018_0000;

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

/// TRX IRQ_CAUSE bits (trx_io.v); UE2 raises only vsync and flip.
const TRX_CAUSES: [&str; 8] = ["stop", "engine", "log", "capture", "flash", "eeprom", "vsync", "flip"];

/// `C64_CLOCK_DETECT` bits (c64.h:88-95).
const CLOCK_DETECT: [&str; 6] = ["phi2", "vcc", "exrom", "game", "reset", "nmi"];

/// The verbs of this file, or `None` when the line is not one of them.
pub(super) fn verb(host: &mut Host, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
    if verb == "dir" {
        return Some(match args {
            [] => dir(host, "/"),
            [path] => dir(host, path),
            _ => Err("dir: usage: dir [path in the emulated machine]".into()),
        });
    }
    if !args.is_empty() {
        return match verb {
            "itu" | "cart" | "joy" | "flash" | "sd" | "usb" | "net" | "audio" | "trx" => {
                Some(Err(format!("{verb}: takes no arguments")))
            }
            _ => None,
        };
    }
    match verb {
        "itu" => Some(Ok(itu(host))),
        "cart" => Some(Ok(cart(host))),
        "joy" => Some(Ok(joy(host))),
        "flash" => Some(flash(host)),
        "sd" => Some(sd(host)),
        "usb" => Some(usb(host)),
        "net" => Some(net(host)),
        "audio" => Some(Ok(audio(host))),
        "trx" => Some(trx(host)),
        _ => None,
    }
}

/// The verbs of this file for `help`.
pub(super) const HELP: &str = concat!(
    "  itu               the interrupt controller, the timers and the capability word\n",
    "  cart              what the firmware programmed for the C64: mode, cartridge, REU, the command interface\n",
    "  joy               the control ports as the firmware drives them: joysticks, paddles, the mouse\n",
    "  flash             the SPI flash: its image, what is not written out yet, the config pages\n",
    "  sd                the SD card\n",
    "  usb               the hub ports and what is on them\n",
    "  net               the Ethernet MAC: the address, the filter and the queues\n",
    "  audio             the SIDs the firmware built and the windows it routes to them\n",
    "  trx               the TRX64-Ultimate block (--trx-io): ID, interrupt, pixel layer\n",
    "  dir [PATH]        what is on a medium, read by the firmware itself\n",
);

/// `joy` — what the firmware drives onto the control ports (u64.h:121-144): the software joystick lines (active low,
/// bit 4 fire), the paddle and mouse values, and the enables. S32: the USB mouse lands in port 1's.
fn joy(host: &mut Host) -> String {
    let byte = |host: &mut Host, off: u32| peek8(&host.m.bus, CORE + off);
    let mut out = String::new();
    for port in 0..2u32 {
        let lines = byte(host, 0x30 + port);
        let (x, y) = (byte(host, 0x32 + 2 * port), byte(host, 0x33 + 2 * port));
        let mouse = byte(host, 0x36 + port);
        out.push_str(&format!(
            "  port {}        joystick {:02x} {}  paddle x {x:02x} y {y:02x}  mouse {}\n",
            port + 1,
            lines,
            bits(!lines & 0x1F, &["up", "down", "left", "right", "fire", "-", "-", "-"]),
            if mouse & 1 != 0 { "on" } else { "off" },
        ));
    }
    let (enable, swap) = (byte(host, 0x13), byte(host, 0x1A));
    out.push_str(&format!("  paddle        override {enable:02x}  swap {swap:02x}\n"));
    out
}

/// `trx` — the TRX64-Ultimate block of `--trx-io` (S37): page 0's ID, CAPS, interrupt and gate, the pause, page 5's
/// freeze session, page 6's layer, still and text registers as the app last committed them, whether the last frame
/// start showed the layer, and TRX64's VIC frame count.
fn trx(host: &mut Host) -> Result<String, String> {
    let t = host.m.bus.io.get::<Trx>().ok_or("this machine has no TRX block (--trx-io)")?;
    let id: String = (0..4).map(|off| char::from(t.peek8(off))).collect();
    let reg = |off: u32| t.reg(off);
    let ctrl = reg(trx::PX_CTRL);
    let (size, pos, key, status) = (reg(trx::PX_SIZE), reg(trx::PX_POS), reg(trx::PX_KEY), reg(trx::PX_STATUS));
    let mut out = String::new();
    // As the app reads them: CAPS2 b5 is clear on a bitstream without APPLY (S41).
    out.push_str(&format!("  id            {id}  caps {:#04x}  caps2 {:#04x}\n", t.peek8(4), t.peek8(5)));
    out.push_str(&format!("  irq enabled   {}\n", bits(t.irq_en(), &TRX_CAUSES)));
    out.push_str(&format!("  irq cause     {}\n", bits(t.cause(), &TRX_CAUSES)));
    out.push_str(&format!(
        "  px_ctrl       {ctrl:#04x}  {}, format {}, scale {}  (shown: {})\n",
        on(ctrl & 1 != 0),
        ctrl >> 1 & 7,
        (ctrl >> 4 & 3).max(1),
        yes(t.shown()),
    ));
    out.push_str(&format!(
        "  px_front      {:#010x}  px_back {:#010x}  flip pending: {}\n",
        reg(trx::PX_FRONT),
        reg(trx::PX_BACK),
        yes(status & 1 != 0),
    ));
    out.push_str(&format!("  px_size       {} x {}  stride {}\n", size & 0xFFFF, size >> 16, reg(trx::PX_STRIDE)));
    out.push_str(&format!("  px_pos        x0 {} y0 {}\n", pos & 0xFFFF, pos >> 16));
    out.push_str(&format!(
        "  px_galpha     {}  px_key {:#06x} {}\n",
        reg(trx::PX_GALPHA),
        key & 0xFFFF,
        on(key & 0x1_0000 != 0),
    ));
    out.push_str(&format!("  px_status     {status:#010x}  frames {}\n", status >> 16));
    let still = reg(trx::STILL_STATUS);
    out.push_str(&format!(
        "  still         {}  base {:#010x}  status {still:#010x}  valid {}, slot {}, frames {}\n",
        on(reg(trx::STILL_CTRL) & 1 != 0),
        reg(trx::STILL_BASE),
        yes(still & 1 != 0),
        still >> 1 & 1,
        still >> 8,
    ));
    let gate = t.sys_reg(trx::GATE_CTRL);
    out.push_str(&format!(
        "  gate_ctrl     {gate:#010x}  frz {}  paused: {}\n",
        on(gate & trx::GATE_FRZ != 0),
        yes(gate & trx::GATE_HALTED != 0),
    ));
    let (cap, pos) = (t.sys_reg(trx::CAP_STAT), t.sys_reg(trx::STOP_POS));
    out.push_str(&format!(
        "  cap_ctrl      every {}  cap_stat gen {}, cadence frames without a snap {}\n",
        t.sys_reg(trx::CAP_CTRL),
        cap & 0xFFFF,
        cap >> 16,
    ));
    out.push_str(&format!(
        "  stop_pos      {pos:#010x}  armed {}, cycle {}, holding: {}\n",
        yes(pos & trx::POS_ARM != 0),
        pos & 0x3F,
        yes(pos & trx::POS_HOLDING != 0),
    ));
    let (sb, load) = (t.state_bus(), t.sys_reg(trx::PAGE1 + trx::SB_LOAD));
    let (w0, w1, w2) = (sb.word(1 << 7), sb.word(1 << 7 | 1), sb.word(1 << 7 | 2));
    out.push_str(&format!(
        "  state bus     pc ${:04x}  a {:02x} x {:02x} y {:02x} s {:02x} p {:02x}  $01 {:02x}  sb_load {load:#04x}\n",
        w1 & 0xFFFF,
        w0 & 0xFF,
        w0 >> 8 & 0xFF,
        w0 >> 16 & 0xFF,
        w0 >> 24,
        w1 >> 16 & 0xFF,
        w2 >> 16 & 0xFF,
    ));
    // S39: the debug block (page 3, page 0 0x118-0x13F).
    let (d, cx) = t.debug();
    let status = d.status(cx);
    out.push_str(&format!(
        "  debug         owner {}  status {status:#010x}  shut {}, debugger stop {}, step {}, stop_new {}, stops {}\n",
        yes(d.own),
        yes(status & 1 != 0),
        yes(status & 2 != 0),
        yes(status & 0x80 != 0),
        yes(d.stop_new),
        d.stops,
    ));
    let r = &d.rec;
    out.push_str(&format!(
        "  stop record   reason {} unit {} flags {:#x}  ipc ${:04x} addr ${:04x} data ${:02x}  line {} cycle {}\n",
        r.reason,
        r.unit,
        r.flags,
        r.ipc,
        r.addr,
        r.data,
        r.pos >> 7,
        r.pos & 0x7F,
    ));
    out.push_str(&format!(
        "  counters      cycles {}  instrs {}  cyc_cmp {:#010x} {}\n",
        d.cycles_now(),
        d.instrs_now(),
        d.cyc_cmp,
        if d.cmp_en { "armed" } else { "off" },
    ));
    let bits_set: u32 = d.bmp.iter().map(|w| w.count_ones()).sum();
    out.push_str(&format!("  exec bitmap   {}  {bits_set} address(es)\n", on(d.bmp_en)));
    for (i, u) in d.units.iter().enumerate() {
        let kind = ["-", "IB", "read", "write", "read/write", "every edge", "-", "-"][usize::from(u.kind())];
        let why = d.refused_units().into_iter().find(|(n, _)| *n == i).map_or(String::new(), |(_, w)| format!("  refused: {w} (S39 T1)"));
        out.push_str(&format!(
            "  unit {i}        {}  {kind} ${:04x}-${:04x}  action {}  hits {}{why}\n",
            on(u.on()),
            u.lo,
            u.hi,
            ["stop", "count", "log", "-"][usize::from(u.action())],
            u.hits,
        ));
    }
    out.push_str(&format!(
        "  drive         dpc ${:04x}  dcycles {}  drv_cmp {:#07x}  held {}\n",
        d.drive.dpc,
        d.drive.dcycles,
        d.drive.cmp,
        yes(d.drive.hold),
    ));
    out.push_str(&format!("  lens          lk_res {:#010x}\n", t.sys_reg(trx::PAGE5 + trx::LK_RES)));
    let eng = t.engine();
    out.push_str(&format!(
        "  engine        log {}  taps {}  frames {}  dropped {}  apply {}\n",
        on(eng.on),
        ["outside", "cartridge RAM", "colour RAM"]
            .iter()
            .enumerate()
            .filter(|(i, _)| eng.taps >> i & 1 != 0)
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .join(", "),
        eng.frame,
        eng.dropped,
        if eng.ap_jobs.is_empty() { "idle" } else { "waiting" },
    ));
    for (s, (base, size)) in eng.areas.iter().enumerate() {
        out.push_str(&format!("  stream {s}      {:#09x} + {:#x}  {} records\n", base, size, eng.cnt[s]));
    }
    out.push_str(&format!("  engine store  {} MB\n", eng.store_mb));
    let (ctrl, status) = (t.sys_reg(trx::PAGE5 + trx::SH_CTRL), t.sys_reg(trx::PAGE5 + trx::SH_STATUS));
    out.push_str(&format!(
        "  sh_ctrl       {ctrl:#010x}  mode {}, sidefx {}, proto {}\n",
        ctrl & 3,
        on(ctrl & 4 != 0),
        on(ctrl & 8 != 0),
    ));
    out.push_str(&format!(
        "  sh_status     {status:#010x}  session: {}, sessions {}\n",
        yes(status & 1 != 0),
        status >> 16,
    ));
    let list = t.sys_reg(trx::PAGE5 + trx::SH_LIST);
    out.push_str(&format!(
        "  sh_counts     accesses {}  real {}  unmapped {}  list {} (dropped {})  last commit {}\n",
        t.sys_reg(trx::PAGE5 + trx::SH_ACCESSES),
        t.sys_reg(trx::PAGE5 + trx::SH_REAL),
        t.sys_reg(trx::PAGE5 + trx::SH_UNMAPPED),
        list & 0xFF,
        list >> 16,
        t.sys_reg(trx::PAGE5 + trx::SH_COMMIT),
    ));
    let (tx, tx_status) = (t.tx_reg(trx::TX_CTRL), t.tx_reg(trx::TX_STATUS));
    out.push_str(&format!(
        "  tx_ctrl       {tx:#010x}  show {}, opaque {}  tx_status {tx_status:#010x} (shown: {})\n",
        on(tx & 1 != 0),
        on(tx & 2 != 0),
        yes(tx_status & 1 != 0),
    ));
    out.push_str(&format!("  vic frames    {}\n", t.vic_frames()));
    Ok(out)
}

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

/// `dir [path]` — what is on a medium, as the firmware's own file manager sees it.
///
/// The listing is the firmware's, not ours: it reads the directory through its DOS target over the same UCI
/// transport `config` writes through (S23 §6), so every mount works — `/flash`, `/Usb0`, `/Temp`, the SD card —
/// and what comes back is exactly what the firmware would show in its menu. A bare `dir` lists the mounts.
fn dir(host: &mut Host, path: &str) -> Result<String, String> {
    let entries = uci::directory(host.m, path)?;
    if entries.is_empty() {
        return Ok(format!("  {path}  empty\n"));
    }
    let mut out = String::new();
    for (attributes, name) in &entries {
        // FatFs attribute bits (ff.h): 0x10 directory, 0x01 read-only, 0x02 hidden, 0x04 system.
        let kind = if attributes & 0x10 != 0 { "dir " } else { "    " };
        let flags = [(0x01, "ro"), (0x02, "hidden"), (0x04, "system")]
            .iter()
            .filter(|(bit, _)| attributes & bit != 0)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("  {kind}{name}{}{flags}\n", if flags.is_empty() { "" } else { "   " }));
    }
    out.push_str(&format!("  {} entries in {path}\n", entries.len()));
    Ok(out)
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
