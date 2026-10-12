//! S41: core dev mode, FPGA blocks absent (`--absent`, `--absent-mode`, `--absent-continue`). Spec:
//! docs/specs/S41-core-dev-mode.md.
//!
//! TRX64-Ultimate builds bitstreams with optional blocks switched off (`TRX_FEATURES`, designs/c64_basic/build.tcl). A
//! block that is not built leaves its app window answering in one of two ways:
//! - [`Mode::NoAck`]: the access never gets an ack, so the RISC-V hangs on that load or store. The machine halts and
//!   reports it, or with `--absent-continue` logs it and completes it as `ack0`.
//! - [`Mode::Ack0`]: reads return 0 and writes are dropped.
//!
//! The bus decides this per 256-byte grain ([`AbsentMap`]) before any device sees the access. What a block does beyond
//! its app window (its IRQ, its C64 side, its DDR fetches, CAPS2) is the business of the device that models it.

use std::collections::{BTreeMap, BTreeSet};

use crate::io::{IO_BASE, IO_GRAIN, IO_SIZE};

/// One optional block, by its `TRX_FEATURES` name, or the whole swappable core (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Block {
    Reu,
    Sampler,
    Uci,
    Jtag,
    Turbo,
    Apply,
    /// dfx-trial's NO_STREAMS (top.v): no UDP stream senders in the host; no bus difference for the app.
    Streams,
    /// §5: every register window of the swappable core (T79 phase B, the grey box).
    Core,
}

/// The core's register windows (§5): the regions whose ack comes from layer 2 in dfx-trial's ultimate_sys.vhd decode
/// (bc97bbe): `r_drive` 0x020000-0x023FFF (drive A, `drv_resp`), `r_gcr` 0x060500-0x0605FF (`gcr_resp`), `r_cw`
/// 0x04C000-0x04CFFF (the cartridge's EEPROM), 0x048000-0x0480FF (the sampler's bytes) and 0x1A0000-0x1AFFFF
/// (trx_io: the trx_sys pages, the ring engine, the pixel layer) (`cw_ack`), `r_dma` 0x050000-0x05FFFF and `r_rom`
/// 0x188000-0x18FFFF (`dma_ack`, after the core's `dma_done_tgl`). Everything else is acked by ultimate_sys itself
/// (`ack_r`): the cart registers (`r_page` page 0), the core config (page 3), the palette (`r_pal`), the UltiSID curves
/// (`r_sidcv`), the IEC processor's idle constants (`r_iec`), and UE2's drive B at 0x10024000 (`r_none`); those stay
/// served by UE2's devices, with the C64 stopped. The UCI is the host's too (`r_uci`).
const CORE_WINDOWS: &[(u32, u32)] = &[
    (0x1002_0000, 0x1002_4000),
    (0x1004_8000, 0x1004_8100),
    (0x1004_C000, 0x1004_D000),
    (0x1005_0000, 0x1006_0000),
    (0x1006_0500, 0x1006_0600),
    (0x1018_8000, 0x1019_0000),
    (0x101A_0000, 0x101B_0000),
];

impl Block {
    pub const ALL: [Block; 8] =
        [Block::Reu, Block::Sampler, Block::Uci, Block::Jtag, Block::Turbo, Block::Apply, Block::Streams, Block::Core];

