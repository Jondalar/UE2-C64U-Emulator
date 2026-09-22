//! U2PIO page, DDR2 PHY, CLOCKMEAS, mixers, LED strip, Blingboard, MMCM (T0).
//! Also hosts [`RegTable`], the table-driven register window shared by the S04 device modules.
//! Spec: docs/specs/S04-board-t0.md; the MDIO pins of the U2PIO page: docs/specs/S11-S14-later.md §S12.

use crate::devices::rmii::Phy;
use crate::io::{IoCtx, IoDevice, IoMap, IO_GRAIN};
use crate::machine::MachineConfig;

/// Behaviour of one byte offset of a [`RegTable`] window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reg {
    /// Read 0, writes ignored (RAZ/WI).
    Raz,
    /// Constant read, writes ignored.
    Const(u8),
    /// Read-back latch holding `val & mask`; `init` after reset.
    Latch { mask: u8, init: u8 },
    /// Latch (reset 0) that ignores writes with any `veto` bit set; those writes address another function.
    Gated { mask: u8, veto: u8 },
    /// Write ORs `val & mask` into the latch at offset `cell`. Reads return `read`, or that latch if `None`.
    Set { cell: u32, mask: u8, read: Option<u8> },
    /// Write clears `val & mask` in the latch at offset `cell`. Reads as for `Set`.
    Clear { cell: u32, mask: u8, read: Option<u8> },
}

/// Plain read/write memory.
pub(crate) const RAM: Reg = Reg::Latch { mask: 0xFF, init: 0 };

/// `reg` applies to window offsets `start..end`. The first matching span wins; other offsets are RAZ/WI.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    start: u32,
    end: u32,
    reg: Reg,
}

/// A single offset.
pub(crate) const fn at(off: u32, reg: Reg) -> Span {
    Span { start: off, end: off + 1, reg }
}

/// Offsets `start..end`.
pub(crate) const fn span(start: u32, end: u32, reg: Reg) -> Span {
    Span { start, end, reg }
}

/// One 256-byte page of read/write memory.
pub(crate) const RAM_PAGE: &[Span] = &[span(0, IO_GRAIN, RAM)];

/// A register window described by a static span table. Reads have no side effects.
pub(crate) struct RegTable {
    name: &'static str,
    spans: &'static [Span],
    /// Storage for latches, indexed by window offset; only as long as the highest latch needs.
    mem: Vec<u8>,
}

impl RegTable {
    pub(crate) fn new(name: &'static str, spans: &'static [Span]) -> Self {
        let len = spans
            .iter()
            .map(|s| match s.reg {
                Reg::Set { cell, .. } | Reg::Clear { cell, .. } => s.end.max(cell + 1),
                _ => s.end,
            })
            .max()
            .unwrap_or(0);
        let mut table = RegTable { name, spans, mem: vec![0; len as usize] };
        table.reset_mem();
        table
    }

    fn reg(&self, off: u32) -> Reg {
        self.spans.iter().find(|s| (s.start..s.end).contains(&off)).map_or(Reg::Raz, |s| s.reg)
    }

    pub(crate) fn get(&self, off: u32) -> u8 {
        match self.reg(off) {
            Reg::Raz => 0,
            Reg::Const(v) => v,
            Reg::Latch { .. } | Reg::Gated { .. } => self.mem[off as usize],
            Reg::Set { cell, read, .. } | Reg::Clear { cell, read, .. } => read.unwrap_or(self.mem[cell as usize]),
        }
    }

    pub(crate) fn set(&mut self, off: u32, val: u8) {
        match self.reg(off) {
            Reg::Raz | Reg::Const(_) => {}
            Reg::Latch { mask, .. } => self.mem[off as usize] = val & mask,
            Reg::Gated { mask, veto } => {
                if val & veto == 0 {
                    self.mem[off as usize] = val & mask;
                }
            }
            Reg::Set { cell, mask, .. } => self.mem[cell as usize] |= val & mask,
            Reg::Clear { cell, mask, .. } => self.mem[cell as usize] &= !(val & mask),
        }
    }

    fn reset_mem(&mut self) {
        self.mem.fill(0);
        for s in self.spans {
            if let Reg::Latch { init, .. } = s.reg {
                self.mem[s.start as usize..s.end as usize].fill(init);
            }
        }
    }
}

impl IoDevice for RegTable {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.get(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        self.set(off, val);
    }

    fn peek8(&self, off: u32) -> u8 {
        self.get(off)
    }

    /// Power-on state: latches back to their `init` values, memory cleared.
    fn reset(&mut self) {
        self.reset_mem();
    }

    crate::impl_as_any!();
}

