//! MultiCore update packages, `.txu` (docs/specs/S46-multicore-txu.md): the reader and the checks of TRX64U-MultiCore's
//! host app, and the flash after the host app wrote a package. Paths are under TRX64U-MultiCore/host-app/software/hostapp
//! unless they name tools/.
//!
//! - [`Txu::parse`]: `txu_probe` (txu.cc:30-99), the header and its rules.
//! - [`Txu::check`]: `txu_stage` (txu.cc:180-237): each part's SHA-256, the host part's `.bit` header length, IDCODE
//!   and AXSS stamp, the application record (`fw_image_seal_parts`, flashfw.cc:530-580).
//! - [`Txu::write_into`]: the flash after `fw_write_parts` (flashfw.cc:393-473).
//! - [`pack`]: the writer, as tools/txu_pack.py `pack`.
//!
//! The FPGA and its flash layout come from [`FPGAS`]. A TXU1 header names no FPGA (txu.h): every TXU1 package is the
//! XC7A50T's ([`TXU1_FPGA`]), as the host app's constants say (flashfw.h:15-18, cfw.h:21).

use anyhow::{bail, ensure, Result};
use sha2::{Digest, Sha256};

/// An FPGA the flash layout and the bitstream checks know.
#[derive(Debug, PartialEq, Eq)]
pub struct Fpga {
    /// `CAPAB_FPGA_TYPE`, capability bits 29:28 (itu.h); the firmware picks the layout by it (w25q_flash.cc:82-84).
    pub fpga_type: u32,
    pub name: &'static str,
    /// The IDCODE the bitstream's configuration packets write, compared under [`IDCODE_MASK`] (cfw.h:21-22).
    pub idcode: u32,
    /// The bitstream slot is [0, `bit_slot`); the application slot starts there (w25q_flash.cc:47-59).
    pub bit_slot: u32,
    /// The application slot's size, the 12-byte record included.
    pub app_slot: u32,
}

impl Fpga {
    /// Where the application record sits in the flash.
    pub fn app_addr(&self) -> u32 {
        self.bit_slot
    }
}

/// The U64-II layouts of firmware 3.15 (`flash_addresses_u64ii_50t` / `_100t`, w25q_flash.cc:47-59), with the
/// XC7A50T's IDCODE (cfw.h:21) and the XC7A100T's.
pub const FPGAS: [Fpga; 2] = [
    Fpga { fpga_type: 2, name: "XC7A50T", idcode: 0x0362_C093, bit_slot: 0x22_0000, app_slot: 0x1E_0000 },
    Fpga { fpga_type: 3, name: "XC7A100T", idcode: 0x0363_1093, bit_slot: 0x3C_0000, app_slot: 0x1C_0000 },
];

/// The version bits 31:28 of an IDCODE are not compared (cfw.h:22).
pub const IDCODE_MASK: u32 = 0x0FFF_FFFF;

/// The FPGA of type `fpga_type`, if the table has it.
pub fn fpga(fpga_type: u32) -> Option<&'static Fpga> {
    FPGAS.iter().find(|f| f.fpga_type == fpga_type)
}

/// The FPGA every TXU1 package is for: the XC7A50T (txu.h: the app at 0x220000; txu.cc:216 its IDCODE).
pub const TXU1_FPGA: &Fpga = &FPGAS[0];

pub const MAGIC: &[u8; 4] = b"TXU1";
pub const KIND_HOST: u32 = 1;
pub const KIND_APP: u32 = 2;
/// 76 header bytes and the CRC-32; each part adds [`PART_BYTES`] (txu.h:28-29).
const HDR_FIXED: usize = 80;
const PART_BYTES: usize = 48;
/// The application record header { load, length, start } (flashfw.h:19).
const REC_HDR: usize = 12;

/// `txu_result_str` (txu.cc:9-20).
const NOT_TXU: &str = "not a MultiCore update file";
const FORMAT: &str = "the update file is damaged or incomplete";
const CRC: &str = "the header of the update file does not match its checksum";
const ORIGIN: &str = "the update file does not come from TRX";

