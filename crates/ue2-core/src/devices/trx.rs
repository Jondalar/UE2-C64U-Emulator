//! TRX64-Ultimate's IO block 0x101A0000-0x101AFFFF (`--trx-io`): page 0, the ID, the interrupt and from 0x100 the
//! gate; page 5, the freeze session; page 6, the pixel layer, the still capture and the C64 text layer. Spec:
//! docs/specs/S37-trx-pixel-layer.md. The hardware is TRX64-Ultimate's designs/c64_basic/trx_io.v (page 0 below
//! 0x100), trx_sys.v (page 0 from 0x100, pages 1, 2, 3, 5, 7), freeze_resp.v (the session), pix_layer.v and c64_text.v
//! (page 6).
//!
//! Sixteen 4 KB pages, byte wide. Page 0 from 0x104 holds the gate's capture control and page 1 the state bus (S39
//! phase A1, docs/specs/S39-trxmon.md; the model is `trx_sb.rs`). Pages 2, 3, 4 and 7 and every offset not listed
//! read 0 and ignore writes.
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
use crate::devices::c64::C64Port;
use crate::devices::trx_sb::StateBus;
use crate::devices::trx_session::{Chips, Session, VicView};
use crate::host::{TrxLayer, TrxText};
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
/// CAPS b0 page 0 gate, b1 page 1 state bus, b5 page 5 session, b6 page 6 video, b7 the layer is RGB565. The
/// bitstream reads $FF; b2-b4 (engine, debug, cartridge) stay off, and with them every app path that needs pages 2-4.
/// A bit is reported only when its pages are built (S39 §2).
pub const CAPS: u8 = 0xE3;
/// CAPS2 b3: the session has the commit list, so the app sets PROTO around Gideon's protocol (trx_video.cc
/// `has_list`). The bitstream reads $3F; the other bits' features are not here.
pub const CAPS2: u8 = 0x08;
const CAPS_OFF: u32 = 0x004;
const CAPS2_OFF: u32 = 0x005;
pub const IRQ_EN: u32 = 0x010;
pub const IRQ_CAUSE: u32 = 0x014;
/// IRQ_CAUSE b0, the machine halted (top.v `stop_tgl`: the pause or the position hold); b3, a capture or a halt
/// (`cap_tgl`); b6, the frame start; b7, a flip was done. The other causes are pages UE2 does not build, so they stay
/// 0.
pub const CAUSE_STOP: u8 = 0x01;
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
/// The 128 overlay bytes and their valid bits.
pub const SH_OVERLAY: u32 = 0x080;
pub const SH_VALID: u32 = 0x100;

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
/// Output frame size the layer is placed in.
pub const OUT_W: usize = 1920;
pub const OUT_H: usize = 1080;
/// Source width limit: the line buffer holds 960 pixels (pix_layer.v `f_w`).
const MAX_W: u16 = 960;

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
}

impl TrxLink {
    fn power_on(frames: u64, frame_clocks: u64, paused: bool, pause_starts: u64) -> Self {
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
        }
    }
}

pub type Link = Arc<Mutex<TrxLink>>;

/// A new link at power on.
pub fn link() -> Link {
    Arc::new(Mutex::new(TrxLink::power_on(0, 0, false, 0)))
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
}

impl Settings {
    const DEFAULT: Settings = Settings { ctrl: 0x22, w: 960, h: 540, stride: 1920, x0: 0, y0: 0, galpha: 255, key: 0 };
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
    scale: u8,
    alpha: u16,
    key: Option<u16>,
}