/// Map a table-driven window of `size` bytes at `base`.
pub(crate) fn add_table(map: &mut IoMap, base: u32, size: u32, name: &'static str, spans: &'static [Span]) {
    debug_assert!(
        spans.iter().all(|s| s.start < s.end && s.end <= size),
        "{name}: span outside its {size:#x}-byte window"
    );
    map.add(base, size, Box::new(RegTable::new(name, spans)));
}

/// U2PIO_BOARDREV: board revision 0x17 "U64E V2.2 (Mass Prod)" in bits 7:3 (product.cc:44,57-62).
const BOARDREV: u8 = 0x17 << 3;

/// Which label the same board wears. `BOARDREV` does not tell the two apart -- 0x16 and 0x17 are one piece of
/// hardware that Commodore's branch calls a C64U and Gideon's calls an Ultimate 64-II Elite, and a C64U running
/// 3.15 writes "ULTIMATE 64-II" in its own title bar. What differs is the Bling Board, the LED and keyboard
/// controller the C64U carries: `BLINGBOARD_INSTALLED` is `BLING_RX_FLAGS & 0x04` (u64.h:35-43), and a firmware
/// that wants to know asks that, not the revision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Board {
    /// No Bling Board. 3.15 reads the flag in one place, `LedStrip::setup_config_menu` (led_strip.cc:497).
    #[default]
    U64II,
    /// The Bling Board answers present. Its data, get and irqen registers stay unmodelled: 3.15 never reads them,
    /// and the branches that do (`bling_board.cc`, `u64_config.cc`) are not in this firmware.
    C64U,
}

/// `BLING_RX_FLAGS` with the presence bit set.
const BLING_PRESENT: &[Span] = &[at(0x02, Reg::Const(0x04))];

/// U2PIO_GET_MDIO / U2PIO_SET_MDC / U2PIO_SET_MDIO (u2p.h:66-68).
const GET_MDIO: u32 = 0x06;
const SET_MDC: u32 = 0x0A;
const SET_MDIO: u32 = 0x0B;

/// U2P misc GPIO page 0x10100000 without the MDIO pins (docs/hw/03-board-init.md §U2P misc GPIO page; u2p.h:66-73,
/// u2p_io.vhd).
const U2PIO: &[Span] = &[
    // 00 §1c M1, 03 H6/H18: BOARDREV is a constant. SPEAKER_EN shares the address and the U64-II build writes
    // 0xFF there (u64_config.cc:1082-1084); a RAM model would read 0x1F and make isEliteBoard false.
    // 00 §2 C11: rev >> 3 = 0x17 is Elite and is not the PLL-channel-swap rev 0x15 (u64ii_init.cc:157,166).
    at(0x0C, Reg::Const(BOARDREV)),
    // U2PIO_HUB_RESET / U2PIO_ULPI_RESET: bit0 latches, read back as bit0 (u2p_io.vhd, reset 0).
    // 00 §2 B5: usb_hwinit.cc:85-91 reads 0x1010000D 50 times as a delay.
    at(0x0D, Reg::Latch { mask: 0x01, init: 0 }),
    at(0x0F, Reg::Latch { mask: 0x01, init: 0 }),
];

/// The U2PIO page: the static table plus the Ethernet PHY on the bit-banged MDIO pins (08 §MDIO, 00 §2 C24 T1).
/// The pins share a 256-byte grain with the rest of the page, so the PHY cannot be a window of its own.
pub struct U2pio {
    table: RegTable,
    pub phy: Phy,
}

impl IoDevice for U2pio {
    fn name(&self) -> &'static str {
        "u2pio"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    /// The firmware writes 0 or 1 to the pin registers (mdio.c:13-16); bit 0 is the level.
    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        match off {
            SET_MDC => self.phy.set_mdc(val & 1 != 0),
            SET_MDIO => self.phy.set_mdio(val & 1 != 0),
            _ => self.table.set(off, val),
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        match off {
            // 08 H2-H4: the firmware tests `!= 0` (mdio.c:109).
            GET_MDIO => u8::from(self.phy.mdio()),
            _ => self.table.get(off),
        }
    }

