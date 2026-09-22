//! RMII Ethernet MAC 0x10060800 and the MDIO PHY behind the U2PIO pins (docs/hw/08-network-rmii.md tier T1,
//! 00-memory-map §2 C24). Spec: docs/specs/S11-S14-later.md §S12.
//!
//! The MAC is the open `ethernet_rmii` block at frame level: `eth_filter` (RX filter + DMA write), `eth_transmit`
//! (TX DMA read) and `free_queue` (buffer-ID FIFOs). Address bits 5:4 select the sub-block, bits 3:0 the register
//! (ethernet_rmii.vhd:96-111); unlisted reads return 0 (08 §Address map). Frames cross to the host in
//! [`Rmii::exchange`]. The [`Phy`] sits in the U2PIO page (`devices::board::U2pio`); its link bit is the "cable".

use std::collections::VecDeque;

use crate::bus::RAM_MASK;
use crate::host::NetBackend;
use crate::io::{IoCtx, IoDevice, IoMap, IO_GRAIN};
use crate::irq::IrqState;
use crate::machine::MachineConfig;

/// `RMII_BASE` (iomap.h:31).
pub const RMII_BASE: u32 = 0x1006_0800;
/// `ITU_INTERRUPT_RMIIRX` = `free_queue.io_irq` = `used_valid` (itu.h:38; free_queue.vhd:86). Level (08 H10).
pub const RX_IRQ_BIT: u8 = 5;
/// `ITU_INTERRUPT_RMIITX` = `eth_transmit.io_irq` (itu.h:39). Level; never enabled by the firmware (08 §Interrupts).
pub const TX_IRQ_BIT: u8 = 6;
/// `CAPAB_ETH_RMII` (itu.h:73): without it `RmiiInterface` never touches the MAC or MDIO (rmii_interface.cc:50, 08 H1).
pub const CAPAB_ETH_RMII: u32 = 0x0100_0000;

// eth_filter registers (eth_filter.vhd:215-229). Offsets 0..=5 are the own MAC.
const RX_PROMISC: u32 = 0x07;
const RX_ENABLE: u32 = 0x08;
// eth_transmit registers (eth_transmit.vhd:152-191). 0x10..=0x13 is the TX address.
const TX_LEN_LO: u32 = 0x14;
const TX_LEN_HI: u32 = 0x15;
const TX_START: u32 = 0x18;
const TX_IRQACK: u32 = 0x19;
// free_queue registers (free_queue.vhd:114-167). 0x21..=0x23 is the pool base.
const FREE_PUT: u32 = 0x24;
const ALLOC_ID: u32 = 0x28;
const ALLOC_SIZE_LO: u32 = 0x2A;
const ALLOC_SIZE_HI: u32 = 0x2B;
const FREE_RESET: u32 = 0x2E;
/// Write: `RMII_ALLOC_POP`. Read: `RMII_ALLOC_VALID` (rmii_interface.h:24-25).
const ALLOC_POP: u32 = 0x2F;

/// 26-bit DMA addresses: TX `sw_addr` 25:0 and the pool base (eth_transmit.vhd:155-162, free_queue.vhd:120-125).
const DMA_MASK: u32 = 0x03FF_FFFF;
/// `g_block_size` of the pool (ethernet_rmii.vhd:160).
const BLOCK_SIZE: u32 = 1536;
/// Frame byte 0 lands at block+2: the first DMA word holds 2 stale bytes (eth_filter.vhd:303-312;
/// rmii_interface.cc:244).
const FRAME_OFFSET: u32 = 2;
/// Longest frame without FCS: the stream (frame + FCS + status byte) overflows past 1535 bytes (eth_filter.vhd:95,146).
pub const MAX_RX_FRAME: usize = 1530;
/// `free_queue` table halves: 7-bit head/tail pointers (free_queue.vhd:59-62).
const RING_LEN: usize = 128;
const PTR_MASK: u8 = RING_LEN as u8 - 1;
/// Guest frames kept for a host that has not taken them; later ones are lost like on an unplugged wire.
const TX_QUEUE_LIMIT: usize = 256;

/// One half of the `free_queue` table. head == tail is empty, head + 1 == tail is full (free_queue.vhd:232-236).
struct Ring<T> {
    slots: [T; RING_LEN],
    head: u8,
    tail: u8,
}

impl<T: Copy + Default> Ring<T> {
    fn new() -> Self {
        Ring { slots: [T::default(); RING_LEN], head: 0, tail: 0 }
    }

    fn is_full(&self) -> bool {
        self.head.wrapping_add(1) & PTR_MASK == self.tail
    }

    /// Write at head without a full check; FREE_PUT checks first, the USED push never does (free_queue.vhd:240-257).
    fn push(&mut self, val: T) {
        self.slots[usize::from(self.head)] = val;
        self.head = self.head.wrapping_add(1) & PTR_MASK;
    }

    fn pop(&mut self) -> Option<T> {
        (self.head != self.tail).then(|| {
            let val = self.slots[usize::from(self.tail)];
            self.tail = self.tail.wrapping_add(1) & PTR_MASK;
            val
        })
    }

    /// How many entries stand between tail and head.
    fn len(&self) -> usize {
        usize::from(self.head.wrapping_sub(self.tail) & PTR_MASK)
    }

    /// `soft_reset`: both pointers to 0 (free_queue.vhd:310-317).
    fn clear(&mut self) {
        (self.head, self.tail) = (0, 0);
    }
}

