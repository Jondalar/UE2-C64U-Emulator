//! USB host, window 0x10080000-0x10080FFF: a high-level model of the FPGA `usb_host_nano` core and the nano CPU
//! program the firmware loads, with a USB2513 hub as the root device and the configured devices on its ports.
//! Spec: docs/specs/S11-S14-later.md §S13. Registers and protocol: docs/hw/09-usb.md (tier T1).
//!
//! The CPU never talks USB itself. It shares 2 K of BRAM with the nano (pipe descriptors, attribute FIFO, status
//! words) and gets ITU interrupt bit 2 for every FIFO push (09 §Address map, §Interrupts). The model ignores the
//! nano code words and does to the shared RAM what `nano_minimal.nan` does (09 F1-F3):
//! - link: the hub attaches about 700 ms after NANO_START; NANO_DO_RESET runs a bus reset that ends high speed
//!   (09 H8, H9);
//! - pipe scheduler on the 8 kHz frame counter: interval, `memHi != 0`, `started`/`timeout`, ABORT_REQ, the DONE
//!   bit and FIFO back-pressure; a whole transfer completes in one scan (09 F3 "HLE simplification");
//! - DMA to guest RAM for MEMREAD/MEMWRITE pipes.
//!
//! Split transactions (`splitCtl`) and PING are not modelled: transfers to full-speed devices behind the hub run as
//! if they were high speed (09 F3 table).
//!
//! Until NANO_START the window is plain RAM plus a write-only run register and never raises the interrupt. That
//! is all the pre-scheduler hub init and a firmware without CAPAB_USB_HOST2 see (00 §2 B5, C31; 09 H1, H5).
//!
//! Hot-plug: a device can be unplugged and plugged back in on its hub port ([`Usb::set_connected`]). The hub
//! reports each connection change on its status endpoint, so the firmware removes and re-enumerates the device
//! (usb_hub.cc:278-316). A plug-in waits until the firmware has cleared C_PORT_CONNECTION of the unplug plus
//! [`REPLUG_GAP`]: a connect seen while the old child still exists is refused by the driver.

pub mod block;
mod device;
mod hub;
mod keyboard;
mod mouse;
mod storage;

use std::path::PathBuf;

use crate::bus::RAM_MASK;
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::irq::IrqState;
use crate::machine::MachineConfig;
use crate::time;
use block::BlockBackend;
use device::{Device, Peripheral, Reply};
use hub::Hub;
use keyboard::Keyboard;
use mouse::Mouse;
pub use mouse::{BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use storage::Storage;

pub use block::{ImageFile, BLOCK_SIZE};
pub use hub::PORTS as HUB_PORTS;

/// `USB_BASE` = IOBASE + 0x80000 (iomap.h:33, usb_nano.h:14-15).
pub const USB_BASE: u32 = 0x1008_0000;
const WINDOW: u32 = 0x1000;
/// `CAPAB_USB_HOST2` (itu.h:72): without it `UsbBase::initHardware` never starts the nano (usb_base.cc:190, 09 H3).
pub const CAPAB_USB_HOST2: u32 = 0x0080_0000;
/// ITU low interrupt of the nano's SEND_INTERRUPT pulse, an edge (ultimate_logic_32.vhd:536; 09 H15).
const IRQ_BIT: u8 = 2;

/// The dual-port BRAM as the CPU sees it (nano.vhd:98-111). Offsets 0x800-0xFFF are the run register.
const BRAM: usize = 0x800;
// Shared-RAM layout in CPU byte offsets (usb_nano.h:17-35; nan:3-40).
const PIPES: usize = 0x600;
const PIPE_SIZE: usize = 0x18;
const PIPES_MAX: usize = 8;
const FIFO: usize = 0x700;
const FIFO_ENTRIES: u16 = 16;
const STATUS: usize = 0x7CC;
const REPORT_FRAME: usize = 0x7D4;
const DO_RESET: usize = 0x7DA;
const LINK_SPEED: usize = 0x7DC;
const NUM_PIPES: usize = 0x7DE;
const FIFO_TAIL: usize = 0x7F0;
const FIFO_HEAD: usize = 0x7F2;

// Pipe descriptor fields (usb_nano.h:37-50).
const P_COMMAND: usize = 0x00;
const P_DEV_EP: usize = 0x02;
const P_LENGTH: usize = 0x04;
const P_MAX_TRANS: usize = 0x06;
const P_INTERVAL: usize = 0x08;
const P_LAST_FRAME: usize = 0x0A;
const P_RESULT: usize = 0x0E;
const P_MEM_LO: usize = 0x10;
const P_MEM_HI: usize = 0x12;
const P_STARTED: usize = 0x14;
const P_TIMEOUT: usize = 0x16;

// Command bits (usb_nano.h:61-72).
const MEMREAD: u16 = 0x8000;
const MEMWRITE: u16 = 0x4000;
const TOGGLE: u16 = 0x0800;
const PAUSED: u16 = 0x0200;
const ABORT_REQ: u16 = 0x0100;
const TOKEN: u16 = 0x0003;
const TOKEN_SETUP: u16 = 0x0000;

// Result word: bit 15 done, 14:12 code, 11 toggle, 10 no data, 9:0 length (usb_cmd_nano.vhd:79-86; nan:87-98).
const RES_DONE: u16 = 0x8000;
const RES_ACK: u16 = 0x1000;
const RES_NAK: u16 = 0x2000;
const RES_STALL: u16 = 0x4000;
const RES_ERROR: u16 = 0x5000;
const RES_ABORTED: u16 = 0x6000;
const RES_TIMEOUT: u16 = 0x0800;
const RES_NO_DATA: u16 = 0x0400;
const RES_LENGTH: u16 = 0x03FF;

/// USTAT_CONNECTED / USTAT_OPERATIONAL in RAM_STATUS (usb_nano.h:57-58; nan:201,220).
const STATUS_CONNECTED: u16 = 0x01;
const STATUS_OPERATIONAL: u16 = 0x02;
/// NANO_LINK_SPEED after the chirp handshake with the high-speed hub (nan:418-420).
const SPEED_HIGH: u16 = 2;

/// Frame counter tick: 8 kHz (host_sequencer.vhd:186-193, 09 F3).
const CLOCKS_PER_FRAME: u64 = time::CLOCK_HZ / 8000;
/// The nano needs about a frame to pick up a pipe the firmware armed: it scans in a loop between FRAME_TICK checks
/// and runs the transaction through the sequencer (nan:941-1026). The firmware relies on that head start: the hub
/// driver clears `irq_data[0]` right after resuming its status pipe (usb_hub.cc:406-407), so an answer within the
/// next instructions would be wiped and the pipe never resumed again (09 H13).
const SCAN_LATENCY: u64 = CLOCKS_PER_FRAME;
/// `delay` with `ResetDelay` before the nano samples the line after START (nan:128-131,178-180).
const ATTACH_DELAY: u64 = 700 * time::CLOCKS_PER_MS;
/// Bus reset with the high-speed chirp. The firmware samples the status 50 ms after DO_RESET (usb_base.cc:303-307).
const RESET_DURATION: u64 = 10 * time::CLOCKS_PER_MS;
/// Largest packet the 10-bit response length can describe (usb_cmd_nano.vhd:85).
const MAX_PACKET: usize = RES_LENGTH as usize;
/// Pause after the firmware has handled an unplug before the device is plugged back in. The driver removes the old
/// child in its cleanup task (usb_base.cc:155-161, 280-298); a connect seen before that ends in "Device already
/// present!" (usb_hub.cc:355-379).
const REPLUG_GAP: u64 = 500 * time::CLOCKS_PER_MS;
/// A plug-in waits at most this long after the unplug for the firmware to clear C_PORT_CONNECTION.
const REPLUG_ACK_TIMEOUT: u64 = 5000 * time::CLOCKS_PER_MS;
/// Re-check interval while a plug-in waits for that.
const REPLUG_POLL: u64 = 20 * time::CLOCKS_PER_MS;

/// Devices on the hub ports, in port order: the images, the storage slots, then the keyboard and the mouse.
#[derive(Clone, Debug, Default)]
pub struct UsbConfig {
    /// Raw images attached as mass-storage devices.
    pub images: Vec<PathBuf>,
    /// Ports left empty after the images for storage the frontend attaches with
    /// `Machine::usb_attach_storage` (`--usb-dir` volumes, docs/status/usb-dir.md).
    pub storage_slots: usize,
    /// A HID keyboard fed by `HostInput::UsbKey`.
    pub keyboard: bool,
    /// S32: a HID mouse fed by `HostInput::UsbMouse`.
    pub mouse: bool,
}

impl UsbConfig {
    /// Devices to attach. With any, the frontend advertises [`CAPAB_USB_HOST2`].
    pub fn devices(&self) -> usize {
        self.images.len() + self.storage_slots + usize::from(self.keyboard) + usize::from(self.mouse)
    }
}

/// A device to plug into a hub port while the machine runs (S33).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbDevice {
    /// A raw image as a USB stick.
    Image(PathBuf),
    Keyboard,
    Mouse,
}

