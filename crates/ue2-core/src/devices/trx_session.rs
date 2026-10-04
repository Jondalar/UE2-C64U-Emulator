//! S37 phase B: TRX64-Ultimate's freeze session (`--trx-io`), the part of `freeze_resp.v` that answers the firmware's
//! DMA window while the machine is paused. Spec: docs/specs/S37-trx-pixel-layer.md §5.4, §5.8.
//!
//! A session starts when the app pauses the machine with C64_STOP under `GATE_CTRL` FRZ and C64_MODE Ultimax
//! (`SH_MODE` 1) or with any pause (`SH_MODE` 2), and runs until that falls. Inside it no window access becomes a bus
//! cycle: RAM and ROM are read and written as they are, the I/O chips are answered from a 128-byte overlay where the
//! app wrote them and from the frozen chip (without side effects) where it did not, and the I/O writes go into a commit
//! list that runs as real bus cycles when the session ends (unless `PROTO` kept them out). A reset in a session writes
//! the overlay's VIC registers for real and ends it.
//!
//! The device answers in the C64's clock with a state machine; UE2 answers each access at once, so `SH_STATUS` b1 (a
//! commit runs) always reads 0.

use crate::c64host::{C64Backend, C64Rom, CartPeek};

/// What the session needs of the C64. Every [`C64Backend`] has it.
pub trait Chips {
    /// C64 RAM itself (the RAM's second port).
    fn ram_read(&self, addr: u16) -> u8;
    fn ram_write(&mut self, addr: u16, val: u8);
    fn rom_read(&self, rom: C64Rom, off: u16) -> u8;
    /// `$D000-$DFFF` as a read shows it, without side effects (c64_core.v's state bus).
    fn io_peek(&self, addr: u16) -> u8;
    fn cia_regs(&self, cia: u8) -> [u8; 16];
    fn cia1_ports(&self, pra: u8, ddra: u8, prb: u8, ddrb: u8) -> (u8, u8);
    fn cart_peek(&self, addr: u16) -> CartPeek;
    fn cpu_port(&self) -> (u8, u8);
    fn cart_lines(&self) -> (bool, bool, bool);
    /// A real bus cycle (SIDEFX).
    fn bus_read(&mut self, addr: u16) -> u8;
    fn bus_write(&mut self, addr: u16, val: u8);
    /// Real bus writes with I/O forced (the commit; also the colour RAM's second port).
    fn io_commit(&mut self, writes: &[(u16, u8)]);
}

impl<T: C64Backend + ?Sized> Chips for T {
    fn ram_read(&self, addr: u16) -> u8 {
        self.ram_peek(addr)
    }
    fn ram_write(&mut self, addr: u16, val: u8) {
        self.dma_write(addr, val, true);
    }
    fn rom_read(&self, rom: C64Rom, off: u16) -> u8 {
        C64Backend::rom_read(self, rom, off)
    }
    fn io_peek(&self, addr: u16) -> u8 {
        C64Backend::io_peek(self, addr)
    }
    fn cia_regs(&self, cia: u8) -> [u8; 16] {
        C64Backend::cia_regs(self, cia)
    }
    fn cia1_ports(&self, pra: u8, ddra: u8, prb: u8, ddrb: u8) -> (u8, u8) {
        C64Backend::cia1_ports(self, pra, ddra, prb, ddrb)
    }
    fn cart_peek(&self, addr: u16) -> CartPeek {
        C64Backend::cart_peek(self, addr)
    }
    fn cpu_port(&self) -> (u8, u8) {
        C64Backend::cpu_port(self)
    }
    fn cart_lines(&self) -> (bool, bool, bool) {
        C64Backend::cart_lines(self)
    }
    fn bus_read(&mut self, addr: u16) -> u8 {
        self.dma_read(addr, false)
    }
    fn bus_write(&mut self, addr: u16, val: u8) {
        self.dma_write(addr, val, false);
    }
    fn io_commit(&mut self, writes: &[(u16, u8)]) {
        C64Backend::io_commit(self, writes);
    }
}

/// The lines the session follows (freeze_resp.v `cond`, `keep`, `start`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    /// `GATE_CTRL` b0: C64_STOP pauses the whole machine (`stop_is_pause`).
    pub frz: bool,
    /// C64_STOP b0.
    pub stop: bool,
    /// C64_MODE b1.
    pub ultimax: bool,
    /// C64_MODE b2, the reset held.
    pub reset: bool,
}

impl Lines {
    /// The machine pause (top.v `c64_halt_req`). In UE2 a held reset wins: the machine is in reset, not paused.
    pub fn paused(&self) -> bool {
        self.frz && self.stop && !self.reset
    }
}

/// `SH_CTRL` b2, b3, b8, b9.
pub const SH_SIDEFX: u32 = 1 << 2;
pub const SH_PROTO: u32 = 1 << 3;
pub const SH_ABORT: u32 = 1 << 8;
pub const SH_DISCARD: u32 = 1 << 9;

