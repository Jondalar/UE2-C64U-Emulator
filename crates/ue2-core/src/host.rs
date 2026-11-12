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
    /// ITU menu button (0x1000000A bit 6). S42: with `--trx-io` the board has no such line; a press is the rocker's
    /// [`Rocker::Up`] and a release does nothing.
    MenuButton(bool),
    /// S42: a gesture on the C64U's rocker, which the ESP32 sends as an event packet over the WiFi UART. Only the
    /// `--trx-io` bitstream decodes it; dropped otherwise.
    Rocker(Rocker),
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

/// S42: the rocker gestures the ESP32 reports (u64ctrl `button_handler.h`, sent by `send_button_event`,
/// rpc_dispatch.c:389-402).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rocker {
    /// BUTTON_UP_SHORT → EVENT_MENU 0x82.
    Up,
    /// BUTTON_UP_LONG → EVENT_RESET 0x83.
    UpLong,
    /// BUTTON_DOWN_SHORT → EVENT_FREEZE 0x81.
    Down,
    /// BUTTON_DOWN_LONG → EVENT_BUTTON 0x80.
    DownLong,
}

impl Rocker {
    /// The event code the ESP32 puts in the command byte of the packet's RPC header (rpc_calls.h:219-222).
    pub fn code(self) -> u8 {
        match self {
            Rocker::Up => 0x82,
            Rocker::UpLong => 0x83,
            Rocker::Down => 0x81,
            Rocker::DownLong => 0x80,
        }
    }

    /// The control-language name (`rocker up|up-long|down|down-long`).
    pub fn by_name(name: &str) -> Option<Rocker> {
        match name {
            "up" => Some(Rocker::Up),
            "up-long" => Some(Rocker::UpLong),
            "down" => Some(Rocker::Down),
            "down-long" => Some(Rocker::DownLong),
            _ => None,
        }
    }
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
    /// S37: the TRX output picture (the C64 at 4× between bars, the pixel layer and the text layer; 1920 × 1080, or
    /// with the MultiCore host the running HDMI mode's active area, S45), always with `--trx-io`, as the bitstream has
    /// no other output. None without it. Filled by `Machine::display`.
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
    /// S45: the MultiCore host's running HDMI mode; None for TRX64-Ultimate's host, whose output is always 1080p.
    pub mode: Option<TrxMode>,
}

/// S45: the MultiCore host's running mode as the picture needs it (hdmi_timing.v `hact`, `vact`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrxMode {
    /// The active area: the size of the output picture.
    pub hact: usize,
    pub vact: usize,
    /// vic_scaler.v `en`: the row is a 1080p one, the only rows the C64 is drawn on.
    pub c64: bool,
}

/// S37 phase B: the freeze menu's text as c64_text.v draws it: 40 × 25 cells of 8 × 8, each C64 pixel ST × ST output
/// pixels, centred in the active area (4 × 4 from output (320, 140) at 1080p; S45 §5 for the other modes), over
/// everything else.
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

/// S37, S45 §6: what the pixel layer shows in one frame (pix_layer.v, the `f_*` values of its frame start).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrxLayer {
    /// Output pixel of the top left in the active area; inside it.
    pub x0: usize,
    pub y0: usize,
    /// Source pixels, w ≤ 960, h ≤ 1080, both at least 1.
    pub w: usize,
    pub h: usize,
    /// Output pixels the picture covers (PX_OSIZE; TRX64's host: S · w by S · h), each 1-2047, with w ≤ 2 · ow and
    /// h ≤ 2 · oh in use. Output pixel x shows source pixel ⌊x · w / ow⌋, line y source line ⌊y · h / oh⌋.
    pub ow: usize,
    pub oh: usize,
    /// Bilinear smoothing per axis (PX_CTRL b6 where the source is not larger than the output, S45 §6).
    pub smooth_h: bool,
    pub smooth_v: bool,
    /// Blend factor 0-256: the global alpha G plus G bit 7.
    pub alpha: u16,
    /// RGB565 colour key, when on (format 1 only).
    pub key: Option<u16>,
    /// Format 2 (PX_CTRL b3:1 = 2): `pixels` are 0x00RRGGBB; format 1: RGB565 in the low 16 bits.
    pub wide: bool,
    /// `w × h` pixels, row by row.
    pub pixels: Vec<u32>,
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
