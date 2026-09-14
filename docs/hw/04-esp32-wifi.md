# ESP32 companion (u64ctrl) UART link and WiFi

Scope: the DMA UART at `WIFI_UART_BASE` on U64-II, the SLIP/RPC protocol to the ESP32 control module
("u64ctrl"), the WiFi application task, and every firmware feature that depends on the module.
Build: `target/u64ii/riscv/ultimate` with `-DRISCV -DU64=2 -DIOBASE=0x10000000 -DCLOCK_FREQ=100000000`.
Paths below are relative to `firmware/1541ultimate/`. Symbol addresses come from the local build
`target/u64ii/riscv/ultimate/result/ultimate.elf` (made by `scripts/build-firmware.sh`) and are only valid for that build.

## Sources read

Firmware side (all listed in `SRCS_CC` of `target/u64ii/riscv/ultimate/Makefile`: `dma_uart.cc esp32.cc network_esp32.cc wifi_cmd.cc wifi.cc`, and `cmd_buffer.c` in `SRCS_C`):
- `software/io/uart/dma_uart.h`, `dma_uart.cc`: `DmaUART` ctor, `EnableIRQ`, `FlowControl`, `EnableSlip`, `ModuleCtrl`, `ClearRxBuffer`, `SetBaudRate`, `DmaUartInterrupt`, `TransmitPacket`, `FreeBuffer`, `GetBuffer`.
- `software/io/uart/cmd_buffer.h`, `cmd_buffer.c`: buffer pools and queues.
- `software/io/wifi/esp32.h`, `esp32.cc`: `Esp32` object, `StartApp`/`StopApp`. The ROM-bootloader code (`Download`, `Flash`, `DetectModule`, `EnableRunMode`, `Boot`, `Quit`, `ReadRxMessage`) is garbage-collected out of the ELF (nm shows only `Esp32::Esp32`, `AttachApplication`, `StartApp`, `StopApp`).
- `software/io/wifi/wifi_cmd.h`, `wifi_cmd.cc`: RPC wrappers, `wifi_rx_isr`, `wifi_command_init`.
- `software/io/wifi/wifi.h`, `wifi.cc`: `WiFi::Init` (InitFunction), `WiFi::RunModeThread`, `wifi_tx_packet`.
- `software/io/network/network_esp32.h`, `network_esp32.cc`: lwIP netif wrapper, config/menu actions.
- `software/u64ctrl/main/rpc_calls.h`: wire structures and command codes (included by the Ultimate side through VPATH `$(PATH_SW)/u64ctrl/main`, Makefile).
- Consumers: `software/u64/u64_config.cc` (power-on mode, wake-on-WiFi), `software/io/c64/c64_subsys.cc` (power off / power cycle), `software/api/route_machine.cc`, `software/network/socket_dma.cc`, `software/userinterface/configio.cc`, `software/api/routes.cc` (`wifi_mac`), `software/userinterface/browsable_root.h`.
- Infrastructure: `software/system/iomap.h`, `software/system/itu.h`, `software/portable/riscv/riscv_main.c`, `software/portable/riscv/crt0.S`, `software/components/init_function.cc`, `software/components/config.cc`, `software/FreeRTOS/Source/FreeRTOSConfig.h`.

FPGA (open IP, assumed to be what the closed U64-II top level instantiates, see Open questions):
- `fpga/io/uart_lite/vhdl_source/uart_dma.vhd`, `slip_encoder.vhd`, `slip_decoder.vhd`, `rx_dma.vhd`, `tx_dma.vhd`.
- `fpga/io/itu/vhdl_source/itu.vhd` (high-IRQ mask/active).
- `fpga/cpu_unit/vhdl_source/wishbone2memio.vhd` (CPU bus to io_bus byte sequencing).

ESP32 side (reference for the functional model; not run by the emulator):
- `software/u64ctrl/main/rpc_dispatch.c/.h`, `my_uart.c/.h`, `wifi_modem.c`, `button_handler.c/.h`, `power_state.h`, `control_main.c`, `pinout.h`.

## Address map

`WIFI_UART_BASE = IOBASE + 0x60900 = 0x10060900` (`iomap.h:32`, IOBASE from the Makefile define).
The register struct is `dma_uart_t` (`dma_uart.h:21-35`). Decoding uses `io_req.address(3 downto 0)` (`uart_dma.vhd:293,372`), so the block is 16 bytes. Upper-address aliasing is unknown (closed top level).

