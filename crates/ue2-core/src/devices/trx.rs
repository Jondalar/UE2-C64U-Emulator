//! TRX64-Ultimate's IO block 0x101A0000-0x101AFFFF (`--trx-io`): page 0, the ID, the interrupt and from 0x100 the
//! gate; page 5, the freeze session; page 6, the pixel layer, the still capture and the C64 text layer. Spec:
//! docs/specs/S37-trx-pixel-layer.md. The hardware is TRX64-Ultimate's designs/c64_basic/trx_io.v (page 0 below
//! 0x100), trx_sys.v (page 0 from 0x100, pages 1, 2, 3, 5, 7), freeze_resp.v (the session), pix_layer.v and c64_text.v
//! (page 6).
//!
//! Sixteen 4 KB pages, byte wide. Page 0 from 0x104 holds the gate's capture control and page 1 the state bus (S39
//! phase A1, docs/specs/S39-trxmon.md; the model is `trx_sb.rs`); page 2 is the engine (S40,
//! docs/specs/S40-trx-engine.md; `trx_engine.rs`); page 3 and page 0 0x118-0x13F are the debug block (S39,
//! `trx_dbg.rs`); page 5 has the session and the lens unit. Page 0 0x006 and 0x040-0x04D are the reSID8 view (S43,
//! docs/specs/S43-trx-resid8-drive-slot.md), page 7 the expansion port's slot block (S43; the slot itself is the
//! backend's). Page 4 and every offset not listed read 0 and ignore writes.
//!
//! With a MultiCore core profile (`--core`, S44, docs/specs/S44-multicore-cores.md) the ID word and CAPS are the
//! core's, and every offset but ID, CAPS, IRQ_EN, IRQ_CAUSE and page 6 is the core's register file; none of the C64
//! pages is there then.
//!
//! The layer's settings and a pending flip take effect at the frame start, one per VIC frame. The device sees a VIC
//! frame through the count [`C64Port`] publishes after each sync, polled every millisecond, so the frame start comes
//! up to 1 ms after the VIC finished the frame. While the machine pause stands the VIC count stands too, and a frame
//! start comes every frame period instead (the output timing runs on). Without a C64 that counts frames it comes every
//! PAL frame period.
//!
//! The machine pause, the freeze session and the still capture live where the C64 is, in [`C64Port`]; the block and
//! the port share them through a [`TrxLink`].

use std::sync::{Arc, Mutex, MutexGuard};

use crate::c64host::{C64Charset, C64Rom};
use crate::coreprof::CoreProfile;
use crate::devices::c64::C64Port;
use crate::devices::hdmi_mode::{self, ModeReq, Row};
use crate::devices::trx_dbg::{self as dbg, DbgLink};
use crate::devices::trx_engine::{self as engine, Engine};
use crate::devices::trx_sb::StateBus;
use crate::devices::trx_session::{Chips, Session, VicView};
use crate::host::{TrxLayer, TrxMode, TrxText};
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::irq::IrqState;
use crate::machine::{MachineConfig, TrxIo};
use crate::time::{CLOCKS_PER_MS, CLOCK_HZ};

pub const BASE: u32 = 0x101A_0000;
pub const SIZE: u32 = 0x1_0000;
/// The block's interrupt: ITU high IRQ 2, drive B's WD177x in Gideon's bitstream (trx_io.v).
pub const IRQ_HIGH_BIT: u8 = 2;

/// Page 0 (trx_io.v): ID "TRX2" at 000-003, CAPS 004, CAPS2 005, BUILD 008-00B, IRQ_EN 010, IRQ_CAUSE 014.
const ID: &[u8; 4] = b"TRX2";
/// CAPS b0 page 0 gate, b1 page 1 state bus, b2 page 2 engine, b3 page 3 debug, b5 page 5 session, b6 page 6 video,
/// b7 the layer is RGB565. The bitstream reads $FF; b4 (cartridge, page 4) stays off and with it every app path that
/// needs page 4. A bit is reported only when its pages are built (S39 §2, S40 §2).
pub const CAPS: u8 = 0xEF;
/// CAPS2 b0: the drain writes stream 2's F4 only after a frame's F3 (`trx_ring.cc` `f4_after_blocks`); b2: the CPU
/// write path (state bus chip 1 w4/w5, the staged words 8-14, `IPC_SET`); b3: the session has the commit list, so the
/// app sets PROTO around Gideon's protocol (trx_video.cc `has_list`); b4: the lens unit (page 5 `LK_REQ`/`LK_RES`);
/// b5: the engine applies stream 0 itself (APPLY, `apply_hw`). t21-p4's bitstream reads $BF; b1 (BUILD) and b6 (turbo)
/// are not here; b7: the instruction trace (stream 3, S40 phase E).
pub const CAPS2: u8 = 0xBD;
/// CAPS2 b5, APPLY: clear on a bitstream built without it (trx_io.v `NO_APPLY`, S41 §2).
pub const CAPS2_APPLY: u8 = 0x20;
const CAPS_OFF: u32 = 0x004;
const CAPS2_OFF: u32 = 0x005;
/// S43: CAPS3 b0, the SID is the 8-SID reDIP with the reSID8 filter control (c64_core.v:887 `sid_resid8`, the default
/// `SID_MULTI` build; top.v:800).
const CAPS3_OFF: u32 = 0x006;
pub const CAPS3: u8 = 0x01;
/// S43: the reSID8 filter control, page 0 0x040-0x04D (trx_io.v): 040/041 filter source, 042/043 Auto bits, 044-047 and
/// 048-04B the 6581 base and offset of UltiSID 1 and 2, 04C/04D the bypass. What each byte keeps, and its reset value.
pub const RS8: u32 = 0x040;
const RS8_LEN: usize = 14;
const RS8_MASK: [u8; RS8_LEN] = [0x03, 0x03, 0x03, 0x03, 0xFF, 0x01, 0xFF, 0x07, 0xFF, 0x01, 0xFF, 0x07, 0x01, 0x01];
const RS8_RESET: [u8; RS8_LEN] = [0, 0, 3, 3, 245, 0, 0xF4, 0x04, 245, 0, 0xF4, 0x04, 0, 0];
/// 04C/04D: the bypass of UltiSID 1 / 2, which the C64 port gives the backend at its clock.
pub const RS8_BYPASS: u32 = 0x04C;
pub const IRQ_EN: u32 = 0x010;
pub const IRQ_CAUSE: u32 = 0x014;
/// IRQ_CAUSE b0, the machine halted (top.v `stop_tgl`: the pause or the position hold); b1, an engine job ended
/// (`eng_tgl`); b3, a capture or a halt (`cap_tgl`); b6, the frame start; b7, a flip was done. b2 (`gap_tgl`, records
/// lost) never comes, the other causes are pages UE2 does not build.
pub const CAUSE_STOP: u8 = 0x01;
pub const CAUSE_ENGINE: u8 = 0x02;
pub const CAUSE_CAPTURE: u8 = 0x08;
pub const CAUSE_VSYNC: u8 = 0x40;
pub const CAUSE_FLIP: u8 = 0x80;

/// trx_sys.v page 0 from 0x100: GATE_CTRL, W b0 FRZ, R b0 FRZ, b8 halted, b9 the gate is shut.
pub const GATE_CTRL: u32 = 0x100;
pub const GATE_FRZ: u32 = 0x001;
pub const GATE_HALTED: u32 = 0x100;
pub const GATE_SHUT: u32 = 0x200;
/// trx_sys.v:24-34: CAP_CTRL RW b7:0, CAP_STAT R `{frames without their snap, CAP_GEN}`, JTAG_PAGE RW b3:0,
/// RESUME_PHASE, STOP_POS `{R b16 holding, b9 W release, b8 arm, b5:0 cycle}`. 118-13F (debug) and the rest read 0.
pub const CAP_CTRL: u32 = 0x104;
pub const CAP_STAT: u32 = 0x108;
pub const JTAG_PAGE: u32 = 0x10C;
pub const RESUME_PHASE: u32 = 0x110;
pub const STOP_POS: u32 = 0x114;
pub const POS_ARM: u32 = 0x100;
pub const POS_RELEASE: u32 = 0x200;
pub const POS_HOLDING: u32 = 0x1_0000;
/// Page 1, the state bus (trx_sys.v:38-48).
pub const PAGE1: u32 = 0x1000;
pub const SB_ADDR: u32 = 0x000;
pub const SB_DATA: u32 = 0x004;
pub const SB_STATUS: u32 = 0x008;
pub const SB_LOAD: u32 = 0x00C;
/// Page 5, the freeze session (trx_sys.v, freeze_resp.v).
pub const PAGE5: u32 = 0x5000;
pub const SH_CTRL: u32 = 0x000;
pub const SH_STATUS: u32 = 0x004;
pub const SH_ACCESSES: u32 = 0x008;
pub const SH_REAL: u32 = 0x00C;
pub const SH_GCLK: u32 = 0x010;
pub const SH_UNMAPPED: u32 = 0x014;
pub const SH_LIST: u32 = 0x018;
pub const SH_COMMIT: u32 = 0x01C;
/// The lens unit (trx_sys.v:63-70, freeze_resp.v:227-234): `LK_REQ` W `{REAL b18, class b17:16, address}`, `LK_RES` R
/// `{busy b31, refused b12, real b11, ext b10, hit b9, done b8, data}`.
pub const LK_REQ: u32 = 0x020;
pub const LK_RES: u32 = 0x024;
pub const LK_REFUSED: u32 = 1 << 12;
pub const LK_REAL: u32 = 1 << 11;
pub const LK_EXT: u32 = 1 << 10;
pub const LK_HIT: u32 = 1 << 9;
pub const LK_DONE: u32 = 1 << 8;
/// The 128 overlay bytes and their valid bits.
pub const SH_OVERLAY: u32 = 0x080;
pub const SH_VALID: u32 = 0x100;

/// S43: page 7, the expansion port's slot block (trx_sys.v:60-72, slot_port.v): SLOT_CTRL b1 ROM_WR_EN (RW, 0 at
/// configuration, no reset term); SLOT_STATUS as the backend's slot reports it, b18 the ROM_WR_EN it applies and b31
/// the block. SLOT_CNT0/1 and SLOT_LAST (0x008-0x010) read 0.
pub const PAGE7: u32 = 0x7000;
pub const SLOT_CTRL: u32 = 0x000;
pub const SLOT_STATUS: u32 = 0x004;
pub const SLOT_WR_EN: u32 = 0x02;
pub const SLOT_ST_WR_EN: u32 = 1 << 18;
pub const SLOT_ST_BLOCK: u32 = 1 << 31;
/// SLOT_STATUS without a C64: the pins high (GAME, EXROM, DMA, IRQ, NMI), INT 0xF and EXT 0x0 (ultimate_sys.vhd:453).
pub const SLOT_IDLE: u32 = 0x0000_F01F;

/// Page 6, the pixel layer (pix_layer.v): 32-bit registers, little endian. Writing byte 0-2 stages it, writing byte
/// 3 commits all four; reading byte 0 latches the register for bytes 1-3.
pub const PAGE6: u32 = 0x6000;
pub const STILL_CTRL: u32 = 0x060;
pub const STILL_BASE: u32 = 0x064;
pub const STILL_STATUS: u32 = 0x068;
pub const PX_CTRL: u32 = 0x080;
pub const PX_FRONT: u32 = 0x084;
pub const PX_BACK: u32 = 0x088;
pub const PX_SIZE: u32 = 0x090;
pub const PX_STRIDE: u32 = 0x094;
pub const PX_POS: u32 = 0x098;
pub const PX_GALPHA: u32 = 0x09C;
pub const PX_KEY: u32 = 0x0A0;
pub const PX_STATUS: u32 = 0x0A4;
/// S45: the MultiCore host's output size of the picture, {height 16, width 16} (pix_layer.v; TRX64's host has none).
pub const PX_OSIZE: u32 = 0x0A8;
/// c64_text.v in page 6: TX_CTRL b0 show, b1 opaque, reads b31:24 = 1; TX_STATUS b0 shown now, b15:8 late lines.
pub const TX_CTRL: u32 = 0x0C0;
pub const TX_STATUS: u32 = 0x0C4;
const TX_THERE: u32 = 0x0100_0000;

