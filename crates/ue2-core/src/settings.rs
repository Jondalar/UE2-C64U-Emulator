//! Firmware settings from a `.cfg` file, written into the flash config pages before the firmware runs
//! (docs/specs/S21-settings.md). Firmware paths are under firmware/1541ultimate/software.
//!
//! - [`tables`] finds the `t_cfg_definition` arrays in the loaded image: item ids, names, choices, ranges (§2).
//! - [`stores`] ties them to the store names and page ids of [`CATALOG`] (§3).
//! - [`parse`] reads a `.cfg` as `ConfigIO::S_read_from_file` does, [`resolve`] turns its lines into flash records
//!   (§4), which `SpiFlash::write_settings` puts into the pages (§5).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::bus::RAM_MASK;
use crate::devices::flash::SpiFlash;

/// Item types (config.h:73-82).
pub const CFG_TYPE_VALUE: u8 = 0x01;
pub const CFG_TYPE_ENUM: u8 = 0x02;
pub const CFG_TYPE_STRING: u8 = 0x03;
pub const CFG_TYPE_FUNC: u8 = 0x04;
pub const CFG_TYPE_SEP: u8 = 0x05;
pub const CFG_TYPE_INFO: u8 = 0x06;
pub const CFG_TYPE_STRFUNC: u8 = 0x07;
pub const CFG_TYPE_STRPASS: u8 = 0x08;

/// `sizeof(t_cfg_definition)` on RV32: id, type, 2 bytes padding, 3 pointers, min, max, def (config.h:87-96).
const ENTRY: usize = 28;
/// Longest item text or choice taken as a string; the menu fits them on a 40-column screen.
const TEXT_MAX: usize = 64;

/// One `t_cfg_definition`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    pub id: u8,
    pub kind: u8,
    pub name: String,
    /// `items[0..=max]` of an ENUM, as far as they read as strings; empty when the list is filled at run time.
    pub choices: Vec<String>,
    pub min: i32,
    pub max: i32,
    pub default: DefaultValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    Int(i32),
    Text(String),
}

impl Def {
    /// Whether the item is stored in the flash (`ConfigItem::pack`, config.cc:706-758).
    pub fn is_setting(&self) -> bool {
        matches!(self.kind, CFG_TYPE_VALUE | CFG_TYPE_ENUM | CFG_TYPE_STRING | CFG_TYPE_STRFUNC | CFG_TYPE_STRPASS)
    }

    /// The choice `n` if the list has it and it lies in `min..=max`.
    fn choice(&self, n: i32) -> Option<&str> {
        (self.min..=self.max).contains(&n).then(|| self.choices.get(n as usize)).flatten().map(String::as_str)
    }
}

/// One definition array in the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub addr: u32,
    pub defs: Vec<Def>,
}

impl Table {
    /// The definition of `name`, as the firmware matches a `.cfg` line: case is ignored
    /// (`ConfigStore::get_item_by_name`).
    pub fn find(&self, name: &str) -> Option<&Def> {
        self.defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))
    }
}

/// The loaded firmware: RAM and the address ranges the loader filled.
struct Image<'a> {
    ram: &'a [u8],
    segments: &'a [(u32, u32)],
}