| Absolute addr | Width (CPU access) | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10060900 | 8 (`sb`) | W / R | `rate_l` / divisor(7:0) | Baud divisor low. FW writes it after `rate_h` (disasm `SetBaudRate` @0xc7944/0xc7948). Divisor = `((2*CLOCK_FREQ/bps)+1)/2 - 1` (`dma_uart.cc:139-144`). 5 000 000 bps gives 19 (0x13). Reset value `g_divisor-1` (`uart_dma.vhd:424`). |
| 0x10060901 | 8 | W / R | `rate_h` | W: bits 2:0 are divisor(10:8) (`uart_dma.vhd:297-298`). R: bits 1:0 are divisor(9:8) (`uart_dma.vhd:376-377`). The ctor reads +0/+1 and discards the value (`dma_uart.h:74`). |
| 0x10060902 | 8 | W | `ictrl` (imask) | Bit 7 = 1: OR bits 0/1/2 into the rx/tx/buf IRQ enables. Bit 7 = 0: clear those enables. Bit 3 = 1: clear overflow (`uart_dma.vhd:300-312`). FW values: 0x85 enables Rx+BufReq, 0x81 Rx, 0x82 Tx, 0x84 BufReq, 0x01/0x02/0x04 disable (`dma_uart.cc:26-31`), 0x08 clears overflow (`dma_uart.h:44,85`). |
| 0x10060902 | 8 (`lbu`) | R | `status` | b0 rx_irq = rx_en & len_valid. b1 tx_irq = tx_en & tx_addr_ready. b2 buf_irq = buf_en & !rx_addr_valid. b3 overflow. b4 cts_c. b5 len_valid (packet received). b6 tx_addr_ready (TX DMA idle). b7 !rx_addr_valid (needs an RX address) (`uart_dma.vhd:379-387,433-436`). The ISR uses only bits 0-2 (`dma_uart.cc:160`). |
| 0x10060903 | 8 (RMW) | W / R | `flowctrl` | b0 HW flow control (cts_enable), b1 loopback, b2 SLIP enable, b4 MOD_BOOT, b5 MOD_ENABLE, b6 route. Every write sets all seven fields. b7 = soft-reset strobe: flushes rxfifo/tx_dma/rx_dma and clears all three IRQ enables (`uart_dma.vhd:314-326`, `dma_uart.h:37-42`). Read returns b0,b1,b2,b4,b5,b6; **b3 and b7 read as 0** (`uart_dma.vhd:389-395`). |
| 0x10060904-07 | 32 (`sw`, bridged to 4 byte writes +4..+7) | W | `tx_addr` | Pointer to TX buffer data. Only 28 bits are latched (`uart_dma.vhd:328-338`) and tx_dma uses 26 (`tx_dma.vhd:51`). |
| 0x10060908-09 | 16 (`sh`) | W | `length` (tx) | TX length in bytes (`uart_dma.vhd:340-344`). FW store: disasm `DmaUartInterrupt` @0xc7acc. |
| 0x10060908-09 | 16 (`lhu`) | R | `length` (rx) | Length of the received packet (`uart_dma.vhd:397-401`). FW load @0xc7a60. |
| 0x1006090A | 8 | W (any value) | `tx_push` | Hands tx_addr/length to tx_dma (`uart_dma.vhd:346-347`). FW writes 1 (`dma_uart.cc:194`). |
| 0x1006090B | 8 | W (any value) | `rx_pop` | `rx_len_ready` pulse; clears len_valid (`uart_dma.vhd:349-350`, `rx_dma.vhd:72-74`). FW writes 1 (`dma_uart.cc:223`). |
| 0x1006090C-0F | 32 (`sw`, bytes +C..+F in order) | W | `rx_addr` | RX buffer pointer. **The write to +F sets rx_addr_valid** (`uart_dma.vhd:352-363`). |
| 0x10000027 | 8 (RMW) | W / R | `ITU_IRQ_HIGH_EN` | Bit 3 = WiFi UART (`itu.h:29,43`). Set by `install_high_irq` in the ctor (`riscv_main.c:43`), zeroed at scheduler start (`riscv_main.c:178`), set again by `EnableIRQ(true)` (`dma_uart.cc:73`). |
| 0x10000028 | 8 | R | `ITU_IRQ_HIGH_ACT` | `irq_high & imask_high` (`itu.vhd:227-228`). Read by the trap handler (`riscv_main.c:118`). |
| 0x10000010 | 8 | W | `UART_DATA` | Debug characters written from ISR context: `N` (reply routed) and `~` (event queued) (`wifi_cmd.cc:23,26`), `^` (no free RX buffer) (`dma_uart.cc:179`), `!` (RX IRQ without armed buffer) (`dma_uart.cc:221`). |
| 0x00000000-0x03FFFFFF | 32 bus master | R/W | DMA window | tx_dma reads and rx_dma writes RAM at `addr & 0x03FFFFFF`, word-aligned with byte enables, little-endian (`tx_dma.vhd:120-125`, `rx_dma.vhd:112-117,168-173`). The CPU-side memory range is `wb_adr(31:26)=0` (`wishbone2memio.vhd:72`). Buffers are on the heap (`esp32.cc:69`), inside `memory` 0x00030000+0xE60000 (`linker.x:7`). |

Bus note: the CPU accesses registers with `sb/sh/sw/lbu/lhu` (disassembly of `DmaUART::*`). `wishbone2memio` turns a 16- or 32-bit IO access into sequential byte accesses at increasing addresses: `c_remain` gives 1 extra byte for sel 0x3/0xC and 3 for 0xF (`wishbone2memio.vhd:50,122-132,148-175`). For `rx_addr`, byte +F is therefore written last, which is what arms the receiver.

Ultimate-side data (fixed by the build):
- Buffers: 12 TX and 12 RX, each with `data[1552]` (`CMD_BUF_PAYLOAD 1536 + HEADER 16`) (`cmd_buffer.h:13-23,33-40`).
- `bufnr` is 0..11 for TX and `0x40|i` for RX (`cmd_buffer.c:11,34,45`).
- Objects: `esp32` @0x00968920, `wifi` @0x0096896c.
- Functions: `DmaUART::DmaUartInterrupt` @0x000c7968, `wifi_rx_isr` @0x000c8938, `wifi_detect` @0x000c8a04, `WiFi::RunModeThread` @0x000ca294.

## Init / boot sequence as seen from the bus

