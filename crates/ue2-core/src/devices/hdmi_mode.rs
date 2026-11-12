//! S45: the MultiCore host's HDMI modes (host/hdmi_modes.vh, hdmi_timing.v) and its mode request (mode_req.v) in the
//! overlay window at 0x10144F00. Spec: docs/specs/S45-multicore-video.md.

use crate::time::CLOCK_HZ;

/// One row of hdmi_modes.vh `hm_row`: active area, porches and sync per axis, and the pixel clock (`hm_rate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub hact: u32,
    pub hfp: u32,
    pub hsync: u32,
    pub hbp: u32,
    pub vact: u32,
    pub vfp: u32,
    pub vsync: u32,
    pub vbp: u32,
    /// Pixel clock in Hz.
    pub rate: u32,
    /// `hm_locked`: the row's frame is the VIC frame (the two 1080p rows).
    pub locked: bool,
    /// The CEA-861 video code (`hm_row` vic), 0 for the PC rows.
    pub vic: u8,
    /// `hpol`, `vpol` (equal on every row): the syncs are positive.
    pub pos_sync: bool,
}

impl Row {
    /// hdmi_timing.v `xact0`, `yact0`: blank pixels before the active ones on a line, blank lines before the active
    /// ones in a frame.
    pub fn xact0(&self) -> u32 {
        self.hfp + self.hsync + self.hbp
    }

    pub fn yact0(&self) -> u32 {
        self.vfp + self.vsync + self.vbp
    }

    pub fn htot(&self) -> u32 {
        self.xact0() + self.hact
    }

    pub fn vtot(&self) -> u32 {
        self.yact0() + self.vact
    }

    /// `lines` output lines in emulator clocks.
    pub fn lines_clocks(&self, lines: u32) -> u64 {
        u64::from(lines) * u64::from(self.htot()) * CLOCK_HZ / u64::from(self.rate)
    }

    /// One frame in emulator clocks.
    pub fn frame_clocks(&self) -> u64 {
        self.lines_clocks(self.vtot())
    }
}

#[allow(clippy::too_many_arguments)]
const fn row(
    vic: u8,
    hact: u32,
    hfp: u32,
    hsync: u32,
    hbp: u32,
    vact: u32,
    vfp: u32,
    vsync: u32,
    vbp: u32,
    rate: u32,
    locked: bool,
    pos_sync: bool,
) -> Option<Row> {
    Some(Row { hact, hfp, hsync, hbp, vact, vfp, vsync, vbp, rate, locked, vic, pos_sync })
}

/// hdmi_modes.vh, row m = family × 6 + hdmi; rows 3, 4 and 5 (50 Hz PC modes) do not exist (`hm_valid`).
pub const ROWS: [Option<Row>; 12] = [
    row(17, 720, 12, 64, 68, 576, 5, 5, 39, 27_067_117, false, false),
    row(19, 1280, 440, 40, 220, 720, 5, 5, 20, 74_434_572, false, true),
    row(31, 1920, 528, 44, 148, 1080, 4, 5, 36, 148_869_143, true, true),
    None,
    None,
    None,
    row(1, 640, 16, 96, 48, 480, 10, 2, 33, 25_125_345, false, false),
    row(4, 1280, 110, 40, 220, 720, 5, 5, 20, 74_035_605, false, true),
    row(16, 1920, 88, 44, 148, 1080, 4, 5, 36, 148_071_211, true, true),
    row(0, 800, 40, 124, 88, 600, 20, 5, 25, 41_051_794, false, true),
    row(0, 1024, 24, 136, 160, 768, 3, 6, 29, 64_995_343, false, false),
    row(0, 1280, 48, 112, 248, 1024, 1, 3, 24, 106_548_910, false, true),
];

/// mode_req.v's reset value: 1080p 50.
pub const ROW_POWER_ON: u8 = 2;

/// Row `m`, None for one that does not exist.
pub fn row_of(m: u8) -> Option<Row> {
    ROWS.get(usize::from(m)).copied().flatten()
}

/// videomode.cc `video_mode_index` of a hand-over video word (`video_pack`: b31 set, b15:8 the system, b7:0 the
/// HDMI mode), None for a word that names no row.
pub fn row_of_video(word: u32) -> Option<u8> {
    if word & 0x8000_0000 == 0 {
        return None;
    }
    let (sys, hdmi) = ((word >> 8) & 0xFF, word & 0xFF);
    // VID_SYS_NTSC60 is 1 (videomode.h); VID_HDMI_COUNT is 6.
    let m = if sys == 1 { 6 } else { 0 } + hdmi;
    (hdmi < 6).then_some(m as u8).filter(|&m| row_of(m).is_some())
}

/// The overlay window's offsets of mode_req.v (`addr` 0x4F00-0x4F09).
pub const REQ: u32 = 0x4F00;
const MAGIC_AT: u32 = 0x4F04;
const VERSION_AT: u32 = 0x4F05;
const FLAGS_AT: u32 = 0x4F06;
const MODE_AT: u32 = 0x4F07;
const SEEN_AT: u32 = 0x4F08;
const DONE_AT: u32 = 0x4F09;