impl Image<'_> {
    /// `len` bytes at `addr`, when they lie inside one loaded segment.
    fn bytes(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let inside = self
            .segments
            .iter()
            .any(|&(base, size)| addr >= base && u64::from(addr) + len as u64 <= u64::from(base) + u64::from(size));
        let at = (addr & RAM_MASK) as usize;
        (inside && at + len <= self.ram.len()).then(|| &self.ram[at..at + len])
    }

    fn word(&self, addr: u32) -> Option<u32> {
        self.bytes(addr, 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    }

    /// The C string at `addr` (printable ASCII, may be empty), or None.
    fn text(&self, addr: u32) -> Option<String> {
        self.string(addr, |b| (0x20..0x7F).contains(&b))
    }

    /// A choice string: printable, Latin-1 above 0x7F allowed.
    fn choice(&self, addr: u32) -> Option<String> {
        self.string(addr, |b| b >= 0x20 && b != 0x7F)
    }

    fn string(&self, addr: u32, printable: impl Fn(u8) -> bool) -> Option<String> {
        if addr == 0 {
            return None;
        }
        let mut text = String::new();
        for i in 0..TEXT_MAX as u32 {
            match self.bytes(addr.checked_add(i)?, 1)?[0] {
                0 => return Some(text),
                b if printable(b) => text.push(char::from(b)),
                _ => return None,
            }
        }
        None
    }

    /// The definition at `addr`, with its format string, or None when it does not look like one (§2).
    fn entry(&self, addr: u32) -> Option<(Def, String)> {
        let b = self.bytes(addr, ENTRY)?;
        let (id, kind) = (b[0], b[1]);
        if b[2..4] != [0, 0] || !(CFG_TYPE_VALUE..=CFG_TYPE_STRPASS).contains(&kind) {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"));
        let name = self.text(word(4))?;
        let format = self.text(word(8))?;
        let (items, min, max, def) = (word(12), word(16) as i32, word(20) as i32, word(24));
        let mut choices = Vec::new();
        if kind == CFG_TYPE_ENUM && items != 0 && (0..=255).contains(&max) {
            for n in 0..=max as u32 {
                match items.checked_add(4 * n).and_then(|a| self.word(a)).and_then(|p| self.choice(p)) {
                    Some(c) => choices.push(c),
                    None => break,
                }
            }
        }
        let default = match kind {
            CFG_TYPE_STRING | CFG_TYPE_STRFUNC | CFG_TYPE_STRPASS => {
                DefaultValue::Text(self.choice(def).unwrap_or_default())
            }
            _ => DefaultValue::Int(def as i32),
        };
        Some((Def { id, kind, name, choices, min, max, default }, format))
    }

    /// Whether `addr` holds the entry that ends a table: id 0xFF or type `CFG_TYPE_END` (config.cc:238-243).
    fn end(&self, addr: u32) -> bool {
        self.bytes(addr, 2).is_some_and(|b| b[0] == 0xFF || b[1] == 0xFF)
    }
}

/// Every definition array of the image: a run of entries whose first has a `%` in its format, ended by the end
/// entry (§2). `segments` are the loader's (address, length) pairs.
pub fn tables(ram: &[u8], segments: &[(u32, u32)]) -> Vec<Table> {
    let image = Image { ram, segments };
    let mut tables = Vec::new();
    for &(base, size) in segments {
        let end = base.saturating_add(size);
        let mut addr = base.next_multiple_of(4);
        while addr.saturating_add(ENTRY as u32) <= end {
            let first = image.entry(addr).filter(|(def, format)| def.id != 0xFF && format.contains('%'));
            if first.is_some() {
                let mut defs = Vec::new();
                let mut at = addr;
                while let Some((def, _)) = image.entry(at).filter(|(def, _)| def.id != 0xFF) {
                    defs.push(def);
                    at += ENTRY as u32;
                }
                if image.end(at) {
                    tables.push(Table { addr, defs });
                    addr = at + ENTRY as u32;
                    continue;
                }
            }
            addr += 4;
        }
    }
    tables
}

/// A store UE2 can write: name, page id and the item that identifies its table (§3).
pub struct Spec {
    pub name: &'static str,
    pub page: u32,
    anchor: &'static str,
    /// A choice the anchor must offer, where two tables share the anchor.
    choice: Option<&'static str>,
}

const fn spec(name: &'static str, page: u32, anchor: &'static str) -> Spec {
    Spec { name, page, anchor, choice: None }
}

/// The stores of the firmware source, checked against the `register_store` call sites of 3.14, 3.15 and C64U 1.1.0.
pub const CATALOG: [Spec; 20] = [
    spec("C64 and Cartridge Settings", 0x4336_3420, "RAM Expansion Unit"), // c64.cc:134
    spec("User Interface Settings", 0x4745_4E2E, "Interface Type"),        // userinterface.cc:156
    spec("Network Settings", 0x4E45_5400, "Host Name"),                    // network_config.cc:50
    spec("WiFi settings", 0x5749_4649, "Connected to"),                    // network_esp32.cc:119
    spec("Ethernet Settings", 0x4E65_7477, "Use DHCP"),                    // network_interface.cc:177
    spec("Tape Settings", 0x5441_5045, "Tape Playback Rate"),              // tape_controller.cc:35
    // bling_board.cc:153 at v3.14; its LED table has the same items as the LED strip's.
    Spec { name: "Keyboard Lighting", page: 0x4452_4557, anchor: "LedStrip Pattern", choice: Some("Circular") },
    spec("LED Strip Settings", 0x4C45_4453, "LedStrip Mode"),              // led_strip.cc:134
    spec("Data Streams", 0x4461_7461, "Stream VIC to"),                    // data_streamer.cc:43
    spec("Speaker Mixer", 0x5536_3444, "Speaker Enable"),                  // u64_config.cc:502
    spec("Audio Mixer", 0x5536_3443, "Vol UltiSid 1"),                     // u64_config.cc:480
    spec("SID Sockets Configuration", 0x5536_3443, "SID Socket 1"),        // u64_config.cc:523
    spec("UltiSID Configuration", 0x5536_3443, "UltiSID 1 Filter Curve"),  // u64_config.cc:824
    spec("SID Addressing", 0x5536_3443, "SID Socket 1 Address"),           // u64_config.cc:853
    spec("U64 Specific Settings", 0x5536_3443, "System Mode"),             // u64_config.cc:906
    spec("Modem Settings", 0x4D4F_444D, "Modem Interface"),                // modem.cc:95
    spec("SoftIEC Drive Settings", 0x4945_4300, "IEC Drive"),              // iec_drive.cc:136
    spec("Printer Settings", 0x4D50_5300, "IEC printer"),                  // iec_printer.cc:356
    spec("Drive A Settings", 0x1002_0000, "Drive Type"),                   // c1541.cc:126, DRIVE_A_BASE (iomap.h:11)
    spec("Drive B Settings", 0x1002_4000, "Drive Type"),                   // DRIVE_B_BASE (iomap.h:12)
];

impl Spec {
    fn matches(&self, table: &Table) -> bool {
        table.find(self.anchor).is_some_and(|def| match self.choice {
            None => true,
            Some(choice) => def.choices.iter().any(|c| c.trim() == choice),
        })
    }
}

/// A catalog store and the table it has in this image.
pub struct Store<'t> {
    pub spec: &'static Spec,
    pub table: &'t Table,
}