/// Commit list entries (freeze_resp.v `cl`, 128 deep).
const LIST_MAX: usize = 128;
/// Overlay indices: VIC 0-63, CIA 1 64-79, CIA 2 80-95, SID 96-127.
const OV_CIA: usize = 0x40;
const OV_SID: usize = 0x60;
const OV_D011: usize = 0x11;
const OV_D016: usize = 0x16;
const OV_D018: usize = 0x18;
const OV_D019: usize = 0x19;
const OV_D01A: usize = 0x1A;
const OV_D021: usize = 0x21;
const OV_CIA2_PRA: usize = 0x50;
const OV_CIA2_DDRA: usize = 0x52;

/// What the PLA maps an address to (freeze_resp.v `C_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Ram,
    Kernal,
    Basic,
    Char,
    RomH,
    RomL,
    Io,
    None,
}

/// c64_bus_arbiter.v's PLA with BA high (freeze_resp.v `pla`): `m` = {GAME, EXROM, CHAREN, HIRAM, LORAM}, `n` the
/// address's top nibble.
fn pla(m: u8, n: u8, we: bool) -> Class {
    use Class::*;
    let (game, exrom) = (m & 0x10 != 0, m & 0x08 != 0);
    let ef = n == 0xE || n == 0xF;
    let ab = n == 0xA || n == 0xB;
    let roml = (n == 0x8 || n == 0x9) && !exrom;
    let ab_rom = if game { Basic } else { RomH };
    let like = |m: u8, pat: u8, mask: u8| m & mask == pat;
    if !game && exrom {
        // 01zzz, Ultimax
        return match n {
            0xE | 0xF => RomH,
            0xD => Io,
            0x8 | 0x9 => RomL,
            0x0 => Ram,
            _ => None,
        };
    }
    let io_w = if we && n == 0xD { Io } else { Ram };
    match m {
        0b10111 | 0b11111 | 0b00111 if we => io_w,
        0b10111 | 0b11111 | 0b00111 => match n {
            _ if ef => Kernal,
            0xD => Io,
            _ if ab => ab_rom,
            _ if roml => RomL,
            _ => Ram,
        },
        0b10011 | 0b11011 | 0b00011 if we => Ram,
        0b10011 | 0b11011 | 0b00011 => match n {
            _ if ef => Kernal,
            0xD => Char,
            _ if ab => ab_rom,
            _ if roml => RomL,
            _ => Ram,
        },
        0b00110 if we => io_w,
        0b00110 => match n {
            _ if ef => Kernal,
            0xD => Io,
            _ if ab => RomH,
            _ => Ram,
        },
        0b00010 if we => Ram,
        0b00010 => match n {
            _ if ef => Kernal,
            0xD => Char,
            _ if ab => RomH,
            _ => Ram,
        },
        _ if like(m, 0b10101, 0b10111) || m == 0b00101 => {
            if n == 0xD {
                Io
            } else {
                Ram
            }
        }
        _ if like(m, 0b10001, 0b10111) => {
            if !we && n == 0xD {
                Char
            } else {
                Ram
            }
        }
        _ if like(m, 0b10110, 0b10111) => match n {
            0xD if we => Io,
            _ if we => Ram,
            _ if ef => Kernal,
            0xD => Io,
            _ => Ram,
        },
        _ if like(m, 0b10010, 0b10111) => match n {
            _ if we => Ram,
            _ if ef => Kernal,
            0xD => Char,
            _ => Ram,
        },
        // 1zz00, 00z00, 00001: the rest is RAM.
        _ => Ram,
    }
}

/// VIC registers whose unused bits read 1 (freeze_resp.v `vic_mask`).
fn vic_mask(i: usize) -> u8 {
    match i {
        0x16 => 0xC0,
        0x18 => 0x01,
        0x1A => 0xF0,
        0x20..=0x2E => 0xF0,
        _ => 0,
    }
}

/// VIC registers the commit list takes once and writes with their last value (`vic_plain`).
fn vic_plain(i: usize) -> bool {
    i <= 0x10 || (0x15..=0x18).contains(&i) || (0x1A..=0x1D).contains(&i) || (0x20..=0x2E).contains(&i)
}

/// VIC registers a write does nothing to (`vic_none`).
fn vic_none(i: usize) -> bool {
    matches!(i, 0x13 | 0x14 | 0x1E | 0x1F) || i >= 0x2F
}

/// The bits of the session's VIC writes the text layer draws with (freeze_resp.v `vic_view`); a register not written
/// in this session counts as 0, CIA 2 PRA as %11 and DDRA as %00.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VicView {
    /// `$D011` b6 and b5.
    pub ecm: bool,
    pub bmm: bool,
    /// `$D016` b4.
    pub mcm: bool,
    /// `$D018` b7:1 (b0 0).
    pub d018: u8,
    /// `$D021` b3:0.
    pub d021: u8,
    /// 3 − (PRA | ¬DDRA) b1:0.
    pub bank: u8,
}