/// mode_req.v as the apps see it. A request is taken at once: the processed count follows the written one, the new
/// row runs, and running, pix_locked and gt_ready stay set (the device needs some clocks and a relock for it).
/// c64_type is 0 (top.v:1398-1402), so every row of the table is taken as asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeReq {
    /// The row running (`mode_q`).
    pub row: u8,
    /// Requests written and processed (8 bits each).
    pub written: u8,
    pub processed: u8,
    /// The last request was refused.
    pub refused: bool,
}

impl ModeReq {
    /// At power on, or with the row the hand-over page names (S45 §4).
    pub fn new(row: u8) -> Self {
        ModeReq { row, written: 0, processed: 0, refused: false }
    }

    /// The running row's table entry.
    pub fn current(&self) -> Row {
        row_of(self.row).expect("the running row is one of the table")
    }

    /// A write to the overlay window at `off`; true when it was mode_req's.
    pub fn write(&mut self, off: u32, v: u8) -> bool {
        if off != REQ {
            return false;
        }
        self.written = self.written.wrapping_add(1);
        self.processed = self.processed.wrapping_add(1);
        let ok = v >> 4 == 0 && row_of(v).is_some();
        self.refused = !ok;
        if ok {
            self.row = v;
        }
        true
    }

    /// A read of the overlay window at `off`, None outside 0x4F04-0x4F09.
    pub fn read(&self, off: u32) -> Option<u8> {
        Some(match off {
            MAGIC_AT => 0x48,
            VERSION_AT => 1,
            // {refused, 0, 0, c64_type, locked, running, pix_locked, gt_ready}
            FLAGS_AT => u8::from(self.refused) << 7 | u8::from(self.current().locked) << 3 | 0x07,
            MODE_AT => self.row,
            SEEN_AT => self.written,
            DONE_AT => self.processed,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S45 T1: the reads, a request taken, refusals (bits 7:4, above 11, the 50 Hz PC rows) leave the row.
    #[test]
    fn t1_mode_req_reads_and_refusals() {
        let mut m = ModeReq::new(ROW_POWER_ON);
        assert_eq!([0x4F04, 0x4F05, 0x4F06, 0x4F07, 0x4F08, 0x4F09].map(|o| m.read(o).unwrap()), [0x48, 1, 0x0F, 2, 0, 0]);
        assert_eq!(m.read(0x4F03), None);
        assert!(m.write(REQ, 6));
        assert_eq!((m.read(0x4F06), m.read(0x4F07), m.read(0x4F08), m.read(0x4F09)), (Some(0x07), Some(6), Some(1), Some(1)));
        for bad in [3, 5, 12, 0x16] {
            m.write(REQ, bad);
            assert_eq!((m.row, m.read(0x4F06).unwrap() & 0x80), (6, 0x80), "{bad} refused");
        }
        m.write(REQ, 11);
        assert_eq!((m.row, m.refused, m.written, m.processed), (11, false, 6, 6));
        assert!(!m.write(0x4F01, 2));
    }

    /// S45 T2: the hand-over word's row (`video_mode_index`).
    #[test]
    fn t2_row_of_the_handover_word() {
        let pack = |sys: u32, hdmi: u32| 0x8000_0000 | sys << 8 | hdmi;
        assert_eq!(row_of_video(pack(0, 0)), Some(0));
        assert_eq!(row_of_video(pack(1, 0)), Some(6));
        assert_eq!(row_of_video(pack(1, 1)), Some(7));
        assert_eq!(row_of_video(pack(2, 2)), Some(2), "PAL60 counts as family 0");
        assert_eq!(row_of_video(pack(0, 4)), None, "no 50 Hz 1024 × 768");
        assert_eq!(row_of_video(pack(1, 4)), Some(10));
        assert_eq!(row_of_video(pack(1, 6)), None);
        assert_eq!(row_of_video(0), None);
    }

    /// The rasters of edid.cc's table (htotal, vtotal), which the generated file and the screen's EDID agree on.
    #[test]
    fn rasters_match_edid_cc() {
        let want = [
            (0, 864, 625),
            (1, 1980, 750),
            (2, 2640, 1125),
            (6, 800, 525),
            (7, 1650, 750),
            (8, 2200, 1125),
            (9, 1052, 650),
            (10, 1344, 806),
            (11, 1688, 1052),
        ];
        for (m, h, v) in want {
            let r = row_of(m).unwrap();
            assert_eq!((r.htot(), r.vtot()), (h, v), "row {m}");
        }
        // 1080p50: 2640 × 1125 at 148.869 MHz is the VIC's 50.12 Hz.
        let r = row_of(2).unwrap();
        assert_eq!(r.frame_clocks(), 1125 * 2640 * CLOCK_HZ / 148_869_143);
    }
}
