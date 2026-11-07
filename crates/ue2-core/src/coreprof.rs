//! S44: a MultiCore core in place of the C64 core (`--core`, `--handover`, `--icap`). Spec:
//! docs/specs/S44-multicore-cores.md.
//!
//! The generic core application (TRX64U-MultiCore `coreapp/`) runs on every core of the MultiCore host. What tells it
//! which core it is on: the four ID bytes at 0x101A0000 (`hw_read_id`, `core_find`), the ICAP loader's ID byte 0x7A at
//! 0x10060600 (`board_detect`: the multicore profile) and the hand-over page the host app writes at 0x03EFF000
//! (`handover.h`). The core's own registers lie in the same 64 KB block as the host's pixel layer (`CoreIo::rd/wr` take
//! a byte offset from 0x101A0000). A [`CoreProfile`] says what the block answers for one core, a [`Handover`] what the
//! page holds.

use std::collections::BTreeMap;

/// The page's address and size (host-app/software/hostapp/handover.h `HANDOVER_ADDR`).
pub const HANDOVER_ADDR: u32 = 0x03EF_F000;
pub const HANDOVER_PAGE: usize = 4096;
const MAGIC: u32 = 0x4858_5254; // "TRXH"
const VERSION: u32 = 1;
const PATH_MAX: usize = 256;
// struct Handover: magic 0, version 4, sum 8, video 12, path 16, core_name 272, core_version 320, host_build 344,
// id_sum 360, core_id 364 (24 bytes).
const O_VIDEO: usize = 12;
const O_PATH: usize = 16;
const O_NAME: usize = O_PATH + PATH_MAX;
const O_VERSION: usize = O_NAME + 48;
const O_HOST: usize = O_VERSION + 24;
const O_ID_SUM: usize = O_HOST + 16;
const O_CORE_ID: usize = O_ID_SUM + 4;
const CORE_ID_LEN: usize = 24;
/// The hand-over's `core_id` field length, for the callers that check a value against it.
pub const HANDOVER_CORE_ID_MAX: usize = CORE_ID_LEN - 1;
const STRUCT_LEN: usize = O_CORE_ID + CORE_ID_LEN;

/// "Menu requested" (handover.h T135 D3): two words at +0xFF8, outside the struct and its sum.
pub const MENU_OFFSET: usize = 0xFF8;
pub const MENU_MAGIC: u32 = 0x554E_454D; // "MENU"

/// The CAPS byte a core profile answers when it names none: b6 the pixel layer, b7 RGB565 (trx_io.h `TRX_CAPS_VIDEO`,
/// `TRX_CAPS_PX565`). No C64 page (gate, state bus, engine, debug, cartridge, session) is there.
pub const CORE_CAPS: u8 = 0xC0;

/// The board a core runs on (coreapp board.h `BoardKind`): the ICAP loader answers 0x7A only on the MultiCore host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoreBoard {
    #[default]
    MultiCore,
    Monolith,
}

/// One core's view of the TRX block (S44 §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreProfile {
    /// What `--core` named, for the log.
    pub name: String,
    /// The ID word at 0x000-0x003 (`d.id` in coreapp/cores/*.cc).
    pub id: [u8; 4],
    /// The CAPS byte at 0x004.
    pub caps: u8,
    pub board: CoreBoard,
    /// Register reset values: written values read back, a reset restores these (block offset to byte).
    pub regs: BTreeMap<u32, u8>,
    /// Read-only registers: they read this whatever is written.
    pub ro: BTreeMap<u32, u8>,
    /// The page the host app would have written, when the profile names one.
    pub handover: Option<Handover>,
    /// Phase B: the ELF of the application this core runs, for the symbols and fault hooks after a core start; None
    /// turns the hooks off then (the start image's addresses mean nothing in another program).
    pub symbols: Option<std::path::PathBuf>,
}