1. **Static constructors, before `main`** (`crt0.S:194-218`). `Esp32 esp32;` (`esp32.cc:458`) runs `Esp32::Esp32` (`esp32.cc:66-74`): it creates a semaphore and `new command_buf_context_t`, calls `cmd_buffer_init`, then constructs `new DmaUART(0x10060900, irq 3, …)` (`dma_uart.h:65-81`):
   - read 0x10060900, 0x10060901 (value unused);
   - write 0x10060903 = 0x80, then 0x80 again (soft reset; also clears slip/boot/enable/route to 0);
   - `install_high_irq(3)`: read 0x10000027, write it back with bit 3 set (`riscv_main.c:38-45`).
2. **`vTaskStartScheduler` → `vPortSetupTimerInterrupt`**: write 0x10000027 = 0x00, clearing the WiFi high enable (`riscv_main.c:178`).
3. **`ultimate_main` → `InitFunction::executeAll`** (`ultimate.cc:94`), sorted by ordering (`init_function.cc:29-38`). "WiFi Application" runs at ordering 52 (`wifi.cc:28`), `WiFi::Init(void*,void*)` (`wifi.cc:41-51`):
   - `AttachApplication`; `new NetworkLWIP_WiFi` registers a netif object (`network_interface.cc:144-162`); `attach_config` registers store 0x57494649 "WiFi settings" (`network_esp32.cc:117-123`).
   - `#if U64 == 2` → `wifi.Enable()` → `esp32.StartApp()` (`wifi.cc:47-49,64-67`, `esp32.cc:91-108`), which calls `WiFi::Init(uart,packets)` → `wifi_command_init()` (`wifi.cc:87-92`, `wifi_cmd.cc:32-44`):
     - `SetBaudRate(5000000)`: write 0x10060901=0x00, 0x10060900=0x13 (`dma_uart.cc:143-144`);
     - `FlowControl(true)`: 0x10060903 = rd|0x01 (`dma_uart.cc:91`);
     - `ClearRxBuffer`: 0x10060903 = rd|0x80 twice, plus a reset of all cmd_buffer queues (`dma_uart.cc:117-120`);
     - `EnableSlip(true)`: 0x10060903 = rd|0x04 (`dma_uart.cc:101`);
     - `SetReceiveCallback(wifi_rx_isr)`;
     - `EnableIRQ(true)`: write 0x10060902 = 0x85, and 0x10000027 = rd|0x08 (`dma_uart.cc:72-73`).
     - buf_irq is now pending (no RX address). ISR: write 0x1006090C..0F = &rx_buf->data, 0x10060902 = 0x81 (`dma_uart.cc:168-177`).
   - `WiFi::Start` → `xTaskCreate(RunModeTaskStart, PRIO_DRIVER=1)` (`wifi.cc:94-102`, `FreeRTOSConfig.h:11`).
   - `netstack->effectuate_registered_settings()` (`wifi.cc:50`); the U64-II branch does nothing with WiFi (`network_esp32.cc:133-138`).
4. **`WiFi::RunModeThread`** (`wifi.cc:179-375`). State starts at `eWifi_NotDetected`, taking the `#else // U64` branch (`wifi.cc:236-247`):
   - `ModuleCtrl(ESP_MODE_RUN=0)`: read 0x10060903, write `(rd & 0x8F) | 0x00` (`dma_uart.cc:110`, `esp32.h:23`);
   - `wifi_command_init()` again, repeating step 3's register sequence (`wifi.cc:240`);
   - state `eWifi_ModuleDetected`. There is no boot-message wait; the comment says the module is already running before the FPGA loads (`wifi.cc:242-244`).
5. **IDENTIFY handshake** (`wifi.cc:249-267`, `wifi_cmd.cc:73-104`):
   - `GetBuffer` (≤1000 ticks = 5 s), header `{0x02, bufnr, seq}`, size 4 (`wifi_cmd.h:68-76`);
   - `TransmitPacket`: queue, then write 0x10060902 = 0x82 (`dma_uart.cc:258-270`);
   - ISR TX: write 0x10060904..07 = &tx_buf->data, 0x10060908..09 = 4, 0x1006090A = 1 (`dma_uart.cc:191-194`). When tx_addr_ready returns: free the buffer, write 0x84, then 0x02 once the queue is empty (`dma_uart.cc:187-204`);
   - wait for the reply ≤100 ticks (0.5 s at 200 Hz, `FreeRTOSConfig.h:21`) (`wifi_cmd.cc:80`).
   - No reply: drain and free everything in `receivedQueue` (`wifi_cmd.cc:96-100`), `break`, and the `while(1)` re-enters the same state immediately. **Infinite retry, one attempt per 0.5 s, no backoff.**
6. **After IDENTIFY succeeds** (`eWifi_AppDetected`). Each call below is a blocking RPC:
   - `wifi_get_voltages` (`wifi.cc:255-260`);
   - `U64Config::pushPowerOnMode` → `wifi_get_power_mode` ×≤2, and set+get when it differs (`u64_config.cc:1431-1460,1493-1520`);
   - `pushWakeOnWifi` → `wifi_get_wake_on_wifi`, same pattern (`u64_config.cc:1523-1600`);
   - `wifi_getmac`, then `netstack->set_mac_address` and `netstack->start()` (netif_add) (`wifi.cc:272-274`);
   - `wifi_is_connected`. If status≠0: `wifi_modem_enable(true)`, `link_up`, state `eWifi_Connected`; otherwise `eWifi_NotConnected` (`wifi.cc:275-283`).
7. **Steady state**: the task blocks on `cmd_buffer_received(portMAX_DELAY)` and dispatches events (`wifi.cc:295-370`).

## Boot hazards

