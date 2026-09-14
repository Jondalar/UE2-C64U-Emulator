//! `--c64-roms [DIR]` (`run`, `install`): the C64 KERNAL, BASIC and character ROMs in `/flash/roms` of the flash
//! image before the firmware runs, as the menu's "Set as ... ROM" leaves them, so the first boot reaches BASIC
//! `READY.` (docs/status/c64.md). Firmware paths are under firmware/1541ultimate/software.
//!
//! What the firmware needs:
//! - `C64::init_system_roms` loads KERNAL (exactly 8192 bytes, else the "shipped without System ROMs" placeholder),
//!   BASIC (8192) and CHAR (4096, else the built-in font) from `/flash/roms` (`ROMS_DIRECTORY`, c64.h:12;
//!   c64.cc:1091-1108) under the file names its config store selects.
//! - Those are the string items 0xE1-0xE3 (c64.h:232-234) of the store "C64 and Cartridge Settings", page id
//!   0x43363420 (c64.cc:132), at most 30 characters, defaults `kernal.bin`, `basic.bin`, `chars.bin` (c64.cc:84-86).
//! - "Set as ... ROM" copies the chosen file into `/flash/roms` under its own name and stores that name
//!   (filetype_bin.cc:166-199, c64.cc:283-297). Writing the files under the names the store already selects has the
//!   same effect without a config write; a flash without the store selects the defaults.
//! - The store is the first config page carrying its id, else `/flash/config/page_43363420.bin` (config.cc:127-152,
//!   426-431). Items are `id type len payload` up to id 0xFF; a wrong type is skipped and the last match wins
//!   (config.cc:398-424, 671-699).
//! - `/flash` is a FAT volume without a partition table (disk.cc:72-82) in `FLASH_ID_FLASHDRIVE`
//!   (w25q_flash.cc:51-63, 78-106) with 4096-byte sectors (blockdev_flash.cc:14-41). A volume that does not mount is
//!   formatted at boot (blockdev_flash.cc:170-178), so an erased region first gets that same format here ([`mkfs`]).
//!   Region contents that are neither a volume nor erased are refused.

use std::fs::OpenOptions;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, ensure, Context, Result};
use clap::Args;
use fatfs::{FileSystem, FsOptions};
use ue2_core::devices::flash::FLASH_SIZE;

#[derive(Args)]
pub struct C64RomsArgs {
    /// Put the C64 KERNAL, BASIC and CHAR ROMs from DIR into /flash/roms of the flash image, as the menu's "Set as ...
    /// ROM" would (run: before boot; install: after the updater) [DIR default: the --roms directory]
    #[arg(long = "c64-roms", value_name = "DIR")]
    c64_roms: Option<Option<PathBuf>>,
    /// With --c64-roms: replace a /flash/roms file whose content differs instead of keeping it
    #[arg(long = "c64-roms-force", requires = "c64_roms")]
    c64_roms_force: bool,
}

impl C64RomsArgs {
    /// [`write_roms`] into `flash` when `--c64-roms` was given; `roms` (the firmware roms directory) is the default DIR.
    pub fn apply(&self, flash: Option<&Path>, capabilities: u32, roms: &Path) -> Result<()> {
        let Some(dir) = &self.c64_roms else {
            return Ok(());
        };
        let flash = flash.context("--c64-roms needs --flash: the ROMs are written into the flash image file")?;
        let report = write_roms(flash, capabilities, dir.as_deref().unwrap_or(roms), self.c64_roms_force)
            .context("--c64-roms")?;
        for line in &report.lines {
            eprintln!("c64-roms: {line}");
        }
        Ok(())
    }
}

/// Flash sector = FAT sector of the flash disk (s25fl_l_flash.cc:79-83, blockdev_flash.cc:14-41).
const SECTOR: usize = 0x1000;
/// Config page `p` is the sector at 0xFE8000 + p × 4 KiB, 24 pages (w25q_flash.cc:261-267, s25fl_l_flash.h:9).
const CONFIG_BASE: usize = 0xFE_8000;
const CONFIG_PAGES: usize = 24;
/// Logical config page (w25q_flash.cc:251-254).
const CONFIG_PAGE_SIZE: usize = 512;
/// "C64 and Cartridge Settings" (c64.cc:132).
const C64_STORE_ID: u32 = 0x4336_3420;
/// `CFG_TYPE_STRFUNC` (config.h:80).
const CFG_TYPE_STRFUNC: u8 = 0x07;
/// `max` of the three ROM name items (c64.cc:84-86), applied by `ConfigItem::unpack` (config.cc:696-699).
const NAME_MAX: usize = 30;

