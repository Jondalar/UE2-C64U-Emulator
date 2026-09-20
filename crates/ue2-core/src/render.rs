//! Overlay renderer and text dump. Spec: docs/specs/S07-overlay-u64io-render.md
//!
//! Pixel semantics follow the open chargen IP as summarised in docs/hw/05-ui-overlay-input.md §B
//! (char_generator_regs.vhd, char_generator_slave12.vhd).
//!
//! The image is the output mode the firmware programmed into the HDMI timing registers: the C64 frame scaled into
//! the active area, and the text grid on top where the chargen registers put it — X_ON/Y_ON for the corner,
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
use crate::host::DisplaySnapshot;

/// Colour of transparent pixels and of the whole image while the overlay is hidden, where no C64 frame shows
/// through.
pub const BACKDROP: u32 = 0x0010_1010;

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
        Renderer { font: font.to_vec(), big_font }
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
    pub fn render(&mut self, snap: &DisplaySnapshot, out: &mut Vec<u32>) -> (usize, usize) {
        let geo = Geometry::new(&snap.regs);
        let lines = geo.lines();
        let (w, h) = (geo.cols * geo.char_width, geo.rows * lines.len());
        let (gw, gh) = if w == 0 || h == 0 { DEFAULT_SIZE } else { (w, h) };
        let output = Output::new(&snap.hdmi);
        let (cw, ch) = match &output {
            Some(o) => (o.width, o.height),
            None => snap.c64.as_ref().map_or((gw, gh), |f| (f.width.max(gw), f.height.max(gh))),
        };
        out.clear();
        out.resize(cw * ch, BACKDROP);
        if let Some(frame) = &snap.c64 {
            match &output {
                Some(_) => draw_c64_scaled(frame, out, cw, ch),
                None => draw_c64(frame, out, cw, ch),
            }
        }
        if w != 0 && h != 0 && geo.visible {
            let origin = match &output {
                Some(o) => (geo.x_on.saturating_sub(o.x0), geo.y_on.saturating_sub(o.y0)),
                None => ((cw - w) / 2, (ch - h) / 2),
            };
            self.draw_text(snap, &geo, &lines, out, (cw, ch), origin);
        }
        (cw, ch)
    }

    /// Draw the opaque pixels of the text grid with its top-left corner at `origin`, clipped to the canvas: a
    /// window placed near the right edge can reach past it (u64_config.cc:2993, 1080p X_ON 1438 + 480).
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
                        if idx != geo.transparent {
                            *px = palette[idx as usize];
                        }
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

/// Palettize `frame` stretched over the whole canvas, as the device's scaler puts the VIC picture on the output
/// (nearest neighbour; the hardware filters).
fn draw_c64_scaled(frame: &C64Frame, out: &mut [u32], canvas_w: usize, canvas_h: usize) {
    if frame.width == 0 || frame.height == 0 {
        return;
    }
    for y in 0..canvas_h {
        let src = (y * frame.height / canvas_h) * frame.width;
        let dst = &mut out[y * canvas_w..][..canvas_w];
        for (x, px) in dst.iter_mut().enumerate() {
            let idx = frame.indices[src + x * frame.width / canvas_w];
            *px = frame.palette[usize::from(idx & 0x0F)];
        }
    }
}