    fn reset(&mut self) {
        self.table.reset();
        self.phy.reset();
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    map.add(0x1010_0000, 0x100, Box::new(U2pio { table: RegTable::new("u2pio", U2PIO), phy: Phy::new() }));
    // DDR2 PHY: boot ROM only (00 §1b).
    add_table(map, 0x1010_0100, 0x100, "ddr2-phy", &[]);
    // U64_CLOCKMEAS: no compiled user (u64.h:14,87).
    add_table(map, 0x1010_0200, 0x100, "clockmeas", &[]);
    // MATRIX_KEYB 0x10100300 feeds the C64 keyboard: devices::c64 (docs/specs/S14-c64-trx64.md §5.6).
    // Audio mixer 0x500, speaker mixer 0x540, resampler 0x580: devices::c64, which hands the audio mixer to the backend
    // (docs/specs/S17-ultisid.md §2.5).
    // LED strip data/map/intensity/start (led_strip.cc:150-154): write-only.
    add_table(map, 0x1010_0600, 0x100, "led-strip", &[]);
    // Blingboard RX: BLING_RX_FLAGS 0x10100802 reads 0 = not installed (led_strip.cc:497-501), or 0x04 for a
    // C64U, which is the one thing that tells the two labels of this board apart ([`Board`]).
    let bling = match cfg.board {
        Board::U64II => &[][..],
        Board::C64U => BLING_PRESENT,
    };
    add_table(map, 0x1010_0800, 0x100, "blingboard", bling);
    // U64II_BLINGBOARD_LEDS: defined (u64.h:36), never accessed.
    add_table(map, 0x1010_0900, 0x100, "blingboard-leds", &[]);
    // MMCM DRP words at 2·idx and MMCM_RESET 0x102000FF (u64ii_init.cc:213-228,266-267): write-only, no lock poll.
    add_table(map, 0x1020_0000, 0x100, "mmcm", &[]);
}

/// Local stand-in for the `SystemBus` IO decode, so device tests do not depend on S02.
#[cfg(test)]
pub(crate) mod rig {
    use std::path::PathBuf;

    use crate::io::{IoCtx, IoMap};
    use crate::irq::IrqState;
    use crate::machine::MachineConfig;

    pub(crate) fn cfg() -> MachineConfig {
        MachineConfig::new(PathBuf::new(), PathBuf::new())
    }

    /// Resolves through `IoMap`, splits 16/32-bit accesses into LE bytes at addr+0..+3 (00 §1 bus rules),
    /// reads 0 when unmapped.
    pub(crate) struct Rig {
        pub(crate) map: IoMap,
        pub(crate) irq: IrqState,
        ram: Vec<u8>,
        console: Vec<u8>,
    }

    impl Rig {
        pub(crate) fn new(install: fn(&mut IoMap, &MachineConfig)) -> Self {
            Self::with_cfg(install, &cfg())
        }

        /// The same rig for a machine configured differently, e.g. another board label.
        pub(crate) fn with_cfg(install: fn(&mut IoMap, &MachineConfig), cfg: &MachineConfig) -> Self {
            let mut map = IoMap::new();
            install(&mut map, cfg);
            Rig { map, irq: IrqState::new(), ram: vec![0; 16], console: Vec::new() }
        }

        pub(crate) fn r8(&mut self, addr: u32) -> u8 {
            let Some((dev, off)) = self.map.resolve(addr) else { return 0 };
            let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].read8(off, &mut ctx)
        }

        pub(crate) fn w8(&mut self, addr: u32, val: u8) {
            let Some((dev, off)) = self.map.resolve(addr) else { return };
            let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            self.map.devices[dev].write8(off, val, &mut ctx);
        }

        pub(crate) fn w16(&mut self, addr: u32, val: u16) {
            for i in 0..2 {
                self.w8(addr + i, (val >> (8 * i)) as u8);
            }
        }

        pub(crate) fn r32(&mut self, addr: u32) -> u32 {
            (0..4).fold(0, |acc, i| acc | u32::from(self.r8(addr + i)) << (8 * i))
        }