/// One C64 system ROM.
struct Rom {
    name: &'static str,
    /// Config item id (c64.h:232-234).
    item: u8,
    /// Item default (c64.cc:84-86).
    default_file: &'static str,
    /// The size `init_system_roms` loads (c64.cc:1091-1108), which "Set as ... ROM" also requires
    /// (filetype_bin.cc:78-95).
    size: usize,
    /// Names looked up in DIR, in order: the firmware tree's roms directory, then the names on /flash.
    sources: [&'static str; 2],
    /// Little-endian words "Set as ... ROM" expects before it copies without asking (filetype_bin.cc:162-194).
    signature: &'static [(usize, u32)],
}

const ROMS: [Rom; 3] = [
    Rom {
        name: "KERNAL",
        item: 0xE1,
        default_file: "kernal.bin",
        size: 8192,
        sources: ["kernal.901227-03.bin", "kernal.bin"],
        signature: &[(0, 0x0F20_5685)],
    },
    Rom {
        name: "BASIC",
        item: 0xE2,
        default_file: "basic.bin",
        size: 8192,
        sources: ["basic.901226-01.bin", "basic.bin"],
        signature: &[(1, 0x424D_4243), (2, 0x4349_5341)],
    },
    // firmware/1541ultimate/roms/chars.bin is the 2K overlay font; its size keeps it out.
    Rom {
        name: "CHAR",
        item: 0xE3,
        default_file: "chars.bin",
        size: 4096,
        sources: ["characters.901225-01.bin", "chars.bin"],
        signature: &[(0, 0x6E6E_663C)],
    },
];

/// What happened to one ROM's file in /flash/roms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Missing, written.
    Written,
    /// Already holds this ROM.
    Identical,
    /// Holds something else and stays (no `--c64-roms-force`).
    Kept,
    /// Held something else, overwritten (`--c64-roms-force`).
    Replaced,
    /// The config store selects no usable file name; nothing written.
    Skipped,
}

#[cfg_attr(not(test), allow(dead_code, reason = "the command prints `lines`; the tests check the rest"))]
pub struct Report {
    /// The erased flash disk region was formatted.
    pub formatted: bool,
    /// KERNAL, BASIC, CHAR.
    pub outcomes: [Outcome; 3],
    /// 4 KiB sectors of the image file that changed.
    pub sectors_written: usize,
    /// One line per action, for the log.
    pub lines: Vec<String>,
}

/// Put the KERNAL, BASIC and CHAR ROMs from `dir` into /flash/roms of the flash image at `flash` (created erased when
/// missing), formatting an erased flash disk first. `capabilities` selects the flash layout. A file with other
/// content is kept unless `force`. Only changed sectors are written, so a second call changes nothing.
pub fn write_roms(flash: &Path, capabilities: u32, dir: &Path, force: bool) -> Result<Report> {
    let sources = ROMS.iter().map(|rom| source(dir, rom)).collect::<Result<Vec<_>>>()?;
    let mut lines = Vec::new();
    for (rom, (path, data)) in ROMS.iter().zip(&sources) {
        if !rom.signature.iter().all(|&(index, value)| word(data, index) == value) {
            lines.push(format!(
                "{}: {} does not start like a {} ROM (the menu would ask \"Are you sure?\"); using it",
                rom.name,
                path.display(),
                rom.name
            ));
        }
    }
    let (mut image, full_size) = read_image(flash)?;
    let original = image.clone();
    let store = store_page(&image).map(<[u8]>::to_vec);
    let (base, sectors) = flash_disk(capabilities);
    let region = &mut image[base..base + sectors * SECTOR];
    let formatted = if FileSystem::new(Cursor::new(&mut *region), FsOptions::new()).is_ok() {
        false
    } else if region[..SECTOR].iter().all(|&b| b == 0xFF) {
        let serial = fattime(SystemTime::now());
        let (clusters, fat_sectors) = mkfs(region, sectors, serial)?;
        lines.push(format!(
            "/flash at {base:#x} is erased: formatted as the firmware would (f_mkfs: FAT12, {sectors} × 4096-byte \
             sectors, {fat_sectors} FAT sector(s), {clusters} clusters, serial {serial:08X})"
        ));
        true
    } else {
        bail!(
            "flash image {}: /flash at {base:#x} is neither a FAT volume nor erased; nothing written (boot once \
             without --c64-roms so the firmware formats it)",
            flash.display()
        );
    };
    let outcomes = update_volume(region, store, &sources, force, &mut lines)?;

    let changed: Vec<usize> = (0..FLASH_SIZE / SECTOR)
        .filter(|&s| image[s * SECTOR..(s + 1) * SECTOR] != original[s * SECTOR..(s + 1) * SECTOR])
        .collect();
    if changed.is_empty() && full_size {
        lines.push(format!("flash image {} unchanged", flash.display()));
    } else {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(flash)
            .with_context(|| format!("opening flash image {}", flash.display()))?;
        let written = if full_size {
            changed.iter().try_for_each(|&s| file.write_all_at(&image[s * SECTOR..(s + 1) * SECTOR], (s * SECTOR) as u64))
        } else {
            // Missing or short: the whole image, erased where nothing is stored, as `SpiFlash::open` pads it.
            file.write_all_at(&image, 0)
        };
        written.with_context(|| format!("writing flash image {}", flash.display()))?;
        lines.push(format!("flash image {}: {} changed 4 KiB sector(s) written", flash.display(), changed.len()));
    }
    Ok(Report { formatted, outcomes, sectors_written: changed.len(), lines })
}

