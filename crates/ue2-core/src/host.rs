//! Types crossing the host/machine boundary. Additive changes only (report them).

use crate::c64host::C64Frame;

/// Host → machine input events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostInput {
    /// C64 keyboard matrix position; convention documented on `devices::u64io::U64Io::set_key`.
    Key { row: u8, col: u8, down: bool },
    /// Physical joystick on C64 control port 2, lines active low (idle 0xFF): `JoystickPort` with port 2.
    Joystick(u8),
    /// S36: physical joystick on C64 control port `port` (1 or 2), lines active low: bit 0 up, 1 down, 2 left,
    /// 3 right, 4 fire (idle 0xFF). The C64 sees them ANDed with the firmware's C64_JOYx_SWOUT; U64II_KEYB_JOY
    /// reads the same. Other ports are dropped.
    JoystickPort { port: u8, lines: u8 },
    /// ITU menu button (0x1000000A bit 6).
    MenuButton(bool),
    /// Key on the USB keyboard (`MachineConfig::usb`): HID usage ID of the Keyboard/Keypad page 0x07, modifiers
    /// 0xE0-0xE7 included (docs/hw/09-usb.md §F7). Dropped when no USB keyboard is attached.
    UsbKey { usage: u8, down: bool },
    /// Plug the device on USB hub port `port` (1-based) in or out (`devices::usb::Usb::set_connected`). A plug-in
    /// waits until the firmware has handled the previous unplug. Dropped for an empty port.
    UsbPlug { port: u8, connected: bool },
    /// S32: move the USB mouse by `dx`, `dy` (right, down), turn its wheel, and hold `buttons` (`usb::BUTTON_*`).
    /// Dropped when no USB mouse is attached.
    UsbMouse { dx: i32, dy: i32, wheel: i32, buttons: u8 },
    /// C64 RESTORE key: NMI level while held (docs/specs/S14-c64-trx64.md §6).
    Restore(bool),
}

/// Everything a renderer needs to draw the overlay UI. Filled by `devices::overlay::Overlay::snapshot`.
#[derive(Clone, Debug, Default)]
pub struct DisplaySnapshot {
    /// Overlay chargen registers 0x10140000.. (latched writes).
    pub regs: [u8; 16],
    /// Screen RAM 0x10141000 (4096 bytes).
    pub screen: Vec<u8>,
    /// Colour RAM 0x10142000 (4096 bytes).
    pub color: Vec<u8>,
    /// HDMI palette 0x10145000 (16 × 4 bytes).
    pub palette: Vec<u8>,
    /// HDMI timing registers 0x10144000 (`t_video_timing_regs`, u64.h:172-203): the output mode the firmware
    /// programmed, which is what the overlay's X_ON/Y_ON count in.
    pub hdmi: [u8; 0x1E],
    /// VIC cropper 0x10148000 (`t_vic_crop_regs`, hdmi_scan.cc:45-60): which part of the VIC picture goes to the
    /// scaler, `offset_x`, `offset_y`, `size_x >> 1`, `size_y >> 1`.
    pub cropper: [u8; 4],
    /// Emulated time of the snapshot.
    pub now_ms: u64,
    /// C64 frame under the overlay; None without an attached C64 (docs/specs/S14-c64-trx64.md §9).
    pub c64: Option<C64Frame>,
    /// S37: the TRX output picture while it differs from the output mode's: the pixel layer is on or the machine
    /// pause is in effect. None otherwise and without `--trx-io`. Filled by `Machine::display`.
    pub trx: Option<TrxOutput>,
}

/// S37: what the TRX bitstream puts over and in place of the C64 picture (docs/specs/S37-trx-pixel-layer.md §1,
/// §5.3, §5.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrxOutput {
    /// The pixel layer, None while it is off.
    pub layer: Option<TrxLayer>,
    /// The machine pause is in effect: the C64 window is black (vic_scaler.v `blank`).
    pub paused: bool,
    /// The C64 text layer (c64_text.v), None while it is not shown.
    pub text: Option<TrxText>,
}

/// S37 phase B: the freeze menu's text as c64_text.v draws it: 40 × 25 cells of 8 × 8, each C64 pixel 4 × 4 output
/// pixels from output (320, 140), over everything else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrxText {
    /// Glyph byte of cell `col` on text line `line` (0-199) at `40 line + col`, bit 7 the leftmost pixel.
    pub glyphs: Vec<u8>,
    /// Colour RAM nibble of cell `40 row + col`.
    pub colours: Vec<u8>,
    /// `$D021` b3:0, drawn where a glyph bit is clear when `opaque`.
    pub background: u8,
    pub opaque: bool,
    /// The 1000 screen codes and the character set they are drawn with, for the text dump of what is shown.
    pub screen: Vec<u8>,
    pub charset: crate::c64host::C64Charset,
}

/// S37: what the pixel layer shows in one frame (pix_layer.v, the `f_*` values of its frame start).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrxLayer {
    /// Output pixel of the top left in the 1920 × 1080 frame; x0 < 1920, y0 < 1080.
    pub x0: usize,
    pub y0: usize,
    /// Source pixels, w ≤ 960, h ≤ 1080, both at least 1.
    pub w: usize,
    pub h: usize,
    /// Output pixels per source pixel in both directions, 1-3.
    pub scale: usize,
    /// Blend factor 0-256: the global alpha G plus G bit 7.
    pub alpha: u16,
    /// RGB565 colour key, when on.
    pub key: Option<u16>,
    /// `w × h` RGB565 pixels, row by row.
    pub pixels: Vec<u16>,
}

/// Host end of the wired Ethernet segment behind the RMII MAC (docs/hw/08-network-rmii.md T1 §8). Frames are
/// Ethernet II from the destination MAC to the end of the payload: no preamble, no FCS. Implemented outside the
/// core (e.g. the libslirp bridge in `ue2-net`) and pumped by the emulation thread through
/// `devices::rmii::Rmii::exchange`.
pub trait NetBackend {
    /// A frame the guest transmitted.
    fn send(&mut self, frame: &[u8]);
    /// Service the host side without blocking and hand every frame addressed to the guest to `deliver`.
    fn poll(&mut self, deliver: &mut dyn FnMut(&[u8]));
}
