//! S44: `--core FILE.toml`, a MultiCore core's profile, and `--handover` / `--icap` (docs/specs/S44-multicore-cores.md).
//!
//! ```toml
//! name = "amiga1200"          # for the log; default: the file's name
//! id = "AMIG"                 # the ID word at 0x101A0000
//! caps = 0xC0                 # CAPS at 0x101A0004 [default 0xC0: the pixel layer, RGB565]
//! board = "multicore"         # multicore: the ICAP loader answers 0x7A; monolith: it does not [default multicore]
//! [regs]                      # reset values: writes read back, a reset restores these
//! 0x0A0 = 0x01
//! [ro]                        # read-only: these read their value whatever is written
//! 0x00C = 0x18
//! symbols = "coreapp.elf"     # phase B: the ELF the core runs, for symbols and fault hooks after a core start
//! [handover]                  # the page at 0x03EFF000, the keys of --handover
//! core = "amiga1200"
//! id = "amiga1200"
//! path = "/Usb0/cores/amiga1200.cor"
//! video = "1080p"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use ue2_core::coreprof::{CoreBoard, CoreProfile, Handover};

/// Reads and checks a profile file.
pub fn load(path: &Path) -> Result<CoreProfile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("--core {}", path.display()))?;
    let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let dir = path.parent().unwrap_or(Path::new("."));
    parse_in(&text, &stem, dir).with_context(|| format!("--core {}", path.display()))
}

#[cfg(test)]
fn parse(text: &str, stem: &str) -> Result<CoreProfile> {
    parse_in(text, stem, Path::new("."))
}

/// A profile whose relative `symbols` path is taken from `dir` (the profile file's directory).
fn parse_in(text: &str, stem: &str, dir: &Path) -> Result<CoreProfile> {
    let doc: toml::Table = toml::from_str(text)?;
    for key in doc.keys() {
        if !["name", "id", "caps", "board", "regs", "ro", "handover", "symbols"].contains(&key.as_str()) {
            bail!("unknown key '{key}' (name, id, caps, board, regs, ro, handover, symbols)");
        }
    }
    let name = match doc.get("name") {
        Some(v) => v.as_str().context("name: a string")?.to_string(),
        None => stem.to_string(),
    };
    let id = doc.get("id").and_then(|v| v.as_str()).context("id: the core's four ID characters, e.g. id = \"AMIG\"")?;
    let mut p = CoreProfile::new(&name, CoreProfile::parse_id(id).map_err(anyhow::Error::msg)?);
    if let Some(v) = doc.get("caps") {
        p.caps = byte(v).context("caps")?;
    }
    if let Some(v) = doc.get("board") {
        p.board = match v.as_str() {
            Some("multicore") => CoreBoard::MultiCore,
            Some("monolith") => CoreBoard::Monolith,
            _ => bail!("board: \"multicore\" or \"monolith\""),
        };
    }
    if let Some(v) = doc.get("symbols") {
        p.symbols = Some(dir.join(v.as_str().context("symbols: the path of the core application's ELF")?));
    }
    p.regs = registers(&doc, "regs")?;
    p.ro = registers(&doc, "ro")?;
    if let Some(t) = doc.get("handover") {
        let t = t.as_table().context("handover: a table")?;
        let mut h = Handover::default();
        for (k, v) in t {
            let v = match v {
                toml::Value::String(s) => s.clone(),
                other => bail!("handover.{k}: a string, not {other}"),
            };
            h.set(k, &v).map_err(anyhow::Error::msg)?;
        }
        h.check().map_err(anyhow::Error::msg)?;
        p.handover = Some(h);
    }
    p.check().map_err(anyhow::Error::msg)?;
    Ok(p)
}

fn byte(v: &toml::Value) -> Result<u8> {
    let n = v.as_integer().context("an integer, e.g. 0x18")?;
    u8::try_from(n).with_context(|| format!("{n} is not a byte"))
}

