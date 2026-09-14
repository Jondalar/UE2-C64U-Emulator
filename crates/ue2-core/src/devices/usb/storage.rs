//! USB mass-storage device: Bulk-Only Transport with the SCSI commands `UsbScsiDriver` sends, backed by a
//! [`BlockBackend`] whose block count is the capacity (docs/hw/09-usb.md §F6, T1 step 9).

use std::io::{self, ErrorKind};
use std::path::Path;

use super::block::{BlockBackend, ImageFile, BLOCK_SIZE as BLOCK};
use super::device::{Function, Reply, Setup, Speed};

/// Device descriptor: USB 2.0, class in the interface, EP0 64 bytes, VID 0x1209 (pid.codes) PID 0x0001 (test
/// range), manufacturer, product and serial strings.
const DEVICE: [u8; 18] = [
    0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x09, 0x12, 0x01, 0x00, 0x00, 0x01, 0x01, 0x02, 0x03, 0x01,
];

/// Configuration 1: one interface, class 08 subclass 06 (SCSI transparent) protocol 0x50 (Bulk-Only; the tester
/// wants 0x50, usb_scsi.cc:59-66), bulk IN 0x81 and bulk OUT 0x02 of 512 bytes (high speed).
const CONFIGURATION: [u8; 32] = [
    0x09, 0x02, 32, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32, // configuration
    0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00, // interface
    0x07, 0x05, 0x81, 0x02, 0x00, 0x02, 0x00, // bulk IN
    0x07, 0x05, 0x02, 0x02, 0x00, 0x02, 0x00, // bulk OUT
];

const STRINGS: &[&str] = &["UE2EMU", "USB Disk Image", "UE2EMU0001"];

/// Bulk endpoint numbers; `find_endpoint` matches direction and type, not the number (usb_device.h:242-255).
const EP_IN: u8 = 1;
const EP_OUT: u8 = 2;

// Class request GET MAX LUN and the standard CLEAR_FEATURE(ENDPOINT_HALT) of `unstall_pipe` (usb_scsi.cc:15;
// usb_device.cc:55,353-357).
const GET_MAX_LUN: u8 = 0xFE;
const CLEAR_FEATURE: u8 = 0x01;

/// Command and status wrappers (usb_scsi.cc:104-108, 258-276).
const CBW_SIGNATURE: &[u8; 4] = b"USBC";
const CBW_LEN: usize = 31;
const CSW_SIGNATURE: &[u8; 4] = b"USBS";
const CBW_DATA_IN: u8 = 0x80;
const CSW_FAILED: u8 = 1;

// SCSI operation codes the driver sends (usb_scsi.cc:307,481,524,550,837,867).
const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;

/// INQUIRY: direct-access, removable, SPC-2, vendor "UE2EMU", product "USB Disk Image", revision "1.00".
const INQUIRY_DATA: [u8; 36] = *b"\x00\x80\x04\x02\x1f\x00\x00\x00UE2EMU  USB Disk Image  1.00";

/// Sense key, additional sense code and qualifier for REQUEST SENSE (usb_scsi.cc:343-366).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sense(u8, u8, u8);

const NO_SENSE: Sense = Sense(0x00, 0x00, 0x00);
const UNRECOVERED_READ_ERROR: Sense = Sense(0x03, 0x11, 0x00);
const WRITE_ERROR: Sense = Sense(0x03, 0x0C, 0x00);
const INVALID_OPCODE: Sense = Sense(0x05, 0x20, 0x00);
const LBA_OUT_OF_RANGE: Sense = Sense(0x05, 0x21, 0x00);
const WRITE_PROTECTED: Sense = Sense(0x07, 0x27, 0x00);

#[derive(Clone, Copy)]
struct Cbw {
    tag: u32,
    length: u32,
    data_in: bool,
    cb: [u8; 16],
}