/// Palettize `frame` centred into the `canvas_w` × `canvas_h` image (canvas at least the frame size).
fn draw_c64(frame: &C64Frame, out: &mut [u32], canvas_w: usize, canvas_h: usize) {
    if frame.width == 0 {
        return;
    }
    let (x0, y0) = ((canvas_w - frame.width) / 2, (canvas_h - frame.height) / 2);
    for (y, row) in frame.indices.chunks_exact(frame.width).take(frame.height).enumerate() {
        let dst = &mut out[(y0 + y) * canvas_w + x0..][..frame.width];
        for (px, &idx) in dst.iter_mut().zip(row) {
            *px = frame.palette[usize::from(idx & 0x0F)];
        }
    }
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
        DisplaySnapshot { regs, screen: vec![0x20; 4096], color: vec![0; 4096], palette, hdmi: [0; 0x1E], now_ms: 0, c64: None }
    }

    /// A `w` × `h` C64 frame of colour index 3 = `C64_BLUE`.
    fn c64_frame(w: usize, h: usize) -> C64Frame {
        let mut palette = [0; 16];
        palette[3] = C64_BLUE;
        C64Frame { width: w, height: h, indices: vec![3; w * h], palette, screen: vec![0x20; 1000], ..C64Frame::default() }
    }

    const C64_BLUE: u32 = 0x0000_00FF;

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
        assert_eq!((px(&out, 2, 1), px(&out, 3, 1)), (PAL[1], PAL[2]), "grid centred at (2, 1), row 80");
        assert!((10..18).all(|x| px(&out, x, 1) == C64_BLUE), "transparent cell shows the C64");

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
        assert_eq!((px(254, 263), px(255, 263)), (PAL[1], PAL[2]), "the window starts at (254, 263)");
        assert_eq!(px(253, 263), C64_BLUE, "and not a pixel earlier");
        assert!((262..270).all(|x| px(x, 264) == C64_BLUE), "a transparent cell shows the C64 through");

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
        assert_eq!((px(8, 9), px(9, 9), px(15, 9)), (PAL[1], PAL[2], PAL[2]), "row 80");
        assert_eq!((px(8, 10), px(15, 10)), (PAL[2], PAL[1]), "row 01");
        assert!((8..16).all(|x| px(x, 11) == PAL[1]), "row FF");
        assert_eq!((px(10, 17), px(11, 17), px(12, 17), px(13, 17)), (PAL[2], PAL[1], PAL[1], PAL[2]), "line 8 = 0x18");
        assert!((0..8).all(|x| (0..18).all(|y| px(x, y) == BACKDROP)), "bg 0 = transparent index 0");
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
        assert!(out[56..64].iter().all(|&p| p == PAL[1]), "row 7 = FF drawn");
        assert!(out[64..72].iter().all(|&p| p == PAL[2]), "line 8 blank for row 7 = FF");
    }

    #[test]
    fn render_reverse_and_transparency() {
        let mut s = snap(1, 1, 0x09, 0x82); // transparent index 2
        s.screen[0] = 0xC1; // reverse 'A'
        s.color[0] = 0x21; // fg 1, bg 2
        let mut out = Vec::new();
        Renderer::new(&font()).render(&s, &mut out);
        assert_eq!((out[0], out[1]), (BACKDROP, PAL[1]), "set bit → bg (transparent), clear bit → fg");
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
        assert_eq!((out[0], out[320], out[640]), (PAL[1], PAL[1], PAL[2]), "stretch doubles rows");

        s.regs[REG_CHAR_HEIGHT] = 0x5E;
        s.regs[REG_CHAR_WIDTH] = 12;
        r.big_font[0x41 * 8] = 0xABC_DEF_801;
        r.big_font[0x41 * 8 + 1] = 0x000_000_800;
        assert_eq!(r.render(&s, &mut out), (480, 575));
        let px = |x: usize, y: usize| out[y * 480 + x];
        assert_eq!((px(0, 0), px(11, 0)), (PAL[1], PAL[1]), "sub-row 0 = 0x801");
        assert_eq!((px(0, 1), px(11, 1)), (PAL[1], PAL[1]), "sub-row 1 = 0xDEF");
        assert_eq!((px(0, 2), px(11, 2)), (PAL[1], PAL[2]), "sub-row 2 = 0xABC");
        assert_eq!((px(0, 3), px(1, 3)), (PAL[1], PAL[2]), "y = 3 skipped, y = 4 is word 1");
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
        let row1: Vec<bool> = out[8..16].iter().map(|&p| p == PAL[1]).collect();
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
}
