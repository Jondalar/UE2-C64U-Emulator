//! WiFi DMA UART 0x10060900 + stub u64ctrl (docs/hw/04-esp32-wifi.md tier T0).
//! Spec: docs/specs/S05-wifi-u64ctrl.md
//!
//! The FPGA side is `uart_dma` (fpga/io/uart_lite/vhdl_source/uart_dma.vhd) modelled at frame level: the CPU
//! never sees UART bytes, only whole SLIP payloads in RAM plus a length (doc 04 §Functional model). Baud rate,
//! HW flow control, loopback and the MOD bits only read back. The ESP32 side is [`U64Ctrl`], a frame-level
//! stand-in for `software/u64ctrl/main/rpc_dispatch.c`.

use std::collections::VecDeque;

use crate::bus::RAM_MASK;
use crate::io::{IoCtx, IoDevice, IoMap, IO_GRAIN};
use crate::machine::MachineConfig;
use crate::time::CLOCKS_PER_MS;

/// `WIFI_UART_BASE` (iomap.h:32).
pub const WIFI_UART_BASE: u32 = 0x1006_0900;
/// ITU high IRQ source (`ITU_IRQHIGH_WIFI`, itu.h:43; esp32.cc:73).
pub const WIFI_IRQ_HIGH_BIT: u8 = 3;
/// Latency of a u64ctrl reply: well inside the 100-tick (0.5 s) IDENTIFY timeout (wifi_cmd.cc:80, doc 04 H4).
pub const REPLY_DELAY: u64 = CLOCKS_PER_MS;

/// Status b4 `cts_c` = cts | !cts_enable; the ESP never blocks, so it reads 1 (uart_dma.vhd:279,384).
const ST_CTS: u8 = 0x10;
/// Status b5 `rx_len_valid`: a received frame waits for `rx_pop` (uart_dma.vhd:385).
const ST_RX_VALID: u8 = 0x20;
/// Status b6 `tx_addr_ready`: tx_dma idle (uart_dma.vhd:386).
const ST_TX_READY: u8 = 0x40;
/// Status b7 `!rx_addr_valid`: the receiver needs an address (uart_dma.vhd:387).
const ST_RX_NEED_ADDR: u8 = 0x80;
/// ictrl b7: 1 sets, 0 clears the rx/tx/buf enables given in bits 2:0 (uart_dma.vhd:300-309).
const ICTRL_SET: u8 = 0x80;
const ICTRL_ENABLES: u8 = 0x07;
/// flowctrl b7: soft-reset strobe (uart_dma.vhd:321-326, dma_uart.h:42).
const FLOW_RESET: u8 = 0x80;
/// flowctrl b2 `DMAUART_SLIPENABLE` (dma_uart.h:39).
const FLOW_SLIP: u8 = 0x04;
/// flowctrl b6:4: the module mode `ModuleCtrl` writes (dma_uart.cc:107-111).
const FLOW_MODE_SHIFT: u8 = 4;
const FLOW_MODE_MASK: u8 = 0x07;
/// `ESP_MODE_BOOT` on U64-II (esp32.h:21-27): the module restarts into its ROM serial loader.
const ESP_MODE_BOOT: u8 = 1;
/// flowctrl bits that latch and read back: b0,b1,b2,b4,b5,b6. b3 and b7 read 0 (uart_dma.vhd:315-320,389-395, H3).
const FLOW_LATCHED: u8 = 0x77;
/// flowctrl after reset: boot = 1, all other fields 0 (uart_dma.vhd:413-419).
const FLOW_RESET_VALUE: u8 = 0x10;
/// `g_divisor - 1` for the entity default `g_divisor = 35` (uart_dma.vhd:14,424). The closed U64-II top level may
/// use another generic (doc 04 open question 1); the ctor discards the value anyway (dma_uart.h:74).
const DIVISOR_RESET_VALUE: u16 = 34;
/// tx_addr and rx_addr latch 28 bits; byte +7/+F keeps only its low nibble (uart_dma.vhd:338,362).
const DMA_ADDR_BITS: u32 = 0x0FFF_FFFF;

/// A frame the ESP32 has sent towards the Ultimate.
struct EspFrame {
    /// Clock at which the frame has fully arrived and may be written into an RX buffer.
    due: u64,
    data: Vec<u8>,
}

/// `uart_dma` register block + tx_dma/rx_dma, with the stub control module on the other end of the link.
///
/// - Status bits 0-2 are live `enable & condition`, never latched (H1).
/// - TX completes at `tx_push` (H6).
/// - RX is the two-slot model: one address latched by rx_dma plus one pending in the register (H11).
/// - The IRQ level drives ITU high bit 3 (H2).
/// - The module mode in flowctrl b6:4 selects the far end: `ESP_MODE_BOOT` the ROM serial loader
///   ([`rom_loader_reply`]), every other mode the control module [`U64Ctrl`]. Only the updater's ESP32 flashing
///   (`Esp32::Download` / `Esp32::Flash`, esp32.cc:231-400) uses the loader; the application ELF only writes mode 0.
pub struct Wifi {
    divisor: u16,
    /// Latched flowctrl fields (`FLOW_LATCHED`).
    flow: u8,
    /// rx/tx/buf IRQ enables in status bit positions 0/1/2.
    irq_en: u8,
    tx_addr: u32,
    tx_len: u16,
    /// `rx_addr_data`: the pending RX address register (+C..+F).
    rx_addr: u32,
    /// `rx_addr_valid`: `rx_addr` has been written (+F) and not yet taken by rx_dma.
    rx_addr_valid: bool,
    /// Address latched by rx_dma, waiting for the next frame (rx_dma.vhd:87-95).
    rx_active: Option<u32>,
    /// `len_data` / `len_valid` of rx_dma (rx_dma.vhd:140-150).
    rx_len: u16,
    rx_len_valid: bool,
    /// Frames from the ESP32, oldest first. Held while no RX buffer is armed; dropped only when the module restarts
    /// (doc 04 §Functional model).
    from_esp: VecDeque<EspFrame>,
    /// Raw bytes from the ESP32 while SLIP is off, not yet cut into a frame: the slip_decoder `ascii` chunk
    /// (slip_decoder.vhd:55-75). Only the ROM loader's banner lands here.
    ascii: Vec<u8>,
    /// The ESP32 control module behind the link.
    pub ctrl: U64Ctrl,
}

impl Default for Wifi {
    fn default() -> Self {
        Self::new()
    }
}

impl Wifi {
    pub fn new() -> Self {
        Wifi {
            divisor: DIVISOR_RESET_VALUE,
            flow: FLOW_RESET_VALUE,
            irq_en: 0,
            tx_addr: 0,
            tx_len: 0,
            rx_addr: 0,
            rx_addr_valid: false,
            rx_active: None,
            rx_len: 0,
            rx_len_valid: false,
            from_esp: VecDeque::new(),
            ascii: Vec::new(),
            ctrl: U64Ctrl::default(),
        }
    }

    /// Module mode, flowctrl b6:4.
    fn esp_mode(flow: u8) -> u8 {
        (flow >> FLOW_MODE_SHIFT) & FLOW_MODE_MASK
    }

