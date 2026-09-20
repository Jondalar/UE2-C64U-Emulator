//! `config` — the Ultimate's settings from the monitor, the things the menu can do with them (S23 §6).
//!
//! Reading is ours alone: the config pages sit in the flash we emulate, and `ue2-core/src/settings.rs` decodes them
//! through each item's own definition (S21). Changing one is not, because the firmware holds its own copy of every
//! store and writes that copy back over anything we put in a page. So `config set` goes the way the menu goes: hand
//! the firmware a `.cfg` and let it apply and effectuate it, and only then — on `config write` — put the same items
//! into the pages, where they survive the next boot.
//!
//! The `.cfg` itself is written by the firmware too ([`super::uci::write_file`]), so no filesystem inside the
//! machine ever has a second writer. That is also what makes a path on any medium work.
//!
//! Firmware paths are under firmware/1541ultimate/software.

use ue2_core::devices::flash::SpiFlash;
use ue2_core::settings::{self, Record};

use super::{uci, Host};

/// Where `config set` leaves the `.cfg` it hands to the firmware: the RAM disk, which is where the firmware puts
/// `CTRL_CMD_LOAD_CONFIG`'s own default file (control_target.cc:565). It is volatile, so nothing of ours outlives
/// the run, and the file stays afterwards for anyone who wants to see what was handed over.
const SCRATCH: &str = "/temp/ue2-monitor.cfg";

pub(super) fn verb(host: &mut Host, args: &[&str]) -> Result<String, String> {
    match args {
        ["flash"] => Ok(pages(host)?),
        ["read", path] => read(host, path),
        ["read", ..] => Err("config read: usage: config read <path in the emulated machine>".into()),
        ["set", category, item, value @ ..] if !value.is_empty() => set(host, category, item, &value.join(" ")),
        ["set", ..] => Err("config set: usage: config set <category> <item> <value>".into()),
        ["write"] => write_pages(host),
        ["write", path] => write_file(host, path),
        ["write", ..] => Err("config write: usage: config write [path in the emulated machine]".into()),
        _ => show(host, args.first().copied(), args.get(1).copied()),
    }
}

/// `config [category [item]]` — the settings as the flash holds them, each value marked `flash` or `default`.
fn show(host: &mut Host, category: Option<&str>, item: Option<&str>) -> Result<String, String> {
    let tables = settings::tables(&host.m.bus.ram, &host.m.segments);
    let stores = settings::stores(&tables);
    let flash = host.m.bus.io.get::<SpiFlash>().ok_or("this machine has no flash")?;
    Ok(settings::stored(&stores, flash, category, item))
}

/// `config flash` — the raw page decode: which pages a store claimed and how many records each holds.
fn pages(host: &mut Host) -> Result<String, String> {
    let flash = host.m.bus.io.get::<SpiFlash>().ok_or("this machine has no flash")?;
    let pages = flash.config_pages();
    let mut out = format!("  {} config pages in use\n", pages.len());
    for page in pages {
        let n = flash.config_page(page).map_or(0, |r| r.len());
        // The id is four ASCII characters, most significant first ("GEN.", "C64 ", `register_store`).
        let name = page.to_be_bytes().map(|b| if b.is_ascii_graphic() { b } else { b'.' });
        out.push_str(&format!("  {page:08x}  {}  {n} records\n", String::from_utf8_lossy(&name)));
    }
    Ok(out)
}

/// `config set <category> <item> <value>` — change it in the running firmware.
///
/// The value is checked against the item's own definition first (`settings::resolve`, S21 §4), so a wrong choice or
/// a number out of range is refused here and the firmware never sees it.
fn set(host: &mut Host, category: &str, item: &str, value: &str) -> Result<String, String> {
    let record = one_record(host, category, item, value)?;
    let cfg = settings::cfg(std::slice::from_ref(&record));
    uci::write_file(host.m, SCRATCH, cfg.as_bytes())?;
    let reply = load_config(host, SCRATCH)?;
    let mut out = format!("  {}\n", record.text);
    out.push_str(&log(&reply));
    if !reply.ok() {
        return Err(format!("config set: the firmware answered {}\n{out}", reply.status).trim_end().to_string());
    }
    host.state.stage(record);
    out.push_str("  the running firmware took it; `config write` puts it in the config pages\n");
    Ok(out)
}

