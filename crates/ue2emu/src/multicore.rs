//! S44 phase B: the MultiCore host app starts cores and takes them back (docs/specs/S44-multicore-cores.md §7).
//!
//! The ICAP loader (`ue2_core::devices::misc::Icap`) reports what the host app does through it. This module acts on
//! those reports, between two instructions:
//! - A decouple cuts the core off, as S41's `core absent` does.
//! - A partial streamed through the loader is found among the `.cor` bundles of `--cor-dir` by its SHA-256.
//! - When the decouple is given back, that core comes up fresh, with its profile from `--core-dir`.
//! - An IPROG reconfigures the FPGA. WBSTAR 0 boots the host bitstream and `--firmware` again. Any other WBSTAR boots
//!   the large core whose bitstream lies there in the flash, and starts its application through the boot magic.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use ue2_core::coreprof::CoreProfile;
use ue2_core::devices::misc::IcapEvent;
use ue2_core::Machine;

/// The boot ROM's magic (TRX64U-MultiCore host/bootrom/bootrom.c:54-56, Gideon's bootloader the same).
const BOOT_MAGIC_ADDR: usize = 0xFFFC;
const BOOT_JUMP_ADDR: usize = 0xFFF8;
const BOOT_MAGIC: u32 = 0x1571_BABE;

/// The ID word of the C64 core: a profile with it is the C64 core with all of its pages (`set_core(None)`).
const C64_ID: [u8; 4] = *b"TRX2";

/// One bundle of `--cor-dir`, as its manifest describes it (tools/cor_pack.py).
#[derive(Clone, Debug)]
pub struct Cor {
    pub file: PathBuf,
    pub name: String,
    /// The core's id (large cores; a DFX manifest has none).
    pub id: String,
    pub large: bool,
    /// Large: the DDR2 address map its CPU sees (`gideon_bl4` or `identity`).
    pub gideon_map: bool,
    /// The SHA-256 (lowercase hex) and size of what the configuration logic gets: the partial (DFX) or the .bit
    /// (large).
    pub sha: String,
    pub size: usize,
}

/// Reads the manifest of a `.cor` (header "TRXCOR01", u32 length, records type/length/value; record 1 the JSON).
pub fn read_cor(path: &Path) -> Result<Cor> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 20 || &bytes[..8] != b"TRXCOR01" {
        bail!("{}: not a .cor bundle", path.display());
    }
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let (kind, len) = (u32_at(12), u32_at(16));
    if kind != 1 || 20 + len > bytes.len() {
        bail!("{}: the first record is not the manifest", path.display());
    }
    let m: serde_json::Value = serde_json::from_slice(&bytes[20..20 + len])
        .with_context(|| format!("{}: the manifest is not JSON", path.display()))?;
    let text = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let num = |k: &str| m.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let large = text("kind") == "large";
    let (sha, size) = if large { (text("bit_sha256"), num("bit_size")) } else { (text("partial_sha256"), num("partial_size")) };
    if sha.len() != 64 || size == 0 {
        bail!("{}: the manifest names no {} with its SHA-256 and size", path.display(), if large { "bitstream" } else { "partial" });
    }
    Ok(Cor {
        file: path.to_path_buf(),
        name: text("name"),
        id: text("id"),
        large,
        gideon_map: text("map") == "gideon_bl4",
        sha: sha.to_ascii_lowercase(),
        size,
    })
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The phase B state of one run.
pub struct MultiCore {
    cors: Vec<Cor>,
    core_dir: Option<PathBuf>,
    /// The core of the run's start (`--core`, None: the C64 core): the host bitstream's, which every IPROG with
    /// WBSTAR 0 brings back.
    start: Option<CoreProfile>,
    /// The core a matched partial brings when the decouple is given back: Some(None) is the C64 core.
    pending: Option<Option<CoreProfile>>,
    /// The running core sees DDR under the gideon_bl4 map (UE2 has permuted it).
    gideon_map: bool,
}

impl MultiCore {
    /// The bundles of `cor_dir` (its `*.cor` files, in name order); `core_dir` holds the profiles.
    pub fn new(cor_dir: Option<&Path>, core_dir: Option<&Path>, start: Option<CoreProfile>) -> Result<Self> {
        let mut cors = Vec::new();
        if let Some(dir) = cor_dir {
            let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
                .with_context(|| format!("--cor-dir {}", dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("cor")))
                .collect();
            files.sort();
            for f in files {
                let cor = read_cor(&f)?;
                eprintln!(
                    "cor: {} {:?}{}, {} {} bytes",
                    f.display(),
                    cor.name,
                    if cor.large { format!(" large (id {:?}, map {})", cor.id, if cor.gideon_map { "gideon_bl4" } else { "identity" }) } else { String::new() },
                    if cor.large { "bitstream" } else { "partial" },
                    cor.size
                );
                cors.push(cor);
            }
        }
        Ok(MultiCore { cors, core_dir: core_dir.map(Path::to_path_buf), start, pending: None, gideon_map: false })
    }