/// The catalog resolved against an image: the stores it has, in catalog order, and the names it lacks.
pub struct Stores<'t> {
    pub found: Vec<Store<'t>>,
    pub missing: Vec<&'static str>,
}

impl Stores<'_> {
    fn get(&self, name: &str) -> Option<&Store<'_>> {
        self.found.iter().find(|s| s.spec.name.eq_ignore_ascii_case(name))
    }
}

/// Each catalog store's table (§3): the one table with its anchor; where several have it, the tables other stores
/// hold alone are taken out, repeated until nothing changes.
pub fn stores(tables: &[Table]) -> Stores<'_> {
    let candidates: Vec<Vec<usize>> = CATALOG
        .iter()
        .map(|spec| (0..tables.len()).filter(|&t| spec.matches(&tables[t])).collect())
        .collect();
    let mut chosen: Vec<Option<usize>> = candidates.iter().map(|c| (c.len() == 1).then(|| c[0])).collect();
    loop {
        let mut progress = false;
        for k in 0..CATALOG.len() {
            if chosen[k].is_some() {
                continue;
            }
            let left: Vec<usize> =
                candidates[k].iter().copied().filter(|&t| !chosen.contains(&Some(t))).collect();
            if left.len() == 1 {
                chosen[k] = Some(left[0]);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (spec, choice) in CATALOG.iter().zip(chosen) {
        match choice {
            Some(t) => found.push(Store { spec, table: &tables[t] }),
            None => missing.push(spec.name),
        }
    }
    Stores { found, missing }
}

/// One `Item=Value` line of a `.cfg`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// `file:line` for messages.
    pub origin: String,
    pub store: String,
    pub item: String,
    pub value: String,
}

/// The `Item=Value` lines of a `.cfg` (`ConfigIO::S_read_from_file`, `S_read_store_element`, configio.cc:227-360).
/// Bytes are taken as Latin-1, so a string value reaches the flash byte for byte. Malformed lines are errors (§4).
pub fn parse(data: &[u8], origin: &str) -> Result<Vec<Line>> {
    let text: String = data.iter().filter(|&&b| b != b'\r').map(|&b| char::from(b)).collect();
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let mut store: Option<String> = None;
    for (n, line) in text.split('\n').enumerate() {
        let at = format!("{origin}:{}", n + 1);
        if line.starts_with('#') || line.starts_with(';') || line.trim_matches([' ', '\t']).is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            match rest.find(']') {
                Some(end) => store = Some(rest[..end].to_string()),
                None => {
                    errors.push(format!("{at}: no ']' after the store name"));
                    store = None;
                }
            }
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            errors.push(format!("{at}: no '=' in \"{line}\""));
            continue;
        };
        let name = name.trim_matches([' ', '\t']);
        if name.is_empty() {
            errors.push(format!("{at}: no item name"));
            continue;
        }
        let Some(store) = &store else {
            errors.push(format!("{at}: \"{name}\" is not inside a [store] section"));
            continue;
        };
        lines.push(Line { origin: at, store: store.clone(), item: name.to_string(), value: value.to_string() });
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(lines)
}

/// One item record for a config page: `id type len payload` (config.cc:706-758).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub page: u32,
    pub id: u8,
    pub kind: u8,
    pub payload: Vec<u8>,
    /// `[Store] Item=Value` in the firmware's spelling, for the report.
    pub text: String,
}

