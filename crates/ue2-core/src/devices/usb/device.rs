//! Device side of the USB model: the transaction interface the pipe scheduler drives (docs/hw/09-usb.md T1
//! step 7), the standard control endpoint shared by every device, and the device tree behind the root port.

use super::hub::Hub;
use super::keyboard::Keyboard;
use super::mouse::Mouse;
use super::storage::Storage;

/// Link speed as the firmware numbers it: `UsbDevice::speed`, from the hub port status (usb_hub.cc:353-354).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Speed {
    Full,
    High,
}

/// A device's answer to one token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// SETUP or OUT accepted.
    Ack,
    /// IN data packet of this many bytes; 0 is a zero-length packet.
    Data(usize),
    Nak,
    Stall,
}

// Standard requests the firmware sends (usb_device.cc:43-54).
const GET_DESCRIPTOR: u8 = 0x06;
const SET_ADDRESS: u8 = 0x05;
const SET_CONFIGURATION: u8 = 0x09;
// Standard descriptor types (usb_device.h:12-14).
const DESCR_DEVICE: u8 = 0x01;
const DESCR_CONFIGURATION: u8 = 0x02;
const DESCR_STRING: u8 = 0x03;
/// String descriptor 0: LANGID US English, read by `get_language` (usb_device.cc:140-151).
const LANGUAGES: [u8; 4] = [0x04, DESCR_STRING, 0x09, 0x04];

/// The 8-byte SETUP packet of a control transfer (little-endian fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    fn parse(b: &[u8; 8]) -> Self {
        Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
}

/// What a device class adds to the standard control endpoint of [`Device`].
pub(crate) trait Function {
    fn speed(&self) -> Speed;
    /// The 18-byte device descriptor.
    fn device_descriptor(&self) -> &'static [u8];
    /// The whole configuration descriptor (`wTotalLength` bytes).
    fn configuration(&self) -> &'static [u8];
    /// Texts of string descriptors 1.. ; empty when the device has none, so string requests STALL.
    fn strings(&self) -> &'static [&'static str];
    /// A control request [`Device`] does not answer itself; `data` is the OUT data stage. Returns the IN data
    /// stage (empty for none), or `None` to STALL.
    fn control(&mut self, setup: &Setup, data: &[u8], now: u64) -> Option<Vec<u8>>;
    /// IN token on a non-control endpoint; at most `buf.len()` (the pipe's maxTrans) bytes go into `buf`.
    fn input(&mut self, ep: u8, buf: &mut [u8], now: u64) -> Reply;
    /// OUT data packet on a non-control endpoint.
    fn output(&mut self, ep: u8, data: &[u8]) -> Reply;
    /// USB reset (bus or port reset): back to the default state.
    fn reset(&mut self);
}

/// The device classes the model provides.
pub(crate) enum Peripheral {
    Hub(Hub),
    Storage(Storage),
    Keyboard(Keyboard),
    Mouse(Mouse),
}

impl Peripheral {
    fn get(&self) -> &dyn Function {
        match self {
            Peripheral::Hub(f) => f,
            Peripheral::Storage(f) => f,
            Peripheral::Keyboard(f) => f,
            Peripheral::Mouse(f) => f,
        }
    }

    fn get_mut(&mut self) -> &mut dyn Function {
        match self {
            Peripheral::Hub(f) => f,
            Peripheral::Storage(f) => f,
            Peripheral::Keyboard(f) => f,
            Peripheral::Mouse(f) => f,
        }
    }
}

/// Stage of the control transfer on endpoint 0.
enum Ep0 {
    Idle,
    /// IN data stage: the reply and the bytes already sent. The host's status OUT ends it.
    DataIn(Vec<u8>, usize),
    /// OUT data stage collecting `wLength` bytes; the host's status IN runs the request.
    DataOut(Setup, Vec<u8>),
    /// A no-data request was accepted; the host's status IN completes it and applies a new address.
    StatusIn(Option<u8>),
    /// Rejected request: the data and status stages STALL until the next SETUP.
    Stall,
}

/// A USB device: address, configuration and control endpoint, with its class behaviour in `function`.
pub(crate) struct Device {
    pub address: u8,
    configuration: u8,
    ep0: Ep0,
    pub function: Peripheral,
}

impl Device {
    pub(crate) fn new(function: Peripheral) -> Self {
        Device { address: 0, configuration: 0, ep0: Ep0::Idle, function }
    }

    pub(crate) fn speed(&self) -> Speed {
        self.function.get().speed()
    }

    /// USB reset: default address 0, unconfigured.
    pub(crate) fn reset(&mut self) {
        self.address = 0;
        self.configuration = 0;
        self.ep0 = Ep0::Idle;
        self.function.get_mut().reset();
    }