    /// The profile of `cor`: `<core-dir>/<id>.toml`, else `<core-dir>/<name>.toml`. A profile with the ID "TRX2" is the
    /// C64 core (None).
    fn profile(&self, cor: &Cor) -> Result<Option<CoreProfile>> {
        let dir = self.core_dir.as_deref().ok_or_else(|| anyhow!("core {:?}: no --core-dir to find its profile in", cor.name))?;
        let file = [&cor.id, &cor.name]
            .into_iter()
            .filter(|k| !k.is_empty())
            .map(|k| dir.join(format!("{k}.toml")))
            .find(|f| f.is_file())
            .ok_or_else(|| anyhow!("core {:?}: no profile {}/{}.toml", cor.name, dir.display(), if cor.id.is_empty() { &cor.name } else { &cor.id }))?;
        let p = crate::coreprofile::load(&file)?;
        Ok((p.id != C64_ID).then_some(p))
    }

    /// Acts on the loader's events; an error halts the run (an IPROG to nothing UE2 can boot).
    pub fn poll<F>(&mut self, machine: &mut Machine, mut present: F) -> Result<()>
    where
        F: FnMut(&mut Machine, bool) -> Result<String, String>,
    {
        for event in machine.take_icap_events() {
            match event {
                IcapEvent::Decouple(true) => {
                    if !machine.core_is_absent() {
                        let line = machine.core_absent().map_err(|e| anyhow!(e))?;
                        eprintln!("icap: decouple: {line}");
                    }
                }
                IcapEvent::Streamed(bytes) => {
                    let sha = sha_hex(&bytes);
                    match self.cors.iter().find(|c| !c.large && c.size == bytes.len() && c.sha == sha).cloned() {
                        Some(cor) => {
                            eprintln!("icap: the partial of {:?} ({}), {} bytes", cor.name, cor.file.display(), bytes.len());
                            self.pending = Some(self.profile(&cor)?);
                        }
                        None => eprintln!(
                            "icap: a partial of {} bytes, SHA-256 {sha}, is in no .cor of --cor-dir: the core stays as it was",
                            bytes.len()
                        ),
                    }
                }
                IcapEvent::Decouple(false) => {
                    if machine.core_is_absent() {
                        let core = match self.pending.take() {
                            Some(core) => core,
                            None => machine.cfg.core.clone(),
                        };
                        self.swap(machine, core, &mut present, true)?;
                    }
                }
                IcapEvent::Iprog(0) => self.iprog_host(machine, &mut present)?,
                IcapEvent::Iprog(wbstar) => self.iprog_large(machine, wbstar, &mut present)?,
            }
        }
        Ok(())
    }

    /// The core goes and `core` comes up fresh: S41's swap, then the block's profile.
    fn swap<F>(&self, machine: &mut Machine, core: Option<CoreProfile>, present: &mut F, symbols: bool) -> Result<()>
    where
        F: FnMut(&mut Machine, bool) -> Result<String, String>,
    {
        if !machine.core_is_absent() {
            machine.core_absent().map_err(|e| anyhow!(e))?;
        }
        present(machine, core.is_none()).map_err(|e| anyhow!(e))?;
        machine.set_core(core.as_ref());
        if symbols {
            self.symbols(machine, core.as_ref())?;
        }
        let what = core.as_ref().map_or("the C64 core (TRX2)".to_string(), |p| {
            format!("{} (ID {})", p.name, String::from_utf8_lossy(&p.id))
        });
        eprintln!("icap: core {what} from power-up");
        Ok(())
    }

    /// The symbols of the program the new core runs: the profile's ELF, or none (the hooks off); the host's own core
    /// (`start`) runs the host's image, whose symbols `iprog_host` loads with it.
    fn symbols(&self, machine: &mut Machine, core: Option<&CoreProfile>) -> Result<()> {
        let symbols = match core.and_then(|p| p.symbols.as_deref()) {
            Some(elf) => ue2_core::symbols::Symbols::from_elf(elf).with_context(|| format!("symbols {}", elf.display()))?,
            None => {
                eprintln!("icap: no symbols for the new core's program (a profile's `symbols`): fault hooks off");
                ue2_core::symbols::Symbols::empty()
            }
        };
        machine.set_symbols(symbols);
        Ok(())
    }