    /// Effects of a flowctrl write on the ESP32 side of the link:
    /// - A new module mode restarts the module (`ModuleCtrl` drives its enable and boot pins, esp32.h:21-27), so
    ///   frames still inside it are lost. Entering `ESP_MODE_BOOT` starts the ROM loader, which prints its banner
    ///   raw; with SLIP on, the decoder waits for a 0xC0 and drops it (slip_decoder.vhd:77-120).
    /// - Switching SLIP on cuts the pending raw bytes into a frame with an appended 0x0A (slip_decoder.vhd:55-75).
    ///   `Esp32::Download` reads the banner that way (esp32.cc:247-266).
    fn flow_changed(&mut self, old: u8, now: u64) {
        if Self::esp_mode(self.flow) != Self::esp_mode(old) {
            self.from_esp.clear();
            self.ascii.clear();
            if Self::esp_mode(self.flow) == ESP_MODE_BOOT && self.flow & FLOW_SLIP == 0 {
                self.ascii.extend_from_slice(ROM_BANNER);
            }
        }
        if self.flow & FLOW_SLIP != 0 && old & FLOW_SLIP == 0 && !self.ascii.is_empty() {
            let mut data = std::mem::take(&mut self.ascii);
            data.push(b'\n');
            self.from_esp.push_back(EspFrame { due: now, data });
        }
    }

    /// Status bits 5-7: `rx_len_valid`, `tx_addr_ready`, `!rx_addr_valid` (uart_dma.vhd:385-387). TX completes
    /// inside the `tx_push` write, so `tx_addr_ready` never reads 0.
    fn stream_flags(&self) -> u8 {
        let mut flags = ST_TX_READY;
        if self.rx_len_valid {
            flags |= ST_RX_VALID;
        }
        if !self.rx_addr_valid {
            flags |= ST_RX_NEED_ADDR;
        }
        flags
    }

    /// rx_interrupt, tx_interrupt, buf_interrupt = enable & flag. Bits 0-2 line up with flags 5-7
    /// (uart_dma.vhd:433-435).
    fn irq_sources(&self) -> u8 {
        (self.stream_flags() >> 5) & self.irq_en
    }

    /// +2 read. The ISR loops `while (status & 7)` (dma_uart.cc:157-165), so bits 0-2 are computed live (H1).
    /// Overflow (b3) cannot happen on a frame-level link and reads 0.
    fn status(&self) -> u8 {
        self.stream_flags() | ST_CTS | self.irq_sources()
    }

    /// flowctrl b7 (uart_dma.vhd:283-288,321-326; rx_dma.vhd:159-164; tx_dma.vhd:111-116): both DMAs are flushed
    /// and the three IRQ enables cleared. Frames still inside the ESP32 stay queued.
    fn soft_reset(&mut self) {
        self.irq_en = 0;
        self.rx_addr_valid = false;
        self.rx_active = None;
        self.rx_len_valid = false;
    }

    /// `tx_push` (+A): tx_dma reads `tx_len` bytes at `tx_addr & 0x03FFFFFF` (tx_dma.vhd:51,70-99) and the ESP32
    /// receives them as one frame. The copy completes at once, so the ISR frees the TX buffer on its next pass
    /// (dma_uart.cc:187-190, H6).
    fn transmit(&mut self, ctx: &mut IoCtx) {
        let frame: Vec<u8> = (0..u32::from(self.tx_len))
            .map(|i| ctx.ram[(self.tx_addr.wrapping_add(i) & RAM_MASK) as usize])
            .collect();
        let reply = match Self::esp_mode(self.flow) {
            ESP_MODE_BOOT => Some(rom_loader_reply(&frame)),
            _ => self.ctrl.handle(&frame),
        };
        if let Some(reply) = reply {
            self.from_esp.push_back(EspFrame { due: ctx.now + REPLY_DELAY, data: reply });
        }
    }

    /// rx_dma: take the pending address while idle (rx_dma.vhd:87-95), then write the oldest arrived frame at it
    /// and report its length (rx_dma.vhd:110-150). A reported frame that has not been popped holds the next one.
    /// Frames land in the oldest armed address, matching the ISR's `rx_bufs` FIFO (dma_uart.cc:173,211, H11).
    fn service_rx(&mut self, ctx: &mut IoCtx) {
        loop {
            if self.rx_active.is_none() && self.rx_addr_valid {
                self.rx_active = Some(self.rx_addr);
                self.rx_addr_valid = false;
            }
            if self.rx_len_valid {
                return;
            }
            let Some(addr) = self.rx_active else { return };
            let Some(frame) = self.from_esp.front() else { return };
            if frame.due > ctx.now {
                return;
            }
            for (i, &byte) in frame.data.iter().enumerate() {
                ctx.ram[(addr.wrapping_add(i as u32) & RAM_MASK) as usize] = byte;
            }
            self.rx_len = frame.data.len() as u16;
            self.from_esp.pop_front();
            self.rx_len_valid = true;
            self.rx_active = None;
        }
    }

    /// Level into ITU high bit 3 (uart_dma.vhd:436, H2). There is no ack register: the level drops when the ISR
    /// pops the frame, arms an address or clears an enable (dma_uart.cc:168-223).
    fn update_irq(&self, ctx: &mut IoCtx) {
        ctx.irq.set_high(WIFI_IRQ_HIGH_BIT, self.irq_sources() != 0);
    }
}

/// Replace byte `idx` (0 = LSB) of a 28-bit DMA address register.
fn set_addr_byte(reg: u32, idx: u32, val: u8) -> u32 {
    let shift = idx * 8;
    ((reg & !(0xFF << shift)) | (u32::from(val) << shift)) & DMA_ADDR_BITS
}

impl IoDevice for Wifi {
    fn name(&self) -> &'static str {
        "wifi"
    }

    fn read8(&mut self, off: u32, _ctx: &mut IoCtx) -> u8 {
        self.peek8(off)
    }

    /// Register writes (uart_dma.vhd:291-367). The block decodes `address(3 downto 0)` and repeats through the
    /// window. Multi-byte registers arrive as LE bytes in address order (wishbone2memio.vhd:148-175), so the +F
    /// byte of `rx_addr` comes last and arms the receiver.
    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        match off & 0xF {
            0x0 => self.divisor = (self.divisor & !0xFF) | u16::from(val),
            0x1 => self.divisor = (self.divisor & 0xFF) | (u16::from(val & 0x07) << 8),
            0x2 => {
                let bits = val & ICTRL_ENABLES;
                if val & ICTRL_SET != 0 {
                    self.irq_en |= bits;
                } else {
                    self.irq_en &= !bits;
                }
            }
            0x3 => {
                let old = self.flow;
                self.flow = val & FLOW_LATCHED;
                if val & FLOW_RESET != 0 {
                    self.soft_reset();
                }
                self.flow_changed(old, ctx.now);
            }
            n @ 0x4..=0x7 => self.tx_addr = set_addr_byte(self.tx_addr, n - 0x4, val),
            0x8 => self.tx_len = (self.tx_len & 0xFF00) | u16::from(val),
            0x9 => self.tx_len = (self.tx_len & 0x00FF) | (u16::from(val) << 8),
            0xA => self.transmit(ctx),
            // rx_pop: `rx_len_ready` pulse clears len_valid (uart_dma.vhd:349-350, rx_dma.vhd:72-74).
            0xB => self.rx_len_valid = false,
            n => {
                self.rx_addr = set_addr_byte(self.rx_addr, n - 0xC, val);
                if n == 0xF {
                    self.rx_addr_valid = true;
                }
            }
        }
        self.service_rx(ctx);
        self.update_irq(ctx);
    }

    /// Register reads (uart_dma.vhd:369-406). Reads have no side effects.
    fn peek8(&self, off: u32) -> u8 {
        match off & 0xF {
            0x0 => self.divisor as u8,
            // Only divisor(9:8) reads back, although three bits are written (uart_dma.vhd:376-377).
            0x1 => ((self.divisor >> 8) & 0x03) as u8,
            0x2 => self.status(),
            0x3 => self.flow,
            0x8 => self.rx_len as u8,
            0x9 => (self.rx_len >> 8) as u8,
            _ => 0,
        }
    }

    /// Only an arrived frame with a free receiver needs time to pass. A frame held by a missing or unpopped RX
    /// buffer waits for a register write, which already triggers a deadline recompute.
    fn next_event(&self) -> Option<u64> {
        if self.rx_len_valid || self.rx_active.is_none() {
            return None;
        }
        self.from_esp.front().map(|frame| frame.due)
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.service_rx(ctx);
        self.update_irq(ctx);
    }

    /// Hardware reset (uart_dma.vhd:413-425). The module's settings survive (they model ESP32 NVS); frames in
    /// flight belonged to the firmware instance that is gone.
    fn reset(&mut self) {
        let ctrl = std::mem::take(&mut self.ctrl);
        *self = Wifi { ctrl, ..Wifi::new() };
    }

    crate::impl_as_any!();
}