No register poll loop exists: nothing at boot branches on a value read from 0x1006090x (only RMW masking and discarded reads). Boot of the rest of the system therefore never waits for the ESP. The hazards are an ISR livelock, the WiFi task, and tasks that block forever.

| # | Where | What goes wrong | Emulator must |
|---|---|---|---|
| H1 | `dma_uart.cc:157-165` | The ISR loops `while ((status & 7) != 0)`. There is no ack register; bits clear only when an enable is dropped, an address is supplied, a TX completes, or rx_pop is written. A status that reads constant nonzero (e.g. 0xFF, or latched IRQ bits) is an endless loop in interrupt context, i.e. a total hang. | Compute +2 bits 0-2 live: `(rx_en&len_valid) \| (tx_en&tx_ready)<<1 \| (buf_en&!rx_addr_valid)<<2`, re-evaluated after every write. Reading 0 is safe. |
| H2 | `itu.vhd:245-252`, `riscv_main.c:118-129` | High IRQs are level-sensitive and have no clear register. If `ITU_IRQ_HIGH_ACT` bit 3 stays set after the ISR, the trap re-enters forever. | `ACT.bit3 = uart_irq_line & HIGH_EN.bit3`, with `uart_irq_line = status & 7 != 0` (`uart_dma.vhd:436`). `HIGH_EN` must read back the last value written (RMW at `riscv_main.c:43`, `dma_uart.cc:73`). |
| H3 | `dma_uart.cc:82-110,117-119` | `flowctrl` is read-modify-write. If a read returns bit 7 = 1 (e.g. echoing the 0x80 written by the ctor), every `FlowControl`/`EnableSlip`/`ModuleCtrl` write re-strobes soft reset, killing the IRQ enables and flushing the DMA. | Read of +3 returns b0,b1,b2,b4,b5,b6 only; b3 = b7 = 0. |
| H4 | `wifi.cc:249-267`, `wifi_cmd.cc:73-104` | IDENTIFY with no answer: the WiFi task retries every 0.5 s forever and prints "Get ident...No reply..." each time. WiFi stays in `eWifi_ModuleDetected` ("Firmware?" `network_esp32.cc:86-88`, "Detected, ..." `:171-172`). There is no hang, but the feature is dead. | Deliver a CMD_IDENTIFY (0x02) reply within 100 ticks (0.5 s), with byte 1 (thread) echoed and a NUL-terminated string. |
| H5 | `wifi_cmd.h:79-84` | Every RPC wrapper except `wifi_detect` uses `xTaskNotifyWait(…, portMAX_DELAY)`; a missing reply blocks the caller forever. Callers: menu "Power OFF"/"Power Cycle" (`c64_subsys.cc:100-101,245-254`); REST `PUT /v1/machine:poweroff` (`route_machine.cc:205-211`); socket `SOCKET_CMD_POWEROFF` (`socket_dma.cc:159-161`); "Cold Boot Required" (`configio.cc:176-180`); WiFi config Enable/Disable/Forget, which are unconditional (`network_esp32.cc:341-354`); the edit hooks for "Power On After Power Loss" and "Wake On Wi-Fi" (`u64_config.cc:396-397,928-929,1465-1600`, fired via `config.cc:840-845,902-911`); and the WiFi task itself after IDENTIFY (`wifi.cc:256-283`). | Answer **every** request except CMD_SEND_PACKET (0x08). Echo bytes 0-3 (command, thread, sequence). Unknown commands get `esp_err = ESP_ERR_NOT_SUPPORTED`, as `cmd_not_implemented` does (`rpc_dispatch.c:366-372`; 0x106 in ESP-IDF). |
| H6 | `cmd_buffer.c:64-71`, `wifi_cmd.h:68-70`, `wifi.cc:377-386` | If TX never completes (no tx_addr_ready edge or no TX IRQ), each IDENTIFY attempt strands one of the 12 TX buffers in `transmitQueue`; the pool is empty after about 6 s. Then `BUFARGS` waits 5 s and returns `pdFALSE` (= 0, which callers read as success), and `WiFi::sendEvent` blocks forever in `cmd_buffer_get(portMAX_DELAY)`. Opening the WiFi row in the browser calls `getSubItems` → `sendEvent(EVENT_RESCAN)` (`browsable_root.h:52-61`, `network_esp32.cc:109-113`), which **hangs the UI task**. A TRANSMIT caller inside the first ~6 s additionally blocks forever (H5). | Model TX DMA completion: after `tx_push`, drop tx_addr_ready and raise it again once the frame is "sent" (instant is fine), so the ISR frees the buffer (`dma_uart.cc:187-190`). |
| H7 | `wifi.cc:255-260` | CMD_GET_VOLTAGES: the struct is zeroed before the call, and `vbus < 8500` posts "Low input voltage." A generic 8-byte `espcmd` reply leaves vbus = 0 and triggers the message. | Reply with `rpc_get_voltages_resp` (24 bytes), `vbus` ≥ 8500 (mV), e.g. 12000. |
| H8 | `u64_config.cc:1431-1441,1493-1520` | GET_POWER_MODE (0x17) without `esp_err==0 && mode<=2` (2 attempts) disables the "Power On After Power Loss" item. If the stored mode differs from the flash config, SET (0x16) runs and then GET must read back the same value, otherwise "Power on behavior not stored." / "Control module does not answer." is posted. | Keep `mode` (0..2) and `last_state` state; SET stores, GET returns the stored value. |
| H9 | `u64_config.cc:1523-1600` | Same as H8 for GET/SET_WAKE_ON_WIFI (0x19/0x18), with `enabled<=1`; otherwise the "Wake On Wi-Fi" item is disabled. | Store `enabled` (0/1) and read it back. |
| H10 | `wifi_cmd.cc:20-28`, `wifi_cmd.h:76`, `wifi.cc:456-466` | Replies are routed only by `hdr.thread` into `tasksWaitingForReply[]`. Entries are not cleared on timeout (`wifi_detect`) or for fire-and-forget sends (`wifi_tx_packet` sets one and never waits). A reply to 0x08, or a late reply, can wake an unrelated waiting task with a foreign buffer. | Never reply to CMD_SEND_PACKET. Unsolicited frames use `thread = 0xFF` (`wifi_modem.c:112,228,247`, `rpc_dispatch.c:380,396`). Reply before the requester's timeout. |
| H11 | `dma_uart.cc:168-182`, `rx_dma.vhd:79-150` | The RX address handshake assumes one latched (active) address plus one pending register. If writes to +F are not tracked as rx_addr_valid, buf_irq stays pending: the ISR moves all 12 RX buffers into `rx_bufs`, prints `^`, disables BufReq, and every `FreeBuffer` re-triggers it (`dma_uart.cc:288`). Reception is broken and the console spams. This is not a boot hang. | Implement the two-slot model (Functional model) and deliver frames into the **oldest** armed address (FIFO, matching `rx_bufs`). |