    pub fn name(self) -> &'static str {
        match self {
            Block::Reu => "reu",
            Block::Sampler => "sampler",
            Block::Uci => "uci",
            Block::Jtag => "jtag",
            Block::Turbo => "turbo",
            Block::Apply => "apply",
            Block::Streams => "streams",
            Block::Core => "core",
        }
    }

    pub fn by_name(name: &str) -> Option<Block> {
        Block::ALL.into_iter().find(|b| b.name() == name)
    }

    /// The app windows that answer as absent, as `start..end` IO addresses:
    /// - sampler: `SAMPLER_BASE` 0x10048000-0x100480FF, trx_io page 8 (ultimate_sys.vhd region `r_cw`, `X"0480"`);
    /// - uci: `CMD_IF_BASE` 0x10044000-0x10045FFF (ultimate_sys.vhd region `r_uci`, `gen_no_uci`);
    /// - core: [`CORE_WINDOWS`].
    ///
    /// reu, turbo, jtag, apply and streams have none: the REU's registers are on the C64 side, the turbo engine's and
    /// APPLY's sit in pages that stay (c64_top.v `NO_TURBO`, trx_engine.v `NO_APPLY`), the JTAG master is not on the
    /// app's bus, and without the streams (top.v `NO_STREAMS`) the template writes (`r_udp`) are still acked.
    pub fn windows(self) -> &'static [(u32, u32)] {
        match self {
            Block::Sampler => &[(0x1004_8000, 0x1004_8100)],
            Block::Uci => &[(0x1004_4000, 0x1004_6000)],
            Block::Core => CORE_WINDOWS,
            Block::Reu | Block::Jtag | Block::Turbo | Block::Apply | Block::Streams => &[],
        }
    }

    /// The answer the bitstream gives (§2, §5): the NO_SAMPLER stub echoes every request with 0 from TRX64-Ultimate
    /// bd16bee on (c64_core.v `smp_a_echo`); `gen_no_uci` acks with 0; without a core the host's decoupler
    /// (`core_decouple`, T79.1) answers the core's windows at once with 0. `noack` for the core stands for a host
    /// without the decoupler.
    pub fn default_mode(self) -> Mode {
        Mode::Ack0
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// How an absent app window answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The access does not complete.
    NoAck,
    /// Reads return 0, writes are dropped.
    Ack0,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::NoAck => "noack",
            Mode::Ack0 => "ack0",
        }
    }

    fn by_name(name: &str) -> Option<Mode> {
        match name {
            "noack" => Some(Mode::NoAck),
            "ack0" => Some(Mode::Ack0),
            _ => None,
        }
    }
}

/// What the command line asked for: the blocks absent from power-on, the modes given for blocks, and whether a
/// `noack` stall halts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AbsentConfig {
    /// Absent from the first instruction.
    pub blocks: Vec<Block>,
    /// Modes given with `--absent-mode`; a block without one has [`Block::default_mode`].
    pub modes: BTreeMap<Block, Mode>,
    /// `--absent-continue`: log a stall and complete it as `ack0`.
    pub keep_going: bool,
}

impl AbsentConfig {
    /// `--absent <list>` (comma separated names, or `none`), `--absent-mode <mode | block=mode,...>` and
    /// `--absent-continue`. A bare mode applies to every block that has an app window; a block named in the modes
    /// need not be in the list (`core=ack0` sets the mode of a later `core absent`), but it must have a window.
    pub fn parse(list: Option<&str>, modes: Option<&str>, keep_going: bool) -> Result<Self, String> {
        let mut cfg = AbsentConfig { keep_going, ..AbsentConfig::default() };
        if let Some(list) = list {
            let names: Vec<&str> = list.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
            if names == ["none"] {
                // All present.
            } else {
                for name in names {
                    let block = match Block::by_name(name) {
                        Some(b) => b,
                        None if name == "none" => return Err("--absent: 'none' stands alone".into()),
                        None => return Err(format!("--absent: unknown block '{name}' (reu, sampler, uci, jtag, turbo, apply, streams, core, none)")),
                    };
                    if !cfg.blocks.contains(&block) {
                        cfg.blocks.push(block);
                    }
                }
            }
        }
        if let Some(modes) = modes {
            for item in modes.split(',').map(str::trim).filter(|m| !m.is_empty()) {
                match item.split_once('=') {
                    None => {
                        let mode = Mode::by_name(item).ok_or_else(|| format!("--absent-mode: unknown mode '{item}' (noack, ack0)"))?;
                        for block in Block::ALL.into_iter().filter(|b| !b.windows().is_empty()) {
                            cfg.modes.insert(block, mode);
                        }
                    }
                    Some((name, mode)) => {
                        let block = Block::by_name(name.trim())
                            .ok_or_else(|| format!("--absent-mode: unknown block '{name}' (sampler, uci, core)"))?;
                        if block.windows().is_empty() {
                            return Err(format!("--absent-mode: {} has no app window, so no mode (sampler, uci, core have one)", block.name()));
                        }
                        let mode = Mode::by_name(mode.trim()).ok_or_else(|| format!("--absent-mode: unknown mode '{mode}' (noack, ack0)"))?;
                        cfg.modes.insert(block, mode);
                    }
                }
            }
        }
        Ok(cfg)
    }