/// `config write` — permanent, where the menu's "Save to Flash" puts it: the config pages.
///
/// Only what `config set` changed in this session. Writing a page the firmware does not know about is undone the
/// next time it saves that store from its own copy, which is why `set` comes first.
fn write_pages(host: &mut Host) -> Result<String, String> {
    let staged = host.state.staged.clone();
    if staged.is_empty() {
        return Err("config write: nothing was set in this session. `config set` first, so the firmware's own copy \
                    carries the change too — a page it does not know about is overwritten from that copy (S23 §6)"
            .into());
    }
    let flash = host.m.bus.io.get_mut::<SpiFlash>().ok_or("this machine has no flash")?;
    flash.write_settings(&staged).map_err(|e| e.to_string())?;
    let mut out = String::new();
    for record in &staged {
        out.push_str(&format!("  {}\n", record.text));
    }
    out.push_str(&format!("  {} item(s) in the config pages; they survive the next boot\n", staged.len()));
    Ok(out)
}

/// `config write <path>` — the menu's "Save to File": every item of every store, as a `.cfg` at a path inside the
/// machine.
fn write_file(host: &mut Host, path: &str) -> Result<String, String> {
    let tables = settings::tables(&host.m.bus.ram, &host.m.segments);
    let stores = settings::stores(&tables);
    let flash = host.m.bus.io.get::<SpiFlash>().ok_or("this machine has no flash")?;
    let body = settings::dump(&stores, flash);
    if body.is_empty() {
        return Err("config write: this firmware has no settings tables".into());
    }
    let body = format!("; ue2emu: the settings as the flash holds them, item defaults where it holds none\n{body}");
    uci::write_file(host.m, path, body.as_bytes())?;
    Ok(format!("  {path}  {} items in {} bytes\n", body.lines().filter(|l| l.contains('=')).count(), body.len()))
}

/// `config read <path>` — the menu's "Load Settings": the firmware opens that `.cfg`, applies the items it knows
/// and effectuates every store they touched (`ControlTarget::load_config`).
fn read(host: &mut Host, path: &str) -> Result<String, String> {
    let reply = load_config(host, path)?;
    let log = log(&reply);
    match reply.ok() {
        true => Ok(format!("  {path}  {}\n{log}", reply.status)),
        false => Err(format!("config read {path}: {}\n{log}", reply.status).trim_end().to_string()),
    }
}

/// `CTRL_CMD_LOAD_CONFIG` for a path inside the emulated machine.
fn load_config(host: &mut Host, path: &str) -> Result<uci::Reply, String> {
    if !path.is_ascii() {
        return Err("config: the firmware's paths are ASCII".into());
    }
    let mut message = vec![uci::TARGET_CONTROL, uci::CTRL_CMD_LOAD_CONFIG];
    message.extend(path.as_bytes());
    // No length field: the command's remainder is a C string, so it needs its terminator
    // (control_target.cc:565-568).
    message.push(0);
    uci::command(host.m, &message)
}

/// The reply data of `CTRL_CMD_LOAD_CONFIG` is its parse log — empty on full success, else the lines the firmware
/// could not apply.
fn log(reply: &uci::Reply) -> String {
    reply.text().lines().map(|line| format!("  {line}\n")).collect()
}

/// The record for one `category item value`, refused with the firmware's own reasons when it does not fit.
fn one_record(host: &mut Host, category: &str, item: &str, value: &str) -> Result<Record, String> {
    let tables = settings::tables(&host.m.bus.ram, &host.m.segments);
    let stores = settings::stores(&tables);
    let line = settings::Line {
        origin: "config set".into(),
        store: category.to_string(),
        item: item.to_string(),
        value: value.to_string(),
    };
    let (records, warnings) = settings::resolve(&[line], &stores).map_err(|e| e.to_string())?;
    match records.into_iter().next() {
        Some(record) => Ok(record),
        None => Err(warnings.join("\n")),
    }
}
