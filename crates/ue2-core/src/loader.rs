//! Firmware image loader: ELF, `.app` and `.ue2`. Spec: docs/specs/S02-core.md
//!
//! `ultimate.elf` is ELF32-LE-RISCV with entry `_start` = 0x00030000 and one RWE PT_LOAD segment: file part
//! .text/detect_sid/.rodata/.data, zero tail .bss (docs/hw/01-cpu-boot-memory.md §B; docs/hw/00-memory-map.md
//! §1a). Loading it directly replaces the boot ROM (01 §A, H18).

use std::path::Path;

use anyhow::{anyhow, bail, ensure, Context, Result};
use object::elf::{FileHeader32, EM_RISCV, PT_LOAD};
use object::read::elf::{FileHeader, ProgramHeader};
use object::{Endianness, FileKind};

use crate::bus::{BOOT_BRAM_PAGE, IO_BIT};

#[derive(Clone, Debug)]
pub struct LoadedElf {
    pub entry: u32,
    /// (vaddr, memsz) per PT_LOAD segment.
    pub segments: Vec<(u32, u32)>,
}

/// Copy PT_LOAD segments into `ram` (addresses masked to RAM), zero the bss tail.
///
/// Addresses wrap modulo `ram.len()` like the 26-bit DDR decode (bus.rs). A segment on the IO bus or in the
/// boot BRAM page is rejected, because the bus never reads those addresses from RAM.
pub fn load_elf(path: &Path, ram: &mut [u8]) -> Result<LoadedElf> {
    let name = path.display();
    let data = std::fs::read(path).with_context(|| format!("reading {name}"))?;
    match FileKind::parse(&*data).with_context(|| format!("{name}: not an ELF file"))? {
        FileKind::Elf32 => {}
        FileKind::Elf64 => bail!("{name}: 64-bit ELF, expected ELF32 RISC-V"),
        kind => bail!("{name}: {kind:?} file, expected ELF32 RISC-V"),
    }
    let header = FileHeader32::<Endianness>::parse(&*data).with_context(|| format!("{name}: bad ELF header"))?;
    let endian = header.endian()?;
    let machine = header.e_machine(endian);
    ensure!(machine == EM_RISCV, "{name}: ELF machine {machine} is not RISC-V");
    ensure!(endian == Endianness::Little, "{name}: big-endian ELF, expected little-endian RISC-V");

    let mut segments = Vec::new();
    for ph in header.program_headers(endian, &*data)? {
        if ph.p_type(endian) != PT_LOAD {
            continue;
        }
        let (vaddr, filesz, memsz) = (ph.p_vaddr(endian), ph.p_filesz(endian), ph.p_memsz(endian));
        if memsz == 0 {
            continue;
        }
        ensure!(filesz <= memsz, "{name}: PT_LOAD {vaddr:#010x} has filesz {filesz:#x} > memsz {memsz:#x}");
        ensure!(memsz as usize <= ram.len(), "{name}: PT_LOAD {vaddr:#010x}+{memsz:#x} is larger than RAM");
        ensure!(in_ddr(vaddr, memsz), "{name}: PT_LOAD {vaddr:#010x}+{memsz:#x} is not in DDR");
        let bytes = ph
            .data(endian, &*data)
            .map_err(|()| anyhow!("{name}: PT_LOAD {vaddr:#010x} file data is outside the file"))?;
        for_each_chunk(ram, vaddr, bytes.len(), |dst, done| dst.copy_from_slice(&bytes[done..done + dst.len()]));
        for_each_chunk(ram, vaddr.wrapping_add(filesz), (memsz - filesz) as usize, |dst, _| dst.fill(0));
        segments.push((vaddr, memsz));
    }
    ensure!(!segments.is_empty(), "{name}: no PT_LOAD segment");
    Ok(LoadedElf { entry: header.e_entry(endian), segments })
}

/// `ultimate` link origin and entry (target/u64ii/riscv/ultimate/linker.x: `ORIGIN = 0x00030000`).
pub const APP_BASE: u32 = 0x0003_0000;
/// First instruction of the portable RISC-V crt0, `csrrci zero, mstatus, 8` (software/portable/riscv/crt0.S:53).
/// Every U64-II application starts with it; together with [`APP_BASE`] it identifies the embedded
/// `ultimate.app` inside a `.ue2` update file.
pub const CRT0_FIRST_INSN: u32 = 0x3004_7073;

