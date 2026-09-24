//! The FPGA's UDP stream generators: `U64_UDP_BASE`, and the datagrams they send.
//! Spec: docs/specs/S24-udp-streams.md
//!
//! The firmware never opens a socket for these. It writes a finished 42-byte Ethernet/IP/UDP header per stream
//! into this window (`data_streamer.cc:309-404`) and sets the stream's bit in `ETHSTREAM_ENA`; the generator in
//! the FPGA then sends datagrams by itself, with that header in front of every one. Three fields of the template
//! belong to the packet and not to the header, and the firmware leaves them as placeholders for the generator to
//! finish: the IP total length, the UDP length and the IP header checksum.
//!
//! The application header in front of a VIC datagram is the generator's own; no firmware source describes it, so
//! the receiver is the specification (`tests/e2e/lib/streams.py:262-300`, S24 §3) and every field of it is fixed
//! rather than negotiated.

use std::collections::VecDeque;

use crate::c64host::C64Frame;
use crate::io::{IoCtx, IoDevice, IoMap};
use crate::machine::MachineConfig;

/// `U64_UDP_BASE` (u64.h:58).
pub const BASE: u32 = 0x1019_0000;
/// Four streams of 64 bytes; the window the firmware decodes is a page.
pub const SIZE: u32 = 0x100;

/// Stream ids, as `ETHSTREAM_ENA`'s low nibble numbers them (u64.h:80).
pub const VIC: usize = 0;
pub const AUDIO: usize = 1;
const STREAMS: usize = 4;
/// Bytes of the wire header the firmware writes per stream.
const TEMPLATE: usize = 42;
/// Register bytes per stream.
const STRIDE: usize = 64;

// --- offsets inside the template, all of them the firmware's (data_streamer.cc:311-330) ---
/// IP total length, big-endian: a placeholder of 28 in the template.
const IP_LEN: usize = 16;
/// IP header checksum, computed by the firmware over the placeholder length.
const IP_SUM: usize = 24;
/// UDP length, big-endian: a placeholder of 8.
const UDP_LEN: usize = 38;
/// First byte of the IP header inside the Ethernet frame.
const IP_START: usize = 14;
/// IP header bytes (no options).
const IP_HEADER: usize = 20;
/// UDP header bytes.
const UDP_HEADER: usize = 8;

// --- the VIC stream's wire format (streams.py:262-300) ---
/// Pixels per line, fixed by the encoder.
pub const PIXELS_PER_LINE: usize = 384;
/// Lines in one datagram.
pub const LINES_PER_PACKET: usize = 4;
/// Two pixels per byte.
const BYTES_PER_LINE: usize = PIXELS_PER_LINE / 2;
/// 4 lines × 192 bytes.
const VIDEO_PAYLOAD: usize = LINES_PER_PACKET * BYTES_PER_LINE;
/// Application header bytes in front of a VIC payload.
const VIDEO_HEADER: usize = 12;
/// Lines a frame has on the wire: 272 under PAL, 240 under NTSC, the receiver's two heights (streams.py:281-286).
const HEIGHT_PAL: usize = 272;
const HEIGHT_NTSC: usize = 240;
/// Bit 15 of the line field marks a frame's last datagram.
const LAST_PACKET: u16 = 0x8000;

// --- the audio stream's wire format (streams.py:613-646) ---
/// Interleaved stereo s16le sample frames per datagram.
const AUDIO_FRAMES_PER_PACKET: usize = 192;
/// Two channels of `i16`.
const AUDIO_FRAME_BYTES: usize = 4;
/// 192 × 4, behind a 2-byte sequence.
const AUDIO_PAYLOAD: usize = AUDIO_FRAMES_PER_PACKET * AUDIO_FRAME_BYTES;

/// How many datagrams may wait for the host. One PAL frame is 68, and a host that stops draining is a host that
/// has gone away: the generator drops rather than grows, as a wire does.
const QUEUE_CAP: usize = 256;