/// Whether `data` starts like a `.txu`.
pub fn is_txu(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

/// One part as the header lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// [`KIND_HOST`] or [`KIND_APP`].
    pub kind: u32,
    pub off: u32,
    pub size: u32,
    pub flash_addr: u32,
    pub sha256: [u8; 32],
}

impl Part {
    /// The part's bytes in the package `data` it was parsed from.
    pub fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.off as usize..(self.off + self.size) as usize]
    }

    fn name(&self) -> &'static str {
        if self.kind == KIND_HOST { "host" } else { "app" }
    }
}

/// A package whose header passed `txu_probe`'s rules.
#[derive(Clone, Debug)]
pub struct Txu {
    pub version: String,
    pub commit: String,
    /// The host part's AXSS stamp; 0 without a host part.
    pub host_key: u32,
    pub iface_provided: u32,
    pub iface_needed: u32,
    pub parts: Vec<Part>,
    /// The FPGA the package is for.
    pub fpga: &'static Fpga,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().expect("4 bytes")))
}

/// A NUL-terminated char array.
fn c_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// zlib's CRC-32 (flashfw.cc:13-35).
pub fn crc32(data: &[u8]) -> u32 {
    let table: [u32; 256] = std::array::from_fn(|i| {
        (0..8).fold(i as u32, |c, _| if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 })
    });
    !data.iter().fold(!0u32, |c, &b| table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8))
}

impl Txu {
    pub fn host(&self) -> Option<&Part> {
        self.parts.iter().find(|p| p.kind == KIND_HOST)
    }

    pub fn app(&self) -> Option<&Part> {
        self.parts.iter().find(|p| p.kind == KIND_APP)
    }

