//! C64 cart/machine control, DMA window, MATRIX_KEYB, core config, palette and ROM windows ([`C64Port`]), plus the
//! C64-side windows that stay T0 stubs (legacy SID, sampler, EEPROM, PLD, …).
//! Spec: docs/specs/S14-c64-trx64.md (T0: docs/specs/S04-board-t0.md). Registers: docs/hw/10-c64-machine.md.
//!
//! Without a backend [`C64Port`] is the T0 stub of doc 10 §T0. `Machine::attach_c64` plugs in a [`C64Backend`], and
//! the same windows then drive a real C64 (S14 §5).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::c64host::{C64Backend, C64CartSlot, C64Frame, C64Rom};
use crate::devices::board::{add_table, at, span, Reg, RegTable, Span, RAM, RAM_PAGE};
use crate::devices::drives::DriveRegs;
use crate::io::{IoCtx, IoDevice, IoMap, IO_BASE};
use crate::machine::MachineConfig;

/// C64_CORE_VERSION (0x10180010). Display only ("1.%02x", product.cc:139, system_info.cc:176); the value on
/// real cores is OPEN (10 Q6), so any fixed non-zero value serves.
const CORE_VERSION: u8 = 0x01;

/// [`C64Port`] windows as offsets from `IO_BASE`, mapped with `IoMap::map_origin` (S14 §2).
const CART: u32 = 0x4_0000;
const CART_END: u32 = CART + 0xFF;
const DMA: u32 = 0x5_0000;
const DMA_END: u32 = DMA + 0xFFFF;
const MATRIX: u32 = 0x10_0300;
const MATRIX_END: u32 = MATRIX + 0xFF;
const CORE: u32 = 0x18_0000;
const CORE_END: u32 = CORE + 0xFF;
const PALETTE: u32 = 0x18_0800;
const PALETTE_END: u32 = PALETTE + 0x7FF;
const BASIC: u32 = 0x18_8000;
const KERNAL: u32 = 0x18_A000;
const CHAR: u32 = 0x18_C000;
const CHAR_END: u32 = CHAR + 0xFFF;
/// EEPROM_BASE 0x1004C000: dirty flag and GMOD2 EEPROM, served by the backend's cartridge (W4-CART).
const EEPROM: u32 = 0x4_C000;
const EEPROM_END: u32 = EEPROM + 0xFFF;
/// W4-DRIVE: drive A (devices/drives.rs), served here so it reaches the backend's drive.
const DRIVE_A: u32 = 0x2_0000;
const DRIVE_A_END: u32 = DRIVE_A + 0x3FFF;
/// UCI: `CMD_IF_BASE` 0x10044000, the firmware side of the Ultimate Command Interface (iomap.h:16). Served by the
/// backend's block when it has one, else by the T0 table [`UCI_T0`] (docs/specs/S15-uci.md).
const UCI: u32 = 0x4_4000;
const UCI_END: u32 = UCI + 0xFFF;

/// (offset, size) of every [`C64Port`] window.
const WINDOWS: [(u32, u32); 11] = [
    (DRIVE_A, 0x4000),
    (CART, 0x100),
    (DMA, 0x1_0000),
    (MATRIX, 0x100),
    (CORE, 0x100),
    (PALETTE, 0x800),
    (BASIC, 0x2000),
    (KERNAL, 0x2000),
    (CHAR, 0x1000),
    (EEPROM, 0x1000),
    (UCI, 0x1000),
];

/// UltiCommand interface 0x10044000 without a backend that has the block (command_protocol.vhd; moved here from
/// devices/iec.rs with the UCI window). No C64 command ever arrives at T0.
const UCI_T0: &[Span] = &[
    // SLOT_BASE, slot_base(6 downto 1).
    at(0x00, Reg::Latch { mask: 0x7E, init: 0 }),
    // SLOT_ENABLE: bit7 = 0 writes the enable (bit0); bit7 = 1 writes the C64 bus ID instead, so the
    // SoftIEC 0x8B (iec_drive.cc:199) leaves the slot disabled for c64.cc:1355.
    at(0x01, Reg::Gated { mask: 0x01, veto: 0x80 }),
    // 0x02 HANDSHAKE_OUT (freeze|trigger|state) and 0x03 STATUSBYTE read 0: 11 H14 `is_dma_active` false,
    // 11 H7 / 00 §2 C3 ITU bit 4 stays low.
    // 00 §2 A5, 11 H6: buffer bases read in the pre-main ctor (command_intf.cc:50-53), buffer address(10:3).
    // +4/+5 writes are IRQ mask set/clear, not stores (command_intf.cc:117-118).
    at(0x04, Reg::Set { cell: 0x0B, mask: 0x07, read: Some(0x00) }),
    at(0x05, Reg::Clear { cell: 0x0B, mask: 0x07, read: Some(0x6F) }),
    at(0x06, Reg::Const(0x70)),
    at(0x07, Reg::Const(0xDF)),
    at(0x08, Reg::Const(0xE0)),
    at(0x09, Reg::Const(0xFF)),
    // 0x0A status read pointer idles at 0x700 (low byte 0).
    // IRQMASK, reset 0b111.
    at(0x0B, Reg::Latch { mask: 0x07, init: 0x07 }),
    // Response read pointer idles at the response buffer 0x380 (`reset_response`).
    at(0x0C, Reg::Const(0x80)),
    at(0x0D, Reg::Const(0x03)),
    // 0x0E/0x0F COMMAND_LEN 0. Command 0x800, response 0xB80, status 0xF00 buffers.
    span(0x800, 0x1000, RAM),
];

/// ITU low bit 4 `ITU_INTERRUPT_CMDIF` (itu.h:37): the UCI's firmware interrupt, a level
/// (command_protocol.vhd:310; docs/hw/02 §Low IRQ byte).
const UCI_IRQ_BIT: u8 = 4;
/// ITU low bit 7 `ITU_INTERRUPT_RESET` (itu.h:40): the C64 reset, an edge (ultimate_logic_32.vhd:531).
const C64_RESET_IRQ_BIT: u8 = 7;
/// ITU high IRQ 6, `unlock_irq` (itu.h:46; u64_config.cc:974). A level with no ack register in the ITU; its handler
/// drops the source with `C64_POKE(0xD038, 0)` (u64_config.cc:1014).
const UNLOCK_IRQ_HIGH_BIT: u8 = 6;
/// The DMA write that acks high IRQ 6 (u64_config.cc:1014).
const UNLOCK_ACK_ADDR: u16 = 0xD038;

/// Clocks between periodic C64 syncs: 1 ms, about 985 C64 cycles (S14 §4).
const SYNC_PERIOD: u64 = 100_000;

/// Cart register latches other than C64_MODE / C64_STOP / C64_CARTRIDGE_KILL (cart_slot_registers.vhd, reset per
/// cart_slot_pkg.vhd `c_cart_control_init`). 10 H16: read-modify-write users need read-back.
const CART_REGS: &[Span] = &[
    // C64_STOP_MODE bits 1:0 (c64.h:77-79). Latch only: every stop is immediate (S14 §5.1).
    at(0x02, Reg::Latch { mask: 0x03, init: 0 }),
    // 00 §2 C14, 10 H4/H8: PHI2 present (bit0) and RESET sense (bit4) never set, so
    // `while (C64_CLOCK_DETECT & 0x10);` (u64_config.cc:662-663) exits and `phi2_present()` holds.
    at(0x03, Reg::Const(0x01)),
    // C64_CARTRIDGE_TYPE: type 4:0 | variant 7:5.
    at(0x05, RAM),
    // 0x06 W kill/force strobes; R CARTRIDGE_ACTIVE bit0 = 0 without a C64 (10 T0).
    at(0x07, Reg::Latch { mask: 0x01, init: 0 }),
    at(0x08, Reg::Latch { mask: 0x01, init: 0 }),
    // C64_REU_SIZE, reset "111".
    at(0x09, Reg::Latch { mask: 0x07, init: 0x07 }),
    // 0x0A SWAP_CART_BUTTONS and 0x0B TIMING_ADDR_VALID: the VHDL read-back is commented out.
    // C64_PHI2_EDGE_RECOVER: bit0 (reset 1) and force_serve_vic bit2 read back; bit3 is a trigger strobe.
    at(0x0C, Reg::Latch { mask: 0x05, init: 0x01 }),
    // C64_SERVE_CONTROL (u64_machine.cc:204-210) and C64_SAMPLER_ENABLE (c64.cc:1362). SERVE_WHILE_STOPPED needs no
    // action: the backend's cartridge always answers the bus.
    at(0x0D, Reg::Latch { mask: 0x01, init: 0 }),
    at(0x0E, Reg::Latch { mask: 0x01, init: 0 }),
];