/// How often the device looks at the VIC frame count: the C64 port syncs at least this often (c64.rs SYNC_PERIOD).
const POLL: u64 = CLOCKS_PER_MS;
/// The frame period without a C64 that says its own: PAL, 312 lines of 63 cycles at 985,248 Hz.
const PAL_FRAME: u64 = 312 * 63 * CLOCK_HZ / 985_248;
/// PX_STATUS b1 holds for the first 45 output lines of 1125 (`px_vblank`, cy < 45): 45 × 2688 clocks of the
/// 151.58 MHz pixel clock (vic_scaler.v), 0.8 ms.
const VBLANK: u64 = 45 * 2688 * CLOCK_HZ / 151_580_000;
/// The DDR byte address the layer's fetch and the still capture take: bits 26:0 (pix_layer.v `rd_a`, `st_a`).
pub const DDR_ADDR_MASK: u32 = 0x07FF_FFFF;
/// Output frame size the layer is placed in on TRX64's host (S45: the MultiCore host's is the running mode's active
/// area).
pub const OUT_W: usize = 1920;
pub const OUT_H: usize = 1080;
/// Source width limit: the line buffer holds 960 pixels (pix_layer.v `f_w`).
const MAX_W: u16 = 960;
/// Source height limit (pix_layer.v `f_hc`).
const MAX_H: u16 = 1080;
/// S45: PX_OSIZE's two sizes must each stay below this (pix_layer.v `en_new`).
const OSIZE_LIMIT: u16 = 2048;
/// S45: vertical smoothing of a 32-bit picture needs its line in 128 words (pix_layer.v `vsm_new`).
const SMOOTH_V_WIDE_W: u16 = 512;

/// The still capture's 50 Hz window (pix_layer.v `X0S`, `Y0S`, 400 × 270): VIC raster columns 96-495, lines 16-285,
/// two pixels a byte, the first in the low nibble. Slot n is at `STILL_BASE` + n × 64 KB.
pub const STILL_W: usize = 400;
pub const STILL_H: usize = 270;
pub const STILL_Y0: usize = 16;
pub const STILL_SLOT: u32 = 0x1_0000;
/// The 60 Hz still (pix_layer.v:519-521, :536, :544, :552): 400 × 240 of one VIC frame, rows 0-231 lines 31-262, rows
/// 232-239 lines 0-7 (written first). Complete at word 2,899, on line 262.
pub const STILL_H_60: usize = 240;
pub const STILL_Y0_60: usize = 31;
const STILL_LOW_60: usize = 8;

/// The scaler's `v60` as UE2 has it: a VIC of fewer than 300 lines (render.rs `draw_c64_trx`).
pub fn still_v60(lines: usize) -> bool {
    lines != 0 && lines < 300
}

/// A VIC of `lines` lines (0: unknown, taken as PAL) completes a still: 50 Hz at line 285 (word 3,374), 60 Hz at line
/// 262 (word 2,899).
pub fn still_complete(lines: usize) -> bool {
    match lines {
        0 => true,
        n if still_v60(n) => n >= STILL_Y0_60 + STILL_H_60 - STILL_LOW_60,
        n => n >= STILL_Y0 + STILL_H,
    }
}

/// The still capture (pix_layer.v): its settings as the app committed them and the status of the last complete
/// frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Still {
    /// STILL_CTRL b0 and STILL_BASE.
    pub on: bool,
    pub base: u32,
    /// STILL_STATUS b0 (a frame completed since power on), b1 (its slot), b2 (it is 60 Hz, 400 × 240), b31:8
    /// (frames, 24 bits).
    pub valid: bool,
    pub slot: u8,
    pub v60: bool,
    pub count: u32,
    /// The slot the capture last completed (`last_slot`): 1 at power on, so the first frame goes to slot 0.
    pub last: u8,
}

impl Still {
    pub const POWER_ON: Still =
        Still { on: true, base: 0x0505_0000, valid: false, slot: 0, v60: false, count: 0, last: 1 };

    pub fn status(&self) -> u32 {
        (self.count & 0xFF_FFFF) << 8 | u32::from(self.v60) << 2 | u32::from(self.slot) << 1 | u32::from(self.valid)
    }

    /// `frames` VIC frames ended since the last look; the newest is captured. A complete one goes into the slot the
    /// device would have used for it and counts them all, and b2 takes its mode `v60` with the slot (pix_layer.v:557,
    /// :177); an incomplete one (a VIC that never reaches the window's last line) goes into the slot after the last
    /// and changes no status. Returns the slot written.
    pub fn frames(&mut self, frames: u64, complete: bool, v60: bool) -> u8 {
        if !complete {
            return self.last ^ 1;
        }
        let slot = self.last ^ (frames & 1) as u8;
        self.last = slot;
        self.slot = slot;
        self.v60 = v60;
        self.valid = true;
        self.count = self.count.wrapping_add(frames as u32) & 0xFF_FFFF;
        slot
    }
}

/// A write to a register of the C64's clock (page 0 from 0x100, page 1, page 2) that acts on the machine: the port
/// takes it at its clock, in order, as it runs the C64 up (S40 §3 "Ordering").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysWrite {
    pub at: u64,
    /// The block offset of the register's byte 0.
    pub off: u32,
    pub v: u32,
}

/// What the block and the C64 port share (S37 phase B). The port owns the C64: it applies the pause, answers the
/// session's window accesses and captures the still; the block owns the registers the app reads and writes.
#[derive(Debug)]
pub struct TrxLink {
    /// TRX64's VIC frame count after the last sync, 0 while no C64 counts frames.
    pub frames: u64,
    /// One VIC frame in emulator clocks, 0 when the C64 does not say.
    pub frame_clocks: u64,
    /// GATE_CTRL b0 as written.
    pub frz: bool,
    /// The writes the port has not taken yet (GATE_CTRL, CAP_CTRL, STOP_POS, SB_LOAD).
    pub ops: Vec<SysWrite>,
    /// The machine is halted (the pause or the position hold, applied by the port), and how often it came to a halt.
    pub paused: bool,
    pub pause_starts: u64,
    pub session: Session,
    pub still: Still,
    /// S39 phase A1: the state bus and the capture control, as the port keeps them.
    pub sb: StateBus,
    /// CAP_CTRL and JTAG_PAGE as last written (they read back at once, trx_sys.v:216-218).
    pub cap_ctrl: u8,
    pub jtag_page: u8,
    /// S40: the engine (page 2).
    pub engine: Engine,
    /// S39: the debug block (page 3), shared with the C64 backend that finds its stops.
    pub dbg: DbgLink,
    /// S39: C64_STOP as written (STATUS b5), and the lens unit's last result.
    pub fw_stop: bool,
    pub lk_res: u32,
    /// S43: SLOT_CTRL's ROM_WR_EN as written, and SLOT_STATUS as the port last read it from the backend.
    pub slot_wr_en: bool,
    pub slot_status: u32,
    /// S43: RS8 04C/04D b0, UltiSID 1 and 2's filter bypassed.
    pub rs8_bypass: [bool; 2],
    /// S45: the MultiCore host's mode request and running row (`--icap` or `--core`); None on TRX64's host.
    pub mode: Option<ModeReq>,
}

impl TrxLink {
    fn power_on(frames: u64, frame_clocks: u64, paused: bool, pause_starts: u64, dbg: DbgLink) -> Self {
        TrxLink {
            frames,
            frame_clocks,
            frz: false,
            ops: Vec::new(),
            paused,
            pause_starts,
            session: Session::default(),
            still: Still::POWER_ON,
            sb: StateBus { halted: paused, ..StateBus::default() },
            cap_ctrl: 0,
            jtag_page: 0,
            engine: Engine::default(),
            dbg,
            fw_stop: false,
            lk_res: 0,
            slot_wr_en: false,
            slot_status: SLOT_IDLE,
            rs8_bypass: [false; 2],
            mode: None,
        }
    }

    /// What the debug block's reads know of the machine.
    pub fn dbg_ctx(&self) -> dbg::Ctx {
        dbg::Ctx { shut: self.paused, fw_stop: self.fw_stop }
    }
}

pub type Link = Arc<Mutex<TrxLink>>;

/// A new link at power on.
pub fn link() -> Link {
    Arc::new(Mutex::new(TrxLink::power_on(0, 0, false, 0, Arc::new(Mutex::new(dbg::Dbg::default())))))
}

/// The link, also after a panic elsewhere held it.
pub fn lock(link: &Link) -> MutexGuard<'_, TrxLink> {
    link.lock().unwrap_or_else(|e| e.into_inner())
}

/// The layer's settings as the app last committed them (pix_layer.v `r_*`, defaults included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settings {
    ctrl: u8,
    w: u16,
    h: u16,
    stride: u16,
    x0: u16,
    y0: u16,
    galpha: u8,
    /// b15:0 the RGB565 key, b16 key on.
    key: u32,
    /// S45: PX_OSIZE (the MultiCore host only).
    ow: u16,
    oh: u16,
}

impl Settings {
    const DEFAULT: Settings =
        Settings { ctrl: 0x22, w: 960, h: 540, stride: 1920, x0: 0, y0: 0, galpha: 255, key: 0, ow: 1920, oh: 1080 };
}

/// What one frame shows, taken at its frame start (pix_layer.v `f_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Shown {
    on: bool,
    front: u32,
    w: u16,
    h: u16,
    stride: u16,
    x0: u16,
    y0: u16,
    /// The output size: S · w by S · h on TRX64's host, PX_OSIZE on the MultiCore host (S45 §6).
    ow: u16,
    oh: u16,
    smooth_h: bool,
    smooth_v: bool,
    /// Format 2, 32 bits a pixel.
    wide: bool,
    alpha: u16,
    key: Option<u16>,
}

impl Shown {
    /// The frame-start copy of `s` (pix_layer.v `en_new`, `s_new`, the `f_*` assignments) on TRX64's host (`row`
    /// None): on with b0, format 1 and a position inside the 1080p frame; width at most 960, height at most 1080;
    /// the scale S of b5:4, 0 counting as 1.
    ///
    /// S45 §6, the MultiCore host (`row` the running mode): on with b0, format 1 or 2, sizes and output sizes not 0,
    /// output sizes below 2048 and a position inside the active area; b5:4 is not read, b6 smooths each axis where the
    /// source is not larger than the output (vertically in format 2 only up to 512 pixels a line).
    fn of(s: &Settings, front: u32, row: Option<&Row>) -> Shown {
        let (w, h) = (s.w.min(MAX_W), s.h.min(MAX_H));
        let fmt = (s.ctrl >> 1) & 0x07;
        let alpha = u16::from(s.galpha) + u16::from(s.galpha >> 7);
        let Some(row) = row else {
            let scale = u16::from(match (s.ctrl >> 4) & 0x03 {
                0 => 1,
                n => n,
            });
            let on = s.ctrl & 0x01 != 0
                && fmt == 1
                && s.w != 0
                && s.h != 0
                && usize::from(s.x0) < OUT_W
                && usize::from(s.y0) < OUT_H;
            return Shown {
                on,
                front,
                w,
                h,
                stride: s.stride,
                x0: s.x0,
                y0: s.y0,
                ow: w * scale,
                oh: h * scale,
                smooth_h: false,
                smooth_v: false,
                wide: false,
                alpha,
                key: (s.key & 0x1_0000 != 0).then_some(s.key as u16),
            };
        };
        let wide = fmt == 2;
        let on = s.ctrl & 0x01 != 0
            && (fmt == 1 || wide)
            && s.w != 0
            && s.h != 0
            && s.ow != 0
            && s.oh != 0
            && s.ow < OSIZE_LIMIT
            && s.oh < OSIZE_LIMIT
            && u32::from(s.x0) < row.hact
            && u32::from(s.y0) < row.vact;
        let smooth = s.ctrl & 0x40 != 0;
        Shown {
            on,
            front,
            w,
            h,
            stride: s.stride,
            x0: s.x0,
            y0: s.y0,
            ow: s.ow,
            oh: s.oh,
            smooth_h: smooth && w <= s.ow,
            smooth_v: smooth && h <= s.oh && (!wide || w <= SMOOTH_V_WIDE_W),
            wide,
            alpha,
            key: (s.key & 0x1_0000 != 0 && !wide).then_some(s.key as u16),
        }
    }
}