/// The stream generators: the templates the firmware wrote, the counters, and the datagrams waiting to go out.
pub struct Streams {
    regs: [[u8; STRIDE]; STREAMS],
    /// Complete Ethernet frames, oldest first.
    queue: VecDeque<Vec<u8>>,
    /// Per-stream 16-bit sequence counter, wrapping, as the receiver expects (`streams.py:112`).
    seq: [u16; STREAMS],
    /// The VIC stream's 16-bit frame counter, wrapping and independent of `seq`.
    frame_no: u16,
    /// Samples handed over but not yet a full datagram.
    pcm: Vec<i16>,
    dropped: u64,
}

impl Default for Streams {
    fn default() -> Self {
        Self::new()
    }
}

impl Streams {
    pub fn new() -> Self {
        Streams {
            regs: [[0; STRIDE]; STREAMS],
            queue: VecDeque::new(),
            seq: [0; STREAMS],
            frame_no: 0,
            pcm: Vec::new(),
            dropped: 0,
        }
    }

    /// The 42-byte wire header of stream `id`, or None while the firmware has not written one. A template whose
    /// destination IP is zero is not one: the firmware only writes a stream it is about to enable.
    pub fn template(&self, id: usize) -> Option<&[u8]> {
        let regs = self.regs.get(id)?;
        let dest_ip = &regs[30..34];
        (dest_ip != [0, 0, 0, 0]).then_some(&regs[..TEMPLATE])
    }

    /// One VIC frame as datagrams: 68 for 272 PAL lines, 60 for 240 NTSC lines (S34). The NTSC canvas has 247 lines;
    /// the 240 sent are its middle ones, which puts the text window where it is on the PAL stream relative to the
    /// borders. Indices outside the frame read as 0, so a frame of another size still produces a well-formed stream.
    pub fn send_frame(&mut self, frame: &C64Frame) {
        let Some(template) = self.template(VIC).map(<[u8]>::to_vec) else { return };
        let (height, top) = if frame.height >= HEIGHT_PAL {
            (HEIGHT_PAL, 0)
        } else {
            let height = HEIGHT_NTSC.min(frame.height);
            (height, (frame.height - height) / 2)
        };
        let mut line = 0;
        while line < height {
            let last = line + LINES_PER_PACKET >= height;
            let mut packet = Vec::with_capacity(VIDEO_HEADER + VIDEO_PAYLOAD);
            let line_field = line as u16 | if last { LAST_PACKET } else { 0 };
            packet.extend_from_slice(&self.seq[VIC].to_le_bytes());
            packet.extend_from_slice(&self.frame_no.to_le_bytes());
            packet.extend_from_slice(&line_field.to_le_bytes());
            packet.extend_from_slice(&(PIXELS_PER_LINE as u16).to_le_bytes());
            packet.push(LINES_PER_PACKET as u8);
            packet.push(4); // bits per pixel
            packet.extend_from_slice(&0u16.to_le_bytes()); // encoding
            for y in line..line + LINES_PER_PACKET {
                for x in (0..PIXELS_PER_LINE).step_by(2) {
                    let px = |x: usize| -> u8 {
                        if y >= height || x >= frame.width {
                            return 0;
                        }
                        frame.indices.get((top + y) * frame.width + x).copied().unwrap_or(0) & 0x0F
                    };
                    // Low nibble first: the even pixel is the low half of the byte (streams.py:250-256).
                    packet.push(px(x) | (px(x + 1) << 4));
                }
            }
            self.seq[VIC] = self.seq[VIC].wrapping_add(1);
            self.push(&template, &packet);
            line += LINES_PER_PACKET;
        }
        self.frame_no = self.frame_no.wrapping_add(1);
    }

