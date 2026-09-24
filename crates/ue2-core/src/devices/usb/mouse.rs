//! USB HID mouse fed by host mouse events (S32).
//!
//! The firmware takes any HID interface whose report descriptor has relative Generic Desktop X and Y under a Mouse
//! application, with optional buttons 1-3 and a relative wheel (hid_decoder.h:909-928), selects report protocol and
//! turns each report into the mouse position it writes to C64_PADDLE_1_X/Y and the buttons it drives on joystick port
//! 1 (usb_hid.cc:1216-1483). The firmware sets an idle rate of 0 (usb_hid_selection.h:79-81): a report only on change.

use super::device::{Function, Reply, Setup, Speed};

/// Device descriptor: USB 2.0, class in the interface, EP0 64 bytes, VID 0x1209 (pid.codes) PID 0x0003 (test
/// range), manufacturer and product strings.
const DEVICE: [u8; 18] = [
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x09, 0x12, 0x03, 0x00, 0x00, 0x01, 0x01, 0x02, 0x00, 0x01,
];

/// Configuration 1: one HID interface, subclass 1 (boot) protocol 2 (mouse), HID descriptor with the report
/// descriptor length, interrupt IN endpoint 0x81 of 4 bytes every 10 ms (full speed).
const CONFIGURATION: [u8; 34] = [
    0x09, 0x02, 34, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, // configuration
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, // interface
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, REPORT_DESCRIPTOR.len() as u8, 0x00, // HID 1.11
    0x07, 0x05, 0x81, 0x03, 0x04, 0x00, 0x0A, // endpoint
];

/// The boot mouse of HID 1.11 Appendix E.10 with a relative wheel after Y: three buttons and five bits of padding,
/// then X, Y and wheel as signed bytes. The first three bytes are the boot report.
const REPORT_DESCRIPTOR: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
    0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
    0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0,
];

const STRINGS: &[&str] = &["UE2EMU", "USB Mouse"];

/// Interrupt IN endpoint number.
const EP_IN: u8 = 1;

// HID class requests (usb_hid.cc:989-995) and the report descriptor type (usb_device.cc:58).
const GET_DESCRIPTOR: u8 = 0x06;
const DESCR_REPORT: u16 = 0x22;
const GET_IDLE: u8 = 0x02;
const SET_IDLE: u8 = 0x0A;
const SET_PROTOCOL: u8 = 0x0B;

/// Button bits of the report: left, right, middle.
pub const BUTTON_LEFT: u8 = 0x01;
pub const BUTTON_RIGHT: u8 = 0x02;
pub const BUTTON_MIDDLE: u8 = 0x04;

pub(crate) struct Mouse {
    buttons: u8,
    /// Motion and wheel the host has not read yet; a report carries at most ±127 of each.
    dx: i32,
    dy: i32,
    wheel: i32,
    /// The buttons changed since the last report.
    changed: bool,
    idle: u8,
}

impl Mouse {
    pub(crate) fn new() -> Self {
        Mouse { buttons: 0, dx: 0, dy: 0, wheel: 0, changed: false, idle: 0 }
    }

    /// Move by `dx`, `dy` (right and down positive), turn the wheel by `wheel` (away from the user positive), with
    /// `buttons` held ([`BUTTON_LEFT`] ...).
    pub(crate) fn update(&mut self, dx: i32, dy: i32, wheel: i32, buttons: u8) {
        self.dx = self.dx.saturating_add(dx);
        self.dy = self.dy.saturating_add(dy);
        self.wheel = self.wheel.saturating_add(wheel);
        let buttons = buttons & 0x07;
        if buttons != self.buttons {
            self.buttons = buttons;
            self.changed = true;
        }
    }

    fn pending(&self) -> bool {
        self.changed || self.dx != 0 || self.dy != 0 || self.wheel != 0
    }
}