    /// Whether any of the three flags asks for something.
    pub fn is_set(&self) -> bool {
        !self.blocks.is_empty() || !self.modes.is_empty() || self.keep_going
    }

    pub fn has(&self, block: Block) -> bool {
        self.blocks.contains(&block)
    }

    pub fn mode(&self, block: Block) -> Mode {
        self.modes.get(&block).copied().unwrap_or(block.default_mode())
    }
}

/// The bus's view of the absent blocks: per 256-byte IO grain the blocks whose window covers it, and per block whether
/// it is absent now and how it answers.
#[derive(Clone, Debug)]
pub struct AbsentMap {
    /// [`Block::bit`]s per grain of the IO space.
    grains: Vec<u8>,
    /// Absent now.
    on: u8,
    modes: [Mode; Block::ALL.len()],
    /// `--absent-continue`.
    pub keep_going: bool,
    /// Stalls completed as `ack0` under `--absent-continue`, per address.
    pub stalls: BTreeMap<u32, StallCount>,
    /// The stall sites seen: address, write, PC.
    sites: BTreeSet<(u32, bool, u32)>,
}

/// Stalls at one address, for the summary at the end of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StallCount {
    pub block: Option<Block>,
    pub reads: u64,
    pub writes: u64,
    /// The PC of the first stall here.
    pub first_pc: u32,
}

/// The first absent byte a `noack` access reached in the instruction that is executing: the access the CPU waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoAck {
    pub block: Block,
    pub addr: u32,
    pub write: bool,
    /// The byte a write would have written.
    pub val: u8,
    pub pc: u32,
    pub now: u64,
}

impl AbsentMap {
    pub fn new(cfg: &AbsentConfig) -> Self {
        let mut grains = vec![0u8; (IO_SIZE / IO_GRAIN) as usize];
        for block in Block::ALL {
            for &(start, end) in block.windows() {
                for g in (start - IO_BASE) / IO_GRAIN..(end - IO_BASE) / IO_GRAIN {
                    grains[g as usize] |= block.bit();
                }
            }
        }
        let mut map = AbsentMap {
            grains,
            on: 0,
            modes: Block::ALL.map(|b| cfg.mode(b)),
            keep_going: cfg.keep_going,
            stalls: BTreeMap::new(),
            sites: BTreeSet::new(),
        };
        for &block in &cfg.blocks {
            map.set(block, true);
        }
        map
    }

    pub fn set(&mut self, block: Block, absent: bool) {
        if absent {
            self.on |= block.bit();
        } else {
            self.on &= !block.bit();
        }
    }

    pub fn is_absent(&self, block: Block) -> bool {
        self.on & block.bit() != 0
    }

    pub fn mode(&self, block: Block) -> Mode {
        self.modes[block as usize]
    }

    /// The absent block answering for IO address `addr`, if any. Where windows overlap the core wins: without the
    /// core no block inside it answers either.
    #[inline]
    pub fn hit(&self, addr: u32) -> Option<(Block, Mode)> {
        let rel = addr.wrapping_sub(IO_BASE);
        if rel >= IO_SIZE {
            return None;
        }
        let bits = self.grains[(rel / IO_GRAIN) as usize] & self.on;
        if bits == 0 {
            return None;
        }
        let block = if bits & Block::Core.bit() != 0 {
            Block::Core
        } else {
            Block::ALL[bits.trailing_zeros() as usize]
        };
        Some((block, self.mode(block)))
    }

    /// Whether a `noack` stall can halt the machine: some absent block answers `noack` and `--absent-continue` is off.
    pub fn halts(&self) -> bool {
        !self.keep_going && Block::ALL.into_iter().any(|b| self.is_absent(b) && self.mode(b) == Mode::NoAck)
    }

