# S05 — WiFi DMA UART + stub u64ctrl

**Status:** built.

**Owns:** `crates/ue2-core/src/devices/wifi.rs`
**Reads:** `docs/hw/04-esp32-wifi.md` (all, T0 exact), `docs/hw/00-memory-map.md` §2 A7, C25, §Interrupts high bit 3; firmware sources cited there (`software/io/network/dma_uart.*`, `esp32.cc`, `wifi_cmd.cc`, `software/u64ctrl/main/*` for the reply formats)

## Scope

- **Window:** `0x10060900-0x100609FF` (registers repeat per doc 04).
- **Register model** per doc 04 T0 §1:
  - live status bits (H1);
  - `flowctrl` read-back with bit 7 = 0 (H3);
  - soft reset;
  - the two-slot TX/RX DMA against `ctx.ram[addr & 0x03FFFFFF]`;
  - TX completion (H6).
- **IRQ:** level on high bit 3 via `ctx.irq.set_high(3, …)`, computed from the status and interrupt enables
  exactly as documented. It must drop once the firmware acks, so the ISR never storms.
- **Stub u64ctrl** (doc 04 T0 §3):
  - Parse SLIP/RPC frames from TX buffers and queue reply frames with the thread id echoed.
  - Deliver replies into armed RX buffers after a short emulated delay (e.g. 1-2 ms via `next_event`/`tick`).
  - Hold replies while no RX buffer is armed.
  - Implement the listed commands with the listed payloads (IDENTIFY "ESP32 WiFi Bridge V1.14", voltages,
    MAC 02:15:41:00:00:01, IS_CONNECTED = 0, …).
  - 0x08 is consumed silently. Everything else gets `esp_err 0x106`, or 0 for the quiet set.
- Structure the command handler so S12 can later add a real L2 bridge (0x08 packets, EVENT_RECV_PACKET).

## Tests

- Firmware-shaped sequences built from the byte layouts in doc 04:
  - ctor register access (A7);
  - EnableIRQ;
  - arming an RX buffer;
  - sending IDENTIFY in a TX buffer → IRQ → the RX buffer holds a well-formed reply → ack drops the IRQ.
- GETMAC reply; unknown command → 0x106; no IRQ storm (level low after ack).

## Acceptance

`cargo test -p ue2-core wifi` passes.