/// The MAC. DMA transfers complete inside the access that starts them:
/// - TX copies on `RMII_TX_START`, so `RMII_TX_BUSY` (0x1A) always reads 0 (08 H7).
/// - A host frame is filtered, written and queued in one step (08 T1 §4), and the next used entry is presented
///   as soon as `RMII_ALLOC_POP` clears the current one (08 T1 §5).
pub struct Rmii {
    /// `my_mac` (eth_filter.vhd:218-219).
    mac: [u8; 6],
    promiscuous: bool,
    rx_enable: bool,
    /// `address_valid`: the block taken by a frame the filter rejected or that overflowed. The next frame reuses it
    /// instead of allocating (eth_filter.vhd:263-286,330-331); disabling RX loses it (:366-372, 08 T1 §7).
    held: Option<u8>,
    /// `sw_addr` 25:0 and `sw_length` 11:0 (eth_transmit.vhd:155-166).
    tx_addr: u32,
    tx_len: u16,
    /// `io_irq` of eth_transmit: set when a copy is done, cleared by `RMII_TX_IRQACK` (eth_transmit.vhd:170-181).
    tx_irq: bool,
    /// Pool base `sw_addr` 25:8; bits 7:0 are never written (free_queue.vhd:118-125).
    pool_base: u32,
    /// FREE side: buffer IDs pushed by `RMII_FREE_PUT`.
    free: Ring<u8>,
    /// USED side: (id, frame size) pushed by the RX filter.
    used: Ring<(u8, u16)>,
    /// Presentation register: `used_valid`, `sw_pop_id` (0xFF after a pop), `sw_pop_size` (free_queue.vhd:80-86).
    used_valid: bool,
    pop_id: u8,
    pop_size: u16,
    /// Frames the guest transmitted that the host has not taken yet, oldest first.
    to_host: VecDeque<Vec<u8>>,
}

impl Default for Rmii {
    fn default() -> Self {
        Self::new()
    }
}

impl Rmii {
    /// Power-on state: RX disabled, promiscuous off, empty FIFOs (eth_filter.vhd:240-243, free_queue.vhd:80,169-173).
    pub fn new() -> Self {
        Rmii {
            mac: [0; 6],
            promiscuous: false,
            rx_enable: false,
            held: None,
            tx_addr: 0,
            tx_len: 0,
            tx_irq: false,
            pool_base: 0,
            free: Ring::new(),
            used: Ring::new(),
            used_valid: false,
            pop_id: 0,
            pop_size: 0,
            to_host: VecDeque::new(),
        }
    }

    /// The filter and the queues as the firmware left them, for the monitor's `net` verb (S23 §5): the MAC the
    /// firmware programmed, whether RX is on, and how much is in flight.
    pub fn summary(&self) -> String {
        let mac = self.mac.map(|b| format!("{b:02x}")).join(":");
        let unset = if self.mac == [0; 6] { "  (the firmware has not programmed one)" } else { "" };
        let mut out = format!("  mac           {mac}{unset}\n");
        out.push_str(&format!(
            "  rx            {}{}\n",
            if self.rx_enable { "on" } else { "off" },
            if self.promiscuous { ", promiscuous" } else { "" },
        ));
        out.push_str(&format!("  free buffers  {}\n", self.free.len()));
        out.push_str(&format!("  received      {} waiting for the firmware\n", self.used.len()));
        out.push_str(&format!("  to the host   {} frame(s) not taken yet\n", self.to_host.len()));
        out.push_str(&format!(
            "  tx            {:#x} + {} bytes, irq {}\n",
            self.tx_addr,
            self.tx_len,
            if self.tx_irq { "yes" } else { "no" },
        ));
        out
    }

    /// One host round trip: hand the frames transmitted since the last call to `net`, then deliver the frames it
    /// has for the guest. `ram` and `irq` are the machine's DDR and ITU core (the MAC is a DMA master and drives
    /// ITU bits 5/6).
    pub fn exchange(&mut self, net: &mut dyn NetBackend, ram: &mut [u8], irq: &mut IrqState) {
        for frame in self.to_host.drain(..) {
            net.send(&frame);
        }
        net.poll(&mut |frame| self.receive(frame, ram, irq));
    }

    /// Host → guest frame, Ethernet without FCS (08 T1 §4). Dropped while RX is disabled or when no buffer ID is
    /// free (eth_filter.vhd:125,279-280). An oversized or filtered frame keeps its block for the next frame.
    /// Otherwise the frame is written at `block + 2`, queued as (id, size) and presented; the RX IRQ level
    /// follows `used_valid` (08 H5).
    pub fn receive(&mut self, frame: &[u8], ram: &mut [u8], irq: &mut IrqState) {
        if !self.rx_enable {
            return;
        }
        let Some(id) = self.held.take().or_else(|| self.free.pop()) else { return };
        if frame.len() > MAX_RX_FRAME || !self.for_me(frame) {
            self.held = Some(id);
            return;
        }
        // `{base[25:8], 0x00} + 1536 * id` (free_queue.vhd:282-288).
        let block = self.pool_base.wrapping_add(BLOCK_SIZE * u32::from(id));
        for (i, &byte) in (0u32..).zip(frame) {
            ram[(block.wrapping_add(FRAME_OFFSET + i) & RAM_MASK) as usize] = byte;
        }
        self.used.push((id, frame.len() as u16));
        self.present();
        self.update_irq(irq);
    }