impl Shown {
    /// The frame-start copy of `s` (pix_layer.v `en_new`, `s_new`, the `f_*` assignments): on with b0, format 1 and a
    /// size and position inside the frame; width at most 960, height at most 1080; scale 0 counts as 1.
    fn of(s: &Settings, front: u32) -> Shown {
        let on = s.ctrl & 0x01 != 0
            && (s.ctrl >> 1) & 0x07 == 1
            && s.w != 0
            && s.h != 0
            && usize::from(s.x0) < OUT_W
            && usize::from(s.y0) < OUT_H;
        Shown {
            on,
            front,
            w: s.w.min(MAX_W),
            h: s.h.min(OUT_H as u16),
            stride: s.stride,
            x0: s.x0,
            y0: s.y0,
            scale: match (s.ctrl >> 4) & 0x03 {
                0 => 1,
                n => n,
            },
            alpha: u16::from(s.galpha) + u16::from(s.galpha >> 7),
            key: (s.key & 0x1_0000 != 0).then_some(s.key as u16),
        }
    }
}

/// Whether block offset `off` is one of trx_sys.v's pages (trx_io.v `far`): page 0 from 0x100, pages 1, 2, 3, 5, 7.
fn far(off: u32) -> bool {
    match off >> 12 {
        0 => off & 0xF00 != 0,
        1 | 2 | 3 | 5 | 7 => true,
        _ => false,
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
    /// The capture events (`cap_tgl`) seen.
    cap_seen: u64,
    /// c64_text.v: TX_CTRL b1:0, its own stage of byte 0 and its own latch.
    tx_ctrl: u8,
    tx_stage: u8,
    tx_latch: u32,
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
}

impl Trx {
    pub fn new(link: Link) -> Self {
        let (pause_seen, cap_seen) = {
            let l = lock(&link);
            (l.pause_starts, l.sb.cap_events)
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
            tx_ctrl: 0,
            tx_stage: 0,
            tx_latch: 0,
            shown: Shown::of(&Settings::DEFAULT, 0),
            frames: 0,
            starts: 0,
            frame_at: 0,
            now: 0,
            vic_seen: 0,
            pause_seen,
            next_poll: POLL,
            link,
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

    /// Page 6 register `off` (its first byte) as a 32-bit read returns it; c64_text.v's two read 0 here.
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
            // b15:8, the underruns, stay 0: the emulated fetch is never late.
            PX_STATUS => {
                let vblank = self.now < self.frame_at + VBLANK;
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
        if off & !0xFFF == PAGE1 {
            return match off & 0xFFF {
                SB_ADDR => u32::from(self.sb_a),
                SB_DATA => sb.word(self.sb_a),
                SB_STATUS => u32::from(sb.err) << 1 | u32::from(sb.frozen()),
                SB_LOAD => u32::from(sb.ldarm),
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
            r @ SH_OVERLAY..=0x0FF => {
                let i = (r - SH_OVERLAY) as usize;
                u32::from_le_bytes(std::array::from_fn(|b| s.overlay(i + b)))
            }
            r @ SH_VALID..=0x10F => s.valid_word(((r - SH_VALID) / 4) as usize),
            // SH_GCLK, LK_RES and the rest.
            _ => 0,
        }
    }

    /// trx_sys.v's commit of byte 3 (trx_sys.v:310-346). What acts on the machine goes to the port, at this clock.
    fn sys_commit(&mut self, off: u32, v: u32, now: u64) {
        let mut l = lock(&self.link);
        let op = SysWrite { at: now, off, v };
        match off {
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
        let mut pixels = Vec::with_capacity(w * h);
        for m in 0..h as u32 {
            let line = s.front.wrapping_add(m * u32::from(s.stride));
            pixels.extend((0..w as u32).map(|p| {
                let at = (line.wrapping_add(2 * p) & DDR_ADDR_MASK) as usize;
                ram.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
            }));
        }
        Some(TrxLayer {
            x0: usize::from(s.x0),
            y0: usize::from(s.y0),
            w,
            h,
            scale: usize::from(s.scale),
            alpha: s.alpha,
            key: s.key,
            pixels,
        })
    }

    /// The frame start (pix_layer.v, cy 0): a pending flip shows PX_BACK and raises b7, the committed settings take
    /// effect, and b6 is raised.
    fn frame_start(&mut self, now: u64, irq: &mut IrqState) {
        if self.flip {
            self.front = self.back;
            self.flip = false;
            self.cause |= CAUSE_FLIP;
        }
        self.shown = Shown::of(&self.set, self.front);
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
        if vic != 0 && vic != self.vic_seen {
            self.vic_seen = vic;
            self.frame_start(now, irq);
        } else if vic == 0 || paused {
            let period = if vic != 0 && period != 0 { period } else { PAL_FRAME };
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
        let s = &mut self.set;
        match reg {
            STILL_CTRL => lock(&self.link).still.on = v & 1 != 0,
            STILL_BASE => lock(&self.link).still.base = v & !0x0F,
            PX_CTRL => s.ctrl = (v & 0x3F) as u8,
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
        if off & 3 == 0 {
            if far(off) {
                self.far_latch = self.sys_reg(off);
                // trx_sys.v:358-364: an SB_DATA read moves SB_ADDR on, after it latched the word.
                if off == PAGE1 + SB_DATA {
                    self.sb_a = (self.sb_a + 1) & 0xFFF;
                }
                return self.far_latch as u8;
            } else if off & !0xFFF == PAGE6 {
                self.latch = self.reg(reg);
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
        if far(off) {
            match off & 3 {
                // trx_sys.v: a write answers 0, which is what the latch then holds.
                3 => {
                    self.sys_commit(off & !3, u32::from(val) << 24 | self.far_stage, ctx.now);
                    self.far_latch = 0;
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
                CAPS2_OFF => CAPS2,
                IRQ_EN => self.irq_en,
                IRQ_CAUSE => self.cause,
                // BUILD 008-00B: no bitstream, no build stamp.
                _ => 0,
            },
            PAGE6 => {
                let tx = tx_sel(reg);
                match off & 3 {
                    0 => (self.reg(reg) | if tx { self.tx_reg(reg) } else { 0 }) as u8,
                    _ => ((self.latch | if tx { self.tx_latch } else { 0 }) >> byte) as u8,
                }
            }
            _ => 0,
        }
    }

    fn next_event(&self) -> Option<u64> {
        Some(self.next_poll)
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
            let (frames, clocks, paused, starts) = (l.frames, l.frame_clocks, l.paused, l.pause_starts);
            *l = TrxLink::power_on(frames, clocks, paused, starts);
        }
        let link = self.link.clone();
        *self = Trx::new(link);
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
    map.add(BASE, SIZE, Box::new(Trx::new(link)));
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
        assert_eq!((r.r8(4), r.r8(5)), (0xE3, 0x08), "gate, state bus, session, video, RGB565; the commit list");
        assert_eq!(r.r32(0x008), 0, "BUILD");
        r.w8(0x4000, 0xFF);
        assert!((0x4000..0x4010).chain(0xF000..0xF004).chain(0x2000..0x2004).chain(0x104..0x140).all(|o| r.r8(o) == 0));
        assert!((0x140..0x1000).step_by(0x3C).all(|o| r.r8(o) == 0), "140-FFF");
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

    #[test]
    fn page_6_defaults_follow_pix_layer() {
        let mut r = Rig::new();
        let regs = [STILL_CTRL, STILL_BASE, STILL_STATUS, PX_CTRL, PX_FRONT, PX_SIZE, PX_STRIDE, PX_POS, PX_GALPHA, PX_KEY];
        let want = [1, 0x0505_0000, 0, 0x22, 0, 540 << 16 | 960, 1920, 0, 255, 0];
        for (reg, want) in regs.into_iter().zip(want) {
            assert_eq!(r.r32(PAGE6 + reg), want, "{reg:#05x}");
        }
        assert!(!r.trx.shown(), "0x22 is format 1 but not on");
    }

    #[test]
    fn bytes_0_to_2_stage_and_byte_3_commits() {
        let mut r = Rig::new();
        for (i, b) in [0x34, 0x12, 0x78].into_iter().enumerate() {
            r.w8(PAGE6 + PX_POS + i as u32, b);
        }
        assert_eq!(r.r32(PAGE6 + PX_POS), 0, "staged, not committed");
        r.w8(PAGE6 + PX_POS + 3, 0x05);
        assert_eq!(r.r32(PAGE6 + PX_POS), 0x0578_1234);
        // One stage for all registers: byte 3 commits the register it is written to.
        r.w8(PAGE6 + PX_SIZE, 0xAA);
        r.w8(PAGE6 + PX_GALPHA + 3, 0);
        assert_eq!(r.r32(PAGE6 + PX_GALPHA), 0xAA);
        // Masks on commit: addresses and the stride to 16 bytes, PX_CTRL 6 bits, the key 17.
        r.w32(PAGE6 + PX_FRONT, 0x0123_456F);
        r.w32(PAGE6 + PX_STRIDE, 0xFFFF_FFFF);
        r.w32(PAGE6 + PX_CTRL, 0xFF);
        r.w32(PAGE6 + PX_KEY, 0xFFFF_FFFF);
        let got = [PX_FRONT, PX_STRIDE, PX_CTRL, PX_KEY].map(|reg| r.r32(PAGE6 + reg));
        assert_eq!(got, [0x0123_4560, 0xFFF0, 0x3F, 0x1_FFFF]);
        r.w32(PAGE6 + STILL_BASE, 0x0012_345F);
        r.w32(PAGE6 + STILL_CTRL, 0);
        let still = lock(&r.link).still;
        assert_eq!((still.base, still.on), (0x0012_3450, false), "the port captures with them");
    }

    #[test]
    fn byte_0_read_latches_bytes_1_to_3() {
        let mut r = Rig::new();
        r.w32(PAGE6 + PX_SIZE, 0x0111_0222);
        assert_eq!(r.r8(PAGE6 + PX_SIZE), 0x22);
        r.w32(PAGE6 + PX_SIZE, 0x0333_0444);
        let late = [1, 2, 3].map(|b| r.r8(PAGE6 + PX_SIZE + b));
        assert_eq!(late, [0x02, 0x11, 0x01], "bytes 1-3 from the latch of the first read");
        assert_eq!(r.r32(PAGE6 + PX_SIZE), 0x0333_0444, "a new byte-0 read takes the new value");
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
        assert_eq!((r.r32(PAGE6 + PX_FRONT), r.r8(IRQ_CAUSE)), (0x10_0000, 0), "nothing before the frame start");
        r.now += POLL;
        r.with(|t, ctx| t.tick(ctx));
        assert_eq!(r.r8(IRQ_CAUSE), 0, "no VIC frame, no frame start");
        r.frame();
        assert_eq!(r.r32(PAGE6 + PX_FRONT), 0x20_0000);
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
            TrxLayer { x0: 7, y0: 5, w: 3, h: 2, scale: 1, alpha: 129, key: Some(0xF81F), pixels: vec![1, 2, 3, 11, 12, 13] }
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
            Shown::of(&Settings { ctrl, w, h, x0, y0, ..Settings::DEFAULT }, 0).on
        };
        assert!(on(0x23, 960, 540, 1919, 1079));
        assert!(!on(0x22, 960, 540, 0, 0), "b0");
        assert!(!on(0x25, 960, 540, 0, 0), "format 2");
        assert!(!on(0x23, 0, 540, 0, 0) && !on(0x23, 960, 0, 0, 0), "empty");
        assert!(!on(0x23, 960, 540, 1920, 0) && !on(0x23, 960, 540, 0, 1080), "outside");
        let s = Shown::of(&Settings { ctrl: 0x03, w: 2000, h: 2000, galpha: 0, ..Settings::DEFAULT }, 0);
        assert_eq!((s.w, s.h, s.scale, s.alpha), (960, 1080, 1, 0), "clamped, scale 0 is 1");
        assert_eq!(Shown::of(&Settings { ctrl: 0x33, ..Settings::DEFAULT }, 0).scale, 3);
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
}