/// A hub port as [`Usb::port_info`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbPortInfo {
    /// `"storage"`, `"keyboard"` or `"mouse"`; None for an empty port.
    pub device: Option<&'static str>,
    /// The plug is in. A plug-in that is still waiting counts as out.
    pub connected: bool,
    /// The firmware has reset and enabled the port, so the device is in use.
    pub enabled: bool,
    /// A plug-in waits for the firmware to handle the unplug.
    pub plug_pending: bool,
}

/// The nano's link state (nan:122-268, 09 F2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// NANO_START = 0: the nano core is held in reset.
    Held,
    /// Started; the hub's pull-up is seen at this clock (`_device_detected`, nan:196-203).
    Attaching(u64),
    /// RAM_STATUS = 1, waiting for NANO_DO_RESET (`_waiting_for_reset`, nan:205-213).
    Attached,
    /// Bus reset until this clock (`do_reset`, nan:215-227).
    Resetting(u64),
    /// Operational: the pipe scheduler runs (`main_loop`, nan:259-268).
    Running,
}

/// The shared BRAM with little-endian 16-bit views of the nano's words.
struct Bram(Vec<u8>);

impl Bram {
    fn word(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.0[off], self.0[off + 1]])
    }

    fn set_word(&mut self, off: usize, val: u16) {
        self.0[off..off + 2].copy_from_slice(&val.to_le_bytes());
    }

    fn field(&self, pipe: usize, field: usize) -> u16 {
        self.word(PIPES + pipe * PIPE_SIZE + field)
    }

    fn set_field(&mut self, pipe: usize, field: usize, val: u16) {
        self.set_word(PIPES + pipe * PIPE_SIZE + field, val);
    }

    /// Pipes the scheduler looks at: NANO_NUM_PIPES, which the firmware raises in `open_pipe` (usb_base.cc:392-394).
    fn num_pipes(&self) -> usize {
        usize::from(self.word(NUM_PIPES)).clamp(1, PIPES_MAX)
    }

    /// The DMA address of a pipe.
    fn mem(&self, pipe: usize) -> u32 {
        u32::from(self.field(pipe, P_MEM_HI)) << 16 | u32::from(self.field(pipe, P_MEM_LO))
    }

    fn advance(&mut self, pipe: usize, bytes: usize) {
        let mem = self.mem(pipe).wrapping_add(bytes as u32);
        self.set_field(pipe, P_MEM_LO, mem as u16);
        self.set_field(pipe, P_MEM_HI, (mem >> 16) as u16);
    }

    /// `attr_fifo_full` (nan:1051-1056): the TAIL write is the only feedback from the firmware.
    fn fifo_full(&self) -> bool {
        (self.word(FIFO_HEAD) + 1) % FIFO_ENTRIES == self.word(FIFO_TAIL)
    }

    /// `attr_fifo_push` and SEND_INTERRUPT (nan:1058-1077).
    fn push(&mut self, word: u16, irq: &mut IrqState) {
        let head = self.word(FIFO_HEAD) % FIFO_ENTRIES;
        self.set_word(FIFO + 2 * usize::from(head), word);
        self.set_word(FIFO_HEAD, (head + 1) % FIFO_ENTRIES);
        irq.pulse(IRQ_BIT);
    }

    /// NAK: abort on request, time out after `timeout` frames since `started`, else retry on a later scan
    /// (nan:584-601). True when the pipe reports.
    fn nak(&mut self, pipe: usize, frame: u16) -> bool {
        let result = RES_DONE | RES_NAK;
        if self.field(pipe, P_COMMAND) & ABORT_REQ != 0 {
            self.set_field(pipe, P_RESULT, result & 0x0FFF | RES_ABORTED);
            return true;
        }
        self.set_field(pipe, P_RESULT, result);
        let timeout = self.field(pipe, P_TIMEOUT);
        if timeout == 0 || frame.wrapping_sub(self.field(pipe, P_STARTED)) < timeout {
            return false;
        }
        self.set_field(pipe, P_RESULT, result | RES_TIMEOUT);
        true
    }
}

pub struct Usb {
    bram: Bram,
    /// NANO_START bit 0.
    run: bool,
    link: Link,
    /// Next pipe scan while `Running`; None while no pipe can make progress.
    next_scan: Option<u64>,
    /// The USB2513 with the configured devices on its ports.
    root: Device,
    packet: Vec<u8>,
    /// Clock of the last unplug per hub port.
    unplugged_at: [u64; HUB_PORTS],
    /// When a waiting plug-in is checked next, per hub port.
    plug_due: [Option<u64>; HUB_PORTS],
}

impl Usb {
    /// `ports[i]` on hub port i + 1; None leaves the port empty.
    fn new(ports: Vec<Option<Device>>) -> Self {
        Usb {
            bram: Bram(vec![0; BRAM]),
            run: false,
            link: Link::Held,
            next_scan: None,
            root: Device::new(Peripheral::Hub(Hub::with_ports(ports))),
            packet: vec![0; MAX_PACKET],
            unplugged_at: [0; HUB_PORTS],
            plug_due: [None; HUB_PORTS],
        }
    }