/// The records of `lines`, a later line for the same item replacing an earlier one, and the warnings for stores
/// and items this firmware does not have. Any other problem is an error, all of them listed (§4).
pub fn resolve(lines: &[Line], stores: &Stores) -> Result<(Vec<Record>, Vec<String>)> {
    let mut records: Vec<Record> = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    for line in lines {
        let at = &line.origin;
        let Some(store) = stores.get(&line.store) else {
            let known = CATALOG.iter().any(|s| s.name.eq_ignore_ascii_case(&line.store));
            let why = if known { "is not in this firmware" } else { "is not a store UE2 can set (S21 §3)" };
            warnings.push(format!("{at}: [{}] {why}; \"{}\" skipped", line.store, line.item));
            continue;
        };
        let name = store.spec.name;
        let Some(def) = store.table.find(&line.item) else {
            warnings.push(format!("{at}: [{name}] has no item \"{}\" in this firmware; skipped", line.item));
            continue;
        };
        match encode(def, &line.value) {
            Ok((payload, shown)) => {
                records.retain(|r| !(r.page == store.spec.page && r.id == def.id));
                records.push(Record {
                    page: store.spec.page,
                    id: def.id,
                    kind: def.kind,
                    payload,
                    text: format!("[{name}] {}={shown}", def.name),
                });
            }
            Err(e) => errors.push(format!("{at}: [{name}] {}: {e}", def.name)),
        }
    }
    if !errors.is_empty() {
        errors.extend(warnings.iter().map(|w| format!("warning: {w}")));
        bail!("{}", errors.join("\n"));
    }
    Ok((records, warnings))
}

/// The payload `ConfigItem::pack` would write for `value`, and the value as the firmware spells it.
fn encode(def: &Def, value: &str) -> Result<(Vec<u8>, String), String> {
    match def.kind {
        CFG_TYPE_ENUM => {
            if def.choices.is_empty() {
                return Err("its choices are filled in at run time, so UE2 cannot read them".into());
            }
            let wanted = value.trim_matches([' ', '\t']);
            let same = |c: &&str| c.trim_matches([' ', '\t']).eq_ignore_ascii_case(wanted);
            (def.min..=def.max)
                .find_map(|n| def.choice(n).filter(same).map(|c| (n, c)))
                .map(|(n, c)| (vec![n as u8], c.trim().to_string()))
                .ok_or_else(|| format!("\"{value}\" is not a choice ({})", choices(def)))
        }
        CFG_TYPE_VALUE => {
            let n = scan_int(value).ok_or_else(|| format!("\"{value}\" is not a number"))?;
            if !(i64::from(def.min)..=i64::from(def.max)).contains(&n) {
                return Err(format!("{n} is outside {}..{}", def.min, def.max));
            }
            Ok(((n as i32).to_be_bytes().to_vec(), n.to_string()))
        }
        CFG_TYPE_STRING | CFG_TYPE_STRFUNC | CFG_TYPE_STRPASS => {
            let bytes: Vec<u8> = value.chars().map(|c| c as u8).collect();
            let max = def.max.clamp(0, 255) as usize;
            if bytes.len() > max {
                return Err(format!("\"{value}\" is longer than {max} characters"));
            }
            let shown = if def.kind == CFG_TYPE_STRPASS { "(hidden)".into() } else { value.to_string() };
            Ok((bytes, shown))
        }
        _ => Err("not a setting (a heading, an action or a read-only field)".into()),
    }
}

/// `sscanf("%d")`: leading white space, a sign, digits; anything after them is ignored.
fn scan_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let digits_at = usize::from(s.starts_with(['+', '-']));
    let len = s[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 || len > 10 {
        return None;
    }
    s[..digits_at + len].parse().ok()
}

fn choices(def: &Def) -> String {
    (def.min..=def.max).filter_map(|n| def.choice(n)).map(str::trim).collect::<Vec<_>>().join(" | ")
}

/// Every setting of the image as a `.cfg` with the defaults, the choices or range in a comment above each (§6).
/// What the flash holds, as `[Store] Item=Value` lines (S23 §6, the monitor's `config`).
///
/// A value the page carries is decoded through the item's own type; an item the page does not carry is the
/// firmware's default and says so. `category` and `item` narrow it, both by case-insensitive name.
pub fn stored(stores: &Stores, flash: &SpiFlash, category: Option<&str>, item: Option<&str>) -> String {
    emit(stores, flash, category, item, true)
}