    /// Destination filter: each 16-bit word of the destination MAC is 0xFFFF or the own MAC word
    /// (eth_filter.vhd:247-250,296-301), unless promiscuous (:306,327). Unicast-to-me and broadcast pass; multicast
    /// is dropped (08 §RX path 3).
    fn for_me(&self, frame: &[u8]) -> bool {
        self.promiscuous
            || frame.chunks_exact(2).zip(self.mac.chunks_exact(2)).all(|(dst, own)| dst == [0xFF; 2] || dst == own)
    }

    /// Fall-through: while nothing is presented, the oldest USED entry moves into the presentation register
    /// (free_queue.vhd:99-111,268-274,292-295).
    fn present(&mut self) {
        if !self.used_valid {
            if let Some((id, size)) = self.used.pop() {
                (self.pop_id, self.pop_size, self.used_valid) = (id, size, true);
            }
        }
    }

    /// `RMII_TX_START`: `sw_length` bytes from `sw_addr` go onto the wire, then busy drops and the TX IRQ rises
    /// (eth_transmit.vhd:167-181,211-263). A length of 0 underflows the 12-bit down counter into 4096 bytes
    /// (:212,246,261). The copy is synchronous, so the firmware's early free of the buffer is harmless (08 §TX path).
    fn transmit(&mut self, ram: &[u8]) {
        let len = if self.tx_len == 0 { 4096 } else { u32::from(self.tx_len) };
        let frame = (0..len).map(|i| ram[(self.tx_addr.wrapping_add(i) & RAM_MASK) as usize]).collect();
        if self.to_host.len() < TX_QUEUE_LIMIT {
            self.to_host.push_back(frame);
        }
        self.tx_irq = true;
    }

    fn update_irq(&self, irq: &mut IrqState) {
        irq.set_level(RX_IRQ_BIT, self.used_valid);
        irq.set_level(TX_IRQ_BIT, self.tx_irq);
    }
}

/// Replace byte `idx` (0 = LSB) of a 26-bit DMA address register.
fn set_dma_byte(reg: u32, idx: u32, val: u8) -> u32 {
    let shift = idx * 8;
    ((reg & !(0xFF << shift)) | (u32::from(val) << shift)) & DMA_MASK
}

impl IoDevice for Rmii {
    fn name(&self) -> &'static str {
        "rmii"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    /// Multi-byte registers arrive as LE bytes (bus_converter.vhd:159-178); only single-byte registers have side
    /// effects (08 §Address map).
    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        match off & 0x3F {
            o @ 0x00..=0x05 => self.mac[o as usize] = val,
            RX_PROMISC => self.promiscuous = val & 1 != 0,
            RX_ENABLE => {
                self.rx_enable = val & 1 != 0;
                if !self.rx_enable {
                    self.held = None;
                }
            }
            o @ 0x10..=0x13 => self.tx_addr = set_dma_byte(self.tx_addr, o - 0x10, val),
            TX_LEN_LO => self.tx_len = (self.tx_len & 0x0F00) | u16::from(val),
            TX_LEN_HI => self.tx_len = (self.tx_len & 0x00FF) | (u16::from(val & 0x0F) << 8),
            TX_START => self.transmit(ctx.ram),
            TX_IRQACK => self.tx_irq = false,
            o @ 0x21..=0x23 => self.pool_base = set_dma_byte(self.pool_base, o - 0x20, val),
            FREE_PUT if !self.free.is_full() => self.free.push(val),
            FREE_RESET => {
                self.free.clear();
                self.used.clear();
            }
            ALLOC_POP if self.used_valid => (self.used_valid, self.pop_id) = (false, 0xFF),
            _ => {}
        }
        self.present();
        self.update_irq(ctx.irq);
    }

    /// Reads have no side effects (free_queue.vhd:141-167; eth_filter.vhd:232-237; eth_transmit.vhd:183-191).
    fn peek8(&self, off: u32) -> u8 {
        match off & 0x3F {
            // bit0 insert pending (always done), bit1 free_full.
            FREE_PUT => u8::from(self.free.is_full()) << 1,
            ALLOC_ID => self.pop_id,
            ALLOC_SIZE_LO => self.pop_size as u8,
            ALLOC_SIZE_HI => (self.pop_size >> 8) as u8,
            ALLOC_POP => u8::from(self.used_valid),
            _ => 0,
        }
    }

    fn reset(&mut self) {
        *self = Rmii::new();
    }

    crate::impl_as_any!();
}

/// Maps the MAC window 0x10060800-0x100608FF; the 64-byte block repeats.
pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(RMII_BASE, IO_GRAIN, Box::new(Rmii::new()));
}

/// Clause 22 address of the modelled PHY; the driver probes 0 before 3 (rmii_interface.cc:59-69, 08 H2).
const PHY_ADDR: u8 = 0;
/// Preamble ones required before a start of frame; `mdio.c` sends 33 (mdio.c:51-53,83-85).
const PREAMBLE_MIN: u8 = 32;
/// ST (2) + OP (2) + PHYAD (5) + REGAD (5) (mdio.c:20-21).
const HEADER_BITS: u8 = 14;
/// TA "10" + 16 data bits after a write header (mdio.c:71-77).
const WRITE_BITS: u8 = 18;
const OP_READ: u16 = 0b10;
const OP_WRITE: u16 = 0b01;

