# Ethernet: RMII MAC, PHY/MDIO, lwIP netif glue

Scope: U64-II RISC-V `ultimate` build (`-DRISCV -DU64=2 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000`,
target/u64ii/riscv/ultimate/Makefile:246). Paths are relative to `firmware/1541ultimate/`.
The U64-II top level is closed. The RMII MAC semantics come from the open `fpga/io/rmii` + `fpga/ip/free_queue` VHDL,
checked against the C driver. Where the two could differ on U64-II, this is marked OPEN QUESTION.

## Sources read

| File | Key content |
|---|---|
| software/io/network/rmii_interface.h / .cc | Register macros, `RmiiInterface` ctor, `initRx`, `rmiiTask`, `rx_interrupt_handler`, `input_packet`, `free_buffer`, `output_packet` |
| software/io/network/network_interface.h / .cc | `NetworkInterface` (lwIP netif wrapper), custom pbuf RX, `lwip_output_callback`, link_up/down, DHCP/static config |
| software/network/network_config.h / .cc | "Network Settings" store (hostname, services, SNTP) |
| software/network/sntp_time.cc | `start_sntp`, `sntp_time_received` |
| software/io/mdio/mdio.c, software/system/u2p.h | Bit-banged MDIO on U2PIO GPIO |
| software/network/sys_arch.c, network/config/arch/{sys_arch.h,cc.h}, network/config/lwipopts.h | lwIP FreeRTOS port + options |
| target/libs/riscv/lwip/makefile | How liblwip.a is built |
| software/portable/riscv/riscv_main.c | Trap dispatcher, ITU bit 0x20 → `RmiiRxInterruptHandler` |
| software/system/itu.h, itu.c, iomap.h | `RMII_BASE`, ITU IRQ registers, capability bits |
| software/network/{socket_gui,ftpd,socket_dma,httpd,syslog}.cc, io/acia/{modem,listener_socket}.cc | Services + ports |
| software/io/network/data_streamer.cc, system/u64.h | Hardware UDP stream generator (shares the netif ARP/MAC) |
| software/system/product.cc, io/flash/w25q_flash.cc | Default hostname, MAC source (flash unique ID) |
| software/io/wifi/wifi.cc | Second `NetworkInterface` (ESP32 WiFi) |
| fpga/io/rmii/vhdl_source/{ethernet_rmii,eth_filter,eth_transmit,rmii_transceiver}.vhd | RX filter/DMA, TX DMA, RMII PHY side |
| fpga/ip/free_queue/vhdl_source/{free_queue,block_bus_pkg}.vhd, fpga/ip/busses/vhdl_source/io_bus_splitter.vhd | Buffer-ID free/used FIFOs, sub-address decode |
| fpga/io/itu/vhdl_source/itu.vhd, fpga/cpu_unit/rvlite/vhdl_source/bus_converter.vhd | IRQ level/edge, multi-byte IO access decomposition |

## Address map

`RMII_BASE = IOBASE + 0x60800 = 0x10060800` (iomap.h:31). Inside the MAC, address bits [5:4] select the sub-block:
0 = RX filter (+0x00), 1 = TX (+0x10), 2 = free queue (+0x20). Each sub-block decodes bits [3:0]
(ethernet_rmii.vhd:96-111, eth_filter.vhd:210, eth_transmit.vhd:146, free_queue.vhd:89).
Sub-address 3 (+0x30..+0x3F) is acked with data 0 (io_bus_splitter.vhd:51-52).
Unlisted reads return 0 (`c_io_resp_init`, e.g. eth_filter.vhd:213,232-237).