Note: the "register sink" model (reads 0, writes ignored, IRQ never raised) survives H1-H3 and boots, but runs straight into H6 (UI hang when opening WiFi; power-off stalls or hangs). The "DMA without ESP" model (TX completes, no RX) avoids H6 but turns every item in H5 into a permanent hang. The safe minimum is T0 below.

## Interrupts

- Source: `DmaUART` registered on **ITU high IRQ 3** (`ITU_IRQHIGH_WIFI`, `itu.h:43`; `esp32.cc:73`; `dma_uart.h:80`).
- Enable: `ITU_IRQ_HIGH_EN` 0x10000027 bit 3. Global `ITU_IRQ_GLOBAL` must be 1 (`riscv_main.c:186`). The CPU IRQ is asserted while `(irq_high & imask_high) != 0` (`itu.vhd:245-252`).
- Dispatch: `freertos_risc_v_application_interrupt_handler` (@0x0003de44) reads `ITU_IRQ_HIGH_ACT` (0x10000028) and calls `DmaUartInterrupt(this)` for bit 3 (`riscv_main.c:118-129`). An active bit with no handler clears its own enable bit (`riscv_main.c:125-127`), which does not apply here since the handler is installed in the ctor.
- Raise/ack: the level is `uart_dma.irq = rx_irq | tx_irq | buf_irq` (`uart_dma.vhd:433-436`). Ack is implicit:
  - rx: write `rx_pop` (+B) (`dma_uart.cc:223`);
  - tx: `tx_push` of the next frame, or `ictrl=0x02` (`dma_uart.cc:194,203`);
  - buf: write `rx_addr` (+F) or `ictrl=0x04` (`dma_uart.cc:176,180`);
  - soft reset (+3 b7) clears all enables (`uart_dma.vhd:322-326`).
- The ISR return value (`HPTaskAwoken`) requests a context switch (`dma_uart.cc:227`, `riscv_main.c:123,131-133`).

## Functional model

### Link layer (FPGA uart_dma, CPU view)

