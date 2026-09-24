//! USB2513 hub, the root device behind the nano's ULPI port (docs/hw/09-usb.md T1b, §F8). The firmware configures
//! it over SMBus with VID 0x0424, PID 0x2513, DID 0x0BA0 and no strings (usb_hwinit.cc:71-74); ports are
//! individually powered and hold the configured devices.

use super::device::{Device, Function, Peripheral, Reply, Setup, Speed};

/// Downstream ports of the USB2513 (bNbrPorts). `UsbHubDriver` handles up to 7 (usb_hub.h `children[7]`).
pub const PORTS: usize = 3;

/// Device descriptor: USB 2.0, hub class 09, protocol 01 (single TT; usb_hub.cc:63-82), EP0 64 bytes.
const DEVICE: [u8; 18] = [
    0x12, 0x01, 0x00, 0x02, 0x09, 0x00, 0x01, 0x40, 0x24, 0x04, 0x13, 0x25, 0xA0, 0x0B, 0x00, 0x00, 0x00, 0x01,
];

/// Configuration 1: self-powered, one hub interface with the status-change endpoint 0x81 (interrupt, 1 byte for
/// 3 ports, bInterval 12 = 256 ms at high speed).
const CONFIGURATION: [u8; 25] = [
    0x09, 0x02, 25, 0x00, 0x01, 0x01, 0x00, 0xE0, 0x01, // configuration
    0x09, 0x04, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x00, // interface
    0x07, 0x05, 0x81, 0x03, 0x01, 0x00, 0x0C, // endpoint
];

/// Hub descriptor 0x29 (usb_hub.cc:127-136): ports, characteristics 0x0009 (individual power switching and
/// over-current), bPwrOn2PwrGood 0x32 (100 ms, the PWRT byte of the SMBus table), bHubContrCurrent 1 mA,
/// no non-removable devices, port power mask.
const HUB_DESCRIPTOR: [u8; 9] = [0x09, 0x29, PORTS as u8, 0x09, 0x00, 0x32, 0x01, 0x00, 0xFF];

/// Status-change endpoint number.
const EP_STATUS: u8 = 1;

// Requests (usb_hub.cc:29-35).
const GET_STATUS: u8 = 0x00;
const CLEAR_FEATURE: u8 = 0x01;
const SET_FEATURE: u8 = 0x03;
const GET_DESCRIPTOR: u8 = 0x06;
const DESCR_HUB: u16 = 0x29;

// Port feature selectors (usb_hub.cc:84-97).
const PORT_ENABLE: u16 = 0x01;
const PORT_RESET: u16 = 0x04;
const PORT_POWER: u16 = 0x08;
const C_PORT_CONNECTION: u16 = 0x10;

// wPortStatus bits (usb_hub.cc:101-111).
const STATUS_CONNECTION: u16 = 0x0001;
const STATUS_ENABLE: u16 = 0x0002;
const STATUS_POWER: u16 = 0x0100;
const STATUS_HIGH_SPEED: u16 = 0x0400;
// wPortChange bits: C_PORT_* selectors 0x10-0x14 map to bits 0-4.
const CHANGE_CONNECTION: u16 = 0x01;
const CHANGE_RESET: u16 = 0x10;

pub(crate) struct Port {
    device: Option<Device>,
    /// The device's plug is in: an unplugged device stays in the port, invisible to the host, until it is plugged
    /// back in (hot-plug, docs/status/usb-dir.md).
    connected: bool,
    powered: bool,
    enabled: bool,
    change: u16,
}

impl Port {
    fn status(&self) -> [u8; 4] {
        let mut status = 0;
        if self.powered {
            status |= STATUS_POWER;
            if let Some(device) = self.device.as_ref().filter(|_| self.connected) {
                status |= STATUS_CONNECTION;
                if device.speed() == Speed::High {
                    status |= STATUS_HIGH_SPEED;
                }
            }
        }
        if self.enabled {
            status |= STATUS_ENABLE;
        }
        let [s0, s1] = status.to_le_bytes();
        let [c0, c1] = self.change.to_le_bytes();
        [s0, s1, c0, c1]
    }