impl VicView {
    /// Standard text mode: what c64_text.v draws.
    pub fn text(&self) -> bool {
        !self.ecm && !self.bmm && !self.mcm
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    plain: bool,
    /// `$Dxxx` bits 11:0.
    addr: u16,
    data: u8,
}

/// freeze_resp.v's session state and trx_sys.v's page 5 registers.
#[derive(Clone, Debug)]
pub struct Session {
    /// `SH_CTRL` b1:0, b2, b3.
    pub mode: u8,
    pub sidefx: bool,
    pub proto: bool,
    /// A session runs (`SH_STATUS` b0).
    pub active: bool,
    /// No new session until C64_STOP falls (after an abort, a commit or a reset).
    block: bool,
    /// `SH_STATUS` b31:16, `SH_ACCESSES`, `SH_REAL`, `SH_UNMAPPED`. `SH_GCLK` (gate clock edges while paused) is not
    /// counted: UE2 has no gated clock.
    pub count: u16,
    pub accesses: u32,
    pub real: u32,
    pub unmapped: u32,
    /// `SH_LIST` b31:16 (wrapping), `SH_COMMIT`.
    pub dropped: u16,
    pub last_commit: u16,
    overlay: [u8; 128],
    valid: u128,
    list: Vec<Entry>,
    /// Plain registers already listed (`mk`, VIC 0-63 and CIA 64-95).
    listed: u128,
    /// `$D019` flags the list acknowledges.
    ack19: u8,
    /// The CPU port and the cartridge's lines at the start.
    port: (u8, u8),
    cart: (bool, bool, bool),
}

impl Default for Session {
    fn default() -> Self {
        Session {
            mode: 1,
            sidefx: false,
            proto: false,
            active: false,
            block: false,
            count: 0,
            accesses: 0,
            real: 0,
            unmapped: 0,
            dropped: 0,
            last_commit: 0,
            overlay: [0; 128],
            valid: 0,
            list: Vec::new(),
            listed: 0,
            ack19: 0,
            port: (0x2F, 0x37),
            cart: (false, true, true),
        }
    }
}

impl Session {
    /// `SH_CTRL` as read: {b3 PROTO, b2 SIDEFX, b1:0 mode} (trx_sys.v:229).
    pub fn ctrl(&self) -> u32 {
        u32::from(self.proto) << 3 | u32::from(self.sidefx) << 2 | u32::from(self.mode)
    }

    /// `SH_STATUS`: b0 session, b1 a commit runs (never, in UE2), b31:16 sessions started.
    pub fn status(&self) -> u32 {
        u32::from(self.count) << 16 | u32::from(self.active)
    }

    /// `SH_LIST`: b7:0 entries now, b31:16 entries a full list dropped.
    pub fn list_reg(&self) -> u32 {
        u32::from(self.dropped) << 16 | self.list.len() as u32
    }

    /// Overlay byte `i` (0-127), as `SH_CTRL` 0x080-0x0FF reads it.
    pub fn overlay(&self, i: usize) -> u8 {
        self.overlay[i & 0x7F]
    }

    /// Valid bits 32 `word` .. +31 (0x100-0x10F).
    pub fn valid_word(&self, word: usize) -> u32 {
        (self.valid >> (32 * (word & 3))) as u32
    }

    fn ov(&self, i: usize) -> Option<u8> {
        (self.valid >> i & 1 != 0).then_some(self.overlay[i])
    }

    fn put(&mut self, i: usize, val: u8) {
        self.overlay[i] = val;
        self.valid |= 1 << i;
    }

    /// The text layer's view (freeze_resp.v:456-474).
    pub fn vic_view(&self) -> VicView {
        let d011 = self.ov(OV_D011).unwrap_or(0);
        let pra = self.ov(OV_CIA2_PRA).map_or(3, |v| v & 3);
        let ddra = self.ov(OV_CIA2_DDRA).map_or(0, |v| v & 3);
        VicView {
            ecm: d011 & 0x40 != 0,
            bmm: d011 & 0x20 != 0,
            mcm: self.ov(OV_D016).unwrap_or(0) & 0x10 != 0,
            d018: self.ov(OV_D018).unwrap_or(0) & 0xFE,
            d021: self.ov(OV_D021).unwrap_or(0) & 0x0F,
            bank: !(pra | !ddra) & 3,
        }
    }

    /// `cond` and `keep` (freeze_resp.v:194-197).
    fn cond(&self, l: Lines) -> bool {
        match self.mode {
            1 => l.frz && l.stop && l.ultimax,
            2 => l.frz && l.stop,
            _ => false,
        }
    }

    fn keep(&self, l: Lines) -> bool {
        match self.mode {
            1 | 2 => l.frz && l.stop,
            _ => false,
        }
    }

    /// After any change of the lines: a session whose condition fell ends (its list runs), the block clears while
    /// C64_STOP is off, and a session starts where the condition holds.
    pub fn step<C: Chips + ?Sized>(&mut self, l: Lines, c: &mut C) {
        if self.active && !self.keep(l) {
            let drop = self.mode == 0;
            self.end(drop, c);
        }
        if !l.stop {
            self.block = false;
        }
        if !self.active && !self.block && self.cond(l) && !l.reset {
            self.active = true;
            self.ack19 = 0;
            self.count = self.count.wrapping_add(1);
            // S_INIT: the frozen CPU port and cartridge lines, once per session.
            self.port = c.cpu_port();
            self.cart = c.cart_lines();
        }
    }