impl Cbw {
    fn parse(data: &[u8]) -> Option<Cbw> {
        if data.len() != CBW_LEN || &data[..4] != CBW_SIGNATURE {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
        Some(Cbw { tag: u32_at(4), length: u32_at(8), data_in: data[12] & CBW_DATA_IN != 0, cb: data[15..].try_into().unwrap() })
    }
}

#[derive(Clone, Copy)]
struct Csw {
    tag: u32,
    residue: u32,
    status: u8,
}

impl Csw {
    fn bytes(&self) -> [u8; 13] {
        let mut out = [0; 13];
        out[..4].copy_from_slice(CSW_SIGNATURE);
        out[4..8].copy_from_slice(&self.tag.to_le_bytes());
        out[8..12].copy_from_slice(&self.residue.to_le_bytes());
        out[12] = self.status;
        out
    }
}

/// Bulk-Only Transport phase.
enum Phase {
    /// Waiting for a CBW on the OUT endpoint.
    Command,
    /// Sending `data[sent..]` on the IN endpoint, then the CSW.
    DataIn { data: Vec<u8>, sent: usize, csw: Csw },
    /// Receiving the data stage of `cbw` on the OUT endpoint.
    DataOut { cbw: Cbw, data: Vec<u8> },
    /// The CSW is ready on the IN endpoint.
    Status(Csw),
}

pub(crate) struct Storage {
    backend: Box<dyn BlockBackend>,
    phase: Phase,
    /// Reported by the next REQUEST SENSE, then cleared.
    sense: Sense,
}

impl Storage {
    /// A raw image, read-write, or read-only (writes fail with DATA PROTECT) when writing is not permitted.
    pub(crate) fn open(path: &Path) -> io::Result<Storage> {
        Storage::new(Box::new(ImageFile::open(path)?))
    }

    /// A stick on `backend`, which must hold at least one block.
    pub(crate) fn new(backend: Box<dyn BlockBackend>) -> io::Result<Storage> {
        if backend.blocks() == 0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "USB medium is smaller than one block"));
        }
        Ok(Storage { backend, phase: Phase::Command, sense: NO_SENSE })
    }

    /// Swap the medium (while the stick is unplugged); returns the previous one.
    pub(crate) fn replace_backend(&mut self, backend: Box<dyn BlockBackend>) -> io::Result<Box<dyn BlockBackend>> {
        if backend.blocks() == 0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "USB medium is smaller than one block"));
        }
        self.reset();
        Ok(std::mem::replace(&mut self.backend, backend))
    }

    /// Starts a command. The data stage has exactly the CBW length, padded or cut, so every IN packet the driver
    /// waits for carries data (09 H10).
    fn command(&mut self, cbw: Cbw) -> Phase {
        let length = cbw.length as usize;
        if !cbw.data_in && length > 0 {
            return Phase::DataOut { cbw, data: Vec::with_capacity(length) };
        }
        let (mut data, csw) = self.finish(&cbw, &[]);
        if length == 0 {
            return Phase::Status(csw);
        }
        data.resize(length, 0);
        Phase::DataIn { data, sent: 0, csw }
    }

    /// Runs the SCSI command of `cbw` with the received OUT data; returns the IN data and the CSW.
    fn finish(&mut self, cbw: &Cbw, out: &[u8]) -> (Vec<u8>, Csw) {
        let (data, status, residue) = match self.scsi(&cbw.cb, out) {
            Ok(data) => (data, 0, 0),
            Err(sense) => {
                self.sense = sense;
                (Vec::new(), CSW_FAILED, cbw.length)
            }
        };
        (data, Csw { tag: cbw.tag, residue, status })
    }

    fn scsi(&mut self, cb: &[u8; 16], out: &[u8]) -> Result<Vec<u8>, Sense> {
        let lba = u64::from(u32::from_be_bytes(cb[2..6].try_into().unwrap()));
        let blocks = u64::from(u16::from_be_bytes([cb[7], cb[8]]));
        match cb[0] {
            TEST_UNIT_READY => Ok(Vec::new()),
            REQUEST_SENSE => {
                let Sense(key, asc, ascq) = std::mem::replace(&mut self.sense, NO_SENSE);
                Ok(vec![0x70, 0, key, 0, 0, 0, 0, 10, 0, 0, 0, 0, asc, ascq, 0, 0, 0, 0])
            }
            INQUIRY => Ok(INQUIRY_DATA.to_vec()),
            READ_CAPACITY_10 => {
                let mut data = ((self.backend.blocks() - 1).min(u64::from(u32::MAX)) as u32).to_be_bytes().to_vec();
                data.extend_from_slice(&(BLOCK as u32).to_be_bytes());
                Ok(data)
            }
            READ_10 => {
                self.check_range(lba, blocks)?;
                let mut data = vec![0; (blocks * BLOCK) as usize];
                self.backend.read_blocks(lba, &mut data).map_err(|_| UNRECOVERED_READ_ERROR)?;
                Ok(data)
            }
            WRITE_10 => {
                if self.backend.read_only() {
                    return Err(WRITE_PROTECTED);
                }
                self.check_range(lba, blocks)?;
                // A short data stage writes only its whole blocks.
                let len = ((blocks * BLOCK) as usize).min(out.len()) / BLOCK as usize * BLOCK as usize;
                self.backend.write_blocks(lba, &out[..len]).map_err(|_| WRITE_ERROR)?;
                Ok(Vec::new())
            }
            _ => Err(INVALID_OPCODE),
        }
    }

    fn check_range(&self, lba: u64, blocks: u64) -> Result<(), Sense> {
        if lba + blocks > self.backend.blocks() {
            return Err(LBA_OUT_OF_RANGE);
        }
        Ok(())
    }
}