/// What `monitor trx` shows of the engine (page 2).
#[derive(Clone, Debug)]
pub struct EngineView {
    pub on: bool,
    /// `LOG_CTRL` b3:1: b0 outside, b1 cartridge RAM, b2 colour RAM.
    pub taps: u8,
    pub frame: u32,
    pub dropped: u16,
    pub cnt: [u32; 3],
    /// Each stream's area: byte address and size.
    pub areas: [(u32, u32); 3],
    pub ap_jobs: Vec<crate::devices::trx_engine::ApJob>,
    pub store_mb: usize,
}

/// Whether block offset `off` is one of trx_sys.v's pages (trx_io.v `far`): page 0 from 0x100, pages 1, 2, 3, 5, 7.
fn far(off: u32) -> bool {
    match off >> 12 {
        0 => off & 0xF00 != 0,
        1 | 2 | 3 | 5 | 7 => true,
        _ => false,
    }
}

/// S44: a MultiCore core's part of the block: every byte the core owns ([`CoreProfile::owns`]) is a register that
/// reads back what was written, from the profile's reset values; the read-only ones read their value whatever is
/// written. The C64 parts (the far pages, reSID8, CAPS2, CAPS3, BUILD) are not there.
struct CoreRegs {
    id: [u8; 4],
    caps: u8,
    bytes: Vec<u8>,
    reset: Vec<u8>,
    ro: Vec<bool>,
}

impl CoreRegs {
    fn new(p: &CoreProfile) -> Self {
        let mut reset = vec![0u8; SIZE as usize];
        let mut ro = vec![false; SIZE as usize];
        for (&off, &v) in &p.regs {
            reset[off as usize] = v;
        }
        for (&off, &v) in &p.ro {
            reset[off as usize] = v;
            ro[off as usize] = true;
        }
        CoreRegs { id: p.id, caps: p.caps, bytes: reset.clone(), reset, ro }
    }
}

/// TX_CTRL and TX_STATUS, c64_text.v's two registers (`sel`, 0C0-0C7).
fn tx_sel(reg: u32) -> bool {
    reg & !7 == TX_CTRL
}

pub struct Trx {
    irq_en: u8,
    cause: u8,
    set: Settings,
    /// PX_FRONT and PX_BACK; `flip` while a PX_BACK write waits for the frame start.
    front: u32,
    back: u32,
    flip: bool,
    /// Bytes 0-2 of the next commit, one stage for all of page 6 (pix_layer.v `stg`).
    stage: u32,
    /// Bytes 1-3 of the last page 6 byte-0 read (pix_layer.v `lat`).
    latch: u32,
    /// trx_sys.v's one collect buffer and one latch for all of its pages (`wbuf`, `rlatch`).
    far_stage: u32,
    far_latch: u32,
    /// SB_ADDR (trx_sys.v `sb_a`): `{chip, word}`, moved on by every SB_DATA read and write.
    sb_a: u16,
    /// The capture events (`cap_tgl`), the engine's ended jobs (`eng_tgl`) and the debugger's stops (`ev`) seen.
    cap_seen: u64,
    eng_seen: u64,
    dbg_seen: u64,
    /// c64_text.v: TX_CTRL b1:0, its own stage of byte 0 and its own latch.
    tx_ctrl: u8,
    tx_stage: u8,
    tx_latch: u32,
    /// S43: RS8 0x040-0x04D as written, masked.
    rs8: [u8; RS8_LEN],
    shown: Shown,
    /// Frame starts, wrapping (PX_STATUS b31:16), and all of them.
    frames: u16,
    starts: u64,
    /// Clock of the last frame start, and the latest clock the device saw.
    frame_at: u64,
    now: u64,
    /// The VIC frame count of the last frame start, and the pause starts seen.
    vic_seen: u64,
    pause_seen: u64,
    next_poll: u64,
    link: Link,
    /// S41: the engine has no APPLY unit (trx_engine.v `NO_APPLY`): AP_START, AP_COUNT and AP_CMD writes do nothing,
    /// AP_CMD never reads busy, CAPS2 b5 reads 0. A build switch, so a reset keeps it.
    apply_absent: bool,
    /// S43: the two-SID reDIP (`--absent resid8`): CAPS3 reads 0 and the bypass goes to no engine. A build switch too.
    resid8_absent: bool,
    /// S41 §5: the core is not loaded: the block neither polls nor drives its interrupt.
    core_absent: bool,
    /// S44: a MultiCore core in place of the C64 core.
    core: Option<CoreRegs>,
    /// S45: the MultiCore host's mode request as the run starts it (§4), set again by a reset; None on TRX64's host.
    mode_start: Option<ModeReq>,
}

impl Trx {
    pub fn new(link: Link) -> Self {
        let (pause_seen, cap_seen, eng_seen, dbg_seen) = {
            let l = lock(&link);
            let ev = dbg::lock(&l.dbg).ev;
            (l.pause_starts, l.sb.cap_events, l.engine.done, ev)
        };
        Trx {
            irq_en: 0,
            cause: 0,
            set: Settings::DEFAULT,
            front: 0,
            back: 0,
            flip: false,
            stage: 0,
            latch: 0,
            far_stage: 0,
            far_latch: 0,
            sb_a: 0,
            cap_seen,
            eng_seen,
            dbg_seen,
            tx_ctrl: 0,
            tx_stage: 0,
            tx_latch: 0,
            rs8: RS8_RESET,
            shown: Shown::of(&Settings::DEFAULT, 0, None),
            frames: 0,
            starts: 0,
            frame_at: 0,
            now: 0,
            vic_seen: 0,
            pause_seen,
            next_poll: POLL,
            link,
            apply_absent: false,
            resid8_absent: false,
            core_absent: false,
            core: None,
            mode_start: None,
        }
    }

    /// S45: the block sits in the MultiCore host, whose mode request starts at `row` (§4).
    pub fn set_multicore(&mut self, row: u8) {
        self.mode_start = Some(ModeReq::new(row));
        lock(&self.link).mode = self.mode_start;
        self.shown = Shown::of(&self.set, self.front, self.mode_row().as_ref());
    }

    /// S45: the block sits in the MultiCore host (its mode request and pixel layer).
    fn multicore(&self) -> bool {
        lock(&self.link).mode.is_some()
    }

    /// S44: the block answers for `p`'s core instead of the C64 core.
    pub fn set_core(&mut self, p: &CoreProfile) {
        self.core = Some(CoreRegs::new(p));
    }

    /// S44: the C64 core again ("TRX2" and its pages).
    pub fn clear_core(&mut self) {
        self.core = None;
    }

    /// S44: the core register at block offset `off` as the app reads it, None without a core profile or for one of
    /// the host's offsets.
    pub fn core_reg(&self, off: u32) -> Option<u8> {
        let c = self.core.as_ref()?;
        CoreProfile::owns(off).then(|| c.bytes[off as usize])
    }

    /// S41 §2: the bitstream has no APPLY unit.
    pub fn set_apply_absent(&mut self, absent: bool) {
        self.apply_absent = absent;
    }

    /// S43: the bitstream is built with the two-SID reDIP.
    pub fn set_resid8_absent(&mut self, absent: bool) {
        self.resid8_absent = absent;
    }

    /// S41 §5: the core goes (its interrupt drops and it stops polling) or comes back; a core that comes back is a
    /// fresh one, which the caller gets with a reset.
    pub fn set_core_absent(&mut self, absent: bool, now: u64, irq: &mut IrqState) {
        self.core_absent = absent;
        if absent {
            irq.set_high(IRQ_HIGH_BIT, false);
        } else {
            self.next_poll = now + POLL;
        }
    }

    pub fn irq_en(&self) -> u8 {
        self.irq_en
    }

    pub fn cause(&self) -> u8 {
        self.cause
    }

    /// Whether the last frame start turned the layer on.
    pub fn shown(&self) -> bool {
        self.shown.on
    }

    /// The engine's state for a report (S40): what the monitor shows of page 2.
    pub fn engine(&self) -> EngineView {
        let l = lock(&self.link);
        let e = &l.engine;
        EngineView {
            on: e.on,
            taps: e.taps(),
            frame: e.frame,
            dropped: e.dropped,
            cnt: e.cnt,
            areas: e.areas(),
            ap_jobs: e.ap_jobs.iter().copied().collect(),
            store_mb: e.store.megabytes(),
        }
    }

    /// S39 §3.8: the debugger (trxmon) owns run control: OWNER is set.
    pub fn owner(&self) -> bool {
        dbg::lock(&lock(&self.link).dbg).own
    }

    /// The debug block's model as it stands (a copy), and what its reads know of the machine.
    pub fn debug(&self) -> (dbg::Dbg, dbg::Ctx) {
        let l = lock(&self.link);
        let d = dbg::lock(&l.dbg).clone();
        (d, l.dbg_ctx())
    }

    /// The state bus and the capture control as the port keeps them (a copy).
    pub fn state_bus(&self) -> StateBus {
        lock(&self.link).sb.clone()
    }

    /// The machine pause is in effect.
    pub fn paused(&self) -> bool {
        lock(&self.link).paused
    }

    /// Frame starts since power on: the output's frames, which go on while the machine pause stands.
    pub fn frame_starts(&self) -> u64 {
        self.starts
    }

    /// TRX64's VIC frame count as the block saw it last.
    pub fn vic_frames(&self) -> u64 {
        lock(&self.link).frames
    }

    /// Page 6 register `off` (its first byte) as a 32-bit read returns it: the configuration registers are write-only
    /// and read 0, only STILL_STATUS and PX_STATUS answer (pix_layer.v `cur`, since 28d8b69); c64_text.v's two read 0
    /// here too.
    pub fn bus_reg(&self, off: u32) -> u32 {
        match off {
            STILL_STATUS | PX_STATUS => self.reg(off),
            _ => 0,
        }
    }

    /// Page 6 register `off` (its first byte) as the app last committed it, for the monitor and the tests; the app
    /// reads these through [`Self::bus_reg`].
    pub fn reg(&self, off: u32) -> u32 {
        let s = &self.set;
        match off {
            STILL_CTRL => u32::from(lock(&self.link).still.on),
            STILL_BASE => lock(&self.link).still.base,
            STILL_STATUS => lock(&self.link).still.status(),
            PX_CTRL => u32::from(s.ctrl),
            PX_FRONT => self.front,
            PX_BACK => self.back,
            PX_SIZE => u32::from(s.h) << 16 | u32::from(s.w),
            PX_STRIDE => u32::from(s.stride),
            PX_POS => u32::from(s.y0) << 16 | u32::from(s.x0),
            PX_GALPHA => u32::from(s.galpha),
            PX_KEY => s.key,
            PX_OSIZE => u32::from(s.oh) << 16 | u32::from(s.ow),
            // b15:8, the underruns, stay 0: the emulated fetch is never late.
            PX_STATUS => {
                let vblank = self.now < self.frame_at + self.mode_row().map_or(VBLANK, |r| r.lines_clocks(r.yact0()));
                u32::from(self.frames) << 16 | u32::from(vblank) << 1 | u32::from(self.flip)
            }
            _ => 0,
        }
    }

    /// c64_text.v's TX_CTRL (`0x0100_0000` | ctrl) or TX_STATUS (b0 shown now; the late lines stay 0).
    pub fn tx_reg(&self, off: u32) -> u32 {
        if off & 4 == 0 {
            TX_THERE | u32::from(self.tx_ctrl)
        } else {
            u32::from(self.text().is_some())
        }
    }

    /// The text layer's view and its opaque bit while it is shown: TX_CTRL b0, a session runs and the session's VIC
    /// writes leave standard text mode (c64_text.v:135, :188).
    pub fn text(&self) -> Option<(VicView, bool)> {
        if self.tx_ctrl & 1 == 0 {
            return None;
        }
        let l = lock(&self.link);
        let view = l.session.vic_view();
        (l.session.active && view.text()).then_some((view, self.tx_ctrl & 2 != 0))
    }

