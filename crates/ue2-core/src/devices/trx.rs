//! TRX64-Ultimate's IO block 0x101A0000-0x101AFFFF (`--trx-io`): page 0, the ID and the interrupt, and page 6, the
//! pixel layer. Spec: docs/specs/S37-trx-pixel-layer.md. The hardware is TRX64-Ultimate's designs/c64_basic/trx_io.v
//! (page 0) and pix_layer.v (page 6).
//!
//! Sixteen 4 KB pages, byte wide. Every other page and every offset not listed reads 0 and ignores writes. Page 6
//! still capture (`STILL_*`) keeps its registers but captures nothing yet (S37 phase B), so `STILL_STATUS` reads 0.
//!
//! The layer's settings and a pending flip take effect at the frame start, one per VIC frame. The device sees a VIC
//! frame through the count [`C64Port`] publishes after each sync, polled every millisecond, so the frame start comes
//! up to 1 ms after the VIC finished the frame. Without a C64 that counts frames it comes every PAL frame period.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::devices::c64::C64Port;
use crate::host::TrxLayer;
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
/// CAPS b6 page 6 video, b7 the layer is RGB565. The bitstream reads $FF; UE2 has only these two, so the app leaves
/// the state bus, the engine, the freeze session and the cartridge alone.
pub const CAPS: u8 = 0xC0;
/// The bitstream reads $3F; UE2 has none of its features.
pub const CAPS2: u8 = 0x00;
const CAPS_OFF: u32 = 0x004;
const CAPS2_OFF: u32 = 0x005;
pub const IRQ_EN: u32 = 0x010;
pub const IRQ_CAUSE: u32 = 0x014;
/// IRQ_CAUSE b6, the frame start; b7, a flip was done. The other causes are pages UE2 does not build, so they stay 0.
pub const CAUSE_VSYNC: u8 = 0x40;
pub const CAUSE_FLIP: u8 = 0x80;

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

/// How often the device looks at the VIC frame count: the C64 port syncs at least this often (c64.rs SYNC_PERIOD).
const POLL: u64 = CLOCKS_PER_MS;
/// The frame period without a C64 that counts frames: PAL, 312 lines of 63 cycles at 985,248 Hz.
const PAL_FRAME: u64 = 312 * 63 * CLOCK_HZ / 985_248;
/// PX_STATUS b1 holds for the first 45 output lines of 1125 (`px_vblank`, cy < 45): 45 × 2688 clocks of the
/// 151.58 MHz pixel clock (vic_scaler.v), 0.8 ms.
const VBLANK: u64 = 45 * 2688 * CLOCK_HZ / 151_580_000;
/// The DDR byte address the layer's fetch takes: bits 26:4 (pix_layer.v `rd_a`), 128 MB.
const DDR_ADDR_MASK: u32 = 0x07FF_FFFF;
/// Output frame size the layer is placed in.
pub const OUT_W: usize = 1920;
pub const OUT_H: usize = 1080;
/// Source width limit: the line buffer holds 960 pixels (pix_layer.v `f_w`).
const MAX_W: u16 = 960;

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

pub struct Trx {
    irq_en: u8,
    cause: u8,
    still_ctrl: bool,
    still_base: u32,
    set: Settings,
    /// PX_FRONT and PX_BACK; `flip` while a PX_BACK write waits for the frame start.
    front: u32,
    back: u32,
    flip: bool,
    /// Bytes 0-2 of the next commit, one stage for all registers (pix_layer.v `stg`).
    stage: u32,
    /// Bytes 1-3 of the last byte-0 read (pix_layer.v `lat`).
    latch: u32,
    shown: Shown,
    /// Frame starts, wrapping (PX_STATUS b31:16).
    frames: u16,
    /// Clock of the last frame start, and the latest clock the device saw.
    frame_at: u64,
    now: u64,
    /// The VIC frame count from [`C64Port`], and the value of the last frame start; 0 while no C64 counts.
    vic: Arc<AtomicU64>,
    vic_seen: u64,
    next_poll: u64,
}