// Registers: IEEE 802.3 clause 22 plus the KSZ8081-class vendor block implied by the writes to 0x16/0x1B (08 Q4).
const BMCR: u8 = 0x00;
const BMSR: u8 = 0x01;
const PHYID1: u8 = 0x02;
const PHYID2: u8 = 0x03;
const ANAR: u8 = 0x04;
const ANLPAR: u8 = 0x05;
const PHYCTRL1: u8 = 0x1E;
/// BMCR reset: 100 Mbit, autonegotiation enabled, full duplex.
const BMCR_RESET: u16 = 0x3100;
/// BMCR reset (15) and restart autonegotiation (9) self-clear; both complete at once here.
const BMCR_SELF_CLEAR: u16 = 0x8200;
/// PHY identifier: Micrel/Microchip OUI the driver checks (rmii_interface.cc:60-61), KSZ8081 model/revision.
const PHYID1_VALUE: u16 = 0x0022;
const PHYID2_VALUE: u16 = 0x1561;
/// ANAR as the driver programs it: 100/10 full/half duplex (rmii_interface.cc:71).
const ANAR_RESET: u16 = 0x01E1;
/// BMSR: 100/10 FD/HD capable, preamble suppression, AN able, extended. Link adds AN complete + link status (08 H3).
const BMSR_LINK_DOWN: u16 = 0x7849;
const BMSR_LINK_UP: u16 = 0x786D;
/// Link partner: acknowledge + 100/10 FD/HD.
const ANLPAR_LINK_UP: u16 = 0x41E1;
/// PHY Control 1: link status (8) + operation mode 110 = 100BASE-TX full duplex.
const PHYCTRL1_LINK_UP: u16 = 0x0106;

/// Position of the MDIO decoder in a clause 22 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mdio {
    /// Consecutive preamble ones seen.
    Preamble(u8),
    /// ST, OP, PHYAD and REGAD shifted in, `n` bits so far.
    Header { bits: u16, n: u8 },
    /// Read of `value` addressed to this PHY; the host's single TA bit comes next (mdio.c:103).
    Turnaround(u16),
    /// Read data phase: `left` bits of `value` still to drive, MSB first.
    Read { value: u16, left: u8 },
    /// Write to `phy`/`reg`: TA + data bits shifted in, `n` so far.
    Write { phy: u8, reg: u8, bits: u32, n: u8 },
}

/// Ethernet PHY on the bit-banged MDIO pins U2PIO_SET_MDC 0x1010000A, U2PIO_SET_MDIO 0x1010000B and
/// U2PIO_GET_MDIO 0x10100006 (u2p.h:66-68, 08 §MDIO). The decoder samples the last MDIO write on each MDC 0→1
/// write. A read addressed to PHY 0 drives TA 0 after the host's TA bit and D15..D0 after the next 16 rising
/// edges, so `mdio_read` samples each bit after its `MDC=0` write (mdio.c:103-113). Other PHY addresses never
/// drive and read 0xFFFF.
pub struct Phy {
    /// Cable present: reported through BMSR/ANLPAR/PHYCTRL1 (08 H3-H4, T1 §8).
    link: bool,
    regs: [u16; 32],
    /// Last U2PIO_SET_MDC level.
    mdc: bool,
    /// Last U2PIO_SET_MDIO value: false drives the line low, true releases it (mdio.c:13-16).
    host: bool,
    /// Level the PHY drives, if any.
    drive: Option<bool>,
    frame: Mdio,
}

impl Default for Phy {
    fn default() -> Self {
        Self::new()
    }
}

impl Phy {
    /// Power-on: no cable, registers at reset, both pins released.
    pub fn new() -> Self {
        let mut regs = [0; 32];
        regs[usize::from(BMCR)] = BMCR_RESET;
        regs[usize::from(ANAR)] = ANAR_RESET;
        Phy { link: false, regs, mdc: false, host: true, drive: None, frame: Mdio::Preamble(0) }
    }

    /// Plug or unplug the cable. The RMII task sees it within 250 ms while down and 1 s while up
    /// (rmii_interface.cc:154-186, 08 H3-H4).
    pub fn set_link(&mut self, up: bool) {
        self.link = up;
    }

    /// Power-on state with the cable left as it is.
    pub fn reset(&mut self) {
        *self = Phy { link: self.link, ..Phy::new() };
    }

    /// U2PIO_SET_MDC write.
    pub fn set_mdc(&mut self, level: bool) {
        if level && !self.mdc {
            self.rising_edge();
        }
        self.mdc = level;
    }

    /// U2PIO_SET_MDIO write.
    pub fn set_mdio(&mut self, level: bool) {
        self.host = level;
    }

    /// U2PIO_GET_MDIO: open-drain wired-AND of host and PHY (08 §MDIO).
    pub fn mdio(&self) -> bool {
        self.host && self.drive.unwrap_or(true)
    }