    /// `txu_probe` (txu.cc:30-99). An error reads as `txu_result_str`'s words, then the rule that failed.
    pub fn parse(data: &[u8]) -> Result<Txu> {
        let fpga = TXU1_FPGA;
        ensure!(is_txu(data), "{NOT_TXU}: no TXU1 at offset 0");
        ensure!(data.len() >= HDR_FIXED, "{FORMAT}: {} bytes, shorter than the {HDR_FIXED}-byte header", data.len());
        let (hdr_len, flags, nparts) = (le32(data, 4) as usize, le32(data, 8), le32(data, 24) as usize);
        ensure!((1..=2).contains(&nparts), "{FORMAT}: {nparts} parts, 1 or 2 expected");
        ensure!(hdr_len == HDR_FIXED + PART_BYTES * nparts, "{FORMAT}: hdr_len {hdr_len} is not 80 + 48 × {nparts}");
        ensure!(data.len() >= hdr_len, "{FORMAT}: {} bytes, shorter than its {hdr_len}-byte header", data.len());
        ensure!(flags == 0, "{FORMAT}: flags {flags:#x}, 0 expected");
        let (stored, computed) = (le32(data, hdr_len - 4), crc32(&data[..hdr_len - 4]));
        ensure!(stored == computed, "{CRC}: CRC-32 {stored:08x}, the header gives {computed:08x}");
        let origin = c_str(&data[28..36]);
        ensure!(origin == "trx", "{ORIGIN}: origin {origin:?}");
        let mut txu = Txu {
            version: c_str(&data[36..60]),
            commit: c_str(&data[60..76]),
            host_key: le32(data, 12),
            iface_provided: le32(data, 16),
            iface_needed: le32(data, 20),
            parts: Vec::new(),
            fpga,
        };
        let mut end = hdr_len as u64;
        for i in 0..nparts {
            let at = HDR_FIXED - 4 + i * PART_BYTES;
            let part = Part {
                kind: le32(data, at),
                off: le32(data, at + 4),
                size: le32(data, at + 8),
                flash_addr: le32(data, at + 12),
                sha256: data[at + 16..at + 48].try_into().expect("32 bytes"),
            };
            let (addr, slot) = match part.kind {
                KIND_HOST => (0, fpga.bit_slot),
                KIND_APP => (fpga.app_addr(), fpga.app_slot),
                kind => bail!("{FORMAT}: part {i}: kind {kind}, 1 (host) or 2 (app) expected"),
            };
            let what = part.name();
            ensure!(part.flash_addr == addr, "{FORMAT}: {what} part at flash {:#08x}, {addr:#08x} expected", part.flash_addr);
            ensure!(
                (1..=slot).contains(&part.size),
                "{FORMAT}: {what} part of {} bytes, the slot holds {slot:#x}",
                part.size
            );
            ensure!(part.off & 3 == 0, "{FORMAT}: {what} part at offset {:#x}, not 4-aligned", part.off);
            ensure!(u64::from(part.off) >= end, "{FORMAT}: {what} part at offset {:#x} overlaps what is before it", part.off);
            ensure!(
                u64::from(part.off) + u64::from(part.size) <= data.len() as u64,
                "{FORMAT}: {what} part [{:#x}, +{:#x}) runs past the end of the {}-byte file",
                part.off,
                part.size,
                data.len()
            );
            ensure!(txu.parts.iter().all(|p| p.kind != part.kind), "{FORMAT}: two {what} parts");
            end = u64::from(part.off) + u64::from(part.size);
            txu.parts.push(part);
        }
        ensure!(end == data.len() as u64, "{FORMAT}: {} bytes behind the last part", data.len() as u64 - end);
        let host = txu.host().is_some();
        ensure!(
            host == (txu.host_key != 0) && host == (txu.iface_provided != 0),
            "{FORMAT}: host_key {:#x} and iface_provided {} with{} a host part",
            txu.host_key,
            txu.iface_provided,
            if host { "" } else { "out" }
        );
        ensure!(
            txu.app().is_some() == (txu.iface_needed != 0),
            "{FORMAT}: iface_needed {} with{} an app part",
            txu.iface_needed,
            if txu.app().is_some() { "" } else { "out" }
        );
        Ok(txu)
    }

    /// `txu_stage` (txu.cc:180-237) on the package `data` it was parsed from: each part against its SHA-256, the host
    /// part as a bitstream for [`Txu::fpga`] carrying `host_key`, the application record as the boot ROM takes it.
    /// Not `txu_check`, which compares interface numbers with a running system.
    pub fn check(&self, data: &[u8]) -> Result<()> {
        for part in &self.parts {
            let sha: [u8; 32] = Sha256::digest(part.bytes(data)).into();
            ensure!(sha == part.sha256, "the {} part does not match its SHA-256", part.name());
        }
        if let Some(host) = self.host() {
            let bit = host.bytes(data);
            let data_off = bit_length_from_header(bit)
                .filter(|&(len, _)| len == bit.len() as u64)
                .map(|(_, off)| off)
                .ok_or_else(|| anyhow::anyhow!("the host part is not a bitstream file whose header length matches"))?;
            let id = bit_idcode(&bit[data_off..]).unwrap_or(0);
            ensure!(
                id != 0 && (id ^ self.fpga.idcode) & IDCODE_MASK == 0,
                "the host part is not a bitstream for this FPGA: IDCODE {id:08x}, the {} has {:08x}",
                self.fpga.name,
                self.fpga.idcode
            );
            let ax = axss(bit, self.fpga.bit_slot);
            ensure!(
                ax == Some(self.host_key),
                "the host part does not carry the host key its header names: {}, header {:08x}",
                ax.map_or("no stamp".into(), |v| format!("stamp {v:08x}")),
                self.host_key
            );
        }
        if let Some(app) = self.app() {
            check_app_record(app.bytes(data), self.fpga.app_slot)?;
        }
        Ok(())
    }