impl Function for Storage {
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
        STRINGS
    }

    fn control(&mut self, setup: &Setup, _data: &[u8], _now: u64) -> Option<Vec<u8>> {
        match (setup.request_type, setup.request) {
            // One LUN: the driver keeps an uninitialised byte unless it gets one (09 H11).
            (0xA1, GET_MAX_LUN) => Some(vec![0]),
            // Endpoints never stay halted, so clearing a halt only acknowledges.
            (0x02, CLEAR_FEATURE) if setup.value == 0 => Some(Vec::new()),
            _ => None,
        }
    }

    fn input(&mut self, ep: u8, buf: &mut [u8], _now: u64) -> Reply {
        if ep != EP_IN {
            return Reply::Stall;
        }
        match std::mem::replace(&mut self.phase, Phase::Command) {
            Phase::DataIn { data, sent, csw } => {
                let n = buf.len().min(data.len() - sent);
                buf[..n].copy_from_slice(&data[sent..sent + n]);
                self.phase =
                    if sent + n == data.len() { Phase::Status(csw) } else { Phase::DataIn { data, sent: sent + n, csw } };
                Reply::Data(n)
            }
            Phase::Status(csw) => {
                let bytes = csw.bytes();
                let n = buf.len().min(bytes.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Reply::Data(n)
            }
            phase => {
                self.phase = phase;
                Reply::Nak
            }
        }
    }

    fn output(&mut self, ep: u8, data: &[u8]) -> Reply {
        if ep != EP_OUT {
            return Reply::Stall;
        }
        match std::mem::replace(&mut self.phase, Phase::Command) {
            Phase::Command => match Cbw::parse(data) {
                Some(cbw) => {
                    self.phase = self.command(cbw);
                    Reply::Ack
                }
                None => Reply::Stall,
            },
            Phase::DataOut { cbw, data: mut received } => {
                received.extend_from_slice(data);
                self.phase = if received.len() >= cbw.length as usize {
                    Phase::Status(self.finish(&cbw, &received).1)
                } else {
                    Phase::DataOut { cbw, data: received }
                };
                Reply::Ack
            }
            phase => {
                self.phase = phase;
                Reply::Stall
            }
        }
    }

    fn reset(&mut self) {
        self.phase = Phase::Command;
        self.sense = NO_SENSE;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::NamedTempFile;

    use super::*;

    fn image(sectors: u64) -> NamedTempFile {
        let img = NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..sectors * BLOCK).map(|i| (i / BLOCK) as u8 ^ (i as u8)).collect();
        fs::write(img.path(), data).unwrap();
        img
    }

    fn cbw(tag: u32, length: u32, data_in: bool, cb: &[u8]) -> Vec<u8> {
        let mut out = CBW_SIGNATURE.to_vec();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[if data_in { CBW_DATA_IN } else { 0 }, 0, cb.len() as u8]);
        out.extend_from_slice(cb);
        out.resize(CBW_LEN, 0);
        out
    }

    /// One BOT command with the driver's exact byte counts: CBW, data stage in 512-byte packets, 13-byte CSW.
    fn exchange(dev: &mut Storage, tag: u32, data_in: bool, cb: &[u8], out: &[u8], in_len: usize) -> (Vec<u8>, [u8; 13]) {
        let length = if data_in { in_len } else { out.len() };
        assert_eq!(dev.output(EP_OUT, &cbw(tag, length as u32, data_in, cb)), Reply::Ack);
        for chunk in out.chunks(512) {
            assert_eq!(dev.output(EP_OUT, chunk), Reply::Ack);
        }
        let mut data = Vec::new();
        let mut buf = [0; 512];
        while data.len() < in_len {
            let max = (in_len - data.len()).min(512);
            match dev.input(EP_IN, &mut buf[..max], 0) {
                Reply::Data(n) => data.extend_from_slice(&buf[..n]),
                other => panic!("data stage: {other:?}"),
            }
        }
        let mut csw = [0; 13];
        assert_eq!(dev.input(EP_IN, &mut csw, 0), Reply::Data(13));
        assert_eq!((&csw[..4], u32::from_le_bytes(csw[4..8].try_into().unwrap())), (&CSW_SIGNATURE[..], tag));
        (data, csw)
    }

    #[test]
    fn inquiry_capacity_and_sense_have_the_driver_lengths() {
        let img = image(64);
        let mut dev = Storage::open(img.path()).unwrap();
        let (inquiry, csw) = exchange(&mut dev, 1, true, &[INQUIRY, 0, 0, 0, 36, 0], &[], 36);
        assert_eq!((inquiry.as_slice(), csw[12]), (&INQUIRY_DATA[..], 0));
        assert_eq!(&inquiry[8..32], b"UE2EMU  USB Disk Image  ");
        let (capacity, _) = exchange(&mut dev, 2, true, &[READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[], 8);
        assert_eq!(capacity, [0, 0, 0, 63, 0, 0, 2, 0]);
        let (_, csw) = exchange(&mut dev, 3, false, &[TEST_UNIT_READY, 0, 0, 0, 0, 0], &[], 0);
        assert_eq!(csw[12], 0);
        assert_eq!(dev.input(EP_IN, &mut [0; 13], 0), Reply::Nak, "nothing pending");
    }

    #[test]
    fn read_and_write_10_use_the_image() {
        let img = image(64);
        let mut dev = Storage::open(img.path()).unwrap();
        let (data, csw) = exchange(&mut dev, 7, true, &[READ_10, 0, 0, 0, 0, 5, 0, 0, 3, 0], &[], 1536);
        assert_eq!((csw[12], &data[..], &fs::read(img.path()).unwrap()[5 * 512..8 * 512]), (0, &data[..], &data[..]));
        assert_eq!(data[0], 5);

        let block: Vec<u8> = (0..1024).map(|i| (i * 7) as u8).collect();
        let (_, csw) = exchange(&mut dev, 8, false, &[WRITE_10, 0, 0, 0, 0, 62, 0, 0, 2, 0], &block, 0);
        assert_eq!(csw[12], 0);
        assert_eq!(&fs::read(img.path()).unwrap()[62 * 512..], &block[..]);
    }

    #[test]
    fn failures_report_through_request_sense() {
        let img = image(8);
        let mut dev = Storage::open(img.path()).unwrap();
        let (data, csw) = exchange(&mut dev, 1, true, &[READ_10, 0, 0, 0, 0, 7, 0, 0, 2, 0], &[], 1024);
        assert_eq!((data.len(), csw[12], u32::from_le_bytes(csw[8..12].try_into().unwrap())), (1024, 1, 1024));
        let (sense, _) = exchange(&mut dev, 2, true, &[REQUEST_SENSE, 0, 0, 0, 18, 0], &[], 18);
        assert_eq!((sense[2], sense[12], sense[13]), (0x05, 0x21, 0x00));
        let (sense, _) = exchange(&mut dev, 3, true, &[REQUEST_SENSE, 0, 0, 0, 18, 0], &[], 18);
        assert_eq!(sense[2], 0, "cleared after reading");
        let (_, csw) = exchange(&mut dev, 4, false, &[0x1B, 0, 0, 0, 1, 0], &[], 0);
        assert_eq!(csw[12], CSW_FAILED, "START STOP UNIT is not supported");
        assert_eq!(dev.output(EP_OUT, b"USBX"), Reply::Stall, "not a CBW");
    }

    #[test]
    fn read_only_image_is_write_protected() {
        let img = image(8);
        let mut perms = fs::metadata(img.path()).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(img.path(), perms).unwrap();
        let mut dev = Storage::open(img.path()).unwrap();
        let (_, csw) = exchange(&mut dev, 1, false, &[WRITE_10, 0, 0, 0, 0, 0, 0, 0, 1, 0], &[0xAA; 512], 0);
        assert_eq!(csw[12], CSW_FAILED);
        let (sense, _) = exchange(&mut dev, 2, true, &[REQUEST_SENSE, 0, 0, 0, 18, 0], &[], 18);
        assert_eq!((sense[2], sense[12]), (0x07, 0x27));
        assert_eq!(fs::read(img.path()).unwrap()[0], 0, "unchanged");
        assert!(Storage::open(NamedTempFile::new().unwrap().path()).is_err(), "empty image");
    }
}