    /// One MDC rising edge: sample the host bit, then update the PHY's own drive.
    fn rising_edge(&mut self) {
        let bit = self.host;
        self.drive = None;
        self.frame = match self.frame {
            Mdio::Preamble(ones) if bit => Mdio::Preamble(ones.saturating_add(1)),
            Mdio::Preamble(ones) if ones >= PREAMBLE_MIN => Mdio::Header { bits: 0, n: 1 },
            Mdio::Preamble(_) => Mdio::Preamble(0),
            Mdio::Header { bits, n } => {
                let (bits, n) = ((bits << 1) | u16::from(bit), n + 1);
                if n < HEADER_BITS {
                    Mdio::Header { bits, n }
                } else {
                    let (start, op) = (bits >> 12, (bits >> 10) & 3);
                    let (phy, reg) = ((bits >> 5) as u8 & 0x1F, bits as u8 & 0x1F);
                    match (start, op) {
                        (0b01, OP_READ) if phy == PHY_ADDR => Mdio::Turnaround(self.read_reg(reg)),
                        (0b01, OP_WRITE) => Mdio::Write { phy, reg, bits: 0, n: 0 },
                        _ => Mdio::Preamble(0),
                    }
                }
            }
            Mdio::Turnaround(value) => {
                self.drive = Some(false);
                Mdio::Read { value, left: 16 }
            }
            Mdio::Read { value, left } => {
                let left = left - 1;
                self.drive = Some(value >> left & 1 != 0);
                if left == 0 {
                    Mdio::Preamble(0)
                } else {
                    Mdio::Read { value, left }
                }
            }
            Mdio::Write { phy, reg, bits, n } => {
                let (bits, n) = ((bits << 1) | u32::from(bit), n + 1);
                if n < WRITE_BITS {
                    Mdio::Write { phy, reg, bits, n }
                } else {
                    if phy == PHY_ADDR {
                        self.write_reg(reg, bits as u16);
                    }
                    Mdio::Preamble(0)
                }
            }
        };
    }

    fn read_reg(&self, reg: u8) -> u16 {
        match (reg, self.link) {
            (BMSR, false) => BMSR_LINK_DOWN,
            (BMSR, true) => BMSR_LINK_UP,
            (PHYID1, _) => PHYID1_VALUE,
            (PHYID2, _) => PHYID2_VALUE,
            (ANLPAR, true) => ANLPAR_LINK_UP,
            (PHYCTRL1, true) => PHYCTRL1_LINK_UP,
            (ANLPAR | PHYCTRL1, false) => 0,
            _ => self.regs[usize::from(reg)],
        }
    }