impl CoreProfile {
    pub fn new(name: &str, id: [u8; 4]) -> Self {
        CoreProfile {
            name: name.to_string(),
            id,
            caps: CORE_CAPS,
            board: CoreBoard::MultiCore,
            regs: BTreeMap::new(),
            ro: BTreeMap::new(),
            handover: None,
            symbols: None,
        }
    }

    /// An ID word of exactly four ASCII bytes.
    pub fn parse_id(s: &str) -> Result<[u8; 4], String> {
        let b = s.as_bytes();
        if b.len() != 4 || !b.iter().all(|c| (0x20..0x7F).contains(c)) {
            return Err(format!("core id '{s}': four printable ASCII characters, e.g. AMIG"));
        }
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// Whether block offset `off` belongs to the core (the register file) and not to the host's part of the block:
    /// the ID word, CAPS, IRQ_EN, IRQ_CAUSE and page 6 (pixel layer, still capture, text layer) stay the host's.
    pub fn owns(off: u32) -> bool {
        !matches!(off, 0x000..=0x004 | 0x010 | 0x014) && off & !0xFFF != 0x6000
    }

    /// Checks every register offset: inside the 64 KB block and not one of the host's.
    pub fn check(&self) -> Result<(), String> {
        for &off in self.regs.keys().chain(self.ro.keys()) {
            if off >= 0x1_0000 {
                return Err(format!("register {off:#x} is outside the core window (0x0000-0xFFFF)"));
            }
            if !Self::owns(off) {
                return Err(format!(
                    "register {off:#x} is the host's (ID 0x000-0x003, CAPS 0x004, IRQ_EN 0x010, IRQ_CAUSE 0x014, page 6)"
                ));
            }
        }
        Ok(())
    }
}

/// The hand-over page's fields (handover.h `struct Handover`, filled as `handover_fill` does).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handover {
    /// The .cor path as the host opened it.
    pub path: String,
    /// The manifest's name (`cor_name` of the descriptor).
    pub core_name: String,
    pub core_version: String,
    /// The host key, 0 = none (the field stays "").
    pub host_key: u32,
    /// `video_pack(sys, hdmi)`, 0 = none chosen.
    pub video: u32,
    /// The core's id (core.toml id), "" = none.
    pub core_id: String,
}

/// The video word (app/video/videomode.cc `video_pack`): b31 set, b15:8 the system mode, b7:0 the HDMI mode.
pub fn video_pack(sys: u8, hdmi: u8) -> u32 {
    0x8000_0000 | u32::from(sys) << 8 | u32::from(hdmi)
}

/// `[SYS:]MODE` with SYS pal50 (default), ntsc60, pal60, ntsc50 and MODE 480p (576p), 720p, 1080p, 800x600,
/// 1024x768, 1280x1024 (videomode.h `VID_SYS_*`, `VID_HDMI_*`); `none` or `0` = no mode chosen.
pub fn parse_video(s: &str) -> Result<u32, String> {
    if s == "none" || s == "0" {
        return Ok(0);
    }
    let (sys, mode) = match s.split_once(':') {
        Some((sys, mode)) => (sys, mode),
        None => ("pal50", s),
    };
    let sys = match sys {
        "pal50" | "pal" => 0,
        "ntsc60" | "ntsc" => 1,
        "pal60" => 2,
        "ntsc50" => 3,
        other => return Err(format!("video system '{other}': pal50, ntsc60, pal60 or ntsc50")),
    };
    let hdmi = match mode {
        "480p" | "576p" => 0,
        "720p" => 1,
        "1080p" => 2,
        "800x600" => 3,
        "1024x768" => 4,
        "1280x1024" => 5,
        other => return Err(format!("video mode '{other}': 480p, 576p, 720p, 1080p, 800x600, 1024x768 or 1280x1024")),
    };
    Ok(video_pack(sys, hdmi))
}