    /// trx_sys.v register `off` (a block offset, 32-bit aligned) as a read returns it (trx_sys.v:211-245), without
    /// the side effect of an SB_DATA read.
    pub fn sys_reg(&self, off: u32) -> u32 {
        let l = lock(&self.link);
        let sb = &l.sb;
        match off {
            // b9: the shadows follow the machine (`sb_frozen`), b8 halted: both the halt here.
            GATE_CTRL => {
                let halted = u32::from(l.paused);
                return halted << 9 | halted << 8 | u32::from(l.frz);
            }
            CAP_CTRL => return u32::from(l.cap_ctrl),
            CAP_STAT => return u32::from(sb.cap_nosnap) << 16 | u32::from(sb.cap_gen),
            JTAG_PAGE => return u32::from(l.jtag_page),
            // Accepted and used by nothing: never pending (S40 §6.3).
            RESUME_PHASE => return 0,
            STOP_POS => {
                return u32::from(sb.pos_hold) << 16 | u32::from(sb.pos_arm) << 8 | u32::from(sb.pos_cyc);
            }
            _ => {}
        }
        if off & !0xFFF == engine::PAGE2 {
            return l.engine.read(off & 0xFFF);
        }
        if dbg::is_dbg(off) {
            let c = l.dbg_ctx();
            return dbg::lock(&l.dbg).read(off, c, false);
        }
        if off & !0xFFF == PAGE1 {
            return match off & 0xFFF {
                SB_ADDR => u32::from(self.sb_a),
                SB_DATA => sb.word(self.sb_a),
                SB_STATUS => u32::from(sb.err) << 1 | u32::from(sb.frozen()),
                SB_LOAD => u32::from(sb.ldarm),
                _ => 0,
            };
        }
        if off & !0xFFF == PAGE7 {
            return match off & 0xFFF {
                SLOT_CTRL => u32::from(l.slot_wr_en) * SLOT_WR_EN,
                SLOT_STATUS => {
                    l.slot_status & !SLOT_ST_WR_EN | u32::from(l.slot_wr_en) * SLOT_ST_WR_EN | SLOT_ST_BLOCK
                }
                _ => 0,
            };
        }
        if off & !0xFFF != PAGE5 {
            return 0;
        }
        let s = &l.session;
        match off & 0xFFF {
            SH_CTRL => s.ctrl(),
            SH_STATUS => s.status(),
            SH_ACCESSES => s.accesses,
            SH_REAL => s.real,
            SH_UNMAPPED => s.unmapped,
            SH_LIST => s.list_reg(),
            SH_COMMIT => u32::from(s.last_commit),
            LK_RES => l.lk_res,
            r @ SH_OVERLAY..=0x0FF => {
                let i = (r - SH_OVERLAY) as usize;
                u32::from_le_bytes(std::array::from_fn(|b| s.overlay(i + b)))
            }
            r @ SH_VALID..=0x10F => s.valid_word(((r - SH_VALID) / 4) as usize),
            // SH_GCLK, LK_RES and the rest.
            _ => 0,
        }
    }

    /// [`Self::sys_reg`] with the read's side effects: the debug block's CYCLES latch and BMP_DATA's increment.
    fn sys_read(&mut self, off: u32) -> u32 {
        if dbg::is_dbg(off) {
            let l = lock(&self.link);
            let c = l.dbg_ctx();
            return dbg::lock(&l.dbg).read(off, c, true);
        }
        self.sys_reg(off)
    }

    /// trx_sys.v's commit of byte 3 (trx_sys.v:310-346). What acts on the machine goes to the port, at this clock.
    /// The mover runs at once, over guest DDR (`ram`) and the engine's store.
    fn sys_commit(&mut self, off: u32, v: u32, now: u64, ram: &mut [u8]) {
        let mut l = lock(&self.link);
        let op = SysWrite { at: now, off, v };
        if off & !0xFFF == engine::PAGE2 {
            match off & 0xFFF {
                // S41: without the APPLY unit its registers take nothing and no job starts (trx_engine.v NO_APPLY).
                engine::AP_START | engine::AP_COUNT | engine::AP_CMD if self.apply_absent => {}
                engine::ENG_SRC | engine::ENG_DST | engine::ENG_LEN | engine::ENG_ARG | engine::AP_START
                | engine::AP_COUNT => l.engine.write_regs(off & 0xFFF, v),
                engine::ENG_CMD => {
                    l.engine.eng_cmd(v, ram);
                }
                engine::AP_CMD => {
                    if l.engine.ap_cmd(v) {
                        l.ops.push(op);
                    }
                }
                _ => l.ops.push(op),
            }
            return;
        }
        match off {
            // S39: the debug block's writes act at the C64's clock, in order with the rest; so does the lens unit.
            o if dbg::is_dbg(o) || o == PAGE5 + LK_REQ => l.ops.push(op),
            GATE_CTRL => {
                l.frz = v & GATE_FRZ != 0;
                l.ops.push(op);
            }
            CAP_CTRL => {
                l.cap_ctrl = v as u8;
                l.ops.push(op);
            }
            JTAG_PAGE => l.jtag_page = (v & 0x0F) as u8,
            STOP_POS => l.ops.push(op),
            o if o == PAGE1 + SB_ADDR => self.sb_a = (v & 0xFFF) as u16,
            o if o == PAGE1 + SB_DATA => {
                l.sb.stage(self.sb_a, v);
                if l.sb.frozen() {
                    self.sb_a = (self.sb_a + 1) & 0xFFF;
                }
            }
            o if o == PAGE1 + SB_STATUS => l.sb.err = false,
            o if o == PAGE1 + SB_LOAD => l.ops.push(op),
            o if o == PAGE5 + SH_CTRL => l.session.write_ctrl(v),
            // S43: read back at once (trx_sys.v:217); the slot takes it at the C64's clock.
            o if o == PAGE7 + SLOT_CTRL => {
                l.slot_wr_en = v & SLOT_WR_EN != 0;
                l.ops.push(op);
            }
            _ => {}
        }
    }

    /// The layer the last frame start set up, its pixels read from the guest DDR at its front buffer; None while it
    /// is off. A pixel past the end of `ram` reads 0.
    pub fn layer(&self, ram: &[u8]) -> Option<TrxLayer> {
        let s = &self.shown;
        if !s.on {
            return None;
        }
        let (w, h) = (usize::from(s.w), usize::from(s.h));
        let bytes = if s.wide { 4 } else { 2 };
        let mut pixels = Vec::with_capacity(w * h);
        for m in 0..h as u32 {
            let line = s.front.wrapping_add(m * u32::from(s.stride));
            pixels.extend((0..w as u32).map(|p| {
                let at = (line.wrapping_add(bytes * p) & DDR_ADDR_MASK) as usize;
                ram.get(at..at + bytes as usize).map_or(0, |b| {
                    if s.wide {
                        u32::from_le_bytes([b[0], b[1], b[2], 0])
                    } else {
                        u32::from(u16::from_le_bytes([b[0], b[1]]))
                    }
                })
            }));
        }
        Some(TrxLayer {
            x0: usize::from(s.x0),
            y0: usize::from(s.y0),
            w,
            h,
            ow: usize::from(s.ow),
            oh: usize::from(s.oh),
            smooth_h: s.smooth_h,
            smooth_v: s.smooth_v,
            alpha: s.alpha,
            key: s.key,
            wide: s.wide,
            pixels,
        })
    }

    /// S45: the MultiCore host's running row, None on TRX64's host.
    pub fn mode_row(&self) -> Option<Row> {
        lock(&self.link).mode.map(|m| m.current())
    }

    /// S45: the MultiCore host's running mode for the picture, None on TRX64's host (always 1080p).
    pub fn mode(&self) -> Option<TrxMode> {
        self.mode_row().map(|r| TrxMode { hact: r.hact as usize, vact: r.vact as usize, c64: r.locked })
    }

    /// The frame start (pix_layer.v, cy 0): a pending flip shows PX_BACK and raises b7, the committed settings take
    /// effect, and b6 is raised.
    fn frame_start(&mut self, now: u64, irq: &mut IrqState) {
        if self.flip {
            self.front = self.back;
            self.flip = false;
            self.cause |= CAUSE_FLIP;
        }
        self.shown = Shown::of(&self.set, self.front, self.mode_row().as_ref());
        self.cause |= CAUSE_VSYNC;
        self.frames = self.frames.wrapping_add(1);
        self.starts += 1;
        self.frame_at = now;
        self.update_irq(irq);
    }

    /// A frame start for each new VIC frame count; while the pause stands, one every frame period from the last; while
    /// no C64 counts frames, one every PAL frame. A pause that came into effect raises b0.
    fn poll(&mut self, now: u64, irq: &mut IrqState) {
        self.now = now;
        let (vic, paused, period, starts, caps) = {
            let l = lock(&self.link);
            (l.frames, l.paused, l.frame_clocks, l.pause_starts, l.sb.cap_events)
        };
        if starts != self.pause_seen {
            self.pause_seen = starts;
            self.cause |= CAUSE_STOP;
            self.update_irq(irq);
        }
        if caps != self.cap_seen {
            self.cap_seen = caps;
            self.cause |= CAUSE_CAPTURE;
            self.update_irq(irq);
        }
        let ev = dbg::lock(&lock(&self.link).dbg).ev;
        if ev != self.dbg_seen {
            // trx_dbg.v:295 `ev`, top.v:1328: every stop of the debugger raises b0, as a halt does.
            self.dbg_seen = ev;
            self.cause |= CAUSE_STOP;
            self.update_irq(irq);
        }
        let done = lock(&self.link).engine.done;
        if done != self.eng_seen {
            self.eng_seen = done;
            self.cause |= CAUSE_ENGINE;
            self.update_irq(irq);
        }
        // S45 §5: the MultiCore host follows the VIC only on a locked (1080p) row while the C64 core counts frames;
        // every other row runs free at its own rate.
        let free = self.mode_row().filter(|r| !(r.locked && vic != 0 && self.core.is_none()));
        if free.is_none() && vic != 0 && vic != self.vic_seen {
            self.vic_seen = vic;
            self.frame_start(now, irq);
        } else if free.is_some() || vic == 0 || paused {
            let period = match free {
                Some(r) => r.frame_clocks(),
                None if vic != 0 && period != 0 => period,
                None => PAL_FRAME,
            };
            let due = self.frame_at + period;
            if now >= due {
                self.frame_start(now, irq);
                // On the cadence, unless the poll came a whole period late.
                if now < due + period {
                    self.frame_at = due;
                }
            }
        }
    }

    /// trx_io.v `irq = (irq_en & cause) != 0`.
    fn update_irq(&self, irq: &mut IrqState) {
        irq.set_high(IRQ_HIGH_BIT, self.irq_en & self.cause != 0);
    }

    /// Byte 3 of page 6 register `reg` with the staged bytes: the commit (pix_layer.v, the `default` branch).
    fn commit(&mut self, reg: u32, v: u32) {
        let mc = self.multicore();
        let s = &mut self.set;
        match reg {
            STILL_CTRL => lock(&self.link).still.on = v & 1 != 0,
            STILL_BASE => lock(&self.link).still.base = v & !0x0F,
            // S45: the MultiCore host keeps b6 (smoothing).
            PX_CTRL => s.ctrl = (v & if mc { 0x7F } else { 0x3F }) as u8,
            // Shown from the next frame start, without a flip and without b7 (the fetch takes `c_front` there).
            PX_FRONT => self.front = v & !0x0F,
            PX_BACK => {
                self.back = v & !0x0F;
                self.flip = true;
            }
            PX_SIZE => (s.w, s.h) = (v as u16, (v >> 16) as u16),
            PX_STRIDE => s.stride = (v & 0xFFF0) as u16,
            PX_POS => (s.x0, s.y0) = (v as u16, (v >> 16) as u16),
            PX_GALPHA => s.galpha = v as u8,
            PX_KEY => s.key = v & 0x1_FFFF,
            PX_OSIZE if mc => (s.ow, s.oh) = (v as u16, (v >> 16) as u16),
            _ => {}
        }
    }
}

