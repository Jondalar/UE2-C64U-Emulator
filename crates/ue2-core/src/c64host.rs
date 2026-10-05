//! The C64 behind the cart/DMA registers, seen from the core: the backend trait `devices::c64::C64Port` drives and the
//! frame it publishes. No emulator types cross this boundary, so ue2-core builds and tests without one.
//! Spec: docs/specs/S14-c64-trx64.md §3.

/// U64 ROM windows (u64.h:56-61; docs/hw/10-c64-machine.md §Other C64-side windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum C64Rom {
    /// U64_BASIC_BASE 0x10188000, 8 K, mapped at $A000.
    Basic,
    /// U64_KERNAL_BASE 0x1018A000, 8 K, mapped at $E000.
    Kernal,
    /// U64_CHARROM_BASE 0x1018C000, 4 K, mapped at $D000.
    Char,
}

/// Where the firmware puts the internal cartridge's ROM in DDR (`__cart_rom_start`) and how far the cart address bits
/// reach: the FPGA's `g_rom_base_cart` and `g_max_cart_bits`, which come with the firmware (docs/status/carts.md,
/// "Cartridge ROM in DDR"). [`crate::fwlayout::cart_rom`] reads it from the loaded image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CartRom {
    pub base: usize,
    pub size: usize,
}

impl CartRom {
    /// 3.15 and later ("Large cart support", 1541ultimate 2e5c9e05, linker.x:269-287): 4 MB at 0x03C00000, 22 cart
    /// bits.
    pub const LARGE: CartRom = CartRom { base: 0x03C0_0000, size: 0x40_0000 };
    /// Before 3.15, C64 Ultimate 1.x included: 1 MB at 0x00F00000, 20 cart bits, ending where the REU starts.
    pub const SMALL: CartRom = CartRom { base: 0x00F0_0000, size: 0x10_0000 };
}

/// The character set the VIC draws the text screen with, which decides how `render::c64_text_dump` reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum C64Charset {
    /// CHAR ROM upper case and graphics (character base $1000).
    #[default]
    Upper,
    /// CHAR ROM lower and upper case (character base $1800).
    Lower,
    /// A character set in RAM, e.g. the Freeze UI's `chars.bin` at $0800 (c64.cc:916-926, 961).
    Ram,
}

/// The last complete video frame of the C64 and its text screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct C64Frame {
    pub width: usize,
    pub height: usize,
    /// `width × height` colour indices 0-15, row by row.
    pub indices: Vec<u8>,
    /// 0x00RRGGBB per colour index: C64_PALETTE 0x10180800 as the firmware wrote it (u64_config.cc:2720-2743).
    pub palette: [u32; 16],
    /// The 1000 screen codes the VIC shows: 40×25 at `$D018 >> 4 << 10` in the VIC bank.
    pub screen: Vec<u8>,
    pub charset: C64Charset,
    /// S37: where the frame sits in the VIC's raster, for the TRX composite's window: the column of the first
    /// text-area pixel, the raster line of row 0 and the lines per frame. A wrapped window (NTSC) continues past the
    /// last line with line 0. All 0 when the backend does not say; the composite then takes PAL's 32, 16 and 312.
    pub text_x: usize,
    pub first_line: usize,
    pub raster_lines: usize,
}

/// What the UCI block saw on the C64 side that the firmware cannot see for itself (TRX64 `uci::UciEvents`, Spec 852
/// D4). `devices::c64::C64Port` turns these into ITU low bit 7 and high IRQ 6 (docs/specs/S15-uci.md §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UciEvents {
    /// The C64 was reset. The firmware learns it through ITU low bit 7 and aborts a pending command
    /// (command_intf.cc `ResetInterruptHandlerCmdIf`).
    pub c64_reset: bool,
    /// `$AB` to `$D038`, then `$CD` to `$D036`. The firmware's `unlock_irq` then maps the internal bus and enables
    /// the block at `$DF1C` (u64_config.cc:1012-1019).
    pub unlock: bool,
}