| Absolute addr | Width | R/W | Name (C macro) | Meaning |
|---|---|---|---|---|
| 0x10060800..05 | 8 ×6 | W | `RMII_RX_MAC(i)` | Own MAC, byte i = MAC octet i (eth_filter.vhd:218-219; rmii_interface.cc:130-133) |
| 0x10060807 | 8 | W | `RMII_RX_PROMISC` | bit0 promiscuous (eth_filter.vhd:221-222). FW writes 0 (rmii_interface.cc:136) |
| 0x10060808 | 8 | W | `RMII_RX_ENABLE` | bit0 RX enable. 0 = RX FIFO + filter FSM held in reset, open block dropped (eth_filter.vhd:125,224-225,366-372) |
| 0x10060810..13 | 32 (LE bytes) | W | `RMII_TX_ADDRESS` | TX frame start address, bits 25:0 used (eth_transmit.vhd:155-162) |
| 0x10060814..15 | 16 (LE bytes) | W | `RMII_TX_LENGTH` | TX length, bits 11:0 used (eth_transmit.vhd:163-166) |
| 0x10060818 | 8 | W | `RMII_TX_START` | Any write: start copy, busy=1 (eth_transmit.vhd:167-169) |
| 0x10060819 | 8 | W | `RMII_TX_IRQACK` | Any write: TX irq=0 (eth_transmit.vhd:170-171). **Never written by FW** |
| 0x1006081A | 8 | R | `RMII_TX_BUSY` | bit0 busy (eth_transmit.vhd:186-187). Cleared + irq=1 when copy is done (:178-181) |
| 0x10060820 | 8 | W | (low byte of `RMII_FREE_BASE`) | Ignored (free_queue.vhd:118-119). Base bits 7:0 are always 0 |
| 0x10060821..23 | 8 ×3 | W | `RMII_FREE_BASE` bytes 1..3 | Buffer pool base, bits 25:8 (free_queue.vhd:120-125) |
| 0x10060824 | 8 | W | `RMII_FREE_PUT` | Push buffer ID onto the free FIFO. Silently ignored if free FIFO full (free_queue.vhd:126-128,240-249) |
| 0x10060824 | 8 | R | — | bit0 insert pending, bit1 free_full (free_queue.vhd:152-154). Not read by FW |
| 0x10060828 | 8 | R | `RMII_ALLOC_ID` | ID of head of used (received) FIFO. 0xFF after pop (free_queue.vhd:132,155-156) |
| 0x1006082A..2B | 16 (LE bytes) | R | `RMII_ALLOC_SIZE` | Frame length without FCS, 12 bits (free_queue.vhd:157-160) |
| 0x1006082E | 8 | W | `RMII_FREE_RESET` | Any write: soft reset of all 4 FIFO pointers (free_queue.vhd:134-135,310-317). Does not clear `used_valid` |
| 0x1006082F | 8 | R | `RMII_ALLOC_VALID` | bit0 `used_valid` = a received frame is presented = RX IRQ line (free_queue.vhd:86,161-162) |
| 0x1006082F | 8 | W | `RMII_ALLOC_POP` | Any write: if valid, valid=0 and ID register=0xFF. The next entry falls through a few clocks later (free_queue.vhd:99-111,129-133) |

MDIO GPIO (U2PIO, `U2P_IO_BASE = 0x10100000`, u2p.h:13-15,66-68; closed top, semantics from mdio.c:13-17):

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x1010000A | 8 | W | `U2PIO_SET_MDC` | MDC pin level = value (0/1) |
| 0x1010000B | 8 | W | `U2PIO_SET_MDIO` | MDIO drive: 1 = release/high, 0 = drive low (open-drain) |
| 0x10100006 | 8 | R | `U2PIO_GET_MDIO` | MDIO pin level. FW tests `!= 0` (mdio.c:109) |

Related registers owned by other blocks:

| Absolute addr | R/W | Name | Relevance |
|---|---|---|---|
| 0x1000000C..0F | R | ITU CAPABILITIES_0..3 (MSB first) | `CAPAB_ETH_RMII = 0x01000000` = bit0 of 0x1000000C (itu.c:5-8,23-26; itu.h:73) |
| 0x10000001 / 0x10000002 | W | `ITU_IRQ_ENABLE` / `ITU_IRQ_DISABLE` | Mask set/clear of bit 0x20 (rmii_interface.cc:108,114) |
| 0x10000004 / 0x10000005 | W / R | `ITU_IRQ_CLEAR` / `ITU_IRQ_ACTIVE` | Trap dispatcher (riscv_main.c:86-87) |
| 0x10190000 + 64·id | W | `U64_UDP_BASE` | 42-byte Eth/IP/UDP header template of the FPGA stream generator (u64.h:58; data_streamer.cc:309-400) |
| 0x1010040F | R/W | `U64_ETHSTREAM_ENA` | Stream generator enables, bits 0..3 (u64.h:15,80; data_streamer.cc:335,410-414) |

Multi-byte IO: the FW uses 16/32-bit `volatile` accesses (rmii_interface.h:17-18,22,27). In the open RISC-V bridge,
a 2/4-byte access becomes sequential 8-bit IO accesses at addr, addr+1, … with little-endian data
(bus_converter.vhd:56,116,159-178). The emulator must decompose these into LE byte accesses. Side effects are
only on single-byte registers, so byte order within one access is irrelevant.

## Init / boot sequence as seen from the bus