/// A table of `offset = value`, the offset written as a key (`0x0A0` or `160`), multi-byte registers byte by byte.
fn registers(doc: &toml::Table, key: &str) -> Result<BTreeMap<u32, u8>> {
    let mut out = BTreeMap::new();
    let Some(t) = doc.get(key) else { return Ok(out) };
    for (k, v) in t.as_table().with_context(|| format!("{key}: a table of offset = value"))? {
        let off = match k.strip_prefix("0x").or_else(|| k.strip_prefix("0X")) {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => k.parse(),
        }
        .with_context(|| format!("{key}.{k}: an offset, e.g. 0x0A0"))?;
        out.insert(off, byte(v).with_context(|| format!("{key}.{k}"))?);
    }
    Ok(out)
}

/// The run's core, ICAP and hand-over from `--core`, `--icap` and `--handover`: the flag's fields go over the
/// profile's page; the ICAP loader is there with `--icap` or a multicore profile.
pub fn configure(
    core: Option<&Path>,
    icap: bool,
    handover: Option<&str>,
) -> Result<(Option<CoreProfile>, bool, Option<Handover>)> {
    let profile = core.map(load).transpose()?;
    let mut page = profile.as_ref().and_then(|p| p.handover.clone());
    if let Some(spec) = handover {
        page.get_or_insert_with(Handover::default).apply(spec).map_err(anyhow::Error::msg)?;
    }
    let icap = icap || profile.as_ref().is_some_and(|p| p.board == CoreBoard::MultiCore);
    Ok((profile, icap, page))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A1200: &str = r#"
id = "AMIG"
[ro]
0x00C = 0x18
[regs]
0x0A0 = 1
161 = 2
[handover]
core = "amiga1200"
id = "amiga1200"
path = "/Usb0/cores/amiga1200.cor"
video = "720p"
"#;

    #[test]
    fn a_profile_file() {
        let p = parse(A1200, "amiga1200").unwrap();
        assert_eq!((p.name.as_str(), p.id, p.caps, p.board), ("amiga1200", *b"AMIG", 0xC0, CoreBoard::MultiCore));
        assert_eq!(p.ro.get(&0x00C), Some(&0x18));
        assert_eq!((p.regs.get(&0x0A0), p.regs.get(&0x0A1)), (Some(&1), Some(&2)));
        let h = p.handover.unwrap();
        assert_eq!((h.core_name.as_str(), h.video), ("amiga1200", 0x8000_0001));
    }

    #[test]
    fn refusals() {
        assert!(parse("id = \"AMIGA\"", "x").is_err());
        assert!(parse("", "x").is_err(), "no id");
        assert!(parse("id = \"AMIG\"\n[regs]\n0x004 = 1", "x").is_err(), "CAPS is the host's");
        assert!(parse("id = \"AMIG\"\n[regs]\n0x10000 = 1", "x").is_err());
        assert!(parse("id = \"AMIG\"\n[regs]\n0x0A0 = 256", "x").is_err());
        assert!(parse("id = \"AMIG\"\nboard = \"u64\"", "x").is_err());
        assert!(parse("id = \"AMIG\"\ncolour = 1", "x").is_err());
    }

    #[test]
    fn the_flag_goes_over_the_profile() {
        let dir = std::env::temp_dir().join(format!("ue2-coreprofile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("amiga1200.toml");
        std::fs::write(&f, A1200).unwrap();
        let (p, icap, h) = configure(Some(&f), false, Some("video=1080p,host=12345678")).unwrap();
        let h = h.unwrap();
        assert!(p.is_some() && icap);
        assert_eq!((h.core_name.as_str(), h.video, h.host_key), ("amiga1200", 0x8000_0002, 0x1234_5678));
        std::fs::write(&f, "id = \"TRX2\"\nboard = \"monolith\"").unwrap();
        let (_, icap, h) = configure(Some(&f), false, None).unwrap();
        assert!(!icap && h.is_none());
        let (p, icap, h) = configure(None, true, Some("core=c128")).unwrap();
        assert!(p.is_none() && icap && h.unwrap().core_name == "c128");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