    fn set_feature(&mut self, feature: u16) -> bool {
        match feature {
            PORT_POWER => {
                if !self.powered && self.device.is_some() && self.connected {
                    self.change |= CHANGE_CONNECTION;
                }
                self.powered = true;
            }
            // The reset finishes at once: the device is in its default state, the port enabled (09 §F8 "Hub
            // model duties").
            PORT_RESET if self.powered => {
                let connected = self.connected;
                if let Some(device) = self.device.as_mut().filter(|_| connected) {
                    device.reset();
                    self.enabled = true;
                }
                self.change |= CHANGE_RESET;
            }
            _ => return false,
        }
        true
    }

    fn clear_feature(&mut self, feature: u16) -> bool {
        match feature {
            PORT_ENABLE => self.enabled = false,
            PORT_POWER => {
                self.powered = false;
                self.enabled = false;
            }
            C_PORT_CONNECTION..=0x14 => self.change &= !(1 << (feature - C_PORT_CONNECTION)),
            _ => return false,
        }
        true
    }
}

pub(crate) struct Hub {
    ports: Vec<Port>,
}

impl Hub {
    /// A hub with `devices` on ports 1.. in order (at most [`PORTS`]).
    #[cfg(test)]
    pub(crate) fn new(devices: Vec<Device>) -> Self {
        Hub::with_ports(devices.into_iter().map(Some).collect())
    }

    /// A hub with `ports[i]` on port i + 1 (at most [`PORTS`]); `None` leaves a port empty.
    pub(crate) fn with_ports(ports: Vec<Option<Device>>) -> Self {
        assert!(ports.len() <= PORTS, "{} ports for a {PORTS}-port hub", ports.len());
        let mut ports = ports.into_iter();
        let ports = (0..PORTS)
            .map(|_| Port { device: ports.next().flatten(), connected: true, powered: false, enabled: false, change: 0 })
            .collect();
        Hub { ports }
    }

    pub(crate) fn devices(&mut self) -> impl Iterator<Item = &mut Device> {
        self.ports.iter_mut().filter_map(|port| port.device.as_mut())
    }

    /// Devices downstream of an enabled port, the only ones that see traffic.
    pub(crate) fn enabled_devices(&mut self) -> impl Iterator<Item = &mut Device> {
        self.ports.iter_mut().filter(|port| port.enabled && port.connected).filter_map(|port| port.device.as_mut())
    }

    fn port(&mut self, index: u16) -> Option<&mut Port> {
        self.ports.get_mut(usize::from(index).checked_sub(1)?)
    }

    /// The device on port `index` (1-based), plugged in or not.
    pub(crate) fn port_device(&mut self, index: usize) -> Option<&mut Device> {
        self.ports.get_mut(index.checked_sub(1)?)?.device.as_mut()
    }

    /// The device class on port `index` (1-based), plugged in or not.
    pub(crate) fn port_peripheral(&self, index: usize) -> Option<&Peripheral> {
        Some(&self.ports.get(index.checked_sub(1)?)?.device.as_ref()?.function)
    }

    /// Take the device off port `index` (1-based), which is then empty. Unplug it first, so the driver sees it go.
    pub(crate) fn take(&mut self, index: usize) -> Option<Device> {
        self.ports.get_mut(index.checked_sub(1)?)?.device.take()
    }

    /// Put `device` on the empty port `index` (1-based), not plugged in yet: no connection change until it is.
    pub(crate) fn insert_unplugged(&mut self, index: usize, device: Device) -> Result<(), String> {
        let port = self.ports.get_mut(index.wrapping_sub(1)).ok_or_else(|| format!("the hub has no port {index}"))?;
        if port.device.is_some() {
            return Err(format!("hub port {index} is in use"));
        }
        port.device = Some(device);
        port.connected = false;
        Ok(())
    }

    /// Put `device` on the empty port `index` (1-based), plugged in.
    pub(crate) fn insert(&mut self, index: usize, device: Device) -> Result<(), String> {
        let port = self.ports.get_mut(index.wrapping_sub(1)).ok_or_else(|| format!("the hub has no port {index}"))?;
        if port.device.is_some() {
            return Err(format!("hub port {index} is in use"));
        }
        port.device = Some(device);
        port.connected = true;
        if port.powered {
            port.change |= CHANGE_CONNECTION;
        }
        Ok(())
    }