1. `vPortSetupTimerInterrupt`: `ITU_IRQ_DISABLE=0xFF`, `ITU_IRQ_CLEAR=0xFF`, `ITU_IRQ_ENABLE=0x01` (riscv_main.c:178-186). RMII RX is masked.
2. `ultimate_main` reads capabilities 0x1000000C..0F (ultimate.cc:87; itu.c:23-26), then `InitFunction::executeAll()` runs in ascending `ordering` (ultimate.cc:94; init_function.cc:29-36,40-50):
   - 20 "Network Config": registers store `0x4E455400`. Hostname default = `getProductDefaultHostname()` (network_config.cc:46-54,83), which reads the flash unique ID (product.cc:184-191).
   - 50 "LwIP Networking": `tcpip_init(NULL,NULL)` → creates `tcpip_thread` (network_interface.cc:54-60).
   - 51 "RMII Interface" → `new RmiiInterface()` (rmii_interface.cc:24-28). The constructor runs **only if `getFpgaCapabilities() & CAPAB_ETH_RMII`** (:50):
     1. `getNetworkStack()` → `NetworkInterface` ctor + `attach_config` (store `0x4E657477` "Ethernet Settings") (:51; network_interface.cc:127-181).
     2. `new uint8_t[32*1536+256]` (0xC100 bytes). `ram_base = (ptr+255) & ~0xFF` (:53-54).
     3. **W** 0x10060820..23 = `ram_base` (:56).
     4. MDIO read reg 2 @ PHY 0. If `!= 0x0022`, read reg 2 @ PHY 3. If still not 0x0022, print "could not find Ethernet PHY", keep addr=3, continue (:59-69).
     5. MDIO writes: reg4=0x01E1, reg0x1B=0x0500, reg0x16=0x0002, reg0=0x1200 (:71-74).
     6. `xQueueCreate(34, 3 bytes)`, create task "RMII Driver Task" prio `PRIO_DRIVER`=1 (:77-78; FreeRTOSConfig.h:11).
   - 52 "WiFi Application": a second `NetworkInterface` (ESP32), always created on U64-II (wifi.cc:28,44).
   - 100 Telnet (socket_gui.cc:29), 101 FTP (ftpd.cc:1259), 102 Socket 64 (socket_dma.cc:656), 103 HTTP (httpd.cc:20-25), 105 Modem if `CAPAB_ACIA` (modem.cc:923-928).
3. `syslog.init()` (ultimate.cc:95). Only if a syslog server is configured does it start a task that polls `DoWeHaveLink()` every 100 ms (syslog.cc:96-100).
4. RMII task body (rmii_interface.cc:117-188):
   1. `flash->read_serial()` (W25Q: command 0x4B, 8 bytes, w25q_flash.cc:236-249). MAC = `02:15:41:s1^s5:s2^s6:s3^s7` (:119-128).
   2. **W** 0x10060800..05 = MAC. **W** 0x10060807=0. **W** 0x10060808=0 (:130-137).
   3. `set_mac_address` + `start()` → `netif_add(..., tcpip_input)` → `init_callback` sets hwaddr, mtu 1500, `etharp_output`, `linkoutput=lwip_output_callback`, flags BROADCAST|ETHARP|LINK_UP, mDNS add, status callback (:138-142; network_interface.cc:193-203,246-288).
   4. Poll loop while link down: MDIO read reg 1. If bit 2 is clear, `vTaskDelay(50)` = 250 ms (tick 200 Hz, FreeRTOSConfig.h:21) (:154-171).
   5. On bit 2 set → `initRx()` (:88-109):
      **W** 0x10060808=0 → delay 1 tick → **W** 0x1006082E=1 → **W** 0x1006082F=1 → **W** 0x1006082E=1 →
      **W** 0x10060824 = 0,1,…,31 → **W** 0x10060808=1 → **W** `ITU_IRQ_ENABLE`(0x10000001)=0x20.
      Then `netstack->link_up()`: `netif_set_up`, `dhcp_start` (DHCP default on, network_interface.cc:27) or static DNS, `set_default_interface` (network_interface.cc:332-343).
   6. Link up: `xQueueReceive(queue, 200 ticks = 1 s)`. On packet → `input_packet`. On timeout → MDIO read reg 1. If bit 2 is clear → `deinitRx()` (**W** 0x10060808=0, **W** `ITU_IRQ_DISABLE`=0x20) + `link_down()` (:172-186,111-115; network_interface.cc:345-352).