impl Handover {
    /// `--handover KEY=VALUE,...` over `self`: path, core (the manifest name), id, version, host (8 hex digits), video.
    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        for item in spec.split(',').filter(|s| !s.is_empty()) {
            let (k, v) = item.split_once('=').ok_or_else(|| format!("--handover: '{item}' is not KEY=VALUE"))?;
            self.set(k, v)?;
        }
        self.check()
    }

    /// One field by its `--handover` key (also the keys of a profile's `[handover]` table).
    pub fn set(&mut self, k: &str, v: &str) -> Result<(), String> {
        match k {
            "path" => self.path = v.to_string(),
            "core" | "name" => self.core_name = v.to_string(),
            "id" => self.core_id = v.to_string(),
            "version" => self.core_version = v.to_string(),
            "host" => {
                self.host_key =
                    u32::from_str_radix(v.trim_start_matches("0x"), 16).map_err(|e| format!("handover host={v}: {e}"))?
            }
            "video" => self.video = parse_video(v)?,
            other => return Err(format!("handover: unknown key '{other}' (path, core, id, version, host, video)")),
        }
        Ok(())
    }

    /// The strings fit their fields with their terminator (handover_valid needs the last byte 0).
    pub fn check(&self) -> Result<(), String> {
        for (what, s, cap) in [
            ("path", &self.path, PATH_MAX),
            ("core", &self.core_name, 48),
            ("version", &self.core_version, 24),
            ("id", &self.core_id, CORE_ID_LEN),
        ] {
            if s.len() >= cap {
                return Err(format!("--handover {what}: {} bytes, the field holds {}", s.len(), cap - 1));
            }
        }
        Ok(())
    }

    /// The struct's bytes as `handover_fill` leaves them (wiped first, strings terminated, the sums set).
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; STRUCT_LEN];
        let put = |b: &mut [u8], at: usize, s: &str| b[at..at + s.len()].copy_from_slice(s.as_bytes());
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&VERSION.to_le_bytes());
        b[O_VIDEO..O_VIDEO + 4].copy_from_slice(&self.video.to_le_bytes());
        put(&mut b, O_PATH, &self.path);
        put(&mut b, O_NAME, &self.core_name);
        put(&mut b, O_VERSION, &self.core_version);
        if self.host_key != 0 {
            put(&mut b, O_HOST, &format!("{:08x}", self.host_key));
        }
        let sum = sum(&b);
        b[8..12].copy_from_slice(&sum.to_le_bytes());
        if !self.core_id.is_empty() {
            put(&mut b, O_CORE_ID, &self.core_id);
            let id_sum = id_sum(&b, sum);
            b[O_ID_SUM..O_ID_SUM + 4].copy_from_slice(&id_sum.to_le_bytes());
        }
        b
    }

    /// Writes the page into guest DDR: the struct, the rest of the 4 KB page 0 (no menu marker).
    pub fn write(&self, ram: &mut [u8]) {
        let at = HANDOVER_ADDR as usize;
        ram[at..at + HANDOVER_PAGE].fill(0);
        let b = self.bytes();
        ram[at..at + b.len()].copy_from_slice(&b);
    }
}

/// `handover_sum`: from byte 12 up to `id_sum`, rotate left 5 and XOR.
fn sum(b: &[u8]) -> u32 {
    b[12..O_ID_SUM].iter().fold(0x1F3D_5B79u32, |s, &c| s.rotate_left(5) ^ u32::from(c))
}

/// `handover_id_sum`: the id's 24 bytes over `sum`, never 0.
fn id_sum(b: &[u8], sum: u32) -> u32 {
    let s = b[O_CORE_ID..O_CORE_ID + CORE_ID_LEN].iter().fold(0x9E37_79B1 ^ sum, |s, &c| s.rotate_left(5) ^ u32::from(c));
    if s == 0 {
        1
    } else {
        s
    }
}

/// Whether the page at [`HANDOVER_ADDR`] passes `handover_valid` (magic, version, sum, terminators).
pub fn page_valid(ram: &[u8]) -> bool {
    let b = &ram[HANDOVER_ADDR as usize..][..STRUCT_LEN];
    let word = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    word(0) == MAGIC
        && word(4) == VERSION
        && word(8) == sum(b)
        && [O_NAME - 1, O_VERSION - 1, O_HOST - 1, O_ID_SUM - 1].iter().all(|&i| b[i] == 0)
}