/// The same values as a `.cfg` the firmware reads back (`ConfigIO::S_read_from_file`): no marks, every store and
/// every item. This is what the menu's "Save to File" leaves behind, as far as UE2 can see it — the flash, and each
/// item's own default where the flash has nothing.
pub fn dump(stores: &Stores, flash: &SpiFlash) -> String {
    emit(stores, flash, None, None, false)
}

/// One `[Store] Item=Value` per record, grouped into `.cfg` sections, in the firmware's own spelling (§4). A `.cfg`
/// of exactly the items that changed is all `CTRL_CMD_LOAD_CONFIG` needs (S23 §6).
pub fn cfg(records: &[Record]) -> String {
    let mut out = String::new();
    let mut section = String::new();
    for record in records {
        let Some((store, item)) = record.text.strip_prefix('[').and_then(|t| t.split_once("] ")) else {
            continue;
        };
        if store != section {
            out.push_str(&format!("[{store}]\n"));
            section = store.to_string();
        }
        out.push_str(item);
        out.push('\n');
    }
    out
}

fn emit(stores: &Stores, flash: &SpiFlash, category: Option<&str>, item: Option<&str>, mark: bool) -> String {
    let mut out = String::new();
    for store in stores.found.iter().filter(|s| category.is_none_or(|c| s.spec.name.eq_ignore_ascii_case(c))) {
        let records = flash.config_page(store.spec.page).unwrap_or_default();
        out.push_str(&format!("[{}]\n", store.spec.name));
        for def in store.table.defs.iter().filter(|d| d.is_setting()) {
            if item.is_some_and(|i| !def.name.eq_ignore_ascii_case(i)) {
                continue;
            }
            let stored = records.iter().rev().find(|(id, _, _)| *id == def.id);
            let (value, source) = match stored {
                Some((_, kind, payload)) => (decode(def, *kind, payload), "flash"),
                None => (default_text(def), "default"),
            };
            match mark {
                true => out.push_str(&format!("{}={}   ({source})\n", def.name, value)),
                false => out.push_str(&format!("{}={}\n", def.name, value)),
            }
        }
    }
    if out.is_empty() {
        if !mark {
            return out;
        }
        return match (category, item) {
            (Some(c), Some(i)) => format!("no item '{i}' in '{c}'\n"),
            (Some(c), None) => format!("no store '{c}' in this firmware\n"),
            _ => "this firmware has no settings tables\n".into(),
        };
    }
    out
}

/// One stored record as text, through the item's type (config.cc `ConfigItem::unpack`).
fn decode(def: &Def, kind: u8, payload: &[u8]) -> String {
    match kind {
        CFG_TYPE_ENUM => {
            let v = usize::from(payload.first().copied().unwrap_or(0));
            def.choices.get(v).cloned().unwrap_or_else(|| v.to_string())
        }
        CFG_TYPE_VALUE => {
            let mut b = [0u8; 4];
            b[..payload.len().min(4)].copy_from_slice(&payload[..payload.len().min(4)]);
            i32::from_be_bytes(b).to_string()
        }
        _ => String::from_utf8_lossy(payload.split(|&b| b == 0).next().unwrap_or(payload)).into_owned(),
    }
}

/// The firmware's own default for an item it has never stored.
fn default_text(def: &Def) -> String {
    match &def.default {
        DefaultValue::Text(t) => t.clone(),
        DefaultValue::Int(v) if def.kind == CFG_TYPE_ENUM => {
            def.choices.get(*v as usize).cloned().unwrap_or_else(|| v.to_string())
        }
        DefaultValue::Int(v) => v.to_string(),
    }
}

pub fn template(stores: &Stores, firmware: &str) -> String {
    let mut out = format!(
        "; Settings of {firmware}, each at its default (docs/specs/S21-settings.md).\n\
         ; Keep the lines to change and pass the file to `ue2emu run --settings`.\n"
    );
    if !stores.missing.is_empty() {
        out += &format!("; Not in this firmware: {}.\n", stores.missing.join(", "));
    }
    for store in &stores.found {
        out += &format!("\n[{}]\n", store.spec.name);
        for def in store.table.defs.iter().filter(|d| d.is_setting()) {
            let (hint, value) = match (&def.default, def.kind) {
                (DefaultValue::Int(_), CFG_TYPE_ENUM) if def.choices.is_empty() => {
                    out += &format!("; {}: choices filled in at run time, not settable here\n", def.name);
                    continue;
                }
                (DefaultValue::Int(n), CFG_TYPE_ENUM) => {
                    (choices(def), def.choice(*n).map(|c| c.trim().to_string()).unwrap_or_default())
                }
                (DefaultValue::Int(n), _) => (format!("{}..{}", def.min, def.max), n.to_string()),
                (DefaultValue::Text(t), _) => (format!("text, at most {} characters", def.max), t.clone()),
            };
            out += &format!("; {hint}\n{}={value}\n", def.name);
        }
    }
    out
}