/// Cart register offsets (c64.h:54-68).
const MODE: u32 = 0x00;
const STOP: u32 = 0x01;
const CLOCK_DETECT: u32 = 0x03;
const CARTRIDGE_TYPE: u32 = 0x05;
const CARTRIDGE_KILL: u32 = 0x06;

/// C64_MODE bits (c64.h:70-73).
const MODE_ULTIMAX: u8 = 0x02;
const MODE_RESET: u8 = 0x04;
const MODE_UNRESET: u8 = 0x08;
const MODE_NMI: u8 = 0x10;

/// C64_CLOCK_DETECT bits (c64.h:88-93).
const CD_PHI2_DETECT: u8 = 0x01;
const CD_RESET_SENSE: u8 = 0x10;

/// C64_CARTRIDGE_KILL write strobes: bit0 kill, bit1 force update (c64.cc:1469-1474, c64_subsys.cc:189-190).
const KILL_CART: u8 = 0x01;
const KILL_FORCE: u8 = 0x02;

/// C64_REU_ENABLE and C64_REU_SIZE (c64.h:62-63). The firmware writes the size first and the enable after
/// (c64.cc:315-317), and both are read back as the latches they are (docs/status/reu.md).
const REU_ENABLE: u32 = 0x08;
const REU_SIZE: u32 = 0x09;

/// C64_REU_SIZE 0..7 as KiB: `128 << n`, the firmware's `reu_size` table (c64.cc:60).
fn reu_size_kb(reg: u8) -> u32 {
    128 << (reg & 0x07)
}

/// `__cart_rom_start` in DDR (linker.x:269-287): the ROM of the CART_TYPE_NORMAL family, 16 K
/// (`set_cartridge` memcpy, c64.cc:1285-1288).
const CART_ROM_DDR: usize = 0x03C0_0000;
const CART_ROM_SIZE: usize = 0x4000;

/// C64 core config 0x10180000 (u64.h:104-154): RAM-like latches (10 H16), CORE_VERSION constant.
const CORE_CONFIG: &[Span] = &[
    at(0x10, Reg::Const(CORE_VERSION)),
    // C64_VOICE_ADSR(x): envelope levels polled by the LED strip task (led_strip.cc:323-325); no SID runs.
    span(0x80, 0x88, Reg::Raz),
    // C64_JOY1/2_SWOUT, active low: released until joystick_output.cc:81-82 or usb_hid.cc:69 writes them. The RAM
    // spans leave them out, because a later span's power-on fill would overwrite this one.
    span(JOY1_SWOUT, JOY2_SWOUT + 1, Reg::Latch { mask: 0xFF, init: 0xFF }),
    span(0x00, JOY1_SWOUT, RAM),
    span(JOY2_SWOUT + 1, 0x100, RAM),
];

/// Core config offsets (u64.h:104-154).
const VIDEOFORMAT: u32 = 0x01;
const DMA_MEMONLY: u32 = 0x03;
const JOY1_SWOUT: u32 = 0x30;
const JOY2_SWOUT: u32 = 0x31;
/// C64_VIDEOFORMAT bit1: 60 Hz (u64.h:163-170).
const VIDEOFORMAT_60HZ: u8 = 0x02;

/// C64_PALETTE RGB: 16 × {R,G,B,pad} at +0x000; YUV at +0x400 has no effect (u64_config.cc:2720-2763).
const PALETTE_RGB_SIZE: u32 = 0x40;

/// MATRIX_KEYB [9]: restore (keyboard_usb.cc:227).
const MATRIX_RESTORE: u32 = 9;
/// MATRIX_KEYB [10]: freeze, the freezer cartridges' button (keyboard_usb.cc:228; W4-CART).
const MATRIX_FREEZE: u32 = 10;

/// 00 §1c M8: ROM windows read back what was written. `U64Machine::read_cpu_block` loads from them
/// (u64_machine.cc:92-107, ELF 0x52A14-0x52A88) and the monitor caches them (u64_memory_backend.cc:74-83).
const ROM_8K: &[Span] = &[span(0, 0x2000, RAM)];
const ROM_4K: &[Span] = &[span(0, 0x1000, RAM)];

/// PAL raster lines; the T0 per-read counter wraps here.
const RASTER_LINES: u16 = 312;

/// T0 stand-in for the C64 bus: a 64 K byte array plus a minimal I/O overlay (10 §Emulator model tiers T0).
/// C64_MODE and DMA_MEMONLY mapping are ignored.
struct DmaStub {
    mem: Vec<u8>,
    /// Raster line 0..311, advanced by each `$D012` read.
    raster: u16,
    /// Pressed keys: bit `col` of `keys[row]`, the `U64Io::set_key` convention.
    keys: [u8; 8],
}

impl DmaStub {
    fn new() -> Self {
        DmaStub { mem: vec![0; 0x1_0000], raster: 0, keys: [0; 8] }
    }

    /// Press/release a C64 matrix key (`U64Io::set_key` convention). A firmware UI on the C64 screen, such as the
    /// updater's, scans it through $DC00/$DC01 (`Keyboard_C64` on `CIA1_DPB`/`CIA1_DPA`, c64.cc:144;
    /// update_common.h:208-218).
    fn set_key(&mut self, row: u8, col: u8, down: bool) {
        if row >= 8 || col >= 8 {
            return;
        }
        if down {
            self.keys[row as usize] |= 1 << col;
        } else {
            self.keys[row as usize] &= !(1 << col);
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        // 00 §2 C16, 10 H5: `while (C64_PEEK(0xD012) != 0xFF);` runs with interrupts masked
        // (u64_config.cc:2239-2240, 2349), so the raster must reach 0xFF by reads alone.
        if addr == 0xD012 {
            self.raster = (self.raster + 1) % RASTER_LINES;
        }
        self.peek(addr)
    }

    fn peek(&self, addr: u16) -> u8 {
        match addr {
            // $D011 bit7 = raster bit 8; the other bits are the written value.
            0xD011 => (self.mem[0xD011] & 0x7F) | ((self.raster >> 8) as u8) << 7,
            0xD012 => self.raster as u8,
            // $D019: no VIC interrupt ever latches (10 T0).
            0xD019 => 0,
            // 00 §2 C18, §3 C11, 10 H11: CIA1 port A reads 0xFF, so a port-2 joystick read after W $DC00 ← 0xFF
            // sees no press.
            0xDC00 => 0xFF,
            // 10 H10: port B returns the keys of the rows the last $DC00 write drives low, a pure function of that
            // write and the pressed keys, so `do{R $DC01; W $DC00} while (R $DC01 differs)` (keyboard_c64.cc:135-138,
            // 240-243) terminates. With no key down it reads 0xFF, a stable "no key".
            0xDC01 => {
                let select = self.mem[0xDC00];
                (0..8).filter(|&row| select & (1 << row) == 0).fold(0xFF, |v, row| v & !self.keys[row])
            }
            // 00 §2 C15, §3 C11, 10 H12/H13: SID sockets read 0, so every probe (u64_config.cc:572-640,
            // sid_device_pdsid.cc:113, sid_device_sidkick.cc:175-183) and S_SidDetector report "none".
            0xD400..=0xD7FF => 0,
            _ => self.mem[usize::from(addr)],
        }
    }
}

/// The firmware's view of the C64: cart/machine control 0x10040000, DMA window 0x10050000, MATRIX_KEYB 0x10100300,
/// core config 0x10180000, palette 0x10180800 and the ROM windows 0x10188000-0x1018CFFF (S14 §5), one device mapped
/// with origin `IO_BASE`.
///
/// Every access to the cart registers, the DMA window or MATRIX_KEYB first advances the backend to the accessing
/// instruction's clock; in between, `tick` advances it every [`SYNC_PERIOD`] (S14 §4).
pub struct C64Port {
    backend: Option<Box<dyn C64Backend>>,
    /// Clock the backend was last advanced to.
    synced: u64,
    cart: RegTable,
    /// C64_MODE read value: ULTIMAX, RESET, NMI.
    mode: u8,
    /// C64_STOP bit0, the stop request.
    stop: bool,
    /// The C64 bus while no backend is attached.
    dma: DmaStub,
    matrix: RegTable,
    core: RegTable,
    /// BASIC, KERNAL and CHAR windows while no backend is attached.
    roms: [RegTable; 3],
    /// Host joystick lines for port 2, active low.
    joystick: u8,
    /// Host RESTORE key held.
    restore: bool,
    /// The PAL-only notice for C64_VIDEOFORMAT 60 Hz was printed.
    pal_noted: bool,
    /// W4-DRIVE: drive A registers; the drive behind them is the backend's (`C64Backend::drive`).
    drive_a: DriveRegs,
    /// CARTSLOT: U64_CART_DETECT, shared with `U64Io` (docs/status/cart-slot.md).
    cart_detect: Option<Arc<AtomicU8>>,
    /// UCI 0x10044000 while the backend has no block of its own (S15).
    uci: RegTable,
}

impl Default for C64Port {
    fn default() -> Self {
        Self::new()
    }
}

impl C64Port {
    pub fn new() -> Self {
        C64Port {
            backend: None,
            synced: 0,
            cart: RegTable::new("c64-cartregs", CART_REGS),
            mode: 0,
            stop: false,
            dma: DmaStub::new(),
            matrix: RegTable::new("matrix-keyb", RAM_PAGE),
            core: RegTable::new("c64-core-config", CORE_CONFIG),
            roms: [
                RegTable::new("basic-rom", ROM_8K),
                RegTable::new("kernal-rom", ROM_8K),
                RegTable::new("char-rom", ROM_4K),
            ],
            joystick: 0xFF,
            restore: false,
            pal_noted: false,
            drive_a: DriveRegs::new(0),
            cart_detect: None,
            uci: RegTable::new("uci", UCI_T0),
        }
    }

