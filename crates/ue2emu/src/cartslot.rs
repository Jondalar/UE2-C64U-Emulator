//! `--cart-slot FILE.crt[,rw|,save=OUT.crt][,flash-decode=11|15|both]`: a cartridge in the physical expansion port
//! (c64-bridge slot.rs), and how its flash and EEPROM changes reach a CRT file; the `cart-info` and `cart-save` control
//! commands (docs/status/cart-slot.md).
//!
//! - Default (`,ro`): the source file is never written.
//! - `,rw`: the current cartridge goes back into the source file on a clean exit, and while running once flash and
//!   EEPROM have been quiet for [`DEBOUNCE_MS`] emulated. Each write goes to a temporary file that replaces the
//!   target; before the first one the original is copied to `FILE.crt.bak`.
//! - `,save=OUT.crt`: the same debounced writes into OUT.crt, without a backup; a clean exit always writes it.
//! - `,flash-decode=11|15|both`: the command addresses a flash cartridge decodes (default `both`).

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use ue2_core::machine::Machine;

/// Emulated quiet time after the last flash or EEPROM change before a running `,rw` / `,save=` writes the CRT.
pub const DEBOUNCE_MS: u64 = 2000;

/// Where the cartridge's changes go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Persist {
    ReadOnly,
    WriteBack,
    SaveTo(PathBuf),
}

/// A parsed `--cart-slot` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CartSlotSpec {
    pub path: PathBuf,
    pub persist: Persist,
    /// `flash-decode=`: "11", "15" or "both" (the default).
    pub flash_decode: String,
}

impl FromStr for CartSlotSpec {
    type Err = String;

    /// Options are taken from the end while they parse as options, so the file name may contain commas; a `save=`
    /// file name may not.
    fn from_str(s: &str) -> Result<Self, String> {
        let (mut rest, mut persist, mut decode) = (s, None, None);
        while let Some((head, option)) = rest.rsplit_once(',') {
            let value = match option {
                "rw" => Persist::WriteBack,
                "ro" => Persist::ReadOnly,
                _ if option.starts_with("save=") => {
                    let out = &option["save=".len()..];
                    if out.is_empty() {
                        return Err("',save=' needs a file name".into());
                    }
                    Persist::SaveTo(PathBuf::from(out))
                }
                _ if option.starts_with("flash-decode=") => {
                    let d = &option["flash-decode=".len()..];
                    if !matches!(d, "11" | "15" | "both") {
                        return Err(format!("flash-decode must be 11, 15 or both, not {d:?}"));
                    }
                    if decode.replace(d.to_string()).is_some() {
                        return Err("flash-decode is given twice".into());
                    }
                    rest = head;
                    continue;
                }
                _ => break,
            };
            if persist.replace(value).is_some() {
                return Err("give one of ro, rw and save=".into());
            }
            rest = head;
        }
        if rest.is_empty() {
            return Err("needs a .crt file".into());
        }
        Ok(CartSlotSpec {
            path: PathBuf::from(rest),
            persist: persist.unwrap_or(Persist::ReadOnly),
            flash_decode: decode.unwrap_or_else(|| "both".into()),
        })
    }
}

impl CartSlotSpec {
    /// The file the changes go to, if any.
    pub fn target(&self) -> Option<&Path> {
        match &self.persist {
            Persist::ReadOnly => None,
            Persist::WriteBack => Some(&self.path),
            Persist::SaveTo(out) => Some(out),
        }
    }

    /// `ro`, `rw` or `save=OUT`.
    pub fn mode(&self) -> String {
        match &self.persist {
            Persist::ReadOnly => "ro".into(),
            Persist::WriteBack => "rw".into(),
            Persist::SaveTo(out) => format!("save={}", out.display()),
        }
    }

    /// The CRT file's bytes (read by `runner::attach_trx64`, which needs the `trx64` feature).
    #[cfg_attr(not(feature = "trx64"), allow(dead_code))]
    pub fn read(&self) -> Result<Vec<u8>> {
        fs::read(&self.path).with_context(|| format!("--cart-slot: read {}", self.path.display()))
    }
}

/// A control request for the cartridge (`runner::Command::Cart`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CartRequest {
    /// `cart-info`.
    Info,
    /// `cart-save <path>`.
    Save(PathBuf),
}