/// Read `files`, resolve them against the firmware in `ram` and write them into `flash` (S21 §5); what was set and
/// the warnings go to stderr.
pub fn apply(files: &[PathBuf], ram: &[u8], segments: &[(u32, u32)], flash: &mut SpiFlash) -> Result<()> {
    let mut lines = Vec::new();
    for file in files {
        let data = std::fs::read(file).with_context(|| format!("--settings: reading {}", file.display()))?;
        lines.extend(parse(&data, &file.display().to_string()).context("--settings")?);
    }
    let tables = tables(ram, segments);
    let stores = stores(&tables);
    let (records, warnings) = resolve(&lines, &stores).context("--settings")?;
    for warning in &warnings {
        eprintln!("settings: warning: {warning}");
    }
    flash.write_settings(&records).context("--settings")?;
    flash.flush().context("--settings: writing the flash image")?;
    for record in &records {
        eprintln!("settings: {}", record.text);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic image: definitions and strings placed in one segment at `BASE`.
    struct Builder {
        ram: Vec<u8>,
        next: u32,
    }

    const BASE: u32 = 0x0010_0000;

    impl Builder {
        fn new() -> Self {
            Builder { ram: vec![0; 0x20_0000], next: BASE + 0x8000 }
        }

        fn string(&mut self, s: &str) -> u32 {
            let at = self.next;
            self.ram[at as usize..at as usize + s.len()].copy_from_slice(s.as_bytes());
            self.next += (s.len() as u32 + 4) & !3;
            at
        }

        fn choices(&mut self, list: &[&str]) -> u32 {
            let ptrs: Vec<u32> = list.iter().map(|s| self.string(s)).collect();
            let at = self.next;
            for (i, p) in ptrs.iter().enumerate() {
                self.ram[at as usize + 4 * i..at as usize + 4 * i + 4].copy_from_slice(&p.to_le_bytes());
            }
            self.next += 4 * ptrs.len() as u32;
            at
        }

        /// Entries `(id, type, name, format, items, min, max, def)` at `at`, then the end entry.
        #[allow(clippy::type_complexity)]
        fn table(&mut self, at: u32, entries: &[(u8, u8, &str, &str, u32, i32, i32, u32)]) {
            let mut p = at as usize;
            for &(id, kind, name, format, items, min, max, def) in entries {
                let name = self.string(name);
                let format = self.string(format);
                let words = [name, format, items, min as u32, max as u32, def];
                self.ram[p] = id;
                self.ram[p + 1] = kind;
                for (i, w) in words.iter().enumerate() {
                    self.ram[p + 4 + 4 * i..p + 8 + 4 * i].copy_from_slice(&w.to_le_bytes());
                }
                p += ENTRY;
            }
            self.ram[p] = 0xFF;
            self.ram[p + 1] = 0xFF;
        }

        fn tables(&self) -> Vec<Table> {
            tables(&self.ram, &[(BASE, 0x10_0000)])
        }
    }

    fn c64_table(b: &mut Builder, at: u32) {
        let en_dis_geo = b.choices(&["Disabled", "Enabled", "GeoRAM"]);
        let en_dis = b.choices(&["Disabled", "Enabled"]);
        let sizes = b.choices(&["128 KB", "256 KB", "512 KB", "1 MB", "2 MB", "4 MB", "8 MB", "16 MB"]);
        let kernal = b.string("kernal.bin");
        b.table(
            at,
            &[
                (0xC3, CFG_TYPE_ENUM, "RAM Expansion Unit", "%s", en_dis_geo, 0, 2, 0),
                (0xC4, CFG_TYPE_ENUM, "REU Size", "%s", sizes, 0, 7, 4),
                (0xFE, CFG_TYPE_SEP, "", "", 0, 0, 0, 0),
                (0x71, CFG_TYPE_ENUM, "Command Interface", "%s", en_dis, 0, 1, 0),
                (0xE1, CFG_TYPE_STRFUNC, "Kernal ROM", "%s", 0, 0, 30, kernal),
                (0x60, CFG_TYPE_VALUE, "Delay", "%d ms", 0, 1, 100, 10),
                (0x61, CFG_TYPE_INFO, "Status", "%s", 0, 0, 32, 0),
            ],
        );
    }

    #[test]
    fn tables_are_found_with_ids_choices_and_defaults() {
        let mut b = Builder::new();
        c64_table(&mut b, BASE + 0x100);
        let tables = b.tables();
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.addr, BASE + 0x100);
        assert_eq!(t.defs.len(), 7);
        let reu = t.find("ram expansion unit").unwrap();
        assert_eq!((reu.id, reu.kind, reu.choices.len()), (0xC3, CFG_TYPE_ENUM, 3));
        assert_eq!(t.find("Kernal ROM").unwrap().default, DefaultValue::Text("kernal.bin".into()));
        assert_eq!(t.find("Delay").unwrap().default, DefaultValue::Int(10));
    }

    #[test]
    fn a_run_without_its_end_entry_is_no_table() {
        let mut b = Builder::new();
        c64_table(&mut b, BASE + 0x100);
        let end = BASE as usize + 0x100 + 7 * ENTRY;
        b.ram[end] = 0x33;
        b.ram[end + 1] = 0x77;
        assert!(b.tables().is_empty());
    }

    /// Two LED tables with the same items (C64U 1.1.0): the choice decides, the other store takes the rest.
    #[test]
    fn stores_resolve_shared_anchors() {
        let mut b = Builder::new();
        c64_table(&mut b, BASE + 0x100);
        let strip_modes = b.choices(&["Off", "Fixed Color", "Default"]);
        let strip_patterns = b.choices(&["SingleColor", "Serpentine"]);
        let kbd_modes = b.choices(&["Off", "Default"]);
        let kbd_patterns = b.choices(&["Single Color", "Circular"]);
        b.table(
            BASE + 0x400,
            &[
                (0x01, CFG_TYPE_ENUM, "LedStrip Mode", "%s", strip_modes, 0, 2, 0),
                (0x0B, CFG_TYPE_ENUM, "LedStrip Pattern", "%s", strip_patterns, 0, 1, 0),
            ],
        );
        b.table(
            BASE + 0x600,
            &[
                (0x01, CFG_TYPE_ENUM, "LedStrip Mode", "%s", kbd_modes, 0, 1, 0),
                (0x0B, CFG_TYPE_ENUM, "LedStrip Pattern", "%s", kbd_patterns, 0, 1, 0),
            ],
        );
        let tables = b.tables();
        let stores = stores(&tables);
        let addr = |name: &str| stores.get(name).map(|s| s.table.addr);
        assert_eq!(addr("LED Strip Settings"), Some(BASE + 0x400));
        assert_eq!(addr("keyboard lighting"), Some(BASE + 0x600));
        assert_eq!(addr("C64 and Cartridge Settings"), Some(BASE + 0x100));
        assert!(stores.missing.contains(&"Audio Mixer"));
    }

    #[test]
    fn parse_follows_the_firmware_reader() {
        let text = b"# comment\r\n; also\n\n[C64 and Cartridge Settings]\r\n  RAM Expansion Unit = Enabled \n\
                     Kernal ROM= my kernal.bin\n[Other]\nX=1\n";
        let lines = parse(text, "a.cfg").unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].origin, "a.cfg:5");
        assert_eq!(lines[0].store, "C64 and Cartridge Settings");
        assert_eq!(lines[0].item, "RAM Expansion Unit");
        assert_eq!(lines[0].value, " Enabled ");
        assert_eq!(lines[1].value, " my kernal.bin");
        assert_eq!(lines[2].store, "Other");
    }

    #[test]
    fn parse_refuses_malformed_lines() {
        let err = parse(b"X=1\n[C64\n[S]\nno equals\n=value\n", "b.cfg").unwrap_err().to_string();
        assert!(err.contains("b.cfg:1: \"X\" is not inside a [store] section"), "{err}");
        assert!(err.contains("b.cfg:2: no ']'"), "{err}");
        assert!(err.contains("b.cfg:4: no '='"), "{err}");
        assert!(err.contains("b.cfg:5: no item name"), "{err}");
    }

    fn resolve_text(text: &str) -> Result<(Vec<Record>, Vec<String>)> {
        let mut b = Builder::new();
        c64_table(&mut b, BASE + 0x100);
        let tables = b.tables();
        let lines = parse(text.as_bytes(), "t.cfg")?;
        resolve(&lines, &stores(&tables))
    }

    #[test]
    fn resolve_encodes_like_config_item_pack() {
        let (records, warnings) = resolve_text(
            "[c64 AND cartridge settings]\nram expansion unit=  enabled\nREU Size=1 MB\nKernal ROM=k.bin\n\
             Delay=  42xyz\nRAM Expansion Unit=GeoRAM\n",
        )
        .unwrap();
        assert!(warnings.is_empty());
        let page = 0x4336_3420;
        let rec = |id: u8| records.iter().find(|r| r.id == id).unwrap();
        assert_eq!(records.len(), 4, "the later REU line replaced the earlier");
        assert_eq!((rec(0xC3).page, rec(0xC3).kind, rec(0xC3).payload.clone()), (page, CFG_TYPE_ENUM, vec![2]));
        assert_eq!(rec(0xC3).text, "[C64 and Cartridge Settings] RAM Expansion Unit=GeoRAM");
        assert_eq!(rec(0xC4).payload, vec![3]);
        assert_eq!(rec(0xE1).payload, b"k.bin".to_vec());
        assert_eq!(rec(0x60).payload, vec![0, 0, 0, 42]);
    }

    #[test]
    fn resolve_warns_on_what_this_firmware_lacks() {
        let text = "[Tape Settings]\nTape Playback Rate=1\n[SwinSID]\nA=1\n[C64 and Cartridge Settings]\nNope=1\n";
        let (records, warnings) = resolve_text(text).unwrap();
        assert!(records.is_empty());
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("[Tape Settings] is not in this firmware"));
        assert!(warnings[1].contains("is not a store UE2 can set"));
        assert!(warnings[2].contains("has no item \"Nope\""));
    }

    #[test]
    fn resolve_is_stricter_than_the_firmware() {
        let err = resolve_text(
            "[C64 and Cartridge Settings]\nRAM Expansion Unit=On\nDelay=500\nDelay=abc\n\
             Kernal ROM=0123456789012345678901234567890\nStatus=x\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"On\" is not a choice (Disabled | Enabled | GeoRAM)"), "{err}");
        assert!(err.contains("500 is outside 1..100"), "{err}");
        assert!(err.contains("\"abc\" is not a number"), "{err}");
        assert!(err.contains("longer than 30 characters"), "{err}");
        assert!(err.contains("Status: not a setting"), "{err}");
    }

    #[test]
    fn template_lists_settings_with_defaults() {
        let mut b = Builder::new();
        c64_table(&mut b, BASE + 0x100);
        let tables = b.tables();
        let text = template(&stores(&tables), "fw");
        let reu = "[C64 and Cartridge Settings]\n; Disabled | Enabled | GeoRAM\nRAM Expansion Unit=Disabled\n";
        assert!(text.contains(reu));
        assert!(text.contains("; 128 KB | 256 KB | 512 KB | 1 MB | 2 MB | 4 MB | 8 MB | 16 MB\nREU Size=2 MB\n"));
        assert!(text.contains("; text, at most 30 characters\nKernal ROM=kernal.bin\n"));
        assert!(text.contains("; 1..100\nDelay=10\n"));
        assert!(!text.contains("Status"));
        // The template reads back as a valid file.
        let lines = parse(text.as_bytes(), "tpl").unwrap();
        resolve(&lines, &stores(&tables)).unwrap();
    }

    /// The upstream ELF (skipped without a firmware tree): the catalog against a real image (§9.3).
    #[test]
    fn upstream_firmware_has_the_catalog_stores() {
        let Some(root) = crate::loader::tests::firmware_root() else { return };
        let mut ram = vec![0; crate::bus::RAM_SIZE];
        let fw = crate::loader::load_firmware(&root.join(crate::loader::tests::FIRMWARE_ELF), &mut ram).unwrap();
        let tables = tables(&ram, &fw.segments);
        let stores = stores(&tables);
        assert_eq!(stores.missing, vec!["Keyboard Lighting"], "only the C64U store is missing");
        let id = |store: &str, item: &str| stores.get(store).unwrap().table.find(item).unwrap().id;
        assert_eq!(id("C64 and Cartridge Settings", "RAM Expansion Unit"), 0xC3);
        assert_eq!(id("C64 and Cartridge Settings", "Command Interface"), 0x71);
        assert_eq!(id("User Interface Settings", "Interface Type"), 0x08);
        // Stores sharing a page must not share ids, or their records would collide.
        let mut ids = std::collections::HashMap::new();
        for store in &stores.found {
            for def in store.table.defs.iter().filter(|d| d.is_setting()) {
                if let Some(other) = ids.insert((store.spec.page, def.id), store.spec.name) {
                    let same_table = other == store.spec.name
                        || stores.get(other).is_some_and(|o| std::ptr::eq(o.table, store.table));
                    let (page, id, name) = (store.spec.page, def.id, store.spec.name);
                    assert!(same_table, "page {page:08x} id {id:02x}: {other} and {name}");
                }
            }
        }
        // Every setting of the template resolves.
        let text = template(&stores, "upstream");
        resolve(&parse(text.as_bytes(), "tpl").unwrap(), &stores).unwrap();
    }
}