    /// `SH_CTRL` written (trx_sys.v:335-341). An abort ends the session without its list and blocks; mode 0 ends it
    /// without its list; discard drops the list and the session goes on. Neither needs the C64.
    pub fn write_ctrl(&mut self, v: u32) {
        self.mode = (v & 3) as u8;
        self.sidefx = v & SH_SIDEFX != 0;
        self.proto = v & SH_PROTO != 0;
        if v & SH_DISCARD != 0 {
            self.list.clear();
            self.listed = 0;
            self.ack19 = 0;
        }
        if self.active && (v & SH_ABORT != 0 || self.mode == 0) {
            self.close();
            self.drop_list();
            if v & SH_ABORT != 0 {
                self.block = true;
            }
        }
    }

    /// The session's end bookkeeping (`ending`, `sess_end`: PROTO clears).
    fn close(&mut self) {
        self.active = false;
        self.ack19 = 0;
        self.proto = false;
    }

    fn drop_list(&mut self) {
        self.valid = 0;
        self.list.clear();
        self.listed = 0;
        self.last_commit = 0;
    }

    /// The end of a session: the list runs as real bus cycles unless `drop` (S_RC0-S_RCW), then the overlay goes.
    fn end<C: Chips + ?Sized>(&mut self, drop: bool, c: &mut C) {
        self.close();
        if drop || self.list.is_empty() {
            self.drop_list();
            return;
        }
        self.block = true;
        let mut written = 0u16;
        for e in std::mem::take(&mut self.list) {
            let a = e.addr;
            let cia = (0xC00..0xE00).contains(&a);
            let vic = a < 0x400;
            let idx = if cia { OV_CIA | usize::from(a >> 8 & 1) << 4 | usize::from(a & 0xF) } else { usize::from(a & 0x3F) };
            // A plain or CIA entry reads its own overlay byte, the others $D011's (for its bits 6-0).
            let ov = self.overlay[if e.plain || cia { idx } else { OV_D011 }];
            let last = if cia && a & 0xE == 0xE { ov & 0xEF } else { ov };
            let addr = 0xD000 | a;
            let write = if e.plain {
                let chip = if cia {
                    let reg = usize::from(a & 0xF);
                    if reg < 4 {
                        c.cia_regs(u8::from(a >> 8 & 1 != 0))[reg]
                    } else {
                        c.io_peek(addr)
                    }
                } else {
                    c.io_peek(0xD000 | idx as u16)
                };
                let differs = if cia { last != chip } else { ov | vic_mask(idx) != chip };
                differs.then_some(last)
            } else if vic && a & 0x3F == 0x11 {
                Some(e.data & 0x80 | ov & 0x7F)
            } else {
                Some(e.data)
            };
            if let Some(val) = write {
                c.io_commit(&[(addr, val)]);
                written = written.wrapping_add(1);
            }
        }
        self.real = self.real.wrapping_add(u32::from(written));
        self.last_commit = written;
        self.valid = 0;
        self.listed = 0;
    }

    /// A reset while a session runs (S_COMMIT): the overlay's VIC registers but $13, $14, $1E, $1F are written for real,
    /// the rest of the list is dropped, the session ends and blocks. The reset goes on after this.
    pub fn reset<C: Chips + ?Sized>(&mut self, c: &mut C) {
        if !self.active {
            return;
        }
        let writes: Vec<(u16, u8)> = (0..0x2F)
            .filter(|&i| !matches!(i, 0x13 | 0x14 | 0x1E | 0x1F))
            .filter_map(|i| self.ov(i).map(|v| (0xD000 | i as u16, v)))
            .collect();
        c.io_commit(&writes);
        self.real = self.real.wrapping_add(writes.len() as u32);
        self.close();
        self.block = true;
        self.valid = 0;
        self.list.clear();
        self.listed = 0;
    }

    /// The decode with the frozen CPU port and cartridge lines, Ultimax from the live C64_MODE (freeze_resp.v:262-316).
    fn class(&self, addr: u16, we: bool, ultimax: bool) -> Class {
        let (ddr, pout) = self.port;
        let lhc = (pout & ddr | !ddr) & 7;
        let (present, exrom, game) = self.cart;
        let present = present || ultimax;
        let game = game && !ultimax || !present;
        let exrom = exrom || ultimax || !present;
        let m = u8::from(game) << 4 | u8::from(exrom) << 3 | lhc;
        pla(m, (addr >> 12) as u8, we)
    }

    /// SIDEFX: I/O but colour RAM, ROML, ROMH and open ranges are real bus cycles.
    fn real_cycle(&self, cls: Class, io: u16) -> bool {
        self.sidefx && match cls {
            Class::Io => !(0x8..0xC).contains(&io),
            Class::RomL | Class::RomH | Class::None => true,
            _ => false,
        }
    }

    fn cart_read<C: Chips + ?Sized>(&mut self, addr: u16, c: &C) -> u8 {
        match c.cart_peek(addr) {
            CartPeek::Hit(v) => v,
            CartPeek::Miss | CartPeek::External => {
                self.unmapped = self.unmapped.wrapping_add(1);
                0xFF
            }
        }
    }