    /// The device answering `address`: this one, or one behind an enabled hub port.
    pub(crate) fn find(&mut self, address: u8) -> Option<&mut Device> {
        if self.address == address {
            return Some(self);
        }
        let Peripheral::Hub(hub) = &mut self.function else { return None };
        hub.enabled_devices().find_map(|dev| dev.find(address))
    }

    /// The keyboard in this subtree, reachable or not: keys queue up until the host polls.
    pub(crate) fn keyboard(&mut self) -> Option<&mut Keyboard> {
        match &mut self.function {
            Peripheral::Keyboard(keyboard) => Some(keyboard),
            Peripheral::Hub(hub) => hub.devices().find_map(Device::keyboard),
            _ => None,
        }
    }

    /// The mouse in this subtree, reachable or not: motion adds up until the host polls (S32).
    pub(crate) fn mouse(&mut self) -> Option<&mut Mouse> {
        match &mut self.function {
            Peripheral::Mouse(mouse) => Some(mouse),
            Peripheral::Hub(hub) => hub.devices().find_map(Device::mouse),
            _ => None,
        }
    }

    /// SETUP token with its data packet. A SETUP is always acknowledged and starts a new control transfer.
    pub(crate) fn setup(&mut self, packet: &[u8], now: u64) -> Reply {
        let Ok(bytes) = <&[u8; 8]>::try_from(packet) else { return Reply::Stall };
        let setup = Setup::parse(bytes);
        self.ep0 = if setup.request_type & 0x80 != 0 {
            match self.request(&setup, &[], now) {
                Some(mut reply) => {
                    reply.truncate(usize::from(setup.length));
                    Ep0::DataIn(reply, 0)
                }
                None => Ep0::Stall,
            }
        } else if setup.length > 0 {
            Ep0::DataOut(setup, Vec::new())
        } else {
            match self.request(&setup, &[], now) {
                // The new address applies after the status stage (USB 2.0 §9.4.6).
                Some(_) if setup.request == SET_ADDRESS => Ep0::StatusIn(Some((setup.value & 0x7F) as u8)),
                Some(_) => Ep0::StatusIn(None),
                None => Ep0::Stall,
            }
        };
        Reply::Ack
    }

    /// IN token on `ep`.
    pub(crate) fn input(&mut self, ep: u8, buf: &mut [u8], now: u64) -> Reply {
        if ep != 0 {
            return if self.configuration != 0 { self.function.get_mut().input(ep, buf, now) } else { Reply::Stall };
        }
        match std::mem::replace(&mut self.ep0, Ep0::Idle) {
            Ep0::DataIn(reply, sent) => {
                let n = buf.len().min(reply.len() - sent);
                buf[..n].copy_from_slice(&reply[sent..sent + n]);
                self.ep0 = Ep0::DataIn(reply, sent + n);
                Reply::Data(n)
            }
            Ep0::StatusIn(address) => {
                if let Some(address) = address {
                    self.address = address;
                }
                Reply::Data(0)
            }
            Ep0::DataOut(setup, data) => match self.request(&setup, &data, now) {
                Some(_) => Reply::Data(0),
                None => Reply::Stall,
            },
            Ep0::Idle | Ep0::Stall => {
                self.ep0 = Ep0::Stall;
                Reply::Stall
            }
        }
    }

    /// OUT data packet on `ep`.
    pub(crate) fn output(&mut self, ep: u8, data: &[u8]) -> Reply {
        if ep != 0 {
            return if self.configuration != 0 { self.function.get_mut().output(ep, data) } else { Reply::Stall };
        }
        match std::mem::replace(&mut self.ep0, Ep0::Idle) {
            Ep0::DataOut(setup, mut received) => {
                let room = usize::from(setup.length) - received.len();
                received.extend_from_slice(&data[..data.len().min(room)]);
                self.ep0 = Ep0::DataOut(setup, received);
                Reply::Ack
            }
            // Status stage of an IN request.
            Ep0::DataIn(..) => Reply::Ack,
            Ep0::Idle | Ep0::StatusIn(_) | Ep0::Stall => {
                self.ep0 = Ep0::Stall;
                Reply::Stall
            }
        }
    }

    /// Standard requests the firmware sends; everything else goes to the function.
    fn request(&mut self, setup: &Setup, data: &[u8], now: u64) -> Option<Vec<u8>> {
        match (setup.request_type, setup.request) {
            (0x80, GET_DESCRIPTOR) => self.descriptor(setup, now),
            (0x00, SET_ADDRESS) => Some(Vec::new()),
            (0x00, SET_CONFIGURATION) => {
                let value = setup.value as u8;
                let valid = value == 0 || value == self.function.get().configuration()[5];
                valid.then(|| {
                    self.configuration = value;
                    Vec::new()
                })
            }
            _ => self.function.get_mut().control(setup, data, now),
        }
    }