/// Firmware image formats accepted by [`load_firmware`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// `ultimate.elf` from a firmware build. The only format that carries symbols.
    Elf,
    /// `.app` record file such as `ultimate.app`, first record at [`APP_BASE`].
    App,
    /// Update file (`update.ue2`, Commodore release `.ue2`): an updater `.app` loaded at 0x03000000 whose
    /// rodata embeds `ultimate.app` (software/application/update_u2p/update_binaries_u64ii.s). The embedded
    /// records are loaded, not the updater; `app_offset` is their file offset.
    Ue2 { app_offset: usize },
    /// Update file loaded as the updater itself ([`load_updater`], `ue2emu install`): every record from the start of
    /// the file, first one at 0x03000000 (target/u64ii/riscv/update/linker.x).
    Updater,
}

#[derive(Clone, Debug)]
pub struct LoadedFirmware {
    pub format: ImageFormat,
    pub entry: u32,
    /// (load address, length) per loaded record or PT_LOAD segment.
    pub segments: Vec<(u32, u32)>,
}

/// Load a firmware image into `ram`, detecting the format by content: ELF magic, an `.app` whose first record
/// loads at [`APP_BASE`], or a `.ue2` update container that embeds `ultimate.app`.
pub fn load_firmware(path: &Path, ram: &mut [u8]) -> Result<LoadedFirmware> {
    let name = path.display();
    let data = std::fs::read(path).with_context(|| format!("reading {name}"))?;
    if data.starts_with(b"\x7FELF") {
        let elf = load_elf(path, ram)?;
        return Ok(LoadedFirmware { format: ImageFormat::Elf, entry: elf.entry, segments: elf.segments });
    }
    let first = record_at(&data, 0).with_context(|| format!("{name}: neither an ELF nor an .app/.ue2 image"))?;
    let (format, offset) = if first.dest == APP_BASE {
        (ImageFormat::App, 0)
    } else {
        let offset = find_embedded_app(&data).with_context(|| {
            format!(
                "{name}: update container loads at {:#010x} but holds no embedded RISC-V ultimate.app \
                 (record at {APP_BASE:#x} starting with the crt0 instruction)",
                first.dest
            )
        })?;
        (ImageFormat::Ue2 { app_offset: offset }, offset)
    };
    let (entry, segments) =
        load_records(&data[offset..], ram).with_context(|| format!("{name}: bad .app record at offset {offset:#x}"))?;
    Ok(LoadedFirmware { format, entry, segments })
}

/// Load the updater of an update file (`update.ue2`, Commodore `.ue2`) instead of the application it embeds: the
/// records from the start of the file up to the one with a start address. An ELF (`update.elf` from a firmware
/// build) loads as it is and keeps its symbols. An application image, whose first record loads at [`APP_BASE`],
/// holds no updater and is refused.
pub fn load_updater(path: &Path, ram: &mut [u8]) -> Result<LoadedFirmware> {
    let name = path.display();
    let data = std::fs::read(path).with_context(|| format!("reading {name}"))?;
    if data.starts_with(b"\x7FELF") {
        let elf = load_elf(path, ram)?;
        return Ok(LoadedFirmware { format: ImageFormat::Elf, entry: elf.entry, segments: elf.segments });
    }
    let first = record_at(&data, 0).with_context(|| format!("{name}: neither an ELF nor an update file"))?;
    ensure!(first.dest != APP_BASE, "{name}: application image (first record at {APP_BASE:#x}), not an updater");
    let (entry, segments) = load_records(&data, ram).with_context(|| format!("{name}: bad updater record"))?;
    Ok(LoadedFirmware { format: ImageFormat::Updater, entry, segments })
}

/// The `ultimate.app` embedded in an update file, from its first record header to the end of the record with the
/// start address: the bytes from `_ultimate_app_start` to `_ultimate_app_end` (update_binaries_u64ii.s) that the
/// updater writes to the application slot (update_u64ii.cc:180-189). None when no application is embedded.
pub fn embedded_app(data: &[u8]) -> Option<&[u8]> {
    let start = find_embedded_app(data)?;
    let mut end = start;
    loop {
        let rec = record_at(data, end)?;
        end += 12 + (rec.len as usize).next_multiple_of(4);
        if rec.run != 0 {
            return data.get(start..end);
        }
    }
}