/// Up to ±127 of `acc`, taken out of it.
fn take(acc: &mut i32) -> i8 {
    let n = (*acc).clamp(-127, 127);
    *acc -= n;
    n as i8
}

impl Function for Mouse {
    fn speed(&self) -> Speed {
        Speed::Full
    }

    fn device_descriptor(&self) -> &'static [u8] {
        &DEVICE
    }

    fn configuration(&self) -> &'static [u8] {
        &CONFIGURATION
    }

    fn strings(&self) -> &'static [&'static str] {
        STRINGS
    }

    fn control(&mut self, setup: &Setup, _data: &[u8], _now: u64) -> Option<Vec<u8>> {
        match (setup.request_type, setup.request) {
            (0x81, GET_DESCRIPTOR) if setup.value >> 8 == DESCR_REPORT => Some(REPORT_DESCRIPTOR.to_vec()),
            (0x21, SET_IDLE) => {
                self.idle = (setup.value >> 8) as u8;
                Some(Vec::new())
            }
            (0xA1, GET_IDLE) => Some(vec![self.idle]),
            // The boot report is the first three bytes of the report-protocol one.
            (0x21, SET_PROTOCOL) => Some(Vec::new()),
            _ => None,
        }
    }

    /// A report when the buttons changed or motion is waiting; NAK otherwise.
    fn input(&mut self, ep: u8, buf: &mut [u8], _now: u64) -> Reply {
        if ep != EP_IN {
            return Reply::Stall;
        }
        if !self.pending() {
            return Reply::Nak;
        }
        self.changed = false;
        let report = [self.buttons, take(&mut self.dx) as u8, take(&mut self.dy) as u8, take(&mut self.wheel) as u8];
        let n = buf.len().min(report.len());
        buf[..n].copy_from_slice(&report[..n]);
        Reply::Data(n)
    }

    fn output(&mut self, _ep: u8, _data: &[u8]) -> Reply {
        Reply::Stall
    }

    /// Buttons stay held across a reset; unread motion does not.
    fn reset(&mut self) {
        (self.dx, self.dy, self.wheel, self.changed, self.idle) = (0, 0, 0, self.buttons != 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(m: &mut Mouse) -> Option<[u8; 4]> {
        let mut buf = [0u8; 4];
        match m.input(EP_IN, &mut buf, 0) {
            Reply::Data(4) => Some(buf),
            Reply::Nak => None,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_carry_motion_in_steps_of_at_most_127() {
        let mut m = Mouse::new();
        assert_eq!(read(&mut m), None, "nothing to report");
        m.update(200, -3, 0, 0);
        assert_eq!(read(&mut m), Some([0, 127, (-3i8) as u8, 0]));
        assert_eq!(read(&mut m), Some([0, 73, 0, 0]), "the rest of the motion");
        assert_eq!(read(&mut m), None);
        m.update(0, 0, 1, BUTTON_LEFT);
        assert_eq!(read(&mut m), Some([BUTTON_LEFT, 0, 0, 1]));
        m.update(0, 0, 0, BUTTON_LEFT);
        assert_eq!(read(&mut m), None, "held, not moved: no report");
        m.update(0, 0, 0, 0);
        assert_eq!(read(&mut m), Some([0, 0, 0, 0]), "the release is a report");
    }

    #[test]
    fn the_descriptor_is_a_relative_mouse() {
        // Generic Desktop / Mouse, then X Y Wheel as Data,Var,Rel (0x81 0x06).
        assert_eq!(&REPORT_DESCRIPTOR[..4], &[0x05, 0x01, 0x09, 0x02]);
        assert_eq!(&REPORT_DESCRIPTOR[34..40], &[0x09, 0x30, 0x09, 0x31, 0x09, 0x38]);
        assert_eq!(&REPORT_DESCRIPTOR[48..50], &[0x81, 0x06]);
        assert_eq!(CONFIGURATION[25], REPORT_DESCRIPTOR.len() as u8);
    }
}
