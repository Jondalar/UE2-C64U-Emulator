//! Types crossing the host/machine boundary. Additive changes only (report them).

use crate::c64host::C64Frame;

/// Host → machine input events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostInput {
    /// C64 keyboard matrix position; convention documented on `devices::u64io::U64Io::set_key`.
    Key { row: u8, col: u8, down: bool },
    /// Joystick lines, active-low mask as read at 0x10100406 (idle 0xFF).
    Joystick(u8),
    /// ITU menu button (0x1000000A bit 6).
    MenuButton(bool),
    /// Key on the USB keyboard (`MachineConfig::usb`): HID usage ID of the Keyboard/Keypad page 0x07, modifiers
    /// 0xE0-0xE7 included (docs/hw/09-usb.md §F7). Dropped when no USB keyboard is attached.
    UsbKey { usage: u8, down: bool },
    /// Plug the device on USB hub port `port` (1-based) in or out (`devices::usb::Usb::set_connected`). A plug-in
    /// waits until the firmware has handled the previous unplug. Dropped for an empty port.
    UsbPlug { port: u8, connected: bool },
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
    /// Emulated time of the snapshot.
    pub now_ms: u64,
    /// C64 frame under the overlay; None without an attached C64 (docs/specs/S14-c64-trx64.md §9).
    pub c64: Option<C64Frame>,
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