impl Trx {
    pub fn new(vic: Arc<AtomicU64>) -> Self {
        Trx {
            irq_en: 0,
            cause: 0,
            still_ctrl: true,
            still_base: 0x0505_0000,
            set: Settings::DEFAULT,
            front: 0,
            back: 0,
            flip: false,
            stage: 0,
            latch: 0,
            shown: Shown::of(&Settings::DEFAULT, 0),
            frames: 0,
            frame_at: 0,
            now: 0,
            vic,
            vic_seen: 0,
            next_poll: POLL,
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

    /// Page 6 register `off` (its first byte) as a 32-bit read returns it.
    pub fn reg(&self, off: u32) -> u32 {
        let s = &self.set;
        match off {
            STILL_CTRL => u32::from(self.still_ctrl),
            STILL_BASE => self.still_base,
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
            // STILL_STATUS: no capture before phase B.
            _ => 0,
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
        self.frame_at = now;
        self.update_irq(irq);
    }

    /// A frame start for each new VIC frame count, or every PAL frame while no C64 counts frames.
    fn poll(&mut self, now: u64, irq: &mut IrqState) {
        self.now = now;
        let vic = self.vic.load(Ordering::Relaxed);
        if vic != 0 {
            if vic != self.vic_seen {
                self.vic_seen = vic;
                self.frame_start(now, irq);
            }
        } else if now >= self.frame_at + PAL_FRAME {
            self.frame_start(now, irq);
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
            STILL_CTRL => self.still_ctrl = v & 1 != 0,
            STILL_BASE => self.still_base = v & !0x0F,
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
        if off & !0xFFF == PAGE6 && off & 3 == 0 {
            self.latch = self.reg(off & 0xFFF);
        }
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        self.poll(ctx.now, ctx.irq);
        let reg = off & 0xFFF;
        match off & !0xFFF {
            0 => {
                match reg {
                    IRQ_EN => self.irq_en = val,
                    IRQ_CAUSE => self.cause &= !val,
                    _ => {}
                }
                self.update_irq(ctx.irq);
            }
            PAGE6 => match reg & 3 {
                3 => self.commit(reg & !3, u32::from(val) << 24 | self.stage),
                b => {
                    let shift = 8 * b;
                    self.stage = self.stage & !(0xFF << shift) | u32::from(val) << shift;
                }
            },
            _ => {}
        }
    }

    /// A read without its side effect: page 6 bytes 1-3 come from the latch of the last byte-0 read.
    fn peek8(&self, off: u32) -> u8 {
        let reg = off & 0xFFF;
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
            PAGE6 => match reg & 3 {
                0 => self.reg(reg) as u8,
                b => (self.latch >> (8 * b)) as u8,
            },
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

    /// The registers' power-on values; the VIC frame count stays connected.
    fn reset(&mut self) {
        let vic = self.vic.clone();
        *self = Trx::new(vic);
    }

    crate::impl_as_any!();
}

/// With `--trx-io` on only (`Machine::new` has resolved auto): the block, with high IRQ 2 and the VIC frame count taken from the C64 port.
pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    if cfg.trx_io != TrxIo::On {
        return;
    }
    let vic = Arc::new(AtomicU64::new(0));
    if let Some(port) = map.get_mut::<C64Port>() {
        port.set_trx_io(vic.clone());
    }
    map.add(BASE, SIZE, Box::new(Trx::new(vic)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block on its own, with DDR and an ITU, driven at clock `now`.
    struct Rig {
        trx: Trx,
        vic: Arc<AtomicU64>,
        ram: Vec<u8>,
        irq: IrqState,
        now: u64,
    }

    impl Rig {
        fn new() -> Self {
            let vic = Arc::new(AtomicU64::new(0));
            Rig { trx: Trx::new(vic.clone()), vic, ram: vec![0; 1 << 20], irq: IrqState::new(), now: 0 }
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
            self.vic.fetch_add(1, Ordering::Relaxed);
            self.now += POLL;
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
        assert_eq!((r.r8(4), r.r8(5)), (0xC0, 0x00));
        assert_eq!(r.r32(0x008), 0, "BUILD");
        r.w8(0x1000, 0xFF);
        assert!((0x1000..0x1010).chain(0xF000..0xF004).chain(0x100..0x104).all(|o| r.r8(o) == 0));
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
        r.now = PAL_FRAME - 1;
        r.with(|t, ctx| t.tick(ctx));
        assert_eq!(r.trx.frames, 0);
        r.now = PAL_FRAME;
        r.with(|t, ctx| t.tick(ctx));
        assert_eq!(r.trx.frames, 1);
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