// RPC command codes the stub answers specifically (rpc_calls.h:185-209).
const CMD_IDENTIFY: u8 = 0x02;
const CMD_WIFI_SCAN: u8 = 0x04;
const CMD_WIFI_DISCONNECT: u8 = 0x06;
const CMD_WIFI_GETMAC: u8 = 0x07;
const CMD_SEND_PACKET: u8 = 0x08;
const CMD_MODEM_ON: u8 = 0x09;
const CMD_MODEM_OFF: u8 = 0x0A;
const CMD_WIFI_IS_CONNECTED: u8 = 0x0B;
const CMD_GET_VOLTAGES: u8 = 0x0C;
const CMD_WIFI_ENABLE: u8 = 0x0D;
const CMD_WIFI_DISABLE: u8 = 0x0E;
const CMD_MACHINE_OFF: u8 = 0x0F;
const CMD_CLEAR_APS: u8 = 0x11;
const CMD_WIFI_AUTOCONNECT: u8 = 0x12;
const CMD_MACHINE_REBOOT: u8 = 0x13;
const CMD_SET_POWER_MODE: u8 = 0x16;
const CMD_GET_POWER_MODE: u8 = 0x17;
const CMD_SET_WAKE_ON_WIFI: u8 = 0x18;
const CMD_GET_WAKE_ON_WIFI: u8 = 0x19;

// esp_err_t values (ESP-IDF).
const ESP_OK: i32 = 0;
const ESP_ERR_INVALID_ARG: i32 = 0x102;
const ESP_ERR_NOT_SUPPORTED: i32 = 0x106;

/// `rpc_header_t`: command u8, thread u8, sequence u16 (rpc_calls.h:17-21).
const HDR_LEN: usize = 4;
/// Identity of a real module (rpc_dispatch.h:29-37).
const IDENT_MAJOR: u16 = 1;
const IDENT_MINOR: u16 = 14;
const IDENT_STRING: &str = "ESP32 WiFi Bridge V1.14";
/// vbus, vaux, v50, v33, v18, v10, vusb in mV. vbus >= 8500 avoids "Low input voltage." (wifi.cc:258-260, H7).
const VOLTAGES_MV: [u16; 7] = [12000, 12000, 5000, 3300, 1800, 1000, 5000];
/// `POWERON_MODE_MAX` / `WAKE_ON_WIFI_MAX` (power_state.h:22,28).
const POWERON_MODE_MAX: u8 = 2;
const WAKE_ON_WIFI_MAX: u8 = 1;
/// sizeof(rpc_set_power_mode_req) = sizeof(rpc_set_wake_on_wifi_req): header + u8, padded to 6 (rpc_calls.h:88-103).
const SETTING_REQ_LEN: usize = 6;

/// A power request the control module has carried out (rpc_dispatch.c:165-185). The machine keeps running; the host
/// decides what the request means (doc 04 T1: MACHINE_OFF stops the machine, MACHINE_REBOOT is a cold boot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    /// CMD_MACHINE_OFF, e.g. the updater's final `turn_off` (update_common.h:53-76, wifi_cmd.cc:259-264).
    Off,
    /// CMD_MACHINE_REBOOT: off, then on again (rpc_dispatch.c:175-185).
    Reboot,
}

/// Frame-level stub of the ESP32 control module (doc 04 T0 §3). A reply reuses the request header, so command,
/// thread and sequence are echoed and `wifi_rx_isr` routes it to the waiting task (rpc_dispatch.c:33-372,
/// wifi_cmd.cc:15-30, H5, H10). Every request except SEND_PACKET gets exactly one reply.
pub struct U64Ctrl {
    /// Station MAC returned by WIFI_GETMAC (locally administered).
    pub mac: [u8; 6],
    /// `POWERON_MODE_*` (power_state.h:19-22) kept for SET/GET_POWER_MODE (H8).
    pub power_mode: u8,
    /// 1 when the machine was on at the last power transition (rpc_calls.h:97).
    pub last_state: u8,
    /// `WAKE_ON_WIFI_*` (power_state.h:26-28) kept for SET/GET_WAKE_ON_WIFI (H9).
    pub wake_on_wifi: u8,
    /// The last power request answered, until the host takes it.
    pub power_event: Option<PowerEvent>,
}

impl Default for U64Ctrl {
    /// The doc 04 T0 defaults: mode OFF, last state on, wake-on-WiFi disabled.
    fn default() -> Self {
        U64Ctrl {
            mac: [0x02, 0x15, 0x41, 0x00, 0x00, 0x01],
            power_mode: 0,
            last_state: 1,
            wake_on_wifi: 0,
            power_event: None,
        }
    }
}