    /// Plug the device of port `index` (1-based) in or out. A powered port reports the change on the status
    /// endpoint (C_PORT_CONNECTION), and the driver installs the device or queues its removal
    /// (usb_hub.cc:278-316). Unplugging disables the port. Returns false for a missing port or no change.
    pub(crate) fn set_connected(&mut self, index: usize, connected: bool) -> bool {
        let Some(port) = self.ports.get_mut(index.wrapping_sub(1)) else { return false };
        if port.connected == connected {
            return false;
        }
        port.connected = connected;
        if !connected {
            port.enabled = false;
        }
        if port.powered && port.device.is_some() {
            port.change |= CHANGE_CONNECTION;
        }
        true
    }

    /// (connected, enabled, connection change acknowledged) of port `index` (1-based). The driver acknowledges a
    /// connection change by clearing C_PORT_CONNECTION (usb_hub.cc:280-284, 309-313); an unpowered port has nothing
    /// to acknowledge, since the driver reads the state when it powers the port.
    pub(crate) fn port_state(&self, index: usize) -> Option<(bool, bool, bool)> {
        let port = self.ports.get(index.checked_sub(1)?)?;
        Some((port.connected, port.enabled, !port.powered || port.change & CHANGE_CONNECTION == 0))
    }
}

impl Function for Hub {
    fn speed(&self) -> Speed {
        Speed::High
    }