    /// Attach the C64 at clock `now`, which anchors its clock (S14 §3).
    pub fn attach(&mut self, mut backend: Box<dyn C64Backend>, now: u64) {
        backend.advance_to(now);
        self.synced = now;
        self.backend = Some(backend);
        self.refresh_cart_detect();
    }

    /// CARTSLOT: share U64_CART_DETECT with `U64Io`; the port keeps it at the backend's physical cartridge lines
    /// (docs/status/cart-slot.md).
    pub fn set_cart_detect(&mut self, cell: Arc<AtomicU8>) {
        self.cart_detect = Some(cell);
        self.refresh_cart_detect();
    }

    /// The cartridge in the backend's physical expansion port.
    pub fn cart_slot(&mut self) -> Option<&mut dyn C64CartSlot> {
        self.backend.as_mut()?.cart_slot()
    }

    /// Store the physical cartridge's GAME/EXROM for U64_CART_DETECT. Called after every access that may run the C64
    /// or reach its bus, so a firmware read sees the lines as of its last C64 access.
    fn refresh_cart_detect(&self) {
        if let (Some(cell), Some(b)) = (&self.cart_detect, &self.backend) {
            cell.store(b.cart_detect(), Ordering::Relaxed);
        }
    }

    /// Host key at the `U64Io::set_key` matrix position: the backend's keyboard, or CIA1 of the T0 stub.
    pub fn set_key(&mut self, row: u8, col: u8, down: bool) {
        match &mut self.backend {
            Some(b) => b.set_key(row, col, down),
            None => self.dma.set_key(row, col, down),
        }
    }

    /// What a DMA read of C64 address `addr` returns, without side effects (the backend's bus or the T0 stub).
    pub fn dma_peek(&self, addr: u16) -> u8 {
        self.peek8(DMA + u32::from(addr))
    }

    /// Host joystick for C64 port 2, active low, ANDed with C64_JOY2_SWOUT (S14 §6).
    pub fn set_joystick(&mut self, lines: u8) {
        self.joystick = lines;
        self.apply_joysticks();
    }

    /// Host RESTORE key, ORed into the NMI line (S14 §6).
    pub fn set_restore(&mut self, held: bool) {
        self.restore = held;
        self.apply_nmi();
    }

    /// The C64 frame, None without a backend.
    pub fn frame(&self) -> Option<C64Frame> {
        self.backend.as_ref().map(|b| b.frame())
    }

    fn sync(&mut self, now: u64) {
        if let Some(b) = &mut self.backend {
            if now > self.synced {
                b.advance_to(now);
                self.synced = now;
            }
        }
    }

    fn mem_only(&self) -> bool {
        self.core.get(DMA_MEMONLY) & 0x01 != 0
    }

    fn cart_get(&self, off: u32) -> u8 {
        match (off & 0x0F, &self.backend) {
            (MODE, _) => self.mode,
            // 00 §2 C12/C17, 10 H1-H3: STOP reads req | req<<1. HAS_STOPPED follows the request at once, so
            // `hard_stop`'s `while(!(C64_STOP & 2));` (c64.cc:405) and the forced poll in `stop()`
            // (c64.cc:490-497) never spin, and a released stop reads 0 (c64.cc:1449).
            (STOP, _) => u8::from(self.stop) * 0x03,
            // 10 H4: RESET sense follows the held reset.
            (CLOCK_DETECT, Some(_)) if self.mode & MODE_RESET != 0 => CD_PHI2_DETECT | CD_RESET_SENSE,
            (CARTRIDGE_KILL, Some(b)) => u8::from(b.cart_active()),
            (r, _) => self.cart.get(r),
        }
    }

    fn cart_write(&mut self, reg: u32, val: u8, ram: &[u8]) {
        match reg {
            // cart_slot_registers.vhd: bit2 asserts reset, else bit3 releases it, else bits 1/4 set ULTIMAX/NMI.
            MODE => {
                let old = self.mode;
                if val & MODE_RESET != 0 {
                    self.mode |= MODE_RESET;
                } else if val & MODE_UNRESET != 0 {
                    self.mode &= !MODE_RESET;
                } else {
                    self.mode = (self.mode & MODE_RESET) | (val & (MODE_ULTIMAX | MODE_NMI));
                }
                let changed = old ^ self.mode;
                let cart_type = self.cart.get(CARTRIDGE_TYPE);
                if let Some(b) = &mut self.backend {
                    if changed & MODE_RESET != 0 {
                        // S14 §7: a release rebuilds the cartridge from DDR, then warm-resets.
                        if self.mode & MODE_RESET == 0 {
                            b.set_cart(cart_type, cart_rom(ram));
                        }
                        b.set_reset(self.mode & MODE_RESET != 0);
                    }
                    if changed & MODE_ULTIMAX != 0 {
                        b.set_ultimax(self.mode & MODE_ULTIMAX != 0);
                    }
                }
                if changed & MODE_NMI != 0 {
                    self.apply_nmi();
                }
            }
            STOP => {
                self.stop = val & 1 != 0;
                if let Some(b) = &mut self.backend {
                    b.set_stopped(self.stop);
                }
            }
            CARTRIDGE_KILL => {
                let cart_type = self.cart.get(CARTRIDGE_TYPE);
                if let Some(b) = &mut self.backend {
                    if val & KILL_CART != 0 {
                        b.kill_cart();
                    }
                    if val & KILL_FORCE != 0 {
                        b.set_cart(cart_type, cart_rom(ram));
                    }
                }
            }
            // REU: both stay the latches the FPGA reads back, and the backend gets the value the latch took — the
            // size first, which is the order the firmware writes them in (c64.cc:315-317, docs/status/reu.md).
            REU_SIZE => {
                self.cart.set(reg, val);
                let size_kb = reu_size_kb(self.cart.get(REU_SIZE));
                if let Some(b) = &mut self.backend {
                    b.set_reu_size_kb(size_kb);
                }
            }
            REU_ENABLE => {
                self.cart.set(reg, val);
                let on = self.cart.get(REU_ENABLE) & 0x01 != 0;
                if let Some(b) = &mut self.backend {
                    b.set_reu_enabled(on);
                }
            }
            r => self.cart.set(r, val),
        }
    }

