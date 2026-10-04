//! Overlay renderer and text dump. Spec: docs/specs/S07-overlay-u64io-render.md
//!
//! Pixel semantics follow the open chargen IP as summarised in docs/hw/05-ui-overlay-input.md §B
//! (char_generator_regs.vhd, char_generator_slave12.vhd).
//!
//! The image is the output mode the firmware programmed into the HDMI timing registers: the C64 picture cropped,
//! scaled and placed where the cropper and the two scalers put it, and the text grid on top where the chargen
//! registers put it — X_ON/Y_ON for the corner,
//! CHARS_PER_LINE and ACTIVE_LINES for the size (05 OQ 4, answered). Nothing here is centred and no size is
//! assumed: `DetermineOverlaySettings` (u64_config.cc) writes a different box per screen and per video mode, so a
//! full-height panel flush to the right edge and a small floating submenu window are the same code path with
//! different registers.

use anyhow::{bail, Result};

use crate::c64host::{C64Charset, C64Frame};
use crate::devices::overlay::{
    REG_ACTIVE_LINES, REG_CHARS_PER_LINE, REG_CHAR_HEIGHT, REG_CHAR_WIDTH, REG_POINTER_HI, REG_POINTER_LO,
    REG_TRANSPARENCY, REG_X_ON_HI, REG_X_ON_LO, REG_Y_ON_HI, REG_Y_ON_LO, TEXT_RAM_SIZE,
};
use crate::host::{DisplaySnapshot, TrxLayer};

/// Colour of transparent pixels and of the whole image while the overlay is hidden, where no C64 frame shows
/// through.
pub const BACKDROP: u32 = 0x0010_1010;