    /// The flash `image` (16 MiB) after `fw_write_parts` (flashfw.cc:393-473) wrote this package's parts:
    /// - the app: the 4 KiB sectors that cover it erased and programmed (its first sector last); the rest of the slot
    ///   stays;
    /// - the host part: the sectors that cover it erased and programmed, then every sector of the rest of the
    ///   bitstream slot erased (`clear_tail`).
    ///
    /// A part the package does not carry leaves its region as it was. Call [`Txu::check`] first.
    pub fn write_into(&self, data: &[u8], image: &mut [u8]) {
        const SECTOR: usize = 0x1000;
        let mut write = |addr: usize, bytes: &[u8], erase_to: usize| {
            image[addr..erase_to].fill(0xFF);
            image[addr..addr + bytes.len()].copy_from_slice(bytes);
        };
        if let Some(app) = self.app() {
            let addr = app.flash_addr as usize;
            write(addr, app.bytes(data), addr + (app.size as usize).next_multiple_of(SECTOR));
        }
        if let Some(host) = self.host() {
            write(host.flash_addr as usize, host.bytes(data), self.fpga.bit_slot as usize);
        }
    }
}

/// `bit_length_from_header` (txu.cc:141-164, tools/trx_pkg.py): the length a Xilinx `.bit` file states in its header
/// and where its configuration data starts; None when it is not one.
pub fn bit_length_from_header(b: &[u8]) -> Option<(u64, usize)> {
    const MAGIC: [u8; 13] = [0x00, 0x09, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x00, 0x00, 0x01];
    if b.get(..13)? != MAGIC {
        return None;
    }
    let mut p = 13;
    for key in [b'a', b'b', b'c', b'd'] {
        let field = b.get(p..p + 3)?;
        if field[0] != key {
            return None;
        }
        p += 3 + usize::from(u16::from_be_bytes([field[1], field[2]]));
    }
    if b.get(p..p + 5)?.first() != Some(&b'e') {
        return None;
    }
    let len = u64::from(be32(b, p + 1)?);
    Some(((p + 5) as u64 + len, p + 5))
}

const SYNC_WORD: u32 = 0xAA99_5566;

/// `scan_config` with `stop_at_id` (cfw.cc:157-205), as `cfw_bit_idcode` calls it (cfw.cc:207-211): after a sync word
/// at a 4-aligned offset in the first 1 KiB of the configuration data, the value a type-1 write of register 0x0C
/// (IDCODE) carries; the walk ends at the DESYNC command. None without a sync word or an IDCODE write.
pub fn bit_idcode(config: &[u8]) -> Option<u32> {
    const REG_CMD: u32 = 0x04;
    const REG_IDCODE: u32 = 0x0C;
    const CMD_DESYNC: u32 = 0x0D;
    let mut i = (0..1024).step_by(4).find(|&i| be32(config, i) == Some(SYNC_WORD))? + 4;
    while let Some(v) = be32(config, i) {
        i += 4;
        match v >> 29 {
            1 => {
                let (op, reg, cnt) = (v >> 27 & 3, v >> 13 & 0x3FFF, (v & 0x7FF) as usize);
                if op == 2 && cnt == 1 {
                    let val = be32(config, i)?;
                    if reg == REG_IDCODE {
                        return Some(val);
                    }
                    if reg == REG_CMD && val == CMD_DESYNC {
                        return None;
                    }
                }
                i += 4 * cnt;
            }
            2 => i += 4 * (v & 0x07FF_FFFF) as usize,
            _ => return None,
        }
    }
    None
}