impl IoDevice for Trx {
    fn name(&self) -> &'static str {
        "trx"
    }

    fn read8(&mut self, off: u32, ctx: &mut IoCtx) -> u8 {
        self.poll(ctx.now, ctx.irq);
        let reg = off & 0xFFF;
        if self.core.is_some() && CoreProfile::owns(off) {
            return self.peek8(off);
        }
        if off & 3 == 0 {
            if far(off) {
                self.far_latch = self.sys_read(off);
                // trx_sys.v:358-364: an SB_DATA read moves SB_ADDR on, after it latched the word.
                if off == PAGE1 + SB_DATA {
                    self.sb_a = (self.sb_a + 1) & 0xFFF;
                }
                return self.far_latch as u8;
            } else if off & !0xFFF == PAGE6 {
                self.latch = self.bus_reg(reg);
                if tx_sel(reg) {
                    self.tx_latch = self.tx_reg(reg);
                }
            }
        }
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        self.poll(ctx.now, ctx.irq);
        let reg = off & 0xFFF;
        if let Some(c) = self.core.as_mut().filter(|_| CoreProfile::owns(off)) {
            if !c.ro[off as usize] {
                c.bytes[off as usize] = val;
            }
            return;
        }
        if far(off) {
            match off & 3 {
                // trx_sys.v: a write answers 0, which is what the latch then holds.
                3 => {
                    self.sys_commit(off & !3, u32::from(val) << 24 | self.far_stage, ctx.now, ctx.ram);
                    self.far_latch = 0;
                    self.poll(ctx.now, ctx.irq);
                }
                b => {
                    let shift = 8 * b;
                    self.far_stage = self.far_stage & !(0xFF << shift) | u32::from(val) << shift;
                }
            }
            return;
        }
        match off & !0xFFF {
            0 => {
                match reg {
                    IRQ_EN => self.irq_en = val,
                    IRQ_CAUSE => self.cause &= !val,
                    r @ RS8..=0x04D => {
                        let i = (r - RS8) as usize;
                        self.rs8[i] = val & RS8_MASK[i];
                        if r >= RS8_BYPASS && !self.resid8_absent {
                            // The engines take it at the C64's clock (trx_sid.v `cfg_r`), so through the port.
                            let mut l = lock(&self.link);
                            l.rs8_bypass[(r - RS8_BYPASS) as usize] = val & 1 != 0;
                            l.ops.push(SysWrite { at: ctx.now, off: r, v: u32::from(val & 1) });
                        }
                    }
                    _ => {}
                }
                self.update_irq(ctx.irq);
            }
            PAGE6 => {
                // c64_text.v keeps its own byte 0 and commits on byte 3 of TX_CTRL only.
                match reg {
                    TX_CTRL => self.tx_stage = val,
                    r if r == TX_CTRL + 3 => self.tx_ctrl = self.tx_stage & 3,
                    _ => {}
                }
                match reg & 3 {
                    3 => self.commit(reg & !3, u32::from(val) << 24 | self.stage),
                    b => {
                        let shift = 8 * b;
                        self.stage = self.stage & !(0xFF << shift) | u32::from(val) << shift;
                    }
                }
            }
            _ => {}
        }
    }

    /// A read without its side effect: bytes 1-3 come from the latch of the last byte-0 read (top.v ORs pix_layer's
    /// and c64_text.v's read data).
    fn peek8(&self, off: u32) -> u8 {
        let reg = off & 0xFFF;
        let byte = 8 * (off & 3);
        if let Some(c) = &self.core {
            match off {
                0..=3 => return c.id[off as usize],
                CAPS_OFF => return c.caps,
                o if CoreProfile::owns(o) => return c.bytes[o as usize],
                _ => {}
            }
        }
        if far(off) {
            return match off & 3 {
                0 => self.sys_reg(off) as u8,
                _ => (self.far_latch >> byte) as u8,
            };
        }
        match off & !0xFFF {
            0 => match reg {
                0..=3 => ID[reg as usize],
                CAPS_OFF => CAPS,
                CAPS2_OFF => CAPS2 & if self.apply_absent { !CAPS2_APPLY } else { 0xFF },
                CAPS3_OFF => if self.resid8_absent { 0 } else { CAPS3 },
                r @ RS8..=0x04D => self.rs8[(r - RS8) as usize],
                IRQ_EN => self.irq_en,
                IRQ_CAUSE => self.cause,
                // BUILD 008-00B: no bitstream, no build stamp.
                _ => 0,
            },
            PAGE6 => {
                let tx = tx_sel(reg);
                match off & 3 {
                    0 => (self.bus_reg(reg) | if tx { self.tx_reg(reg) } else { 0 }) as u8,
                    _ => ((self.latch | if tx { self.tx_latch } else { 0 }) >> byte) as u8,
                }
            }
            _ => 0,
        }
    }

    fn next_event(&self) -> Option<u64> {
        (!self.core_absent).then_some(self.next_poll)
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.poll(ctx.now, ctx.irq);
        self.next_poll = ctx.now + POLL;
    }

    /// The registers' power-on values, the shared ones included; the VIC frame count and the pause the port applied
    /// stay, they are the C64's.
    fn reset(&mut self) {
        {
            let mut l = lock(&self.link);
            let (frames, clocks, paused, starts, d) = (l.frames, l.frame_clocks, l.paused, l.pause_starts, l.dbg.clone());
            // The debug block resets with the block; the backend keeps the same model.
            *dbg::lock(&d) = dbg::Dbg::default();
            *l = TrxLink::power_on(frames, clocks, paused, starts, d);
        }
        let (link, apply_absent, resid8_absent) = (self.link.clone(), self.apply_absent, self.resid8_absent);
        let mode = self.mode_start;
        let core = self.core.take().map(|mut c| {
            c.bytes.copy_from_slice(&c.reset);
            c
        });
        *self = Trx::new(link);
        (self.apply_absent, self.resid8_absent, self.core) = (apply_absent, resid8_absent, core);
        if let Some(m) = mode {
            self.set_multicore(m.row);
        }
    }

    crate::impl_as_any!();
}

/// With `--trx-io` on only (`Machine::new` has resolved auto): the block, with high IRQ 2, and the link to the C64 port.
pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    if cfg.trx_io != TrxIo::On {
        return;
    }
    let link = link();
    if let Some(port) = map.get_mut::<C64Port>() {
        port.set_trx_io(link.clone());
    }
    // S45 §2: a MultiCore run sits in the MultiCore host: its mode request answers in the overlay window, from the
    // row the hand-over page names (§4).
    let mode_row = cfg.multicore_host().then(|| {
        cfg.handover.as_ref().and_then(|h| hdmi_mode::row_of_video(h.video)).unwrap_or(hdmi_mode::ROW_POWER_ON)
    });
    if mode_row.is_some() {
        if let Some(ov) = map.get_mut::<crate::devices::overlay::Overlay>() {
            ov.mode_link = Some(link.clone());
        }
    }
    let mut trx = Trx::new(link);
    if let Some(p) = &cfg.core {
        trx.set_core(p);
    }
    if let Some(row) = mode_row {
        trx.set_multicore(row);
    }
    map.add(BASE, SIZE, Box::new(trx));
}

/// The text layer's pixels as c64_text.v fetches them, from the C64's RAM, char ROM and colour RAM with the session's
/// VIC registers (c64_text.v:106-173): the screen at bank × $4000 + `$D018` b7:4 × $400; the glyphs from the char ROM
/// at `$D018` b1 × $800 when the bank is 0 or 2 and `$D018` b3:2 = %01, else from RAM at bank × $4000 + `$D018` b3:1 ×
/// $800.
pub fn text_layer<C: Chips + ?Sized>(view: VicView, opaque: bool, c: &C) -> TrxText {
    let bank = u16::from(view.bank) << 14;
    let d018 = u16::from(view.d018);
    let screen = bank | d018 >> 4 << 10;
    let rom = view.bank & 1 == 0 && d018 >> 2 & 3 == 1;
    let chars = if rom { (d018 >> 1 & 1) << 11 } else { bank | (d018 >> 1 & 7) << 11 };
    let codes: Vec<u8> = (0..1000u16).map(|i| c.ram_read(screen | i)).collect();
    let glyphs = (0..200usize)
        .flat_map(|line| (0..40usize).map(move |col| (line, col)))
        .map(|(line, col)| {
            let a = chars | u16::from(codes[line / 8 * 40 + col]) << 3 | (line & 7) as u16;
            if rom {
                c.rom_read(C64Rom::Char, a)
            } else {
                c.ram_read(a)
            }
        })
        .collect();
    let colours = (0..1000u16).map(|i| c.io_peek(0xD800 + i) & 0x0F).collect();
    let charset = match (rom, d018 & 2 != 0) {
        (false, _) => C64Charset::Ram,
        (true, false) => C64Charset::Upper,
        (true, true) => C64Charset::Lower,
    };
    TrxText { glyphs, colours, background: view.d021, opaque, screen: codes, charset }
}