/// A C64 driven by `devices::c64::C64Port` (S14 §3). Addresses are C64 bus addresses; `now` is the 100 MHz clock.
pub trait C64Backend {
    /// Run the C64 up to `now` (monotonic). The first call anchors the C64 clock to `now`. While stopped only the
    /// chips run, while the reset is held only the VIC (S14 §4).
    fn advance_to(&mut self, now: u64);
    /// Hold (`true`) or release the reset line (C64_MODE bits 2/3). A release warm-resets the C64 with the cartridge
    /// last given to [`C64Backend::set_cart`].
    fn set_reset(&mut self, held: bool);
    /// Hold the 6510 (C64_STOP bit 0) while VIC, CIAs and SID keep running.
    fn set_stopped(&mut self, stopped: bool);
    /// Force the ULTIMAX decode of the freezer cartridge (C64_MODE bit 1).
    fn set_ultimax(&mut self, on: bool);
    /// NMI line level: C64_MODE bit 4, MATRIX_KEYB[9] and the host RESTORE key together.
    fn set_nmi(&mut self, level: bool);
    /// One DMA read (0x10050000 + addr). `mem_only`: C64_DMA_MEMONLY, RAM without I/O or ROM.
    fn dma_read(&mut self, addr: u16, mem_only: bool) -> u8;
    /// One DMA write (0x10050000 + addr).
    fn dma_write(&mut self, addr: u16, val: u8, mem_only: bool);
    /// What a DMA read would return, without side effects (debugger).
    fn dma_peek(&self, addr: u16) -> u8;
    fn rom_write(&mut self, rom: C64Rom, off: u16, val: u8);
    fn rom_read(&self, rom: C64Rom, off: u16) -> u8;
    /// (Re)build the cartridge from C64_CARTRIDGE_TYPE (type | variant, c64.h:115-163) and the cart ROM copied to
    /// DDR at the [`CartRom`] base (`rom`, 16 K). The cartridge starts enabled.
    fn set_cart(&mut self, type_variant: u8, rom: &[u8]);
    /// Where the cartridge ROM is in DDR for this firmware; given on attach and before any [`C64Backend::set_cart`].
    fn set_cart_rom(&mut self, _rom: CartRom) {}
    /// C64_CARTRIDGE_KILL bit 0: disable the cartridge.
    fn kill_cart(&mut self);
    /// C64_CARTRIDGE_ACTIVE bit 0.
    fn cart_active(&self) -> bool;
    /// C64_PALETTE RGB byte `off` of 16 × {R,G,B,pad}.
    fn set_palette_byte(&mut self, off: u8, val: u8);
    /// Host key at the `U64Io::set_key` matrix position.
    fn set_key(&mut self, row: u8, col: u8, down: bool);
    /// MATRIX_KEYB [0..7]: bit b of `rows[a]` = key (a, b) pressed, ORed with the host keys (keyboard_usb.cc:214-229).
    fn set_matrix_keyb(&mut self, rows: [u8; 8]);
    /// Joystick port 1 or 2, lines active low (bit 0 up, 1 down, 2 left, 3 right, 4 fire).
    fn set_joystick(&mut self, port: u8, lines: u8);
    fn frame(&self) -> C64Frame;
    /// The VIC's own frame counter, so a caller can see a new picture without building one. 0 means the backend
    /// does not count frames, and nothing that watches for a change will ever fire (S24 §4).
    fn frame_counter(&self) -> u64 {
        0
    }
    /// The UDP audio stream was enabled or disabled (S24 §1 M3). A backend that can produce samples starts or
    /// stops doing so here; the default does nothing, so a machine without one simply streams no audio.
    fn set_stream_audio(&mut self, _on: bool) {}
    /// Interleaved stereo samples collected since the last call, for the audio stream.
    fn take_stream_audio(&mut self) -> Vec<i16> {
        Vec::new()
    }

    // --- W4-SID (docs/specs/S14-c64-trx64.md §W4-SID) ---
    /// C64 core config latch 0x10180000 + `off` was written (u64.h:104-154). The SID decode and UltiSID settings reach
    /// the backend here. Ignored by default.
    fn core_config_write(&mut self, _off: u8, _val: u8) {}

    // --- Cartridges beyond CART_TYPE_NORMAL (W4-CART, docs/status/carts.md) ---

    /// Lend guest DDR (64 MB) for the backend calls that follow, or take it back with `None`. `C64Port` lends
    /// `IoCtx::ram` at the start of every access that can run the C64 or reach its bus and takes it back before the
    /// access returns, so the cartridge logic reads its ROM at 0x03C00000 and its RAM at 0x00EF0000 live, as the FPGA
    /// does. A backend must not use the memory once it is taken back. Default: ignored.
    fn lend_ddr(&mut self, _ddr: Option<&mut [u8]>) {}
    /// EEPROM_BASE 0x1004C000 + `off`: the dirty flag below +0x800, the 2 K GMOD2 EEPROM above (microwire_eeprom.vhd).
    fn eeprom_read(&self, _off: u16) -> u8 {
        0
    }
    fn eeprom_write(&mut self, _off: u16, _val: u8) {}
    /// The freeze button of freezer cartridges, MATRIX_KEYB[10] (freezer.vhd; keyboard_usb.cc:228).
    fn set_freeze_button(&mut self, _down: bool) {}

    // ---- REU: the RAM Expansion Unit (docs/status/reu.md) ----

    /// C64_REU_ENABLE (cart regs +0x8, c64.h:62): put the REU on the C64's expansion port, or take it off. The
    /// firmware sets it from its "RAM Expansion Unit" setting at every cart init (c64.cc:315-317) and expects it to
    /// take effect on a running machine.
    ///
    /// The REU's RAM is not the REU's: it is the DDR at `REU_MEMORY_BASE 0x1000000` (c64.h:14) that
    /// [`C64Backend::lend_ddr`] lends, where the firmware preloads images with its own CPU (reu_preloader.cc:104). So
    /// attaching keeps whatever is already there and detaching leaves it alone. Ignored by default.
    fn set_reu_enabled(&mut self, _on: bool) {}
    /// C64_REU_SIZE (cart regs +0x9, c64.h:63) as KiB: `128 << n` for n = 0..7, the firmware's `reu_size` table
    /// (c64.cc:60), written at c64.cc:315. Applied to an REU that is already on the port without disturbing its
    /// contents; an REU attached later starts at the last size given. Ignored by default.
    fn set_reu_size_kb(&mut self, _size_kb: u32) {}
    /// Whether the REU is on the port now. C64_REU_ENABLE itself reads back the latch, as the FPGA does; this is for
    /// tests and status.
    fn reu_attached(&self) -> bool {
        false
    }

    // ---- W4-DRIVE (docs/specs/S14-c64-trx64.md §W4-DRIVE) ----
    /// Disk drive `unit` (0 = drive A) on this C64's IEC bus, or None if the backend has none.
    fn drive(&mut self, _unit: u8) -> Option<&mut dyn C64Drive> {
        None
    }

