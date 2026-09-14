//! USB HID boot keyboard fed by host key events (docs/hw/09-usb.md §F7, T1 step 10).
//!
//! The firmware reads the report descriptor, finds the keyboard application with a key array and modifier bits,
//! selects report protocol and turns each 8-byte report into a boot report for `Keyboard_USB` (usb_hid.cc:895-927,
//! 1190-1198). With this descriptor both protocols use the same layout: modifiers, reserved, six key usages.

use std::collections::VecDeque;

use super::device::{Function, Reply, Setup, Speed};
use crate::time;

/// Device descriptor: USB 2.0, class in the interface, EP0 64 bytes, VID 0x1209 (pid.codes) PID 0x0002 (test
/// range), manufacturer and product strings.
const DEVICE: [u8; 18] = [
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x09, 0x12, 0x02, 0x00, 0x00, 0x01, 0x01, 0x02, 0x00, 0x01,
];

/// Configuration 1: one HID interface, subclass 1 (boot) protocol 1 (keyboard), HID descriptor with the report
/// descriptor length, interrupt IN endpoint 0x81 of 8 bytes every 10 ms (full speed).
const CONFIGURATION: [u8; 34] = [
    0x09, 0x02, 34, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, // configuration
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, // interface
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, REPORT_DESCRIPTOR.len() as u8, 0x00, // HID 1.11
    0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0A, // endpoint
];

/// The boot keyboard report descriptor of HID 1.11 Appendix E.6: 8 modifier bits, a reserved byte, 5 LED bits
/// out, 6 key array bytes (usages 0-101).
const REPORT_DESCRIPTOR: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01,
    0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
    0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

const STRINGS: &[&str] = &["UE2EMU", "USB Keyboard"];

/// Interrupt IN endpoint number.
const EP_IN: u8 = 1;

// HID class requests (usb_hid.cc:900-901, 998-1001) and the report descriptor type (usb_device.cc:58).
const GET_DESCRIPTOR: u8 = 0x06;
const DESCR_REPORT: u16 = 0x22;
const GET_IDLE: u8 = 0x02;
const SET_IDLE: u8 = 0x0A;
const SET_PROTOCOL: u8 = 0x0B;

/// Modifier usages Left Control .. Right GUI map to report byte 0 bits 0-7.
const MODIFIERS: std::ops::RangeInclusive<u8> = 0xE0..=0xE7;
/// Reports kept while the host polls slower than keys change (the driver polls every 20 ms, usb_hid.cc:969), so a
/// short scripted tap is not lost.
const QUEUE_LIMIT: usize = 64;
/// One SET_IDLE unit (usb_hid_selection.h:74-76).
const IDLE_UNIT: u64 = 4 * time::CLOCKS_PER_MS;
/// Default idle rate of a boot keyboard, 500 ms (HID 1.11 §7.2.4).
const DEFAULT_IDLE: u8 = 125;

pub(crate) struct Keyboard {
    /// Current state: modifiers, reserved, up to six pressed usages in press order.
    report: [u8; 8],
    /// State changes the host has not read yet.
    queue: VecDeque<[u8; 8]>,
    /// SET_IDLE duration in 4 ms units; 0 = report only on change.
    idle: u8,
    /// Clock of the last report sent.
    last_sent: u64,
}

impl Keyboard {
    pub(crate) fn new() -> Self {
        Keyboard { report: [0; 8], queue: VecDeque::new(), idle: DEFAULT_IDLE, last_sent: 0 }
    }

    /// Press or release the key with HID usage `usage` (page 0x07). A seventh simultaneous key is ignored.
    pub(crate) fn key(&mut self, usage: u8, down: bool) {
        let mut report = self.report;
        if MODIFIERS.contains(&usage) {
            let bit = 1 << (usage - MODIFIERS.start());
            if down {
                report[0] |= bit;
            } else {
                report[0] &= !bit;
            }
        } else if usage != 0 {
            let keys = &mut report[2..];
            let held = keys.iter().position(|&k| k == usage);
            match (down, held) {
                (true, None) => {
                    if let Some(slot) = keys.iter_mut().find(|k| **k == 0) {
                        *slot = usage;
                    }
                }
                (false, Some(i)) => {
                    keys.copy_within(i + 1.., i);
                    keys[5] = 0;
                }
                _ => {}
            }
        }
        if report != self.report {
            self.report = report;
            if self.queue.len() == QUEUE_LIMIT {
                self.queue.pop_front();
            }
            self.queue.push_back(report);
        }
    }
}