    /// Samples for the audio stream, as many whole datagrams as they fill. What is left over waits for the next
    /// call, so a caller may hand over any number of samples.
    ///
    /// The samples arrive as stereo frames, left then right (S29), which is the stream's own layout.
    pub fn send_audio(&mut self, pcm: &[i16]) {
        let Some(template) = self.template(AUDIO).map(<[u8]>::to_vec) else {
            self.pcm.clear();
            return;
        };
        self.pcm.extend_from_slice(pcm);
        let per_packet = AUDIO_FRAMES_PER_PACKET * 2;
        while self.pcm.len() >= per_packet {
            let mut packet = Vec::with_capacity(2 + AUDIO_PAYLOAD);
            packet.extend_from_slice(&self.seq[AUDIO].to_le_bytes());
            for sample in &self.pcm[..per_packet] {
                packet.extend_from_slice(&sample.to_le_bytes());
            }
            self.pcm.drain(..per_packet);
            self.seq[AUDIO] = self.seq[AUDIO].wrapping_add(1);
            self.push(&template, &packet);
        }
    }

    /// The template in front of `payload`, with the three fields the generator owns filled in.
    fn push(&mut self, template: &[u8], payload: &[u8]) {
        if self.queue.len() >= QUEUE_CAP {
            self.queue.pop_front();
            self.dropped += 1;
        }
        let mut frame = Vec::with_capacity(TEMPLATE + payload.len());
        frame.extend_from_slice(template);
        frame.extend_from_slice(payload);
        let ip_total = (IP_HEADER + UDP_HEADER + payload.len()) as u16;
        frame[IP_LEN..IP_LEN + 2].copy_from_slice(&ip_total.to_be_bytes());
        let udp_total = (UDP_HEADER + payload.len()) as u16;
        frame[UDP_LEN..UDP_LEN + 2].copy_from_slice(&udp_total.to_be_bytes());
        frame[IP_SUM..IP_SUM + 2].copy_from_slice(&[0, 0]);
        let sum = ip_checksum(&frame[IP_START..IP_START + IP_HEADER]);
        frame[IP_SUM..IP_SUM + 2].copy_from_slice(&sum.to_be_bytes());
        self.queue.push_back(frame);
    }

    /// Every datagram waiting, oldest first, and the queue is empty afterwards.
    pub fn take(&mut self) -> Vec<Vec<u8>> {
        self.queue.drain(..).collect()
    }