    // ---- CARTSLOT: a physical cartridge in the expansion port (docs/status/cart-slot.md) ----
    /// U64_CART_DETECT (0x10100403): bit 0 GAME, bit 1 EXROM of the external expansion port, 1 = released
    /// (c64.cc:1514). [`CART_DETECT_NONE`] without a cartridge in the port.
    fn cart_detect(&self) -> u8 {
        CART_DETECT_NONE
    }
    /// The cartridge in the physical expansion port, or None if the port is empty.
    fn cart_slot(&mut self) -> Option<&mut dyn C64CartSlot> {
        None
    }

    // ---- UCI: the Ultimate Command Interface (docs/specs/S15-uci.md) ----

    /// Whether this backend serves the UCI block. TRX64 has it on the `u64` profile, where it is part of the machine
    /// (Spec 852). Without it `devices::c64::C64Port` keeps the T0 register table at 0x10044000.
    /// Downcast hook for a frontend that needs more of a backend than this trait offers — the monitor host of S23
    /// reaches the TRX64 machine through it. A backend returns itself; `ue2-core` stays ignorant of what that is.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    fn has_uci(&self) -> bool {
        false
    }

    // ---- S30: the IEC processor at `IEC_BASE` 0x10028000 (docs/specs/S30-soft-iec.md) ----
    /// Whether this backend has the IEC processor on its bus. Without one the window stays the T0 table.
    fn has_iec(&self) -> bool {
        false
    }
    /// A firmware read of the processor's register `off`: reading the up FIFO's data takes the entry.
    fn iec_read(&mut self, _off: u16) -> u8 {
        0
    }
    /// Register `off` without side effects.
    fn iec_peek(&self, _off: u16) -> u8 {
        0
    }
    fn iec_write(&mut self, _off: u16, _val: u8) {}
    /// A read of `CMD_IF_BASE` 0x10044000 + `off`: the sixteen registers below 0x800, the 2 K dual-port RAM above
    /// (docs/hw/11 §UltiCommand). Side-effect free, as TRX64's `Uci::fw_read` is — none of the firmware-side reads
    /// in `command_protocol.vhd` has one.
    fn uci_read(&self, _off: u16) -> u8 {
        0
    }
    fn uci_write(&mut self, _off: u16, _val: u8) {}
    /// ITU low bit 4: `(handshake_in AND NOT irq_mask) != 0`, a level (command_protocol.vhd:310; itu.h:37).
    fn uci_irq(&self) -> bool {
        false
    }
    /// The C64-side events since the last call (TRX64 `Uci::take_events`).
    fn uci_take_events(&mut self) -> UciEvents {
        UciEvents::default()
    }

    // ---- Ultimate Audio: the sampler (docs/specs/S16-ultimate-audio.md) ----

    /// Whether this backend serves the sampler. Without it the window at `SAMPLER_BASE` 0x10048000 stays RAZ/WI, which
    /// is what it was before S16: the firmware never reads it, and its reset writes are swallowed.
    fn has_sampler(&self) -> bool {
        false
    }
    /// A read of `SAMPLER_BASE` 0x10048000 + `off`. Only bit 0 of the offset is decoded: even is the IRQ status vector,
    /// odd the version constant 0x10 (`sampler_regs.vhd:81-87`). Side-effect free — reading clears no latch — so
    /// `peek8` uses it too.
    fn sampler_read(&self, _off: u16) -> u8 {
        0
    }
    fn sampler_write(&mut self, _off: u16, _val: u8) {}
    /// `C64_SAMPLER_ENABLE` (cart regs +0xE, c64.h:68): map the block at `$DF20-$DFFF` for the C64, or take it away.
    /// The register itself stays the latch the firmware reads back (c64.cc:1362).
    fn set_sampler_enabled(&mut self, _on: bool) {}

    // ---- UltiSID: the audio mixer (docs/specs/S17-ultisid.md) ----

    /// `U64_AUDIO_MIXER` 0x10100500 + `off`, `off` < 20: two bytes per channel, 0 UltiSID 1, 1 UltiSID 2, 2 socket 1,
    /// 3 socket 2, 4-5 sampler, 6-7 drives, 8-9 tape (u64_config.cc:1349-1358). Write-only.
    fn mixer_write(&mut self, _off: u8, _val: u8) {}

    // ---- S37 phase B: TRX64-Ultimate's machine pause and freeze session (docs/specs/S37-trx-pixel-layer.md §5) ----

    /// The whole machine stands (top.v `c64_halt_req`): no C64 cycle runs, so CPU, VIC, CIAs, SID and the drives
    /// stand and the SID is silent. A release runs on from the same cycle; the clock is re-anchored at the release.
    /// The DMA window, keys and ROM windows work while paused. Ignored by default.
    fn set_paused(&mut self, _paused: bool) {}
    /// TRX64-Ultimate's `c64_mute` (top.v:1059, `c64_halted | C64_STOP`): from the C64's current cycle on, the audio
    /// output — the device, the WAV and the UDP stream — carries zeros, cut without a ramp, until it is cleared. The
    /// chips are not touched: under C64_STOP alone they run on beneath the cut. Gideon's bitstream has no such cut,
    /// so only `--trx-io` calls it. Ignored by default.
    fn set_muted(&mut self, _muted: bool) {}
    // ---- S39 phase A1 / S40: the board's state bus and engine (docs/specs/S40-trx-engine.md) ----

    /// The state bus's shadow words as the machine stands now (a capture while halted). Default: all 0.
    fn sb_shadow(&self) -> SbShadow {
        SbShadow::default()
    }
    /// Words 1.3-1.5 now: `{IB, opcode at IPC, IPC}`, `{S, Y, X, A}`, `{P}` of the settled registers.
    fn sb_live(&self) -> [u32; 3] {
        [0; 3]
    }
    /// What the engine wants recorded and stopped at from now on. Ignored by default.
    fn set_engine(&mut self, _cfg: EngineCfg) {}
    /// The engine's events since the last call, in order.
    fn take_tap(&mut self) -> Vec<TapEvent> {
        Vec::new()
    }
    /// `STOP_POS`: stop on line 0 at raster cycle `Some(c)` and hold the whole machine there; `None` disarms.
    fn set_stop_pos(&mut self, _cycle: Option<u8>) {}
    /// `STOP_POS` b9: let go of the position hold.
    fn release_pos(&mut self) {}
    /// `SB_LOAD`: the staged hidden-state words load into the chips now, before the machine moves on.
    fn sb_load(&mut self, _load: &crate::devices::trx_sb::SbLoad) {}
    /// APPLY: a byte into C64 RAM through the RAM's second port, logged by nothing.
    fn apply_ram(&mut self, addr: u16, val: u8) {
        self.dma_write(addr, val, true);
    }
    /// The engine's cycle counter (phi2 edges the C64 ran).
    fn engine_cycles(&self) -> u64 {
        0
    }

    /// S40 §0: the C64 sits on TRX64-Ultimate's bitstream (`--trx-io`), whose `main` RTL has no turbo: the CPU runs at
    /// 1 MHz whatever the firmware's menu sets, and `$D030`/`$D031` are the VIC's unused registers (read `$FF`, take no
    /// write). Ignored by default.
    fn set_trx_bitstream(&mut self, _on: bool) {}
    /// One VIC frame in emulator clocks (lines × cycles per line at the row's clock), 0 when unknown.
    fn frame_clocks(&self) -> u64 {
        0
    }
    /// Raster lines per VIC frame, 0 when unknown.
    fn frame_lines(&self) -> usize {
        0
    }
    /// `$D000-$DFFF` as a read shows it, without its side effects and whatever the PLA maps there (the state bus of
    /// c64_core.v: raster, collisions and IRQ flags not cleared, timers and TOD as they stand).
    fn io_peek(&self, _addr: u16) -> u8 {
        0xFF
    }
    /// C64 RAM itself, whatever the PLA maps at `addr`.
    fn ram_peek(&self, addr: u16) -> u8 {
        self.dma_peek(addr)
    }
    /// The 6510 port: (`$00` direction, `$01` output latch).
    fn cpu_port(&self) -> (u8, u8) {
        (0x2F, 0x37)
    }
    /// The cartridge's own lines before a forced Ultimax: (present, EXROM, GAME), true = high (c64_core.v's glue
    /// word). No cartridge: (false, true, true).
    fn cart_lines(&self) -> (bool, bool, bool) {
        (false, true, true)
    }
    /// CIA 1 (`cia` 0) or CIA 2 (1): the register file as written (PRA, PRB, DDRA, DDRB, …).
    fn cia_regs(&self, _cia: u8) -> [u8; 16] {
        [0; 16]
    }
    /// CIA 1's port A and B as a read shows them with these port registers, the keys and the joysticks.
    fn cia1_ports(&self, pra: u8, ddra: u8, prb: u8, ddrb: u8) -> (u8, u8) {
        (pra | !ddra, prb | !ddrb)
    }
    /// A cartridge read of `addr` (ROML, ROMH, IO1, IO2) without its side effects.
    fn cart_peek(&self, _addr: u16) -> CartPeek {
        CartPeek::Miss
    }
    /// Real bus writes to `$D000-$DFFF` with I/O mapped whatever the CPU port says (freeze_resp.v's commit: Ultimax
    /// forced), in this order.
    fn io_commit(&mut self, writes: &[(u16, u8)]) {
        for &(addr, val) in writes {
            self.dma_write(addr, val, false);
        }
    }
}