    /// A window read in a session.
    pub fn read<C: Chips + ?Sized>(&mut self, addr: u16, l: Lines, c: &mut C) -> u8 {
        self.accesses = self.accesses.wrapping_add(1);
        let cls = self.class(addr, false, l.ultimax);
        let io = addr >> 8 & 0xF;
        if self.real_cycle(cls, io) {
            self.real = self.real.wrapping_add(1);
            return c.bus_read(addr);
        }
        match cls {
            Class::Ram => c.ram_read(addr),
            Class::Kernal => c.rom_read(C64Rom::Kernal, addr & 0x1FFF),
            Class::Basic => c.rom_read(C64Rom::Basic, addr & 0x1FFF),
            Class::Char => c.rom_read(C64Rom::Char, addr & 0x0FFF),
            Class::Io => match io {
                0x0..=0x3 => self.vic_read(addr, c),
                // SID: writes held, no reads.
                0x4..=0x7 => 0xFF,
                0x8..=0xB => 0xF0 | c.io_peek(addr) & 0x0F,
                0xC | 0xD => self.cia_read(addr, c),
                _ => self.cart_read(addr, c),
            },
            Class::RomL | Class::RomH => self.cart_read(addr, c),
            Class::None => {
                self.unmapped = self.unmapped.wrapping_add(1);
                0xFF
            }
        }
    }

    /// A window write in a session.
    pub fn write<C: Chips + ?Sized>(&mut self, addr: u16, val: u8, l: Lines, c: &mut C) {
        self.accesses = self.accesses.wrapping_add(1);
        let cls = self.class(addr, true, l.ultimax);
        let io = addr >> 8 & 0xF;
        if self.real_cycle(cls, io) {
            self.real = self.real.wrapping_add(1);
            c.bus_write(addr, val);
            return;
        }
        match cls {
            Class::Ram => c.ram_write(addr, val),
            Class::Io => {
                self.list_write(addr, val);
                match io {
                    0x0..=0x3 => self.put(usize::from(addr & 0x3F), val),
                    0x4..=0x7 => self.put(OV_SID | usize::from(addr & 0x1F), val),
                    0x8..=0xB => c.io_commit(&[(addr, val)]),
                    0xC | 0xD => self.put(cia_index(addr), val),
                    // IO1, IO2: the list only.
                    _ => {}
                }
            }
            // ROML, ROMH, open Ultimax ranges; the ROMs never take a write (the PLA maps writes to RAM).
            Class::RomL | Class::RomH | Class::None => self.unmapped = self.unmapped.wrapping_add(1),
            Class::Kernal | Class::Basic | Class::Char => {}
        }
    }

    /// The commit list's part of an I/O write (freeze_resp.v:479-489, :610-618).
    fn list_write(&mut self, addr: u16, val: u8) {
        let io = addr >> 8 & 0xF;
        let (vic, sid, io12, cia) = (io < 4, (4..8).contains(&io), io >= 0xE, io == 0xC || io == 0xD);
        let reg = addr & 0xF;
        let vidx = usize::from(addr & 0x3F);
        let load = cia && reg >= 0xE && val & 0x10 != 0;
        let plain = (if vic { vic_plain(vidx) } else { cia && (reg < 4 || reg >= 0xE) }) && !load;
        let mki = if cia { cia_index(addr) } else { vidx };
        let listed = self.listed >> mki & 1 != 0;
        if self.proto || !(sid || io12 || (vic && !vic_none(vidx)) || cia) || (plain && listed) {
            return;
        }
        if self.list.len() >= LIST_MAX {
            self.dropped = self.dropped.wrapping_add(1);
        } else {
            self.list.push(Entry { plain, addr: addr & 0xFFF, data: val });
        }
        if plain {
            self.listed |= 1 << mki;
        }
        if vic && vidx == OV_D019 {
            self.ack19 |= val & 0x0F;
        }
    }

    /// A VIC register as a read shows it (freeze_resp.v `vic_rd`).
    fn vic_read<C: Chips + ?Sized>(&self, addr: u16, c: &C) -> u8 {
        let i = usize::from(addr & 0x3F);
        if i >= 0x2F {
            return 0xFF;
        }
        let chip = c.io_peek(0xD000 | i as u16);
        let own = self.ov(i);
        match i {
            0x11 => chip & 0x80 | own.unwrap_or(chip) & 0x7F,
            0x19 if self.ack19 != 0 => {
                let flags = chip & 0x0F & !self.ack19;
                let mask = self.ov(OV_D01A).unwrap_or_else(|| c.io_peek(0xD01A));
                u8::from(flags & mask & 0x0F != 0) << 7 | 0x70 | flags
            }
            0x12 | 0x13 | 0x14 | 0x19 | 0x1E | 0x1F => chip,
            _ => own.map_or(chip, |v| v | vic_mask(i)),
        }
    }