    /// Datagrams dropped because the host did not drain the queue.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// The one's-complement sum of an IP header, as the firmware computes it (`data_streamer.cc:386-399`).
fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for pair in header.chunks(2) {
        let word = u32::from(pair[0]) << 8 | u32::from(*pair.get(1).unwrap_or(&0));
        sum += word;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

impl IoDevice for Streams {
    fn name(&self) -> &'static str {
        "udp-streams"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    fn write8(&mut self, off: u32, val: u8, _ctx: &mut IoCtx) {
        let (id, byte) = (off as usize / STRIDE, off as usize % STRIDE);
        if let Some(regs) = self.regs.get_mut(id) {
            regs[byte] = val;
        }
    }

    fn peek8(&self, off: u32) -> u8 {
        let (id, byte) = (off as usize / STRIDE, off as usize % STRIDE);
        self.regs.get(id).map_or(0, |regs| regs[byte])
    }

    fn reset(&mut self) {
        *self = Streams::new();
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(BASE, SIZE, Box::new(Streams::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A template like the firmware's, with a destination and the placeholder lengths it leaves behind.
    fn template() -> Vec<u8> {
        let mut t = vec![0u8; TEMPLATE];
        t[..6].copy_from_slice(&[0x01, 0x00, 0x5E, 0x00, 0x01, 0x40]); // multicast MAC
        t[6..12].copy_from_slice(&[0x00, 0x15, 0x41, 0xAA, 0xAA, 0x01]);
        t[12..14].copy_from_slice(&[0x08, 0x00]);
        t[14] = 0x45;
        t[IP_LEN..IP_LEN + 2].copy_from_slice(&28u16.to_be_bytes());
        t[22..24].copy_from_slice(&[0x40, 0x11]);
        t[26..30].copy_from_slice(&[10, 0, 2, 15]);
        t[30..34].copy_from_slice(&[239, 0, 1, 64]);
        t[34..36].copy_from_slice(&53248u16.to_be_bytes());
        t[36..38].copy_from_slice(&11000u16.to_be_bytes());
        t[UDP_LEN..UDP_LEN + 2].copy_from_slice(&8u16.to_be_bytes());
        t
    }

    fn armed(id: usize) -> Streams {
        let mut s = Streams::new();
        let t = template();
        for (i, byte) in t.iter().enumerate() {
            s.regs[id][i] = *byte;
        }
        s
    }

    fn frame(width: usize, height: usize) -> C64Frame {
        let indices = (0..width * height).map(|i| (i % 16) as u8).collect();
        C64Frame { width, height, indices, palette: [0; 16], screen: vec![0x20; 1000], ..C64Frame::default() }
    }

    #[test]
    fn a_template_is_only_a_template_once_it_has_a_destination() {
        let mut s = Streams::new();
        assert!(s.template(VIC).is_none(), "nothing written yet");
        s.send_frame(&frame(384, 272));
        assert!(s.take().is_empty(), "no datagram without a destination");
        let s = armed(VIC);
        assert_eq!(s.template(VIC).unwrap().len(), TEMPLATE);
        assert!(s.template(AUDIO).is_none(), "the other streams are untouched");
    }

    #[test]
    fn one_pal_frame_is_68_datagrams_of_780_bytes() {
        let mut s = armed(VIC);
        s.send_frame(&frame(384, 272));
        let out = s.take();
        assert_eq!(out.len(), 68, "272 lines / 4 per datagram");
        for frame in &out {
            assert_eq!(frame.len(), TEMPLATE + VIDEO_HEADER + VIDEO_PAYLOAD, "822 bytes on the wire");
        }
        // The application header of the first and the last datagram.
        let head = |f: &Vec<u8>| -> (u16, u16, u16, u16, u8, u8, u16) {
            let h = &f[TEMPLATE..TEMPLATE + VIDEO_HEADER];
            let w = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
            (w(0), w(2), w(4), w(6), h[8], h[9], w(10))
        };
        assert_eq!(head(&out[0]), (0, 0, 0, 384, 4, 4, 0), "seq 0, frame 0, line 0, then the fixed fields");
        assert_eq!(head(&out[67]), (67, 0, 268 | LAST_PACKET, 384, 4, 4, 0), "the last packet carries bit 15");
        // Both counters advance the way the receiver expects.
        s.send_frame(&frame(384, 272));
        let next = s.take();
        assert_eq!(head(&next[0]), (68, 1, 0, 384, 4, 4, 0), "seq runs on, frame counts frames");
    }

    #[test]
    fn two_pixels_per_byte_low_nibble_first() {
        let mut s = armed(VIC);
        let mut f = frame(384, 272);
        (f.indices[0], f.indices[1], f.indices[2]) = (0x0A, 0x0B, 0x0C);
        s.send_frame(&f);
        let out = s.take();
        let payload = &out[0][TEMPLATE + VIDEO_HEADER..];
        assert_eq!(payload[0], 0xBA, "pixel 0 in the low nibble, pixel 1 in the high");
        assert_eq!(payload[1] & 0x0F, 0x0C, "pixel 2 is the next byte's low nibble");
    }

    #[test]
    fn the_generator_finishes_the_three_fields_the_firmware_left_open() {
        let mut s = armed(VIC);
        s.send_frame(&frame(384, 272));
        let out = s.take();
        let f = &out[0];
        let be = |i: usize| u16::from_be_bytes([f[i], f[i + 1]]);
        assert_eq!(be(IP_LEN), (IP_HEADER + UDP_HEADER + VIDEO_HEADER + VIDEO_PAYLOAD) as u16, "IP total length");
        assert_eq!(be(UDP_LEN), (UDP_HEADER + VIDEO_HEADER + VIDEO_PAYLOAD) as u16, "UDP length");
        // The checksum is the one's complement of the header's sum, so summing the header including it gives 0xFFFF.
        let mut sum: u32 = 0;
        for pair in f[IP_START..IP_START + IP_HEADER].chunks(2) {
            sum += u32::from(pair[0]) << 8 | u32::from(pair[1]);
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        assert_eq!(sum, 0xFFFF, "the IP header checksum covers the length the generator wrote");
        assert_ne!(be(IP_SUM), 0, "and it is not the placeholder");
    }

    /// S34: an NTSC frame (247 canvas lines) goes out as 240 lines, 60 datagrams, the middle ones.
    #[test]
    fn an_ntsc_frame_is_240_lines() {
        let mut s = armed(VIC);
        let mut f = frame(384, 247);
        for (i, px) in f.indices.iter_mut().enumerate() {
            *px = (i / 384 % 16) as u8;
        }
        s.send_frame(&f);
        let out = s.take();
        assert_eq!(out.len(), 60);
        let line = |p: &[u8]| u16::from_le_bytes([p[TEMPLATE + 4], p[TEMPLATE + 5]]);
        assert_eq!((line(&out[0]), line(&out[59])), (0, 236 | LAST_PACKET), "the last datagram says 240 lines");
        assert_eq!(out[0][TEMPLATE + VIDEO_HEADER], 3 | 3 << 4, "the first line sent is canvas line 3");
    }

    #[test]
    fn audio_is_770_bytes_of_192_stereo_frames() {
        let mut s = armed(AUDIO);
        // Stereo frames in: 192 frames are 384 samples.
        s.send_audio(&vec![0; 100]);
        assert!(s.take().is_empty(), "a partial datagram waits for the rest");
        s.send_audio(&vec![0x1234; 384 - 100 + 8]);
        let out = s.take();
        assert_eq!(out.len(), 1, "one whole datagram, the remainder held back");
        assert_eq!(out[0].len(), TEMPLATE + 2 + AUDIO_PAYLOAD, "812 bytes on the wire, 770 of them UDP payload");
        assert_eq!(&out[0][TEMPLATE..TEMPLATE + 2], &0u16.to_le_bytes(), "the audio stream's own sequence");
        let last = &out[0][out[0].len() - 2..];
        assert_eq!(i16::from_le_bytes([last[0], last[1]]), 0x1234, "s16 little endian");
    }

    #[test]
    fn the_window_keeps_four_templates_and_reads_back() {
        let mut s = Streams::new();
        let mut ram = vec![0u8; 0x100];
        let mut irq = crate::irq::IrqState::default();
        let mut console = Vec::new();
        let mut ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut ram, irq: &mut irq, console: &mut console };
        for id in 0..STREAMS {
            s.write8((id * STRIDE) as u32, 0x40 + id as u8, &mut ctx);
        }
        for id in 0..STREAMS {
            assert_eq!(s.read8((id * STRIDE) as u32, &mut ctx), 0x40 + id as u8, "stream {id} has its own 64 bytes");
            assert_eq!(s.peek8((id * STRIDE) as u32), 0x40 + id as u8, "and a debugger sees the same");
        }
    }

    #[test]
    fn a_host_that_stops_draining_loses_the_oldest() {
        let mut s = armed(VIC);
        for _ in 0..5 {
            s.send_frame(&frame(384, 272));
        }
        assert_eq!(s.take().len(), QUEUE_CAP, "the queue is a wire, not a buffer");
        assert_eq!(s.dropped(), 5 * 68 - QUEUE_CAP as u64);
    }
}