/// `handover_menu_take` without the clear: the marker a core application leaves before its way back to the host.
pub fn menu_requested(ram: &[u8]) -> bool {
    let at = HANDOVER_ADDR as usize + MENU_OFFSET;
    let w0 = u32::from_le_bytes(ram[at..at + 4].try_into().unwrap());
    let w1 = u32::from_le_bytes(ram[at + 4..at + 8].try_into().unwrap());
    w0 == MENU_MAGIC && w1 == !MENU_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    /// handover_fill's bytes for one block, worked by hand from handover.cc: the sums are what its C computes.
    #[test]
    fn layout_and_sums_match_handover_cc() {
        let h = Handover {
            path: "/SD/cores/amiga1200.cor".into(),
            core_name: "amiga1200".into(),
            core_version: "1.2".into(),
            host_key: 0x1234_ABCD,
            video: parse_video("1080p").unwrap(),
            core_id: "amiga1200".into(),
        };
        let b = h.bytes();
        assert_eq!(b.len(), 388, "sizeof(Handover) = 364 + 24");
        assert_eq!(&b[0..4], b"TRXH");
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 0x8000_0002);
        assert_eq!(&b[16..39], b"/SD/cores/amiga1200.cor");
        assert_eq!(&b[272..281], b"amiga1200");
        assert_eq!(&b[344..352], b"1234abcd", "lowercase, as handover_fill writes it");
        // The C loop, written out once more without the helpers.
        let mut s = 0x1F3D_5B79u32;
        for &c in &b[12..360] {
            s = (s << 5 | s >> 27) ^ u32::from(c);
        }
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), s);
        let mut t = 0x9E37_79B1u32 ^ s;
        for &c in &b[364..388] {
            t = (t << 5 | t >> 27) ^ u32::from(c);
        }
        assert_eq!(u32::from_le_bytes(b[360..364].try_into().unwrap()), t);
        let mut ram = vec![0xAAu8; crate::bus::RAM_SIZE];
        h.write(&mut ram);
        assert!(page_valid(&ram));
        assert!(!menu_requested(&ram), "the page of a core start has no marker");
        ram[HANDOVER_ADDR as usize + 20] ^= 1;
        assert!(!page_valid(&ram), "a changed path byte breaks the sum");
    }

    #[test]
    fn no_id_no_id_sum_and_no_host_key() {
        let b = Handover { core_name: "c128".into(), ..Handover::default() }.bytes();
        assert_eq!(&b[360..388], &[0; 28]);
        assert_eq!(&b[344..360], &[0; 16]);
    }

    #[test]
    fn handover_spec_and_video() {
        let mut h = Handover::default();
        h.apply("core=amiga1200,id=amiga1200,path=/SD/cores/amiga1200.cor,video=ntsc60:720p,host=0a0b0c0d").unwrap();
        assert_eq!(h.core_name, "amiga1200");
        assert_eq!(h.video, 0x8000_0101);
        assert_eq!(h.host_key, 0x0A0B_0C0D);
        assert_eq!(parse_video("480p").unwrap(), 0x8000_0000);
        assert_eq!(parse_video("none").unwrap(), 0);
        assert!(parse_video("4k").is_err());
        assert!(h.apply("colour=red").is_err());
        assert!(h.apply(&format!("id={}", "x".repeat(24))).is_err(), "24 bytes leave no terminator");
    }

    #[test]
    fn profile_checks_the_host_offsets() {
        let mut p = CoreProfile::new("amiga1200", CoreProfile::parse_id("AMIG").unwrap());
        p.ro.insert(0x00C, 0x18);
        p.regs.insert(0xE010, 0);
        assert!(p.check().is_ok());
        for host in [0x000, 0x004, 0x010, 0x014, 0x6080] {
            let mut q = p.clone();
            q.regs.insert(host, 1);
            assert!(q.check().is_err(), "{host:#x}");
        }
        assert!(CoreProfile::parse_id("AMIGA").is_err());
    }
}