/// Receives the result lines of a [`CartRequest`], or the reason it failed.
pub type CartDone = Sender<std::result::Result<Vec<String>, String>>;

/// Write `bytes` to `path` through a temporary file in the same directory, renamed over the target. With `keep_bak`
/// an existing target is copied to `<path>.bak` first.
pub fn write_crt(path: &Path, bytes: &[u8], keep_bak: bool) -> Result<()> {
    let name = path.file_name().with_context(|| format!("{} is not a file name", path.display()))?;
    if keep_bak && path.is_file() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        fs::copy(path, &bak).with_context(|| format!("keep {}", Path::new(&bak).display()))?;
    }
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let written = fs::write(&tmp, bytes).and_then(|()| fs::File::open(&tmp)?.sync_all()).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("write {}", path.display()))
}

/// The emulation thread's side: debounced write-back, the final write on a clean exit, `cart-info` and `cart-save`.
pub struct CartSlot {
    spec: Option<CartSlotSpec>,
    /// Flash/EEPROM generation of the last write to the target (or at insertion).
    saved: u64,
    /// Generation at the last poll, and the emulated ms it was first seen.
    seen: u64,
    changed_ms: u64,
    /// The `.bak` of a `,rw` source exists.
    bak_done: bool,
}

impl CartSlot {
    pub fn new(spec: Option<CartSlotSpec>, machine: &mut Machine) -> CartSlot {
        let generation = machine.c64_cart_slot().map_or(0, |s| s.generation());
        CartSlot { spec, saved: generation, seen: generation, changed_ms: 0, bak_done: false }
    }

    /// After each slice: write the target once the cartridge's changes have been quiet for [`DEBOUNCE_MS`].
    pub fn poll(&mut self, machine: &mut Machine, now_ms: u64) {
        if self.spec.as_ref().and_then(CartSlotSpec::target).is_none() {
            return;
        }
        let Some(generation) = machine.c64_cart_slot().map(|s| s.generation()) else {
            return;
        };
        if generation != self.seen {
            (self.seen, self.changed_ms) = (generation, now_ms);
        } else if generation != self.saved && now_ms >= self.changed_ms + DEBOUNCE_MS {
            self.write_target(machine);
        }
    }

    /// On a clean exit: `,save=` always writes its file; `,rw` rewrites the source only if the cartridge changed
    /// since the last write.
    pub fn finish(&mut self, machine: &mut Machine) {
        let Some(persist) = self.spec.as_ref().map(|s| &s.persist) else { return };
        let due = match persist {
            Persist::ReadOnly => false,
            Persist::SaveTo(_) => machine.c64_cart_slot().is_some(),
            Persist::WriteBack => machine.c64_cart_slot().is_some_and(|s| s.generation() != self.saved),
        };
        if due {
            self.write_target(machine);
        }
    }

    fn write_target(&mut self, machine: &mut Machine) {
        let Some(spec) = &self.spec else { return };
        let Some(target) = spec.target().map(Path::to_path_buf) else { return };
        let keep_bak = spec.persist == Persist::WriteBack && !self.bak_done;
        match self.save(machine, &target, keep_bak) {
            Ok((bytes, generation)) => {
                self.bak_done |= keep_bak;
                eprintln!("cart-slot: wrote {} ({bytes} bytes, flash generation {generation})", target.display());
            }
            Err(e) => {
                // Not retried until the cartridge changes again.
                self.saved = self.seen;
                eprintln!("cart-slot: {e:#}");
            }
        }
    }

    /// Write the cartridge to `path`; returns the size and the generation written.
    fn save(&mut self, machine: &mut Machine, path: &Path, keep_bak: bool) -> Result<(usize, u64)> {
        let slot = machine.c64_cart_slot().context("no cartridge in the expansion port")?;
        let image = slot.crt_image().map_err(anyhow::Error::msg)?;
        // After the image: a flash erase that was due completes while it is taken.
        let generation = slot.generation();
        write_crt(path, &image, keep_bak)?;
        if self.spec.as_ref().and_then(CartSlotSpec::target).is_some_and(|t| same_file(t, path)) {
            (self.saved, self.seen) = (generation, generation);
        }
        Ok((image.len(), generation))
    }