/// The first of `rom.sources` in `dir` with the ROM's size.
fn source(dir: &Path, rom: &Rom) -> Result<(PathBuf, Vec<u8>)> {
    let mut wrong_size = Vec::new();
    for name in rom.sources {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(data) if data.len() == rom.size => return Ok((path, data)),
            Ok(data) => wrong_size.push(format!("{name} has {} bytes", data.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
    let wrong_size = if wrong_size.is_empty() { String::new() } else { format!("; {}", wrong_size.join(", ")) };
    bail!(
        "no {}-byte {} ROM in {} (looked for {}{wrong_size})",
        rom.size,
        rom.name,
        dir.display(),
        rom.sources.join(", ")
    )
}

fn word(data: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(data[4 * index..4 * index + 4].try_into().expect("4 bytes"))
}

/// The image as `SpiFlash::open` sees it (ue2-core devices/flash.rs): a missing file is erased flash, a shorter one is
/// padded with 0xFF, a longer one is refused. Also whether the file already has the full size.
fn read_image(path: &Path) -> Result<(Vec<u8>, bool)> {
    let mut image = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading flash image {}", path.display())),
    };
    ensure!(
        image.len() <= FLASH_SIZE,
        "flash image {}: {} bytes, larger than the {FLASH_SIZE}-byte flash",
        path.display(),
        image.len()
    );
    let full_size = image.len() == FLASH_SIZE;
    image.resize(FLASH_SIZE, 0xFF);
    Ok((image, full_size))
}

/// Start and sector count of `FLASH_ID_FLASHDRIVE`: the 100T table for FPGA type 3, else the 50T table
/// (w25q_flash.cc:51-63, 78-106). The type is capability bits 29:28 (itu.c:40-47).
fn flash_disk(capabilities: u32) -> (usize, usize) {
    if (capabilities >> 28) & 3 >= 3 {
        (0x58_0000, 0xA6_8000 / SECTOR)
    } else {
        (0x40_0000, 0xBE_8000 / SECTOR)
    }
}

/// The C64 store's page: the first of the 24 config pages whose id matches (config.cc:127-137).
fn store_page(image: &[u8]) -> Option<&[u8]> {
    (0..CONFIG_PAGES)
        .map(|p| &image[CONFIG_BASE + p * SECTOR..CONFIG_BASE + p * SECTOR + CONFIG_PAGE_SIZE])
        .find(|page| page[..4] == C64_STORE_ID.to_le_bytes())
}

/// String item `id` of a store page as `ConfigStore::unpack` walks it (config.cc:398-424): items from offset 4 up to
/// id 0xFF, a length that does not fit ends the walk, the last match wins. `ConfigItem::unpack` skips a type mismatch
/// and takes at most 30 bytes up to the first NUL (config.cc:671-699).
fn string_item(page: &[u8], id: u8) -> Option<String> {
    let mut found = None;
    let mut index = 4;
    while index + 3 <= page.len() && page[index] != 0xFF {
        let len = usize::from(page[index + 2]);
        if len > page.len() - index - 3 {
            break;
        }
        if page[index] == id && page[index + 1] == CFG_TYPE_STRFUNC {
            let bytes = &page[index + 3..index + 3 + len.min(NAME_MAX)];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            found = Some(String::from_utf8_lossy(&bytes[..end]).into_owned());
        }
        index += len + 3;
    }
    found
}

type Volume<'a> = Cursor<&'a mut [u8]>;

/// Mount /flash, resolve the file names and put each ROM; unmounted before returning.
fn update_volume(
    region: &mut [u8],
    store: Option<Vec<u8>>,
    sources: &[(PathBuf, Vec<u8>)],
    force: bool,
    lines: &mut Vec<String>,
) -> Result<[Outcome; 3]> {
    let fs = FileSystem::new(Cursor::new(region), FsOptions::new()).context("mounting /flash")?;
    let store = match store {
        Some(page) => Some(page),
        None => store_file(&fs)?,
    };
    let mut outcomes = [Outcome::Skipped; 3];
    {
        let roms = fs.root_dir().create_dir("roms").context("/flash/roms")?;
        for (outcome, (rom, (path, data))) in outcomes.iter_mut().zip(ROMS.iter().zip(sources)) {
            *outcome = place(&roms, rom, store.as_deref(), path, data, force, lines)?;
        }
    }
    fs.unmount().context("unmounting /flash")?;
    Ok(outcomes)
}

/// `/flash/config/page_43363420.bin`, the store's file fallback (config.cc:139-152), as a 512-byte page.
fn store_file(fs: &FileSystem<Volume>) -> Result<Option<Vec<u8>>> {
    let name = format!("config/page_{C64_STORE_ID:08x}.bin");
    match fs.root_dir().open_file(&name) {
        Ok(mut file) => {
            let mut page = Vec::new();
            file.read_to_end(&mut page).with_context(|| format!("reading /flash/{name}"))?;
            page.resize(CONFIG_PAGE_SIZE, 0xFF);
            Ok(Some(page))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("/flash/{name}")),
    }
}

/// Put one ROM into `roms` under the name the store selects.
fn place(
    roms: &fatfs::Dir<Volume>,
    rom: &Rom,
    store: Option<&[u8]>,
    path: &Path,
    data: &[u8],
    force: bool,
    lines: &mut Vec<String>,
) -> Result<Outcome> {
    let (file, selected) = match store.and_then(|page| string_item(page, rom.item)) {
        Some(name) => (name, " (the name C64 and Cartridge Settings selects)"),
        None => (rom.default_file.to_string(), ""),
    };
    if file.is_empty() || file.contains('/') || !file.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
        lines.push(format!(
            "{}: C64 and Cartridge Settings selects {file:?}, no file in /flash/roms; nothing written",
            rom.name
        ));
        return Ok(Outcome::Skipped);
    }
    let what = format!("{}: /flash/roms/{file}{selected}", rom.name);
    let existing = match roms.open_file(&file) {
        Ok(mut f) => {
            let mut content = Vec::new();
            f.read_to_end(&mut content).with_context(|| format!("{what}: reading"))?;
            Some(content)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| what.clone()),
    };
    let outcome = match existing {
        Some(content) if content == data => Outcome::Identical,
        Some(_) if !force => Outcome::Kept,
        Some(_) => Outcome::Replaced,
        None => Outcome::Written,
    };
    if matches!(outcome, Outcome::Written | Outcome::Replaced) {
        let write = || -> io::Result<()> {
            let mut f = roms.create_file(&file)?;
            f.truncate()?;
            f.write_all(data)?;
            // UTC, as the firmware stamps its own writes (ue2-vfat `image::fat_time`); fatfs's own clock is local time.
            let now = ue2_vfat::image::fat_time(SystemTime::now());
            #[allow(deprecated)]
            {
                f.set_created(now);
                f.set_modified(now);
            }
            f.flush()
        };
        write().with_context(|| format!("{what}: writing"))?;
    }
    let src = path.display();
    lines.push(match outcome {
        Outcome::Written => format!("{what}: written from {src}"),
        Outcome::Identical => format!("{what}: already holds {src}; unchanged"),
        Outcome::Kept => format!("{what}: differs from {src}; kept (--c64-roms-force replaces it)"),
        Outcome::Replaced => format!("{what}: differed; replaced by {src} (--c64-roms-force)"),
        Outcome::Skipped => unreachable!(),
    });
    Ok(outcome)
}

/// `MAX_FAT12` (ff.c:40).
const MAX_FAT12: usize = 0xFF5;
/// FAT12/16 cluster size boundaries in units of 4K sectors (ff.c:5909).
const CST: [usize; 6] = [1, 4, 16, 64, 256, 512];
/// `n_root = 0` selects 512 root directory entries (ff.c:5948).
const N_ROOT: usize = 512;
/// `SZDIRE`, one directory entry.
const SZDIRE: usize = 32;

/// `GET_FATTIME()` (ff.c:272), the volume serial `f_mkfs` stores: `get_fattime` on the emulated RTC, which counts UTC.
fn fattime(t: SystemTime) -> u32 {
    let dt = ue2_vfat::image::fat_time(t);
    (u32::from(dt.date.year - 1980) << 25)
        | (u32::from(dt.date.month) << 21)
        | (u32::from(dt.date.day) << 16)
        | (u32::from(dt.time.hour) << 11)
        | (u32::from(dt.time.min) << 5)
        | u32::from(dt.time.sec / 2)
}

/// Format `region` (`sectors` × 4096 bytes) as `format_flash` does (blockdev_flash.cc:137-146): `f_mkfs` with
/// `FM_SFD | FM_ANY`, one FAT, `n_root` 0, `au_size` 0, `align` 0 (filesystem_fat.cc:107-120) on a device of 4096-byte
/// sectors and a 1-sector erase block (blockdev_flash.cc:115-123).
///
/// With `FM_ANY` the type starts as FAT16 (ff.c:6018-6035). Fewer than 4096 sectors give 1 sector per cluster
/// (ff.c:6215-6218) and at most `MAX_FAT12` clusters, hence FAT12 with `(n * 3 + 1) / 2 + 3` FAT bytes, 1 reserved
/// sector and 4 root directory sectors (ff.c:6219-6229). A 1-sector erase block needs no alignment (ff.c:6233-6242).
/// The VBR (ff.c:6280-6306), FAT[0..1] = 0xFFFFF8 (ff.c:6328-6346) and a zeroed root directory (ff.c:6348-6354) are
/// written; the data area is left as it is and `FM_SFD` writes no partition table. Returns (clusters, FAT sectors).
fn mkfs(region: &mut [u8], sectors: usize, serial: u32) -> Result<(usize, usize)> {
    ensure!(region.len() == sectors * SECTOR, "region of {} bytes for {sectors} sectors", region.len());
    let mut pau = 1;
    for boundary in CST {
        if boundary > sectors / 0x1000 {
            break;
        }
        pau <<= 1;
    }
    ensure!(sectors / pau <= MAX_FAT12, "{sectors} sectors make a FAT16 volume, which is not replicated here");
    // ff.c's `(n_clst * 3 + 1) / 2 + 3` bytes.
    let fat_sectors = ((sectors / pau * 3).div_ceil(2) + 3).div_ceil(SECTOR);
    let root_sectors = N_ROOT * SZDIRE / SECTOR;
    let data = 1 + fat_sectors + root_sectors;
    ensure!(sectors >= data + pau * 16 && sectors < 0x1_0000, "{sectors} sectors do not make a FAT12 volume");
    let clusters = (sectors - data) / pau;

    region[..data * SECTOR].fill(0);
    let vbr = &mut region[..SECTOR];
    vbr[0..11].copy_from_slice(b"\xEB\xFE\x90MSDOS5.0");
    vbr[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    vbr[13] = pau as u8;
    vbr[14..16].copy_from_slice(&1u16.to_le_bytes());
    vbr[16] = 1;
    vbr[17..19].copy_from_slice(&(N_ROOT as u16).to_le_bytes());
    vbr[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
    vbr[21] = 0xF8;
    vbr[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
    vbr[24..26].copy_from_slice(&63u16.to_le_bytes());
    vbr[26..28].copy_from_slice(&255u16.to_le_bytes());
    vbr[36] = 0x80;
    vbr[38] = 0x29;
    vbr[39..43].copy_from_slice(&serial.to_le_bytes());
    vbr[43..62].copy_from_slice(b"NO NAME    FAT     ");
    vbr[510..512].copy_from_slice(&[0x55, 0xAA]);
    region[SECTOR..SECTOR + 4].copy_from_slice(&0x00FF_FFF8u32.to_le_bytes());
    Ok((clusters, fat_sectors))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MachineConfig::new` capabilities: FPGA type 3, the 100T layout.
    const CAPS_100T: u32 = 0x3400_0222;
    /// FPGA type 2, the 50T layout.
    const CAPS_50T: u32 = 0x2400_0222;

    /// Synthetic ROMs that start the way "Set as ... ROM" expects (filetype_bin.cc:162-194).
    fn roms() -> [Vec<u8>; 3] {
        let mut kernal = vec![0xEA; 8192];
        kernal[..4].copy_from_slice(&[0x85, 0x56, 0x20, 0x0F]);
        let mut basic = vec![0x60; 8192];
        basic[4..12].copy_from_slice(b"CBMBASIC");
        let mut chars = vec![0x18; 4096];
        chars[..4].copy_from_slice(&[0x3C, 0x66, 0x6E, 0x6E]);
        [kernal, basic, chars]
    }

    /// A roms directory like firmware/1541ultimate/roms, whose chars.bin is the 2K overlay font.
    fn rom_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, data) in ["kernal.901227-03.bin", "basic.901226-01.bin", "characters.901225-01.bin"].iter().zip(roms()) {
            std::fs::write(dir.path().join(name), data).unwrap();
        }
        std::fs::write(dir.path().join("chars.bin"), vec![0u8; 2048]).unwrap();
        dir
    }

    /// A copy of /flash from an image file.
    fn volume(flash: &Path, caps: u32) -> FileSystem<Cursor<Vec<u8>>> {
        let (base, sectors) = flash_disk(caps);
        let image = std::fs::read(flash).unwrap();
        FileSystem::new(Cursor::new(image[base..base + sectors * SECTOR].to_vec()), FsOptions::new()).unwrap()
    }

    fn read_file(fs: &FileSystem<Cursor<Vec<u8>>>, path: &str) -> Option<Vec<u8>> {
        let mut file = fs.root_dir().open_file(path).ok()?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();
        Some(data)
    }

    /// A store page as `ConfigStore::pack` writes it (config.cc:275-325).
    fn c64_store(items: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut page = C64_STORE_ID.to_le_bytes().to_vec();
        for &(id, ty, payload) in items {
            page.extend([id, ty, payload.len() as u8]);
            page.extend_from_slice(payload);
        }
        page.push(0xFF);
        page.resize(CONFIG_PAGE_SIZE, 0xFF);
        page
    }

    /// The head below equals, serial aside, the VBR the firmware's own `format_flash` wrote on an erased image
    /// (docs/status/c64.md, `--c64-roms` verification).
    #[test]
    fn c64_roms_mkfs_writes_the_firmware_fat12_layout() {
        for (sectors, fat_sectors) in [(2664, 1), (3048, 2)] {
            let mut region = vec![0xFF; sectors * SECTOR];
            let (clusters, fats) = mkfs(&mut region, sectors, 0x1234_5678).unwrap();
            assert_eq!((clusters, fats), (sectors - 1 - fat_sectors - 4, fat_sectors));
            let [lo, hi] = (sectors as u16).to_le_bytes();
            #[rustfmt::skip]
            let head = [
                0xEB, 0xFE, 0x90, b'M', b'S', b'D', b'O', b'S', b'5', b'.', b'0',
                0x00, 0x10, 0x01, 0x01, 0x00, 0x01, 0x00, 0x02, lo, hi, 0xF8, fat_sectors as u8, 0x00,
                0x3F, 0x00, 0xFF, 0x00, 0, 0, 0, 0, 0, 0, 0, 0,
                0x80, 0x00, 0x29, 0x78, 0x56, 0x34, 0x12,
            ];
            assert_eq!(region[..head.len()], head);
            assert_eq!(&region[43..62], b"NO NAME    FAT     ");
            assert!(region[62..510].iter().all(|&b| b == 0));
            assert_eq!(region[510..512], [0x55, 0xAA]);
            assert_eq!(region[SECTOR..SECTOR + 4], [0xF8, 0xFF, 0xFF, 0x00]);
            let data = (1 + fat_sectors + 4) * SECTOR;
            assert!(region[SECTOR + 4..data].iter().all(|&b| b == 0), "rest of the FAT and the root directory zeroed");
            assert!(region[data..].iter().all(|&b| b == 0xFF), "data area untouched");
            let fs = FileSystem::new(Cursor::new(&mut region[..]), FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat12);
            let stats = fs.stats().unwrap();
            assert_eq!((stats.total_clusters() as usize, stats.cluster_size()), (clusters, 4096));
        }
    }

    #[test]
    fn c64_roms_erased_flash_gets_volume_and_roms_once() {
        let dir = rom_dir();
        let tmp = tempfile::tempdir().unwrap();
        let flash = tmp.path().join("flash.bin");
        let report = write_roms(&flash, CAPS_100T, dir.path(), false).unwrap();
        assert!(report.formatted);
        assert_eq!(report.outcomes, [Outcome::Written; 3]);
        let image = std::fs::read(&flash).unwrap();
        assert_eq!(image.len(), FLASH_SIZE);
        assert!(
            image[..0x58_0000].iter().chain(&image[0x58_0000 + 0xA6_8000..]).all(|&b| b == 0xFF),
            "nothing outside /flash"
        );
        let fs = volume(&flash, CAPS_100T);
        let [kernal, basic, chars] = roms();
        assert_eq!(read_file(&fs, "roms/kernal.bin"), Some(kernal));
        // Names match without case, as in FatFs.
        assert_eq!(read_file(&fs, "ROMS/BASIC.BIN"), Some(basic));
        assert_eq!(read_file(&fs, "roms/chars.bin"), Some(chars));

        let again = write_roms(&flash, CAPS_100T, dir.path(), false).unwrap();
        assert!(!again.formatted);
        assert_eq!(again.outcomes, [Outcome::Identical; 3]);
        assert_eq!(again.sectors_written, 0);
        assert!(std::fs::read(&flash).unwrap() == image, "a second call changes nothing");
    }

    #[test]
    fn c64_roms_different_file_is_kept_unless_forced() {
        let dir = rom_dir();
        let tmp = tempfile::tempdir().unwrap();
        let flash = tmp.path().join("flash.bin");
        write_roms(&flash, CAPS_100T, dir.path(), false).unwrap();
        let mut image = std::fs::read(&flash).unwrap();
        let (base, sectors) = flash_disk(CAPS_100T);
        {
            let fs = FileSystem::new(Cursor::new(&mut image[base..base + sectors * SECTOR]), FsOptions::new()).unwrap();
            let mut file = fs.root_dir().open_file("roms/kernal.bin").unwrap();
            file.write_all(&[0x42; 8192]).unwrap();
        }
        std::fs::write(&flash, &image).unwrap();

        let kept = write_roms(&flash, CAPS_100T, dir.path(), false).unwrap();
        assert_eq!(kept.outcomes, [Outcome::Kept, Outcome::Identical, Outcome::Identical]);
        assert_eq!(kept.sectors_written, 0);
        assert!(std::fs::read(&flash).unwrap() == image);
        assert_eq!(read_file(&volume(&flash, CAPS_100T), "roms/kernal.bin"), Some(vec![0x42; 8192]));

        let forced = write_roms(&flash, CAPS_100T, dir.path(), true).unwrap();
        assert_eq!(forced.outcomes, [Outcome::Replaced, Outcome::Identical, Outcome::Identical]);
        assert_eq!(read_file(&volume(&flash, CAPS_100T), "roms/kernal.bin"), Some(roms()[0].clone()));
    }

    #[test]
    fn c64_roms_store_page_selects_the_names() {
        let dir = rom_dir();
        let tmp = tempfile::tempdir().unwrap();
        let flash = tmp.path().join("flash.bin");
        let mut image = vec![0xFF; FLASH_SIZE];
        // Page 0 belongs to the U64 stores (u64_config.cc:99); the C64 store is page 2.
        image[CONFIG_BASE..CONFIG_BASE + 4].copy_from_slice(&0x5536_3443u32.to_le_bytes());
        let page = c64_store(&[
            (0xE1, CFG_TYPE_STRFUNC, b"kernal.901227-03.bin"),
            // A type mismatch is skipped, so BASIC keeps its default (config.cc:674-677).
            (0xE2, 0x02, &[1]),
            // "No file", which list_chars offers (c64.cc:1751-1757).
            (0xE3, CFG_TYPE_STRFUNC, b""),
        ]);
        image[CONFIG_BASE + 2 * SECTOR..CONFIG_BASE + 2 * SECTOR + CONFIG_PAGE_SIZE].copy_from_slice(&page);
        std::fs::write(&flash, &image).unwrap();

        let report = write_roms(&flash, CAPS_100T, dir.path(), false).unwrap();
        assert_eq!(report.outcomes, [Outcome::Written, Outcome::Written, Outcome::Skipped]);
        let fs = volume(&flash, CAPS_100T);
        assert_eq!(read_file(&fs, "roms/kernal.901227-03.bin"), Some(roms()[0].clone()));
        assert_eq!(read_file(&fs, "roms/kernal.bin"), None);
        assert_eq!(read_file(&fs, "roms/basic.bin"), Some(roms()[1].clone()));
        assert_eq!(read_file(&fs, "roms/chars.bin"), None);
        let after = std::fs::read(&flash).unwrap();
        assert!(after[CONFIG_BASE..] == image[CONFIG_BASE..], "no config write");
    }

    #[test]
    fn c64_roms_store_file_on_the_50t_layout() {
        let dir = rom_dir();
        let tmp = tempfile::tempdir().unwrap();
        let flash = tmp.path().join("flash.bin");
        assert_eq!(flash_disk(CAPS_50T), (0x40_0000, 3048));
        write_roms(&flash, CAPS_50T, dir.path(), false).unwrap();
        let mut image = std::fs::read(&flash).unwrap();
        assert!(image[..0x40_0000].iter().all(|&b| b == 0xFF));
        {
            let fs = FileSystem::new(Cursor::new(&mut image[0x40_0000..0x40_0000 + 3048 * SECTOR]), FsOptions::new())
                .unwrap();
            let config = fs.root_dir().create_dir("config").unwrap();
            let mut file = config.create_file("page_43363420.bin").unwrap();
            file.write_all(&c64_store(&[(0xE3, CFG_TYPE_STRFUNC, b"characters.901225-01.bin")])).unwrap();
        }
        std::fs::write(&flash, &image).unwrap();

        let report = write_roms(&flash, CAPS_50T, dir.path(), false).unwrap();
        assert_eq!(report.outcomes, [Outcome::Identical, Outcome::Identical, Outcome::Written]);
        let fs = volume(&flash, CAPS_50T);
        assert_eq!(read_file(&fs, "roms/characters.901225-01.bin"), Some(roms()[2].clone()));
    }

    #[test]
    fn c64_roms_string_item_follows_config_unpack() {
        let long = [b'x'; 40];
        let page = c64_store(&[
            (0xE1, CFG_TYPE_STRFUNC, b"first.bin"),
            (0xE1, CFG_TYPE_STRFUNC, b"last.bin\0tail"),
            (0xE2, CFG_TYPE_STRFUNC, &long),
        ]);
        assert_eq!(string_item(&page, 0xE1).as_deref(), Some("last.bin"));
        assert_eq!(string_item(&page, 0xE2), Some("x".repeat(NAME_MAX)));
        assert_eq!(string_item(&page, 0xE3), None);
        // A length that does not fit the block ends the walk (config.cc:411-414).
        let short = c64_store(&[(0xE1, CFG_TYPE_STRFUNC, b"a.bin")]);
        assert_eq!(string_item(&short, 0xE1).as_deref(), Some("a.bin"));
        assert_eq!(string_item(&short[..4 + 3 + 4], 0xE1), None);
    }

    #[test]
    fn c64_roms_refuses_unknown_disk_content_and_missing_roms() {
        let dir = rom_dir();
        let tmp = tempfile::tempdir().unwrap();
        let flash = tmp.path().join("flash.bin");
        let mut image = vec![0xFF; FLASH_SIZE];
        image[0x58_0000..0x58_1000].fill(0x00);
        std::fs::write(&flash, &image).unwrap();
        let err = write_roms(&flash, CAPS_100T, dir.path(), false).err().unwrap();
        assert!(format!("{err:#}").contains("neither a FAT volume nor erased"), "{err:#}");
        assert!(std::fs::read(&flash).unwrap() == image);

        // Only the 2K overlay font as chars.bin: no CHAR ROM, and nothing is created.
        let partial = tempfile::tempdir().unwrap();
        let [kernal, basic, _] = roms();
        std::fs::write(partial.path().join("kernal.bin"), kernal).unwrap();
        std::fs::write(partial.path().join("basic.901226-01.bin"), basic).unwrap();
        std::fs::write(partial.path().join("chars.bin"), vec![0u8; 2048]).unwrap();
        let fresh = tmp.path().join("fresh.bin");
        let err = write_roms(&fresh, CAPS_100T, partial.path(), false).err().unwrap();
        assert!(format!("{err:#}").contains("chars.bin has 2048 bytes"), "{err:#}");
        assert!(!fresh.exists());
    }
}