    fn device_descriptor(&self) -> &'static [u8] {
        &DEVICE
    }

    fn configuration(&self) -> &'static [u8] {
        &CONFIGURATION
    }

    fn strings(&self) -> &'static [&'static str] {
        &[]
    }

    fn control(&mut self, setup: &Setup, _data: &[u8], _now: u64) -> Option<Vec<u8>> {
        match (setup.request_type, setup.request) {
            (0xA0, GET_DESCRIPTOR) if setup.value >> 8 == DESCR_HUB => Some(HUB_DESCRIPTOR.to_vec()),
            (0xA0, GET_STATUS) => Some(vec![0; 4]),
            (0xA3, GET_STATUS) => self.port(setup.index).map(|port| port.status().to_vec()),
            (0x23, SET_FEATURE) => self.port(setup.index)?.set_feature(setup.value).then(Vec::new),
            (0x23, CLEAR_FEATURE) => self.port(setup.index)?.clear_feature(setup.value).then(Vec::new),
            _ => None,
        }
    }

    /// Status-change bitmap, bit n for port n; NAK while nothing changed, since the driver re-arms the pipe only for
    /// a non-zero bitmap (09 H13). One byte covers 3 ports, within the driver's 4-byte assert (09 H12).
    fn input(&mut self, ep: u8, buf: &mut [u8], _now: u64) -> Reply {
        if ep != EP_STATUS || buf.is_empty() {
            return Reply::Stall;
        }
        let bitmap = self.ports.iter().enumerate().filter(|(_, port)| port.change != 0).fold(0, |b, (i, _)| b | 2 << i);
        if bitmap == 0 {
            return Reply::Nak;
        }
        buf[0] = bitmap;
        Reply::Data(1)
    }

    fn output(&mut self, _ep: u8, _data: &[u8]) -> Reply {
        Reply::Stall
    }

    /// Hub reset: every port unpowered and disabled, the devices reset.
    fn reset(&mut self) {
        for port in &mut self.ports {
            port.powered = false;
            port.enabled = false;
            port.change = 0;
            if let Some(device) = &mut port.device {
                device.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::usb::device::Peripheral;
    use crate::devices::usb::keyboard::Keyboard;

    fn request(hub: &mut Hub, request_type: u8, request: u8, value: u16, index: u16) -> Option<Vec<u8>> {
        let setup = Setup { request_type, request, value, index, length: 4 };
        hub.control(&setup, &[], 0)
    }

    #[test]
    fn port_power_reports_the_connection_and_reset_enables_the_device() {
        let mut hub = Hub::new(vec![Device::new(Peripheral::Keyboard(Keyboard::new()))]);
        let mut buf = [0; 64];
        assert_eq!(request(&mut hub, 0xA0, GET_DESCRIPTOR, 0x2900, 0).unwrap()[2], 3);
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 1), Some(vec![0, 0, 0, 0]), "unpowered");
        assert_eq!(hub.input(EP_STATUS, &mut buf, 0), Reply::Nak);

        for port in 1..=3 {
            assert_eq!(request(&mut hub, 0x23, SET_FEATURE, PORT_POWER, port), Some(vec![]));
        }
        assert_eq!(hub.input(EP_STATUS, &mut buf, 0), Reply::Data(1));
        assert_eq!(buf[0], 0x02, "port 1 changed");
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 1), Some(vec![0x01, 0x01, 0x01, 0x00]));
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 2), Some(vec![0x00, 0x01, 0x00, 0x00]), "empty port");
        assert!(hub.enabled_devices().next().is_none());

        request(&mut hub, 0x23, CLEAR_FEATURE, C_PORT_CONNECTION, 1).unwrap();
        assert_eq!(hub.input(EP_STATUS, &mut buf, 0), Reply::Nak);
        request(&mut hub, 0x23, SET_FEATURE, PORT_RESET, 1).unwrap();
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 1), Some(vec![0x03, 0x01, 0x10, 0x00]), "enabled, full speed");
        assert_eq!(hub.enabled_devices().count(), 1);
        request(&mut hub, 0x23, CLEAR_FEATURE, 0x14, 1).unwrap();
        assert_eq!(hub.input(EP_STATUS, &mut buf, 0), Reply::Nak);

        assert_eq!(request(&mut hub, 0x23, SET_FEATURE, PORT_POWER, 4), None, "no port 4");
        assert_eq!(request(&mut hub, 0x23, SET_FEATURE, 0x02, 1), None, "suspend is not modelled");
        hub.reset();
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 1), Some(vec![0, 0, 0, 0]));
    }

    #[test]
    fn unplug_and_plug_report_connection_changes() {
        let mut hub = Hub::with_ports(vec![Some(Device::new(Peripheral::Keyboard(Keyboard::new()))), None]);
        let mut buf = [0; 64];
        assert!(hub.set_connected(1, false) && !hub.set_connected(1, false), "no change the second time");
        assert_eq!(hub.port_state(1), Some((false, false, true)), "unpowered: nothing to acknowledge");
        request(&mut hub, 0x23, SET_FEATURE, PORT_POWER, 1).unwrap();
        assert_eq!(hub.input(EP_STATUS, &mut buf, 0), Reply::Nak, "unplugged at power-on: no connection");
        assert!(hub.set_connected(1, true));
        assert_eq!((hub.input(EP_STATUS, &mut buf, 0), buf[0]), (Reply::Data(1), 0x02));
        request(&mut hub, 0x23, CLEAR_FEATURE, C_PORT_CONNECTION, 1).unwrap();
        request(&mut hub, 0x23, SET_FEATURE, PORT_RESET, 1).unwrap();
        request(&mut hub, 0x23, CLEAR_FEATURE, 0x14, 1).unwrap();
        assert_eq!(hub.port_state(1), Some((true, true, true)));
        assert_eq!(hub.enabled_devices().count(), 1);

        assert!(hub.set_connected(1, false));
        assert_eq!(hub.port_state(1), Some((false, false, false)), "the driver has not seen the disconnect yet");
        assert_eq!((hub.input(EP_STATUS, &mut buf, 0), buf[0]), (Reply::Data(1), 0x02));
        assert_eq!(request(&mut hub, 0xA3, GET_STATUS, 0, 1), Some(vec![0x00, 0x01, 0x01, 0x00]), "disconnect change");
        assert_eq!(hub.enabled_devices().count(), 0, "no traffic to an unplugged device");
        request(&mut hub, 0x23, CLEAR_FEATURE, C_PORT_CONNECTION, 1).unwrap();
        assert_eq!(hub.port_state(1), Some((false, false, true)));
        request(&mut hub, 0x23, SET_FEATURE, PORT_RESET, 1).unwrap();
        assert_eq!(hub.enabled_devices().count(), 0, "a reset does not enable an unplugged port");

        assert!(hub.insert(1, Device::new(Peripheral::Keyboard(Keyboard::new()))).is_err(), "port 1 is in use");
        assert!(hub.insert(4, Device::new(Peripheral::Keyboard(Keyboard::new()))).is_err(), "no port 4");
        hub.insert(2, Device::new(Peripheral::Keyboard(Keyboard::new()))).unwrap();
        assert_eq!(hub.port_state(2), Some((true, false, true)), "port 2 is not powered yet");
    }
}