/// S39/S40: the C64's state as the board's state bus shows it at a capture (`c64_core.v:575-612`): the shadow words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SbShadow {
    /// Chip 0 b3:1: the cartridge's own lines `{present, EXROM, GAME}`, 1 = high.
    pub cart: u8,
    /// Chip 1 w0 `{S, Y, X, A}`, w1 `{sync, P, PC}`, w2 `{$01 as read, $01 latch, $00}`.
    pub cpu: [u32; 3],
    /// Chip 2 w0-w11: `$D000-$D02F` as a read returns them.
    pub vic: [u32; 12],
    /// Chips 3, 4 w0-w3: registers 0-15 as read; w4 `{DDRB, DDRA, PRB, PRA}`.
    pub cia: [[u32; 5]; 2],
    /// Chip 7 w0 `{reg_1, reg_0}` of icepi's cartridge model.
    pub bank: u32,
    /// Chip 1 w8-w14: the 6502N's 209 flip-flops.
    pub cpuf: [u32; 7],
    /// Chips 3, 4 w8-w19: each reDIP-CIA's 363 flip-flops.
    pub ciaf: [[u32; 12]; 2],
}

/// S39/S40: a snap candidate the C64 stood on (`c64_core.v:643-644`): the first instruction boundary on line 0 from
/// cycle 12 where the conditions hold, with the machine running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snap {
    /// The engine's cycle counter at the boundary, before the opcode fetch the board's snap edge ends.
    pub cycle: u64,
    /// F3: the VIC's x (8 × cycle + 7) and line.
    pub x: u16,
    pub line: u16,
    /// The next instruction is an interrupt's: the fetch is dropped (`mk_intg`).
    pub intg: bool,
    pub shadow: SbShadow,
    /// Words 1.3-1.5 at the snap.
    pub live: [u32; 3],
}