    /// Press or release a key on the attached keyboard (HID usage, page 0x07). Ignored without a keyboard.
    pub fn key(&mut self, usage: u8, down: bool) {
        if let Some(keyboard) = self.root.keyboard() {
            keyboard.key(usage, down);
        }
    }

    /// Move the attached mouse and set its buttons (S32). Ignored without a mouse.
    pub fn mouse(&mut self, dx: i32, dy: i32, wheel: i32, buttons: u8) {
        if let Some(mouse) = self.root.mouse() {
            mouse.update(dx, dy, wheel, buttons);
        }
    }

    fn hub(&mut self) -> &mut Hub {
        match &mut self.root.function {
            Peripheral::Hub(hub) => hub,
            _ => unreachable!("the root device is the hub"),
        }
    }

    fn hub_ref(&self) -> &Hub {
        match &self.root.function {
            Peripheral::Hub(hub) => hub,
            _ => unreachable!("the root device is the hub"),
        }
    }

    /// Put a mass-storage device on `backend` on the empty hub port `port` (1-based), plugged in.
    pub fn attach_storage(&mut self, port: usize, backend: Box<dyn BlockBackend>) -> Result<(), String> {
        let storage = Storage::new(backend).map_err(|e| e.to_string())?;
        self.hub().insert(port, Device::new(Peripheral::Storage(storage)))
    }

    /// S33: unplug the device on hub port `port` (1-based) and take it off the port, so another can go there.
    pub fn unplug(&mut self, port: usize, now: u64) -> Result<&'static str, String> {
        let kind = self.port_info(port).ok_or_else(|| format!("the hub has no port {port}"))?.device;
        let kind = kind.ok_or_else(|| format!("hub port {port} is empty"))?;
        self.set_connected(port, false, now);
        self.hub().take(port);
        Ok(kind)
    }

    /// S33: put a new device on the empty hub port `port` (1-based) and plug it in, after the usual gap when the
    /// port was unplugged a moment ago.
    pub fn plug(&mut self, port: usize, device: UsbDevice, now: u64) -> Result<(), String> {
        let function = match device {
            UsbDevice::Image(path) => {
                Peripheral::Storage(Storage::open(&path).map_err(|e| format!("{}: {e}", path.display()))?)
            }
            UsbDevice::Keyboard => Peripheral::Keyboard(Keyboard::new()),
            UsbDevice::Mouse => Peripheral::Mouse(Mouse::new()),
        };
        self.hub().insert_unplugged(port, Device::new(function))?;
        self.set_connected(port, true, now);
        Ok(())
    }

    /// Swap the medium of the mass-storage device on `port` (1-based); returns the previous medium. Meant for an
    /// unplugged stick: the firmware sees a new stick when it is plugged back in.
    pub fn replace_backend(&mut self, port: usize, backend: Box<dyn BlockBackend>) -> Result<Box<dyn BlockBackend>, String> {
        match self.hub().port_device(port).map(|dev| &mut dev.function) {
            Some(Peripheral::Storage(storage)) => storage.replace_backend(backend).map_err(|e| e.to_string()),
            _ => Err(format!("no USB storage on hub port {port}")),
        }
    }

    /// Plug the device on hub port `port` (1-based) out, or back in. An unplug takes effect at once. A plug-in
    /// waits until the firmware has cleared C_PORT_CONNECTION of the unplug (at most [`REPLUG_ACK_TIMEOUT`]) and
    /// then [`REPLUG_GAP`] more. Ignored for an empty or missing port.
    pub fn set_connected(&mut self, port: usize, connected: bool, now: u64) {
        let i = port.wrapping_sub(1);
        if i >= HUB_PORTS || self.hub().port_device(port).is_none() {
            return;
        }
        if !connected {
            self.plug_due[i] = None;
            if self.hub().set_connected(port, false) {
                self.unplugged_at[i] = now;
            }
        } else if self.hub_ref().port_state(port).is_some_and(|(plugged, _, _)| plugged) {
            self.plug_due[i] = None;
        } else {
            self.plug_due[i] = Some((self.unplugged_at[i] + REPLUG_GAP).max(now));
        }
    }

    /// State of hub port `port` (1-based); None for a port the hub does not have.
    pub fn port_info(&self, port: usize) -> Option<UsbPortInfo> {
        let hub = self.hub_ref();
        let (connected, enabled, _) = hub.port_state(port)?;
        let device = hub.port_peripheral(port).map(|p| match p {
            Peripheral::Storage(_) => "storage",
            Peripheral::Keyboard(_) => "keyboard",
            Peripheral::Mouse(_) => "mouse",
            Peripheral::Hub(_) => "hub",
        });
        Some(UsbPortInfo { device, connected, enabled, plug_pending: self.plug_due[port - 1].is_some() })
    }

    /// Waiting plug-ins that are due: plug in once the firmware has acknowledged the unplug (or gave no sign for
    /// [`REPLUG_ACK_TIMEOUT`]), else look again after [`REPLUG_POLL`].
    fn plug_in_due(&mut self, now: u64) {
        for i in 0..HUB_PORTS {
            if self.plug_due[i].is_none_or(|due| due > now) {
                continue;
            }
            let acknowledged = self.hub_ref().port_state(i + 1).is_some_and(|(_, _, ack)| ack);
            if acknowledged || now >= self.unplugged_at[i] + REPLUG_ACK_TIMEOUT {
                self.hub().set_connected(i + 1, true);
                self.plug_due[i] = None;
            } else {
                self.plug_due[i] = Some(now + REPLUG_POLL);
            }
        }
    }

    fn link_event(&self) -> Option<u64> {
        match self.link {
            Link::Attaching(at) | Link::Resetting(at) => Some(at),
            Link::Running => self.next_scan,
            Link::Held | Link::Attached => None,
        }
    }

    fn set_run(&mut self, on: bool, now: u64) {
        if on && !self.run {
            // `begin` (nan:122-127). The on-board hub is always there.
            self.bram.set_word(NUM_PIPES, 1);
            self.bram.set_field(0, P_COMMAND, 0);
            self.root.reset();
            self.link = Link::Attaching(now + ATTACH_DELAY);
        } else if !on {
            self.link = Link::Held;
            self.next_scan = None;
        }
        self.run = on;
    }

    /// NANO_DO_RESET as `_waiting_for_reset` and `main_loop` see it (nan:207-208, 215-219, 263-264).
    fn check_reset(&mut self, now: u64) {
        if self.bram.word(DO_RESET) != 0 && matches!(self.link, Link::Attached | Link::Running) {
            self.bram.set_word(DO_RESET, 0);
            self.bram.set_word(STATUS, 0);
            self.next_scan = None;
            self.link = Link::Resetting(now + RESET_DURATION);
        }
    }

    /// When the next scan can do something: the earliest active pipe past its interval, at most one attempt per
    /// frame (nan:994-1011), and not before [`SCAN_LATENCY`]. A full FIFO waits for the TAIL write.
    fn reschedule(&mut self, now: u64) {
        if self.link != Link::Running || self.bram.fifo_full() {
            self.next_scan = None;
            return;
        }
        let frame = now / CLOCKS_PER_FRAME;
        let bram = &self.bram;
        self.next_scan = (0..bram.num_pipes())
            .filter_map(|pipe| {
                let command = bram.field(pipe, P_COMMAND);
                if command == 0 || command & PAUSED != 0 || bram.field(pipe, P_MEM_HI) == 0 {
                    return None;
                }
                let wait = bram.field(pipe, P_INTERVAL).max(1);
                let elapsed = (frame as u16).wrapping_sub(bram.field(pipe, P_LAST_FRAME));
                let due = (frame + u64::from(wait.saturating_sub(elapsed))) * CLOCKS_PER_FRAME;
                Some(due.max(now + SCAN_LATENCY))
            })
            .min();
    }

    /// `check_pipes` (nan:958-1026).
    fn scan(&mut self, ctx: &mut IoCtx) {
        let frame = (ctx.now / CLOCKS_PER_FRAME) as u16;
        for pipe in 0..self.bram.num_pipes() {
            let command = self.bram.field(pipe, P_COMMAND);
            if command == 0 || command & PAUSED != 0 {
                continue;
            }
            if frame.wrapping_sub(self.bram.field(pipe, P_LAST_FRAME)) < self.bram.field(pipe, P_INTERVAL) {
                continue;
            }
            if self.bram.fifo_full() || self.bram.field(pipe, P_MEM_HI) == 0 {
                continue;
            }
            if self.bram.field(pipe, P_STARTED) == 0 {
                self.bram.set_field(pipe, P_STARTED, frame);
            }
            self.bram.set_field(pipe, P_LAST_FRAME, frame);
            if self.transfer(pipe, frame, ctx) {
                // `_report` (nan:1028-1047).
                self.bram.push(pipe as u16, ctx.irq);
                if pipe == 0 {
                    self.bram.set_word(REPORT_FRAME, frame);
                }
                self.bram.set_field(pipe, P_COMMAND, self.bram.field(pipe, P_COMMAND) | PAUSED);
            }
        }
    }

    /// One transfer on `pipe`: every packet until the length is done, a short packet, or a handshake other than
    /// ACK/DATA (nan:604-937). True when the pipe reports.
    fn transfer(&mut self, pipe: usize, frame: u16, ctx: &mut IoCtx) -> bool {
        let dev_ep = self.bram.field(pipe, P_DEV_EP);
        let (address, ep) = ((dev_ep >> 8 & 0x7F) as u8, (dev_ep & 0x0F) as u8);
        let max_trans = self.bram.field(pipe, P_MAX_TRANS);
        let packet = &mut self.packet[..usize::from(max_trans).min(MAX_PACKET)];
        let Some(device) = self.root.find(address) else {
            // No handshake at all: the core answers ERROR and the nano gives up after its retries (nan:680-685).
            self.bram.set_field(pipe, P_RESULT, RES_DONE | RES_ERROR);
            return true;
        };
        let setup = self.bram.field(pipe, P_COMMAND) & TOKEN == TOKEN_SETUP;
        loop {
            let command = self.bram.field(pipe, P_COMMAND);
            let length = self.bram.field(pipe, P_LENGTH);
            if command & MEMREAD != 0 {
                // SETUP/OUT: `min(length, maxTrans)` bytes from memory per packet (nan:772-904).
                let sent = usize::from(length).min(packet.len());
                dma_read(ctx.ram, self.bram.mem(pipe), &mut packet[..sent]);
                let reply = if setup { device.setup(&packet[..sent], ctx.now) } else { device.output(ep, &packet[..sent]) };
                match reply {
                    Reply::Ack => {
                        self.bram.set_field(pipe, P_RESULT, RES_DONE | RES_ACK | RES_NO_DATA);
                        self.bram.set_field(pipe, P_COMMAND, command ^ TOGGLE);
                        self.bram.advance(pipe, sent);
                        self.bram.set_field(pipe, P_LENGTH, length - sent as u16);
                        if usize::from(length) == sent {
                            return true;
                        }
                        if sent == 0 {
                            return false;
                        }
                    }
                    Reply::Nak => return self.bram.nak(pipe, frame),
                    Reply::Stall | Reply::Data(_) => {
                        self.bram.set_field(pipe, P_RESULT, RES_DONE | RES_STALL);
                        return true;
                    }
                }
            } else {
                // IN: store `min(received, length)` bytes if MEMWRITE; go on while a full packet left some length
                // (nan:613-770). The device always sends the toggle the pipe expects.
                match device.input(ep, packet, ctx.now) {
                    Reply::Data(n) => {
                        let received = n as u16;
                        let no_data = if n == 0 { RES_NO_DATA } else { 0 };
                        self.bram.set_field(pipe, P_RESULT, RES_DONE | command & TOGGLE | no_data | received);
                        if command & MEMWRITE != 0 && n > 0 {
                            dma_write(ctx.ram, self.bram.mem(pipe), &packet[..n.min(usize::from(length))]);
                            self.bram.advance(pipe, n);
                        }
                        self.bram.set_field(pipe, P_COMMAND, command ^ TOGGLE);
                        let left = length.wrapping_sub(received);
                        self.bram.set_field(pipe, P_LENGTH, left);
                        if left == 0 || left & 0x8000 != 0 || received != max_trans || n == 0 {
                            return true;
                        }
                    }
                    Reply::Nak => return self.bram.nak(pipe, frame),
                    Reply::Stall | Reply::Ack => {
                        self.bram.set_field(pipe, P_RESULT, RES_DONE | RES_STALL);
                        return true;
                    }
                }
            }
        }
    }
}