    /// Count a stall that `--absent-continue` completed; true for the first one of its site (address, direction and
    /// PC), which is the one a run logs.
    pub fn count(&mut self, s: &NoAck) -> bool {
        let c = self.stalls.entry(s.addr).or_insert(StallCount { block: Some(s.block), first_pc: s.pc, ..StallCount::default() });
        if s.write {
            c.writes += 1;
        } else {
            c.reads += 1;
        }
        self.sites.insert((s.addr, s.write, s.pc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_modes_and_defaults() {
        let cfg = AbsentConfig::parse(Some("sampler,uci"), Some("sampler=noack"), false).unwrap();
        assert_eq!(cfg.blocks, [Block::Sampler, Block::Uci]);
        assert_eq!((cfg.mode(Block::Sampler), cfg.mode(Block::Uci), cfg.mode(Block::Core)), (Mode::NoAck, Mode::Ack0, Mode::Ack0));
        let all = AbsentConfig::parse(Some("reu,sampler,uci,jtag,turbo,apply,streams"), Some("noack"), true).unwrap();
        assert_eq!(all.blocks.len(), 7);
        assert!(AbsentConfig::parse(None, Some("streams=ack0"), false).is_err(), "no window, no mode");
        assert!(all.keep_going && [Block::Sampler, Block::Uci, Block::Core].iter().all(|&b| all.mode(b) == Mode::NoAck));
        assert_eq!(AbsentConfig::parse(Some("none"), None, false).unwrap(), AbsentConfig::default());
        assert!(!AbsentConfig::parse(Some("none"), None, false).unwrap().is_set());
        assert!(AbsentConfig::parse(Some("none,uci"), None, false).is_err());
        assert!(AbsentConfig::parse(Some("vic"), None, false).is_err());
        assert!(AbsentConfig::parse(None, Some("reu=noack"), false).unwrap_err().contains("no app window"));
        assert!(AbsentConfig::parse(None, Some("sampler=maybe"), false).is_err());
        let core = AbsentConfig::parse(None, Some("core=ack0"), false).unwrap();
        assert!(core.blocks.is_empty() && core.is_set() && core.mode(Block::Core) == Mode::Ack0);
    }

    #[test]
    fn the_map_answers_per_grain_and_the_core_wins() {
        let cfg = AbsentConfig::parse(Some("sampler"), Some("sampler=ack0,core=noack"), false).unwrap();
        let mut map = AbsentMap::new(&cfg);
        assert_eq!(map.hit(0x1004_8000), Some((Block::Sampler, Mode::Ack0)));
        assert_eq!(map.hit(0x1004_80FF), Some((Block::Sampler, Mode::Ack0)));
        assert_eq!(map.hit(0x1004_8100), None, "only the 256 bytes of page 8");
        assert_eq!(map.hit(0x1004_4000), None, "the UCI is present");
        assert!(!map.halts());
        map.set(Block::Core, true);
        assert_eq!(map.hit(0x1004_8000), Some((Block::Core, Mode::NoAck)));
        assert_eq!(map.hit(0x101A_0005), Some((Block::Core, Mode::NoAck)));
        assert_eq!(map.hit(0x1005_FFFF), Some((Block::Core, Mode::NoAck)));
        assert_eq!(map.hit(0x1004_4000), None, "the UCI is the host's");
        assert_eq!(map.hit(0x1010_0300), None, "the keyboard matrix is the host's");
        // ultimate_sys acks these itself (`ack_r`): the cart registers, the core config, the palette, the SID curves,
        // the IEC processor, UE2's drive B.
        for addr in [0x1004_0000, 0x1018_0010, 0x1018_0800, 0x1018_4000, 0x1018_5000, 0x1002_8000, 0x1002_4000] {
            assert_eq!(map.hit(addr), None, "{addr:#010x}");
        }
        for addr in [0x1002_0000, 0x1006_0500, 0x1004_C000, 0x1018_8000, 0x1018_F3FF, 0x101A_FFFF] {
            assert_eq!(map.hit(addr), Some((Block::Core, Mode::NoAck)), "{addr:#010x}");
        }
        assert!(map.halts());
        map.keep_going = true;
        assert!(!map.halts());
        map.set(Block::Core, false);
        assert_eq!(map.hit(0x101A_0005), None);
        assert_eq!(map.hit(0x2000_0000), None);
    }
}