/// The still capture's window of `frame` (pix_layer.v:496-563), mapped onto TRX64's canvas as the composite maps it
/// (render.rs `draw_c64_trx`): 200 bytes a row from the slot's start, as many rows as the VIC reaches.
/// 50 Hz: 270 rows from line 16; a complete frame needs 286 lines (pix_layer.v:520, :552).
/// 60 Hz ([`still_v60`]): 240 rows, rows 0-231 lines 31-262 and rows 232-239 lines 0-7 of the same frame
/// (pix_layer.v:519, :536, :544); a complete frame needs 263 lines. A shorter 60 Hz VIC gives the rows from line 31 it
/// reaches; the device would also have written rows 232-239.
pub fn still_window(frame: &crate::c64host::C64Frame) -> Vec<u8> {
    if frame.width == 0 || frame.height == 0 {
        return Vec::new();
    }
    let (text_x, first, lines) = match frame.raster_lines {
        0 => (32, 16, 312),
        n => (frame.text_x, frame.first_line, n),
    };
    let rows: Vec<usize> = if still_v60(lines) {
        let low = STILL_H_60 - STILL_LOW_60;
        let tail = if still_complete(lines) { 0..STILL_LOW_60 } else { 0..0 };
        (STILL_Y0_60..lines.min(STILL_Y0_60 + low)).chain(tail).collect()
    } else {
        (STILL_Y0..lines.min(STILL_Y0 + STILL_H)).collect()
    };
    let cols: Vec<usize> =
        (0..STILL_W).map(|x| (crate::render::TRX_X0 + x + text_x).saturating_sub(crate::render::TRX_X_TEXT).min(frame.width - 1)).collect();
    let mut out = Vec::with_capacity(rows.len() * STILL_W / 2);
    for line in rows {
        let row = if line < first { line + lines } else { line } - first;
        let src = &frame.indices[row.min(frame.height - 1) * frame.width..][..frame.width];
        out.extend(cols.chunks(2).map(|pair| src[pair[0]] & 0x0F | (src[pair[1]] & 0x0F) << 4));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block on its own, with DDR and an ITU, driven at clock `now`.
    struct Rig {
        trx: Trx,
        link: Link,
        ram: Vec<u8>,
        irq: IrqState,
        now: u64,
    }

    impl Rig {
        fn new() -> Self {
            let link = link();
            Rig { trx: Trx::new(link.clone()), link, ram: vec![0; 1 << 20], irq: IrqState::new(), now: 0 }
        }

        fn with<R>(&mut self, f: impl FnOnce(&mut Trx, &mut IoCtx) -> R) -> R {
            let mut console = Vec::new();
            let mut ctx =
                IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut console };
            f(&mut self.trx, &mut ctx)
        }

        fn r8(&mut self, off: u32) -> u8 {
            self.with(|t, ctx| t.read8(off, ctx))
        }

        fn w8(&mut self, off: u32, val: u8) {
            self.with(|t, ctx| t.write8(off, val, ctx));
        }

        /// A 32-bit store, bytes 0 to 3 as the bus splits it.
        fn w32(&mut self, off: u32, val: u32) {
            for (i, b) in val.to_le_bytes().into_iter().enumerate() {
                self.w8(off + i as u32, b);
            }
        }

        fn r32(&mut self, off: u32) -> u32 {
            u32::from_le_bytes(std::array::from_fn(|i| self.r8(off + i as u32)))
        }

        /// The next VIC frame, seen by the next poll.
        fn frame(&mut self) {
            lock(&self.link).frames += 1;
            self.now += POLL;
            self.with(|t, ctx| t.tick(ctx));
        }

        fn tick_at(&mut self, now: u64) {
            self.now = now;
            self.with(|t, ctx| t.tick(ctx));
        }

        fn irq(&self) -> bool {
            self.irq.high_src & (1 << IRQ_HIGH_BIT) != 0
        }
    }

    #[test]
    fn page_0_reads_id_and_caps_and_other_pages_read_0() {
        let mut r = Rig::new();
        assert_eq!([r.r8(0), r.r8(1), r.r8(2), r.r8(3)], *b"TRX2");
        assert_eq!((r.r8(4), r.r8(5)), (0xEF, 0xBD), "gate, state bus, engine, debug, session, video, RGB565; F4, CPU write, list, lens, APPLY, trace");
        assert_eq!(r.r32(0x008), 0, "BUILD");
        r.w8(0x4000, 0xFF);
        assert!((0x4000..0x4010).chain(0xF000..0xF004).chain(0x2000..0x2004).chain(0x104..0x140).all(|o| r.r8(o) == 0));
        assert!((0x140..0x1000).step_by(0x3C).all(|o| r.r8(o) == 0), "140-FFF");
    }

    /// S44: an A1200 profile: its ID, CAPS, the read-only feature register, registers that read back across the
    /// window, a reset back to the profile's values; IRQ_EN and the pixel layer stay the host's; no C64 page answers.
    #[test]
    fn a_core_profile_answers_for_its_core() {
        let mut p = CoreProfile::new("amiga1200", *b"AMIG");
        p.ro.insert(0x00C, 0x18);
        p.regs.insert(0x0A0, 0x01);
        let mut r = Rig::new();
        r.trx.set_core(&p);
        assert_eq!([r.r8(0), r.r8(1), r.r8(2), r.r8(3)], *b"AMIG");
        assert_eq!(r.r8(4), 0xC0, "the pixel layer and RGB565 only");
        assert_eq!((r.r8(5), r.r8(6)), (0, 0), "no CAPS2, no CAPS3");
        r.w8(0x00C, 0);
        assert_eq!(r.r8(0x00C), 0x18, "AM_FEAT is read-only");
        assert_eq!(r.r8(0x0A0), 0x01, "AM_CTRL's reset value: HOLD");
        r.w8(0x0A0, 0x00);
        r.w32(0x1A4, 0x1234_5678);
        r.w8(0xE021, 0x01);
        assert_eq!((r.r8(0x0A0), r.r32(0x1A4), r.r8(0xE021)), (0, 0x1234_5678, 1));
        r.w8(GATE_CTRL, 1);
        assert!(!lock(&r.link).frz && lock(&r.link).ops.is_empty(), "GATE_CTRL is a core register now, not the gate");
        r.w8(IRQ_EN, CAUSE_VSYNC);
        assert_eq!(r.r8(IRQ_EN), CAUSE_VSYNC, "IRQ_EN stays the host's");
        r.w32(PAGE6 + PX_GALPHA, 0x80);
        assert_eq!(r.trx.reg(PX_GALPHA), 0x80, "page 6 stays the pixel layer");
        r.trx.reset();
        assert_eq!((r.r8(0x0A0), r.r32(0x1A4), r.r8(0x00C)), (1, 0, 0x18));
        assert_eq!(r.r8(0), b'A', "a reset keeps the core");
    }

    /// S43 S1: CAPS3 b0 and the reSID8 filter control at its reset values (trx_io.v: 0, 3, 245, -780, no bypass).
    #[test]
    fn caps3_and_the_resid8_reset_values() {
        let mut r = Rig::new();
        assert_eq!(r.r8(0x006), 0x01, "CAPS3 b0, the 8-SID reDIP");
        let bytes: Vec<u8> = (0x040..0x04E).map(|o| r.r8(o)).collect();
        assert_eq!(bytes, [0, 0, 3, 3, 0xF5, 0, 0xF4, 0x04, 0xF5, 0, 0xF4, 0x04, 0, 0]);
        assert_eq!(r.r8(0x04E), 0, "past the block");
    }

    /// S43 S2: each byte keeps its bits; the bypass goes to the C64 port as an op; an FPGA reset restores them.
    #[test]
    fn the_resid8_registers_read_back_masked() {
        let mut r = Rig::new();
        for o in 0x040..0x04E {
            r.w8(o, 0xFF);
        }
        let bytes: Vec<u8> = (0x040..0x04E).map(|o| r.r8(o)).collect();
        assert_eq!(bytes, [3, 3, 3, 3, 0xFF, 1, 0xFF, 7, 0xFF, 1, 0xFF, 7, 1, 1]);
        {
            let l = lock(&r.link);
            assert_eq!(l.rs8_bypass, [true, true]);
            let ops: Vec<(u32, u32)> = l.ops.iter().map(|op| (op.off, op.v)).collect();
            assert_eq!(ops, [(RS8_BYPASS, 1), (RS8_BYPASS + 1, 1)], "only the bypass acts on the machine");
        }
        IoDevice::reset(&mut r.trx);
        assert_eq!((r.r8(0x044), r.r8(0x04C)), (0xF5, 0));
        assert_eq!(lock(&r.link).rs8_bypass, [false, false]);
    }

    /// S43: `--absent resid8`, the two-SID reDIP: CAPS3 0, the bytes still read and write, no bypass for the port; a
    /// reset keeps the build switch.
    #[test]
    fn without_resid8_caps3_is_0_and_the_bypass_goes_nowhere() {
        let mut r = Rig::new();
        r.trx.set_resid8_absent(true);
        assert_eq!(r.r8(0x006), 0);
        r.w8(0x04C, 1);
        assert_eq!(r.r8(0x04C), 1);
        assert!(lock(&r.link).ops.is_empty() && lock(&r.link).rs8_bypass == [false, false]);
        IoDevice::reset(&mut r.trx);
        assert_eq!(r.r8(0x006), 0);
    }

    /// S43 S6: page 7, the slot block: SLOT_CTRL reads back at once and queues for the C64's clock; SLOT_STATUS is what
    /// the port last published, with the block's b31 and b18 from ROM_WR_EN.
    #[test]
    fn page_7_is_the_slot_block() {
        let mut r = Rig::new();
        assert_eq!((r.r32(PAGE7 + SLOT_CTRL), r.r32(PAGE7 + SLOT_STATUS)), (0, 0x8000_F01F));
        r.w32(PAGE7 + SLOT_CTRL, 0xFFFF_FFFF);
        assert_eq!(r.r32(PAGE7 + SLOT_CTRL), SLOT_WR_EN, "b1 only");
        assert_eq!(r.r32(PAGE7 + SLOT_STATUS), 0x8004_F01F, "b18 the ROM_WR_EN applied");
        assert_eq!(lock(&r.link).ops.last().map(|op| (op.off, op.v)), Some((PAGE7 + SLOT_CTRL, 0xFFFF_FFFF)));
        lock(&r.link).slot_status = 0x0001_0704;
        assert_eq!(r.r32(PAGE7 + SLOT_STATUS), 0x8005_0704);
        assert!((0x008..0x014).all(|o| r.r8(PAGE7 + o) == 0), "SLOT_CNT0/1, SLOT_LAST");
    }

    /// S41 §2: without APPLY (trx_engine.v and trx_io.v `NO_APPLY`) CAPS2 b5 reads 0, the AP_* writes take nothing and
    /// AP_CMD never reads busy; a reset keeps the build switch. t80-lean-ring's $F7 reads $D7 that way, UE2's $BD $9D.
    #[test]
    fn without_apply_caps2_b5_is_clear_and_no_job_starts() {
        let mut r = Rig::new();
        r.trx.set_apply_absent(true);
        assert_eq!(r.r8(5), 0x9D);
        r.w32(engine::PAGE2 + engine::AP_START, 0);
        r.w32(engine::PAGE2 + engine::AP_COUNT, 16);
        r.w32(engine::PAGE2 + engine::AP_CMD, 0);
        assert_eq!(r.r32(engine::PAGE2 + engine::AP_CMD), 0, "never busy");
        assert!(lock(&r.link).engine.ap_jobs.is_empty() && lock(&r.link).ops.is_empty(), "no job for the C64 side");
        IoDevice::reset(&mut r.trx);
        assert_eq!(r.r8(5), 0x9D, "a build switch outlives a reset");
        r.trx.set_apply_absent(false);
        r.w32(engine::PAGE2 + engine::AP_COUNT, 16);
        r.w32(engine::PAGE2 + engine::AP_CMD, 0);
        assert_eq!((r.r8(5), r.r32(engine::PAGE2 + engine::AP_CMD)), (0xBD, 1), "with APPLY the job waits for the C64 side");
    }

    /// S41 §5: without a core the block's interrupt drops and it stops polling; back, it polls again.
    #[test]
    fn an_absent_core_drops_the_interrupt_and_stops_polling() {
        let mut r = Rig::new();
        r.w8(IRQ_EN, CAUSE_VSYNC);
        r.frame();
        assert!(r.irq());
        r.trx.set_core_absent(true, r.now, &mut r.irq);
        assert!(!r.irq() && r.trx.next_event().is_none());
        r.trx.set_core_absent(false, 1000, &mut r.irq);
        assert_eq!(r.trx.next_event(), Some(1000 + POLL));
    }

    #[test]
    fn the_far_pages_share_one_stage_and_one_latch() {
        let mut r = Rig::new();
        // SH_CTRL resets to mode 1.
        assert_eq!(r.r32(PAGE5 + SH_CTRL), 1);
        // Bytes 0-2 collect in one buffer for every far page: byte 3 of SH_CTRL commits what was written at GATE_CTRL.
        r.w8(GATE_CTRL, 0x0E);
        r.w8(PAGE5 + SH_CTRL + 3, 0);
        assert_eq!(r.r32(PAGE5 + SH_CTRL), 0x0E, "mode 2, SIDEFX, PROTO");
        assert_eq!(r.r32(GATE_CTRL), 0, "GATE_CTRL never committed");
        // Byte 0 latches for bytes 1-3, across pages; a commit leaves the latch 0.
        r.w32(PAGE5 + SH_CTRL, 1);
        lock(&r.link).session.count = 0x1234;
        assert_eq!(r.r8(PAGE5 + SH_STATUS), 0);
        lock(&r.link).session.count = 0x5678;
        assert_eq!([r.r8(PAGE5 + SH_STATUS + 2), r.r8(GATE_CTRL + 3)], [0x34, 0x12], "the latch of the byte-0 read");
        r.r8(PAGE5 + SH_STATUS);
        r.w32(GATE_CTRL, 1);
        assert_eq!(r.r8(PAGE5 + SH_STATUS + 2), 0, "a commit answers 0");
        // GATE_CTRL: b0 RW, b8 and b9 the pause.
        assert_eq!(r.r32(GATE_CTRL), 0x001);
        assert_eq!(lock(&r.link).ops, [SysWrite { at: 0, off: GATE_CTRL, v: 1 }], "for the port, at its clock");
        lock(&r.link).paused = true;
        assert_eq!(r.r32(GATE_CTRL), 0x301);
    }

    /// S39 §1.1, §1.3, §1.4: the debug block's and the lens unit's registers on trx_sys.v's byte rule: the CYCLES read
    /// latches CYCLES_HI, a BMP_DATA read moves BMP_IDX, writes go to the port at their clock, LK_RES reads the result.
    #[test]
    fn the_debug_block_and_the_lens_on_the_byte_rule() {
        let mut r = Rig::new();
        let d = lock(&r.link).dbg.clone();
        (dbg::lock(&d).cycles, dbg::lock(&d).instrs) = (0x0002_1234_5678, 9);
        assert_eq!(r.r32(dbg::CYCLES), 0x1234_5678);
        dbg::lock(&d).cycles = 0x0005_0000_0000;
        assert_eq!((r.r32(dbg::CYCLES_HI), r.r32(dbg::INSTRS)), (2, 9), "CYCLES_HI as the CYCLES read latched it");
        assert_eq!(r.r32(dbg::PAGE3 + dbg::STATUS), 0x4, "an IB, nothing else");
        dbg::lock(&d).bmp[5] = 0x55;
        dbg::lock(&d).bmp_idx = 5;
        assert_eq!(r.r32(dbg::PAGE3 + dbg::BMP_DATA), 0x55);
        assert_eq!(dbg::lock(&d).bmp_idx, 6);
        r.w32(dbg::PAGE3 + dbg::CMD, 2);
        r.w32(PAGE5 + LK_REQ, 0x1_8000);
        let ops = lock(&r.link).ops.clone();
        assert_eq!(ops, [SysWrite { at: 0, off: dbg::PAGE3 + dbg::CMD, v: 2 }, SysWrite { at: 0, off: PAGE5 + LK_REQ, v: 0x1_8000 }]);
        lock(&r.link).lk_res = LK_DONE | LK_HIT | 0x42;
        assert_eq!(r.r32(PAGE5 + LK_RES), 0x342);
    }

    #[test]
    fn page_6_defaults_follow_pix_layer() {
        let mut r = Rig::new();
        let regs = [STILL_CTRL, STILL_BASE, STILL_STATUS, PX_CTRL, PX_FRONT, PX_SIZE, PX_STRIDE, PX_POS, PX_GALPHA, PX_KEY];
        let want = [1, 0x0505_0000, 0, 0x22, 0, 540 << 16 | 960, 1920, 0, 255, 0];
        for (reg, want) in regs.into_iter().zip(want) {
            assert_eq!(r.trx.reg(reg), want, "{reg:#05x}");
        }
        assert!(!r.trx.shown(), "0x22 is format 1 but not on");
    }

    #[test]
    fn bytes_0_to_2_stage_and_byte_3_commits() {
        let mut r = Rig::new();
        for (i, b) in [0x34, 0x12, 0x78].into_iter().enumerate() {
            r.w8(PAGE6 + PX_POS + i as u32, b);
        }
        assert_eq!(r.trx.reg(PX_POS), 0, "staged, not committed");
        r.w8(PAGE6 + PX_POS + 3, 0x05);
        assert_eq!(r.trx.reg(PX_POS), 0x0578_1234);
        // One stage for all registers: byte 3 commits the register it is written to.
        r.w8(PAGE6 + PX_SIZE, 0xAA);
        r.w8(PAGE6 + PX_GALPHA + 3, 0);
        assert_eq!(r.trx.reg(PX_GALPHA), 0xAA);
        // Masks on commit: addresses and the stride to 16 bytes, PX_CTRL 6 bits, the key 17.
        r.w32(PAGE6 + PX_FRONT, 0x0123_456F);
        r.w32(PAGE6 + PX_STRIDE, 0xFFFF_FFFF);
        r.w32(PAGE6 + PX_CTRL, 0xFF);
        r.w32(PAGE6 + PX_KEY, 0xFFFF_FFFF);
        let got = [PX_FRONT, PX_STRIDE, PX_CTRL, PX_KEY].map(|reg| r.trx.reg(reg));
        assert_eq!(got, [0x0123_4560, 0xFFF0, 0x3F, 0x1_FFFF]);
        r.w32(PAGE6 + STILL_BASE, 0x0012_345F);
        r.w32(PAGE6 + STILL_CTRL, 0);
        let still = lock(&r.link).still;
        assert_eq!((still.base, still.on), (0x0012_3450, false), "the port captures with them");
    }

    #[test]
    fn byte_0_read_latches_bytes_1_to_3() {
        let mut r = Rig::new();
        r.frame();
        assert_eq!(r.r8(PAGE6 + PX_STATUS) & 1, 0);
        r.frame();
        assert_eq!(r.r8(PAGE6 + PX_STATUS + 2), 1, "the frame count from the latch of the first read");
        assert_eq!(r.r32(PAGE6 + PX_STATUS) >> 16, 2, "a new byte-0 read takes the new value");
    }

    #[test]
    fn configuration_registers_are_write_only() {
        let mut r = Rig::new();
        r.w32(PAGE6 + PX_SIZE, 0x0111_0222);
        r.w32(PAGE6 + STILL_BASE, 0x0012_3450);
        let regs = [STILL_CTRL, STILL_BASE, PX_CTRL, PX_FRONT, PX_BACK, PX_SIZE, PX_STRIDE, PX_POS, PX_GALPHA, PX_KEY];
        assert_eq!(regs.map(|reg| r.r32(PAGE6 + reg)), [0; 10], "pix_layer.v reads 0 there (28d8b69)");
        assert_eq!(r.trx.reg(PX_SIZE), 0x0111_0222, "committed all the same");
    }

    #[test]
    fn tx_ctrl_is_c64_texts_own_register() {
        let mut r = Rig::new();
        assert_eq!(r.r32(PAGE6 + TX_CTRL), 0x0100_0000, "b31:24 = 1: the text layer is there");
        r.w32(PAGE6 + TX_CTRL, 0xFFFF_FF03);
        assert_eq!(r.r32(PAGE6 + TX_CTRL), 0x0100_0003, "b1:0 only");
        assert_eq!(r.r32(PAGE6 + TX_STATUS), 0, "no session: not shown");
        // Its own byte-0 stage: byte 3 of TX_CTRL takes byte 0, whatever the pixel layer's stage holds.
        r.w8(PAGE6 + TX_CTRL, 0x01);
        r.w8(PAGE6 + PX_GALPHA, 0x02);
        r.w8(PAGE6 + TX_CTRL + 3, 0);
        assert_eq!(r.r32(PAGE6 + TX_CTRL), 0x0100_0001);
        // Shown with a session in text mode; ECM, BMM or MCM hide it.
        lock(&r.link).session.active = true;
        assert_eq!(r.r32(PAGE6 + TX_STATUS), 1);
        assert!(r.trx.text().is_some());
        r.w32(PAGE6 + TX_CTRL, 0);
        assert_eq!((r.r32(PAGE6 + TX_STATUS), r.trx.text()), (0, None), "without show");
    }

    #[test]
    fn a_flip_lands_at_the_frame_start_and_raises_b7() {
        let mut r = Rig::new();
        r.w32(PAGE6 + PX_FRONT, 0x10_0000);
        r.frame();
        r.w8(IRQ_CAUSE, 0xFF);
        r.w32(PAGE6 + PX_BACK, 0x20_0000);
        assert_eq!(r.r32(PAGE6 + PX_STATUS) & 1, 1, "flip pending");
        assert_eq!((r.trx.reg(PX_FRONT), r.r8(IRQ_CAUSE)), (0x10_0000, 0), "nothing before the frame start");
        r.now += POLL;
        r.with(|t, ctx| t.tick(ctx));
        assert_eq!(r.r8(IRQ_CAUSE), 0, "no VIC frame, no frame start");
        r.frame();
        assert_eq!(r.trx.reg(PX_FRONT), 0x20_0000);
        assert_eq!(r.r8(IRQ_CAUSE), CAUSE_FLIP | CAUSE_VSYNC);
        assert_eq!(r.r32(PAGE6 + PX_STATUS) & 1, 0);
        // PX_FRONT: from the next frame start, without b7.
        r.w8(IRQ_CAUSE, 0xFF);
        r.w32(PAGE6 + PX_FRONT, 0x30_0000);
        assert_eq!(r.trx.shown.front, 0x20_0000);
        r.frame();
        assert_eq!((r.trx.shown.front, r.r8(IRQ_CAUSE)), (0x30_0000, CAUSE_VSYNC));
    }

    #[test]
    fn frame_starts_count_and_set_vertical_blank() {
        let mut r = Rig::new();
        r.frame();
        r.frame();
        let status = r.r32(PAGE6 + PX_STATUS);
        assert_eq!((status >> 16, status >> 1 & 1), (2, 1), "two frames, in the blank");
        r.now += VBLANK;
        assert_eq!(r.r32(PAGE6 + PX_STATUS) >> 1 & 1, 0, "45 lines later");
        // Without a C64 counting frames: every PAL frame.
        let mut r = Rig::new();
        r.tick_at(PAL_FRAME - 1);
        assert_eq!(r.trx.frames, 0);
        r.tick_at(PAL_FRAME);
        assert_eq!(r.trx.frames, 1);
    }

    #[test]
    fn frame_starts_go_on_while_paused_one_per_frame_period() {
        let mut r = Rig::new();
        let period = 20 * CLOCKS_PER_MS;
        lock(&r.link).frame_clocks = period;
        r.frame();
        let start = r.now;
        // The pause: the VIC count stands, the frame starts keep the frame period from the last one.
        {
            let mut l = lock(&r.link);
            l.paused = true;
            l.pause_starts += 1;
        }
        r.tick_at(start + 1);
        assert_eq!((r.trx.frames, r.r8(IRQ_CAUSE) & CAUSE_STOP), (1, CAUSE_STOP), "b0: the pause came into effect");
        let mut at = start;
        for n in 2..=11 {
            at += period;
            r.tick_at(at - 1);
            assert_eq!(r.trx.frames, n - 1);
            r.tick_at(at + POLL / 2);
            assert_eq!(r.trx.frames, n, "frame {n} on time");
        }
        // The release: the VIC frame count moves again and the next frame start is the VIC's.
        lock(&r.link).paused = false;
        r.tick_at(at + period + POLL);
        assert_eq!(r.trx.frames, 11, "running: only the VIC count makes frame starts");
        r.frame();
        assert_eq!(r.trx.frames, 12);
    }

    #[test]
    fn irq_cause_is_write_1_to_clear_and_drives_high_irq_2() {
        let mut r = Rig::new();
        r.frame();
        r.w32(PAGE6 + PX_BACK, 0);
        r.frame();
        assert_eq!(r.r8(IRQ_CAUSE), 0xC0);
        assert!(!r.irq(), "IRQ_EN 0");
        r.w8(IRQ_EN, CAUSE_FLIP);
        assert!(r.irq());
        r.w8(IRQ_CAUSE, CAUSE_VSYNC);
        assert_eq!(r.r8(IRQ_CAUSE), CAUSE_FLIP, "b6 cleared, b7 kept");
        assert!(r.irq());
        r.w8(IRQ_CAUSE, CAUSE_FLIP);
        assert!(!r.irq());
        r.w8(IRQ_EN, CAUSE_VSYNC);
        r.frame();
        assert!(r.irq(), "the next frame start");
        assert_eq!(r.r8(IRQ_EN), CAUSE_VSYNC);
    }

    #[test]
    fn the_layer_reads_its_front_buffer_from_ddr() {
        let mut r = Rig::new();
        r.w32(PAGE6 + PX_CTRL, 0x13); // on, RGB565, scale 1
        r.w32(PAGE6 + PX_SIZE, 2 << 16 | 3);
        r.w32(PAGE6 + PX_STRIDE, 32);
        r.w32(PAGE6 + PX_POS, 5 << 16 | 7);
        r.w32(PAGE6 + PX_GALPHA, 128);
        r.w32(PAGE6 + PX_KEY, 0x1_F81F);
        r.w32(PAGE6 + PX_FRONT, 0x1000);
        for (i, px) in [1u16, 2, 3].into_iter().enumerate() {
            r.ram[0x1000 + 2 * i..][..2].copy_from_slice(&px.to_le_bytes());
            r.ram[0x1020 + 2 * i..][..2].copy_from_slice(&(px + 10).to_le_bytes());
        }
        assert_eq!(r.trx.layer(&r.ram), None, "settings wait for the frame start");
        r.frame();
        let layer = r.trx.layer(&r.ram).expect("on");
        assert_eq!(
            layer,
            TrxLayer {
                x0: 7,
                y0: 5,
                w: 3,
                h: 2,
                ow: 3,
                oh: 2,
                alpha: 129,
                key: Some(0xF81F),
                pixels: vec![1, 2, 3, 11, 12, 13],
                ..TrxLayer::default()
            }
        );
        // Past the end of DDR: 0. The address keeps bits 26:0.
        r.w32(PAGE6 + PX_FRONT, 0x0800_1000 + (1 << 20) - 2);
        r.frame();
        assert_eq!(r.trx.layer(&r.ram).unwrap().pixels[..2], [0, 0]);
        r.w32(PAGE6 + PX_FRONT, 0x0800_1000);
        r.frame();
        assert_eq!(r.trx.layer(&r.ram).unwrap().pixels[..3], [1, 2, 3], "bit 27 is not an address bit");
    }

    #[test]
    fn the_layer_is_off_unless_on_format_1_and_inside_the_frame() {
        let on = |ctrl: u8, w: u16, h: u16, x0: u16, y0: u16| {
            Shown::of(&Settings { ctrl, w, h, x0, y0, ..Settings::DEFAULT }, 0, None).on
        };
        assert!(on(0x23, 960, 540, 1919, 1079));
        assert!(!on(0x22, 960, 540, 0, 0), "b0");
        assert!(!on(0x25, 960, 540, 0, 0), "format 2");
        assert!(!on(0x23, 0, 540, 0, 0) && !on(0x23, 960, 0, 0, 0), "empty");
        assert!(!on(0x23, 960, 540, 1920, 0) && !on(0x23, 960, 540, 0, 1080), "outside");
        let s = Shown::of(&Settings { ctrl: 0x03, w: 2000, h: 2000, galpha: 0, ..Settings::DEFAULT }, 0, None);
        assert_eq!((s.w, s.h, s.ow, s.oh, s.alpha), (960, 1080, 960, 1080, 0), "clamped, scale 0 is 1");
        let s = Shown::of(&Settings { ctrl: 0x33, ..Settings::DEFAULT }, 0, None);
        assert_eq!((s.ow, s.oh), (2880, 1620), "scale 3: S · w by S · h");
    }

    #[test]
    fn the_still_fills_slots_0_1_0_and_counts() {
        let mut s = Still::POWER_ON;
        assert_eq!(s.status(), 0);
        assert_eq!(s.frames(1, true, false), 0, "the first frame goes to slot 0");
        assert_eq!(s.status(), 1 << 8 | 0 << 1 | 1);
        assert_eq!(s.frames(1, true, false), 1);
        assert_eq!(s.frames(1, true, false), 0);
        assert_eq!(s.status(), 3 << 8 | 1);
        // Three frames in one sync: the newest goes where the device would have put it, all three count.
        assert_eq!(s.frames(3, true, false), 1);
        assert_eq!(s.status(), 6 << 8 | 1 << 1 | 1);
        // A frame that never completes goes to the slot after the last and changes nothing.
        let before = s;
        assert_eq!(s.frames(5, false, false), 0);
        assert_eq!(s, before);
        // STILL_STATUS b2 is latched with the slot: the mode of the frame in it.
        assert_eq!(s.frames(1, true, true), 0);
        assert_eq!(s.status(), 7 << 8 | 1 << 2 | 1);
        assert_eq!(s.frames(1, true, false), 1);
        assert_eq!(s.status(), 8 << 8 | 1 << 1 | 1);
    }

    #[test]
    fn a_still_completes_at_line_285_or_at_60_hz_on_line_262() {
        assert!(still_complete(0) && still_complete(312) && still_complete(286));
        assert!(!still_v60(300) && !still_v60(0), "300 lines and more, or unknown: 50 Hz");
        assert!(still_v60(263) && still_complete(263));
        assert!(still_v60(262) && !still_complete(262), "line 262 never comes");
    }

    #[test]
    fn the_60_hz_still_is_lines_31_to_262_then_0_to_7() {
        use crate::c64host::C64Frame;
        // A canvas of all 263 lines from line 0, the pixel's index its line; text area at column 32 as in PAL.
        let (w, h) = (384, 263);
        let indices = (0..w * h).map(|i| (i / w) as u8 & 0x0F).collect();
        let frame =
            C64Frame { width: w, height: h, indices, text_x: 32, first_line: 0, raster_lines: 263, ..C64Frame::default() };
        let still = still_window(&frame);
        assert_eq!(still.len(), 48_000, "400 x 240, 3,000 words");
        let row = |r: usize| still[r * 200] & 0x0F;
        for (r, line) in [(0, 31), (1, 32), (231, 262), (232, 0), (239, 7)] {
            assert_eq!(row(r), line as u8 & 0x0F, "row {r} is line {line}");
        }
        // A 262-line VIC never reaches line 262: the rows from line 31 it reaches, nothing completes.
        let short = C64Frame { raster_lines: 262, ..frame };
        assert_eq!(still_window(&short).len(), 231 * 200);
    }

    #[test]
    fn the_still_window_packs_two_pixels_a_byte_low_nibble_first() {
        use crate::c64host::C64Frame;
        // A PAL canvas of 384 × 272 from raster line 16, text area at column 32: pixel (x, y) of the window is canvas
        // column 96 + x + 32 − 136 = x − 8, clamped to the canvas.
        let (w, h) = (384, 272);
        let indices = (0..w * h).map(|i| ((i % w) as u8 ^ (i / w) as u8) & 0x0F).collect();
        let frame = C64Frame { width: w, height: h, indices, text_x: 32, first_line: 16, raster_lines: 312, ..C64Frame::default() };
        let px = |x: usize, y: usize| ((x.saturating_sub(8).min(w - 1) as u8) ^ y as u8) & 0x0F;
        let still = still_window(&frame);
        assert_eq!(still.len(), 54_000);
        let nibble = |k: usize| still[k / 2] >> (4 * (k % 2)) & 0x0F;
        for k in [0, 1, 399, 400, 107_999] {
            assert_eq!(nibble(k), px(k % 400, k / 400), "pixel {k}");
        }
        // A 263-line VIC: the 60 Hz window, 240 rows.
        let ntsc = C64Frame { raster_lines: 263, first_line: 28, height: 235, indices: vec![1; w * 235], ..frame };
        assert_eq!(still_window(&ntsc).len(), 48_000);
    }

    #[test]
    fn install_follows_the_config() {
        let mut cfg = crate::devices::board::rig::cfg();
        let mut map = IoMap::new();
        install(&mut map, &cfg);
        assert!(map.resolve(BASE).is_none(), "auto, unresolved, is off: 0x101A0000 stays unmapped");
        cfg.trx_io = TrxIo::On;
        let mut map = IoMap::new();
        crate::devices::c64::install(&mut map, &cfg);
        install(&mut map, &cfg);
        assert_eq!(map.resolve(BASE + 0xFFFF).map(|(_, off)| off), Some(0xFFFF));
    }

    impl Rig {
        /// S45: the block in the MultiCore host, at `row`.
        fn multicore(row: u8) -> Self {
            let mut r = Rig::new();
            r.trx.set_multicore(row);
            r
        }

        fn request(&mut self, row: u8) {
            lock(&self.link).mode.as_mut().unwrap().write(hdmi_mode::REQ, row);
        }
    }

    /// S45 T3: off the 1080p rows the frame runs free at the row's rate, vertical blank for its first Bv lines; on a
    /// 1080p row the C64's frames start it, as on TRX64's host.
    #[test]
    fn t3_the_multicore_frame_follows_the_row() {
        let mut r = Rig::multicore(6);
        let row = hdmi_mode::row_of(6).unwrap();
        let (period, blank) = (row.frame_clocks(), row.lines_clocks(45));
        assert_eq!(row.yact0(), 45);
        r.tick_at(period);
        assert_eq!(r.trx.frame_starts(), 1);
        assert_eq!(r.trx.reg(PX_STATUS) & 2, 2, "in the vertical blank");
        r.tick_at(period + blank);
        assert_eq!(r.trx.reg(PX_STATUS) & 2, 0, "after Bv lines");
        // The VIC's frames do not start a 480p frame.
        lock(&r.link).frames = 5;
        r.tick_at(period + blank + POLL);
        assert_eq!(r.trx.frame_starts(), 1);
        r.tick_at(2 * period);
        assert_eq!(r.trx.frame_starts(), 2);
        // 1080p 50: the VIC's.
        r.request(2);
        r.frame();
        assert_eq!(r.trx.frame_starts(), 3);
        r.frame();
        assert_eq!(r.trx.frame_starts(), 4);
        // A core from a profile counts no VIC frames: 1080p runs free too.
        let mut r = Rig::multicore(2);
        r.tick_at(hdmi_mode::row_of(2).unwrap().frame_clocks());
        assert_eq!(r.trx.frame_starts(), 1);
    }

    /// S45 §6: PX_OSIZE and b6 are the MultiCore host's; the layer is on inside the active area, in format 1 or 2, and
    /// smooths an axis only where the source is not larger than the output.
    #[test]
    fn s45_the_multicore_layer_takes_osize_format_2_and_smoothing() {
        let mut r = Rig::multicore(6);
        r.w32(PAGE6 + PX_CTRL, 0x45); // on, format 2, smoothing
        r.w32(PAGE6 + PX_SIZE, 2 << 16 | 3);
        r.w32(PAGE6 + PX_STRIDE, 16);
        r.w32(PAGE6 + PX_POS, 479 << 16 | 639);
        r.w32(PAGE6 + PX_OSIZE, 4 << 16 | 6);
        r.w32(PAGE6 + PX_KEY, 0x1_0000);
        r.w32(PAGE6 + PX_FRONT, 0x1000);
        for (i, px) in [0x0011_2233u32, 0xFF44_5566, 0x0077_8899].into_iter().enumerate() {
            r.ram[0x1000 + 4 * i..][..4].copy_from_slice(&px.to_le_bytes());
        }
        assert_eq!(r.trx.reg(PX_OSIZE), 4 << 16 | 6);
        assert_eq!(r.trx.reg(PX_CTRL), 0x45);
        r.tick_at(hdmi_mode::row_of(6).unwrap().frame_clocks());
        let l = r.trx.layer(&r.ram).expect("on");
        assert_eq!((l.x0, l.y0, l.w, l.h, l.ow, l.oh, l.wide, l.key), (639, 479, 3, 2, 6, 4, true, None));
        assert_eq!((l.smooth_h, l.smooth_v), (true, true));
        assert_eq!(l.pixels[..3], [0x11_2233, 0x44_5566, 0x77_8899], "bits 31:24 dropped");
        let shown = |s: Settings, m: u8| Shown::of(&s, 0, hdmi_mode::row_of(m).as_ref());
        let d = Settings { ctrl: 0x43, w: 600, h: 400, ow: 640, oh: 480, ..Settings::DEFAULT };
        assert!(shown(d, 6).on && shown(d, 6).smooth_h && shown(d, 6).smooth_v);
        assert!(!shown(Settings { x0: 640, ..d }, 6).on && shown(Settings { x0: 640, ..d }, 7).on, "x0 < hact");
        assert!(!shown(Settings { ow: 2048, ..d }, 6).on && !shown(Settings { oh: 0, ..d }, 6).on);
        assert!(!shown(Settings { ow: 500, ..d }, 6).smooth_h, "a source wider than its output: nearest");
        let wide = Settings { ctrl: 0x45, w: 600, ..d };
        assert!(shown(wide, 6).smooth_h && !shown(wide, 6).smooth_v, "format 2 over 512 pixels: no vertical smoothing");
        assert!(!shown(Settings { ctrl: 0x07, ..d }, 6).on, "format 3");
        // TRX64's host has neither PX_OSIZE nor b6.
        let mut t = Rig::new();
        t.w32(PAGE6 + PX_OSIZE, 4 << 16 | 6);
        t.w32(PAGE6 + PX_CTRL, 0x63);
        assert_eq!((t.trx.reg(PX_OSIZE), t.trx.reg(PX_CTRL)), (1080 << 16 | 1920, 0x23));
    }

    /// S45 §4: the mode request answers in the overlay window, from the hand-over page's row; a reset keeps the host.
    #[test]
    fn s45_install_puts_the_mode_request_into_the_overlay_window() {
        let mut cfg = crate::devices::board::rig::cfg();
        (cfg.trx_io, cfg.icap) = (TrxIo::On, true);
        cfg.handover = Some(crate::coreprof::Handover { video: 0x8000_0101, ..Default::default() });
        let mut map = IoMap::new();
        crate::devices::overlay::install(&mut map, &cfg);
        install(&mut map, &cfg);
        let ov = map.get::<crate::devices::overlay::Overlay>().unwrap();
        assert_eq!((ov.peek8(0x4F04), ov.peek8(0x4F07)), (0x48, 7), "720p 60 from the hand-over word");
        let trx = map.get_mut::<Trx>().unwrap();
        assert_eq!(trx.mode().map(|m| (m.hact, m.vact, m.c64)), Some((1280, 720, false)));
        trx.reset();
        assert_eq!(trx.mode_row().map(|r| r.hact), Some(1280));
        // Not a MultiCore run: no mode request, TRX64's 1080p.
        let mut cfg = crate::devices::board::rig::cfg();
        cfg.trx_io = TrxIo::On;
        let mut map = IoMap::new();
        crate::devices::overlay::install(&mut map, &cfg);
        install(&mut map, &cfg);
        assert_eq!(map.get::<crate::devices::overlay::Overlay>().unwrap().peek8(0x4F04), 0);
        assert_eq!(map.get::<Trx>().unwrap().mode(), None);
    }
}