    fn matrix_write(&mut self, reg: u32, val: u8) {
        self.matrix.set(reg, val);
        if reg < 8 {
            let rows = std::array::from_fn(|i| self.matrix.get(i as u32));
            if let Some(b) = &mut self.backend {
                b.set_matrix_keyb(rows);
            }
        } else if reg == MATRIX_RESTORE {
            self.apply_nmi();
        } else if reg == MATRIX_FREEZE {
            if let Some(b) = &mut self.backend {
                b.set_freeze_button(val != 0);
            }
        }
    }

    fn core_write(&mut self, reg: u32, val: u8) {
        self.core.set(reg, val);
        // W4-SID: the SID decode (SIDx/EMUSIDx BASE, MASK, EN, WAVES, SPLIT) is the backend's (S14 §W4-SID).
        if let Some(b) = &mut self.backend {
            b.core_config_write(reg as u8, val);
        }
        match reg {
            VIDEOFORMAT if val & VIDEOFORMAT_60HZ != 0 && self.backend.is_some() && !self.pal_noted => {
                eprintln!("c64: C64_VIDEOFORMAT {val:#04x} asks for 60 Hz; the C64 core is PAL-only (S14 §13 OQ3)");
                self.pal_noted = true;
            }
            JOY1_SWOUT | JOY2_SWOUT => self.apply_joysticks(),
            _ => {}
        }
    }

    fn rom_window(off: u32) -> (C64Rom, u32) {
        match off {
            BASIC..KERNAL => (C64Rom::Basic, off - BASIC),
            KERNAL..CHAR => (C64Rom::Kernal, off - KERNAL),
            _ => (C64Rom::Char, off - CHAR),
        }
    }

    fn apply_joysticks(&mut self) {
        let port1 = self.core.get(JOY1_SWOUT);
        let port2 = self.core.get(JOY2_SWOUT) & self.joystick;
        if let Some(b) = &mut self.backend {
            b.set_joystick(1, port1);
            b.set_joystick(2, port2);
        }
    }

    /// Lend guest DDR to the backend for this access: its cartridge logic reads and writes cart ROM and RAM there
    /// (W4-CART, `C64Backend::lend_ddr`). An access that lends takes it back with [`Self::return_ddr`] before it
    /// returns and uses `ctx.ram` in between for reads only.
    fn lend_ddr(&mut self, ctx: &mut IoCtx) {
        if let Some(b) = &mut self.backend {
            b.lend_ddr(Some(&mut *ctx.ram));
        }
    }

    fn return_ddr(&mut self) {
        if let Some(b) = &mut self.backend {
            b.lend_ddr(None);
        }
        // CARTSLOT: every access that lends DDR may have run the C64 or reached its cartridges.
        self.refresh_cart_detect();
    }

    fn apply_nmi(&mut self) {
        let level = self.mode & MODE_NMI != 0 || self.matrix.get(MATRIX_RESTORE) != 0 || self.restore;
        if let Some(b) = &mut self.backend {
            b.set_nmi(level);
        }
    }

    /// UCI: whether the backend serves the block. Without one the window stays the T0 table (S15).
    fn has_uci(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.has_uci())
    }

    /// The UCI's lines into the ITU, after anything that may have run the C64 or reached the block (S15 §3):
    /// low bit 4 is the firmware IRQ level, recomputed every time; low bit 7 is the C64-reset edge and high IRQ 6
    /// the unlock, both taken from the block's event queue. Without a UCI nothing is driven, as before.
    fn update_uci(&mut self, ctx: &mut IoCtx) {
        let Some(b) = self.backend.as_mut().filter(|b| b.has_uci()) else { return };
        let irq = b.uci_irq();
        let events = b.uci_take_events();
        ctx.irq.set_level(UCI_IRQ_BIT, irq);
        if events.c64_reset {
            ctx.irq.pulse(C64_RESET_IRQ_BIT);
        }
        if events.unlock {
            ctx.irq.set_high(UNLOCK_IRQ_HIGH_BIT, true);
        }
    }
}

/// The cart ROM area of DDR.
fn cart_rom(ram: &[u8]) -> &[u8] {
    &ram[CART_ROM_DDR..CART_ROM_DDR + CART_ROM_SIZE]
}