/// S40: what the C64 did that the board's engine taps see, in order (docs/specs/S40-trx-engine.md §4, §10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapEvent {
    /// A write that landed in the 64 KB RAM, with the RAM's own old byte: the machine's port A, or `outside` (the
    /// RAM's second port: MEMONLY, the session, a restore behind the firmware).
    Ram { addr: u16, old: u8, new: u8, outside: bool, cycle: u64 },
    /// A write into colour RAM (`$D800-$DBFF` with I/O visible), nibbles; `outside`: the second port.
    Colour { addr: u16, old: u8, new: u8, outside: bool, cycle: u64 },
    /// The cartridge logic wrote its RAM at `0xEF0000 + off`.
    CartRam { off: u16, old: u8, new: u8, cycle: u64 },
    /// The REU moved data.
    Reu,
    Snap(Box<Snap>),
    /// The VIC reached line 16 (`l16`): the boundary of a frame that had no snap.
    Line16 { cycle: u64, x: u16 },
    /// The machine stood at the stop position (`pos_hit`) and holds.
    PosHit,
}

/// S40: what the board's engine wants of the C64's run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineCfg {
    /// Snap candidates and line 16: the log or the capture cadence is on.
    pub boundaries: bool,
    /// Memory writes: the log is on.
    pub records: bool,
}

/// S37: what the cartridge answers to a read without its side effects (freeze_resp.v `pk_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CartPeek {
    /// The cartridge or a device on the port drives the read.
    Hit(u8),
    /// Nothing drives it.
    Miss,
    /// The cartridge is on the physical port: no read without a bus cycle.
    External,
}

// ---- CARTSLOT: a physical cartridge in the expansion port (docs/status/cart-slot.md) ----

/// U64_CART_DETECT with an empty expansion port: GAME and EXROM pulled up, `(v & 3) == 3` (c64.cc:1514).
pub const CART_DETECT_NONE: u8 = 0x03;

/// What `cart-info` reports about the physical cartridge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CartSlotInfo {
    /// Name from the CRT header.
    pub name: String,
    /// CRT hardware type (header offset 0x16).
    pub hw_type: u16,
    /// Cartridge family, e.g. "EasyFlash".
    pub family: String,
    /// The model serving it: "trx64" (TRX64's mapper), "trx64-flash" (a flash board on TRX64's chip models) or
    /// "u64-logic" (the ported all_carts_v5.vhd).
    pub model: String,
    /// Banks the cartridge holds.
    pub banks: usize,
    /// EXROM and GAME as the cartridge drives them, 1 = released.
    pub exrom: u8,
    pub game: u8,
    /// C64_BUS_INTERNAL and C64_BUS_EXTERNAL (0x1018002B/2C) as last written: bit 0 IO1, 1 IO2, 2 ROM, 3 IRQ.
    pub bus_internal: u8,
    pub bus_external: u8,
    /// C64_BUS_BRIDGE (0x1018002A) as last written: bit 0 mirrors writes to the expansion port.
    pub bus_bridge: u8,
    /// The flash command decode of a flash cartridge: "11", "15" or "both"; None for ROM cartridges.
    pub flash_decode: Option<String>,
    /// Flash or EEPROM changed since the cartridge was inserted.
    pub dirty: bool,
    /// Mutation counter of flash and EEPROM.
    pub generation: u64,
    /// The cartridge has flash or EEPROM that a CRT written by [`C64CartSlot::crt_image`] carries.
    pub writable: bool,
}

/// A cartridge in the physical expansion port, independent of the internal cartridge emulation.
pub trait C64CartSlot {
    fn info(&self) -> CartSlotInfo;
    /// The cartridge as it is now, as a CRT: the inserted header and chip packets with their data taken from the
    /// cartridge (flash, EEPROM), plus packets for flash banks the inserted CRT did not have that are no longer erased.
    fn crt_image(&mut self) -> Result<Vec<u8>, String>;
    /// Mutation counter of flash and EEPROM; a change means `crt_image` differs.
    fn generation(&self) -> u64;
}

// ---- W4-DRIVE: a disk drive on the C64's IEC bus (docs/specs/S14-c64-trx64.md §W4-DRIVE) ----

/// Half-track positions of a 1541 head on side 0: `floppy_stream.vhd` stops the stepper at 83.
pub const DRIVE_HALF_TRACKS: usize = 84;

/// What the firmware drives through the drive registers (drive_registers.vhd; docs/hw/11 §Drive A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveLines {
    /// POWER bit 0: the drive CPU is clocked and may pull the IEC lines (c1541_timing.vhd, mm_drive_cpu.vhd:743-750).
    pub power: bool,
    /// RESET bit 0: the drive CPU is held in reset. A release starts it from its ROM.
    pub reset: bool,
    /// RESET bit 1: the C64's reset resets the drive too (c1541_timing.vhd `use_c64_reset`).
    pub follow_c64_reset: bool,
    /// RESET bit 2: the drive stops while the C64 is stopped (mm_drive.vhd `drive_stop and stop_on_freeze`).
    pub stop_on_freeze: bool,
    /// HW_ADDR bits 1:0: device 8 + n, the VIA1 PB5/PB6 jumpers.
    pub device: u8,
    /// SENSOR bit 0 clear: the write-protect photo sensor is dark.
    pub write_protect: bool,
    /// DRIVETYPE bits 1:0: 0 = 1541, 1 = 1571, 2 = 1581.
    pub drive_type: u8,
    /// S31, for a 1581's mechanism: INSERTED bit 0 (a disk in), DISKCHANGE bit 0 (/DISK CHANGE asserted) and bit 1
    /// (force ready) (drive_registers.vhd:170-177; mm_drive.vhd:269 `rdy_n`).
    pub inserted: bool,
    pub disk_change: bool,
    pub force_ready: bool,
}