    /// A CIA register as a read shows it (freeze_resp.v `cia_rd`): the ports from the overlay's port registers (CIA 1
    /// with the keys and joysticks), timers, TOD and ICR from the frozen chip.
    fn cia_read<C: Chips + ?Sized>(&self, addr: u16, c: &C) -> u8 {
        let n = u8::from(addr >> 8 & 0xF == 0xD);
        let base = OV_CIA + 16 * usize::from(n);
        let regs = c.cia_regs(n);
        let reg = |r: usize| self.ov(base + r).unwrap_or(regs[r]);
        let (pra, prb, ddra, ddrb) = (reg(0), reg(1), reg(2), reg(3));
        let r = usize::from(addr & 0xF);
        let chip = c.io_peek(addr);
        match r {
            0 | 1 if n == 0 => {
                let (pa, pb) = c.cia1_ports(pra, ddra, prb, ddrb);
                if r == 0 {
                    pa
                } else {
                    pb
                }
            }
            0 => pra & ddra | chip & !ddra,
            1 => prb & ddrb | chip & !ddrb,
            2 => ddra,
            3 => ddrb,
            0xC => self.ov(base + r).unwrap_or(chip),
            0xE | 0xF => self.ov(base + r).map_or(chip, |v| v & 0xEF),
            _ => chip,
        }
    }
}

/// CIA 1 64-79, CIA 2 80-95.
fn cia_index(addr: u16) -> usize {
    OV_CIA | usize::from(addr >> 8 & 1) << 4 | usize::from(addr & 0xF)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frozen C64: RAM, a VIC register file, two CIAs and the writes that reached the chips.
    struct Fake {
        ram: Vec<u8>,
        io: [u8; 0x1000],
        cia: [[u8; 16]; 2],
        port: (u8, u8),
        cart: (bool, bool, bool),
        rom: Option<u8>,
        bus: Vec<(u16, u8)>,
        commits: Vec<(u16, u8)>,
    }

    impl Fake {
        fn new() -> Self {
            let mut io = [0; 0x1000];
            io[0x011] = 0x9B;
            io[0x012] = 0x37;
            io[0x016] = 0xC8;
            io[0x018] = 0x15;
            io[0x019] = 0x71;
            io[0x01A] = 0xF1;
            Fake {
                ram: vec![0; 0x10000],
                io,
                cia: [[0xFF, 0xFF, 0xFF, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x81, 0x11, 0x08], [0x97, 0xFF, 0x3F, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]],
                port: (0x2F, 0x37),
                cart: (false, true, true),
                rom: None,
                bus: Vec::new(),
                commits: Vec::new(),
            }
        }
    }

    impl Chips for Fake {
        fn ram_read(&self, addr: u16) -> u8 {
            self.ram[usize::from(addr)]
        }
        fn ram_write(&mut self, addr: u16, val: u8) {
            self.ram[usize::from(addr)] = val;
        }
        fn rom_read(&self, rom: C64Rom, off: u16) -> u8 {
            (rom as u8) << 6 | off as u8 & 0x3F
        }
        fn io_peek(&self, addr: u16) -> u8 {
            match addr {
                0xDC00..=0xDCFF => self.cia[0][usize::from(addr & 0xF)],
                0xDD00..=0xDDFF => self.cia[1][usize::from(addr & 0xF)],
                _ => self.io[usize::from(addr & 0xFFF)],
            }
        }
        fn cia_regs(&self, cia: u8) -> [u8; 16] {
            self.cia[usize::from(cia)]
        }
        /// Key A (row 1, column 2) held: PA0 driven low pulls PB... here simply PB2 low when PA1 is low.
        fn cia1_ports(&self, pra: u8, ddra: u8, prb: u8, ddrb: u8) -> (u8, u8) {
            let pa = pra | !ddra;
            let pb = (prb | !ddrb) & if pa & 0x02 == 0 { !0x04 } else { 0xFF };
            (pa, pb)
        }
        fn cart_peek(&self, _addr: u16) -> CartPeek {
            self.rom.map_or(CartPeek::Miss, CartPeek::Hit)
        }
        fn cpu_port(&self) -> (u8, u8) {
            self.port
        }
        fn cart_lines(&self) -> (bool, bool, bool) {
            self.cart
        }
        fn bus_read(&mut self, addr: u16) -> u8 {
            self.bus.push((addr, 0));
            0x5A
        }
        fn bus_write(&mut self, addr: u16, val: u8) {
            self.bus.push((addr, val));
        }
        fn io_commit(&mut self, writes: &[(u16, u8)]) {
            for &(addr, val) in writes {
                self.commits.push((addr, val));
                match addr {
                    0xDC00..=0xDCFF => self.cia[0][usize::from(addr & 0xF)] = val,
                    0xDD00..=0xDDFF => self.cia[1][usize::from(addr & 0xF)] = val,
                    _ => self.io[usize::from(addr & 0xFFF)] = val,
                }
            }
        }
    }

    const FROZEN: Lines = Lines { frz: true, stop: true, ultimax: true, reset: false };

    fn started() -> (Session, Fake) {
        let (mut s, mut c) = (Session::default(), Fake::new());
        s.step(FROZEN, &mut c);
        assert!(s.active);
        (s, c)
    }

    #[test]
    fn the_session_starts_with_mode_1_frz_stop_and_ultimax_and_counts() {
        let (mut s, mut c) = (Session::default(), Fake::new());
        for l in [
            Lines { frz: false, ..FROZEN },
            Lines { stop: false, ..FROZEN },
            Lines { ultimax: false, ..FROZEN },
            Lines { reset: true, ..FROZEN },
        ] {
            s.step(l, &mut c);
            assert!(!s.active, "{l:?}");
        }
        s.step(FROZEN, &mut c);
        assert_eq!(s.status(), 0x0001_0001);
        // Ultimax may go; the session ends when C64_STOP falls, with PROTO cleared.
        s.write_ctrl(1 | SH_PROTO);
        s.step(Lines { ultimax: false, ..FROZEN }, &mut c);
        assert!(s.active);
        s.step(Lines { stop: false, ultimax: false, ..FROZEN }, &mut c);
        assert_eq!((s.status(), s.ctrl()), (0x0001_0000, 1));
        // Mode 0: none; mode 2: any pause, Ultimax or not.
        s.write_ctrl(0);
        s.step(FROZEN, &mut c);
        assert!(!s.active);
        s.write_ctrl(2);
        s.step(Lines { ultimax: false, ..FROZEN }, &mut c);
        assert_eq!(s.status(), 0x0002_0001);
    }

    #[test]
    fn an_abort_blocks_until_stop_is_off_and_a_reset_ends_and_blocks() {
        let (mut s, mut c) = started();
        s.write_ctrl(1 | SH_ABORT);
        assert!(!s.active);
        s.step(FROZEN, &mut c);
        assert!(!s.active, "blocked");
        s.step(Lines { stop: false, ..FROZEN }, &mut c);
        s.step(FROZEN, &mut c);
        assert!(s.active, "C64_STOP fell: unblocked");
        // A reset: the written VIC registers but $13, $14, $1E, $1F for real, then the session ends and blocks.
        s.write(0xD020, 0x02, FROZEN, &mut c);
        s.write(0xD013, 0x44, FROZEN, &mut c);
        s.write(0xDC00, 0x7F, FROZEN, &mut c);
        s.reset(&mut c);
        assert_eq!(c.commits, [(0xD020, 0x02)]);
        assert!(!s.active);
        s.step(FROZEN, &mut c);
        assert!(!s.active, "blocked after the reset");
    }

    #[test]
    fn the_decode_is_the_pla_with_the_frozen_port_and_ultimax() {
        let (mut s, mut c) = started();
        c.ram[0x0400] = 0x11;
        c.ram[0x2000] = 0x22;
        assert_eq!(s.read(0x0400, FROZEN, &mut c), 0x11, "Ultimax: RAM at $0xxx");
        assert_eq!(s.read(0x2000, FROZEN, &mut c), 0xFF, "Ultimax: open");
        assert_eq!(s.read(0xE000, FROZEN, &mut c), 0xFF, "Ultimax ROMH, no cartridge");
        assert_eq!(s.unmapped, 2);
        c.rom = Some(0x99);
        assert_eq!(s.read(0x8000, FROZEN, &mut c), 0x99, "ROML from the cartridge model");
        // Ultimax off: the frozen port $37 maps BASIC, I/O and KERNAL.
        let normal = Lines { ultimax: false, ..FROZEN };
        assert_eq!(s.read(0x2000, normal, &mut c), 0x22);
        assert_eq!(s.read(0xA001, normal, &mut c), (C64Rom::Basic as u8) << 6 | 1);
        assert_eq!(s.read(0xE002, normal, &mut c), (C64Rom::Kernal as u8) << 6 | 2);
        s.write(0xA000, 0x33, normal, &mut c);
        assert_eq!(c.ram[0xA000], 0x33, "a write under ROM lands in RAM");
        assert_eq!(pla(0b10011, 0xD, false), Class::Char);
        assert_eq!(pla(0b10000, 0xD, false), Class::Ram);
        assert!(c.bus.is_empty() && c.commits.is_empty());
    }

    #[test]
    fn io_reads_come_from_the_overlay_and_the_frozen_chip() {
        let (mut s, mut c) = started();
        assert_eq!(s.read(0xD018, FROZEN, &mut c), 0x15, "unwritten: the chip");
        s.write(0xD3D8, 0x14, FROZEN, &mut c);
        assert_eq!(s.read(0xD018, FROZEN, &mut c), 0x15, "written: the overlay, b0 reads 1");
        assert_eq!(c.io[0x018], 0x15, "the chip never sees it");
        s.write(0xD011, 0x1B, FROZEN, &mut c);
        assert_eq!(s.read(0xD011, FROZEN, &mut c), 0x9B, "b7 the raster");
        assert_eq!(s.read(0xD012, FROZEN, &mut c), 0x37);
        assert_eq!(s.read(0xD070, FROZEN, &mut c), 0xFF, "$2F-$3F, mirrored");
        assert_eq!(s.read(0xD400, FROZEN, &mut c), 0xFF, "SID");
        // $D019 less the flags the list acknowledges.
        assert_eq!(s.read(0xD019, FROZEN, &mut c), 0x71);
        s.write(0xD019, 0x01, FROZEN, &mut c);
        assert_eq!(s.read(0xD019, FROZEN, &mut c), 0x70);
        // CIA 1 ports from the overlay and the keys; timers from the chip.
        s.write(0xDC02, 0xFF, FROZEN, &mut c);
        s.write(0xDC00, 0xFD, FROZEN, &mut c);
        assert_eq!(s.read(0xDC01, FROZEN, &mut c), 0xFB);
        s.write(0xDC04, 0x12, FROZEN, &mut c);
        assert_eq!(s.read(0xDC04, FROZEN, &mut c), 0x00);
        s.write(0xDC0E, 0x11, FROZEN, &mut c);
        assert_eq!(s.read(0xDC0E, FROZEN, &mut c), 0x01, "CRA without LOAD");
        // CIA 2 PA: (PR & DDR) | (pins & ¬DDR).
        s.write(0xDDF2, 0x03, FROZEN, &mut c);
        s.write(0xDD00, 0x00, FROZEN, &mut c);
        assert_eq!(s.read(0xDD00, FROZEN, &mut c), 0x94);
        // Colour RAM: real, high nibble $F.
        s.write(0xD800, 0x05, FROZEN, &mut c);
        assert_eq!((s.read(0xD800, FROZEN, &mut c), c.commits.as_slice()), (0xF5, &[(0xD800, 0x05)][..]));
    }

    #[test]
    fn vic_view_takes_only_the_session_writes() {
        let (mut s, mut c) = started();
        assert_eq!(s.vic_view(), VicView::default(), "unwritten: 0 and bank 0");
        s.write(0xD3D8, 0x13, FROZEN, &mut c);
        s.write(0xD021, 0xF6, FROZEN, &mut c);
        s.write(0xDDF2, 0x03, FROZEN, &mut c);
        s.write(0xDD00, 0x01, FROZEN, &mut c);
        s.write(0xD016, 0x18, FROZEN, &mut c);
        assert_eq!(s.vic_view(), VicView { d018: 0x12, d021: 6, bank: 2, mcm: true, ..VicView::default() });
        s.step(Lines { stop: false, ..FROZEN }, &mut c);
        assert_eq!(s.vic_view(), VicView::default(), "dropped at the end");
    }

    #[test]
    fn the_commit_list_writes_plain_registers_once_and_the_others_as_written() {
        let (mut s, mut c) = started();
        s.write(0xD020, 0x01, FROZEN, &mut c);
        s.write(0xD020, 0x02, FROZEN, &mut c);
        s.write(0xD012, 0x10, FROZEN, &mut c);
        s.write(0xD011, 0x9B, FROZEN, &mut c);
        s.write(0xD011, 0x1B, FROZEN, &mut c);
        s.write(0xD400, 0x05, FROZEN, &mut c);
        s.write(0xD400, 0x06, FROZEN, &mut c);
        s.write(0xDC0E, 0x11, FROZEN, &mut c);
        s.write(0xDC00, 0xFF, FROZEN, &mut c);
        s.write(0xD013, 0x00, FROZEN, &mut c);
        assert_eq!(s.list_reg(), 8, "$D020 and $DC00 once, $D013 not at all");
        s.step(Lines { stop: false, ..FROZEN }, &mut c);
        let want = [(0xD020, 0x02), (0xD012, 0x10), (0xD011, 0x9B), (0xD011, 0x1B), (0xD400, 0x05), (0xD400, 0x06), (0xDC0E, 0x11)];
        assert_eq!(c.commits, want, "$DC00 equals the chip and is left out");
        assert_eq!((s.last_commit, s.real), (7, 7));
        assert_eq!(s.list_reg(), 0);
    }

    #[test]
    fn proto_keeps_writes_out_of_the_list_and_discard_or_abort_drop_it() {
        let (mut s, mut c) = started();
        s.write_ctrl(1 | SH_PROTO);
        s.write(0xD020, 0x05, FROZEN, &mut c);
        assert_eq!(s.list_reg(), 0);
        assert_eq!(s.read(0xD020, FROZEN, &mut c), 0xF5, "held for reads");
        s.write_ctrl(1);
        s.write(0xD021, 0x05, FROZEN, &mut c);
        s.write_ctrl(1 | SH_DISCARD);
        assert_eq!((s.list_reg(), s.active), (0, true));
        s.write(0xD022, 0x05, FROZEN, &mut c);
        s.write_ctrl(1 | SH_ABORT);
        s.step(Lines { stop: false, ..FROZEN }, &mut c);
        assert!(c.commits.is_empty());
        // A full list drops and counts.
        s.step(FROZEN, &mut c);
        for i in 0..130 {
            s.write(0xD400, i as u8, FROZEN, &mut c);
        }
        assert_eq!(s.list_reg(), 2 << 16 | 128);
    }

    #[test]
    fn sidefx_makes_io_and_cartridge_accesses_real() {
        let (mut s, mut c) = started();
        s.write_ctrl(1 | SH_SIDEFX);
        s.write(0xD020, 0x07, FROZEN, &mut c);
        assert_eq!(s.read(0xD019, FROZEN, &mut c), 0x5A);
        assert_eq!(s.read(0x8000, FROZEN, &mut c), 0x5A);
        assert_eq!(c.bus, [(0xD020, 0x07), (0xD019, 0), (0x8000, 0)]);
        assert_eq!((s.real, s.list_reg()), (3, 0));
    }
}