/// DMA reads and writes through the 26-bit USB memory controller, identity-mapped onto DDR
/// (usb_memory_ctrl.vhd:47,135-138; 09 T1 step 6, Q3).
fn dma_read(ram: &[u8], addr: u32, buf: &mut [u8]) {
    for (i, byte) in (0u32..).zip(buf.iter_mut()) {
        *byte = ram[(addr.wrapping_add(i) & RAM_MASK) as usize];
    }
}

fn dma_write(ram: &mut [u8], addr: u32, data: &[u8]) {
    for (i, &byte) in (0u32..).zip(data) {
        ram[(addr.wrapping_add(i) & RAM_MASK) as usize] = byte;
    }
}

impl IoDevice for Usb {
    fn name(&self) -> &'static str {
        "usb-nano"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    /// BRAM bytes, or the run register at 0x800-0xFFF, where only bit 11 is decoded (nano.vhd:51-66). The
    /// firmware's 16-bit writes arrive low byte first.
    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        let off = off as usize;
        if off >= BRAM {
            self.set_run(val & 1 != 0, ctx.now);
            return;
        }
        self.bram.0[off] = val;
        if off & !1 == DO_RESET {
            self.check_reset(ctx.now);
        } else if (PIPES..PIPES + PIPES_MAX * PIPE_SIZE).contains(&off) || off & !1 == NUM_PIPES || off & !1 == FIFO_TAIL {
            self.reschedule(ctx.now);
        }
    }

    /// The run register reads 0 (nano.vhd:118-119).
    fn peek8(&self, off: u32) -> u8 {
        self.bram.0.get(off as usize).copied().unwrap_or(0)
    }

    fn next_event(&self) -> Option<u64> {
        self.plug_due.iter().flatten().copied().chain(self.link_event()).min()
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.plug_in_due(ctx.now);
        // A plug-in tick must not run the scheduler early: SCAN_LATENCY (09 H16).
        if self.link_event().is_none_or(|at| at > ctx.now) {
            return;
        }
        match self.link {
            Link::Attaching(_) => {
                self.bram.set_word(STATUS, STATUS_CONNECTED);
                self.bram.push(0xFFF0 | STATUS_CONNECTED, ctx.irq);
                self.link = Link::Attached;
                self.check_reset(ctx.now);
            }
            Link::Resetting(_) => {
                self.root.reset();
                self.bram.set_word(LINK_SPEED, SPEED_HIGH);
                self.bram.set_word(STATUS, STATUS_CONNECTED | STATUS_OPERATIONAL);
                self.link = Link::Running;
                self.reschedule(ctx.now);
            }
            Link::Running => {
                self.scan(ctx);
                self.reschedule(ctx.now);
            }
            Link::Held | Link::Attached => {}
        }
    }

    fn reset(&mut self) {
        self.bram.0.fill(0);
        self.run = false;
        self.link = Link::Held;
        self.next_scan = None;
        self.root.reset();
    }

    crate::impl_as_any!();
}