/// `.app` record header `{load_addr, length, start_addr}`, 32-bit LE each, followed by `length` data bytes
/// padded to a multiple of 4 (tools/hex2bin.c:37-45; docs/hw/01-cpu-boot-memory.md, `ultimate.app`).
#[derive(Clone, Copy, Debug)]
struct AppRecord {
    dest: u32,
    len: u32,
    run: u32,
}

/// The record at `offset`, if its header is complete, its data fits in `data` and it loads into DDR.
fn record_at(data: &[u8], offset: usize) -> Option<AppRecord> {
    let word = |i: usize| data.get(offset + i..offset + i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let rec = AppRecord { dest: word(0)?, len: word(4)?, run: word(8)? };
    let end = (offset + 12).checked_add(rec.len as usize)?;
    (rec.len > 0 && end <= data.len() && in_ddr(rec.dest, rec.len)).then_some(rec)
}

/// Load consecutive records up to the one with a start address; only the last record carries a non-zero
/// `start_addr` (hex2bin.c:105-113,152-158). Returns (entry, segments).
fn load_records(data: &[u8], ram: &mut [u8]) -> Result<(u32, Vec<(u32, u32)>)> {
    let mut offset = 0;
    let mut segments = Vec::new();
    loop {
        let rec = match record_at(data, offset) {
            Some(rec) => rec,
            None if offset >= data.len() => bail!("no record carries a start address"),
            None => bail!("truncated or invalid record at +{offset:#x}"),
        };
        let payload = &data[offset + 12..offset + 12 + rec.len as usize];
        for_each_chunk(ram, rec.dest, payload.len(), |dst, done| dst.copy_from_slice(&payload[done..done + dst.len()]));
        segments.push((rec.dest, rec.len));
        if rec.run != 0 {
            return Ok((rec.run, segments));
        }
        offset += 12 + (rec.len as usize).next_multiple_of(4);
    }
}

/// File offset of the embedded `ultimate.app` in an update container: a 4-byte-aligned record (`.align 4`
/// before `.incbin "ultimate.app"`, update_binaries_u64ii.s) that loads at [`APP_BASE`] and starts with
/// [`CRT0_FIRST_INSN`]. The updater itself loads elsewhere, so its own crt0 does not match.
fn find_embedded_app(data: &[u8]) -> Option<usize> {
    (12..data.len().saturating_sub(16)).step_by(4).find(|&off| {
        record_at(data, off).is_some_and(|rec| {
            rec.dest == APP_BASE && data.get(off + 12..off + 16) == Some(&CRT0_FIRST_INSN.to_le_bytes()[..])
        })
    })
}

/// True when every address of `vaddr..vaddr+len` (len > 0) decodes to DDR on the data bus (bus.rs).
fn in_ddr(vaddr: u32, len: u32) -> bool {
    let Some(last) = vaddr.checked_add(len - 1) else {
        return false;
    };
    // Bit 28 is constant inside one 256 MB block; the boot BRAM page is the only hole below it.
    let bram = BOOT_BRAM_PAGE << 16..=(BOOT_BRAM_PAGE << 16 | 0xFFFF);
    vaddr >> 28 == last >> 28 && vaddr & IO_BIT == 0 && !(vaddr <= *bram.end() && last >= *bram.start())
}

/// Call `f(chunk, bytes_done)` for the RAM chunks covering `len` bytes at `addr`, wrapping modulo `ram.len()`.
fn for_each_chunk(ram: &mut [u8], addr: u32, len: usize, mut f: impl FnMut(&mut [u8], usize)) {
    let mut pos = addr as usize % ram.len();
    let mut done = 0;
    while done < len {
        let n = (len - done).min(ram.len() - pos);
        f(&mut ram[pos..pos + n], done);
        done += n;
        pos = 0;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use object::elf::{ELFCLASS32, ELFCLASS64, EM_X86_64, PT_NOTE};

    use super::*;
    use crate::bus::RAM_SIZE;

    /// ELF path below the firmware tree root.
    pub(crate) const FIRMWARE_ELF: &str = "target/u64ii/riscv/ultimate/result/ultimate.elf";

    /// Firmware tree from `UE2_FIRMWARE` or `<repo root>/firmware/1541ultimate`; None (with a skip message)
    /// when its ELF is missing.
    pub(crate) fn firmware_root() -> Option<PathBuf> {
        let root = std::env::var_os("UE2_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/1541ultimate"));
        if root.join(FIRMWARE_ELF).is_file() {
            Some(root)
        } else {
            eprintln!("skipping: firmware ELF not found at {}", root.join(FIRMWARE_ELF).display());
            None
        }
    }

    /// Minimal ELF32 LE executable with `(p_type, vaddr, file bytes, memsz)` program headers.
    fn elf(class: u8, machine: u16, entry: u32, phdrs: &[(u32, u32, &[u8], u32)]) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(bytes);
        }
        let mut out = vec![0x7F, b'E', b'L', b'F', class, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        put(&mut out, &2u16.to_le_bytes()); // e_type ET_EXEC
        put(&mut out, &machine.to_le_bytes());
        for word in [1, entry, 52, 0, 0] {
            put(&mut out, &u32::to_le_bytes(word)); // e_version, e_entry, e_phoff, e_shoff, e_flags
        }
        for half in [52, 32, phdrs.len() as u16, 40, 0, 0] {
            put(&mut out, &u16::to_le_bytes(half)); // e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx
        }
        let mut offset = 52 + 32 * phdrs.len() as u32;
        for &(p_type, vaddr, bytes, memsz) in phdrs {
            for word in [p_type, offset, vaddr, vaddr, bytes.len() as u32, memsz, 7, 1] {
                put(&mut out, &word.to_le_bytes());
            }
            offset += bytes.len() as u32;
        }
        for (_, _, bytes, _) in phdrs {
            put(&mut out, bytes);
        }
        out
    }

    fn load(image: &[u8], ram: &mut [u8]) -> Result<LoadedElf> {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), image).unwrap();
        load_elf(file.path(), ram)
    }

    #[test]
    fn copies_filesz_zeroes_bss_tail_and_masks_into_ram() {
        let mut ram = vec![0xEE; RAM_SIZE];
        let image =
            elf(ELFCLASS32, EM_RISCV, 0x30000, &[(PT_LOAD, 0x0400_1000, &[1, 2, 3, 4], 8), (PT_NOTE, 0x2000, &[9], 1)]);
        let loaded = load(&image, &mut ram).unwrap();
        assert_eq!((loaded.entry, loaded.segments), (0x30000, vec![(0x0400_1000, 8)]));
        assert_eq!(ram[0x1000..0x1009], [1, 2, 3, 4, 0, 0, 0, 0, 0xEE]);
        assert_eq!(ram[0xFFF], 0xEE);
    }

    #[test]
    fn segment_wraps_at_the_ram_end() {
        let mut ram = vec![0xEE; RAM_SIZE];
        load(&elf(ELFCLASS32, EM_RISCV, 0, &[(PT_LOAD, 0x03FF_FFFE, &[1, 2, 3], 5)]), &mut ram).unwrap();
        assert_eq!((ram[RAM_SIZE - 2], ram[RAM_SIZE - 1]), (1, 2));
        assert_eq!(ram[0..4], [3, 0, 0, 0xEE]);
    }

    #[test]
    fn rejects_non_riscv_64bit_and_non_ddr_images() {
        let mut ram = vec![0; RAM_SIZE];
        let seg: &[(u32, u32, &[u8], u32)] = &[(PT_LOAD, 0x30000, &[0x13, 0, 0, 0], 4)];
        let err = |image: Vec<u8>, ram: &mut [u8]| format!("{:#}", load(&image, ram).unwrap_err());
        assert!(err(elf(ELFCLASS32, EM_X86_64, 0, seg), &mut ram).contains("not RISC-V"));
        assert!(err(elf(ELFCLASS64, EM_RISCV, 0, seg), &mut ram).contains("64-bit"));
        assert!(err(b"not an elf at all".to_vec(), &mut ram).contains("not an ELF"));
        assert!(err(elf(ELFCLASS32, EM_RISCV, 0, &[(PT_LOAD, 0x1000_0000, &[1], 1)]), &mut ram).contains("not in DDR"));
        assert!(err(elf(ELFCLASS32, EM_RISCV, 0, &[(PT_LOAD, 0x7FFF_FFFF, &[], 2)]), &mut ram).contains("not in DDR"));
        assert!(err(elf(ELFCLASS32, EM_RISCV, 0, &[]), &mut ram).contains("no PT_LOAD"));
    }

    #[test]
    fn loads_the_firmware_elf() {
        let Some(root) = firmware_root() else { return };
        let path = root.join(FIRMWARE_ELF);
        let mut ram = vec![0xEE; RAM_SIZE];
        let loaded = load_elf(&path, &mut ram).unwrap();
        // 00-memory-map §1a: single RWE segment 0x30000, filesz 0x126AE8, memsz 0xE19644.
        assert_eq!((loaded.entry, loaded.segments), (0x30000, vec![(0x30000, 0xE1_9644)]));
        let file = std::fs::read(&path).unwrap();
        assert_eq!(ram[0x30000..0x30000 + 0x12_6AE8], file[0x1000..0x1000 + 0x12_6AE8]);
        assert!(ram[0x30000 + 0x12_6AE8..0x30000 + 0xE1_9644].iter().all(|&b| b == 0));
        assert_eq!((ram[0x2FFFF], ram[0x30000 + 0xE1_9644]), (0xEE, 0xEE));
    }

    /// `.app` record bytes in the hex2bin layout, data padded to 4.
    fn app_record(dest: u32, run: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for word in [dest, payload.len() as u32, run] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out.resize(out.len().next_multiple_of(4), 0);
        out
    }

    /// Ten bytes of application code starting with the crt0 instruction.
    fn app_payload() -> Vec<u8> {
        let mut payload = CRT0_FIRST_INSN.to_le_bytes().to_vec();
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        payload
    }

    fn load_fw(image: &[u8], ram: &mut [u8]) -> Result<LoadedFirmware> {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), image).unwrap();
        load_firmware(file.path(), ram)
    }

    #[test]
    fn app_file_loads_its_records_and_enters_at_the_last_start_address() {
        let mut ram = vec![0xEE; RAM_SIZE];
        let mut image = app_record(APP_BASE, 0, &app_payload());
        image.extend(app_record(0x0010_0000, APP_BASE, &[9, 8, 7]));
        let fw = load_fw(&image, &mut ram).unwrap();
        assert_eq!(fw.format, ImageFormat::App);
        assert_eq!((fw.entry, fw.segments), (APP_BASE, vec![(APP_BASE, 10), (0x0010_0000, 3)]));
        assert_eq!(ram[0x30000..0x3000A], app_payload()[..]);
        assert_eq!(ram[0x10_0000..0x10_0004], [9, 8, 7, 0xEE]);
    }

    #[test]
    fn ue2_container_loads_the_embedded_app_not_the_updater() {
        let mut ram = vec![0xEE; RAM_SIZE];
        let mut updater = CRT0_FIRST_INSN.to_le_bytes().to_vec(); // the updater starts with the same crt0
        updater.extend_from_slice(&[0x55; 64]); // e.g. an FPGA bitstream
        updater.extend(app_record(APP_BASE, APP_BASE, &[0x13, 0, 0, 0])); // lookalike without the crt0 start
        let app_offset = 12 + updater.len();
        updater.extend(app_record(APP_BASE, APP_BASE, &app_payload()));
        updater.extend_from_slice(&[0xAA; 32]); // trailing rodata (ROMs, html)
        let fw = load_fw(&app_record(0x0300_0000, 0x0300_0000, &updater), &mut ram).unwrap();
        assert_eq!(fw.format, ImageFormat::Ue2 { app_offset });
        assert_eq!((fw.entry, fw.segments), (APP_BASE, vec![(APP_BASE, 10)]));
        assert_eq!(ram[0x30000..0x3000A], app_payload()[..]);
        assert_eq!(ram[0x0300_0000], 0xEE, "the updater is not loaded");
    }

    #[test]
    fn rejects_images_without_a_loadable_app() {
        let mut ram = vec![0; RAM_SIZE];
        let err = |image: Vec<u8>, ram: &mut [u8]| format!("{:#}", load_fw(&image, ram).unwrap_err());
        assert!(err(b"garbage!".to_vec(), &mut ram).contains("neither an ELF"));
        let mut truncated = app_record(APP_BASE, 0, &app_payload());
        truncated.truncate(14);
        assert!(err(truncated, &mut ram).contains("neither an ELF"));
        assert!(err(app_record(0x0300_0000, 0x0300_0000, &[0x55; 40]), &mut ram).contains("no embedded RISC-V"));
        assert!(err(app_record(APP_BASE, 0, &app_payload()), &mut ram).contains("no record carries a start address"));
    }

    fn load_upd(image: &[u8], ram: &mut [u8]) -> Result<LoadedFirmware> {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), image).unwrap();
        load_updater(file.path(), ram)
    }

    #[test]
    fn updater_loads_itself_and_embedded_app_is_its_application_file() {
        let mut ram = vec![0xEE; RAM_SIZE];
        let mut updater = CRT0_FIRST_INSN.to_le_bytes().to_vec();
        updater.extend_from_slice(&[0x55; 64]); // FPGA images
        let app_offset = 12 + updater.len();
        let mut app = app_record(APP_BASE, 0, &app_payload());
        app.extend(app_record(0x0010_0000, APP_BASE, &[9, 8, 7]));
        updater.extend_from_slice(&app);
        updater.extend_from_slice(&[0xAA; 32]); // trailing rodata
        let image = app_record(0x0300_0000, 0x0300_0004, &updater);
        let fw = load_upd(&image, &mut ram).unwrap();
        assert_eq!(fw.format, ImageFormat::Updater);
        assert_eq!((fw.entry, fw.segments), (0x0300_0004, vec![(0x0300_0000, updater.len() as u32)]));
        assert_eq!(ram[0x0300_0000..0x0300_0004], CRT0_FIRST_INSN.to_le_bytes());
        assert_eq!(ram[0x30000], 0xEE, "the embedded application is not loaded");
        assert_eq!(embedded_app(&image), Some(&image[app_offset..app_offset + app.len()]));
        assert_eq!(embedded_app(&image), Some(&app[..]));
    }

    #[test]
    fn updater_loader_refuses_application_images() {
        let mut ram = vec![0; RAM_SIZE];
        let err = |image: Vec<u8>, ram: &mut [u8]| format!("{:#}", load_upd(&image, ram).unwrap_err());
        assert!(err(app_record(APP_BASE, APP_BASE, &app_payload()), &mut ram).contains("not an updater"));
        assert!(err(b"garbage!".to_vec(), &mut ram).contains("neither an ELF"));
        assert!(err(app_record(0x0300_0000, 0, &[0x55; 8]), &mut ram).contains("bad updater record"));
        assert_eq!(embedded_app(&app_record(APP_BASE, APP_BASE, &app_payload())), None);
    }

    #[test]
    fn embedded_app_spans_the_whole_firmware_app_file() {
        let Some(root) = firmware_root() else { return };
        let app = std::fs::read(root.join("target/u64ii/riscv/ultimate/result/ultimate.app")).unwrap();
        // update_binaries_u64ii.s: `.align 4`, then `.incbin "ultimate.app"` after the FPGA images.
        let mut rodata = CRT0_FIRST_INSN.to_le_bytes().to_vec();
        rodata.extend_from_slice(&[0x55; 64]);
        rodata.extend_from_slice(&app);
        rodata.extend_from_slice(b"trailing rodata!");
        let image = app_record(0x0300_0000, 0x0300_0000, &rodata);
        assert_eq!(embedded_app(&image), Some(&app[..]));
    }

    #[test]
    fn elf_images_keep_the_elf_path() {
        let mut ram = vec![0; RAM_SIZE];
        let image = elf(ELFCLASS32, EM_RISCV, 0x30000, &[(PT_LOAD, 0x30000, &[0x13, 0, 0, 0], 4)]);
        let fw = load_fw(&image, &mut ram).unwrap();
        assert_eq!((fw.format, fw.entry), (ImageFormat::Elf, 0x30000));
    }

    #[test]
    fn firmware_app_loads_the_same_ram_as_the_elf() {
        let Some(root) = firmware_root() else { return };
        let (mut from_elf, mut from_app) = (vec![0; RAM_SIZE], vec![0; RAM_SIZE]);
        let elf = load_elf(&root.join(FIRMWARE_ELF), &mut from_elf).unwrap();
        let fw = load_firmware(&root.join("target/u64ii/riscv/ultimate/result/ultimate.app"), &mut from_app).unwrap();
        assert_eq!((fw.format, fw.entry), (ImageFormat::App, elf.entry));
        // hex2bin keeps gaps of up to 32 bytes inside one record and fills them with 0xFF (hex2bin.c:164-178),
        // where the ELF has zeros, e.g. the .rodata/.data alignment gap 0x14B604-0x14B607. Nothing reads them.
        let diffs: Vec<usize> = (0..RAM_SIZE).filter(|&i| from_elf[i] != from_app[i]).collect();
        assert!(diffs.len() < 64, "{} bytes differ", diffs.len());
        for i in diffs {
            assert_eq!((from_elf[i], from_app[i]), (0x00, 0xFF), "unexpected difference at {i:#010x}");
        }
    }
}