impl IoDevice for C64Port {
    fn name(&self) -> &'static str {
        "c64"
    }

    fn read8(&mut self, off: u32, ctx: &mut IoCtx) -> u8 {
        match off {
            CART..=CART_END | MATRIX..=MATRIX_END => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                let val = self.peek8(off);
                self.return_ddr();
                self.update_uci(ctx);
                val
            }
            DMA..=DMA_END => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                let (addr, mem_only) = ((off - DMA) as u16, self.mem_only());
                let val = match &mut self.backend {
                    Some(b) => b.dma_read(addr, mem_only),
                    None => self.dma.read(addr),
                };
                self.return_ddr();
                self.update_uci(ctx);
                val
            }
            // UCI: the block is C64-side state, so the C64 runs up to this instruction first, as the cart registers
            // do. `uci_read` itself has no side effects (S15 §3).
            UCI..=UCI_END if self.has_uci() => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                let val = self.backend.as_ref().map_or(0, |b| b.uci_read((off - UCI) as u16));
                self.return_ddr();
                self.update_uci(ctx);
                val
            }
            // W4-DRIVE: drive A.
            DRIVE_A..=DRIVE_A_END => {
                // W4-CART: the sync may run the C64, whose cartridge reads DDR; the lease ends before DriveRegs uses ctx.
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                self.return_ddr();
                self.update_uci(ctx);
                let drive = self.backend.as_mut().and_then(|b| b.drive(0));
                self.drive_a.read(off - DRIVE_A, ctx, drive)
            }
            _ => self.peek8(off),
        }
    }

    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        match off {
            CART..=CART_END => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                self.cart_write(off & 0x0F, val, ctx.ram);
                self.return_ddr();
                self.update_uci(ctx);
            }
            DMA..=DMA_END => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                let (addr, mem_only) = ((off - DMA) as u16, self.mem_only());
                match &mut self.backend {
                    Some(b) => b.dma_write(addr, val, mem_only),
                    None => self.dma.mem[usize::from(addr)] = val,
                }
                self.return_ddr();
                // UCI: `unlock_irq` acks high IRQ 6 with `C64_POKE(0xD038, 0)`; the ITU has no ack register of its
                // own, so this write is the only thing that drops the source (u64_config.cc:1012-1014, 02 H8).
                if addr == UNLOCK_ACK_ADDR && val == 0 && self.has_uci() {
                    ctx.irq.set_high(UNLOCK_IRQ_HIGH_BIT, false);
                }
                self.update_uci(ctx);
            }
            // UCI, when the backend has the block.
            UCI..=UCI_END if self.has_uci() => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                if let Some(b) = &mut self.backend {
                    b.uci_write((off - UCI) as u16, val);
                }
                self.return_ddr();
                self.update_uci(ctx);
            }
            UCI..=UCI_END => self.uci.set(off - UCI, val),
            MATRIX..=MATRIX_END => {
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                self.matrix_write(off - MATRIX, val);
                self.return_ddr();
                self.update_uci(ctx);
            }
            EEPROM..=EEPROM_END => {
                if let Some(b) = &mut self.backend {
                    b.eeprom_write((off - EEPROM) as u16, val);
                }
            }
            CORE..=CORE_END => self.core_write(off - CORE, val),
            PALETTE..=PALETTE_END => {
                if let Some(b) = self.backend.as_mut().filter(|_| off - PALETTE < PALETTE_RGB_SIZE) {
                    b.set_palette_byte((off - PALETTE) as u8, val);
                }
            }
            BASIC..=CHAR_END => {
                let (rom, rel) = Self::rom_window(off);
                match &mut self.backend {
                    Some(b) => b.rom_write(rom, rel as u16, val),
                    None => self.roms[rom as usize].set(rel, val),
                }
            }
            // W4-DRIVE: drive A.
            DRIVE_A..=DRIVE_A_END => {
                // W4-CART: the sync may run the C64, whose cartridge reads DDR; the lease ends before DriveRegs uses ctx.
                self.lend_ddr(ctx);
                self.sync(ctx.now);
                self.return_ddr();
                self.update_uci(ctx);
                let drive = self.backend.as_mut().and_then(|b| b.drive(0));
                self.drive_a.write(off - DRIVE_A, val, ctx, drive);
            }
            _ => {}
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        match off {
            CART..=CART_END => self.cart_get(off),
            DMA..=DMA_END => {
                let addr = (off - DMA) as u16;
                match &self.backend {
                    Some(b) => b.dma_peek(addr),
                    None => self.dma.peek(addr),
                }
            }
            MATRIX..=MATRIX_END => self.matrix.get(off - MATRIX),
            CORE..=CORE_END => self.core.get(off - CORE),
            // Without a backend the window reads 0: EEPROM not dirty (10 T0).
            EEPROM..=EEPROM_END => self.backend.as_ref().map_or(0, |b| b.eeprom_read((off - EEPROM) as u16)),
            // UCI: `uci_read` is side-effect free, so a peek is the same read (S15 §3).
            UCI..=UCI_END => match self.backend.as_ref().filter(|b| b.has_uci()) {
                Some(b) => b.uci_read((off - UCI) as u16),
                None => self.uci.get(off - UCI),
            },
            BASIC..=CHAR_END => {
                let (rom, rel) = Self::rom_window(off);
                match &self.backend {
                    Some(b) => b.rom_read(rom, rel as u16),
                    None => self.roms[rom as usize].get(rel),
                }
            }
            // W4-DRIVE: drive A.
            DRIVE_A..=DRIVE_A_END => self.drive_a.peek(off - DRIVE_A),
            // The palette is write-only (u64_config.cc:2724-2763).
            _ => 0,
        }
    }

    fn next_event(&self) -> Option<u64> {
        self.backend.as_ref().map(|_| self.synced + SYNC_PERIOD)
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.lend_ddr(ctx);
        self.sync(ctx.now);
        self.return_ddr();
        self.update_uci(ctx);
        // W4-DRIVE: carry drive A's writes into DDR.
        let drive = self.backend.as_mut().and_then(|b| b.drive(0));
        self.drive_a.tick(ctx, drive);
    }

    /// Power-on register state; an attached backend stays attached, and keys held on the keyboard stay held. The
    /// backend's own UCI block is not reset here either: only the FPGA system reset clears it
    /// (command_protocol.vhd:292-306), which on TRX64 is `Machine::new` (Spec 852 §2).
    fn reset(&mut self) {
        let (backend, synced, keys, cart_detect) = (self.backend.take(), self.synced, self.dma.keys, self.cart_detect.take());
        *self = C64Port::new();
        (self.backend, self.synced, self.dma.keys, self.cart_detect) = (backend, synced, keys, cart_detect);
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    let port = map.devices.len();
    map.devices.push(Box::new(C64Port::new()));
    for (off, size) in WINDOWS {
        map.map_origin(IO_BASE + off, size, port, IO_BASE);
    }
    // Legacy U2 SID_BASE and its filter RAM at +0x800: no compiled user (12 Region C).
    add_table(map, 0x1004_2000, 0x1000, "legacy-sid", &[]);
    // CART_TIMING_BASE == COPPER_BASE (00 §1c M2): developer bus measurement only (c64.cc:1793-1852).
    add_table(map, 0x1004_6000, 0x800, "cart-timing", &[]);
    // Sampler, 256 B aliased over 8 K: never read, reset writes only (12 Region A).
    add_table(map, 0x1004_8000, 0x2000, "sampler", &[]);
    // C64_PLD_ACC: `release_ownership` reads 0x10181000/01 and writes 0x10181010/11 (00 §2 C32).
    add_table(map, 0x1018_1000, 0x100, "c64-pld", RAM_PAGE);
    // U64_DEBUG_REGISTER: REST / socket read-back (route_machine.cc:462-490).
    add_table(map, 0x1018_1800, 0x100, "u64-debug", RAM_PAGE);
    // C64_GLYPH: defined, no compiled user.
    add_table(map, 0x1018_2000, 0x200, "c64-glyph", &[]);
    // C64_SID_BASE: UltiSID filter curves at +0x1000/+0x1800, write-only (u64_config.cc:1259-1298).
    add_table(map, 0x1018_4000, 0x2000, "ultisid", &[]);
    // U64_UDP_BASE stream header templates: write-only (data_streamer.cc:311-404).
    add_table(map, 0x1019_0000, 0x100, "udp-headers", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::RAM_SIZE;
    use crate::c64host::mock::{self, Call, Mock};
    use crate::c64host::UciEvents;
    use crate::devices::board::rig::{cfg, Rig};
    use crate::irq::IrqState;

    const MODE_ADDR: u32 = 0x1004_0000;
    const STOP_ADDR: u32 = 0x1004_0001;
    const STOP_MODE: u32 = 0x1004_0002;
    const CLOCK_DETECT_ADDR: u32 = 0x1004_0003;
    const TYPE_ADDR: u32 = 0x1004_0005;
    const KILL_ADDR: u32 = 0x1004_0006;
    const DMA_ADDR: u32 = 0x1005_0000;
    const REU_ENABLE_ADDR: u32 = 0x1004_0008;
    const REU_SIZE_ADDR: u32 = 0x1004_0009;
    const MATRIX_ADDR: u32 = 0x1010_0300;
    const CORE_ADDR: u32 = 0x1018_0000;
    const UCI_ADDR: u32 = 0x1004_4000;

    /// Moved here from devices/iec.rs with the UCI window: without a backend that has the block, 0x10044000 is the
    /// T0 table (00 §2 A5, 11 H6/H7/H14).
    #[test]
    fn a5_uci_buffer_bases() {
        let mut rig = Rig::new(install);
        // CommandInterface ctor (command_intf.cc:44-53).
        rig.w8(UCI_ADDR, 0x47);
        rig.w8(UCI_ADDR + 2, 0x87);
        assert_eq!([4, 5, 6, 7, 8, 9].map(|o| rig.r8(UCI_ADDR + o)), [0x00, 0x6F, 0x70, 0xDF, 0xE0, 0xFF]);
        assert_eq!((rig.r8(UCI_ADDR + 2), rig.r8(UCI_ADDR + 3)), (0, 0), "11 H7/H14");
        assert_eq!(rig.r8(UCI_ADDR), 0x46);
        // C3: IRQ mask clear at UCI task start, ISR mask set (command_intf.cc:85-96,117-118).
        assert_eq!(rig.r8(UCI_ADDR + 0x0B), 0x07);
        rig.w8(UCI_ADDR + 0x05, 0x07);
        assert_eq!(rig.r8(UCI_ADDR + 0x0B), 0x00);
        rig.w8(UCI_ADDR + 0x04, 0x05);
        assert_eq!(rig.r8(UCI_ADDR + 0x0B), 0x05);
        assert_eq!(rig.r8(UCI_ADDR + 0x04), 0x00);
        // Bus ID write does not enable the slot.
        rig.w8(UCI_ADDR + 0x01, 0x8B);
        assert_eq!(rig.r8(UCI_ADDR + 0x01), 0);
        rig.w8(UCI_ADDR + 0x01, 0x01);
        assert_eq!(rig.r8(UCI_ADDR + 0x01), 1);
        rig.w32(UCI_ADDR + 0xB80, 0x1234_5678);
        assert_eq!(rig.r32(UCI_ADDR + 0xB80), 0x1234_5678);
    }

    #[test]
    fn c12_stop_ack_immediate() {
        let mut rig = Rig::new(install);
        // C64 ctor (c64.cc:147-148), then hard_stop (c64.cc:399-409).
        rig.w8(STOP_MODE, 2);
        rig.w8(MODE_ADDR, 0);
        assert_eq!(rig.r8(STOP_ADDR) & 0x02, 0);
        rig.w8(STOP_ADDR, 1);
        assert_eq!(rig.r8(STOP_ADDR), 0x03);
        assert_eq!(rig.r8(STOP_MODE), 2);
        // resume (c64.cc:589-596).
        rig.w8(STOP_ADDR, 0);
        assert_eq!(rig.r8(STOP_ADDR), 0);
        // address(3:0) decode repeats the registers.
        rig.w8(0x1004_0011, 1);
        assert_eq!(rig.r8(STOP_ADDR), 0x03);
    }

    #[test]
    fn c14_clock_detect() {
        let mut rig = Rig::new(install);
        rig.w8(MODE_ADDR, 0x08);
        assert_eq!(rig.r8(CLOCK_DETECT_ADDR), 0x01);
        assert_eq!(rig.r8(CLOCK_DETECT_ADDR) & 0x10, 0);
        rig.w8(CLOCK_DETECT_ADDR, 0xFF);
        assert_eq!(rig.r8(CLOCK_DETECT_ADDR), 0x01);
    }

    #[test]
    fn c30_cart_latches() {
        let mut rig = Rig::new(install);
        rig.w8(MODE_ADDR, 0x04);
        assert_eq!(rig.r8(MODE_ADDR), 0x04);
        rig.w8(MODE_ADDR, 0x02);
        assert_eq!(rig.r8(MODE_ADDR), 0x06, "ULTIMAX set while reset is held");
        rig.w8(MODE_ADDR, 0x08);
        assert_eq!(rig.r8(MODE_ADDR), 0x02);
        rig.w8(MODE_ADDR, 0x10);
        assert_eq!(rig.r8(MODE_ADDR), 0x10);
        rig.w8(MODE_ADDR, 0x00);
        assert_eq!(rig.r8(MODE_ADDR), 0x00);
        assert_eq!(rig.r8(0x1004_0009), 0x07, "REU_SIZE reset");
        assert_eq!(rig.r8(0x1004_000C), 0x01, "PHI2_EDGE_RECOVER reset");
        rig.w8(TYPE_ADDR, 0x41);
        rig.w8(KILL_ADDR, 0x02);
        rig.w8(0x1004_000A, 0xFF);
        rig.w8(0x1004_000D, 0x03);
        assert_eq!(rig.r8(TYPE_ADDR), 0x41);
        assert_eq!(rig.r8(KILL_ADDR), 0, "CARTRIDGE_ACTIVE");
        assert_eq!(rig.r8(0x1004_000A), 0);
        assert_eq!(rig.r8(0x1004_000D), 0x01);
    }

    /// REU: `C64_REU_SIZE` 0..7 is the firmware's `reu_size` table (c64.cc:60).
    #[test]
    fn reu_size_is_the_firmware_table() {
        assert_eq!([0, 1, 2, 3, 4, 5, 6, 7].map(reu_size_kb), [128, 256, 512, 1024, 2048, 4096, 8192, 16384]);
        assert_eq!(reu_size_kb(0xFF), 16384, "the register is three bits (CART_REGS latches it with mask 0x07)");
    }

    /// REU: both registers reach the backend, in the order `set_emulation_flags` writes them — enable cleared, size,
    /// enable set (c64.cc:309-318) — and both still read back as the latches the FPGA has (docs/status/reu.md).
    #[test]
    fn reu_enable_and_size_reach_the_backend() {
        let mut b = Bench::new();
        assert_eq!(b.r8(REU_SIZE_ADDR), 0x07, "C64_REU_SIZE resets to 16 MB");
        assert_eq!(b.r8(REU_ENABLE_ADDR), 0x00, "and the REU is off");
        assert!(!b.mock.reu_attached());

        b.w8(REU_ENABLE_ADDR, 0);
        b.w8(REU_SIZE_ADDR, 2);
        b.w8(REU_ENABLE_ADDR, 1);
        assert_eq!(b.mock.take(), [Call::Reu(false), Call::ReuSize(512), Call::Reu(true)]);
        assert_eq!(*b.mock.reu.borrow(), Some(512), "a 512 KB REU is on the port");
        assert_eq!((b.r8(REU_SIZE_ADDR), b.r8(REU_ENABLE_ADDR)), (2, 1), "read-back is the latch");

        // The size moves under a running REU without taking it off the port (TRX64 Spec 854 D4).
        b.w8(REU_SIZE_ADDR, 7);
        assert_eq!(b.mock.take(), [Call::ReuSize(16384)]);
        assert_eq!(*b.mock.reu.borrow(), Some(16384), "still attached, now 16 MB");

        // Only bits 2:0 of the size and bit 0 of the enable are latched, so those are what the backend is told.
        b.w8(REU_SIZE_ADDR, 0xF8);
        assert_eq!((b.r8(REU_SIZE_ADDR), b.mock.take()), (0, vec![Call::ReuSize(128)]));
        b.w8(REU_ENABLE_ADDR, 0xFE);
        assert_eq!((b.r8(REU_ENABLE_ADDR), b.mock.take()), (0, vec![Call::Reu(false)]));
        assert!(!b.mock.reu_attached(), "off the port again");
    }

    #[test]
    fn c16_d012_reaches_ff() {
        let mut rig = Rig::new(install);
        let reads = (1..=u32::from(RASTER_LINES)).find(|_| rig.r8(DMA_ADDR + 0xD012) == 0xFF);
        assert_eq!(reads, Some(255));
        assert_eq!(rig.r8(DMA_ADDR + 0xD011) & 0x80, 0);
        assert!((0..RASTER_LINES).map(|_| rig.r8(DMA_ADDR + 0xD012)).any(|v| v == 0xFF), "wraps and comes back");
        let port = rig.map.get::<C64Port>().unwrap();
        assert_eq!(port.peek8(DMA + 0xD012), port.peek8(DMA + 0xD012), "peek does not advance");
        // Lines 256..311 set $D011 bit7; the written low bits stay.
        rig.w8(DMA_ADDR + 0xD011, 0x1B);
        while rig.r8(DMA_ADDR + 0xD012) != 0x00 {}
        assert_eq!(rig.r8(DMA_ADDR + 0xD011), 0x9B);
        assert_eq!(rig.r8(DMA_ADDR + 0xD019), 0);
    }

    #[test]
    fn c18_dc01_stable() {
        let mut rig = Rig::new(install);
        // Boot hotkey (u64_config.cc:950-952, keyboard_c64.cc:124-125).
        rig.w8(DMA_ADDR + 0xDC02, 0xFF);
        rig.w8(DMA_ADDR + 0xDC03, 0x00);
        rig.w8(DMA_ADDR + 0xDC00, 0x00);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFF);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), rig.r8(DMA_ADDR + 0xDC01));
        rig.w8(DMA_ADDR + 0xDC00, 0xFF);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC00), 0xFF);
        // SID probes read 0; RAM elsewhere reads back.
        rig.w8(DMA_ADDR + 0xD41D, b'S');
        assert_eq!([0xD400, 0xD401, 0xD41B, 0xD41C, 0xD51B].map(|a| rig.r8(DMA_ADDR + a)), [0; 5]);
        rig.w32(DMA_ADDR + 0x0800, 0xDEAD_BEEF);
        assert_eq!(rig.r32(DMA_ADDR + 0x0800), 0xDEAD_BEEF);
    }

    #[test]
    fn c19_matrix_wasd_32bit_store() {
        let mut rig = Rig::new(install);
        rig.w32(0x1010_030B, 0x0403_0201);
        assert_eq!([0x30B, 0x30C, 0x30D, 0x30E].map(|o| rig.r8(0x1010_0000 + o)), [1, 2, 3, 4]);
    }

    #[test]
    fn h10_cia1_port_b_scans_the_keyboard() {
        let mut rig = Rig::new(install);
        // `Keyboard_C64::scan` (keyboard_c64.cc:231-243): all rows, then one row at a time.
        rig.w8(DMA_ADDR + 0xDC00, 0x00);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFF, "no key");
        // Y is matrix (3,1) (keymap_normal, keyboard_c64.cc:27-36).
        rig.map.get_mut::<C64Port>().unwrap().set_key(3, 1, true);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFD, "all rows selected");
        rig.w8(DMA_ADDR + 0xDC00, 0xF7);
        assert_eq!((rig.r8(DMA_ADDR + 0xDC01), rig.r8(DMA_ADDR + 0xDC01)), (0xFD, 0xFD), "row 3, stable");
        assert_eq!(rig.map.get::<C64Port>().unwrap().dma_peek(0xDC01), 0xFD, "dma_peek sees the same");
        rig.w8(DMA_ADDR + 0xDC00, 0xFE);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFF, "row 0");
        assert_eq!(rig.r8(DMA_ADDR + 0xDC00), 0xFF, "port A still reads no joystick (H11)");
        rig.map.get_mut::<C64Port>().unwrap().reset();
        rig.w8(DMA_ADDR + 0xDC00, 0x00);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFD, "a held key survives a reset");
        rig.map.get_mut::<C64Port>().unwrap().set_key(3, 1, false);
        rig.map.get_mut::<C64Port>().unwrap().set_key(8, 0, true);
        assert_eq!(rig.r8(DMA_ADDR + 0xDC01), 0xFF, "released; positions outside the matrix are ignored");
    }

    #[test]
    fn m8_rom_window_readback() {
        let mut rig = Rig::new(install);
        for (base, len) in [(0x1018_8000, 0x2000), (0x1018_A000, 0x2000), (0x1018_C000, 0x1000)] {
            rig.w32(base, 0x0403_0201);
            rig.w8(base + len - 1, 0xA5);
            assert_eq!(rig.r32(base), 0x0403_0201);
            assert_eq!(rig.r8(base + len - 1), 0xA5);
        }
    }

    #[test]
    fn c30_core_config_latches() {
        let mut rig = Rig::new(install);
        assert_eq!(rig.r8(0x1018_0010), CORE_VERSION);
        rig.w8(0x1018_0010, 0x77);
        assert_eq!(rig.r8(0x1018_0010), CORE_VERSION);
        // VIDEOFORMAT (u64_memory_backend.cc:241), DMA_MEMONLY save/restore (c64.cc:728,828), BUS_INTERNAL r-m-w.
        for off in [0x01, 0x03, 0x2B] {
            rig.w8(0x1018_0000 + off, 0x2B);
            assert_eq!(rig.r8(0x1018_0000 + off), 0x2B);
        }
        rig.w8(0x1018_0080, 0x33);
        assert_eq!(rig.r8(0x1018_0080), 0, "VOICE_ADSR");
        rig.w8(0x1018_1010, 0x5A);
        assert_eq!(rig.r8(0x1018_1010), 0x5A, "PLD");
        rig.w8(0x1018_0800, 0x5A);
        assert_eq!(rig.r8(0x1018_0800), 0, "palette is write-only");
    }

    #[test]
    fn install_maps_one_port_with_absolute_offsets() {
        let mut map = IoMap::new();
        install(&mut map, &cfg());
        let port = map.resolve(0x1004_0000).unwrap().0;
        for (off, size) in WINDOWS {
            assert_eq!(map.resolve(IO_BASE + off), Some((port, off)));
            assert_eq!(map.resolve(IO_BASE + off + size - 1), Some((port, off + size - 1)));
        }
        assert_eq!(map.devices[port].name(), "c64");
        assert_ne!(map.resolve(0x1018_1000).unwrap().0, port, "PLD stays a table");
    }

    /// The installed windows with a mock backend, a full-size DDR and a settable clock.
    struct Bench {
        map: IoMap,
        irq: IrqState,
        ram: Vec<u8>,
        console: Vec<u8>,
        now: u64,
        mock: Mock,
    }

    impl Bench {
        fn new() -> Self {
            let mut map = IoMap::new();
            install(&mut map, &cfg());
            let mock = Mock::default();
            map.get_mut::<C64Port>().unwrap().attach(Box::new(mock.clone()), 0);
            assert_eq!(mock.take(), [Call::Advance(0)], "attach anchors the clock");
            Bench { map, irq: IrqState::new(), ram: vec![0; RAM_SIZE], console: Vec::new(), now: 0, mock }
        }

        fn with_ctx<R>(&mut self, addr: u32, f: impl FnOnce(&mut dyn IoDevice, u32, &mut IoCtx) -> R) -> R {
            let (dev, off) = self.map.resolve(addr).expect("mapped");
            let mut ctx =
                IoCtx { now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            f(self.map.devices[dev].as_mut(), off, &mut ctx)
        }

        fn r8(&mut self, addr: u32) -> u8 {
            self.with_ctx(addr, |dev, off, ctx| dev.read8(off, ctx))
        }

        fn w8(&mut self, addr: u32, val: u8) {
            self.with_ctx(addr, |dev, off, ctx| dev.write8(off, val, ctx));
        }

        fn port(&mut self) -> &mut C64Port {
            self.map.get_mut::<C64Port>().unwrap()
        }
    }

    #[test]
    fn backend_syncs_to_the_access_clock_first() {
        let mut b = Bench::new();
        assert_eq!(b.port().next_event(), Some(SYNC_PERIOD));
        b.now = 250;
        assert_eq!(b.r8(STOP_ADDR), 0);
        b.w8(DMA_ADDR + 0xD020, 0x0E);
        assert_eq!(b.mock.take(), [Call::Advance(250), Call::DmaWrite(0xD020, 0x0E, false)], "one sync per clock");
        b.w8(CORE_ADDR + 0x03, 1);
        b.now = 300;
        assert_eq!(b.r8(DMA_ADDR + 0x0801), !0x01);
        b.w8(STOP_ADDR, 1);
        b.w8(MATRIX_ADDR + 0x20, 0);
        assert_eq!(b.mock.take(), [Call::Advance(300), Call::DmaRead(0x0801, true), Call::Stopped(true)]);
        b.now = 400;
        b.w8(MATRIX_ADDR + 0x20, 0);
        assert_eq!(b.r8(STOP_ADDR), 0x03, "HAS_STOPPED at once");
        assert_eq!(b.port().peek8(DMA + 0xD012), !0x12, "peek reaches the backend");
        assert_eq!(b.mock.take(), [Call::Advance(400)]);
        assert_eq!(b.port().next_event(), Some(400 + SYNC_PERIOD));
        b.now = 400 + SYNC_PERIOD;
        b.with_ctx(STOP_ADDR, |dev, _, ctx| dev.tick(ctx));
        assert_eq!(b.mock.take(), [Call::Advance(400 + SYNC_PERIOD)]);
        b.w8(CORE_ADDR + 0x03, 0);
        b.w8(0x1018_8001, 0xA9);
        assert_eq!(b.r8(0x1018_A123), 0x23, "ROM windows read the backend");
        assert_eq!(b.mock.take(), [Call::RomWrite(C64Rom::Basic, 1, 0xA9)], "no sync for core config and ROMs");
    }

    /// W4-SID: core config writes reach the backend, unsynced, and stay latched.
    #[test]
    fn core_config_writes_reach_the_backend() {
        let mut b = Bench::new();
        b.w8(CORE_ADDR + 0x08, 0x40);
        b.w8(CORE_ADDR + 0x11, 1);
        assert_eq!(*b.mock.core.borrow(), [(0x08, 0x40), (0x11, 1)]);
        assert_eq!(b.r8(CORE_ADDR + 0x08), 0x40, "still a latch");
        assert!(b.mock.take().is_empty(), "no sync");
    }

    #[test]
    fn mode_edges_drive_reset_ultimax_and_nmi() {
        let mut b = Bench::new();
        b.w8(TYPE_ADDR, 0x41);
        (b.ram[CART_ROM_DDR], b.ram[CART_ROM_DDR + CART_ROM_SIZE - 1]) = (0x09, 0xC3);
        b.w8(MODE_ADDR, 0x04);
        assert_eq!(b.r8(CLOCK_DETECT_ADDR), 0x11, "RESET sense while held");
        b.w8(MODE_ADDR, 0x04);
        b.w8(MODE_ADDR, 0x02);
        assert_eq!(b.mock.take(), [Call::Reset(true), Call::Ultimax(true)]);
        b.w8(MODE_ADDR, 0x08);
        assert_eq!(b.r8(CLOCK_DETECT_ADDR), 0x01);
        assert_eq!(b.mock.take(), [Call::Cart(0x41, 0x09, 0xC3, CART_ROM_SIZE), Call::Reset(false)]);
        b.w8(MODE_ADDR, 0x10);
        b.w8(MODE_ADDR, 0x00);
        assert_eq!(b.mock.take(), [Call::Ultimax(false), Call::Nmi(true), Call::Nmi(false)]);
        // MATRIX_KEYB[9] and the host RESTORE key share the line.
        b.w8(MATRIX_ADDR + 9, 1);
        b.port().set_restore(true);
        b.w8(MATRIX_ADDR + 9, 0);
        b.port().set_restore(false);
        assert_eq!(b.mock.take(), [Call::Nmi(true), Call::Nmi(true), Call::Nmi(true), Call::Nmi(false)]);
    }

    #[test]
    fn kill_strobes_and_active() {
        let mut b = Bench::new();
        b.ram[CART_ROM_DDR] = 0x55;
        b.w8(TYPE_ADDR, 0x01);
        b.w8(KILL_ADDR, 0x02);
        b.w8(KILL_ADDR, 0x01);
        assert_eq!(b.mock.take(), [Call::Cart(0x01, 0x55, 0x00, CART_ROM_SIZE), Call::Kill]);
        assert_eq!(b.r8(KILL_ADDR), 0);
        *b.mock.active.borrow_mut() = true;
        assert_eq!(b.r8(KILL_ADDR), 1, "CARTRIDGE_ACTIVE from the backend");
        assert_eq!(b.r8(TYPE_ADDR), 0x01);
    }

    #[test]
    fn ddr_is_lent_per_access_and_eeprom_and_freeze_reach_the_backend() {
        let mut b = Bench::new();
        b.w8(TYPE_ADDR, 0x48);
        b.w8(MODE_ADDR, 0x04);
        b.w8(DMA_ADDR + 0x8000, 1);
        assert_eq!(b.r8(DMA_ADDR + 0x8000), 0xFF);
        b.w8(MATRIX_ADDR + MATRIX_FREEZE, 1);
        b.now = SYNC_PERIOD;
        b.with_ctx(STOP_ADDR, |dev, _, ctx| dev.tick(ctx));
        b.w8(0x1004_C800, 0x12);
        assert_eq!((b.r8(0x1004_C801), b.r8(0x1004_CFFF)), (0x01 ^ 0xA5, 0xFF ^ 0xA5), "offsets from 0x1004C000");
        assert_eq!(
            b.mock.take(),
            [
                Call::Reset(true),
                Call::DmaWrite(0x8000, 1, false),
                Call::DmaRead(0x8000, false),
                Call::Freeze(true),
                Call::Advance(SYNC_PERIOD),
                Call::Eeprom(0x800, 0x12),
            ]
        );
        let lent = Some(RAM_SIZE);
        assert_eq!(b.mock.leases.borrow()[1..], [lent, lent, lent, lent, lent, None], "EEPROM needs no DDR");
        assert_eq!(*b.mock.ddr.borrow(), None, "taken back after every access");
        assert_eq!(C64Port::new().peek8(EEPROM), 0, "T0: not dirty");
    }

    /// UCI (S15 §3): with a block behind the backend the firmware window is the backend's, and its lines drive ITU
    /// low bit 4 (the firmware IRQ, a level), low bit 7 (C64 reset, an edge) and high IRQ 6 (unlock).
    #[test]
    fn uci_window_and_itu_bits_follow_the_backend() {
        let mut b = Bench::new();
        assert_eq!(b.r8(UCI_ADDR + 0x06), 0x70, "no block: the T0 table still answers");
        *b.mock.uci.borrow_mut() = Some(mock::Uci::default());

        // Registers and the 2 K RAM go to the block; a peek is the same read, because `uci_read` has no side effects.
        b.w8(UCI_ADDR + 0x01, 0x47);
        b.w8(UCI_ADDR + 0x800, 0x5A);
        assert_eq!((b.r8(UCI_ADDR + 0x01), b.r8(UCI_ADDR + 0x800)), (0x47, 0x5A));
        assert_eq!(b.port().peek8(UCI + 0x800), 0x5A);
        assert_eq!(b.r8(UCI_ADDR + 0x06), 0, "the T0 table is out of the way");
        assert_eq!(b.mock.take(), [Call::Uci(0x01, 0x47), Call::Uci(0x800, 0x5A)]);

        // Low bit 4 is a level, recomputed after every access.
        b.irq.mask = 0x90;
        b.mock.uci.borrow_mut().as_mut().unwrap().irq = true;
        b.r8(UCI_ADDR + 0x03);
        assert_eq!(b.irq.active() & 0x10, 0x10);
        b.mock.uci.borrow_mut().as_mut().unwrap().irq = false;
        b.r8(UCI_ADDR + 0x03);
        assert_eq!(b.irq.active() & 0x10, 0, "a level source follows its line");

        // The events reach the ITU on the next sync, here the periodic tick.
        b.mock.uci.borrow_mut().as_mut().unwrap().events = UciEvents { c64_reset: true, unlock: true };
        b.now = SYNC_PERIOD;
        b.with_ctx(STOP_ADDR, |dev, _, ctx| dev.tick(ctx));
        assert_eq!(b.irq.active() & 0x80, 0x80, "low bit 7 latched by the C64 reset edge");
        assert_eq!(b.irq.high_src & 1 << 6, 1 << 6, "high IRQ 6 on the unlock");
        assert_eq!(b.mock.take(), [Call::Advance(SYNC_PERIOD)]);
        // `unlock_irq` acks the source with `C64_POKE(0xD038, 0)` (u64_config.cc:1012-1014); the ITU has none.
        b.w8(DMA_ADDR + 0xD038, 0);
        assert_eq!(b.irq.high_src & 1 << 6, 0);
        b.irq.clear(0x80);
        assert_eq!(b.irq.active(), 0, "one edge per reset");

        // Without a block nothing is driven and the T0 table is back.
        *b.mock.uci.borrow_mut() = None;
        b.r8(UCI_ADDR + 0x03);
        assert_eq!(b.r8(UCI_ADDR + 0x06), 0x70);
    }

    #[test]
    fn palette_matrix_keys_and_joysticks_reach_the_backend() {
        let mut b = Bench::new();
        b.w8(0x1018_0805, 0xEF);
        b.w8(0x1018_0C05, 0x12);
        b.w8(MATRIX_ADDR + 3, 0x10);
        for (i, byte) in 0x0403_0201u32.to_le_bytes().into_iter().enumerate() {
            b.w8(MATRIX_ADDR + 0x0B + i as u32, byte);
        }
        b.port().set_key(7, 7, true);
        b.port().set_joystick(0xEF);
        b.w8(CORE_ADDR + JOY2_SWOUT, 0xFE);
        b.w8(CORE_ADDR + JOY1_SWOUT, 0xE0 | 0x0B);
        let mut rows = [0; 8];
        rows[3] = 0x10;
        assert_eq!(
            b.mock.take(),
            [
                Call::Palette(5, 0xEF),
                Call::Matrix(rows),
                Call::Key(7, 7, true),
                Call::Joystick(1, 0xFF),
                Call::Joystick(2, 0xEF),
                Call::Joystick(1, 0xFF),
                Call::Joystick(2, 0xEE),
                Call::Joystick(1, 0xEB),
                Call::Joystick(2, 0xEE),
            ]
        );
        assert_eq!(b.port().frame().map(|f| f.indices), Some(vec![1, 2]));
        assert_eq!(C64Port::new().frame(), None);
    }
}