impl Default for DriveLines {
    /// The register reset values (drive_registers.vhd reset branch): off, held in reset, sensor dark.
    fn default() -> Self {
        DriveLines {
            power: false,
            reset: true,
            follow_c64_reset: true,
            stop_on_freeze: true,
            device: 0,
            write_protect: true,
            drive_type: 0,
            inserted: false,
            disk_change: false,
            force_ready: false,
        }
    }
}

/// Head, motor and LED of a drive, as the drive registers report them (drive_registers.vhd read branch).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveStatus {
    /// Head position in half-tracks, 0 = track 1 (TRACK).
    pub half_track: u8,
    /// Spindle motor on, VIA2 PB2 (STATUS bit 0).
    pub motor: bool,
    /// Head in write mode, VIA2 CB2 low (STATUS bit 1).
    pub writing: bool,
    /// Activity LED, VIA2 PB3. No register reports it.
    pub led: bool,
    /// SIDE bit 0: a 1581's CIA PA0 as it stands (mm_drive_cpu.vhd:556); a 1541 reports 0.
    pub side: u8,
}

/// A drive driven by `devices::drives::DriveRegs`. The firmware owns the disk: it hands the drive GCR bytes per
/// half-track and takes back what the drive wrote.
pub trait C64Drive {
    /// Apply the firmware's lines. A reset release (RESET bit 0, or the C64's reset with `follow_c64_reset`) starts
    /// the drive CPU from `rom`, the 32 K image at drive area + 0x8000 that the CPU sees at $8000-$FFFF
    /// (c1541.cc:929-940).
    fn set_lines(&mut self, lines: DriveLines, rom: &[u8]);
    /// The surface at `half_track` (side 0) now holds `gcr`, one byte per 8 bit cells, wrapping at its end.
    fn set_track(&mut self, half_track: u8, gcr: &[u8]);
    /// The surface at `half_track`, including what the drive wrote.
    fn track(&self, half_track: u8) -> &[u8];
    /// Half-tracks the drive may have written since the last call, bit n = half-track n.
    fn take_written(&mut self) -> u128;
    fn status(&self) -> DriveStatus;
    /// Copy the drive CPU's RAM from $0000 into `out` (at most $0800 bytes).
    fn read_ram(&self, out: &mut [u8]);

    // ---- S31: a 1581's WD177x at drive window + 0x1800 (wd177x.vhd `io_req`) ----
    /// Whether the drive is a 1581 whose controller serves the WD window. Without one the window stays the T0 table.
    fn has_wd(&self) -> bool {
        false
    }
    /// The firmware's WD register `off` (0-15). Side-effect free: popping the command FIFO is a write.
    fn wd_read(&self, _off: u16) -> u8 {
        0
    }
    /// A firmware write of WD register `off`; `ram` is guest DDR, which the DMA reads and writes.
    fn wd_write(&mut self, _off: u16, _val: u8, _ram: &mut [u8]) {}
    /// The command FIFO holds an entry: ITU high IRQ 1 (drive A) or 2 (drive B) (wd177x.cc:63).
    fn wd_irq(&self) -> bool {
        false
    }
}

/// A [`C64Backend`] that records calls, for device and machine tests.
#[cfg(test)]
pub(crate) mod mock {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::{C64Backend, C64Frame, C64Rom, CartRom, UciEvents};

    /// A stand-in for TRX64's UCI block: the firmware window reads back what was written, and the test drives the
    /// IRQ level and the events by hand.
    #[derive(Clone)]
    pub(crate) struct Uci {
        /// The 0x1000-byte firmware window (registers below 0x800, RAM above).
        pub(crate) mem: Vec<u8>,
        /// What `uci_irq` reports (ITU low bit 4).
        pub(crate) irq: bool,
        /// Taken by the next `uci_take_events`.
        pub(crate) events: UciEvents,
    }

    impl Default for Uci {
        fn default() -> Self {
            Uci { mem: vec![0; 0x1000], irq: false, events: UciEvents::default() }
        }
    }

    /// A stand-in for the sampler's register file: writes are kept so a test can see what reached it, and reads
    /// follow the hardware's two decodes — even offsets the status vector, odd ones the version byte.
    #[derive(Clone)]
    pub(crate) struct Sampler {
        /// The 256-byte register file, as written.
        pub(crate) regs: Vec<u8>,
        /// What an even offset reads back.
        pub(crate) status: u8,
        /// Whether `C64_SAMPLER_ENABLE` has mapped it for the C64.
        pub(crate) enabled: bool,
    }