    fn iprog_host<F>(&mut self, machine: &mut Machine, present: &mut F) -> Result<()>
    where
        F: FnMut(&mut Machine, bool) -> Result<String, String>,
    {
        if std::mem::take(&mut self.gideon_map) {
            machine.remap_ddr(false);
        }
        self.pending = None;
        // The host's image comes back with its own symbols below.
        self.swap(machine, self.start.clone(), present, false)?;
        let entry = machine.reload_firmware()?;
        machine.reconfigure(entry);
        let elf = machine.cfg.elf.clone();
        let symbols = match ue2_core::symbols::Symbols::from_elf(&elf) {
            Ok(s) => s,
            Err(_) => ue2_core::symbols::Symbols::empty(),
        };
        machine.set_symbols(symbols);
        eprintln!(
            "icap: IPROG WBSTAR 0: the host bitstream, {} from {entry:#010x}, DDR kept{}",
            machine.cfg.elf.display(),
            if ue2_core::coreprof::menu_requested(&machine.bus.ram) { ", the menu marker set" } else { "" }
        );
        Ok(())
    }

    fn iprog_large<F>(&mut self, machine: &mut Machine, wbstar: u32, present: &mut F) -> Result<()>
    where
        F: FnMut(&mut Machine, bool) -> Result<String, String>,
    {
        let flash = machine.flash_array().context("no SPI flash")?;
        let at = wbstar as usize;
        let cor = self
            .cors
            .iter()
            .filter(|c| c.large && at + c.size <= flash.len())
            .find(|c| sha_hex(&flash[at..at + c.size]) == c.sha)
            .cloned()
            .ok_or_else(|| anyhow!("IPROG WBSTAR {wbstar:#010x}: the flash there holds the bitstream of no large .cor of --cor-dir"))?;
        let core = self.profile(&cor)?;
        if std::mem::take(&mut self.gideon_map) {
            machine.remap_ddr(false);
        }
        self.pending = None;
        self.swap(machine, core, present, true)?;
        if cor.gideon_map {
            machine.remap_ddr(true);
            self.gideon_map = true;
        }
        // The boot ROM (bootrom.c:372-379): the magic first; it clears it and jumps to the word at 0xFFF8.
        let ram = &mut machine.bus.ram;
        let word = |ram: &[u8], at: usize| u32::from_le_bytes(ram[at..at + 4].try_into().unwrap());
        if word(ram, BOOT_MAGIC_ADDR) != BOOT_MAGIC {
            bail!(
                "IPROG WBSTAR {wbstar:#010x}: {:?} has no boot magic at 0xFFFC; its boot ROM would load the application from flash, which UE2 does not model",
                cor.name
            );
        }
        let entry = word(ram, BOOT_JUMP_ADDR);
        ram[BOOT_MAGIC_ADDR..BOOT_MAGIC_ADDR + 4].fill(0);
        machine.reconfigure(entry);
        eprintln!(
            "icap: IPROG WBSTAR {wbstar:#010x}: the large core {:?} ({}), map {}, boot magic: {entry:#010x}",
            cor.name,
            cor.file.display(),
            if cor.gideon_map { "gideon_bl4" } else { "identity" }
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle as cor_pack.py writes it: the header, the manifest record, then `parts` as records 2.. in order.
    fn cor(manifest: &str, parts: &[(u32, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, v) in std::iter::once((1u32, manifest.as_bytes())).chain(parts.iter().copied()) {
            body.extend(t.to_le_bytes());
            body.extend((v.len() as u32).to_le_bytes());
            body.extend(v);
        }
        let mut out = b"TRXCOR01".to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn manifests_of_both_kinds() {
        let dir = std::env::temp_dir().join(format!("ue2-multicore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = [1u8, 2, 3, 4];
        let m = format!(r#"{{"name":"vc20","partial_sha256":"{}","partial_size":4}}"#, sha_hex(&partial));
        std::fs::write(dir.join("vc20.cor"), cor(&m, &[(2, &partial)])).unwrap();
        let m = format!(
            r#"{{"bit_sha256":"{}","bit_size":4,"id":"amiga1200","kind":"large","map":"gideon_bl4","name":"Amiga 1200"}}"#,
            "AB".repeat(32)
        );
        std::fs::write(dir.join("a1200.cor"), cor(&m, &[(5, &partial)])).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a bundle").unwrap();
        let mc = MultiCore::new(Some(&dir), None, None).unwrap();
        assert_eq!(mc.cors.len(), 2);
        let (a, v) = (&mc.cors[0], &mc.cors[1]);
        assert!(a.large && a.gideon_map && a.id == "amiga1200" && a.sha == "ab".repeat(32));
        assert!(!v.large && v.name == "vc20" && v.size == 4);
        assert!(mc.profile(v).is_err(), "no --core-dir");
        std::fs::write(dir.join("bad.cor"), b"TRXCOR00").unwrap();
        assert!(MultiCore::new(Some(&dir), None, None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