        pub(crate) fn w32(&mut self, addr: u32, val: u32) {
            for i in 0..4 {
                self.w8(addr + i, (val >> (8 * i)) as u8);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rig::{cfg, Rig};
    use crate::machine::MachineConfig;
    use super::*;
    use crate::devices::{c64, drives, i2c, iec, install_all, misc, rmii, usb};

    #[test]
    fn m1_boardrev_independent() {
        let mut rig = Rig::new(install);
        assert_eq!(rig.r8(0x1010_000C), 0xB8);
        // effectuate_settings writes the speaker enable (u64_config.cc:1082-1084).
        rig.w8(0x1010_000C, 0xFF);
        assert_eq!(rig.r8(0x1010_000C), 0xB8);
        assert_eq!(rig.r8(0x1010_000C) >> 3, 0x17);
        assert_eq!(rig.r8(0x1010_0006), 1, "GET_MDIO: line released, no PHY frame in progress");
    }

    /// The Bling Board's presence bit is the only thing that separates a C64U from an Elite: the revision is 0x17
    /// either way (product.cc:44,57-62; u64.h:35-43). `--board c64u` sets it, and nothing else moves.
    #[test]
    fn the_bling_board_answers_only_for_a_c64u() {
        let mut elite = Rig::new(install);
        assert_eq!(elite.r8(0x1010_0802), 0, "BLING_RX_FLAGS: no Bling Board");

        let mut c64u = Rig::with_cfg(install, &MachineConfig { board: Board::C64U, ..cfg() });
        assert_eq!(c64u.r8(0x1010_0802) & 0x04, 0x04, "BLINGBOARD_INSTALLED");
        assert_eq!(c64u.r8(0x1010_000C) >> 3, 0x17, "the revision is the same board");
        c64u.w8(0x1010_0802, 0);
        assert_eq!(c64u.r8(0x1010_0802) & 0x04, 0x04, "a constant, not a latch");
    }

    #[test]
    fn b5_usb_reset_latches() {
        let mut rig = Rig::new(install);
        rig.w8(0x1010_000D, 1);
        assert!((0..50).all(|_| rig.r8(0x1010_000D) == 1));
        rig.w8(0x1010_000D, 0);
        assert_eq!(rig.r8(0x1010_000D), 0);
        rig.w8(0x1010_000F, 0x80);
        assert_eq!(rig.r8(0x1010_000F), 0);
        rig.w8(0x1010_000F, 0x01);
        assert_eq!(rig.r8(0x1010_000F), 1);
    }

    /// MATRIX_KEYB and its 32-bit MATRIX_WASD_TO_JOY store moved to devices::c64 (c19_matrix_wasd_32bit_store).
    #[test]
    fn write_only_sinks_read_zero() {
        let mut rig = Rig::new(install);
        rig.w8(0x1010_0600, 0x55);
        rig.w8(0x1020_00FF, 0xB3);
        assert_eq!(rig.r8(0x1010_0600), 0);
        assert_eq!(rig.r8(0x1020_00FF), 0);
        assert_eq!(rig.r8(0x1010_0802), 0, "Blingboard not installed");
    }

    #[test]
    fn reg_table_ops() {
        const T: &[Span] = &[
            at(0, Reg::Const(0x5A)),
            at(1, Reg::Latch { mask: 0x0F, init: 0x03 }),
            at(2, Reg::Gated { mask: 0x01, veto: 0x80 }),
            at(3, Reg::Set { cell: 1, mask: 0x0F, read: Some(0x11) }),
            at(4, Reg::Clear { cell: 1, mask: 0x0F, read: None }),
        ];
        let mut t = RegTable::new("t", T);
        t.set(0, 0);
        assert_eq!(t.get(0), 0x5A);
        assert_eq!(t.get(1), 0x03);
        t.set(1, 0xF4);
        assert_eq!(t.get(1), 0x04);
        t.set(2, 0x81);
        assert_eq!(t.get(2), 0);
        t.set(2, 0x01);
        assert_eq!(t.get(2), 1);
        t.set(3, 0xF1);
        assert_eq!((t.get(3), t.get(1)), (0x11, 0x05));
        t.set(4, 0x04);
        assert_eq!(t.get(4), 0x01);
        assert_eq!(t.get(99), 0, "unlisted offsets are RAZ");
        t.reset();
        assert_eq!((t.get(1), t.get(2)), (0x03, 0));
    }

    /// 00 §1b windows owned by S03 (ITU), S05 (WiFi), S06 (flash), S07 (U64 IO, overlay) and S09 (SD).
    const FOREIGN: &[(u32, u32)] = &[
        (0x1000_0000, 0x100),
        (0x1006_0000, 0x100),
        (0x1006_0200, 0x100),
        (0x1006_0900, 0x100),
        (0x1010_0400, 0x100),
        (0x1014_0000, 0x1_0000),
    ];

    #[test]
    fn s04_leaves_foreign_windows_free() {
        let mut map = IoMap::new();
        let installs: [fn(&mut IoMap, &MachineConfig); 8] =
            [install, i2c::install, c64::install, usb::install, drives::install, iec::install, misc::install, rmii::install];
        for install in installs {
            install(&mut map, &cfg());
        }
        for &(base, size) in FOREIGN {
            for addr in (base..base + size).step_by(IO_GRAIN as usize) {
                assert!(map.resolve(addr).is_none(), "{addr:#010x} is mapped by S04");
            }
        }
    }

    #[test]
    fn install_all_no_overlap() {
        let mut map = IoMap::new();
        install_all(&mut map, &cfg());
        for addr in [0x1002_1806, 0x1002_8000, 0x1004_0001, 0x1005_D012, 0x1006_0400, 0x1006_082F, 0x1008_07F2, 0x1010_000C, 0x1010_0701, 0x1018_A000] {
            assert!(map.resolve(addr).is_some(), "{addr:#010x} unmapped");
        }
    }
}