/// Map the window with the devices of `cfg.usb` on the hub ports: images, empty storage slots, keyboard, mouse. Port
/// numbers stay as configured: an image that cannot be opened leaves its port empty. Devices beyond the hub's
/// ports are reported and left out.
pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    let mut ports = Vec::new();
    for path in &cfg.usb.images {
        match Storage::open(path) {
            Ok(storage) => ports.push(Some(Device::new(Peripheral::Storage(storage)))),
            Err(e) => {
                eprintln!("usb: cannot use image {}: {e}; hub port {} left empty", path.display(), ports.len() + 1);
                ports.push(None);
            }
        }
    }
    ports.extend((0..cfg.usb.storage_slots).map(|_| None));
    if cfg.usb.keyboard {
        ports.push(Some(Device::new(Peripheral::Keyboard(Keyboard::new()))));
    }
    if cfg.usb.mouse {
        ports.push(Some(Device::new(Peripheral::Mouse(Mouse::new()))));
    }
    if ports.len() > HUB_PORTS {
        eprintln!("usb: the hub has {HUB_PORTS} ports; {} device(s) not attached", ports.len() - HUB_PORTS);
        ports.truncate(HUB_PORTS);
    }
    map.add(USB_BASE, WINDOW, Box::new(Usb::new(ports)));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::bus::RAM_SIZE;
    use crate::devices::board::rig::{cfg, Rig};

    /// Guest buffers; memHi must be non-zero (nan:1010-1011).
    const SETUP_BUF: u32 = 0x0012_3450;
    const DATA_BUF: u32 = 0x0023_0000;
    const IRQ_BUF: u32 = 0x0034_0000;

    #[test]
    fn c31_usb_ram_widths() {
        let mut rig = Rig::new(install);
        // initialize_usb_hub (usb_hwinit.cc:146-151).
        rig.w8(0x1008_0800, 0);
        for i in 0..1024 {
            rig.w16(0x1008_0000 + 2 * i, 0xFFFF);
        }
        // UsbBase::init: first blob words, then the "First DW" read (09 §Init step 4).
        rig.w16(0x1008_0000, 0x0A9D);
        rig.w16(0x1008_0002, 0x83EF);
        assert_eq!(rig.r32(0x1008_0000), 0x83EF_0A9D);
        assert_eq!(rig.r8(0x1008_0800), 0);
        assert_eq!(rig.r8(0x1008_0FFF), 0);
        rig.w16(0x1008_07F0, 0);
        rig.w16(0x1008_07F2, 0);
        assert_eq!(rig.r32(0x1008_07F0), 0);
        let usb = rig.map.get::<Usb>().unwrap();
        assert_eq!((usb.next_event(), rig.irq.flags), (None, 0), "held nano: no events, ITU bit 2 never raised");
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
        let [v0, v1] = value.to_le_bytes();
        let [i0, i1] = index.to_le_bytes();
        let [l0, l1] = length.to_le_bytes();
        [request_type, request, v0, v1, i0, i1, l0, l1]
    }

    fn image(sectors: usize) -> NamedTempFile {
        let img = NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..sectors * 512).map(|i| (i / 512) as u8 ^ (i as u8)).collect();
        fs::write(img.path(), data).unwrap();
        img
    }

    /// The firmware side, from `UsbBase` (usb_base.cc), driving the model through its IO window.
    struct Host {
        usb: Usb,
        ram: Vec<u8>,
        irq: IrqState,
        console: Vec<u8>,
        now: u64,
        /// `pipe->Command` toggles of the bulk IN and OUT pipes (usb_base.cc:798,891).
        toggle_in: u16,
        toggle_out: u16,
    }

    impl Host {
        fn new(devices: Vec<Peripheral>) -> Host {
            Host {
                usb: Usb::new(devices.into_iter().map(|p| Some(Device::new(p))).collect()),
                ram: vec![0; RAM_SIZE],
                irq: IrqState::new(),
                console: Vec::new(),
                now: 0,
                toggle_in: 0,
                toggle_out: 0,
            }
        }

        fn w8(&mut self, off: usize, val: u8) {
            let mut ctx = IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            self.usb.write8(off as u32, val, &mut ctx);
        }

        fn w16(&mut self, off: usize, val: u16) {
            self.w8(off, val as u8);
            self.w8(off + 1, (val >> 8) as u8);
        }

        fn r16(&self, off: usize) -> u16 {
            u16::from(self.usb.peek8(off as u32)) | u16::from(self.usb.peek8(off as u32 + 1)) << 8
        }

        fn set(&mut self, pipe: usize, field: usize, val: u16) {
            self.w16(PIPES + pipe * PIPE_SIZE + field, val);
        }

        fn get(&self, pipe: usize, field: usize) -> u16 {
            self.r16(PIPES + pipe * PIPE_SIZE + field)
        }

        fn set_mem(&mut self, pipe: usize, addr: u32) {
            self.set(pipe, P_MEM_HI, (addr >> 16) as u16);
            self.set(pipe, P_MEM_LO, addr as u16);
        }

        fn fifo_pending(&self) -> bool {
            self.r16(FIFO_TAIL) != self.r16(FIFO_HEAD)
        }

        /// Let up to `ms` of emulated time pass, ticking due events, until `stop` holds after a tick.
        fn run(&mut self, ms: u64, stop: impl Fn(&Host) -> bool) -> bool {
            let end = self.now + ms * time::CLOCKS_PER_MS;
            while let Some(at) = self.usb.next_event().filter(|&at| at <= end) {
                self.now = self.now.max(at);
                let mut ctx =
                    IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
                self.usb.tick(&mut ctx);
                if stop(self) {
                    return true;
                }
            }
            self.now = end;
            false
        }

        /// `irq_handler`/`get_fifo` (usb_base.cc:929-972) after the ISR acknowledged ITU bit 2.
        fn drain(&mut self) -> Vec<u16> {
            assert_ne!(self.irq.flags & 1 << IRQ_BIT, 0, "a push raises ITU bit 2");
            self.irq.clear(1 << IRQ_BIT);
            let mut words = Vec::new();
            let mut tail = self.r16(FIFO_TAIL);
            while tail != self.r16(FIFO_HEAD) {
                words.push(self.r16(FIFO + 2 * usize::from(tail)));
                tail = (tail + 1) % FIFO_ENTRIES;
                self.w16(FIFO_TAIL, tail);
            }
            words
        }

        /// `complete_command` on pipe 0, well inside its 100 ticks (09 H7): the result without bit 15.
        fn complete(&mut self) -> u16 {
            assert!(self.run(20, Host::fifo_pending), "no pipe 0 report");
            assert_eq!(self.drain(), [0]);
            self.get(0, P_RESULT) & 0x7FFF
        }

        /// `init` + `attach_root` + `bus_reset` (usb_base.cc:94-114, 301-346).
        fn start(&mut self) {
            self.w8(BRAM, 1);
            assert!(!self.run(690, Host::fifo_pending));
            assert!(self.run(20, Host::fifo_pending));
            assert_eq!((self.drain(), self.r16(STATUS)), (vec![0xFFF1], 1));
            self.w16(DO_RESET, 1);
            assert_eq!((self.r16(DO_RESET), self.r16(STATUS)), (0, 0), "the nano takes the reset request");
            self.run(50, |_| false);
            assert_eq!((self.r16(STATUS), self.r16(LINK_SPEED)), (3, 2));
        }

        /// `control_exchange` (usb_base.cc:561-642).
        fn control(&mut self, address: u8, request: [u8; 8], in_len: u16) -> Result<Vec<u8>, u16> {
            self.ram[SETUP_BUF as usize..][..8].copy_from_slice(&request);
            self.set(0, P_DEV_EP, u16::from(address) << 8);
            self.set(0, P_LENGTH, 8);
            self.set(0, P_MAX_TRANS, 64);
            self.set_mem(0, SETUP_BUF);
            self.set(0, P_STARTED, 0);
            self.set(0, P_COMMAND, 0x8040);
            let result = self.complete();
            if result & 0xF000 != RES_ACK {
                return Err(result);
            }
            self.set(0, P_LENGTH, in_len);
            self.set_mem(0, DATA_BUF);
            self.set(0, P_STARTED, 0);
            self.set(0, P_COMMAND, 0x4C42);
            let result = self.complete();
            let transferred = usize::from(in_len - self.get(0, P_LENGTH));
            if result & 0xF000 == RES_STALL {
                return Err(result);
            }
            if transferred > 0 {
                self.set(0, P_LENGTH, 0);
                self.set(0, P_STARTED, 0);
                self.set(0, P_RESULT, 0xFFFF);
                self.set(0, P_COMMAND, 0x8C41);
                assert_eq!(self.complete() & 0xF000, RES_ACK);
            }
            Ok(self.ram[DATA_BUF as usize..][..transferred].to_vec())
        }

        /// `bulk_out` for one chunk (usb_base.cc:747-821).
        fn bulk_out(&mut self, dev_ep: u16, data: &[u8]) -> u16 {
            self.ram[DATA_BUF as usize..][..data.len()].copy_from_slice(data);
            self.set(0, P_DEV_EP, dev_ep);
            self.set(0, P_MAX_TRANS, 512);
            self.set_mem(0, DATA_BUF);
            self.set(0, P_LENGTH, data.len() as u16);
            self.set(0, P_TIMEOUT, 20000);
            self.set(0, P_STARTED, 0);
            self.set(0, P_COMMAND, self.toggle_out | 0x8441);
            let result = self.complete();
            self.toggle_out = self.get(0, P_COMMAND) & TOGGLE;
            assert_eq!(self.get(0, P_LENGTH), 0);
            result
        }

        /// `bulk_in` for one chunk (usb_base.cc:823-903).
        fn bulk_in(&mut self, dev_ep: u16, len: u16) -> (Vec<u8>, u16) {
            self.set(0, P_DEV_EP, dev_ep);
            self.set(0, P_MAX_TRANS, len.min(512));
            self.set_mem(0, DATA_BUF);
            self.set(0, P_TIMEOUT, 20000);
            self.set(0, P_LENGTH, len);
            self.set(0, P_STARTED, 0);
            self.set(0, P_COMMAND, self.toggle_in | 0x4442);
            let result = self.complete();
            self.toggle_in = self.get(0, P_COMMAND) & TOGGLE;
            let transferred = usize::from(len - self.get(0, P_LENGTH));
            (self.ram[DATA_BUF as usize..][..transferred].to_vec(), result)
        }

        /// `activate_autopipe` + `resume_input_pipe` (usb_base.cc:426-477).
        fn interrupt_pipe(&mut self, pipe: usize, dev_ep: u16, max_trans: u16, interval: u16, length: u16) {
            if usize::from(self.r16(NUM_PIPES)) < pipe + 1 {
                self.w16(NUM_PIPES, pipe as u16 + 1);
            }
            self.set(pipe, P_DEV_EP, dev_ep);
            self.set(pipe, P_MAX_TRANS, max_trans);
            self.set(pipe, P_INTERVAL, interval);
            self.set(pipe, P_COMMAND, 0x4242);
            self.resume(pipe, length);
        }

        fn resume(&mut self, pipe: usize, length: u16) {
            self.set(pipe, P_LENGTH, length);
            self.set_mem(pipe, IRQ_BUF + pipe as u32);
            self.set(pipe, P_STARTED, 0);
            self.set(pipe, P_COMMAND, self.get(pipe, P_COMMAND) & !PAUSED);
        }

        /// Hub on address 1 with ports powered and its status pipe on autopipe 1; the device of port 1 reset,
        /// addressed as 2 and configured (usb_hub.cc:118-204, 252-379; usb_device.cc:374-402). Returns the device
        /// descriptor and the high byte of the port status (power and speed bits).
        fn enumerate_port_1(&mut self) -> (Vec<u8>, u8) {
            self.start();
            let hub = self.control(0, setup(0x80, 0x06, 0x0100, 0, 18), 18).unwrap();
            assert_eq!((hub.len(), hub[4], &hub[8..12]), (18, 0x09, &[0x24, 0x04, 0x13, 0x25][..]));
            assert_eq!(self.control(0, setup(0x00, 0x05, 1, 0, 0), 0), Ok(vec![]));
            assert_eq!(self.control(0, setup(0x80, 0x06, 0x0100, 0, 18), 18), Err(RES_ERROR), "nobody at address 0");
            let head = self.control(1, setup(0x80, 0x06, 0x0200, 0, 9), 9).unwrap();
            assert_eq!(self.control(1, setup(0x80, 0x06, 0x0200, 0, 25), head[2].into()).unwrap().len(), 25);
            assert_eq!(self.control(1, setup(0x00, 0x09, 1, 0, 0), 0), Ok(vec![]));
            let descriptor = self.control(1, setup(0xA0, 0x06, 0x2900, 0, 64), 64).unwrap();
            assert_eq!((descriptor.len(), descriptor[2]), (9, 3));
            for port in 1..=3 {
                assert_eq!(self.control(1, setup(0x23, 0x03, 8, port, 0), 64), Ok(vec![]));
            }

            self.interrupt_pipe(1, 0x0101, 64, 1160, 2);
            assert!(self.run(200, Host::fifo_pending));
            assert_eq!(self.drain(), [1]);
            let received = 2 - self.get(1, P_LENGTH);
            assert_eq!((received, self.ram[IRQ_BUF as usize + 1]), (1, 0x02), "one bitmap byte for port 1 (09 H12)");
            let connected = self.control(1, setup(0xA3, 0x00, 0, 1, 4), 8).unwrap();
            assert_eq!([connected[0], connected[2], connected[3]], [0x01, 0x01, 0x00], "connection change");
            self.control(1, setup(0x23, 0x01, 0x10, 1, 0), 8).unwrap();
            self.control(1, setup(0x23, 0x03, 0x04, 1, 0), 8).unwrap();
            self.resume(1, 2);
            let due = self.usb.next_event().unwrap();
            assert!(due >= self.now + CLOCKS_PER_FRAME, "the driver clears irq_data after resuming (usb_hub.cc:406-407)");
            assert!(self.run(200, Host::fifo_pending));
            assert_eq!(self.drain(), [1]);
            let status = self.control(1, setup(0xA3, 0x00, 0, 1, 4), 8).unwrap();
            assert_eq!(&status[2..], [0x10, 0x00], "reset change");

            let device = self.control(0, setup(0x80, 0x06, 0x0100, 0, 18), 18).unwrap();
            assert_eq!(self.control(0, setup(0x00, 0x05, 2, 0, 0), 0), Ok(vec![]));
            self.control(1, setup(0x23, 0x01, 0x14, 1, 0), 8).unwrap();
            assert_eq!(self.control(2, setup(0x00, 0x09, 1, 0, 0), 0), Ok(vec![]));
            self.resume(1, 2);
            assert!(!self.run(300, Host::fifo_pending), "no port change: the hub NAKs (09 H13)");
            (device, connected[1])
        }
    }

    fn cbw(tag: u32, length: u32, flags: u8, cb: &[u8]) -> Vec<u8> {
        let mut out = b"USBC".to_vec();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[flags, 0, cb.len() as u8]);
        out.extend_from_slice(cb);
        out.resize(31, 0);
        out
    }

    #[test]
    fn storage_behind_the_hub_answers_the_scsi_driver() {
        let img = image(64);
        let mut h = Host::new(vec![Peripheral::Storage(Storage::open(img.path()).unwrap())]);
        let (device, port) = h.enumerate_port_1();
        assert_eq!((&device[8..12], port), (&[0x09, 0x12, 0x01, 0x00][..], 0x05), "powered, high speed");
        assert_eq!(h.control(2, setup(0xA1, 0xFE, 0, 0, 1), 8), Ok(vec![0]), "GET_MAX_LUN (09 H11)");
        let product = h.control(2, setup(0x80, 0x06, 0x0302, 0x0409, 256), 256).unwrap();
        assert_eq!(product.len(), 30, "\"USB Disk Image\" as UTF-16");

        assert_eq!(h.bulk_out(0x0202, &cbw(1, 36, 0x80, &[0x12, 0, 0, 0, 36, 0])), RES_ACK | RES_NO_DATA);
        let (inquiry, result) = h.bulk_in(0x0201, 36);
        assert_eq!((inquiry.len(), &inquiry[8..14], result & 0xF000), (36, &b"UE2EMU"[..], 0));
        let (csw, _) = h.bulk_in(0x0201, 13);
        assert_eq!((&csw[..4], csw[12]), (&b"USBS"[..], 0));

        h.bulk_out(0x0202, &cbw(2, 1024, 0x80, &[0x28, 0, 0, 0, 0, 3, 0, 0, 2, 0]));
        let (data, _) = h.bulk_in(0x0201, 1024);
        assert_eq!(data, fs::read(img.path()).unwrap()[3 * 512..5 * 512]);
        assert_eq!(h.bulk_in(0x0201, 13).0[12], 0);

        let block = [0x5A; 512];
        h.bulk_out(0x0202, &cbw(3, 512, 0x00, &[0x2A, 0, 0, 0, 0, 9, 0, 0, 1, 0]));
        h.bulk_out(0x0202, &block);
        let (csw, _) = h.bulk_in(0x0201, 13);
        assert_eq!((u32::from_le_bytes(csw[4..8].try_into().unwrap()), csw[12]), (3, 0));
        assert_eq!(fs::read(img.path()).unwrap()[9 * 512..10 * 512], block);
    }

    #[test]
    fn keyboard_reports_arrive_on_its_interrupt_pipe() {
        let mut h = Host::new(vec![Peripheral::Keyboard(Keyboard::new())]);
        assert_eq!(h.enumerate_port_1().1, 0x01, "powered, full speed");
        assert_eq!(h.control(2, setup(0x21, 0x0A, 0, 0, 0), 0), Ok(vec![]), "SET_IDLE 0");
        h.interrupt_pipe(2, 0x0201, 8, 160, 8);
        assert!(!h.run(100, Host::fifo_pending), "NAK while nothing changes");

        h.usb.key(0xE1, true);
        h.usb.key(0x04, true);
        assert!(h.run(21, Host::fifo_pending), "within one 20 ms interval");
        assert_eq!(h.drain(), [2]);
        let report = &h.ram[IRQ_BUF as usize + 2..][..8];
        assert_eq!((8 - h.get(2, P_LENGTH), report), (8, &[0x02, 0, 0, 0, 0, 0, 0, 0][..]));
        h.resume(2, 8);
        assert!(h.run(21, Host::fifo_pending));
        h.drain();
        assert_eq!(&h.ram[IRQ_BUF as usize + 2..][..8], [0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        h.usb.key(0x04, false);
        assert!(!h.run(100, Host::fifo_pending), "a reported pipe waits for its resume");
        h.resume(2, 8);
        assert!(h.run(21, Host::fifo_pending));
        h.drain();
        assert_eq!(&h.ram[IRQ_BUF as usize + 2..][..8], [0x02, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_naking_pipe_times_out_or_aborts() {
        let mut h = Host::new(vec![Peripheral::Keyboard(Keyboard::new())]);
        h.enumerate_port_1();
        h.control(2, setup(0x21, 0x0A, 0, 0, 0), 0).unwrap();
        h.set(0, P_DEV_EP, 0x0201);
        h.set(0, P_MAX_TRANS, 8);
        h.set_mem(0, DATA_BUF);
        h.set(0, P_LENGTH, 8);
        h.set(0, P_TIMEOUT, 80);
        h.set(0, P_STARTED, 0);
        h.set(0, P_COMMAND, 0x4442);
        assert!(!h.run(9, Host::fifo_pending));
        assert!(h.run(3, Host::fifo_pending), "80 frames = 10 ms");
        assert_eq!((h.drain(), h.get(0, P_RESULT) & 0x7FFF), (vec![0], RES_NAK | RES_TIMEOUT));

        h.set(0, P_TIMEOUT, 0);
        h.set(0, P_STARTED, 0);
        h.set(0, P_COMMAND, 0x4442);
        assert!(!h.run(1000, Host::fifo_pending), "no timeout: NAK retried until aborted");
        h.set(0, P_COMMAND, 0x4442 | ABORT_REQ);
        assert!(h.run(1, Host::fifo_pending));
        assert_eq!((h.drain(), h.get(0, P_RESULT) & 0xF000), (vec![0], RES_ABORTED));
    }

    #[test]
    fn a_full_fifo_holds_reports_until_the_tail_moves() {
        let mut h = Host::new(Vec::new());
        h.start();
        let head = h.r16(FIFO_HEAD);
        h.w16(FIFO_TAIL, (head + 1) % FIFO_ENTRIES);
        h.ram[SETUP_BUF as usize..][..8].copy_from_slice(&setup(0x80, 0x06, 0x0100, 0, 18));
        h.set(0, P_LENGTH, 8);
        h.set(0, P_MAX_TRANS, 64);
        h.set_mem(0, SETUP_BUF);
        h.set(0, P_COMMAND, 0x8040);
        assert_eq!(h.usb.next_event(), None);
        h.w16(FIFO_TAIL, head);
        assert!(h.run(1, |h| h.r16(FIFO_HEAD) != head));
        assert_eq!(h.get(0, P_RESULT) & 0x7FFF & 0xF000, RES_ACK);
        h.w8(BRAM, 0);
        assert_eq!(h.usb.next_event(), None, "NANO_START = 0 holds the nano");
    }

    /// S33: a device plugged into an empty port waits for the plug-in, an unplug empties the port again.
    #[test]
    fn devices_plug_in_and_out_while_running() {
        let mut map = IoMap::new();
        install(&mut map, &cfg());
        let usb = map.get_mut::<Usb>().unwrap();
        let info = |usb: &Usb, port| usb.port_info(port).unwrap();
        usb.plug(2, UsbDevice::Mouse, 0).unwrap();
        assert_eq!((info(usb, 2).device, info(usb, 2).plug_pending), (Some("mouse"), true), "plugged in on the next tick");
        assert!(usb.plug(2, UsbDevice::Keyboard, 0).is_err(), "port 2 is in use");
        assert!(usb.plug(1, UsbDevice::Image("/nonexistent/usb.img".into()), 0).is_err(), "no such image");
        assert_eq!(info(usb, 1).device, None, "and the port stays empty");
        assert_eq!(usb.unplug(2, 0), Ok("mouse"));
        assert_eq!(info(usb, 2).device, None);
        assert!(usb.unplug(2, 0).is_err(), "nothing left to unplug");
    }

    #[test]
    fn install_attaches_the_configured_devices_to_hub_ports() {
        let kinds = |config: &MachineConfig| {
            let mut map = IoMap::new();
            install(&mut map, config);
            let usb = map.get_mut::<Usb>().unwrap();
            let Peripheral::Hub(hub) = &mut usb.root.function else { unreachable!("the root is the hub") };
            hub.devices()
                .map(|dev| match dev.function {
                    Peripheral::Storage(_) => "storage",
                    Peripheral::Keyboard(_) => "keyboard",
                    Peripheral::Mouse(_) => "mouse",
                    Peripheral::Hub(_) => "hub",
                })
                .collect::<Vec<_>>()
        };
        let img = image(8);
        let mut config = cfg();
        assert!(kinds(&config).is_empty());
        config.usb =
            UsbConfig { images: vec![img.path().into(), "/nonexistent/usb.img".into()], storage_slots: 0, keyboard: true, mouse: false };
        assert_eq!((config.usb.devices(), kinds(&config)), (3, vec!["storage", "keyboard"]));
        config.usb.images = vec![img.path().into(); 3];
        assert_eq!(kinds(&config), ["storage"; 3], "the keyboard does not fit");

        config.usb = UsbConfig { images: vec!["/nonexistent/usb.img".into()], storage_slots: 1, keyboard: true, mouse: false };
        let mut map = IoMap::new();
        install(&mut map, &config);
        let usb = map.get_mut::<Usb>().unwrap();
        let kind = |usb: &Usb, port| usb.port_info(port).unwrap().device;
        assert_eq!([kind(usb, 1), kind(usb, 2), kind(usb, 3)], [None, None, Some("keyboard")], "ports stay in order");
        usb.attach_storage(2, Box::new(ImageFile::open(img.path()).unwrap())).unwrap();
        assert!(usb.attach_storage(3, Box::new(ImageFile::open(img.path()).unwrap())).is_err(), "port 3 is in use");
        assert_eq!(kind(usb, 2), Some("storage"));
        assert!(usb.replace_backend(3, Box::new(ImageFile::open(img.path()).unwrap())).is_err(), "not storage");
        assert_eq!(usb.port_info(4), None);
    }

    /// Port 1 of a running hub unplugged and plugged back in, the way `UsbHubDriver::handle_irqdata` sees it.
    #[test]
    fn replug_waits_for_the_driver_to_handle_the_unplug() {
        let img = image(64);
        let mut h = Host::new(vec![Peripheral::Storage(Storage::open(img.path()).unwrap())]);
        h.enumerate_port_1();
        let info = h.usb.port_info(1).unwrap();
        assert_eq!((info.device, info.connected, info.enabled, info.plug_pending), (Some("storage"), true, true, false));

        h.usb.set_connected(1, false, h.now);
        h.usb.set_connected(1, true, h.now);
        assert!(h.usb.port_info(1).unwrap().plug_pending);
        assert!(h.run(200, Host::fifo_pending), "the hub reports the disconnect");
        assert_eq!(h.drain(), [1]);
        let status = h.control(1, setup(0xA3, 0x00, 0, 1, 4), 8).unwrap();
        assert_eq!(status, [0x00, 0x01, 0x01, 0x00], "powered, not connected, connection change");
        assert_eq!(h.control(2, setup(0x80, 0x06, 0x0100, 0, 18), 18), Err(RES_ERROR), "the stick is gone");

        h.run(1000, |_| false);
        assert!(!h.usb.port_info(1).unwrap().connected, "no plug-in before the driver clears C_PORT_CONNECTION");
        h.control(1, setup(0x23, 0x01, 0x10, 1, 0), 8).unwrap();
        let cleared = h.now;
        h.resume(1, 2);
        assert!(h.run(600, Host::fifo_pending), "plugged in again");
        assert!(h.now >= cleared, "after the acknowledgement");
        assert_eq!(h.drain(), [1]);
        let status = h.control(1, setup(0xA3, 0x00, 0, 1, 4), 8).unwrap();
        assert_eq!(status, [0x01, 0x05, 0x01, 0x00], "connected, high speed, connection change");
        assert!(!h.usb.port_info(1).unwrap().plug_pending);

        // No acknowledgement at all: the plug-in happens after REPLUG_ACK_TIMEOUT.
        h.usb.set_connected(1, false, h.now);
        h.usb.set_connected(1, true, h.now);
        h.run(4900, |_| false);
        assert!(!h.usb.port_info(1).unwrap().connected);
        h.run(200, |_| false);
        assert!(h.usb.port_info(1).unwrap().connected);
    }
}