5. `netif_set_up` → status callback → `statusUpdate` → `set_default_interface` + `start_sntp()` (network_interface.cc:211-243). If SNTP is enabled: TZ set, servers `time.windows.com`, `time.google.com`, `pool.ntp.org`, `sntp_init` (sntp_time.cc:18-32; network_config.cc:29-33). DHCP-provided NTP servers override these (lwipopts.h:1453). Time lands via `SNTP_SET_SYSTEM_TIME` → `rtc.set_time_utc` (lwipopts.h:1451; sntp_time.cc:11-15).

## Boot hazards

| # | Where | Read / condition | Effect of 0 / 0xFF | Required emulator response |
|---|---|---|---|---|
| H1 | rmii_interface.cc:50 | `CAPAB_ETH_RMII` (0x1000000C bit0) | 0 → no wired netif, no RMII task, no MDIO traffic. Boot OK, "Wired Network" absent. `NetworkInterface::getInterface(0)` becomes the WiFi netif (data_streamer.cc:95) | Set bit0 of 0x1000000C for T1. Either value boots |
| H2 | rmii_interface.cc:60-69 | MDIO reg 2 at PHY 0 then 3 must equal 0x0022 | GET_MDIO=0 → 0x0000; GET_MDIO=1 → 0xFFFF. Both only print an error and fall back to addr 3. No hang | Model a PHY at address 0 returning 0x0022 for reg 2 |
| H3 | rmii_interface.cc:155-156 | MDIO reg 1 bit 2 (link) | GET_MDIO stuck 0 → link never up, task polls every 250 ms forever. Network shows "Link Down", services bind but never see traffic. GET_MDIO stuck 1 → 0xFFFF → link up immediately → `initRx` + DHCP starts. No boot hang in either case | Return reg1 with bit 2 = 1 when a host bridge is attached (e.g. 0x786D), bit 2 = 0 otherwise (e.g. 0x7849) |
| H4 | rmii_interface.cc:175-176 | Periodic reg 1 re-read after 1 s without RX | bit 2 = 0 → `deinitRx` + `link_down` (DHCP stopped, IP lost) | Keep bit 2 stable while "cable" is present |
| H5 | rmii_interface.cc:204-207 | 0x1006082F bit0 inside the ISR | ITU 0x20 raised while bit0=0 → prints `\` and returns. A level IRQ stuck high with nothing to pop gives an **interrupt storm** | RX IRQ line (ITU bit 5) must equal `used_valid` exactly. Never assert it without a presented frame |
| H6 | rmii_interface.cc:209-220 | 0x10060828 ID | 0xFF → `<`, frame ignored. Any ID ≥ 32 → buffer outside the pool (`ram_base+id*1536`) and a bogus `FREE_PUT` later | ID must be one previously pushed via 0x10060824. Value must be ≤ 31 as used by FW |
| H7 | rmii_interface.cc:308-311 | 0x1006081A bit0 | Stuck 1 → every TX returns `ERR_INPROGRESS` + "Oops.. tx is busy!" printf → no ARP/DHCP ever leaves. No hang | Return 0 (copy synchronously on START write) |
| H8 | rmii_interface.cc:88-109; free_queue.vhd:240-249 | Free list bookkeeping | Not recycling IDs from `FREE_PUT` → RX starves after 32 frames (alloc error → drop, eth_filter.vhd:279-280) | Implement free + used FIFOs (≥128 entries free side) |
| H9 | network_interface.cc:284-285 | `mdns_resp_add_netif` without any `mdns_resp_init` call in the tree | Firmware-internal. See OPEN QUESTION Q1. Not caused by the MAC model | Do not "fix" in the emulator. Watch for asserts (`for(;;)` in cc.h:75-76) |
| H10 | itu.vhd:16,275 | ITU edge config of bit 5 is never written by FW (no `ITU_IRQ_EDGE` writes in software) | If modelled edge-triggered, a frame arriving while `used_valid` is already 1 is lost until the next edge. RX stalls | Treat ITU bit 5 (and bit 6) as **level** (open tops: `g_edge_init="10000101"`, ultimate_logic_32.vhd:507) |

No MDIO or MAC read sits in an unbounded busy-wait. The only indefinite loops are FreeRTOS-delay polls (H3) and
service tasks that retry `bind` every 2 s (ftpd.cc:265-268; socket_gui.cc:255-258).

## Interrupts

- **RX: ITU low IRQ bit 0x20 (`ITU_INTERRUPT_RMIIRX`, itu.h:38).** Source = `free_queue.io_irq = used_valid`, a level signal (free_queue.vhd:86; ethernet_rmii.vhd:171).
  - Enabled in `initRx`, disabled in `deinitRx` (rmii_interface.cc:108,114).
  - Dispatch: trap handler reads `ITU_IRQ_ACTIVE` (0x10000005) = `irq_active & imask`, writes the same value to `ITU_IRQ_CLEAR` (0x10000004, clears edge flags only, itu.vhd:144-145,275). If `& 0x20` → `RmiiRxInterruptHandler()` → `rx_interrupt_handler()` → forces a context switch (riscv_main.c:86-98).
  - Ack protocol (exact order): R 0x1006082F (valid?) → R 0x1006082A/2B (size) → R 0x10060828 (id) → **W 0x1006082F** (pop = ack) → `xQueueSendFromISR` (rmii_interface.cc:204-231).
  - One frame per trap. If more are queued, `used_valid` re-asserts (hardware: a few clocks after the pop, free_queue.vhd:99-111) and the trap re-enters.
- **TX: ITU low IRQ bit 0x40 (`ITU_INTERRUPT_RMIITX`).** Set on copy done, cleared by W 0x10060819 (eth_transmit.vhd:170-181).
  - The FW never enables bit 0x40 and never writes `RMII_TX_IRQACK`. No references outside itu.h/rmii_interface.h.
  - The line therefore stays 1 after the first TX, but is masked (`ITU_IRQ_ACTIVE` returns `irq_active and imask`, itu.vhd:171-172). The emulator may model it, but must keep it masked-out of `ACTIVE`.
- No high-IRQ (`ITU_IRQ_HIGH_*`) is used by this block.

## Functional model

### Buffer ownership (32 buffers of 1536 bytes)

- Pool: `ram_base + id*1536`, id 0..31, allocated once from the FreeRTOS heap (rmii_interface.cc:53-56).
- Heap = `ucHeap[8 MB]` in .bss (heap_4.c:102; FreeRTOSConfig.h:24). `new` → `__wrap_malloc` → `pvPortMalloc` (Makefile:252; memory_wrap.cc:11-16). All RAM is in 0x00030000..0x00E90000 (linker.x:7).
- So every DMA pointer (TX address, pool base) is < 0x01000000. The MAC's 26-bit address equals the CPU address: **DMA address = register value & 0x03FFFFFF, direct RAM offset**.
- States of an ID: FREE (in HW free FIFO) → FILLING (HW took it, eth_filter `address_valid`) → USED (HW used FIFO / presented) → POPPED (ISR queued it) → OWNED by lwIP (custom pbuf) → FREE again via `FREE_PUT`.
  - `FREE_PUT` comes from `input_packet` if `netif->input` fails (rmii_interface.cc:243-258).
  - Otherwise it comes from the pbuf free callback (`lwip_free_callback` → `free_buffer`, id = (ptr−ram_base)/1536; network_interface.cc:117-122; rmii_interface.cc:281-300). That runs in whatever task frees the pbuf.
- Software pbuf wrappers: 69 (`PBUF_FIFO_SIZE-1`, network_interface.cc:153-155). This is never the limiting factor.
- On re-link, `initRx` soft-resets the FIFOs and re-inserts all 32 IDs, even IDs still held by lwIP pbufs. Those are `FREE_PUT` again when freed, so duplicates can enter the free list (capacity 127, free_queue.vhd:232-236). Real-HW behaviour; the emulator should replicate it, not dedupe.

### Free queue (free_queue.vhd)

- Two circular FIFOs, 7-bit pointers.
  - FREE side: `FREE_PUT` pushes unless full (head+1 == tail).
  - USED side: the RX filter pushes (id, bytes).
- Presentation register: when `used_valid=0` and USED is non-empty, the head is loaded into `sw_pop_id`/`sw_pop_size` and `used_valid=1` (:99-111,268-274,292-295).
- `ALLOC_POP` write: `used_valid=0`, id=0xFF (:129-133).
- `FREE_RESET`: all four pointers = 0, `used_valid` untouched (:134-135,310-317). This is why FW pops between the two resets (rmii_interface.cc:92-94).
- Allocation (by RX): pop FREE tail. If empty → error → frame dropped. address = `{base[25:8],0x00} + 1536*id` (:258-290).

### RX path (eth_filter.vhd, rmii_transceiver.vhd)

1. The PHY stream includes FCS and one trailing status byte (0xFF good / 0x00 bad), not stripped (rmii_transceiver.vhd:8-13,84-94,175).
2. Drops:
   - Total stream bytes > 1535 (overflow, eth_filter.vhd:95,146) → max frame without FCS = **1530**.
   - Bad CRC (:319-322).
   - RX FIFO full.
   - No free ID (:279-280).
   - RX disabled (:366-372).
3. Destination filter: accept if `promiscuous` or, for each of the 3 dest-MAC 16-bit words `w = b[2k] | b[2k+1]<<8`, `w == 0xFFFF || w == mac[2k] | mac[2k+1]<<8` (:247-250,296-301,306,327).
   - Consequence with PROMISC=0 (FW default): unicast-to-me and broadcast only. **All multicast (01:00:5E:…, 33:33:…) is dropped**, e.g. mDNS/IGMP queries never reach lwIP over RMII.
   - A rejected frame's block is reused for the next frame; it is not pushed (:330-331,345-356).
4. Memory write: 32-bit little-endian words starting at the block address.
   - The first word holds 2 stale bytes then frame bytes 0-1. **Frame byte 0 is at block+2** (:303-312). FW passes `payload = buffer+2` (rmii_interface.cc:244). Bytes block+0..1 and bytes after the frame end are don't-care.
5. `size = total_stream_bytes − 5` = frame length without FCS (eth_filter.vhd:325). Pushed to USED (:345-362).

### TX path (eth_transmit.vhd, rmii_transceiver.vhd)

- FW (rmii_interface.cc:303-323; network_interface.cc:84-115):
  - Requires `link_up`, else `ERR_CONN`. If busy → `ERR_INPROGRESS` (frame lost, no retry).
  - Buffer = single-pbuf payload pointer (Ethernet header). A pbuf chain is first copied into a static 1536-byte buffer (>1536 → `ERR_ARG`).
  - Writes addr, `len<60 ? 60 : len`, START, and returns immediately.
  - Padding bytes 60−len are whatever follows in RAM (not zeroed).
- HW: busy=1 on START. Copies `length` bytes from `address` (byte-exact start, LE words) into a FIFO; busy=0 + TX irq when the last byte is queued (:178-181,235-263). The transceiver prepends 7×0x55 + 0xD5, appends FCS and a 64-cycle gap (rmii_transceiver.vhd:190-248).
- Ownership race on real HW: the pointer may be freed/overwritten (lwIP pbuf, or the static concat buffer on the next call) while the HW still copies.

### MDIO (mdio.c)

Everything happens on writes to 0x1010000A/0B and reads of 0x10100006. `mdio_bit(v)` = W MDIO=v, W MDC=1, W MDC=0 (mdio.c:28-37).

- Read frame (mdio.c:81-114): 33×1, `01`, `10`, `000`, 2 addr bits (both = `addr!=0`, so PHY 0 or 3), 5 reg bits MSB first, one `1` bit (TA1). Then 16× { `mdio_bit(1)`; sample GET_MDIO } → D15 first.
- Write frame (mdio.c:49-79): 33×1, `01`, `01`, `000AA`, reg, `10`, 16 data bits MSB first, trailing `1`.
- Emulator decoder: sample the last MDIO write on each MDC 0→1 write. Once ≥32 ones + `01` + op + 5 phy + 5 reg bits are seen:
  - Read: after the next rising edge (TA1), present data bit `15-(k-1)` on GET_MDIO for rising edges k=1..16 (the value must be readable after the MDC=0 write that follows).
  - Write: collect 2 TA + 16 data bits.
  - Outside a read data phase, GET_MDIO = last MDIO write value (open-drain wired-AND). An unoccupied PHY address reads 0xFFFF.
- PHY registers the FW touches:
  - reg2 (must be 0x0022, Micrel/Microchip OUI).
  - reg1 bit 2 (link).
  - Writes to reg4=0x01E1 (advertise 100/10 FD/HD), reg0=0x1200 (AN enable+restart), 0x1B=0x0500 (KSZ-style IRQ enable), 0x16=0x0002 (strap override).
  - The PHY interrupt pin is unused (`GET_IRQ` hard-wired 1, mdio.c:18,116-121).
  - Register semantics beyond ident/link are ignorable. The PHY part number is inferred (OPEN QUESTION Q4).

### lwIP / sys_arch glue

- liblwip.a is lwIP 2.2.1 (lwip/src/include/lwip/init.h:53-57), built separately with `-march=rv32i` and only `-DIOBASE/-DU2P_IO_BASE`: no `OS/RISCV/U64` (target/libs/riscv/lwip/makefile:6,99). Linked into the app (Makefile:250).
- Options (lwipopts.h):
  - `NO_SYS 0`, `LWIP_TCPIP_CORE_LOCKING 1` (:67,948).
  - `MEM_LIBC_MALLOC 1` → lwIP heap = FreeRTOS heap (:93).
  - `LWIP_SUPPORT_CUSTOM_PBUF 1` + `LWIP_PBUF_CUSTOM_DATA {custom_obj, buffer_start}` (:277-278,726).
  - `ETH_PAD_SIZE 0` (:45).
  - DHCP on with extra options 100,101 (:449,455).
  - DNS 2 servers (:535,544). IGMP (:524). `LWIP_MDNS_RESPONDER 1` (:573).
  - `TCP_MSS 1460`, `TCP_WND 5*MSS`, `TCP_SND_BUF 16384` (:615,640,656).
  - `MEMP_NUM_TCP_PCB 30` (:203). `TCPIP_MBOX_SIZE 80`, `TCPIP_THREAD_PRIO=PRIO_TCPIP=2`, stack 2048 (:838-852; FreeRTOSConfig.h:14).
  - Loopback netif on (:769). `CHECKSUM_CHECK_UDP/TCP 0` (:1253,1258), so host-bridged frames need no valid L4 checksum.
  - SNTP via DNS, 5 servers, startup delay `rand()%5000` ms (:1451-1479; cc.h:105; sntp_opts.h:156).
- sys_arch (network/sys_arch.c):
  - `sys_now = ticks*5 ms` (:61-64).
  - mboxes = FreeRTOS queues, `sys_mbox_post` blocks forever (:81-94,141-144).
  - Semaphores/mutexes = FreeRTOS (:320-447).
  - `sys_arch_protect` = `taskENTER_CRITICAL` unless `xInsideISR` (:545-552). `xInsideISR` is never set by the RISC-V trap path (:69); lwIP is never called from the ISR.
  - `sys_thread_new` = `xTaskCreate` (:506-524).
  - lwIP asserts print and spin `for(;;)` (cc.h:75-76).
- Timing constants the emulator affects: tick 200 Hz from the ITU timer (riscv_main.c:173-186). Link poll 250 ms, RX-idle link re-check 1 s, `initRx` settle 5 ms.

### Services (as built)

| Service | Port / proto | Enable item (store "Network Settings") | Source |
|---|---|---|---|
| Telnet remote menu | TCP 23 | `CFG_NETWORK_TELNET_SERVICE` 0x23, default on | socket_gui.cc:29,71,249-264; network_config.cc:23 |
| FTP | TCP 21, passive data ports 51000..61000 | `CFG_NETWORK_FTP_SERVICE` 0x24, default on | ftpd.cc:220,258-274,324-327,1259 |
| Ultimate DMA (binary command socket) | TCP 64 | `CFG_NETWORK_ULTIMATE_DMA_SERVICE` 0x22, default on | socket_dma.cc:75-81,419-438 |
| Ultimate Ident | UDP 64 (reply text or JSON) | `CFG_NETWORK_ULTIMATE_IDENT_SERVICE` 0x21, default on | socket_dma.cc:81,556-650 |
| HTTP / REST API | TCP 80 | `CFG_NETWORK_HTTP_SERVICE` 0x25, default on | httpd.cc:20-25,40,57; httpd/FreeRTOS/lib/server.h:11 |
| Modem listener (ACIA) | TCP 3000 (config) | modem store, only with `CAPAB_ACIA` | modem.cc:71,120,923-928; listener_socket.cc:75-87 |
| Syslog client | UDP 514 → configured server | `CFG_NETWORK_REMOTE_SYSLOG_SERVER` 0x26, default empty | syslog.cc:14-50 |
| SNTP client | UDP 123 → servers | `CFG_NETWORK_NTP_EN` 0x30, default on | sntp_time.cc:18-32 |
| DHCP client | UDP 68 | "Use DHCP" 0xE0, default on. Static default 192.168.2.64/24 gw .1 dns 8.8.8.8 | network_interface.cc:27-31,364-424 |
| mDNS responder | UDP 5353 | always (see Q1) | network_interface.cc:284-285 |
| FPGA UDP streams (VIC/audio/debug) | UDP src 53248/54272/6510 → 11000+id | REST/menu start. Uses netif 0 MAC/IP + ARP | data_streamer.cc:95,204-206,309-414 |

- Every listener binds `INADDR_ANY` in its own task (prio `PRIO_NETSERVICE`=1). A listener retries on error and does not depend on link state.
- Hostname = `Ultimate-64-II-XXYYZZ` (XXYYZZ = MAC octets 3..5) when product id maps to entry 6 (product.cc:21-29,169-194). Only `[A-Za-z0-9-]` is kept (network_interface.cc:384-396).

## Emulator model tiers

**T0: boot without hang, network absent or down**
- Either clear `CAPAB_ETH_RMII` (H1), or keep it and:
  - RMII: all writes accepted, all reads 0 (so `TX_BUSY=0`, `ALLOC_VALID=0`), RX IRQ never asserted.
  - MDIO: GET_MDIO reads 0 → link stays down (H3). The task polls MDIO every 250 ms (~200 GPIO writes per poll).
- Result: menu shows "Net0 MAC … Link Down" (network_interface.cc:295-297). All services start. Nothing blocks boot.

**T1: functional MAC bridged to the host**
1. Capability bit set. MDIO decoder + PHY at addr 0: reg2=0x0022, reg1 bit2 = host-link state (e.g. 0x786D / 0x7849). Other registers are stored but ignored.
2. RX-filter registers: MAC[6], promisc, rx_enable. TX registers: addr(26), len(12), START, IRQACK, busy.
3. Free queue: `base[25:8]`, FREE FIFO (128, push ignored when 127 full), USED FIFO of (id,size), presentation register (valid,id,size), POP, soft-reset exactly as above.
4. **Host → guest frame** (Ethernet frame without FCS, ≤1530 bytes), only when `rx_enable=1`:
   1. Apply the dest filter (or accept everything when promisc — optionally also accept multicast to let mDNS work; deviation from HW, make it a switch).
   2. Pop a FREE id. If none → drop.
   3. `memcpy(RAM[(base&~0xFF) + id*1536 + 2], frame, len)`.
   4. Push (id, len) to USED. If not currently presenting, present immediately.
   5. RX IRQ (ITU bit 5, level) = `used_valid`.
5. POP write: clear valid, id=0xFF. If USED is non-empty, present the next one (immediately or after a tiny delay). The IRQ follows `used_valid`.
6. **Guest → host frame**: on START write, synchronously copy `len` bytes from `RAM[addr & 0x03FFFFFF]`, hand them to the host bridge, keep busy=0, set the TX irq bit (masked). Synchronous copy removes the real-HW ownership race.
7. `rx_enable 1→0`: drop any in-progress (unpushed) allocation. The id is lost from the free list, as on HW. Presented/used entries are preserved.
8. Host bridge: tap/vmnet (L2) is the natural fit, since the guest does ARP/DHCP itself with MAC `02:15:41:…`. Provide a link up/down toggle through MDIO reg1 bit 2 (detected within ≤1 s idle / 250 ms when down).
9. Optional: the FPGA UDP stream generator (0x10190000, 0x1010040F) injects frames into the same TX path on HW. This belongs to the U64 streaming model.

## Open questions

- **Q1 (firmware, not MAC):** `mdns_resp_init()` is never called in any built source (only defined at lwip/src/apps/mdns/mdns.c:2811). So `mdns_netif_client_id` stays 0 (mdns.c:108), which equals `LWIP_NETIF_CLIENT_DATA_INDEX_DHCP` (netif.h:115-117).
  - `mdns_resp_add_netif` stores its host struct in netif client-data slot 0 (mdns.c:2381), the DHCP slot (dhcp.h:147).
  - `dhcp_start` then finds non-NULL data and memsets `sizeof(struct dhcp)` over it (dhcp.c:818-850). `mdns_pcb` is NULL.
  - Real hardware obtains DHCP leases, so either this is benign in practice or the behaviour differs. Verify in the emulator before blaming the MAC model for DHCP/mDNS anomalies.
- **Q2:** The U64-II top is closed. It is unverified that its RMII block is the open `ethernet_rmii` (same sub-address layout, 26-bit DMA, 1536-byte blocks, level IRQ on ITU bit 5, `ITU g_edge_init` bit 5 = 0). The C driver matches the open VHDL register-for-register.
- **Q3:** Is multi-byte IO on the U64-II CPU bridge also LE sequential bytes (as rvlite bus_converter.vhd:159-178)? The FW's 32/16-bit accesses to RMII rely on it.
- **Q4:** Which PHY is fitted (reg2=0x0022 plus writes to 0x16/0x1B suggest a Microchip KSZ8081-class)? Where do its reset and clock come from? No reset/strap GPIO is touched by the U64-II app (bootloader_u64ii.c:142-143 only has commented-out MDIO writes).
- **Q5:** How does the FPGA UDP stream generator (U64_UDP_BASE/U64_ETHSTREAM_ENA) mux into the RMII TX path, and does it interact with `RMII_TX_BUSY`? Not visible in open sources.
- **Q6:** Which `Flash` subclass answers `get_flash()` on U64-II? This determines the unique-ID bytes and thus the MAC/hostname (w25q_flash.cc:236-249 assumed; see the flash block doc).