    fn descriptor(&mut self, setup: &Setup, now: u64) -> Option<Vec<u8>> {
        let function = self.function.get_mut();
        let index = usize::from(setup.value & 0xFF);
        match (setup.value >> 8) as u8 {
            DESCR_DEVICE => Some(function.device_descriptor().to_vec()),
            DESCR_CONFIGURATION if index == 0 => Some(function.configuration().to_vec()),
            DESCR_STRING if function.strings().is_empty() => None,
            DESCR_STRING if index == 0 => Some(LANGUAGES.to_vec()),
            DESCR_STRING => function.strings().get(index - 1).map(|text| string_descriptor(text)),
            _ => function.control(setup, &[], now),
        }
    }
}

/// String descriptor: length, type, UTF-16LE text (USB 2.0 §9.6.7).
fn string_descriptor(text: &str) -> Vec<u8> {
    let mut out = vec![0, DESCR_STRING];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out[0] = out.len() as u8;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::usb::keyboard::Keyboard;

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
        let [v0, v1] = value.to_le_bytes();
        let [i0, i1] = index.to_le_bytes();
        let [l0, l1] = length.to_le_bytes();
        [request_type, request, v0, v1, i0, i1, l0, l1]
    }

    /// IN data stage of up to `max`-byte packets until a short one, like the nano IN loop (nan:752-758).
    fn read_in(dev: &mut Device, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0; max];
        loop {
            match dev.input(0, &mut buf, 0) {
                Reply::Data(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if n < max {
                        return out;
                    }
                }
                other => panic!("IN stage: {other:?}"),
            }
        }
    }

    #[test]
    fn descriptors_are_truncated_to_wlength_and_strings_are_utf16() {
        let mut dev = Device::new(Peripheral::Keyboard(Keyboard::new()));
        assert_eq!(dev.setup(&setup(0x80, GET_DESCRIPTOR, 0x0100, 0, 18), 0), Reply::Ack);
        let descriptor = read_in(&mut dev, 64);
        assert_eq!((descriptor.len(), descriptor[1], descriptor[7]), (18, DESCR_DEVICE, 64));
        assert_eq!(dev.output(0, &[]), Reply::Ack, "status stage");

        dev.setup(&setup(0x80, GET_DESCRIPTOR, 0x0200, 0, 9), 0);
        let head = read_in(&mut dev, 64);
        assert_eq!((head.len(), head[1]), (9, DESCR_CONFIGURATION));

        dev.setup(&setup(0x80, GET_DESCRIPTOR, 0x0300, 0x0409, 4), 0);
        assert_eq!(read_in(&mut dev, 64), LANGUAGES);
        dev.setup(&setup(0x80, GET_DESCRIPTOR, 0x0302, 0x0409, 255), 0);
        assert_eq!(read_in(&mut dev, 8), string_descriptor("USB Keyboard"), "8-byte packets and a short last one");
        assert_eq!(string_descriptor("AB"), [6, 3, b'A', 0, b'B', 0]);
    }

    #[test]
    fn address_applies_after_the_status_stage() {
        let mut dev = Device::new(Peripheral::Keyboard(Keyboard::new()));
        dev.setup(&setup(0x00, SET_ADDRESS, 5, 0, 0), 0);
        assert_eq!(dev.address, 0);
        assert_eq!(dev.input(0, &mut [0; 64], 0), Reply::Data(0));
        assert_eq!(dev.address, 5);
        assert!(dev.find(5).is_some() && dev.find(0).is_none());
    }

    #[test]
    fn rejected_requests_stall_until_the_next_setup_and_endpoints_need_a_configuration() {
        let mut dev = Device::new(Peripheral::Keyboard(Keyboard::new()));
        let mut buf = [0; 64];
        assert_eq!(dev.input(1, &mut buf, 0), Reply::Stall, "unconfigured");
        dev.setup(&setup(0x00, SET_CONFIGURATION, 7, 0, 0), 0);
        assert_eq!(dev.input(0, &mut buf, 0), Reply::Stall, "no configuration 7");
        assert_eq!(dev.input(0, &mut buf, 0), Reply::Stall);
        dev.setup(&setup(0x00, SET_CONFIGURATION, 1, 0, 0), 0);
        assert_eq!(dev.input(0, &mut buf, 0), Reply::Data(0));
        let idle = 500 * crate::time::CLOCKS_PER_MS;
        assert_eq!(dev.input(1, &mut buf, idle), Reply::Data(8), "keyboard report after the default idle time");
        dev.reset();
        assert_eq!(dev.input(1, &mut buf, 0), Reply::Stall, "reset unconfigures");
    }
}