    impl Default for Sampler {
        fn default() -> Self {
            Sampler { regs: vec![0; 0x100], status: 0, enabled: false }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        Advance(u64),
        Reset(bool),
        Stopped(bool),
        Ultimax(bool),
        Nmi(bool),
        DmaRead(u16, bool),
        DmaWrite(u16, u8, bool),
        RomWrite(C64Rom, u16, u8),
        /// Type, first and last byte and length of the cart ROM.
        Cart(u8, u8, u8, usize),
        CartRom(CartRom),
        Kill,
        Palette(u8, u8),
        Key(u8, u8, bool),
        Matrix([u8; 8]),
        Joystick(u8, u8),
        /// EEPROM window write (offset, value).
        Eeprom(u16, u8),
        Freeze(bool),
        /// C64_REU_ENABLE.
        Reu(bool),
        /// C64_REU_SIZE, in KiB.
        ReuSize(u32),
        /// UCI firmware-side write (offset, value).
        Uci(u16, u8),
        /// Sampler firmware-side write (offset, value).
        Sampler(u16, u8),
        /// C64_SAMPLER_ENABLE.
        SamplerEnable(bool),
        /// U64_AUDIO_MIXER write (offset, value).
        Mixer(u8, u8),
        /// S37: the machine pause.
        Paused(bool),
    }

    /// Shared with the test after the backend moved into the device.
    #[derive(Clone, Default)]
    pub(crate) struct Mock {
        pub(crate) calls: Rc<RefCell<Vec<Call>>>,
        pub(crate) active: Rc<RefCell<bool>>,
        /// `core_config_write` calls, kept apart from `calls` (W4-SID).
        pub(crate) core: Rc<RefCell<Vec<(u8, u8)>>>,
        /// Length of the DDR lent right now.
        pub(crate) ddr: Rc<RefCell<Option<usize>>>,
        /// The lent DDR length at each recorded call, in call order.
        pub(crate) leases: Rc<RefCell<Vec<Option<usize>>>>,
        /// What `cart_detect` returns; None is the trait default (CARTSLOT).
        pub(crate) detect: Rc<RefCell<Option<u8>>>,
        /// UCI: `Some` when the mock has a block, so `has_uci` is true (S15).
        pub(crate) uci: Rc<RefCell<Option<Uci>>>,
        /// REU: its size in KiB while it is on the port, else None.
        pub(crate) reu: Rc<RefCell<Option<u32>>>,
        /// REU: the last `set_reu_size_kb`, which an attach takes.
        pub(crate) reu_size: Rc<RefCell<u32>>,
        /// Sampler: `Some` when the mock has the block, so `has_sampler` is true (S16).
        pub(crate) sampler: Rc<RefCell<Option<Sampler>>>,
        /// S37: what `frame_counter` returns.
        pub(crate) frames: Rc<RefCell<u64>>,
        /// S37: what `frame_lines` returns, and the frame's `raster_lines` (0: unknown).
        pub(crate) lines: Rc<RefCell<usize>>,
        /// S37: `set_muted` calls, kept apart from `calls`.
        pub(crate) mutes: Rc<RefCell<Vec<bool>>>,
        /// S40: the last `set_trx_bitstream`.
        pub(crate) trx_bitstream: Rc<RefCell<bool>>,
        /// S40: the events `take_tap` hands out next, and what the engine asked for, kept apart from `calls`.
        pub(crate) tap: Rc<RefCell<Vec<super::TapEvent>>>,
        pub(crate) engine: Rc<RefCell<Vec<super::EngineCfg>>>,
        /// S40: `set_stop_pos` (Some(cycle) or None) and `release_pos` (as Err(())) in order.
        pub(crate) pos: Rc<RefCell<Vec<Result<Option<u8>, ()>>>>,
        /// S40: the loads `sb_load` got, and what `sb_shadow` returns.
        pub(crate) loads: Rc<RefCell<Vec<crate::devices::trx_sb::SbLoad>>>,
        pub(crate) shadow: Rc<RefCell<super::SbShadow>>,
    }

    impl Mock {
        /// Calls recorded since the last `take`.
        pub(crate) fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.calls.borrow_mut())
        }