/// The boot ROM's AXSS walk (flashfw.cc:67-113, `fw_axss`) over the whole `.bit` as it lies in the flash: the sync word
/// byte by byte in the first 4 KiB, then the packets; the value of the first one-word write of register 0x0D (AXSS),
/// unless DESYNC, an unknown packet or the end of the bitstream slot comes first.
pub fn axss(bit: &[u8], bit_slot: u32) -> Option<u32> {
    let slot = bit_slot as usize;
    let mut sh = 0u32;
    let mut pos = 0;
    while sh != SYNC_WORD {
        if pos >= 4096 || pos >= bit.len() {
            return None;
        }
        sh = sh << 8 | u32::from(bit[pos]);
        pos += 1;
    }
    for _ in 0..65536 {
        if pos >= slot {
            return None;
        }
        let w = be32(bit, pos)?;
        pos += 4;
        let count = match w >> 29 {
            1 => {
                let (op, reg, count) = (w >> 27 & 3, w >> 13 & 0x3FFF, w & 0x7FF);
                if op == 2 && count == 1 {
                    let v = be32(bit, pos)?;
                    pos += 4;
                    if reg == 0x0D {
                        return Some(v);
                    }
                    if reg == 0x04 && v == 0x0D {
                        return None;
                    }
                    continue;
                }
                count
            }
            2 => w & 0x07FF_FFFF,
            _ => return None,
        };
        if count as usize >= slot >> 2 {
            return None;
        }
        pos += 4 * count as usize;
    }
    None
}

/// The application record as the boot ROM takes it (`fw_image_seal_parts`, flashfw.cc:545-571; tools/trx_pkg.py
/// `check_app_record`): { load, length, start } and the data padded to 4, inside a slot of `slot` bytes.
pub fn check_app_record(app: &[u8], slot: u32) -> Result<()> {
    let slot = slot as usize;
    ensure!((REC_HDR + 4..=slot).contains(&app.len()), "the application does not fit its slot");
    let (dest, len, run) = (le32(app, 0), le32(app, 4), le32(app, 8));
    ensure!(len != 0 && len as usize <= slot - REC_HDR, "the application does not fit its slot");
    ensure!(
        dest & 3 == 0 && dest >= 0x1_0000 && dest <= 0x0400_0000 - len,
        "the application's load address is not one the boot ROM takes: {dest:#010x}"
    );
    ensure!(
        run >= dest && run < dest + len,
        "the application's start address lies outside the image: {run:#010x} not in [{dest:#010x}, +{len:#x})"
    );
    ensure!(app.len() == REC_HDR + (len as usize).next_multiple_of(4), "the application record and its length disagree");
    Ok(())
}

/// A host part for [`pack`]: the `.bit` file and its interface number.
pub struct HostIn<'a> {
    pub bit: &'a [u8],
    pub iface_provided: u32,
}

/// An app part for [`pack`]: the `.app` record and its data, and its interface number.
pub struct AppIn<'a> {
    pub app: &'a [u8],
    pub iface_needed: u32,
}