impl U64Ctrl {
    /// Handle one frame from the Ultimate and return the reply frame, if any. A frame shorter than the header has
    /// nothing to echo; the firmware never sends one.
    pub fn handle(&mut self, req: &[u8]) -> Option<Vec<u8>> {
        let hdr = req.get(..HDR_LEN)?;
        let reply = match hdr[0] {
            // No reply: the ESP only forwards the payload (rpc_dispatch.c:158-163), and a reply would wake an
            // unrelated task (H10). An L2 bridge (S12) sends `length u32 @4, data @8` to the host segment here.
            CMD_SEND_PACKET => return None,
            // rpc_identify_resp: major @4, minor @6, string @8; size sizeof = 10 plus strlen, which covers the
            // NUL (rpc_calls.h:32-37, rpc_dispatch.c:38-46, H4).
            CMD_IDENTIFY => {
                let mut reply = hdr.to_vec();
                reply.extend_from_slice(&IDENT_MAJOR.to_le_bytes());
                reply.extend_from_slice(&IDENT_MINOR.to_le_bytes());
                reply.extend_from_slice(IDENT_STRING.as_bytes());
                reply.resize(10 + IDENT_STRING.len(), 0);
                reply
            }
            // rpc_getmac_resp, 16 bytes (rpc_calls.h:51-55, rpc_dispatch.c:48-54).
            CMD_WIFI_GETMAC => esp_reply(hdr, ESP_OK, &self.mac, 16),
            // `wifi_scan` copies `rec` from the reply whatever `esp_err` says (wifi_cmd.cc:219-229), so the error
            // uses the ESP's own layout with num_records = 0, reserved = 0, size 10 (wifi_modem.c:795-803).
            CMD_WIFI_SCAN => esp_reply(hdr, ESP_ERR_NOT_SUPPORTED, &[0, 0], 10),
            // rpc_get_connection_resp, status 0 = not connected, 12 bytes (rpc_calls.h:57-61). No
            // EVENT_CONNECTED/EVENT_DISABLED follows (wifi_modem.c:264-271), so the task settles in NotConnected
            // (wifi.cc:275-283).
            CMD_WIFI_IS_CONNECTED => esp_reply(hdr, ESP_OK, &[0], 12),
            // rpc_get_voltages_resp, 24 bytes (rpc_calls.h:39-49, rpc_dispatch.c:227-237, H7).
            CMD_GET_VOLTAGES => {
                let body: Vec<u8> = VOLTAGES_MV.iter().flat_map(|mv| mv.to_le_bytes()).collect();
                esp_reply(hdr, ESP_OK, &body, 24)
            }
            // rpc_get_power_mode_resp: mode @8, last_state @9, 12 bytes (rpc_calls.h:93-98, rpc_dispatch.c:328-336).
            CMD_GET_POWER_MODE => esp_reply(hdr, ESP_OK, &[self.power_mode, self.last_state], 12),
            CMD_SET_POWER_MODE => {
                let err = store_setting(req, &mut self.power_mode, POWERON_MODE_MAX);
                esp_reply(hdr, err, &[], 8)
            }
            // rpc_get_wake_on_wifi_resp: enabled @8, 12 bytes (rpc_calls.h:105-109, rpc_dispatch.c:357-364).
            CMD_GET_WAKE_ON_WIFI => esp_reply(hdr, ESP_OK, &[self.wake_on_wifi], 12),
            CMD_SET_WAKE_ON_WIFI => {
                let err = store_setting(req, &mut self.wake_on_wifi, WAKE_ON_WIFI_MAX);
                esp_reply(hdr, err, &[], 8)
            }
            // Power off / power cycle succeed and are recorded for the host (rpc_dispatch.c:165-185).
            CMD_MACHINE_OFF => {
                self.power_event = Some(PowerEvent::Off);
                esp_reply(hdr, ESP_OK, &[], 8)
            }
            CMD_MACHINE_REBOOT => {
                self.power_event = Some(PowerEvent::Reboot);
                esp_reply(hdr, ESP_OK, &[], 8)
            }
            // The quiet set keeps the UI flows free of error messages (doc 04 T0 §3).
            CMD_WIFI_DISCONNECT | CMD_MODEM_ON | CMD_MODEM_OFF | CMD_WIFI_ENABLE | CMD_WIFI_DISABLE | CMD_CLEAR_APS
            | CMD_WIFI_AUTOCONNECT => esp_reply(hdr, ESP_OK, &[], 8),
            // `cmd_not_implemented` (rpc_dispatch.c:366-372).
            _ => esp_reply(hdr, ESP_ERR_NOT_SUPPORTED, &[], 8),
        };
        Some(reply)
    }
}

/// Reply that starts like `rpc_espcmd_resp`: echoed header, `esp_err i32 @4`, then `body` @8, zero-padded to the
/// C struct size (rpc_calls.h:23-26).
fn esp_reply(hdr: &[u8], esp_err: i32, body: &[u8], size: usize) -> Vec<u8> {
    let mut reply = Vec::with_capacity(size);
    reply.extend_from_slice(hdr);
    reply.extend_from_slice(&esp_err.to_le_bytes());
    reply.extend_from_slice(body);
    reply.resize(size, 0);
    reply
}

/// SET_POWER_MODE / SET_WAKE_ON_WIFI: `value u8 @4`. A short request or an out-of-range value gets
/// ESP_ERR_INVALID_ARG and stores nothing (rpc_dispatch.c:316-325,343-353; power_state.c:84-86,104-106).
fn store_setting(req: &[u8], slot: &mut u8, max: u8) -> i32 {
    match req.get(HDR_LEN) {
        Some(&value) if req.len() >= SETTING_REQ_LEN && value <= max => {
            *slot = value;
            ESP_OK
        }
        _ => ESP_ERR_INVALID_ARG,
    }
}

/// Banner of the ROM loader after a reset into download mode, in the form of the ESP32-C3/S3 ROMs the U64-II
/// driver targets (`Esp32::Flash`, esp32.cc:349-370). `Esp32::Download` accepts the module once "DOWNLOAD" appears
/// in the first 100 bytes (esp32.cc:263-268).
const ROM_BANNER: &[u8] = b"ESP-ROM:esp32c3-api1-20210207\r\nBuild:Feb  7 2021\r\n\
    rst:0x1 (POWERON),boot:0x4 (DOWNLOAD(USB/UART0/1))\r\nwaiting for download\r\n";

// ESP ROM serial loader opcodes the updater sends (esp32.cc:45-50; SYNC in the frame at esp32.cc:233-239).
const ROM_FLASH_BEGIN: u8 = 0x02;
const ROM_FLASH_DATA: u8 = 0x03;
const ROM_FLASH_END: u8 = 0x04;
const ROM_SYNC: u8 = 0x08;
const ROM_SET_FLASH_PARAMS: u8 = 0x0B;
const ROM_ATTACH_SPI: u8 = 0x0D;
const ROM_CHANGE_BAUDRATE: u8 = 0x0F;
/// Status error of a request the loader does not accept (esptool `ROM_INVALID_RECV_MSG`).
const ROM_INVALID_RECV_MSG: u8 = 0x05;

/// Reply of the ESP ROM serial loader to one request `direction 0, opcode, size u16, checksum u32, data`
/// (`Esp32::Command`, esp32.cc:187-200). The response is `direction 1, opcode, size 4, value u32, status 4 bytes`:
/// the 12-byte SYNC answer `Download` compares (esp32.cc:240-241) and the test of `Command` (size 2 or 4, first
/// status byte 0 = success, esp32.cc:204-214). Every opcode the updater sends succeeds at once; the flashed data is
/// not kept and FLASH_DATA checksums are not checked. Anything else gets status `1, ROM_INVALID_RECV_MSG`.
fn rom_loader_reply(req: &[u8]) -> Vec<u8> {
    let op = req.get(1).copied().unwrap_or(0);
    let accepted = req.first() == Some(&0)
        && matches!(
            op,
            ROM_FLASH_BEGIN
                | ROM_FLASH_DATA
                | ROM_FLASH_END
                | ROM_SYNC
                | ROM_SET_FLASH_PARAMS
                | ROM_ATTACH_SPI
                | ROM_CHANGE_BAUDRATE
        );
    let status = if accepted { [0, 0] } else { [1, ROM_INVALID_RECV_MSG] };
    vec![1, op, 4, 0, 0, 0, 0, 0, status[0], status[1], 0, 0]
}