- **Framing on the wire**:
  - FPGA TX with SLIP on: `C0, escaped bytes, C0`, with 0xC0→`DB DC` and 0xDB→`DB DD` (`slip_encoder.vhd:66-117`).
  - ESP TX: the same format (`my_uart.c:160-191`).
  - FPGA RX: waits for C0 (sync), ignores repeated C0, and a frame ends at the next C0 (`slip_decoder.vhd:77-120`). **Max 1536 decoded bytes**; longer frames are cut with `last` (`slip_decoder.vhd:17,105-107`).
  - SLIP off (`ascii`): bytes pass raw, in 1536-byte chunks. Switching SLIP on flushes a partial chunk with an appended 0x0A (`slip_decoder.vhd:55-75`). The U64-II ELF enables SLIP in `wifi_command_init` and never turns it off (the SLIP-off users `DetectModule`/`Boot`/`Download` are gc'd).
- **Emulate at frame level.** The CPU never sees bytes, only whole frames in RAM plus a length. Baud (5 Mbit, divisor 19), HW flow control (b0), loopback (b1, never set in the ELF) and the MOD bits have no CPU-visible effect beyond read-back.
- **TX**:
  - `tx_push`: latch `tx_addr & 0x03FFFFFF`, `len`; `tx_addr_ready = 0`.
  - Read `len` bytes from RAM, which is one ESP-bound frame, then set `tx_addr_ready = 1`.
  - Hardware FIFO depth is 2047 bytes (`uart_dma.vhd:124-128`), and `addr_ready` returns once tx_dma has copied the frame into the FIFO (`tx_dma.vhd:60,97-99`); instant completion is acceptable.
- **RX** (two slots):
  - Write +F: `pending = rx_addr`, `pending_valid = 1`.
  - When the DMA is idle and `pending_valid`: `active = pending; pending_valid = 0` (`rx_dma.vhd:87-95`, `uart_dma.vhd:286-288`).
  - A queued ESP frame is delivered only when `active` exists: write bytes at `active & 0x03FFFFFF`, set `rx_len = n`, `len_valid = 1`, `active = none` (`rx_dma.vhd:110-150`).
  - While `len_valid = 1` the next frame is held (`rx_dma.vhd:145-150`); in hardware this backpressures through the rxfifo and RTS (`uart_dma.vhd:273`).
  - `rx_pop`: `len_valid = 0`.
  - Without an armed buffer, frames stay queued; never drop.
- **Soft reset** (+3 b7): `pending_valid = 0`, `active = none`, `len_valid = 0`, in-flight TX dropped, `tx_addr_ready = 1`, rx/tx/buf enables = 0 (`uart_dma.vhd:280,283-288,322-326`; `rx_dma.vhd:159-164`; `tx_dma.vhd:111-116`).
- **Reset values**: `boot=1, enable=0, route=0, loopback=0, cts_enable=0, slip=0`, enables 0, divisor `g_divisor-1` (`uart_dma.vhd:413-425`). Idle status reads 0xD0 (b4 cts_c=1 because cts_enable=0, b6=1, b7=1).
- `status.b3` (overflow) and `b4` (cts) are only printed (`dma_uart.h:98`); return 0 and 1.
- `ModuleCtrl` bits 4-6 (`dma_uart.cc:107-112`) on U64-II use `ESP_MODE_RUN=0, BOOT=1, RUN_MODE=2, OFF=3, RUN_UART=4, BOOT_UART=5` (`esp32.h:21-27`). In this ELF only `ModuleCtrl(ESP_MODE_RUN)` is reachable (`wifi.cc:238`). Store and read back; no side effect needed.

### RPC protocol (payload of each SLIP frame)

Wire structs are little-endian, ilp32, default alignment (`rpc_calls.h`); offsets are computed.

- Header, 4 bytes: `command u8 @0, thread u8 @1, sequence u16 @2` (`rpc_calls.h:17-21`).
- Requests are built in TX buffers: `thread = bufnr` (0..11) and `sequence = sequence_nr++` (`wifi_cmd.h:71-75`).
- The ESP reuses the request buffer, so the reply header is identical (`rpc_dispatch.c:33-372`).
- Routing on the Ultimate (`wifi_cmd.cc:15-30`): if `thread < 12 && tasksWaitingForReply[thread]`, the buffer pointer is handed to that task via `xTaskNotifyFromISR`. Otherwise it goes to `receivedQueue` and is consumed by `RunModeThread`.
- Common reply `rpc_espcmd_resp`: `esp_err i32 @4`, size 8 (`rpc_calls.h:23-26`).

| Cmd | Code | Request (size) | Reply (size, fields) | FW caller / use |
|---|---|---|---|---|
| ECHO | 0x01 | any | same frame echoed (`rpc_dispatch.c:33-36`) | not reachable (`RequestEcho` gc'd) |
| IDENTIFY | 0x02 | hdr (4) | `major u16@4, minor u16@6, string@8`; size `10+strlen` (includes NUL) (`rpc_dispatch.c:38-46`). Real module: 1.14 "ESP32 WiFi Bridge V1.14" (`rpc_dispatch.h:29-37`) | `wifi_detect`, 0.5 s timeout; copies ≤31 chars (`wifi.cc:251`, `wifi_cmd.cc:84-88`) |
| SET_BAUD | 0x03 | `baudrate i32@4, flowctrl@8, inversions@9` (12) | espcmd, sent after 1 s at the new rate (`rpc_dispatch.c:56-77`) | not reachable (`wifi_setbaud` gc'd) |
| WIFI_SCAN | 0x04 | hdr | `esp_err@4, num_records u8@8, reserved@9, aps[24]@10`, AP record 42 bytes: `bssid[6]@0, ssid[33]@6, channel@39, rssi i8@40, authmode@41`; struct 1020 (`rpc_calls.h:111-130`). Sent size = `4+4+2+n*42`; on error the ESP still sets `num_records = 0` (`wifi_modem.c:798-802`). Sent asynchronously by the connector thread | `wifi_scan` in `eWifi_Scanning` (`wifi.cc:287-293`). `wifi_scan` copies `rec` whatever `esp_err` says (`wifi_cmd.cc:219-229`), overwriting the `num_records = 0` preset (`wifi.cc:288-289`), so every reply, including an error, needs a valid `num_records` |
| WIFI_CONNECT | 0x05 | `auth_mode@4, ssid[32]@5, password[64]@37` (104); ESP requires ≥98 (`rpc_dispatch.c:100`) | espcmd (async) | AP list / manual connect (`wifi.cc:432-448`, `network_esp32.cc:259-288`) |
| WIFI_DISCONNECT | 0x06 | hdr | espcmd; EVENT_DISCONNECTED follows | `network_esp32.cc:231-235,331-339` |
| WIFI_GETMAC | 0x07 | hdr | `esp_err@4, mac[6]@8` (16) | `wifi.cc:272`; becomes REST `wifi_mac`, omitted if all-zero (`routes.cc:229-250`) |
| SEND_PACKET | 0x08 | `length u32@4, data@8`; frame size `length+8`, ≤1536 payload (`wifi.cc:456-466`) | **no reply** (`rpc_dispatch.c:158-163`) | lwIP output, only in `eWifi_Connected` (`wifi.cc:452-454`) |
| MODEM_ON / OFF | 0x09 / 0x0A | hdr | espcmd; ON starts forwarding received Ethernet frames (`rpc_dispatch.c:187-203`, `wifi_modem.c:310-326`) | `wifi.cc:278,325` |
| WIFI_IS_CONNECTED | 0x0B | hdr | `esp_err@4, status u8@8` (12); ESP then also sends EVENT_CONNECTED (if connected) or EVENT_DISABLED (`rpc_dispatch.c:239-249`, `wifi_modem.c:264-271`) | `wifi.cc:276` |
| GET_VOLTAGES | 0x0C | hdr | `esp_err@4, vbus@8, vaux@10, v50@12, v33@14, v18@16, v10@18, vusb@20`, u16 mV (24) | `wifi.cc:256` (H7) |
| WIFI_ENABLE / DISABLE | 0x0D / 0x0E | hdr | espcmd, immediate; connector acts later (`rpc_dispatch.c:205-225`) | config Enable/Disable, context menu (`network_esp32.cc:341-349,372-390`) |
| MACHINE_OFF | 0x0F | hdr | espcmd; after 1 s the ESP cuts the regulators (`rpc_dispatch.c:165-173`, `button_handler.c:25-55,91-97`) | Power OFF paths (H5) |
| GET_TIME | 0x10 | `timezone[128]@4` | `esp_err@4, datetime@8` | not used (commented out, `wifi_cmd.cc:200-218`) |
| CLEAR_APS | 0x11 | hdr | espcmd | "Forget APs" (`network_esp32.cc:237-251,351-354`) |
| WIFI_AUTOCONNECT | 0x12 | hdr | espcmd, immediate | "Connect", "Connect to last AP" (`network_esp32.cc:253-257,321-329`) |
| MACHINE_REBOOT | 0x13 | hdr | espcmd; then off after 1 s, on after 2 s more, i.e. an FPGA cold boot (`rpc_dispatch.c:175-185`) | "Power Cycle" (`c64_subsys.cc:245-251`) |
| SET/GET_SERIAL | 0x14 / 0x15 | `serial[16]@4` | espcmd / `esp_err@4, serial[16]@8` | gc'd in this ELF |
| SET_POWER_MODE | 0x16 | `mode@4` (6) | espcmd (0x102 INVALID_ARG if mode>2, or if the request is shorter than 6 bytes, `rpc_dispatch.c:316-320`) | `u64_config.cc:1444-1460` |
| GET_POWER_MODE | 0x17 | hdr | `esp_err@4, mode@8, last_state@9` (12) | `u64_config.cc:1431-1441` |
| SET_WAKE_ON_WIFI | 0x18 | `enabled@4` (6) | espcmd (0x102 INVALID_ARG if the request is shorter than 6 bytes, `rpc_dispatch.c:343-347`) | `u64_config.cc:1534-1549` |
| GET_WAKE_ON_WIFI | 0x19 | hdr | `esp_err@4, enabled@8` (12) | `u64_config.cc:1523-1532` |

Mode values: `POWERON_MODE_OFF=0, ON=1, LAST_STATE=2`; `WAKE_ON_WIFI_DISABLED=0, ENABLED=1` (`power_state.h:19-28`).

Events from the ESP always carry `thread=0xFF` and go to `RunModeThread` (`wifi.cc:303-369`). They are processed only in states NotConnected/Connected/Disabled; in other states they sit in `receivedQueue`, which `wifi_detect` drains while looping.

| Event | Code | Frame | Ultimate reaction |
|---|---|---|---|
| EVENT_CONNECTED | 0x40 | `ssid[32]@4` (36) (`wifi_modem.c:250-254`) | copy `last_ap`; if already Connected, `link_down`; `wifi_modem_enable(true)` (blocking RPC); state Connected; `link_up` (DHCP if enabled, `network_interface.cc:332-343`) (`wifi.cc:318-331`) |
| EVENT_GOTIP | 0x41 | `ip@4, netmask@8, gw@12, changed@16` (20) | log only (`wifi.cc:357-363`) |
| EVENT_DISCONNECTED | 0x42 | hdr | state NotConnected, `link_down` (`wifi.cc:333-340`) |
| EVENT_RECV_PACKET | 0x43 | `len u16@4, data@6`; size `len+7` (`wifi_modem.c:109-118`) | `netstack->input(buf, data, len)`. The buffer is held by lwIP until `wifi_free` → `FreeBuffer` (`wifi.cc:342-355,472-478`). The ESP forwards only frames addressed to the station MAC or broadcast (`wifi_modem.c:104-105`). |
| EVENT_RESCAN | 0x44 | internal loopback only (`wifi.cc:377-386`) | state Scanning |
| EVENT_KEEPALIVE | 0x45 | hdr | "Unexpected Event type", freed (`wifi.cc:365-368`) |
| EVENT_DISABLED | 0x46 | hdr | state Disabled, `link_down` (`wifi.cc:311-316`) |
| EVENT_BUTTON/FREEZE/MENU/RESET | 0x80-0x83 | hdr (`rpc_dispatch.c:389-403`, from `button_handler.c:63-65`) | no CPU handler (default case). May never reach the CPU, see Open questions. |

### Features that depend on the ESP32 on U64-II

- **WiFi network interface** ("WI" netif, `network_esp32.cc:130-131`) and every service over it. Ethernet via RMII is independent.
- **Power off and power cycle**: menu, REST, socket DMA, and the post-config-clear cold boot (H5). On U64-II there is no power register path; U64 v1 uses `U64_POWER_REG` (`c64_subsys.cc:252-262`).
- **"Power On After Power Loss" and "Wake On Wi-Fi"** settings, stored in ESP NVS (`u64_config.cc:1462-1464,1551-1552`).
- **Input-voltage warning** (`wifi.cc:258-260`).
- **REST `/v1/info` `wifi_mac`** (`routes.cc:223-250`).
- **Front-panel buttons**: owned by the ESP (`button_handler.c`), including the power rail (`pinout.h:13-14,22,27`). How presses reach the Ultimate is an open question.
- **Not ESP-dependent**:
  - RTC/time (no ESP reference in `rtc_dummy.cc`/`sntp_time.cc`; `CMD_GET_TIME` unused);
  - the C64 keyboard (`Keyboard_C64` on `U64II_KEYB_*`, `ultimate.cc:126`);
  - serial number (gc'd);
  - ESP flashing (`filetype_esp.cc` not in SRCS; the `esp32.Quit()` in `filetype_u2p.cc:131-135` is `#ifndef RISCV`);
  - the Developer ESP32 actions (`u64_config.cc:1685-1696`), whose command cases only exist `#ifdef NO_ESP` (`u64_config.cc:1781-1793`).

## Emulator model tiers

**T0: boot without hangs, WiFi shows "Link Down"** (recommended minimum):
1. Register block 0x10060900-0F exactly as in the Address map: live status (H1), `flowctrl` read-back with b7=0 (H3), soft reset, and the TX/RX two-slot DMA against guest RAM at `addr & 0x03FFFFFF`.
2. IRQ: level into ITU high bit 3 with a read-back of `HIGH_EN` (H2).
3. A frame-level stub u64ctrl that consumes TX frames and queues reply frames (thread echoed). Latency should be well under 100 ticks.
   - 0x02 IDENTIFY: `major=1, minor=14`, "ESP32 WiFi Bridge V1.14\0".
   - 0x0C GET_VOLTAGES: `vbus=12000, vaux=12000, v50=5000, v33=3300, v18=1800, v10=1000, vusb=5000`.
   - 0x17/0x16 and 0x19/0x18: stored state, defaults `mode=0, last_state=1, enabled=0`.
   - 0x07 GETMAC: esp_err 0 and a fixed locally-administered MAC, e.g. `02:15:41:00:00:01`.
   - 0x0B IS_CONNECTED: `status=0`, then EVENT_DISABLED or nothing.
   - 0x0F/0x13: esp_err 0; optionally signal host power-off / cold reset after 1 s / 3 s.
   - 0x04 WIFI_SCAN: `esp_err` plus `num_records = 0`, 10 bytes; never a bare 8-byte espcmd (see the table).
   - 0x08: consume silently.
   - Everything else: espcmd with `esp_err=0x106` (or 0 for 0x06/0x09/0x0A/0x0D/0x0E/0x11/0x12 to keep UI flows quiet).

   Result: `AppDetected` → `NotConnected` (`wifi.cc:269-285`), power settings enabled, no blocked tasks.

**T1: functional WiFi and power control**:
- Everything in T0, plus a virtual u64ctrl:
  - scan list (0x04 → `rpc_scan_resp`);
  - connect/autoconnect/disconnect state machine emitting EVENT_CONNECTED, DISCONNECTED and DISABLED;
  - AP store with CLEAR_APS;
  - persistent NVS (power mode, wake-on-WiFi, APs).
- L2 bridge: 0x08 payloads go to a host virtual Ethernet segment (vmnet/tap or a userspace NAT with DHCP); host frames for the station MAC or broadcast come back as EVENT_RECV_PACKET, but only after MODEM_ON (`rpc_dispatch.c:187-194`).
- The Ultimate runs its own lwIP DHCP on this link (`network_interface.cc:332-343`); EVENT_GOTIP is informational only.
- Hold RX frames while no RX buffer is armed; lwIP pins buffers until `wifi_free`.
- Host power semantics: MACHINE_OFF stops the machine; MACHINE_REBOOT does a full cold boot (FPGA and firmware reload, RAM lost).
- Optional: host buttons → EVENT_MENU/RESET/FREEZE, pending the open question below.

## Open questions

1. OPEN QUESTION: Does the closed U64-II top level instantiate `uart_dma` for the WiFi link, and with which generics (`g_events`, `g_divisor`, mem tags)? The C driver matches this entity bit for bit (`dma_uart.h:21-44` vs `uart_dma.vhd:103-121,291-406`), which is strong evidence. If `g_events=true`, any frame whose first byte is ≥0x80 (EVENT_BUTTON 0x80 … EVENT_JOYSTICK 0x90) is diverted to the FPGA `event` port and dropped from the CPU stream (`rx_dma.vhd:83-86,97-104,152-155`). That would also explain why the CPU has no handler for these codes.
2. OPEN QUESTION: Where do ESP button events (menu/reset/freeze) surface on U64-II: FPGA event path into ITU button register or C64 reset/freeze logic? This belongs to the ITU/C64 docs.
3. OPEN QUESTION: What do flowctrl bits 4-6 (boot/enable/route) drive on U64-II? The ESP_MODE encoding is inverted relative to U64 v1 (`esp32.h:21-35`: OFF=3 vs 0). The ESP owns FPGA power and JTAG (`pinout.h:12-15,22,27`), so these bits cannot power-gate it in any meaningful way; `route` may be UART passthrough (`ESP_MODE_*_UART`). This ELF only writes mode 0.
4. OPEN QUESTION: Does the high-IRQ line 3 wiring match `ITU_IRQHIGH_WIFI` in the closed top level? It is assumed from `esp32.cc:73`.
5. OPEN QUESTION: What is the value of `DEVELOPER` in this build? It decides whether the no-op "Disable/Enable ESP32" actions appear in the Developer menu (`u64_config.cc:1691-1697`); cosmetic only.
6. OPEN QUESTION: How do upper address bits of the 0x10060900 window alias in the top-level IO decoder? The firmware only uses offsets 0x0-0xF.