        fn push(&self, call: Call) {
            self.calls.borrow_mut().push(call);
            self.leases.borrow_mut().push(*self.ddr.borrow());
        }
    }

    impl C64Backend for Mock {
        fn advance_to(&mut self, now: u64) {
            self.push(Call::Advance(now));
        }
        fn set_reset(&mut self, held: bool) {
            self.push(Call::Reset(held));
        }
        fn set_stopped(&mut self, stopped: bool) {
            self.push(Call::Stopped(stopped));
        }
        fn set_ultimax(&mut self, on: bool) {
            self.push(Call::Ultimax(on));
        }
        fn set_nmi(&mut self, level: bool) {
            self.push(Call::Nmi(level));
        }
        /// Returns the low address byte inverted.
        fn dma_read(&mut self, addr: u16, mem_only: bool) -> u8 {
            self.push(Call::DmaRead(addr, mem_only));
            !(addr as u8)
        }
        fn dma_write(&mut self, addr: u16, val: u8, mem_only: bool) {
            self.push(Call::DmaWrite(addr, val, mem_only));
        }
        fn dma_peek(&self, addr: u16) -> u8 {
            !(addr as u8)
        }
        fn rom_write(&mut self, rom: C64Rom, off: u16, val: u8) {
            self.push(Call::RomWrite(rom, off, val));
        }
        /// Returns the low offset byte.
        fn rom_read(&self, _rom: C64Rom, off: u16) -> u8 {
            off as u8
        }
        fn set_cart(&mut self, type_variant: u8, rom: &[u8]) {
            self.push(Call::Cart(type_variant, rom[0], rom[rom.len() - 1], rom.len()));
        }
        fn set_cart_rom(&mut self, rom: CartRom) {
            self.push(Call::CartRom(rom));
        }
        fn kill_cart(&mut self) {
            self.push(Call::Kill);
        }
        fn cart_active(&self) -> bool {
            *self.active.borrow()
        }
        fn set_palette_byte(&mut self, off: u8, val: u8) {
            self.push(Call::Palette(off, val));
        }
        fn set_key(&mut self, row: u8, col: u8, down: bool) {
            self.push(Call::Key(row, col, down));
        }
        fn set_matrix_keyb(&mut self, rows: [u8; 8]) {
            self.push(Call::Matrix(rows));
        }
        fn set_joystick(&mut self, port: u8, lines: u8) {
            self.push(Call::Joystick(port, lines));
        }
        /// A 2×1 frame of indices 1 and 2.
        fn frame(&self) -> C64Frame {
            let mut palette = [0; 16];
            (palette[1], palette[2]) = (0x0011_2233, 0x0044_5566);
            let raster_lines = *self.lines.borrow();
            C64Frame {
                width: 2,
                height: 1,
                indices: vec![1, 2],
                palette,
                screen: vec![0x20; 1000],
                raster_lines,
                ..C64Frame::default()
            }
        }
        fn core_config_write(&mut self, off: u8, val: u8) {
            self.core.borrow_mut().push((off, val));
        }
        fn lend_ddr(&mut self, ddr: Option<&mut [u8]>) {
            *self.ddr.borrow_mut() = ddr.map(|d| d.len());
        }
        /// Returns the low offset byte xor 0xA5.
        fn eeprom_read(&self, off: u16) -> u8 {
            off as u8 ^ 0xA5
        }
        fn eeprom_write(&mut self, off: u16, val: u8) {
            self.push(Call::Eeprom(off, val));
        }
        fn set_freeze_button(&mut self, down: bool) {
            self.push(Call::Freeze(down));
        }

        fn set_reu_enabled(&mut self, on: bool) {
            let size = *self.reu_size.borrow();
            *self.reu.borrow_mut() = on.then_some(size);
            self.push(Call::Reu(on));
        }
        fn set_reu_size_kb(&mut self, size_kb: u32) {
            *self.reu_size.borrow_mut() = size_kb;
            if let Some(kb) = self.reu.borrow_mut().as_mut() {
                *kb = size_kb;
            }
            self.push(Call::ReuSize(size_kb));
        }
        fn reu_attached(&self) -> bool {
            self.reu.borrow().is_some()
        }
        fn cart_detect(&self) -> u8 {
            self.detect.borrow().unwrap_or(super::CART_DETECT_NONE)
        }

        fn has_uci(&self) -> bool {
            self.uci.borrow().is_some()
        }
        fn uci_read(&self, off: u16) -> u8 {
            self.uci.borrow().as_ref().map_or(0, |u| u.mem[usize::from(off)])
        }
        fn uci_write(&mut self, off: u16, val: u8) {
            if let Some(u) = self.uci.borrow_mut().as_mut() {
                u.mem[usize::from(off)] = val;
            }
            self.push(Call::Uci(off, val));
        }
        fn uci_irq(&self) -> bool {
            self.uci.borrow().as_ref().is_some_and(|u| u.irq)
        }
        fn uci_take_events(&mut self) -> UciEvents {
            self.uci.borrow_mut().as_mut().map(|u| std::mem::take(&mut u.events)).unwrap_or_default()
        }

        fn has_sampler(&self) -> bool {
            self.sampler.borrow().is_some()
        }
        fn sampler_read(&self, off: u16) -> u8 {
            self.sampler.borrow().as_ref().map_or(0, |s| if off & 1 == 0 { s.status } else { 0x10 })
        }
        fn sampler_write(&mut self, off: u16, val: u8) {
            if let Some(s) = self.sampler.borrow_mut().as_mut() {
                s.regs[usize::from(off) & 0xFF] = val;
            }
            self.push(Call::Sampler(off, val));
        }
        fn set_sampler_enabled(&mut self, on: bool) {
            if let Some(s) = self.sampler.borrow_mut().as_mut() {
                s.enabled = on;
            }
            self.push(Call::SamplerEnable(on));
        }

        fn mixer_write(&mut self, off: u8, val: u8) {
            self.push(Call::Mixer(off, val));
        }

        fn frame_counter(&self) -> u64 {
            *self.frames.borrow()
        }
        fn frame_lines(&self) -> usize {
            *self.lines.borrow()
        }
        fn set_paused(&mut self, paused: bool) {
            self.push(Call::Paused(paused));
        }
        fn set_muted(&mut self, muted: bool) {
            self.mutes.borrow_mut().push(muted);
        }
        fn set_trx_bitstream(&mut self, on: bool) {
            *self.trx_bitstream.borrow_mut() = on;
        }
        fn sb_shadow(&self) -> super::SbShadow {
            self.shadow.borrow().clone()
        }
        fn set_engine(&mut self, cfg: super::EngineCfg) {
            self.engine.borrow_mut().push(cfg);
        }
        fn take_tap(&mut self) -> Vec<super::TapEvent> {
            std::mem::take(&mut self.tap.borrow_mut())
        }
        fn set_stop_pos(&mut self, cycle: Option<u8>) {
            self.pos.borrow_mut().push(Ok(cycle));
        }
        fn release_pos(&mut self) {
            self.pos.borrow_mut().push(Err(()));
        }
        fn sb_load(&mut self, load: &crate::devices::trx_sb::SbLoad) {
            self.loads.borrow_mut().push(*load);
        }
    }
}