    /// Writable registers are stored; the firmware's writes (reg 4, 0x1B, 0x16, 0) only need to read back
    /// (rmii_interface.cc:71-74, 08 §MDIO).
    fn write_reg(&mut self, reg: u8, val: u16) {
        match reg {
            BMSR | PHYID1 | PHYID2 | ANLPAR | PHYCTRL1 => {}
            BMCR => self.regs[usize::from(BMCR)] = val & !BMCR_SELF_CLEAR,
            _ => self.regs[usize::from(reg)] = val,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::RAM_SIZE;
    use crate::devices::board::{self, rig::Rig, U2pio};

    const RMII: u32 = RMII_BASE;
    /// Pool home like `new uint8_t[32*1536+256]` rounded up to 256 (rmii_interface.cc:53-56).
    const POOL: u32 = 0x0012_3400;
    const OWN_MAC: [u8; 6] = [0x02, 0x15, 0x41, 0xA1, 0xB2, 0xC3];
    const NUM_BUFFERS: u8 = 32;

    /// The MAC with a full DDR and the ITU core, accessed like the bus does.
    struct Mac {
        dev: Rmii,
        ram: Vec<u8>,
        irq: IrqState,
        console: Vec<u8>,
    }

    impl Mac {
        fn new() -> Self {
            Mac { dev: Rmii::new(), ram: vec![0; RAM_SIZE], irq: IrqState::new(), console: Vec::new() }
        }

        fn ctx(&mut self) -> (&mut Rmii, IoCtx<'_>) {
            let ctx = IoCtx { stall: 0, now: 0, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            (&mut self.dev, ctx)
        }

        fn w8(&mut self, addr: u32, val: u8) {
            let (dev, mut ctx) = self.ctx();
            dev.write8(addr - RMII, val, &mut ctx);
        }

        fn r8(&mut self, addr: u32) -> u8 {
            let (dev, mut ctx) = self.ctx();
            dev.read8(addr - RMII, &mut ctx)
        }

        fn w32(&mut self, addr: u32, val: u32) {
            for (i, byte) in (0u32..).zip(val.to_le_bytes()) {
                self.w8(addr + i, byte);
            }
        }

        fn host_frame(&mut self, frame: &[u8]) {
            self.dev.receive(frame, &mut self.ram, &mut self.irq);
        }

        /// RmiiInterface ctor + task + `initRx` (rmii_interface.cc:56,130-137,88-108).
        fn init_rx(&mut self) {
            self.w32(0x1006_0820, POOL);
            for (i, &b) in (0u32..).zip(&OWN_MAC) {
                self.w8(0x1006_0800 + i, b);
            }
            self.w8(0x1006_0807, 0);
            self.w8(0x1006_0808, 0);
            self.w8(0x1006_082E, 1);
            self.w8(0x1006_082F, 1);
            self.w8(0x1006_082E, 1);
            for id in 0..NUM_BUFFERS {
                self.w8(0x1006_0824, id);
            }
            self.w8(0x1006_0808, 1);
            self.irq.mask |= 1 << RX_IRQ_BIT;
        }

        /// `rx_interrupt_handler` (rmii_interface.cc:202-231): valid, size, id, pop. None when nothing is valid.
        fn isr(&mut self) -> Option<(u8, u16)> {
            if self.r8(0x1006_082F) == 0 {
                return None;
            }
            let size = u16::from_le_bytes([self.r8(0x1006_082A), self.r8(0x1006_082B)]);
            let id = self.r8(0x1006_0828);
            self.w8(0x1006_082F, 1);
            Some((id, size))
        }

        fn block(&self, id: u8, len: usize) -> &[u8] {
            let start = (POOL + 1536 * u32::from(id) + 2) as usize;
            &self.ram[start..start + len]
        }
    }

    /// Ethernet frame to `dst` with a payload of `len - 14` bytes of `fill`.
    fn frame(dst: [u8; 6], len: usize, fill: u8) -> Vec<u8> {
        let mut f = dst.to_vec();
        f.extend_from_slice(&[0x52, 0x55, 0x0A, 0x00, 0x02, 0x02, 0x08, 0x00]);
        f.resize(len, fill);
        f
    }

    #[test]
    fn c24_rx_irq_level_follows_used_valid() {
        let mut m = Mac::new();
        m.init_rx();
        assert_eq!(m.irq.active(), 0);
        assert_eq!(m.isr(), None);

        let f = frame([0xFF; 6], 342, 0x5A);
        m.host_frame(&f);
        assert_eq!(m.irq.active(), 0x20, "level IRQ while a frame is presented");
        m.irq.clear(0xFF);
        assert_eq!(m.irq.active(), 0x20, "ITU_IRQ_CLEAR does not ack a level source");
        assert_eq!(m.isr(), Some((0, 342)));
        assert_eq!(m.block(0, 342), &f[..]);
        assert_eq!((m.irq.active(), m.r8(0x1006_0828)), (0, 0xFF), "pop drops the line, id reads 0xFF");
        assert_eq!(m.r8(0x1006_081A), 0, "TX_BUSY");
    }

    #[test]
    fn next_frame_is_presented_after_pop_and_ids_recycle() {
        let mut m = Mac::new();
        m.init_rx();
        for i in 0..=NUM_BUFFERS {
            m.host_frame(&frame(OWN_MAC, 60 + usize::from(i), i));
        }
        let mut seen = Vec::new();
        while let Some((id, size)) = m.isr() {
            assert_eq!(m.irq.active(), if seen.len() < 31 { 0x20 } else { 0 }, "line re-asserts for the next entry");
            assert_eq!(m.block(id, 1)[0], 0x02);
            seen.push((id, size));
        }
        let expected: Vec<_> = (0..NUM_BUFFERS).map(|i| (i, 60 + u16::from(i))).collect();
        assert_eq!(seen, expected, "33rd frame starved (08 H8)");

        m.host_frame(&frame(OWN_MAC, 64, 1));
        assert_eq!(m.isr(), None, "no free ID until FREE_PUT");
        m.w8(0x1006_0824, 7);
        m.host_frame(&frame(OWN_MAC, 64, 1));
        assert_eq!(m.isr(), Some((7, 64)));
    }

    #[test]
    fn filter_passes_unicast_and_broadcast_and_reuses_the_rejected_block() {
        let mut m = Mac::new();
        m.init_rx();
        m.host_frame(&frame([0x02, 0x15, 0x41, 0xA1, 0xB2, 0xC4], 60, 0));
        m.host_frame(&frame([0x01, 0x00, 0x5E, 0x00, 0x00, 0xFB], 60, 0));
        m.host_frame(&frame([0x33, 0x33, 0x00, 0x00, 0x00, 0x01], 60, 0));
        m.host_frame(&vec![0x11; MAX_RX_FRAME + 1]);
        assert_eq!(m.isr(), None, "other unicast, multicast and oversize are dropped");
        // Word-wise compare: 0xFFFF words mix with own-MAC words.
        m.host_frame(&frame([0xFF, 0xFF, 0x41, 0xA1, 0xFF, 0xFF], 60, 0));
        assert_eq!(m.isr(), Some((0, 60)), "the block of the rejected frames is reused");
        m.host_frame(&frame(OWN_MAC, MAX_RX_FRAME, 0));
        assert_eq!(m.isr(), Some((1, MAX_RX_FRAME as u16)));

        m.w8(0x1006_0807, 1);
        m.host_frame(&frame([0x01, 0x00, 0x5E, 0x00, 0x00, 0xFB], 70, 0));
        assert_eq!(m.isr(), Some((2, 70)), "promiscuous");
    }

    #[test]
    fn rx_disable_drops_frames_and_loses_the_held_block() {
        let mut m = Mac::new();
        m.host_frame(&frame([0xFF; 6], 60, 0));
        assert_eq!(m.isr(), None, "RX disabled after reset");
        m.init_rx();
        m.host_frame(&frame([0x01; 6], 60, 0));
        assert_eq!(m.dev.held, Some(0));
        m.w8(0x1006_0808, 0);
        assert_eq!(m.dev.held, None);
        m.host_frame(&frame([0xFF; 6], 60, 0));
        m.w8(0x1006_0808, 1);
        m.host_frame(&frame([0xFF; 6], 60, 0));
        assert_eq!(m.isr(), Some((1, 60)), "ID 0 is gone from the free list");
    }

    #[test]
    fn free_reset_keeps_the_presented_frame() {
        let mut m = Mac::new();
        m.init_rx();
        m.host_frame(&frame([0xFF; 6], 60, 0));
        m.host_frame(&frame([0xFF; 6], 61, 0));
        m.w8(0x1006_082E, 1);
        assert_eq!(m.irq.active(), 0x20);
        assert_eq!(m.isr(), Some((0, 60)));
        assert_eq!(m.isr(), None, "the queued entry was reset away");
        m.w8(0x1006_082F, 1);
        assert_eq!(m.r8(0x1006_0828), 0xFF, "pop without a valid entry leaves the id register");
    }

    #[test]
    fn free_put_is_ignored_when_full() {
        let mut m = Mac::new();
        m.w8(0x1006_0808, 1);
        for i in 0..130u8 {
            m.w8(0x1006_0824, i);
        }
        assert_eq!(m.r8(0x1006_0824), 0x02, "free_full");
        m.w32(0x1006_0820, POOL);
        let mut ids = Vec::new();
        for _ in 0..130 {
            m.host_frame(&frame([0xFF; 6], 60, 0));
            if let Some((id, _)) = m.isr() {
                ids.push(id);
            }
        }
        assert_eq!(ids, (0..127).collect::<Vec<_>>());
        assert_eq!(m.r8(0x1006_0824), 0);
    }

    #[test]
    fn tx_copies_synchronously_and_its_irq_stays_masked() {
        let mut m = Mac::new();
        m.irq.mask = !(1 << TX_IRQ_BIT);
        m.ram[0x4_0000..0x4_0000 + 42].copy_from_slice(&frame([0xFF; 6], 42, 0xEE));
        m.ram[0x4_0000 + 42..0x4_0000 + 60].fill(0x99);
        // `output_packet` (rmii_interface.cc:318-320): 32-bit address, 16-bit length padded to 60, start.
        m.w32(0x1006_0810, 0x0404_0000);
        m.w8(0x1006_0814, 60);
        m.w8(0x1006_0815, 0);
        m.w8(0x1006_0818, 1);
        assert_eq!(m.r8(0x1006_081A), 0, "busy never reads 1");
        let sent: Vec<_> = m.dev.to_host.drain(..).collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][..42], frame([0xFF; 6], 42, 0xEE), "26-bit address");
        assert_eq!(sent[0][42..], [0x99; 18], "padding comes from RAM");
        assert_eq!((m.irq.level & 0x40, m.irq.active()), (0x40, 0));
        m.w8(0x1006_0819, 1);
        assert_eq!(m.irq.level & 0x40, 0);

        m.w8(0x1006_0814, 0);
        m.w8(0x1006_0818, 1);
        assert_eq!(m.dev.to_host[0].len(), 4096, "zero length underflows the counter");
    }

    #[test]
    fn tx_queue_is_bounded() {
        let mut m = Mac::new();
        m.w8(0x1006_0814, 60);
        for _ in 0..TX_QUEUE_LIMIT + 5 {
            m.w8(0x1006_0818, 1);
        }
        assert_eq!(m.dev.to_host.len(), TX_QUEUE_LIMIT);
    }

    /// Records sent frames and replies to each with one broadcast frame of the same length.
    #[derive(Default)]
    struct Echo {
        sent: Vec<Vec<u8>>,
        pending: Vec<Vec<u8>>,
    }

    impl NetBackend for Echo {
        fn send(&mut self, sent: &[u8]) {
            self.sent.push(sent.to_vec());
            self.pending.push(frame([0xFF; 6], sent.len(), 0x77));
        }

        fn poll(&mut self, deliver: &mut dyn FnMut(&[u8])) {
            for f in self.pending.drain(..) {
                deliver(&f);
            }
        }
    }

    #[test]
    fn exchange_sends_then_delivers() {
        let mut m = Mac::new();
        m.init_rx();
        m.w8(0x1006_0814, 64);
        m.w8(0x1006_0818, 1);
        let mut net = Echo::default();
        m.dev.exchange(&mut net, &mut m.ram, &mut m.irq);
        assert_eq!(net.sent.len(), 1);
        assert!(m.dev.to_host.is_empty());
        assert_eq!(m.irq.active(), 0x20);
        assert_eq!(m.isr(), Some((0, 64)));
    }

    #[test]
    fn install_maps_the_window_and_the_block_repeats() {
        let mut rig = Rig::new(install);
        assert_eq!(rig.map.resolve(0x1006_08FF), Some((0, 0xFF)));
        for id in 0..127 {
            assert_eq!(rig.r8(0x1006_08A4), 0);
            rig.w8(0x1006_0864, id);
        }
        assert_eq!([0x1006_0824, 0x1006_0864, 0x1006_08E4].map(|a| rig.r8(a)), [0x02; 3], "free_full at every alias");
        assert_eq!((rig.irq.flags, rig.irq.level), (0, 0));
    }

    /// `mdio_bit` / `mdio_read` / `mdio_write` exactly as mdio.c:28-114, through the U2PIO page.
    fn bit(rig: &mut Rig, val: bool) {
        rig.w8(0x1010_000B, u8::from(val));
        rig.w8(0x1010_000A, 1);
        rig.w8(0x1010_000A, 0);
    }

    fn header(rig: &mut Rig, op: [bool; 2], reg: u8, addr: u8) {
        for _ in 0..33 {
            bit(rig, true);
        }
        for b in [false, true, op[0], op[1], false, false, false, addr != 0, addr != 0] {
            bit(rig, b);
        }
        for i in (0..5).rev() {
            bit(rig, reg >> i & 1 != 0);
        }
    }

    fn mdio_read(rig: &mut Rig, reg: u8, addr: u8) -> u16 {
        header(rig, [true, false], reg, addr);
        bit(rig, true);
        (0..16).fold(0, |acc, _| {
            bit(rig, true);
            acc << 1 | u16::from(rig.r8(0x1010_0006) != 0)
        })
    }

    fn mdio_write(rig: &mut Rig, reg: u8, data: u16, addr: u8) {
        header(rig, [false, true], reg, addr);
        bit(rig, true);
        bit(rig, false);
        for i in (0..16).rev() {
            bit(rig, data >> i & 1 != 0);
        }
        bit(rig, true);
    }

    fn phy(rig: &mut Rig) -> &mut Phy {
        &mut rig.map.get_mut::<U2pio>().unwrap().phy
    }

    #[test]
    fn c24_phy_ident_at_address_0_and_link_follows_the_cable() {
        let mut rig = Rig::new(board::install);
        // RmiiInterface ctor (rmii_interface.cc:59-74).
        assert_eq!(mdio_read(&mut rig, 2, 0), 0x0022, "08 H2");
        assert_eq!(mdio_read(&mut rig, 2, 3), 0xFFFF, "no PHY at address 3");
        mdio_write(&mut rig, 0x04, 0x01E1, 0);
        mdio_write(&mut rig, 0x1B, 0x0500, 0);
        mdio_write(&mut rig, 0x16, 0x0002, 0);
        mdio_write(&mut rig, 0x00, 0x1200, 0);
        assert_eq!(
            [0x04, 0x1B, 0x16, 0x00, 0x03].map(|r| mdio_read(&mut rig, r, 0)),
            [0x01E1, 0x0500, 0x0002, 0x1000, 0x1561],
            "stored; restart-AN self-clears"
        );

        assert_eq!(mdio_read(&mut rig, 1, 0) & 0x04, 0, "no cable: link poll keeps waiting (08 H3)");
        assert_eq!((mdio_read(&mut rig, 5, 0), mdio_read(&mut rig, 0x1E, 0)), (0, 0));
        phy(&mut rig).set_link(true);
        assert_eq!(mdio_read(&mut rig, 1, 0), 0x786D, "link up, AN complete");
        assert_eq!((mdio_read(&mut rig, 5, 0), mdio_read(&mut rig, 0x1E, 0)), (0x41E1, 0x0106), "100BASE-TX FD");
        mdio_write(&mut rig, 0x01, 0, 0);
        mdio_write(&mut rig, 0x02, 0, 3);
        assert_eq!((mdio_read(&mut rig, 1, 0), mdio_read(&mut rig, 2, 0)), (0x786D, 0x0022), "read-only");

        phy(&mut rig).reset();
        assert_eq!((mdio_read(&mut rig, 1, 0), mdio_read(&mut rig, 0x1B, 0)), (0x786D, 0), "reset keeps the cable");
    }

    #[test]
    fn mdio_pin_is_wired_and_and_the_phy_drives_only_its_data_phase() {
        let mut rig = Rig::new(board::install);
        assert_eq!(rig.r8(0x1010_0006), 1, "released");
        rig.w8(0x1010_000B, 0);
        assert_eq!(rig.r8(0x1010_0006), 0);
        header(&mut rig, [true, false], 2, 0);
        bit(&mut rig, true);
        assert_eq!(rig.r8(0x1010_0006), 0, "PHY drives TA 0");
        for i in (0..16).rev() {
            bit(&mut rig, false);
            assert_eq!(rig.r8(0x1010_0006), 0, "host low wins");
            rig.w8(0x1010_000B, 1);
            assert_eq!(u16::from(rig.r8(0x1010_0006)), (0x0022 >> i) & 1);
        }
        bit(&mut rig, true);
        assert_eq!(rig.r8(0x1010_0006), 1, "released after D0");
        // A start without 32 preamble ones is ignored.
        for b in [true, false, true, true, false] {
            bit(&mut rig, b);
        }
        for _ in 0..16 {
            bit(&mut rig, true);
            assert_eq!(rig.r8(0x1010_0006), 1);
        }
        assert_eq!(mdio_read(&mut rig, 2, 0), 0x0022, "decoder resynchronises");
    }

    #[test]
    fn fw_rmii_brings_the_link_up_and_sends_dhcp_discover() {
        use crate::loader::tests::{firmware_root, FIRMWARE_ELF};
        use crate::machine::{Machine, RunExit};

        let Some(root) = firmware_root() else { return };
        let mut cfg = MachineConfig::new(root.join(FIRMWARE_ELF), root.join("roms"));
        cfg.capabilities |= CAPAB_ETH_RMII;
        let mut m = Machine::new(cfg).unwrap();
        m.bus.io.get_mut::<U2pio>().unwrap().phy.set_link(true);

        #[derive(Default)]
        struct Wire(Vec<Vec<u8>>);
        impl NetBackend for Wire {
            fn send(&mut self, frame: &[u8]) {
                self.0.push(frame.to_vec());
            }
            fn poll(&mut self, _deliver: &mut dyn FnMut(&[u8])) {}
        }
        let mut wire = Wire::default();
        // DHCP DISCOVER: broadcast, IPv4 UDP 68 → 67, source MAC 02:15:41:xx:xx:xx (rmii_interface.cc:123-128).
        let is_discover = |f: &Vec<u8>| {
            f.len() >= 42 && f[..6] == [0xFF; 6] && f[6..9] == [0x02, 0x15, 0x41] && f[12..14] == [0x08, 0x00]
                && f[23] == 17 && f[34..38] == [0, 68, 0, 67]
        };
        let deadline_ms = 20_000;
        while !wire.0.iter().any(is_discover) {
            assert_eq!(m.run(1_000_000), RunExit::Budget);
            assert!(m.now_ms() < deadline_ms, "no DHCP DISCOVER within {deadline_ms} ms emulated");
            let bus = &mut m.bus;
            bus.io.get_mut::<Rmii>().unwrap().exchange(&mut wire, &mut bus.ram, &mut bus.irq);
        }
        let console = String::from_utf8_lossy(&m.drain_console()).into_owned();
        assert!(!console.contains("could not find Ethernet PHY"), "{console}");
        assert!(!console.contains("tx is busy"), "{console}");
    }
}