/// Maps the register window (0x10060900-0x100609FF; the 16-byte block repeats, doc 04 open question 6).
pub fn install(map: &mut IoMap, _cfg: &MachineConfig) {
    map.add(WIFI_UART_BASE, IO_GRAIN, Box::new(Wifi::new()));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::bus::RAM_SIZE;
    use crate::irq::IrqState;

    // Status bits and ictrl values used by the driver (dma_uart.cc:17-31).
    const RX_IRQ: u8 = 0x01;
    const TX_IRQ: u8 = 0x02;
    const BUF_IRQ: u8 = 0x04;
    const RX_IRQ_EN: u8 = 0x81;
    const TX_IRQ_EN: u8 = 0x82;
    const BUF_REQ_EN: u8 = 0x84;
    const TX_IRQ_DIS: u8 = 0x02;
    const BUF_REQ_DIS: u8 = 0x04;
    /// `command_buf_t.data` size (cmd_buffer.h:13-15) and the buffer counts (cmd_buffer.h:17-23).
    const CMD_BUF_SIZE: u32 = 1552;
    const NUM_BUFFERS: u32 = 12;
    /// Heap-like homes for the TX and RX pools (esp32.cc:69, linker.x:7).
    const TX_POOL: u32 = 0x0090_0000;
    const RX_POOL: u32 = 0x0098_0000;
    /// One FreeRTOS tick: the 200 Hz ITU timer period in clocks (00-memory-map B9).
    const TICK: u64 = 499_968;

    #[derive(Debug)]
    struct Buf {
        addr: u32,
        bufnr: u8,
        size: u16,
    }

    /// The device plus a model of the firmware side: `DmaUART` (dma_uart.cc) with its `cmd_buffer` pools and
    /// the `wifi_cmd` RPC wrappers.
    struct Board {
        dev: Wifi,
        ram: Vec<u8>,
        irq: IrqState,
        console: Vec<u8>,
        now: u64,
        sequence_nr: u16,
        free_tx: VecDeque<Buf>,
        free_rx: VecDeque<Buf>,
        transmit_queue: VecDeque<Buf>,
        rx_bufs: VecDeque<Buf>,
        current_tx: Option<Buf>,
        received: VecDeque<Buf>,
    }

    impl Board {
        fn new() -> Self {
            let mut board = Board {
                dev: Wifi::new(),
                ram: vec![0; RAM_SIZE],
                irq: IrqState::new(),
                console: Vec::new(),
                now: 0,
                sequence_nr: 0,
                free_tx: VecDeque::new(),
                free_rx: VecDeque::new(),
                transmit_queue: VecDeque::new(),
                rx_bufs: VecDeque::new(),
                current_tx: None,
                received: VecDeque::new(),
            };
            board.cmd_buffer_reset();
            board
        }

        /// `cmd_buffer_reset`: every buffer back in its free pool; TX bufnr 0..11, RX 0x40|i (cmd_buffer.c:11,34,45).
        fn cmd_buffer_reset(&mut self) {
            self.free_tx =
                (0..NUM_BUFFERS).map(|i| Buf { addr: TX_POOL + i * CMD_BUF_SIZE, bufnr: i as u8, size: 0 }).collect();
            self.free_rx = (0..NUM_BUFFERS)
                .map(|i| Buf { addr: RX_POOL + i * CMD_BUF_SIZE, bufnr: 0x40 | i as u8, size: 0 })
                .collect();
            self.transmit_queue.clear();
            self.received.clear();
        }

        fn wr(&mut self, off: u32, val: u8) {
            let mut ctx =
                IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            self.dev.write8(off, val, &mut ctx);
        }

        fn rd(&mut self, off: u32) -> u8 {
            let mut ctx =
                IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut self.ram, irq: &mut self.irq, console: &mut self.console };
            self.dev.read8(off, &mut ctx)
        }

        /// `sh`/`sw` through wishbone2memio: LE bytes at increasing addresses.
        fn wr16(&mut self, off: u32, val: u16) {
            for (i, byte) in val.to_le_bytes().into_iter().enumerate() {
                self.wr(off + i as u32, byte);
            }
        }

        fn wr32(&mut self, off: u32, val: u32) {
            for (i, byte) in val.to_le_bytes().into_iter().enumerate() {
                self.wr(off + i as u32, byte);
            }
        }

        fn rd16(&mut self, off: u32) -> u16 {
            u16::from_le_bytes([self.rd(off), self.rd(off + 1)])
        }

        fn level(&self) -> bool {
            self.irq.high_src & (1 << WIFI_IRQ_HIGH_BIT) != 0
        }

        fn frame(&self, buf: &Buf) -> &[u8] {
            &self.ram[buf.addr as usize..buf.addr as usize + buf.size as usize]
        }

        /// `DmaUART` ctor run from the `Esp32` static constructor (dma_uart.h:74-80, 00-memory-map A7).
        fn ctor(&mut self) {
            self.rd(0);
            self.rd(1);
            self.wr(3, 0x80);
            self.wr(3, 0x80);
            // install_high_irq(3): read-modify-write of ITU_IRQ_HIGH_EN (riscv_main.c:38-45).
            self.irq.high_en |= 1 << WIFI_IRQ_HIGH_BIT;
        }

        /// vTaskStartScheduler: HIGH_EN <- 0, then the ITU is enabled globally (riscv_main.c:178-186).
        fn scheduler_start(&mut self) {
            self.irq.high_en = 0;
            self.irq.global_en = true;
        }

        /// `wifi_command_init` register sequence (wifi_cmd.cc:32-44), without taking the IRQ.
        fn command_init_registers(&mut self) {
            // SetBaudRate(5000000) (dma_uart.cc:139-144).
            self.wr(1, 0x00);
            self.wr(0, 0x13);
            // FlowControl(true) (dma_uart.cc:91).
            let flow = self.rd(3);
            self.wr(3, flow | 0x01);
            // ClearRxBuffer (dma_uart.cc:117-120).
            let flow = self.rd(3);
            self.wr(3, flow | 0x80);
            self.rx_bufs.clear();
            let flow = self.rd(3);
            self.wr(3, flow | 0x80);
            self.cmd_buffer_reset();
            // EnableSlip(true) (dma_uart.cc:101).
            let flow = self.rd(3);
            self.wr(3, flow | 0x04);
            // EnableIRQ(true) (dma_uart.cc:72-73).
            self.wr(2, RX_IRQ_EN | BUF_REQ_EN);
            self.irq.high_en |= 1 << WIFI_IRQ_HIGH_BIT;
        }

        /// Boot to the IDENTIFY handshake: A7, scheduler start, `WiFi::Init` → `StartApp`, then `RunModeThread`
        /// with `ModuleCtrl(ESP_MODE_RUN)` and a second init (wifi.cc:41-51,236-247).
        fn booted() -> Self {
            let mut board = Board::new();
            board.ctor();
            board.scheduler_start();
            board.command_init_registers();
            board.take_irq();
            let flow = board.rd(3);
            board.wr(3, flow & 0x8F);
            board.command_init_registers();
            board.take_irq();
            board
        }

        /// `DmaUART::DmaUartInterrupt` (dma_uart.cc:149-228), with `wifi_rx_isr` queuing into `received`.
        fn isr(&mut self) {
            for _ in 0..64 {
                let status = self.rd(2) & (RX_IRQ | TX_IRQ | BUF_IRQ);
                if status == 0 {
                    return;
                }
                if status & BUF_IRQ != 0 {
                    match self.free_rx.pop_front() {
                        Some(buf) => {
                            let addr = buf.addr;
                            self.rx_bufs.push_back(buf);
                            self.wr32(0xC, addr);
                            self.wr(2, RX_IRQ_EN);
                        }
                        None => {
                            self.console.push(b'^');
                            self.wr(2, BUF_REQ_DIS);
                        }
                    }
                }
                if status & TX_IRQ != 0 {
                    if let Some(buf) = self.current_tx.take() {
                        self.free_tx.push_back(buf);
                        self.wr(2, BUF_REQ_EN);
                    } else if let Some(buf) = self.transmit_queue.pop_front() {
                        self.wr32(4, buf.addr);
                        self.wr16(8, buf.size);
                        self.wr(0xA, 1);
                        self.current_tx = Some(buf);
                    } else {
                        self.wr(2, TX_IRQ_DIS);
                    }
                }
                if status & RX_IRQ != 0 {
                    match self.rx_bufs.pop_front() {
                        Some(mut buf) => {
                            buf.size = self.rd16(8);
                            self.received.push_back(buf);
                        }
                        None => self.console.push(b'!'),
                    }
                    self.wr(0xB, 1);
                }
            }
            panic!("DmaUartInterrupt livelock, status {:#04x}", self.rd(2));
        }

        /// The CPU trap on the ITU line (riscv_main.c:118-129). Once the ISR returns the line must be low, or
        /// the trap re-enters forever (H1, H2).
        fn take_irq(&mut self) {
            if self.irq.line() {
                self.isr();
                assert!(!self.irq.line(), "IRQ storm: the line is still high after the ISR");
            }
        }

        /// Let `clocks` pass like the machine loop does: tick at `next_event`, take the IRQ when it rises.
        fn run(&mut self, clocks: u64) {
            let end = self.now + clocks;
            for _ in 0..1000 {
                match self.dev.next_event() {
                    Some(at) if at <= end => {
                        self.now = self.now.max(at);
                        let mut ctx = IoCtx { stall: 0,
                            now: self.now,
                            pc: 0,
                            ram: &mut self.ram,
                            irq: &mut self.irq,
                            console: &mut self.console,
                        };
                        self.dev.tick(&mut ctx);
                        self.take_irq();
                    }
                    _ => {
                        self.now = end;
                        return;
                    }
                }
            }
            panic!("next_event does not advance");
        }

        /// BUFARGS + `TransmitPacket` (wifi_cmd.h:68-79, dma_uart.cc:258-270). Returns the thread id (= bufnr).
        fn request(&mut self, cmd: u8, args: &[u8]) -> u8 {
            let mut buf = self.free_tx.pop_front().expect("free TX buffer");
            let at = buf.addr as usize;
            self.ram[at] = cmd;
            self.ram[at + 1] = buf.bufnr;
            self.ram[at + 2..at + 4].copy_from_slice(&self.sequence_nr.to_le_bytes());
            self.ram[at + 4..at + 4 + args.len()].copy_from_slice(args);
            self.sequence_nr = self.sequence_nr.wrapping_add(1);
            buf.size = (HDR_LEN + args.len()) as u16;
            let thread = buf.bufnr;
            self.transmit_queue.push_back(buf);
            self.wr(2, TX_IRQ_EN);
            self.take_irq();
            thread
        }

        /// `xTaskNotifyWait` with a timeout in FreeRTOS ticks.
        fn wait_reply(&mut self, ticks: u64) -> Buf {
            let deadline = self.now + ticks * TICK;
            while self.received.is_empty() && self.now < deadline {
                self.run(CLOCKS_PER_MS);
            }
            self.received.pop_front().expect("reply within the timeout")
        }

        /// `ModuleCtrl` (dma_uart.cc:107-111) and `EnableSlip` (dma_uart.cc:96-105): read-modify-write of flowctrl.
        fn module_ctrl(&mut self, mode: u8) {
            let flow = self.rd(3);
            self.wr(3, (flow & 0x8F) | (mode << 4));
        }

        fn enable_slip(&mut self, on: bool) {
            let flow = self.rd(3);
            self.wr(3, if on { flow | FLOW_SLIP } else { flow & !FLOW_SLIP });
            self.take_irq();
        }

        /// `SendSlipPacket` (dma_uart.cc:242-255): raw bytes in a TX buffer.
        fn send(&mut self, bytes: &[u8]) {
            let mut buf = self.free_tx.pop_front().expect("free TX buffer");
            self.ram[buf.addr as usize..buf.addr as usize + bytes.len()].copy_from_slice(bytes);
            buf.size = bytes.len() as u16;
            self.transmit_queue.push_back(buf);
            self.wr(2, TX_IRQ_EN);
            self.take_irq();
        }

        /// `DmaUART::FreeBuffer` (dma_uart.cc:283-291).
        fn free_buffer(&mut self, buf: Buf) {
            self.free_rx.push_back(buf);
            self.wr(2, BUF_REQ_EN);
            self.take_irq();
        }

        /// One blocking RPC as the `wifi_cmd.cc` wrappers do it; returns a copy of the reply frame.
        fn rpc(&mut self, cmd: u8, args: &[u8]) -> Vec<u8> {
            let thread = self.request(cmd, args);
            let buf = self.wait_reply(100);
            let reply = self.frame(&buf).to_vec();
            assert_eq!(reply[..2], [cmd, thread], "command and thread echoed (H5, H10)");
            self.free_buffer(buf);
            reply
        }
    }

    fn le16(frame: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(frame[at..at + 2].try_into().unwrap())
    }

    fn le32(frame: &[u8], at: usize) -> i32 {
        i32::from_le_bytes(frame[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn wifi_ctor_register_access() {
        let mut b = Board::new();
        assert_eq!(b.rd(2), 0xD0, "idle status: cts, tx ready, needs an RX address");
        assert_eq!(b.rd(3), FLOW_RESET_VALUE);
        b.ctor();
        assert_eq!(b.rd(0), DIVISOR_RESET_VALUE as u8);
        assert_eq!(b.rd(1), 0);
        assert_eq!(b.rd(3), 0x00, "flowctrl b7 reads 0 after the reset strobe (H3)");
        assert_eq!(b.rd(2), 0xD0);
        assert!(!b.level());
    }

    #[test]
    fn wifi_enable_irq_arms_two_rx_slots_and_drops_level() {
        let mut b = Board::new();
        b.ctor();
        b.scheduler_start();
        b.command_init_registers();
        assert_eq!(b.rd(0), 0x13);
        assert_eq!(b.rd(3), 0x05, "HW flow control + SLIP, no reset bit");
        assert_eq!(b.rd(2) & 7, BUF_IRQ, "buffer request pending after EnableIRQ");
        assert!(b.level() && b.irq.line());

        b.take_irq();
        assert_eq!(b.rx_bufs.len(), 2, "one address latched by rx_dma, one pending (H11)");
        assert_eq!(b.rd(2), 0x50, "no IRQ source, receiver armed");
        assert!(!b.level());
        assert!(b.console.is_empty());
    }

    #[test]
    fn wifi_identify_round_trip() {
        let mut b = Board::booted();
        let first_armed = b.rx_bufs[0].bufnr;
        let start = b.now;
        let thread = b.request(CMD_IDENTIFY, &[]);
        assert!(b.current_tx.is_none() && b.free_tx.len() == 12, "TX completed and freed by the ISR (H6)");
        assert!(b.received.is_empty() && !b.level(), "the reply comes later");

        let buf = b.wait_reply(100);
        assert!(b.now - start >= REPLY_DELAY && b.now - start < 100 * TICK);
        assert_eq!(buf.bufnr, first_armed);
        let reply = b.frame(&buf).to_vec();
        assert_eq!(reply.len(), 33);
        assert_eq!(reply[..4], [CMD_IDENTIFY, thread, 0, 0]);
        assert_eq!((le16(&reply, 4), le16(&reply, 6)), (1, 14));
        assert_eq!(&reply[8..32], b"ESP32 WiFi Bridge V1.14\0");

        assert!(!b.level(), "rx_pop acked the level");
        assert_eq!(b.rx_bufs.len(), 2, "the ISR re-armed the used slot");
        b.free_buffer(buf);
        assert!(!b.level());
        assert!(b.console.is_empty());
    }

    #[test]
    fn wifi_getmac_reply() {
        let mut b = Board::booted();
        let reply = b.rpc(CMD_WIFI_GETMAC, &[]);
        assert_eq!(reply.len(), 16);
        assert_eq!(le32(&reply, 4), ESP_OK);
        assert_eq!(reply[8..14], [0x02, 0x15, 0x41, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn wifi_unknown_command_gets_not_supported() {
        let mut b = Board::booted();
        let reply = b.rpc(0x14, b"0123456789abcdef");
        assert_eq!(reply.len(), 8);
        assert_eq!(le32(&reply, 4), 0x106);
        let reply = b.rpc(0x7F, &[]);
        assert_eq!((reply.len(), le32(&reply, 4)), (8, 0x106));
        let reply = b.rpc(CMD_WIFI_ENABLE, &[]);
        assert_eq!((reply.len(), le32(&reply, 4)), (8, ESP_OK), "quiet set answers 0");
        let reply = b.rpc(CMD_WIFI_IS_CONNECTED, &[]);
        assert_eq!((reply.len(), le32(&reply, 4), reply[8]), (12, ESP_OK, 0));
    }

    #[test]
    fn wifi_scan_error_reports_zero_records() {
        let mut b = Board::booted();
        // Leave stale IDENTIFY text in the RX buffers, like a running system would.
        b.rpc(CMD_IDENTIFY, &[]);
        b.rpc(CMD_IDENTIFY, &[]);
        let reply = b.rpc(CMD_WIFI_SCAN, &[]);
        assert_eq!(reply.len(), 10);
        assert_eq!(le32(&reply, 4), 0x106);
        assert_eq!(reply[8..10], [0, 0], "num_records and reserved");
    }

    #[test]
    fn wifi_send_packet_is_consumed_silently() {
        let mut b = Board::booted();
        let mut args = 4u32.to_le_bytes().to_vec();
        args.extend_from_slice(&[1, 2, 3, 4]);
        b.request(CMD_SEND_PACKET, &args);
        b.run(50 * CLOCKS_PER_MS);
        assert!(b.received.is_empty(), "no reply to SEND_PACKET (H10)");
        assert_eq!(b.free_tx.len(), 12);
        assert!(!b.level());
        assert_eq!(b.dev.next_event(), None);
    }

    #[test]
    fn wifi_voltages_and_power_settings() {
        let mut b = Board::booted();
        let reply = b.rpc(CMD_GET_VOLTAGES, &[]);
        assert_eq!(reply.len(), 24);
        assert_eq!(le32(&reply, 4), ESP_OK);
        assert_eq!((8..22).step_by(2).map(|at| le16(&reply, at)).collect::<Vec<_>>(), VOLTAGES_MV);

        // H8: stored power mode reads back.
        let reply = b.rpc(CMD_GET_POWER_MODE, &[]);
        assert_eq!((reply.len(), le32(&reply, 4), reply[8], reply[9]), (12, ESP_OK, 0, 1));
        assert_eq!(le32(&b.rpc(CMD_SET_POWER_MODE, &[2, 0]), 4), ESP_OK);
        assert_eq!(le32(&b.rpc(CMD_SET_POWER_MODE, &[3, 0]), 4), ESP_ERR_INVALID_ARG);
        assert_eq!(le32(&b.rpc(CMD_SET_POWER_MODE, &[1]), 4), ESP_ERR_INVALID_ARG, "short request");
        assert_eq!(b.rpc(CMD_GET_POWER_MODE, &[])[8], 2);

        // H9: wake on WiFi.
        assert_eq!(b.rpc(CMD_GET_WAKE_ON_WIFI, &[])[8], 0);
        assert_eq!(le32(&b.rpc(CMD_SET_WAKE_ON_WIFI, &[1, 0]), 4), ESP_OK);
        assert_eq!(le32(&b.rpc(CMD_SET_WAKE_ON_WIFI, &[2, 0]), 4), ESP_ERR_INVALID_ARG);
        let reply = b.rpc(CMD_GET_WAKE_ON_WIFI, &[]);
        assert_eq!((reply.len(), reply[8]), (12, 1));
    }

    #[test]
    fn wifi_power_requests_are_recorded_for_the_host() {
        let mut b = Board::booted();
        assert_eq!(b.dev.ctrl.power_event, None);
        let reply = b.rpc(CMD_MACHINE_REBOOT, &[]);
        assert_eq!((reply.len(), le32(&reply, 4)), (8, ESP_OK));
        assert_eq!(b.dev.ctrl.power_event.take(), Some(PowerEvent::Reboot));
        let reply = b.rpc(CMD_MACHINE_OFF, &[]);
        assert_eq!((reply.len(), le32(&reply, 4)), (8, ESP_OK));
        b.dev.reset();
        assert_eq!(b.dev.ctrl.power_event, Some(PowerEvent::Off), "kept until the host takes it");
    }

    #[test]
    fn wifi_rom_loader_download_and_flash() {
        let mut b = Board::booted();
        // `Esp32::Boot` (esp32.cc:155-166): module off, SLIP off, IRQs on, then boot mode.
        b.module_ctrl(3);
        let flow = b.rd(3);
        b.wr(3, flow | 0x80);
        b.rx_bufs.clear();
        b.cmd_buffer_reset();
        b.enable_slip(false);
        b.wr(2, RX_IRQ_EN | BUF_REQ_EN);
        b.take_irq();
        b.module_ctrl(1);
        b.run(100 * TICK);
        assert!(b.received.is_empty(), "the raw banner waits in the decoder");

        // `Download` (esp32.cc:253-268): SLIP on and off again forces the banner out as one frame.
        b.enable_slip(true);
        b.enable_slip(false);
        let buf = b.received.pop_front().expect("banner frame");
        let banner = b.frame(&buf).to_vec();
        b.free_buffer(buf);
        assert!(banner[..100].windows(8).any(|w| w == b"DOWNLOAD"), "{}", String::from_utf8_lossy(&banner));
        assert_eq!(banner.last(), Some(&b'\n'));

        // SYNC and a command (esp32.cc:232-245, 187-214).
        b.enable_slip(true);
        let mut sync = vec![0x00, ROM_SYNC, 0x24, 0x00, 0, 0, 0, 0, 0x07, 0x07, 0x12, 0x20];
        sync.extend_from_slice(&[0x55; 32]);
        b.send(&sync);
        let buf = b.wait_reply(200);
        let mut reply = b.frame(&buf).to_vec();
        b.free_buffer(buf);
        reply[4..8].fill(0xEE);
        assert_eq!(reply, [0x01, 0x08, 0x04, 0x00, 0xEE, 0xEE, 0xEE, 0xEE, 0x00, 0x00, 0x00, 0x00]);
        let mut attach = vec![0x00, ROM_ATTACH_SPI, 8, 0, 0, 0, 0, 0];
        attach.extend_from_slice(&[0; 8]);
        b.send(&attach);
        let buf = b.wait_reply(500);
        let reply = b.frame(&buf).to_vec();
        b.free_buffer(buf);
        assert_eq!((reply[0], reply[1], reply[2], reply[8]), (1, ROM_ATTACH_SPI, 4, 0), "success");
        b.send(&[0x00, 0x99, 0, 0, 0, 0, 0, 0]);
        let buf = b.wait_reply(200);
        assert_eq!(b.frame(&buf)[8..10], [1, ROM_INVALID_RECV_MSG]);
        b.free_buffer(buf);

        // `EnableRunMode` + `wifi_command_init`: the control module answers again.
        b.module_ctrl(3);
        b.module_ctrl(0);
        b.command_init_registers();
        b.take_irq();
        assert_eq!(b.rpc(CMD_IDENTIFY, &[]).len(), 33);
        assert!(b.console.is_empty());
    }

    #[test]
    fn wifi_module_restart_drops_frames_and_banner_needs_slip_off() {
        let mut b = Board::booted();
        b.request(CMD_IDENTIFY, &[]);
        b.module_ctrl(3);
        b.run(10 * CLOCKS_PER_MS);
        assert!(b.received.is_empty(), "the reply died with the module");
        // Boot mode with SLIP still on: the decoder drops the unframed banner.
        b.module_ctrl(1);
        b.enable_slip(false);
        b.enable_slip(true);
        b.run(10 * CLOCKS_PER_MS);
        assert!(b.received.is_empty());
    }

    #[test]
    fn wifi_replies_fill_rx_buffers_in_arm_order() {
        let mut b = Board::booted();
        let armed: Vec<u8> = b.rx_bufs.iter().map(|buf| buf.bufnr).collect();
        let t1 = b.request(CMD_IDENTIFY, &[]);
        let t2 = b.request(CMD_WIFI_GETMAC, &[]);
        b.run(10 * CLOCKS_PER_MS);
        assert_eq!(b.received.len(), 2);
        let got: Vec<u8> = b.received.iter().map(|buf| buf.bufnr).collect();
        assert_eq!(got, armed, "oldest armed address first (H11)");
        assert_eq!(b.frame(&b.received[0])[..2], [CMD_IDENTIFY, t1]);
        assert_eq!(b.frame(&b.received[1])[..2], [CMD_WIFI_GETMAC, t2]);
        assert_eq!(b.received[1].size, 16);
        assert!(!b.level());
    }

    #[test]
    fn wifi_reply_waits_for_delay_then_for_rx_buffer() {
        let mut b = Board::new();
        b.ctor();
        // GLOBAL resets to 1 (itu.vhd:256): keep the harness ISR out so the raw RX registers can be inspected.
        b.irq.global_en = false;
        let tx = TX_POOL as usize;
        b.ram[tx..tx + 4].copy_from_slice(&[CMD_WIFI_GETMAC, 5, 0x34, 0x12]);
        b.wr32(4, TX_POOL);
        b.wr16(8, 4);
        b.wr(0xA, 1);
        b.run(20 * CLOCKS_PER_MS);
        assert_eq!(b.rd(2) & ST_RX_VALID, 0, "no RX buffer armed: the reply is held");
        assert_eq!(b.dev.next_event(), None, "a held reply waits for a register write");

        b.wr(2, RX_IRQ_EN);
        assert!(!b.level());
        b.wr32(0xC, RX_POOL);
        assert_eq!(b.rd(2) & (ST_RX_VALID | RX_IRQ), ST_RX_VALID | RX_IRQ, "delivered when armed");
        assert!(b.level());
        assert_eq!(b.rd16(8), 16);
        let rx = RX_POOL as usize;
        assert_eq!(b.ram[rx..rx + 4], [CMD_WIFI_GETMAC, 5, 0x34, 0x12]);
        b.wr(0xB, 1);
        assert!(!b.level(), "rx_pop drops the level");
        assert_eq!(b.rd(2), 0xD0);

        // Armed first: the reply lands only after REPLY_DELAY, driven by next_event/tick.
        b.wr32(0xC, RX_POOL + CMD_BUF_SIZE);
        b.wr(0xA, 1);
        let due = b.now + REPLY_DELAY;
        assert_eq!(b.dev.next_event(), Some(due));
        assert_eq!(b.rd(2) & ST_RX_VALID, 0);
        b.run(REPLY_DELAY - 1);
        assert_eq!(b.rd(2) & ST_RX_VALID, 0);
        b.run(1);
        assert_eq!(b.rd(2) & ST_RX_VALID, ST_RX_VALID);
        assert!(b.level());
    }

    #[test]
    fn wifi_flowctrl_rmw_keeps_enables_and_reset_strobe_clears_them() {
        let mut b = Board::new();
        b.wr(2, 0x87);
        b.wr32(0xC, RX_POOL);
        assert_eq!(b.rd(2) & 7, TX_IRQ | BUF_IRQ, "address taken by rx_dma, register wants the next one");
        // ModuleCtrl(ESP_MODE_RUN) is read-modify-write and must not re-strobe the reset (dma_uart.cc:110, H3).
        let flow = b.rd(3);
        b.wr(3, flow & 0x8F);
        assert_eq!(b.rd(2) & 7, TX_IRQ | BUF_IRQ);
        assert!(b.level());
        // ClearRxBuffer: reset strobe clears the enables and flushes the receiver.
        let flow = b.rd(3);
        b.wr(3, flow | 0x80);
        assert_eq!(b.rd(2), 0xD0);
        assert!(!b.level());
        // The flushed address is gone: a reply has nowhere to go.
        let tx = TX_POOL as usize;
        b.ram[tx..tx + 4].copy_from_slice(&[CMD_IDENTIFY, 0, 0, 0]);
        b.wr32(4, TX_POOL);
        b.wr16(8, 4);
        b.wr(0xA, 1);
        b.run(10 * CLOCKS_PER_MS);
        assert_eq!(b.rd(2) & ST_RX_VALID, 0);
    }

    #[test]
    fn wifi_empty_rx_pool_does_not_storm() {
        let mut b = Board::booted();
        let spare: Vec<Buf> = b.free_rx.drain(..).collect();
        b.request(CMD_IDENTIFY, &[]);
        let buf = b.wait_reply(100);
        assert_eq!(b.console, b"^", "no free RX buffer: BufReq disabled, level dropped");
        assert!(!b.level());
        assert_eq!(b.rx_bufs.len(), 1);
        b.free_rx.extend(spare);
        b.free_buffer(buf);
        assert_eq!(b.rx_bufs.len(), 2, "FreeBuffer re-enables BufReq and the ISR arms again");
        assert!(!b.level());
    }

    #[test]
    fn wifi_window_repeats_and_install_maps_it() {
        let mut map = IoMap::new();
        install(&mut map, &MachineConfig::new(PathBuf::new(), PathBuf::new()));
        let (idx, off) = map.resolve(WIFI_UART_BASE + 0xF2).expect("wifi window mapped");
        assert_eq!((map.devices[idx].name(), off), ("wifi", 0xF2));
        assert_eq!(map.resolve(WIFI_UART_BASE + IO_GRAIN), None);
        assert!(map.get::<Wifi>().is_some());

        let mut b = Board::new();
        assert_eq!(b.rd(0xF2), b.rd(0x02));
        b.wr(0x13, 0x04);
        assert_eq!(b.rd(0x03), 0x04);
    }

    #[test]
    fn wifi_reset_keeps_module_settings() {
        let mut b = Board::booted();
        b.rpc(CMD_SET_WAKE_ON_WIFI, &[1, 0]);
        b.request(CMD_IDENTIFY, &[]);
        b.dev.reset();
        assert_eq!(b.dev.ctrl.wake_on_wifi, 1);
        assert_eq!((b.rd(2), b.rd(3)), (0xD0, FLOW_RESET_VALUE));
        assert_eq!(b.dev.next_event(), None);
    }
}