    /// Run a control request and send its result.
    pub fn request(&mut self, machine: &mut Machine, req: CartRequest, done: CartDone) {
        let result = match req {
            CartRequest::Info => self.info(machine),
            CartRequest::Save(path) => self
                .save(machine, &path, false)
                .map(|(bytes, generation)| {
                    vec![format!("saved: {}", path.display()), format!("bytes: {bytes}"), format!("generation: {generation}")]
                })
                .map_err(|e| format!("cart-save: {e:#}")),
        };
        let _ = done.send(result);
    }

    fn info(&self, machine: &mut Machine) -> std::result::Result<Vec<String>, String> {
        let slot = machine.c64_cart_slot().ok_or("cart-info: no cartridge in the expansion port (--cart-slot)")?;
        let i = slot.info();
        let mode = match (i.exrom, i.game) {
            (0, 0) => "16K",
            (0, _) => "8K",
            (_, 0) => "ULTIMAX",
            _ => "off",
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        let (source, persist) = match &self.spec {
            Some(spec) => (spec.path.display().to_string(), spec.mode()),
            None => ("-".into(), "ro".into()),
        };
        Ok(vec![
            format!("type: {}", i.family),
            format!("name: {}", i.name),
            format!("hardware: {}", i.hw_type),
            format!("model: {}", i.model),
            format!("banks: {}", i.banks),
            format!("exrom: {}", i.exrom),
            format!("game: {}", i.game),
            format!("mode: {mode}"),
            format!("cart_detect: 0x{:02X}", i.game | (i.exrom << 1)),
            format!("bus_internal: 0x{:02X}", i.bus_internal),
            format!("bus_external: 0x{:02X}", i.bus_external),
            format!("bus_bridge: 0x{:02X}", i.bus_bridge),
            format!("flash_decode: {}", i.flash_decode.as_deref().unwrap_or("-")),
            format!("source: {source}"),
            format!("persist: {persist}"),
            format!("writable: {}", yes(i.writable)),
            format!("dirty: {}", yes(i.dirty)),
            format!("generation: {}", i.generation),
            format!("saved_generation: {}", self.saved),
            format!("unsaved: {}", yes(i.writable && i.generation != self.saved)),
        ])
    }
}

/// Whether `a` and `b` name the same file (both resolved when they exist).
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_modes_and_flash_decode() {
        let spec = |s: &str| s.parse::<CartSlotSpec>();
        let ro = |p: &str| CartSlotSpec { path: p.into(), persist: Persist::ReadOnly, flash_decode: "both".into() };
        assert_eq!(spec("a.crt").unwrap(), ro("a.crt"));
        assert_eq!(spec("dir,x/a.crt,ro").unwrap(), ro("dir,x/a.crt"));
        assert_eq!(spec("a.crt,rw").unwrap().persist, Persist::WriteBack);
        let save = spec("in,1.crt,save=out/b.crt").unwrap();
        assert_eq!((save.path.to_str(), save.target(), save.mode()), (Some("in,1.crt"), Some(Path::new("out/b.crt")), "save=out/b.crt".into()));
        let both = spec("a.crt,rw,flash-decode=15").unwrap();
        assert_eq!((both.persist, both.flash_decode.as_str()), (Persist::WriteBack, "15"));
        assert_eq!(spec("a.crt,flash-decode=11,save=o.crt").unwrap().flash_decode, "11");
        assert!(spec("a.crt,save=").is_err());
        assert!(spec(",rw").is_err());
        assert!(spec("a.crt,rw,ro").unwrap_err().contains("one of"));
        assert!(spec("a.crt,flash-decode=16").unwrap_err().contains("11, 15 or both"));
        assert_eq!(spec("a.crt").unwrap().target(), None);
    }

    #[test]
    fn write_crt_replaces_the_file_and_keeps_a_backup() {
        let dir = std::env::temp_dir().join(format!("ue2-cartslot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.crt");
        fs::write(&path, b"old").unwrap();
        write_crt(&path, b"new", true).unwrap();
        assert_eq!((fs::read(&path).unwrap(), fs::read(dir.join("game.crt.bak")).unwrap()), (b"new".to_vec(), b"old".to_vec()));
        write_crt(&path, b"newer", false).unwrap();
        assert_eq!(fs::read(dir.join("game.crt.bak")).unwrap(), b"old", "no second backup");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
        assert!(write_crt(&dir.join("missing/x.crt"), b"x", false).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