impl Function for Keyboard {
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
            // usb_hid_idle_rate_accepted wants the stored duration back (usb_hid_selection.h:84-92).
            (0xA1, GET_IDLE) => Some(vec![self.idle]),
            // Boot and report protocol share the report layout.
            (0x21, SET_PROTOCOL) => Some(Vec::new()),
            _ => None,
        }
    }

    /// The oldest unread state change; the current state again once the idle duration has passed; NAK otherwise
    /// (09 §F7 "Device side").
    fn input(&mut self, ep: u8, buf: &mut [u8], now: u64) -> Reply {
        if ep != EP_IN {
            return Reply::Stall;
        }
        let report = match self.queue.pop_front() {
            Some(report) => report,
            None if self.idle != 0 && now >= self.last_sent + u64::from(self.idle) * IDLE_UNIT => self.report,
            None => return Reply::Nak,
        };
        let n = buf.len().min(report.len());
        buf[..n].copy_from_slice(&report[..n]);
        self.last_sent = now;
        Reply::Data(n)
    }

    fn output(&mut self, _ep: u8, _data: &[u8]) -> Reply {
        Reply::Stall
    }

    /// Keys stay held across a reset; unread changes and the idle rate do not.
    fn reset(&mut self) {
        self.queue.clear();
        self.idle = DEFAULT_IDLE;
        self.last_sent = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(kb: &mut Keyboard, now_ms: u64) -> Option<[u8; 8]> {
        let mut buf = [0; 64];
        match kb.input(EP_IN, &mut buf, now_ms * time::CLOCKS_PER_MS) {
            Reply::Data(8) => Some(buf[..8].try_into().unwrap()),
            Reply::Nak => None,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_follow_key_changes_in_order() {
        let mut kb = Keyboard::new();
        kb.idle = 0;
        assert_eq!(read(&mut kb, 0), None, "NAK without a change");
        kb.key(0xE1, true); // left shift
        kb.key(0x04, true); // a
        kb.key(0x05, true); // b
        kb.key(0x04, true); // repeated press: no change
        kb.key(0x04, false);
        kb.key(0xE1, false);
        assert_eq!(read(&mut kb, 1), Some([0x02, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(read(&mut kb, 2), Some([0x02, 0, 0x04, 0, 0, 0, 0, 0]));
        assert_eq!(read(&mut kb, 3), Some([0x02, 0, 0x04, 0x05, 0, 0, 0, 0]));
        assert_eq!(read(&mut kb, 4), Some([0x02, 0, 0x05, 0, 0, 0, 0, 0]), "release closes the gap");
        assert_eq!(read(&mut kb, 5), Some([0x00, 0, 0x05, 0, 0, 0, 0, 0]));
        assert_eq!(read(&mut kb, 6), None);
    }

    #[test]
    fn a_seventh_key_is_ignored() {
        let mut kb = Keyboard::new();
        for usage in 0x04..0x0B {
            kb.key(usage, true);
        }
        assert_eq!(kb.report, [0, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
        assert_eq!(kb.queue.len(), 6);
    }

    #[test]
    fn idle_rate_repeats_the_current_report() {
        let mut kb = Keyboard::new();
        let set_idle = Setup { request_type: 0x21, request: SET_IDLE, value: 25 << 8, index: 0, length: 0 };
        assert_eq!(kb.control(&set_idle, &[], 0), Some(vec![]));
        let get_idle = Setup { request_type: 0xA1, request: GET_IDLE, value: 0, index: 0, length: 1 };
        assert_eq!(kb.control(&get_idle, &[], 0), Some(vec![25]));
        kb.key(0x28, true);
        assert_eq!(read(&mut kb, 0), Some([0, 0, 0x28, 0, 0, 0, 0, 0]));
        assert_eq!(read(&mut kb, 99), None, "within 100 ms");
        assert_eq!(read(&mut kb, 100), Some([0, 0, 0x28, 0, 0, 0, 0, 0]));
        kb.reset();
        assert_eq!(kb.idle, DEFAULT_IDLE);
    }

    #[test]
    fn descriptors_are_consistent() {
        assert_eq!(usize::from(CONFIGURATION[2]), CONFIGURATION.len());
        assert_eq!(usize::from(CONFIGURATION[25]), REPORT_DESCRIPTOR.len());
        let setup = Setup { request_type: 0x81, request: GET_DESCRIPTOR, value: 0x2200, index: 0, length: 63 };
        assert_eq!(Keyboard::new().control(&setup, &[], 0).unwrap(), REPORT_DESCRIPTOR);
    }
}