/// tools/txu_pack.py `pack`: a TXU1 package from a host bitstream and/or an application, the host key read from the
/// bitstream. Nothing is checked here; [`Txu::parse`] and [`Txu::check`] do that.
pub fn pack(host: Option<HostIn>, app: Option<AppIn>, version: &str, commit: &str) -> Vec<u8> {
    let fpga = TXU1_FPGA;
    let mut parts: Vec<(u32, &[u8], u32)> = Vec::new();
    if let Some(h) = &host {
        parts.push((KIND_HOST, h.bit, 0));
    }
    if let Some(a) = &app {
        parts.push((KIND_APP, a.app, fpga.app_addr()));
    }
    let hdr_len = HDR_FIXED + PART_BYTES * parts.len();
    let host_key = host.as_ref().and_then(|h| axss(h.bit, fpga.bit_slot)).unwrap_or(0);
    let mut hdr = MAGIC.to_vec();
    for word in [
        hdr_len as u32,
        0,
        host_key,
        host.as_ref().map_or(0, |h| h.iface_provided),
        app.as_ref().map_or(0, |a| a.iface_needed),
        parts.len() as u32,
    ] {
        hdr.extend_from_slice(&word.to_le_bytes());
    }
    for (text, n) in [("trx", 8), (version, 24), (commit, 16)] {
        let mut field = text.as_bytes().to_vec();
        field.resize(n, 0);
        hdr.extend_from_slice(&field);
    }
    let mut body = Vec::new();
    let mut off = hdr_len;
    for (i, &(kind, bytes, addr)) in parts.iter().enumerate() {
        for word in [kind, off as u32, bytes.len() as u32, addr] {
            hdr.extend_from_slice(&word.to_le_bytes());
        }
        hdr.extend_from_slice(&Sha256::digest(bytes));
        body.extend_from_slice(bytes);
        off += bytes.len();
        if i + 1 < parts.len() {
            let pad = bytes.len().next_multiple_of(4) - bytes.len();
            body.resize(body.len() + pad, 0);
            off += pad;
        }
    }
    let crc = crc32(&hdr);
    hdr.extend_from_slice(&crc.to_le_bytes());
    hdr.extend_from_slice(&body);
    hdr
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `.bit` file: the header fields a-d, then `e` and the length of the configuration data: padding, the sync word,
    /// a write of IDCODE, NOOPs, a write of AXSS, a type-2 block, DESYNC.
    pub(crate) fn bit(idcode: u32, axss: Option<u32>) -> Vec<u8> {
        let type1_write = |reg: u32, count: u32| (1u32 << 29 | 2 << 27 | reg << 13 | count).to_be_bytes();
        let mut config = vec![0xFF; 16];
        config.extend_from_slice(&SYNC_WORD.to_be_bytes());
        config.extend_from_slice(&0x2000_0000u32.to_be_bytes()); // NOOP
        config.extend_from_slice(&type1_write(0x0C, 1));
        config.extend_from_slice(&idcode.to_be_bytes());
        if let Some(v) = axss {
            config.extend_from_slice(&type1_write(0x0D, 1));
            config.extend_from_slice(&v.to_be_bytes());
        }
        config.extend_from_slice(&type1_write(0x02, 0)); // FDRI, count in the type-2 word
        config.extend_from_slice(&(2u32 << 29 | 3).to_be_bytes());
        config.extend_from_slice(&[0x5A; 12]);
        config.extend_from_slice(&type1_write(0x04, 1));
        config.extend_from_slice(&0x0Du32.to_be_bytes());
        let mut out = vec![0x00, 0x09, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x00, 0x00, 0x01];
        for (key, text) in [(b'a', &b"host;UserID=0XFFFFFFFF\0"[..]), (b'b', b"7a50tcsg324\0"), (b'c', b"2026/11/20\0"), (b'd', b"12:00:00\0")]
        {
            out.push(key);
            out.extend_from_slice(&(text.len() as u16).to_be_bytes());
            out.extend_from_slice(text);
        }
        out.push(b'e');
        out.extend_from_slice(&(config.len() as u32).to_be_bytes());
        out.extend_from_slice(&config);
        out
    }

    /// A `.app` record at 0x30000 with `n` data bytes, entering at its start.
    pub(crate) fn app(n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for word in [0x3_0000u32, n as u32, 0x3_0000] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend((0..n).map(|i| (i * 7) as u8));
        out.resize(REC_HDR + n.next_multiple_of(4), 0);
        out
    }

    pub(crate) const KEY: u32 = 0x5A17_C0DE;

    pub(crate) fn both() -> Vec<u8> {
        let bit = bit(0x1362_C093, Some(KEY));
        pack(Some(HostIn { bit: &bit, iface_provided: 1 }), Some(AppIn { app: &app(10), iface_needed: 1 }), "main-1a2b3c4d", "1a2b3c4d")
    }

    /// The writer's packages pass the reader; the fields come back.
    #[test]
    fn packed_packages_parse_and_check() {
        let data = both();
        let txu = Txu::parse(&data).unwrap();
        assert_eq!((txu.version.as_str(), txu.commit.as_str(), txu.host_key), ("main-1a2b3c4d", "1a2b3c4d", KEY));
        assert_eq!((txu.iface_provided, txu.iface_needed, txu.fpga.name), (1, 1, "XC7A50T"));
        let (host, app) = (txu.host().unwrap(), txu.app().unwrap());
        assert_eq!((host.off, host.flash_addr, app.flash_addr), (176, 0, 0x22_0000));
        assert_eq!(app.off % 4, 0);
        assert_eq!(app.bytes(&data), &self::app(10)[..]);
        txu.check(&data).unwrap();
        let only_app = pack(None, Some(AppIn { app: &self::app(10), iface_needed: 1 }), "v", "c");
        let txu = Txu::parse(&only_app).unwrap();
        assert_eq!((txu.parts.len(), txu.host_key, txu.iface_provided), (1, 0, 0));
        txu.check(&only_app).unwrap();
        // zlib's check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Re-seal a changed header: a new CRC-32.
    fn reseal(data: &mut [u8]) {
        let hdr_len = le32(data, 4) as usize;
        let crc = crc32(&data[..hdr_len - 4]);
        data[hdr_len - 4..hdr_len].copy_from_slice(&crc.to_le_bytes());
    }

    fn parse_err(data: &[u8]) -> String {
        format!("{:#}", Txu::parse(data).unwrap_err())
    }

    /// T2: every rule of `txu_probe` with its words.
    #[test]
    fn every_header_rule_is_refused_with_its_words() {
        let good = both();
        let edits = |words: &[(usize, u32)]| {
            let mut d = good.clone();
            for &(at, word) in words {
                d[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
            reseal(&mut d);
            parse_err(&d)
        };
        let edit = |at: usize, word: u32| edits(&[(at, word)]);
        assert!(parse_err(b"TXU2....").starts_with(NOT_TXU));
        assert!(parse_err(&good[..60]).starts_with(FORMAT));
        assert!(edit(24, 3).contains("3 parts"));
        assert!(edit(4, 129).contains("hdr_len 129"));
        assert!(edit(8, 1).contains("flags 0x1"));
        let mut bad_crc = good.clone();
        bad_crc[40] ^= 1;
        assert!(parse_err(&bad_crc).starts_with(CRC));
        let mut origin = good.clone();
        origin[28..31].copy_from_slice(b"gid");
        reseal(&mut origin);
        assert!(parse_err(&origin).starts_with(ORIGIN));
        // Part 0 (host) at 76, part 1 (app) at 124: kind, off, size, flash_addr.
        assert!(edit(76, 3).contains("kind 3"));
        assert!(edits(&[(124, 1), (136, 0)]).contains("two host parts"));
        assert!(edit(88, 0x1000).contains("host part at flash 0x001000"));
        assert!(edit(136, 0x3C_0000).contains("app part at flash 0x3c0000"));
        assert!(edit(84, 0).contains("host part of 0 bytes"));
        assert!(edit(84, 0x22_0001).contains("the slot holds 0x220000"));
        assert!(edit(80, 178).contains("not 4-aligned"));
        assert!(edit(80, 172).contains("overlaps"));
        assert!(edit(132, 0x10_0000).contains("runs past the end"));
        let mut tail = good.clone();
        tail.extend_from_slice(&[0; 4]);
        assert!(parse_err(&tail).contains("4 bytes behind the last part"));
        assert!(edit(12, 0).contains("host_key 0x0"));
        assert!(edit(16, 0).contains("iface_provided 0"));
        assert!(edit(20, 0).contains("iface_needed 0 with an app part"));
    }

    /// T2: the checks of `txu_stage`: SHA-256, the `.bit` header length, the IDCODE, the AXSS stamp, the record.
    #[test]
    fn stage_checks_refuse_with_their_words() {
        let check = |data: &[u8]| Txu::parse(data).and_then(|t| t.check(data)).map_err(|e| format!("{e:#}"));
        let mut data = both();
        let last = data.len() - 1;
        data[last] ^= 0xFF;
        assert!(check(&data).unwrap_err().contains("the app part does not match its SHA-256"));
        let package = |bit: &[u8], app: &[u8]| {
            pack(Some(HostIn { bit, iface_provided: 1 }), Some(AppIn { app, iface_needed: 1 }), "v", "c")
        };
        let mut short = bit(0x0362_C093, Some(KEY));
        short.push(0);
        assert!(check(&package(&short, &app(8))).unwrap_err().contains("header length matches"));
        assert!(check(&package(&bit(0x0363_1093, Some(KEY)), &app(8))).unwrap_err().contains("not a bitstream for this FPGA"));
        // No stamp: the packer finds none, the header's key is 0 and the reader refuses it already.
        assert!(check(&package(&bit(0x0362_C093, None), &app(8))).unwrap_err().contains("host_key 0x0"));
        let mut odd = app(8);
        odd[8..12].copy_from_slice(&0x4_0000u32.to_le_bytes());
        assert!(check(&package(&bit(0x0362_C093, Some(KEY)), &odd)).unwrap_err().contains("start address"));
        let mut low = app(8);
        low[0..4].copy_from_slice(&0x8000u32.to_le_bytes());
        assert!(check(&package(&bit(0x0362_C093, Some(KEY)), &low)).unwrap_err().contains("load address"));
        let mut long = app(8);
        long.extend_from_slice(&[0; 4]);
        assert!(check(&package(&bit(0x0362_C093, Some(KEY)), &long)).unwrap_err().contains("record and its length disagree"));
    }

    /// T3: the regions after `fw_write_parts`, into an erased flash and over an old one; a one-part package leaves the
    /// other region.
    #[test]
    fn write_into_matches_fw_write_parts() {
        let data = both();
        let txu = Txu::parse(&data).unwrap();
        let (bit, app) = (txu.host().unwrap().bytes(&data).to_vec(), txu.app().unwrap().bytes(&data).to_vec());
        for fill in [0xFFu8, 0x00] {
            let mut image = vec![fill; 16 << 20];
            txu.write_into(&data, &mut image);
            assert_eq!(image[..bit.len()], bit[..]);
            assert!(image[bit.len()..0x22_0000].iter().all(|&b| b == 0xFF), "the rest of the bitstream slot is erased");
            assert_eq!(image[0x22_0000..0x22_0000 + app.len()], app[..]);
            assert!(image[0x22_0000 + app.len()..0x22_1000].iter().all(|&b| b == 0xFF), "the app's sector");
            assert!(image[0x22_1000..].iter().all(|&b| b == fill), "beyond: as it was");
        }
        let only_app = pack(None, Some(AppIn { app: &self::app(5000), iface_needed: 1 }), "v", "c");
        let txu = Txu::parse(&only_app).unwrap();
        let mut image = vec![0u8; 16 << 20];
        txu.write_into(&only_app, &mut image);
        assert!(image[..0x22_0000].iter().all(|&b| b == 0), "no host part: the bitstream slot stays");
        assert_eq!(image[0x22_0000..0x22_0000 + 5012], self::app(5000)[..]);
        assert!(image[0x22_0000 + 5012..0x22_2000].iter().all(|&b| b == 0xFF));
        assert!(image[0x22_2000..].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_fpga_table() {
        assert_eq!(fpga(2).map(|f| (f.app_addr(), f.app_slot)), Some((0x22_0000, 0x1E_0000)));
        assert_eq!(fpga(3).map(|f| (f.app_addr(), f.app_slot)), Some((0x3C_0000, 0x1C_0000)));
        assert!(fpga(1).is_none());
        // The slots end where /flash starts: 0x400000 and 0x580000 (w25q_flash.cc:47-59).
        assert_eq!(FPGAS.map(|f| f.bit_slot + f.app_slot), [0x40_0000, 0x58_0000]);
        assert_eq!(bit_idcode(&bit(0x0363_1093, None)[bit_length_from_header(&bit(0, None)).unwrap().1..]), Some(0x0363_1093));
    }
}