/// Half the overlay colour over half of what is under it, per channel: the whole window is drawn at 50 %, its
/// background included. The cells' background is colour 0, which is black, so the panel is black at half strength
/// over the C64 picture and the picture stays visible through it. The open chargen IP would leave a pixel of the
/// transparent index fully see-through (`pixel_opaque = '0'`, char_generator_slave12.vhd:161-172), which is a text
/// layer with no panel behind it; the mixer that makes it a panel is in the closed U64 part of the FPGA.
fn blend_half(over: u32, under: u32) -> u32 {
    let mix = |shift: u32| (((over >> shift) & 0xFF) + ((under >> shift) & 0xFF)) / 2;
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// An overlay layer pixel the text grid does not cover. Pixels are 0x00RRGGBB, so this is never a colour.
const CLEAR: u32 = u32::MAX;

/// Image size while the chargen registers are unprogrammed: 40×25 cells of 8×9, the default SD geometry
/// (u64_config.cc:2984-2986).
const DEFAULT_SIZE: (usize, usize) = (320, 225);
/// Both fonts: 128 glyphs of 8 rows, addressed by the 7-bit screen code (slave12.vhd:188-191).
const FONT_ROWS: usize = 128 * 8;

/// The video mode the firmware programmed into the HDMI timing registers (`t_video_timing_regs`, u64.h:172-203;
/// `SetScanModeRegisters`, hdmi_scan.cc:6-27, which stores the low bits of the horizontal values in
/// VID_HREPETITION). This is the picture the device puts out: the C64 frame scaled into it, with the overlay
/// window on top where X_ON/Y_ON put it. The chargen's counters restart on the sync pulse
/// (char_generator_timing.vhd:108-113), so the active area begins `sync + back porch` in.
struct Output {
    width: usize,
    height: usize,
    /// Counter value where the active area starts.
    x0: usize,
    y0: usize,
}

/// The horizontal scaler's ratio per `hscaler` code, and the vertical one per `vscaler` code, as numerator and
/// denominator (hdmi_scan.cc:118-155, the tables in the comment above `SetVideoMode1080p`).
///
/// The firmware's table lists output widths for a 384-pixel and a 400-pixel crop, and heights for 240 and 270 —
/// every pair is one ratio, so a crop of any other size scales by the same factor. The four entries whose 384/240
/// column is rounded (533, 686, 1067, 1371) are taken from the 400/270 column, where the fraction comes out
/// exact: 20/9, 20/7, 40/9, 40/7.
const HSCALE: [(u8, u32, u32); 10] = [
    (0x00, 1, 1),
    (0x01, 5, 4),
    (0x02, 4, 3),
    (0x03, 5, 3),
    (0x04, 15, 8),
    (0x08, 2, 1),
    (0x09, 5, 2),
    (0x0A, 8, 3),
    (0x0B, 10, 3),
    (0x0C, 15, 4),
];
const VSCALE: [(u8, u32, u32); 18] = [
    (0x00, 2, 1),
    (0x10, 20, 9),
    (0x11, 9, 4),
    (0x12, 5, 2),
    (0x13, 8, 3),
    (0x14, 20, 7),
    (0x15, 3, 1),
    (0x16, 16, 5),
    (0x17, 18, 5),
    (0x08, 4, 1),
    (0x18, 40, 9),
    (0x19, 9, 2),
    (0x1A, 5, 1),
    (0x1B, 16, 3),
    (0x1C, 40, 7),
    (0x1D, 6, 1),
    (0x1E, 32, 5),
    (0x1F, 36, 5),
];

/// Where the C64 picture lands in the output, and which part of the VIC frame goes there.
///
/// The device does not stretch the picture over the whole screen: `SetVicCrop` takes a window out of the VIC
/// frame, the two scalers blow it up by a fixed ratio each, and `x_offset` places the result
/// (hdmi_scan.cc:111-163). At 1080p PAL that is a 384x270 crop at (8, 9), scaled 15/4 by 4/1 to 1440x1080 and put
/// at x 240 — pillarboxed in the 1920-wide active area, not stretched across it.
#[derive(Debug, PartialEq, Eq)]
struct Picture {
    /// Source rectangle in the FPGA's video stream. Only its size is used; see `draw_c64`.
    crop: (usize, usize, usize, usize),
    /// Destination in the active area: left edge, then the size after both scalers.
    x: usize,
    width: usize,
    height: usize,
}

impl Picture {
    /// `None` when the scaler codes are ones this firmware never writes, which is also the unprogrammed state.
    fn new(hdmi: &[u8; 0x1E], cropper: &[u8; 4]) -> Option<Picture> {
        let ratio = |table: &[(u8, u32, u32)], code: u8| {
            table.iter().find(|(c, _, _)| *c == code).map(|&(_, n, d)| (n, d))
        };
        let (hn, hd) = ratio(&HSCALE, hdmi[19])?;
        let (vn, vd) = ratio(&VSCALE, hdmi[20])?;
        // `SetVicCrop` stores the size halved (hdmi_scan.cc:58-59).
        let (cw, ch) = (usize::from(cropper[2]) * 2, usize::from(cropper[3]) * 2);
        if cw == 0 || ch == 0 {
            return None;
        }
        Some(Picture {
            crop: (usize::from(cropper[0]), usize::from(cropper[1]), cw, ch),
            x: usize::from(hdmi[16]),
            width: cw * hn as usize / hd as usize,
            height: ch * vn as usize / vd as usize,
        })
    }
}

impl Output {
    fn new(hdmi: &[u8; 0x1E]) -> Option<Output> {
        let (v, rep) = (|i: usize| hdmi[i] as usize, hdmi[5] as usize);
        let width = (v(3) << 3) | ((rep >> 1) & 6);
        let height = v(11) << 3;
        let x0 = ((v(1) << 1) | ((rep >> 1) & 1)) + ((v(2) << 2) | ((rep >> 4) & 3));
        let y0 = v(9) + v(10);
        (width != 0 && height != 0).then_some(Output { width, height, x0, y0 })
    }
}

/// Chargen register latches decoded as in 05 §B (char_generator_regs.vhd:43-75).
struct Geometry {
    cols: usize,
    rows: usize,
    char_width: usize,
    char_height: usize,
    big: bool,
    stretch: bool,
    pointer: usize,
    /// X_ON/Y_ON: where the window starts, in the chargen's own counters (chargen.h:19-22).
    x_on: usize,
    y_on: usize,
    transparent: u8,
    visible: bool,
}

impl Geometry {
    fn new(regs: &[u8; 16]) -> Self {
        let height = regs[REG_CHAR_HEIGHT];
        let transparency = regs[REG_TRANSPARENCY];
        Geometry {
            cols: regs[REG_CHARS_PER_LINE] as usize,
            rows: (regs[REG_ACTIVE_LINES] & 0x3F) as usize,
            char_width: (regs[REG_CHAR_WIDTH] & 0x0F) as usize,
            char_height: (height & 0x1F) as usize,
            big: height & 0x40 != 0,
            stretch: height & 0x80 != 0,
            pointer: (((regs[REG_POINTER_HI] & 0x7F) as usize) << 8) | regs[REG_POINTER_LO] as usize,
            x_on: ((regs[REG_X_ON_HI] as usize) << 8) | regs[REG_X_ON_LO] as usize,
            y_on: ((regs[REG_Y_ON_HI] as usize) << 8) | regs[REG_Y_ON_LO] as usize,
            transparent: transparency & 0x0F,
            visible: transparency & 0x80 != 0,
        }
    }

    /// Cell scanlines `y` that are drawn; the big font skips `y & 3 == 3` (slave12.vhd:114-119).
    fn lines(&self) -> Vec<usize> {
        (0..self.char_height).filter(|y| !self.big || y & 3 != 3).collect()
    }

    /// Screen/colour RAM index of cell (row, col): the 12-bit `pointer + char_x` (slave12.vhd:111, :186).
    fn cell(&self, row: usize, col: usize) -> usize {
        (self.pointer + row * self.cols + col) % TEXT_RAM_SIZE
    }
}

pub struct Renderer {
    /// 8×8 font: glyph g row y = byte `g * 8 + y`, bit 7 leftmost (`roms/chars.bin`, 05 §B Fonts).
    pub font: Vec<u8>,
    /// 12×24 font: glyph g row y = word `g * 8 + y`, sub-row s = bits `12s+11..12s`, bit 11 leftmost.
    /// `new` fills it with the 8×8 font widened to 12 px and tripled per row; assign
    /// `parse_font_pkg` output for the real `c_font`.
    pub big_font: Vec<u64>,
    /// The overlay drawn at canvas size, [`CLEAR`] where it shows nothing; kept between frames for its allocation.
    layer: Vec<u32>,
    /// S37: the TRX composite at its own size, when the image is made at another; kept for its allocation.
    composite: Vec<u32>,
}

impl Renderer {
    /// `font`: bytes of `roms/chars.bin` (layout per docs/hw/05-ui-overlay-input.md).
    pub fn new(font: &[u8]) -> Self {
        let big_font = (0..FONT_ROWS)
            .map(|i| {
                let byte = font.get(i).copied().unwrap_or(0);
                let wide = (0..12).fold(0u64, |w, x| (w << 1) | ((byte >> (7 - x * 8 / 12)) & 1) as u64);
                wide | (wide << 12) | (wide << 24)
            })
            .collect();
        Renderer { font: font.to_vec(), big_font, layer: Vec::new(), composite: Vec::new() }
    }

    /// Render the overlay into 0x00RRGGBB pixels; returns (width, height).
    ///
    /// Size = CHARS_PER_LINE × CHAR_WIDTH by ACTIVE_LINES × drawn scanlines per cell. Unprogrammed
    /// registers (any of those 0) give `DEFAULT_SIZE`. Hidden (TRANSPARENCY bit 7 = 0) and transparent
    /// pixels are `BACKDROP`.
    ///
    /// Once the firmware has programmed the HDMI timing ([`Output`]) the image is that output: the C64 frame
    /// (`snap.c64`, docs/specs/S14-c64-trx64.md §9) scaled into the active area, and the overlay window 1:1 where
    /// X_ON/Y_ON put it — a 40×25 window right of centre, not a full screen (u64_config.cc:2979-3002). Hidden and
    /// transparent overlay pixels show the C64 through, as the chargen's `pixel_opaque` does (05 §B, OQ 3).
    ///
    /// Before that, and without a C64, the canvas covers frame and grid, both centred, as it always did.
    ///
    /// While the TRX pixel layer is on (`snap.trx`, S37) the image is the TRX output instead: 1920 × 1080 or half
    /// that ([`trx_canvas`]).
    pub fn render(&mut self, snap: &DisplaySnapshot, out: &mut Vec<u32>) -> (usize, usize) {
        let canvas = Self::canvas(snap);
        self.render_scaled(snap, out, canvas);
        canvas
    }

    /// The size of the image [`Renderer::render`] makes for `snap`: the TRX composite while its pixel layer is on
    /// (S37), else the output mode once the firmware has programmed one, else frame and grid covered.
    pub fn canvas(snap: &DisplaySnapshot) -> (usize, usize) {
        match &snap.trx {
            Some(layer) => trx_canvas(snap, layer),
            None => Self::output_canvas(snap),
        }
    }

    /// [`Renderer::canvas`] without the TRX layer: the canvas the overlay window is placed on.
    fn output_canvas(snap: &DisplaySnapshot) -> (usize, usize) {
        let geo = Geometry::new(&snap.regs);
        let (w, h) = (geo.cols * geo.char_width, geo.rows * geo.lines().len());
        let (gw, gh) = if w == 0 || h == 0 { DEFAULT_SIZE } else { (w, h) };
        match Output::new(&snap.hdmi) {
            Some(o) => (o.width, o.height),
            None => snap.c64.as_ref().map_or((gw, gh), |f| (f.width.max(gw), f.height.max(gh))),
        }
    }

    /// [`Renderer::render`]'s image resampled to `size` in one step: each target pixel goes straight to the C64 pixel
    /// and the overlay pixel under it, so every C64 pixel is rounded once, as trx64-cli does. Scaling `render`'s
    /// image instead rounds twice, through the output mode's own non-integer ratios, and the C64 font comes out
    /// uneven. At `size` = [`Renderer::canvas`] it is `render`.
    pub fn render_scaled(&mut self, snap: &DisplaySnapshot, out: &mut Vec<u32>, size: (usize, usize)) {
        if let Some(layer) = &snap.trx {
            self.render_trx(snap, layer, out, size);
            return;
        }
        let output = Output::new(&snap.hdmi);
        let picture = Picture::new(&snap.hdmi, &snap.cropper);
        let canvas = Self::output_canvas(snap);
        let (tw, th) = size;
        out.clear();
        out.resize(tw * th, BACKDROP);
        if tw == 0 || th == 0 {
            return;
        }
        if let Some(frame) = &snap.c64 {
            draw_c64(frame, output.is_some().then_some(picture.as_ref()), out, canvas, size);
        }
        self.blend_overlay(snap, out, size);
    }

    /// S37: the TRX output picture (docs/specs/S37-trx-pixel-layer.md §1), made at [`trx_canvas`] and resampled to
    /// `size`: the C64 as vic_scaler.v places it, the overlay window over it as [`Renderer::render_scaled`] puts it,
    /// and the pixel layer on top.
    fn render_trx(&mut self, snap: &DisplaySnapshot, layer: &TrxLayer, out: &mut Vec<u32>, size: (usize, usize)) {
        let canvas = trx_canvas(snap, layer);
        let (cw, ch) = canvas;
        let (tw, th) = size;
        let direct = canvas == size;
        let mut comp = if direct { std::mem::take(out) } else { std::mem::take(&mut self.composite) };
        comp.clear();
        comp.resize(cw * ch, 0);
        if let Some(frame) = &snap.c64 {
            draw_c64_trx(frame, &mut comp, canvas);
        }
        self.blend_overlay(snap, &mut comp, canvas);
        draw_layer(layer, &mut comp, canvas);
        if direct {
            *out = comp;
            return;
        }
        out.clear();
        out.resize(tw * th, BACKDROP);
        if tw != 0 && th != 0 {
            let xs: Vec<usize> = (0..tw).map(|x| x * cw / tw).collect();
            for y in 0..th {
                let row = &comp[(y * ch / th) * cw..][..cw];
                for (px, &x) in out[y * tw..][..tw].iter_mut().zip(&xs) {
                    *px = row[x];
                }
            }
        }
        self.composite = comp;
    }

    /// The overlay window at half strength over `out`, a `size` image of the output canvas.
    fn blend_overlay(&mut self, snap: &DisplaySnapshot, out: &mut [u32], size: (usize, usize)) {
        let geo = Geometry::new(&snap.regs);
        let lines = geo.lines();
        let (w, h) = (geo.cols * geo.char_width, geo.rows * lines.len());
        let output = Output::new(&snap.hdmi);
        let (cw, ch) = Self::output_canvas(snap);
        let (tw, th) = size;
        if w != 0 && h != 0 && geo.visible {
            let origin = match &output {
                Some(o) => (geo.x_on.saturating_sub(o.x0), geo.y_on.saturating_sub(o.y0)),
                None => ((cw - w) / 2, (ch - h) / 2),
            };
            let mut layer = std::mem::take(&mut self.layer);
            layer.clear();
            layer.resize(cw * ch, CLEAR);
            self.draw_text(snap, &geo, &lines, &mut layer, (cw, ch), origin);
            let xs: Vec<usize> = (0..tw).map(|x| x * cw / tw).collect();
            for y in 0..th {
                let row = &layer[(y * ch / th) * cw..][..cw];
                for (px, &ox) in out[y * tw..][..tw].iter_mut().zip(&xs) {
                    if row[ox] != CLEAR {
                        *px = blend_half(row[ox], *px);
                    }
                }
            }
            self.layer = layer;
        }
    }

    /// Draw the opaque pixels of the text grid into the overlay layer with its top-left corner at `origin`, clipped
    /// to the canvas: a window placed near the right edge can reach past it (u64_config.cc:2993, 1080p X_ON 1438 +
    /// 480). The layer is blended over the C64 afterwards, at whatever size the image is made.
    fn draw_text(
        &self,
        snap: &DisplaySnapshot,
        geo: &Geometry,
        lines: &[usize],
        out: &mut [u32],
        canvas: (usize, usize),
        origin: (usize, usize),
    ) {
        let (canvas_w, canvas_h) = canvas;
        let palette: [u32; 16] = std::array::from_fn(|i| {
            let c = |k: usize| snap.palette.get(i * 4 + k).copied().unwrap_or(0) as u32;
            (c(0) << 16) | (c(1) << 8) | c(2)
        });
        for row in 0..geo.rows {
            for col in 0..geo.cols {
                let i = geo.cell(row, col);
                let code = snap.screen.get(i).copied().unwrap_or(0);
                let attr = snap.color.get(i).copied().unwrap_or(0);
                let reverse = code & 0x80 != 0;
                for (ly, &y) in lines.iter().enumerate() {
                    let bits = self.glyph_row(geo, (code & 0x7F) as usize, y);
                    let (px_y, px_x) = (origin.1 + row * lines.len() + ly, origin.0 + col * geo.char_width);
                    if px_y >= canvas_h || px_x >= canvas_w {
                        continue;
                    }
                    let width = geo.char_width.min(canvas_w - px_x);
                    let start = px_y * canvas_w + px_x;
                    for (x, px) in out[start..start + width].iter_mut().enumerate() {
                        // slave12.vhd:159-173: foreground where the glyph bit differs from reverse.
                        let fg = ((bits >> (geo.char_width - 1 - x)) & 1 != 0) != reverse;
                        let idx = if fg { attr & 0x0F } else { attr >> 4 };
                        *px = palette[idx as usize];
                    }
                }
            }
        }
    }

    /// Glyph `g` on cell scanline `y` as pixel bits, bit `char_width - 1` leftmost (05 §B).
    fn glyph_row(&self, geo: &Geometry, g: usize, y: usize) -> u32 {
        let rom8 = |row: usize| self.font.get(g * 8 + row).copied().unwrap_or(0) as u32;
        if geo.big {
            // slave12.vhd:141-148, :191
            let word = self.big_font.get(g * 8 + (y >> 2)).copied().unwrap_or(0);
            ((word >> (12 * (y & 3))) & 0xFFF) as u32
        } else if geo.stretch {
            rom8((y >> 1) & 7) // slave12.vhd:188
        } else if (y & 8) == 0 {
            rom8(y & 7) // slave12.vhd:189
        } else {
            // slave12.vhd:151-153, :190: lines 8+ repeat row 7 only to continue a vertical bar.
            match rom8(7) {
                0x18 => 0x18,
                _ => 0,
            }
        }
    }
}

/// Palettize the C64 frame into a `size` image of the `canvas`, rounding each target pixel to a C64 pixel once.
///
/// With a `Picture`, the cropped VIC picture goes into the rectangle the cropper and the two scalers put it in
/// (nearest neighbour; the hardware filters). Everything outside it keeps the backdrop, which is what the device
/// sends where the picture is not. Only the crop's size is taken over: its origin counts in the FPGA's video stream,
/// which starts elsewhere than the TRX64 canvas; laid on the canvas as is, 480p put 35 border lines above the text
/// and 5 below it (issue #3). The device shows a centred C64, so the crop is centred on the canvas, which has
/// symmetric borders.
///
/// With an output mode but no `Picture` — the scaler registers unprogrammed, which is every machine before the
/// firmware sets a video mode — the frame goes over the whole canvas. Without an output mode (`picture` None) the
/// frame sits 1:1 in the middle of the canvas.
fn draw_c64(
    frame: &C64Frame,
    picture: Option<Option<&Picture>>,
    out: &mut [u32],
    canvas: (usize, usize),
    size: (usize, usize),
) {
    let ((cw, ch), (tw, th)) = (canvas, size);
    if frame.width == 0 || frame.height == 0 {
        return;
    }
    // Per axis: the part of the canvas the picture covers (offset, length) and the source span it shows.
    let (x_axis, y_axis) = match picture {
        Some(Some(p)) => ((p.x, p.width, p.crop.2), (0, p.height, p.crop.3)),
        Some(None) => ((0, cw, frame.width), (0, ch, frame.height)),
        None => (
            ((cw - frame.width) / 2, frame.width, frame.width),
            ((ch - frame.height) / 2, frame.height, frame.height),
        ),
    };
    let xs = axis(tw, cw, x_axis, frame.width);
    let ys = axis(th, ch, (y_axis.0, y_axis.1.min(ch), y_axis.2), frame.height);
    for (y, row) in ys.iter().enumerate() {
        let Some(row) = row else { continue };
        let src = &frame.indices[row * frame.width..][..frame.width];
        for (px, col) in out[y * tw..][..tw].iter_mut().zip(&xs) {
            if let Some(col) = col {
                *px = frame.palette[usize::from(src[*col] & 0x0F)];
            }
        }
    }
}

/// For each of `target` pixels along one axis of a `canvas`-long image, the frame pixel it shows, or None: the
/// picture covers `len` canvas pixels from `off` and shows `span` frame pixels centred in the frame's `have`
/// (negative when the span is larger, and those stay backdrop). One division per pixel, from the target straight
/// to the frame.
fn axis(target: usize, canvas: usize, (off, len, span): (usize, usize, usize), have: usize) -> Vec<Option<usize>> {
    let start = (have as isize - span as isize) / 2;
    (0..target)
        .map(|t| {
            // The target pixel's position on the canvas is t * canvas / target, kept as a fraction.
            let at = t * canvas;
            if len == 0 || span == 0 || at < off * target || at >= (off + len) * target {
                return None;
            }
            let col = start + ((at - off * target) * span / (target * len)) as isize;
            (0..have as isize).contains(&col).then_some(col as usize)
        })
        .collect()
}

/// S37: the size the TRX composite is made at. 960 × 540 when every layer pixel is a 2 × 2 block on even output
/// coordinates (S = 2, x0 and y0 even, LVGL's case) and no overlay window is up, whose text is drawn 1:1 in the output
/// mode; 1920 × 1080 otherwise.
fn trx_canvas(snap: &DisplaySnapshot, layer: &TrxLayer) -> (usize, usize) {
    let geo = Geometry::new(&snap.regs);
    let overlay = geo.visible && geo.cols * geo.char_width != 0 && geo.rows != 0;
    if layer.scale == 2 && layer.x0 % 2 == 0 && layer.y0 % 2 == 0 && !overlay {
        (TRX_W / 2, TRX_H / 2)
    } else {
        (TRX_W, TRX_H)
    }
}

/// The TRX output frame (vic_scaler.v).
const TRX_W: usize = 1920;
const TRX_H: usize = 1080;
/// vic_scaler.v: the C64 window's first raster column and line (50 Hz `X0`, `Y0`; 60 Hz `Y0_60`), the raster column
/// of the first text-area pixel (its `x_text` calibration, T40: 136 in PAL and NTSC), and where its 1600 output
/// pixels start (`X_WIN0`).
const TRX_X0: usize = 96;
const TRX_Y0: usize = 16;
const TRX_Y0_60: usize = 31;
const TRX_X_TEXT: usize = 136;
const TRX_X_WIN0: usize = 160;
/// vic_scaler.v: the 400 VIC columns of the window, and the line where a 60 Hz window wraps to line 0.
const TRX_WIN_W: usize = 400;
const TRX_WRAP_60: usize = 263;
/// vic_scaler.v `palette`: the bitstream's own colours, whatever the firmware writes to C64_PALETTE.
const TRX_PALETTE: [u32; 16] = [
    0x000000, 0xFFFFFF, 0x813338, 0x75CEC8, 0x8E3C97, 0x56AC4D, 0x2E2C9B, 0xEDF171, 0x8E5029, 0x553800, 0xC46C71,
    0x4A4A4A, 0x7B7B7B, 0xA9FF9F, 0x706DEB, 0xB2B2B2,
];

/// S37: the C64 frame as vic_scaler.v puts it into the 1920 × 1080 output, on a `canvas` of that or half its size:
/// a window of 400 VIC columns from raster column 96, four output pixels each, from x 160 (black bars of 160 left and
/// right); 270 lines from raster line 16 at four output lines each, or at 60 Hz (a frame of fewer than 300 lines) 240
/// lines from line 31 at 5, 4, 5, 4 output lines (`acc` += 2 per line, the next VIC line at 9).
///
/// The frame is TRX64's canvas, which starts elsewhere in the raster and is narrower than the window: columns map
/// through the text area's first pixel, lines through the frame's first line, and the window's columns beyond the
/// canvas repeat its edge pixel.
fn draw_c64_trx(frame: &C64Frame, out: &mut [u32], canvas: (usize, usize)) {
    let (cw, ch) = canvas;
    if frame.width == 0 || frame.height == 0 {
        return;
    }
    let k = TRX_W / cw;
    let (text_x, first, lines) = match frame.raster_lines {
        0 => (32, 16, 312),
        n => (frame.text_x, frame.first_line, n),
    };
    let v60 = lines < 300;
    let cols: Vec<Option<usize>> = (0..cw)
        .map(|x| {
            let fx = (x * k).checked_sub(TRX_X_WIN0).filter(|&fx| fx < TRX_WIN_W * 4)?;
            let col = (TRX_X0 + fx / 4 + text_x).saturating_sub(TRX_X_TEXT);
            Some(col.min(frame.width - 1))
        })
        .collect();
    for y in 0..ch {
        let fy = y * k;
        let line = if v60 {
            let ln = fy / 9 * 2 + usize::from(fy % 9 >= 5);
            (TRX_Y0_60 + ln) % TRX_WRAP_60
        } else {
            TRX_Y0 + fy / 4
        };
        let row = if line < first { line + lines } else { line } - first;
        let src = &frame.indices[row.min(frame.height - 1) * frame.width..][..frame.width];
        for (px, col) in out[y * cw..][..cw].iter_mut().zip(&cols) {
            if let Some(col) = col {
                *px = TRX_PALETTE[usize::from(src[*col] & 0x0F)];
            }
        }
    }
}

/// S37: the pixel layer over `out`, a `canvas` of the 1920 × 1080 output or of half its size (pix_layer.v): source
/// pixel (p, m) covers output pixels x0 + S·p .. + S, y0 + S·m .. + S, clipped at the frame's edge. A pixel equal to the
/// key is transparent, every other one is blended with the global alpha.
fn draw_layer(layer: &TrxLayer, out: &mut [u32], canvas: (usize, usize)) {
    let (cw, ch) = canvas;
    let k = TRX_W / cw;
    let s = layer.scale.max(1);
    for y in layer.y0.div_ceil(k)..ch {
        let m = (y * k - layer.y0) / s;
        if m >= layer.h {
            break;
        }
        let src = &layer.pixels[m * layer.w..][..layer.w];
        for x in layer.x0.div_ceil(k)..cw {
            let p = (x * k - layer.x0) / s;
            if p >= layer.w {
                break;
            }
            if layer.key != Some(src[p]) {
                let px = &mut out[y * cw + x];
                *px = trx_blend(widen_565(src[p]), *px, layer.alpha);
            }
        }
    }
}

/// RGB565 to 0x00RRGGBB, each channel widened by repeating its top bits (pix_layer.v `fg888`), so 0 and full scale
/// stay 0 and 255.
pub fn widen_565(c: u16) -> u32 {
    let c = u32::from(c);
    let (r, g, b) = (c >> 11, (c >> 5) & 0x3F, c & 0x1F);
    (r << 3 | r >> 2) << 16 | (g << 2 | g >> 4) << 8 | (b << 3 | b >> 2)
}

/// `fg` over `bg` per channel with the blend factor `a` (0-256): `bg + ((fg - bg) · a) >> 8`, the shift arithmetic
/// (pix_layer.v E5-E8). The result never leaves 0-255: it lies between bg and fg.
pub fn trx_blend(fg: u32, bg: u32, a: u16) -> u32 {
    let mix = |shift: u32| {
        let (f, b) = (((fg >> shift) & 0xFF) as i32, ((bg >> shift) & 0xFF) as i32);
        ((b + (((f - b) * i32::from(a)) >> 8)) as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Whether `font` is a C64 character ROM rather than the firmware's overlay font (issue #3). The overlay font
/// (`roms/chars.bin`, 2 KB) has its glyphs at their ASCII codes, so 'A' is glyph 0x41; the C64 ROM (4 KB) puts 'A'
/// at screen code 1 and a graphics symbol at 0x41. A roms directory built from C64 ROMs therefore draws the whole
/// menu in graphics symbols, which is worth saying rather than drawing.
pub fn looks_like_c64_char_rom(font: &[u8]) -> bool {
    /// 'A' in both fonts, at different places (chars.bin 0x41, character ROM 0x01).
    const A: [u8; 8] = [0x18, 0x3C, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00];
    font.len() >= 0x41 * 8 + 8 && font[8..16] == A && font[0x41 * 8..0x41 * 8 + 8] != A
}

/// Parse `fpga/ip/video/vhdl_gen/font_pkg.vhd` into `Renderer::big_font`: the 1024 36-bit `X"…"`
/// words of `c_font` in order (font_pkg.vhd:5-8).
pub fn parse_font_pkg(src: &str) -> Result<Vec<u64>> {
    let words = src
        .split("X\"")
        .skip(1)
        .map(|s| u64::from_str_radix(s.split('"').next().unwrap_or(""), 16))
        .collect::<Result<Vec<_>, _>>()?;
    if words.len() != FONT_ROWS {
        bail!("font_pkg.vhd: {} font words, expected {FONT_ROWS}", words.len());
    }
    Ok(words)
}

/// chars.bin glyphs 0x00-0x1F as ASCII: line graphics (screen.h:6-24) → `+ - |`, solid bars and logo
/// fragments `#`, alpha/beta `a b`, diamond `*`, ball `o`, blank glyphs (0x00, 0x1B) space.
const LOW_GLYPHS: &[u8; 32] = b" +-+|++++++#++++ab#*######o ####";

/// Active text area as ASCII lines (screen codes mapped, trailing spaces trimmed).
///
/// One `\n`-terminated line per ACTIVE_LINES row, starting at POINTER like the chargen. Reverse (bit 7)
/// and colour are dropped, so a selection drawn only with colour (default scheme 1, userinterface.cc:203-208;
/// tree_browser_state.cc:158-159) does not show here. The dump ignores TRANSPARENCY: the firmware keeps
/// the RAM populated while the overlay is hidden (05 Init step 9).
pub fn text_dump(snap: &DisplaySnapshot) -> String {
    let geo = Geometry::new(&snap.regs);
    let mut text = String::with_capacity(geo.rows * (geo.cols + 1));
    for row in 0..geo.rows {
        let line: String = (0..geo.cols)
            .map(|col| match snap.screen.get(geo.cell(row, col)).copied().unwrap_or(0x20) & 0x7F {
                c @ 0x20..=0x7E => c as char,
                c @ 0x00..=0x1F => LOW_GLYPHS[c as usize] as char,
                _ => '#',
            })
            .collect();
        text.push_str(line.trim_end_matches(' '));
        text.push('\n');
    }
    text
}

/// The C64 text screen as ASCII lines: 25 rows of 40, trailing spaces trimmed; empty without a C64.
///
/// Bit 7 (reverse video, the cursor) is dropped, then per character set (docs/specs/S14-c64-trx64.md §9):
/// - `Upper` screen codes: 0x00 `@`, 0x01-0x1A `A`-`Z`, 0x1B-0x1F `[\]^_`, 0x20-0x3F as ASCII, graphics `#`;
/// - `Lower` screen codes: 0x01-0x1A `a`-`z`, 0x41-0x5A `A`-`Z`, the rest as `Upper`;
/// - `Ram`: the Freeze UI's `chars.bin`, read like the overlay (`text_dump`): 0x20-0x7E as ASCII, line graphics.
pub fn c64_text_dump(snap: &DisplaySnapshot) -> String {
    let Some(frame) = &snap.c64 else { return String::new() };
    let glyph = |code: u8| match (frame.charset, code & 0x7F) {
        (C64Charset::Ram, c @ 0x00..=0x1F) => char::from(LOW_GLYPHS[usize::from(c)]),
        (C64Charset::Ram, c @ 0x20..=0x7E) => char::from(c),
        (C64Charset::Lower, c @ 0x01..=0x1A) => char::from(b'`' + c),
        (C64Charset::Lower, c @ 0x41..=0x5A) => char::from(c),
        (C64Charset::Upper | C64Charset::Lower, c @ 0x00..=0x1F) => char::from(b'@' + c),
        (C64Charset::Upper | C64Charset::Lower, c @ 0x20..=0x3F) => char::from(c),
        _ => '#',
    };
    let mut text = String::with_capacity(25 * 41);
    for row in frame.screen.chunks(40).take(25) {
        let line: String = row.iter().map(|&code| glyph(code)).collect();
        text.push_str(line.trim_end_matches(' '));
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const PAL: [u32; 3] = [0x000000, 0x112233, 0x445566];

    fn firmware_file(rel: &str) -> Option<PathBuf> {
        let root = std::env::var_os("UE2_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/1541ultimate"));
        let path = root.join(rel);
        if path.is_file() {
            Some(path)
        } else {
            eprintln!("skipping: {} not found (set UE2_FIRMWARE)", path.display());
            None
        }
    }

    /// Snapshot with `cols` × `rows` cells, CHAR_WIDTH 8, the given CHAR_HEIGHT and TRANSPARENCY,
    /// blank cells (0x20, attr 0x00) and palette entries 0..3 = `PAL`.
    fn snap(cols: u8, rows: u8, height: u8, transparency: u8) -> DisplaySnapshot {
        let mut regs = [0u8; 16];
        regs[REG_CHARS_PER_LINE] = cols;
        regs[REG_ACTIVE_LINES] = rows;
        regs[REG_CHAR_WIDTH] = 8;
        regs[REG_CHAR_HEIGHT] = height;
        regs[REG_TRANSPARENCY] = transparency;
        let mut palette = vec![0u8; 64];
        for (i, rgb) in PAL.iter().enumerate() {
            palette[i * 4..i * 4 + 3].copy_from_slice(&rgb.to_be_bytes()[1..]);
        }
        DisplaySnapshot { regs, screen: vec![0x20; 4096], color: vec![0; 4096], palette, hdmi: [0; 0x1E], cropper: [0; 4], now_ms: 0, c64: None, trx: None }
    }

    /// A `w` × `h` C64 frame of colour index 3 = `C64_BLUE`.
    fn c64_frame(w: usize, h: usize) -> C64Frame {
        let mut palette = [0; 16];
        palette[3] = C64_BLUE;
        C64Frame { width: w, height: h, indices: vec![3; w * h], palette, screen: vec![0x20; 1000], ..C64Frame::default() }
    }

    const C64_BLUE: u32 = 0x0000_00FF;

    /// An overlay pixel of palette index `i` over `under`: the window is drawn at 50 % ([`blend_half`]).
    fn over(i: usize, under: u32) -> u32 {
        blend_half(PAL[i], under)
    }

    #[test]
    fn overlay_pixels_are_drawn_at_half() {
        assert_eq!(blend_half(0x00FF_FFFF, BACKDROP), 0x0087_8787, "white over the backdrop");
        assert_eq!(blend_half(0x0000_0000, 0x00FF_FFFF), 0x007F_7F7F, "black over white");
        assert_eq!(blend_half(0x0000_00FF, 0x0000_00FF), 0x0000_00FF, "a colour over itself is itself");
    }

    #[test]
    fn render_overlay_over_c64_frame() {
        let mut s = snap(2, 1, 0x09, 0x80); // 16×9 grid, transparent index 0
        s.screen[0] = 0x41;
        s.color[0] = 0x21; // fg 1, bg 2; cell 1 is attr 0x00, all transparent
        s.c64 = Some(c64_frame(20, 11));
        let mut out = Vec::new();
        let mut r = Renderer::new(&font());
        assert_eq!(r.render(&s, &mut out), (20, 11), "canvas covers the frame");
        let px = |out: &[u32], x: usize, y: usize| out[y * 20 + x];
        assert_eq!((px(&out, 0, 0), px(&out, 19, 10)), (C64_BLUE, C64_BLUE), "frame around the grid");
        assert_eq!((px(&out, 2, 1), px(&out, 3, 1)), (over(1, C64_BLUE), over(2, C64_BLUE)), "grid centred at (2, 1), row 80");
        assert!((10..18).all(|x| px(&out, x, 1) == over(0, C64_BLUE)), "a bg-0 cell is the panel: black at half over the C64");

        s.regs[REG_TRANSPARENCY] = 0x00;
        r.render(&s, &mut out);
        assert!(out.iter().all(|&p| p == C64_BLUE), "hidden overlay shows the C64");

        s.c64 = Some(c64_frame(4, 2));
        assert_eq!(r.render(&s, &mut out), (16, 9), "canvas covers the grid");
        let px = |x: usize, y: usize| out[y * 16 + x];
        assert_eq!((px(5, 3), px(6, 3), px(9, 4), px(10, 4)), (BACKDROP, C64_BLUE, C64_BLUE, BACKDROP), "frame centred");
    }

    /// PAL SD as the firmware programs it: `SetVideoMode` (hdmi_scan.cc:34) 720×576 with hsync 64, back porch 68,
    /// vsync 5, back porch 39, and the overlay at X_ON 386 / Y_ON 307 (u64_config.cc:2982).
    fn pal_sd(regs: &mut [u8; 16], hdmi: &mut [u8; 0x1E]) {
        hdmi[1] = 64 / 2;
        hdmi[2] = 68 / 4;
        hdmi[3] = (720 / 8) as u8;
        hdmi[9] = 5;
        hdmi[10] = 39;
        hdmi[11] = (576 / 8) as u8;
        (regs[REG_X_ON_HI], regs[REG_X_ON_LO]) = ((386u16 >> 8) as u8, 386u16 as u8);
        (regs[REG_Y_ON_HI], regs[REG_Y_ON_LO]) = ((307u16 >> 8) as u8, 307u16 as u8);
    }

    /// With the HDMI timing programmed the canvas is the output, the C64 fills it, and the overlay window sits
    /// where X_ON/Y_ON put it: 386 - (64 + 68) = 254 across, 307 - (5 + 39) = 263 down.
    #[test]
    fn render_places_the_window_where_the_output_mode_puts_it() {
        let mut s = snap(40, 25, 0x09, 0x80);
        pal_sd(&mut s.regs, &mut s.hdmi);
        s.screen[0] = 0x41;
        s.color[0] = 0x21; // fg 1, bg 2
        s.c64 = Some(c64_frame(384, 272));
        let mut out = Vec::new();
        let mut r = Renderer::new(&font());
        assert_eq!(r.render(&s, &mut out), (720, 576), "the canvas is the output mode");
        let px = |x: usize, y: usize| out[y * 720 + x];
        assert_eq!((px(0, 0), px(719, 575)), (C64_BLUE, C64_BLUE), "the C64 fills the output");
        assert_eq!((px(254, 263), px(255, 263)), (over(1, C64_BLUE), over(2, C64_BLUE)), "the window starts at (254, 263)");
        assert_eq!(px(253, 263), C64_BLUE, "and not a pixel earlier");
        assert!((262..270).all(|x| px(x, 264) == over(0, C64_BLUE)), "a bg-0 cell is the panel over the C64");

        // A window that reaches past the right edge is clipped, not a panic (1080p: X_ON 1438 + 480 of 1920).
        (s.regs[REG_X_ON_HI], s.regs[REG_X_ON_LO]) = ((700u16 >> 8) as u8, 700u16 as u8);
        assert_eq!(r.render(&s, &mut out), (720, 576));
    }

    #[test]
    fn c64_text_dump_maps_screen_codes() {
        let mut s = DisplaySnapshot::default();
        assert_eq!(c64_text_dump(&s), "");
        let mut frame = c64_frame(1, 1);
        frame.screen[..11].copy_from_slice(&[0x08, 0x05, 0x0C, 0x0C, 0x0F, 0x20, 0x00, 0x1B, 0x1F, 0xB1, 0x40]);
        frame.screen[999] = 0x2E;
        s.c64 = Some(frame);
        let dump = c64_text_dump(&s);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines.len(), 25);
        assert_eq!(lines[0], "HELLO @[_1#");
        assert!(lines[1..24].iter().all(|l| l.is_empty()));
        assert_eq!(lines[24], format!("{}.", " ".repeat(39)));

        let frame = s.c64.as_mut().unwrap();
        frame.screen[..11].copy_from_slice(&[0x05, 0x41, 0xDA, 0x5B, 0x31, 0x40, 0x02, 0x20, 0x20, 0x20, 0x20]);
        frame.charset = C64Charset::Lower;
        assert_eq!(c64_text_dump(&s).lines().next(), Some("eAZ#1#b"));
        let frame = s.c64.as_mut().unwrap();
        frame.screen[..11].copy_from_slice(b"SD Card\x02\x06\x7F~");
        frame.charset = C64Charset::Ram;
        assert_eq!(c64_text_dump(&s).lines().next(), Some("SD Card-+#~"), "Freeze UI font");
    }

    /// S07/issue #3: the C64 picture is cropped, scaled by the two scaler ratios and placed by `x_offset` — the
    /// device does not stretch it across the screen. 1080p PAL is the case the firmware's own numbers cover.
    #[test]
    fn the_picture_is_cropped_scaled_and_placed_like_the_device() {
        let mut hdmi = [0u8; 0x1E];
        // hdmi_scan.cc:145-151, e_1920x1080 PAL: SetVicCrop(8, 9, 384, 270), hscaler 0x0C, vscaler 0x08,
        // x_offset 240.
        hdmi[16] = 240;
        hdmi[19] = 0x0C;
        hdmi[20] = 0x08;
        // The firmware writes the size halved, so these are the bytes the register holds.
        let cropper: [u8; 4] = [8, 9, 192, 135];
        let picture = Picture::new(&hdmi, &cropper).expect("the firmware programmed both scalers");
        assert_eq!(picture.crop, (8, 9, 384, 270), "the size registers hold half the size");
        assert_eq!((picture.x, picture.width, picture.height), (240, 1440, 1080));
        assert_eq!(picture.x + picture.width + 240, 1920, "pillarboxed, not stretched across the active area");

        // 720p60: SetVicCrop(8, 0, 384, 240), hscaler 0x09, vscaler 0x15 (SetVideoModeTester, hdmi_scan.cc:106).
        hdmi[19] = 0x09;
        hdmi[20] = 0x15;
        let picture = Picture::new(&hdmi, &[8, 0, 192, 120]).expect("a picture");
        assert_eq!((picture.width, picture.height), (960, 720), "5/2 by 3/1");

        // A code this firmware never writes leaves the picture unplaced, and so does an empty crop.
        hdmi[19] = 0x7F;
        assert_eq!(Picture::new(&hdmi, &cropper), None);
        hdmi[19] = 0x0C;
        assert_eq!(Picture::new(&hdmi, &[8, 9, 0, 0]), None);
    }

    /// Issue #3: the crop's origin counts in the FPGA's stream, not in the TRX64 canvas. Taken as is, 480p left 35
    /// border lines above the text and 5 below it; centred, the borders come out as a C64 has them.
    #[test]
    fn the_crop_is_centred_on_the_canvas() {
        // The PAL canvas: 384x272 from raster line 16, text at lines 51-250 and pixels 32-351.
        let mut frame = c64_frame(384, 272);
        frame.palette[6] = 0x0000_0066;
        for y in 35..235 {
            frame.indices[y * 384 + 32..y * 384 + 352].fill(6);
        }
        // SetVideoModeTester's SetVicCrop(8, 0, 384, 240), shown 1:1.
        let picture = Picture { crop: (8, 0, 384, 240), x: 0, width: 384, height: 240 };
        let mut out = vec![BACKDROP; 384 * 240];
        draw_c64(&frame, Some(Some(&picture)), &mut out, (384, 240), (384, 240));
        let text_rows: Vec<usize> = (0..240).filter(|&y| out[y * 384 + 192] == 0x66).collect();
        assert_eq!((text_rows[0], 239 - text_rows[199]), (19, 21), "border lines above and below the text");
        let text_cols: Vec<usize> = (0..384).filter(|&x| out[120 * 384 + x] == 0x66).collect();
        assert_eq!((text_cols[0], 383 - text_cols[319]), (32, 32), "border pixels left and right");
        assert!(!out.contains(&BACKDROP), "the crop fits the canvas, so no backdrop shows");
    }

    /// Scaled in one step, every C64 column is as wide as its neighbours give or take one target pixel; through the
    /// output mode's 15/8 first and then 2x it would be 2 or 4 wide.
    #[test]
    fn one_step_scaling_keeps_c64_pixels_even() {
        let mut frame = c64_frame(384, 240);
        (frame.palette[1], frame.palette[2]) = (0x11, 0x22);
        for (i, px) in frame.indices.iter_mut().enumerate() {
            *px = 1 + (i % 2) as u8;
        }
        let picture = Picture { crop: (0, 0, 384, 240), x: 0, width: 720, height: 480 };
        let mut out = vec![BACKDROP; 1440 * 960];
        draw_c64(&frame, Some(Some(&picture)), &mut out, (720, 480), (1440, 960));
        let row = &out[500 * 1440..][..1440];
        let mut runs = Vec::new();
        let mut run = 1;
        for w in row.windows(2) {
            if w[0] == w[1] {
                run += 1;
            } else {
                runs.push(run);
                run = 1;
            }
        }
        runs.push(run);
        assert_eq!(runs.len(), 384, "every column shows");
        assert!(runs.iter().all(|r| (3..=4).contains(r)), "3.75 target pixels per column: {runs:?}");
    }

    /// Font with glyph 0x41 = rows 80 01 FF 00 00 00 00 18, all other glyphs blank.
    fn font() -> Vec<u8> {
        let mut font = vec![0u8; 2048];
        font[0x41 * 8..0x41 * 8 + 8].copy_from_slice(&[0x80, 0x01, 0xFF, 0, 0, 0, 0, 0x18]);
        font
    }

    #[test]
    fn render_char_cell_pixels() {
        let mut s = snap(2, 2, 0x09, 0x80);
        s.screen[3] = 0x41; // cell (row 1, col 1)
        s.color[3] = 0x21; // fg 1, bg 2
        let mut out = Vec::new();
        assert_eq!(Renderer::new(&font()).render(&s, &mut out), (16, 18));
        let px = |x: usize, y: usize| out[y * 16 + x];
        assert_eq!((px(8, 9), px(9, 9), px(15, 9)), (over(1, BACKDROP), over(2, BACKDROP), over(2, BACKDROP)), "row 80");
        assert_eq!((px(8, 10), px(15, 10)), (over(2, BACKDROP), over(1, BACKDROP)), "row 01");
        assert!((8..16).all(|x| px(x, 11) == over(1, BACKDROP)), "row FF");
        assert_eq!((px(10, 17), px(11, 17), px(12, 17), px(13, 17)), (over(2, BACKDROP), over(1, BACKDROP), over(1, BACKDROP), over(2, BACKDROP)), "line 8 = 0x18");
        assert!((0..8).all(|x| (0..18).all(|y| px(x, y) == over(0, BACKDROP))), "bg 0 is drawn as the panel, not skipped");
    }

    #[test]
    fn render_line_8_blank_unless_vertical_bar() {
        let mut s = snap(1, 1, 0x09, 0x80);
        s.screen[0] = 0x42;
        s.color[0] = 0x21;
        let mut font = font();
        font[0x42 * 8 + 7] = 0xFF;
        let mut out = Vec::new();
        Renderer::new(&font).render(&s, &mut out);
        assert!(out[56..64].iter().all(|&p| p == over(1, BACKDROP)), "row 7 = FF drawn");
        assert!(out[64..72].iter().all(|&p| p == over(2, BACKDROP)), "line 8 blank for row 7 = FF");
    }

    #[test]
    fn render_reverse_and_transparency() {
        let mut s = snap(1, 1, 0x09, 0x82); // transparent index 2
        s.screen[0] = 0xC1; // reverse 'A'
        s.color[0] = 0x21; // fg 1, bg 2
        let mut out = Vec::new();
        Renderer::new(&font()).render(&s, &mut out);
        assert_eq!((out[0], out[1]), (over(2, BACKDROP), over(1, BACKDROP)), "set bit → bg 2, clear bit → fg 1, both at half; nothing is skipped any more");
    }

    #[test]
    fn render_hidden_and_unprogrammed_are_backdrop() {
        let mut s = snap(40, 25, 0x09, 0x40);
        s.screen.fill(0x41);
        s.color.fill(0x21);
        let mut out = Vec::new();
        let mut r = Renderer::new(&font());
        assert_eq!(r.render(&s, &mut out), (320, 225));
        assert!(out.iter().all(|&p| p == BACKDROP));
        assert_eq!(r.render(&DisplaySnapshot::default(), &mut out), DEFAULT_SIZE);
        assert_eq!(out.len(), 320 * 225);
        assert!(out.iter().all(|&p| p == BACKDROP));
    }

    /// 05 §B cell geometry table: 0x90 = 8×16, 0x5E with CHAR_WIDTH 12 = 12×23.
    #[test]
    fn render_stretch_and_big_font_geometry() {
        let mut out = Vec::new();
        let mut r = Renderer::new(&font());
        let mut s = snap(40, 25, 0x90, 0x80);
        s.screen[0] = 0x41;
        s.color[0] = 0x21;
        assert_eq!(r.render(&s, &mut out), (320, 400));
        assert_eq!((out[0], out[320], out[640]), (over(1, BACKDROP), over(1, BACKDROP), over(2, BACKDROP)), "stretch doubles rows");

        s.regs[REG_CHAR_HEIGHT] = 0x5E;
        s.regs[REG_CHAR_WIDTH] = 12;
        r.big_font[0x41 * 8] = 0xABC_DEF_801;
        r.big_font[0x41 * 8 + 1] = 0x000_000_800;
        assert_eq!(r.render(&s, &mut out), (480, 575));
        let px = |x: usize, y: usize| out[y * 480 + x];
        assert_eq!((px(0, 0), px(11, 0)), (over(1, BACKDROP), over(1, BACKDROP)), "sub-row 0 = 0x801");
        assert_eq!((px(0, 1), px(11, 1)), (over(1, BACKDROP), over(1, BACKDROP)), "sub-row 1 = 0xDEF");
        assert_eq!((px(0, 2), px(11, 2)), (over(1, BACKDROP), over(2, BACKDROP)), "sub-row 2 = 0xABC");
        assert_eq!((px(0, 3), px(1, 3)), (over(1, BACKDROP), over(2, BACKDROP)), "y = 3 skipped, y = 4 is word 1");
    }

    #[test]
    fn render_big_font_fallback_widens_8x8() {
        let r = Renderer::new(&font());
        assert_eq!(r.big_font[0x41 * 8], 0xC00_C00_C00, "0x80 → leftmost 2 of 12 px");
        assert_eq!(r.big_font[0x41 * 8 + 1], 0x001_001_001, "0x01 → rightmost 1 of 12 px");
    }

    #[test]
    fn render_real_chars_bin_glyph() {
        let Some(path) = firmware_file("roms/chars.bin") else { return };
        let mut s = snap(1, 1, 0x09, 0x80);
        s.screen[0] = b'A';
        s.color[0] = 0x01;
        let mut out = Vec::new();
        Renderer::new(&std::fs::read(path).unwrap()).render(&s, &mut out);
        let row1: Vec<bool> = out[8..16].iter().map(|&p| p == over(1, BACKDROP)).collect();
        assert_eq!(row1, [false, false, true, true, true, true, false, false], "chars.bin 'A' row 1 = 0x3C");
    }

    /// Issue #3: the C64 character ROM in a roms directory is the overlay's font only by name.
    #[test]
    fn a_c64_character_rom_is_recognised() {
        let Some(overlay) = firmware_file("roms/chars.bin").and_then(|p| std::fs::read(p).ok()) else { return };
        let Some(c64) = firmware_file("roms/characters.901225-01.bin").and_then(|p| std::fs::read(p).ok()) else {
            return;
        };
        assert!(!looks_like_c64_char_rom(&overlay), "the firmware's own font");
        assert!(looks_like_c64_char_rom(&c64), "the C64 character ROM");
        assert!(!looks_like_c64_char_rom(&[]), "no font at all is someone else's error");
    }

    #[test]
    fn parse_real_font_pkg() {
        let Some(path) = firmware_file("fpga/ip/video/vhdl_gen/font_pkg.vhd") else { return };
        let words = parse_font_pkg(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(words.len(), 1024);
        assert_eq!((words[0x41 * 8], words[0x41 * 8 + 4]), (0x0_2000_0000, 0x3_063F_E3FE), "glyph 'A'");
    }

    #[test]
    fn parse_font_pkg_rejects_bad_input() {
        assert!(parse_font_pkg("X\"000000000\", X\"00000000F\"").is_err(), "too few words");
        assert!(parse_font_pkg(&"X\"00000000G\",".repeat(1024)).is_err(), "bad hex");
    }

    #[test]
    fn text_dump_hello_row() {
        let mut s = snap(40, 3, 0x09, 0x00);
        s.screen[..5].copy_from_slice(b"HELLO");
        s.screen[1] |= 0x80; // reverse / cursor bit is ignored
        s.screen[40..43].copy_from_slice(&[0x06, 0x02, 0x05]); // CHR_UPPER_LEFT, HORIZONTAL, UPPER_RIGHT
        s.screen[119] = b'!';
        assert_eq!(text_dump(&s), format!("HELLO\n+-+\n{}!\n", " ".repeat(39)));
        s.regs[REG_POINTER_LO] = 40;
        s.regs[REG_ACTIVE_LINES] = 1;
        assert_eq!(text_dump(&s), "+-+\n");
        assert_eq!(text_dump(&DisplaySnapshot::default()), "");
    }

    /// pix_layer.v E5-E8 bit for bit: a 9-bit signed difference times the 9-bit factor, bits 15:8 of the product
    /// added to bg in 8 bits.
    fn verilog_blend(fg: u8, bg: u8, galpha: u8) -> u8 {
        let a = i32::from(galpha) + i32::from(galpha >> 7);
        let product = (i32::from(fg) - i32::from(bg)) * a;
        let p8 = (product as u32 >> 8) & 0xFF;
        (u32::from(bg) + p8) as u8
    }

    #[test]
    fn rgb565_widens_by_repeating_the_top_bits() {
        assert_eq!(widen_565(0x0000), 0x00_0000);
        assert_eq!(widen_565(0xFFFF), 0xFF_FFFF);
        assert_eq!((widen_565(0xF800), widen_565(0x07E0), widen_565(0x001F)), (0xFF_0000, 0x00_FF00, 0x00_00FF));
        assert_eq!(widen_565(0x8410), 0x84_8284, "R 10000 -> 10000100, G 100000 -> 10000010");
    }

    #[test]
    fn the_blend_is_the_verilogs_arithmetic() {
        for galpha in [0u8, 1, 127, 128, 200, 254, 255] {
            let a = u16::from(galpha) + u16::from(galpha >> 7);
            for fg in (0..=255u8).step_by(5) {
                for bg in (0..=255u8).step_by(3) {
                    let want = verilog_blend(fg, bg, galpha);
                    let got = trx_blend(u32::from(fg) << 16 | u32::from(bg), u32::from(bg) << 16 | u32::from(fg), a);
                    assert_eq!((got >> 16) as u8, want, "fg {fg} bg {bg} G {galpha}");
                    assert_eq!(got as u8, verilog_blend(bg, fg, galpha), "the other way round");
                }
            }
        }
        assert_eq!(trx_blend(0x12_3456, 0xAB_CDEF, 256), 0x12_3456, "G 255: the layer, exactly");
        assert_eq!(trx_blend(0x12_3456, 0xAB_CDEF, 0), 0xAB_CDEF, "G 0: the picture below");
        assert_eq!(trx_blend(0xFF_0000, 0x00_00FF, 129), 0x80_007E, "G 128: a = 129, the shift floors");
    }

    /// A snapshot with the TRX layer `layer` on, no overlay window and no C64.
    fn trx_snap(layer: TrxLayer) -> DisplaySnapshot {
        DisplaySnapshot { trx: Some(layer), ..DisplaySnapshot::default() }
    }

    fn trx_layer(x0: usize, y0: usize, w: usize, h: usize, scale: usize, pixels: Vec<u16>) -> TrxLayer {
        TrxLayer { x0, y0, w, h, scale, alpha: 256, key: None, pixels }
    }

    const RED: u32 = 0xFF_0000;
    const BLUE: u32 = 0x00_00FF;

    #[test]
    fn the_layer_scales_by_s_and_clips_at_the_frame() {
        let mut r = Renderer::new(&font());
        let mut out = Vec::new();
        let s = trx_snap(trx_layer(10, 20, 2, 1, 3, vec![0xF800, 0x001F]));
        assert_eq!(r.render(&s, &mut out), (1920, 1080));
        let px = |out: &[u32], x: usize, y: usize| out[y * 1920 + x];
        assert!((10..13).all(|x| (20..23).all(|y| px(&out, x, y) == RED)), "3 x 3");
        assert!((13..16).all(|x| px(&out, x, 22) == BLUE));
        assert_eq!((px(&out, 9, 20), px(&out, 16, 20), px(&out, 10, 23)), (0, 0, 0));

        let s = trx_snap(trx_layer(5, 7, 2, 1, 1, vec![0xF800, 0x001F]));
        r.render(&s, &mut out);
        assert_eq!((px(&out, 5, 7), px(&out, 6, 7), px(&out, 7, 7), px(&out, 5, 8)), (RED, BLUE, 0, 0), "1 x 1");

        let s = trx_snap(trx_layer(1918, 1079, 4, 3, 1, vec![0xFFFF; 12]));
        r.render(&s, &mut out);
        assert_eq!(out.iter().filter(|&&p| p != 0).count(), 2, "clipped to 1918-1919 of line 1079");
    }

    #[test]
    fn scale_2_on_even_coordinates_is_built_at_half_size() {
        let mut r = Renderer::new(&font());
        let mut out = Vec::new();
        let s = trx_snap(trx_layer(100, 50, 2, 2, 2, vec![0xF800, 0x001F, 0x001F, 0xF800]));
        assert_eq!(r.render(&s, &mut out), (960, 540));
        let px = |out: &[u32], x: usize, y: usize| out[y * 960 + x];
        assert_eq!((px(&out, 50, 25), px(&out, 51, 25), px(&out, 50, 26), px(&out, 52, 25)), (RED, BLUE, BLUE, 0));
        let s = trx_snap(trx_layer(101, 50, 2, 2, 2, vec![0xF800; 4]));
        assert_eq!(Renderer::canvas(&s), (1920, 1080), "odd x0");
        let mut s = trx_snap(trx_layer(100, 50, 2, 2, 2, vec![0xF800; 4]));
        s.regs = snap(40, 25, 0x09, 0x80).regs;
        assert_eq!(Renderer::canvas(&s), (1920, 1080), "an overlay window up: its text is drawn 1:1");
        // Resampled to the window's size in one step.
        let s = trx_snap(trx_layer(0, 0, 1, 1, 2, vec![0xF800]));
        r.render_scaled(&s, &mut out, (480, 270));
        assert_eq!((out.len(), out[0], out[1]), (480 * 270, RED, 0));
    }

    #[test]
    fn the_key_is_transparent_and_alpha_blends() {
        let mut r = Renderer::new(&font());
        let mut out = Vec::new();
        let mut layer = trx_layer(0, 0, 2, 1, 1, vec![0x1234, 0xFFFF]);
        layer.key = Some(0x1234);
        layer.alpha = 129;
        let mut s = trx_snap(layer);
        let mut frame = c64_frame(384, 272);
        frame.indices.fill(1);
        (frame.text_x, frame.first_line, frame.raster_lines) = (32, 16, 312);
        s.c64 = Some(frame);
        r.render(&s, &mut out);
        assert_eq!(out[0], 0, "keyed: the bar below");
        assert_eq!(out[1], trx_blend(0xFF_FFFF, 0, 129));
        let row = &out[100 * 1920..][..1920];
        assert_eq!((row[159], row[160], row[1759], row[1760]), (0, 0xFF_FFFF, 0xFF_FFFF, 0), "bars of 160");
    }

    #[test]
    fn the_c64_sits_at_4x_as_vic_scaler_puts_it() {
        let mut r = Renderer::new(&font());
        let mut out = Vec::new();
        let mut s = trx_snap(trx_layer(0, 0, 1, 1, 1, vec![0]));
        s.trx.as_mut().unwrap().key = Some(0);
        let mut frame = c64_frame(384, 272);
        frame.indices.fill(0);
        (frame.text_x, frame.first_line, frame.raster_lines) = (32, 16, 312);
        frame.indices[32] = 1; // line 16, the first text-area column
        frame.indices[0] = 2;
        frame.indices[269 * 384 + 383] = 7; // line 285, the window's last
        s.c64 = Some(frame);
        r.render(&s, &mut out);
        let px = |x: usize, y: usize| out[y * 1920 + x];
        let white = TRX_PALETTE[1];
        assert!((320..324).all(|x| (0..4).all(|y| px(x, y) == white)), "raster column 136 at x 160 + 4 * 40");
        assert_eq!((px(319, 0), px(324, 0), px(320, 4)), (0, 0, 0));
        assert!((160..196).all(|x| px(x, 0) == TRX_PALETTE[2]), "columns 96-104 left of the canvas repeat its edge");
        assert_eq!(px(196, 0), 0);
        assert_eq!((px(1724, 1079), px(1724, 1076)), (TRX_PALETTE[7], TRX_PALETTE[7]), "canvas column 383 is window 391");
        assert_eq!(px(1724, 1075), 0);

        // 60 Hz: 240 lines from line 31 at 5, 4, 5, 4 output lines; TRX64's NTSC canvas starts at line 28.
        let mut frame = c64_frame(384, 247);
        frame.indices.fill(0);
        (frame.text_x, frame.first_line, frame.raster_lines) = (32, 28, 263);
        frame.indices[3 * 384 + 32] = 1; // line 31
        frame.indices[4 * 384 + 32] = 2; // line 32
        frame.indices[242 * 384 + 32] = 7; // line 7 of the next frame, the window's last
        s.c64 = Some(frame);
        r.render(&s, &mut out);
        let px = |x: usize, y: usize| out[y * 1920 + x];
        let col: Vec<u32> = (0..10).map(|y| px(320, y)).collect();
        assert_eq!(col[..5], [white; 5]);
        assert_eq!(col[5..9], [TRX_PALETTE[2]; 4]);
        assert_eq!((px(320, 1079), px(320, 1076)), (TRX_PALETTE[7], TRX_PALETTE[7]), "the last 4 of 9");
        assert_ne!(px(320, 1075), TRX_PALETTE[7]);
    }
}
