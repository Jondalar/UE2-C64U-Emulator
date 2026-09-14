# USB host: FPGA USB 2.0 core, nano CPU, USB2513 hub

Scope: the U64-II RISC-V `ultimate` build. Its defines are `-DRISCV -DU64=2 -DUSB2513 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000` (target/u64ii/riscv/ultimate/Makefile `OPTIONS`). All paths are relative to `firmware/1541ultimate/`.

The U64-II top level is closed. The register semantics below come from these open sources:
- `fpga/io/usb2` (the "usb_host_nano" core)
- `fpga/ip/nano_cpu`
- the U2+ reference top `fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd`
- `fpga/altera/u2p_io.vhd`

Each was checked against the C drivers and the nano program `software/io/usb/nano_minimal.nan`. That program is the real protocol reference: the CPU never talks to USB directly. It only exchanges data with a 16-bit soft CPU (the "nano") through 2 KB of shared BRAM plus one interrupt.

## Sources read

| File | Key content |
|---|---|
| software/io/usb/usb_nano.h | Shared-RAM layout macros (FIFO, status words, pipe descriptors, `NANO_START`); `UCMD_*`, `URES_*`, `SPLIT_*` bits |
| software/io/usb/usb_hwinit.cc/.h | `initialize_usb_hub`, `USB2513Init`, `USB2503Init` |
| software/io/usb/usb_base.cc/.h | `UsbBase`: ctor, `initHardware`, `init` (blob load), `poll`, `irq_handler`/`get_fifo`, `process_fifo`/`handle_status`, `attach_root`, `bus_reset`, `control_exchange`, `control_write`, `bulk_in`/`bulk_out`, autopipes (`allocate_input_pipe`, `resume_input_pipe`, ...) |
| software/io/usb/usb_device.cc/.h | `UsbDevice::init/init2`, descriptor parsing, `UsbInterface::install` (driver factory), `get_pathname` |
| software/components/factory.h | Driver tester chain (first match wins) |
| software/io/usb/usb_hub.cc/.h | `UsbHubDriver` (`install`, `handle_irqdata`, `reset_port`) |
| software/io/usb/usb_scsi.cc/.h | `UsbScsiDriver` (Bulk-Only Transport), `UsbScsi` block device (INQUIRY, TUR, READ CAPACITY, READ/WRITE 10) |
| software/io/usb/usb_ms_cbi.cc | CBI tester (protocol 0) |
| software/io/usb/usb_hid.cc/.h, usb_hid_selection.h, hid_decoder.h | `UsbHidDriver::test_driver/install/interrupt_handler`, boot/report selection, SET_IDLE/GET_IDLE |
| software/io/usb/usb_ax88772.cc | AX88772 tester (VID/PID) and its pipes (not needed for T1) |
| software/io/usb/keyboard_usb.cc/.h | `system_usb_keyboard.process_data`, matrix output, `getch` |
| software/io/usb/nano_minimal.nan | Nano firmware: attach/reset state machine, pipe scheduler, result coding, FIFO push |
| tools/parse_nano.py; target/common/rules.mk:156-158,164-173; target/common/environment.mk:12 | `.nan` → `.b` (16-bit LE words) → `.o` with symbols `_nano_minimal_b_start/_size` |
| target/u64ii/riscv/ultimate/result/ultimate.elf (nm), output/nano_minimal.b, output/usb_base.lst, output/usb_hwinit.lst | `_nano_minimal_b_start = 0x00121482`, `_nano_minimal_b_size = 0x58E` (1422 bytes = 711 words); the literal `0x10080000` is present in both listings |
| software/system/iomap.h, itu.h, itu.c, u2p.h, u64.h, u64ii_init.cc, assert.c | Bases, capability bit, `wait_ms`, pre-scheduler hardware init, `vAssertCalled` |
| software/portable/riscv/riscv_main.c, crt0.S; software/application/ultimate/ultimate.cc; software/FreeRTOS/Source/FreeRTOSConfig.h | Constructor pass, IRQ dispatch, init order, tick rate, priorities |
| software/io/i2c/i2c_drv.h/.cc, hw_i2c_drv.h/.cc, hw_i2c.h | `i2c_probe`, `i2c_write_block` (SMBus block with length byte), busy polling |
| fpga/io/usb2/vhdl_source/usb_host_nano.vhd, nano_minimal_io.vhd, usb_cmd_nano.vhd, usb_cmd_pkg.vhd, bridge_to_mem_ctrl.vhd, usb_memory_ctrl.vhd, host_sequencer.vhd, usb_host_interface.vhd | Nano I/O map, command/response words, DMA controller, 8 kHz frame counter, IRQ pulse |
| fpga/ip/nano_cpu/vhdl_source/nano.vhd, nano_cpu.vhd, nano_cpu_pkg.vhd, nano_alu.vhd | BRAM/regs split at bit 11, run/reset register, instruction set |
| fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd, fpga/io/itu/vhdl_source/itu.vhd, fpga/altera/u2p_io.vhd | Reference decode (USB window, IRQ bit 2), edge latch, hub/ULPI reset GPIO bits |
| Not read | `fpga/io/usb/*` (USB1 core). The reference top instantiates `usb_host_nano` (ultimate_logic_32.vhd:1042-1066) |

## Address map

`NANO_BASE = USB2_BASE = USB_BASE = IOBASE + 0x80000 = 0x10080000` (iomap.h:33, usb_nano.h:14-15).

On the nano side, word address `w` (0x000-0x3FF) is CPU address `0x10080000 + 2*w`. The byte order is little endian when `g_big_endian = false` (nano.vhd:88-107). The firmware stores LE-assembled `uint16_t` values (usb_base.cc:333-335).

### CPU-visible window

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10080000-0x1008058D | 16 | W (R) | nano code/data | `nano_minimal.b`, 711 words, copied by usb_base.cc:328-336. The nano starts at word 0 (nano_cpu.vhd:46-48) |
| 0x1008058E-0x100805FF | 16 | W | zero fill | usb_base.cc:337-339 |
| 0x10080600-0x100806BF | 16 | R/W | `USB2_DESCRIPTORS[0..7]` | 12-word pipe descriptors, pipe n at `0x10080600 + 0x18*n` (usb_nano.h:32-52; nan:3-20). See below |
| 0x10080700-0x1008071E | 16 | R (FW), W (nano) | attr FIFO data[16] | Report words (usb_nano.h:17-18, usb_base.cc:168; nan:23,1060-1071) |
| 0x100807CC | 16 | R | `USB2_STATUS` (nano `RAM_STATUS` $3E6) | bit0 connected, bit1 operational (bus reset done), bit2 suspended. Values used: 0, 1, 3, 0x8000 after disconnect (usb_nano.h:20,57-59; nan:25,201-202,219-222,244) |
| 0x100807D4 | 16 | R | `NANO_REPORT_FRAME` ($3EA) | Frame counter at the last pipe-0 report (nan:1036-1037). Only printed (usb_base.cc:801,868) |
| 0x100807D6 | 16 | – | `NANO_SIMULATION` ($3EB) | ≠0 makes the nano skip attach detection (nan:129-130,249-257). FW only zero-fills it |
| 0x100807D8 | 16 | – | `NANO_DO_SUSPEND` ($3EC) | Never written by FW |
| 0x100807DA | 16 | W (FW) / clr (nano) | `NANO_DO_RESET` ($3ED) | FW writes 1 (usb_base.cc:303). Nano clears it and runs the bus reset (nan:215-227) |
| 0x100807DC | 16 | R | `NANO_LINK_SPEED` ($3EE) | 0 LS, 1 FS, 2 HS (nan:376-386,418-420). Read at usb_base.cc:311 |
| 0x100807DE | 16 | R/W | `NANO_NUM_PIPES` ($3EF) | Pipe scan limit. Nano writes 1 at start (nan:124-125); FW raises it in `open_pipe` (usb_base.cc:392-394) |
| 0x100807F0 | 16 | R/W (FW) | `ATTR_FIFO_TAIL` ($3F8) | 0..15, advanced by the ISR (usb_base.cc:955,967-971) |
| 0x100807F2 | 16 | R (FW), W (nano) | `ATTR_FIFO_HEAD` ($3F9) | 0..15 (nan:1067-1070) |
| 0x100807F8-0x100807FF | 16 | – | nano scratch $3FC-$3FF | `Descriptor`, temps (nan:37-40). $3F8-$3FF are the only legal LOADI/STORI base slots (parse_nano.py:108-111) |
| 0x10080000-0x100807FF | 8/16/32 | R/W | byte view | Dual-port BRAM, byte-addressed from the CPU (nano.vhd:98-111). FW uses 16-bit writes, one 32-bit read (usb_base.cc:340-341) and 8-bit writes in `deinit` (not called in this ELF) |
| 0x10080800 | 8 | W | `NANO_START` | Any write to 0x800-0xFFF: bit0=1 releases the nano core reset, bit0=0 holds it (nano.vhd:51-66,121-123, 2-FF sync 131-136). Read returns 0 with ack (nano.vhd:118-119). Only the nano CPU is reset; the I/O, sequencer and memory controller keep their state (usb_host_nano.vhd:233-317) |

Reference decode: the USB window is `io_req_usb`, bits 17-19 = 4, commented "8K" (ultimate_logic_32.vhd:963-978). Inside `nano`, only bit 11 is decoded (nano.vhd:51-55), so 0x10080800-0x10080FFF all hit the run register. See Q1 for aliasing.

### Pipe descriptor (12 × uint16 at 0x10080600 + 0x18·n)

| Off | Nano idx | Field | Written by | Meaning |
|---|---|---|---|---|
| +0x00 | 0 | command | FW (start/pause/abort); nano (toggle flip, DONE, PING_EN) | `UCMD_*` bits below. 0 = pipe free (usb_base.cc:391,482) |
| +0x02 | 1 | devEP | FW | `(address<<8) \| endpoint` (usb_base.cc:408; usb_device.cc:205) → core dev 14:8, ep 3:0 (usb_cmd_nano.vhd:55-57) |
| +0x04 | 2 | length | FW; nano decrements | Bytes still to transfer (nan:748-750,896-898) |
| +0x06 | 3 | maxTrans | FW | Packet size; an IN chunk loop continues while the received length equals maxTrans (nan:756-758) |
| +0x08 | 4 | interval | FW (autopipes) | Minimum frames (125 µs) between attempts (nan:994-1004) |
| +0x0A | 5 | lastFrame | nano | Frame of the last attempt (nan:642-643,845-846) |
| +0x0C | 6 | splitCtl | FW | 0 = no split; else `SPLIT_*` (usb_base.cc:508-520) |
| +0x0E | 7 | result | nano (FW writes 0xFFFF once, usb_base.cc:623) | Response word, bit15 = done (nan:651-652). FW reads `& 0x7FFF` (usb_base.cc:499,502) |
| +0x10 | 8 | memLo | FW; nano advances | DMA address low (nan:735-741,889-894) |
| +0x12 | 9 | memHi | FW; nano advances | DMA address high. **memHi==0 ⇒ the pipe is skipped** (nan:1010-1011) |
| +0x14 | 10 | started | FW writes 0; nano stamps | Frame at the first attempt; timeout base (nan:604-607,1019-1026) |
| +0x16 | 11 | timeout | FW | Frames. 0 = retry NAK forever until ABORT_REQ (nan:593-594) |

**Command bits** (usb_nano.h:61-72; nan:73-83):
- 0x8000 `MEMREAD`: data comes from RAM. It also selects the OUT/SETUP code path (nan:610-611).
- 0x4000 `MEMWRITE`: store IN data.
- 0x2000 `PING_ENABLE`: nano-managed.
- 0x0800 `TOGGLEBIT`: expected / next DATA0/1.
- 0x0400 `RETRY_ON_NAK`: interpreted by neither the nano nor the core (usb_cmd_nano.vhd:42-48).
- 0x0200 `PAUSED` = nano `CMD_REQ_DONE`.
- 0x0100 `ABORT_REQ`.
- 0x0040 `DO_DATA`.
- bits 1:0: 0 SETUP, 1 OUT, 2 IN, 3 PING (core decodes bits 2:0, usb_cmd_pkg.vhd:67).

**Result word** (usb_cmd_nano.vhd:79-86; usb_cmd_pkg.vhd:19; nan:87-98; usb_nano.h:86-96):
- 15: done latch.
- 14:12: result code. FW tests `& 0xF000` against 0x0000 `PACKET`/DATA, 0x1000 ACK, 0x2000 NAK, 0x3000 NYET, 0x4000 STALL, 0x5000 ERROR, 0x6000 ABORTED (the last is synthesised by the nano).
- 11: received data PID toggle. The nano also ORs 0x0800 here as `CMD_RES_TIMEOUT` (nan:96,598-600).
- 10: no_data (zero-length packet).
- 9:0: length of the last packet.

**SplitCtl** (usb_nano.h:74-83; usb_cmd_nano.vhd:59-65): 15 do_split, 14:8 hub address, 7 complete (nano ORs it, nan:537), 6 low speed, 5:4 endpoint type, 3:0 hub port (1-based).

### Other registers touched by the USB path

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x1010000D | 8 | W/R | `U2PIO_HUB_RESET` | Write 1 asserts hub reset, 0 releases it (usb_hwinit.cc:76-77). Read = latch bit0. `hub_reset_n = not bit0` (u2p_io.vhd:78-79,125-126,146). 50 dummy reads are used as a delay (usb_hwinit.cc:85-91) |
| 0x1010000F | 8 | W | `U2PIO_ULPI_RESET` | bit0 = ULPI PHY reset; bit7 = buffer enable, bit6 = disable (u2p_io.vhd:129-135,147; u2p.h:100-101). FW writes 0x01, 0x00, 0x80 (usb_hwinit.cc:147,165,168) |
| 0x10100700 / 01 / 03 / 06 / 08 | 8 | W/R | HW I2C (`U64II_HW_I2C_BASE`, u64.h:34) | data_out, status (0x80 BUSY, 0x04 ERROR=NACK), stop, channel, scan_enable (hw_i2c.h:5-20). Hub probe/config on channel 2 = `I2C_CHANNEL_3V3` (i2c_drv.h:80) |
| 0x1000000D bit7 | 8 | R | `CAPAB_USB_HOST2` 0x00800000 | itu.h:72; itu.c:6,24; gate at usb_base.cc:190. Reference top: `cap(23) := g_usb_host2` (ultimate_logic_32.vhd:311) |
| 0x10000001/02/04/05 | 8 | W/W/W/R | ITU IRQ enable/disable/clear/active, bit 0x04 | usb_base.cc:43,201-202,344-345; riscv_main.c:86-87 (doc 02) |
| 0x10000024 | 8 | W | `ITU_USB_BUSY` | 1 around READ(10)/WRITE(10) (usb_scsi.cc:843-855,873-885). LED only |
| 0x10100300-0x1010030B | 8/32 | W | `MATRIX_KEYB` | USB keyboard → C64 matrix (keyboard_usb.cc:214-229), see doc 05 |
| 0x00000000 | 32 | R | NULL→`current_address` | Root-device pipe setup dereferences `dev->parent` == NULL (usb_base.cc:411; usb_device.cc:62). See H10 |

### Nano-internal I/O space (only needed for a low-level model; not CPU-visible)

| Nano io addr | Dir | Function (nano_minimal_io.vhd / usb_cmd_nano.vhd / bridge_to_mem_ctrl.vhd) |
|---|---|---|
| $20/$21/$27/$28/$29 | W | set do_chirp / chirp level / SOF enable / **IRQ pulse** / reset chirp filter (107-133) |
| $26 | W | speed ← data(1:0) (123-124). Reset value "01" (193) |
| $30/$31/$37/$3C/$3E | W | clear do_chirp / chirp level / SOF enable / sof_tick latch / disconnect latch (135-155) |
| $39/$3B/$3C/$3D/$3E/$3F | R | bit15 chirp-K filter, frame_count[15:0], bit15 sof_tick, bit15 mem_ctrl_ready, bit15 disconnect latch, ULPI status byte (211-228). Disconnect = RxEvent "10" or linestate 00 for 512 clocks at non-HS (62,86-95) |
| $60/$61/$62/$63 | W | CMD_REQUEST, buffer ctrl (15:14 index, 13 no_data, 9:0 len), dev/ep, split (usb_cmd_nano.vhd:42-65) |
| $64 | R | response word (usb_cmd_nano.vhd:79-86) |
| $70/$71/$72/$73/$74 | W | DMA addr lo, addr hi (9:0 → 26-bit address), write BRAM→RAM (size n+3), read RAM→BRAM, buffer index (bridge_to_mem_ctrl.vhd:30-31; usb_memory_ctrl.vhd:133-151) |
| $80-$BF / $C0-$FF | R / W | ULPI register 5:0, stalls the nano until the PHY ack (nano_minimal_io.vhd:156-175,199) |

## Init / boot sequence as seen from the bus

0. **Before `main`**: crt0 runs `__init_array` (crt0.S:197-208). The global `UsbBase usb2` constructor (usb_base.cc:15,41-57) writes W8 `0x10000002 ← 0x04` (usb_base.cc:43).

1. **`custom_hardware_init`** (u64ii_init.cc:114-138). This runs before the scheduler with IRQs off (doc 02 §boot).
   - The `Hw_I2C_Driver` ctor writes W8 `0x10100708 ← 0` (u64ii_init.cc:116; hw_i2c_drv.h:17).
   - `nau8822_init` (u64ii_init.cc:120).
   - `initialize_usb_hub` (u64ii_init.cc:123 → usb_hwinit.cc:144-169) then runs these steps:
     1. W8 `0x10080800 ← 0x00` (146).
     2. W8 `0x1010000F ← 0x01` (147).
     3. 1024 × W16 `0x10080000 + 2i ← 0` (148-151).
     4. `set_channel(2)`: no bus access (156; hw_i2c_drv.cc:7-10).
     5. `i2c_probe(0x58)` (157; i2c_drv.cc:124-130):
        - W `0x10100706 ← 2`, poll R `0x10100701` until bit7 = 0 (hw_i2c_drv.cc:12-16).
        - W `0x10100700 ← 0x58`, poll until bit7 = 0. bit2 = NACK (hw_i2c_drv.cc:31-37).
        - W `0x10100703 ← 1`, poll bit7 (25-29).
     6. If there was an ACK, `USB2513Init` (79-115) runs:
        - Hub reset pulse: W `0x1010000D ← 1`, 50 × R; W `← 0`, 50 × R.
        - `write_block` sends `58 FF 01 02`: STCD reset (98).
        - Then `58 00 18` + 24 config bytes (104; table at 71-74): VID 0x0424, PID 0x2513, DID 0x0BA0, CFG1 0x9A, CFG2 0x20, CFG3 0x02, NRD/PDS/PDB 0, MAXPS 0x01, MAXPB 0x32, HCMCS 0x01, HCMCB 0x32, PWRT 0x32, then 7 × 0.
        - Then `58 FF 01 01`: STCD attach (110). The block format is SMBus with a length byte (i2c_drv.cc:290-322).
        - Any error prints a message and returns early (98-113).
     7. If 0x58 NACKed: `i2c_probe(0x5A)` → `USB2503Init`: reset pulse + 17 `write_byte` pairs (117-142). Otherwise it prints "No USB hub found." (162).
     8. W8 `0x1010000F ← 0x00` (165), then `← 0x80` (168).

2. **`ultimate_main`** (ultimate.cc:87) reads capabilities from 0x1000000C-0F. Then `usb2.initHardware()` (ultimate.cc:109 → usb_base.cc:185-208) runs. It sets `prev_status = 0xFF` (188) and reads capabilities again (190).
   - **bit23 = 0**: printf "No USB2 hardware found. (%08x)" (206). Nothing else happens.
   - **bit23 = 1**: creates the mutex, `queue`(16), `cleanup_queue`(16) and the binary semaphore (191-195). Then W8 `0x10080800 ← 0` (196), 1024 × W16 zeros (197-200), W8 `0x10000002 ← 0x04` (201) and W8 `0x10000004 ← 0x04` (202). Finally it creates task "USB Task", PRIO_POLL = 1 (204; FreeRTOSConfig.h:13).

3. `system_usb_keyboard.setMatrix(0x10100300)` / `enableMatrix(true)` (ultimate.cc:114-116) (doc 05).

4. **"USB Task"** `poll_usb2` → `usb2.init()` (usb_base.cc:170-177, 317-350):
   - W8 `0x10080800 ← 0` (325).
   - 711 × W16 blob words from `0x00121482` (328-336).
   - 313 × W16 `0` (337-339).
   - R32 `0x10080000`, printed as "First DW". Expected `83ef0a9d` (340-342; bytes `9d 0a ef 83` of nano_minimal.b).
   - W8 `0x10000004 ← 0x04` (344), W8 `0x10000001 ← 0x04` (345), W8 **`0x10080800 ← 0x01`** (346).
   - Creates "USB Input Event Task", PRIO_DRIVER = 1 (349; FreeRTOSConfig.h:11).

5. **Steady state without a device** (no bus I/O):
   - The USB Task loops `poll()` every 2 ticks. That is `xQueueReceive(cleanup_queue, 40)`, and `rootDevice == NULL` means nothing is polled (usb_base.cc:173-176,353-371).
   - The input task blocks on `xQueueReceive(queue, 5000)` and prints "@" every 25 s (239-245; tick = 200 Hz, FreeRTOSConfig.h:21).

6. **Real nano after START**, as a reference for HLE timing:
   - `PipesInUse = 1`, `Pipes[0].command = 0` (nan:124-127).
   - ULPI power/VBUS handling (132-171), then about 700 ms delay (178-180; `ResetDelay` 5601 × `loop_timer`).
   - No device: it spins in `_wait_for_device_attachment` (190-194) with no CPU-visible effect.
   - Device present: SPEED, then `RAM_STATUS = 1`, then it pushes 0xFFF1 and raises the IRQ (196-203).

## Boot hazards

| # | Where | What the firmware depends on | Failure with a naive model | Required emulator response |
|---|---|---|---|---|
| H1 | usb_hwinit.cc:146-151 (from u64ii_init.cc:123) | Pre-scheduler writes to 0x10080000-0x100807FF and 0x10080800. **Unconditional**: they run even when CAPAB bit23 = 0 | Bus fault / trap on an unmapped IO window → boot dies before the scheduler | Map 0x10080000-0x10080FFF. Accept 8/16/32-bit writes (RAM, or ignore) |
| H2 | usb_hwinit.cc:157-159 → i2c_drv.cc:124-130; hw_i2c_drv.cc:4-5,12-37 | I2C status 0x10100701 bit7 (BUSY) is polled with no timeout, IRQs off. bit2 = NACK | BUSY stuck 1 (e.g. read 0xFF) → hang before scheduler start | bit7 = 0 after each op. bit2 = 1 for 0x58/0x5A gives "No USB hub found."; bit2 = 0 gives "USB Hub successfully configured." Both boot. The hub config has no later functional effect |
| H3 | usb_base.cc:190 | `CAPAB_USB_HOST2`, 0x1000000D bit7 | – | 0: USB stack absent, no hang. 1: H4-H6 apply |
| H4 | usb_base.cc:325-346 | Blob load, 32-bit read of 0x10080000, NANO_START = 1. Read values are only printed | Trap on 16/32-bit access widths | Accept all widths. Any read value boots |
| H5 | riscv_main.c:86-87,107 → usb_base.cc:929-972 | ITU bit 2 raised → ISR drains the FIFO: reads TAIL 0x100807F0, then HEAD 0x100807F2 in a loop until two reads match (958-960) | Spurious bit 2 with unstable HEAD reads → endless loop in the ISR. With stable garbage head≠tail → bogus words (H6) | **T0: never raise ITU bit 2.** HEAD/TAIL must read back stably (RAM, or 0/0) |
| H6 | usb_base.cc:254-268 | FIFO word dispatch: 0x0000 → semaphore (937-939); 0xFFF0-0xFFFF → status; else `inputPipeObjects[pipe]` is read **before** the `pipe >= 8` check (261 vs 264) | Word 8..0xFFEF → out-of-bounds read, then a call through a garbage callback pointer → crash | Push only 0x0000, an allocated autopipe index 1..7, or `0xFFF0 \| status` |
| H7 | usb_base.cc:485-505 | Pipe-0 completion = semaphore given by the ISR. `complete_command(100)` waits 0.5 s, sets ABORT_REQ (0x0100), waits 0.5 s more, then returns 0xFFFF | No report → every control transfer takes 1 s and fails → enumeration fails (no hang) | Report pipe 0 well inside 100 ticks. On ABORT_REQ report `ABORTED` |
| H8 | usb_base.cc:301-311 | After status 1: `DO_RESET = 1`, then `wait_ms(50)` ×3 checking 0x100807CC bit1, then `LinkSpeed` is read unconditionally | Status never 3 → 150 ms lost and a stale speed is used. `wait_ms` needs the ITU timer (doc 02 H2) | Within ≤ 50 ms after DO_RESET: DO_RESET = 0, `USB2_STATUS = 3`, `LINK_SPEED = 2` (root) |
| H9 | usb_base.cc:411; usb_device.cc:62; usb_base.cc:508-520 | `initialize_pipe` for a **root** device reads `NULL->current_address` (32-bit read at 0x00000000). The split for speed 0/1 then uses that garbage as hub address | Trap on reading address 0 → crash when a root device installs a driver. FS/LS root → non-zero SplitCtl → nano issues splits to a non-existent TT → the device never works | Reads at 0x00000000 must not trap. **The root device must report speed 2 (HS)**, which forces SplitCtl = 0 (on real HW the root is the USB2513 HS hub) |
| H10 | usb_base.cc:853-892; nan:701-708,752-765 | `bulk_in` loops `while (len > 0)`. A DATA result (0x0000) that consumed nothing (ZLP, or toggle mismatch → nano reports with length unchanged) is not an error | Infinite loop in the calling task **while holding the USB mutex** → all USB I/O dead | Every DATA report on a bulk IN must consume ≥1 byte. The device model returns exactly the requested counts: CSW 13, sense 18, INQUIRY 36, capacity 8, sectors × block size |
| H11 | usb_scsi.cc:75-88,121-134 | `get_max_lun`: `if(!i) return 0;` otherwise it returns uninitialised `dummy_buffer[0]`, including when `i < 0` (STALL) | STALL on GET_MAX_LUN → garbage max_lun → overrun of the 16-entry `scsi_blk_dev/path_dev` arrays | Answer class request `A1 FE` with 1 byte 0x00 (≤ 15) |
| H12 | usb_hub.cc:240-250; FreeRTOSConfig.h:72; assert.c:23-30 | Hub status pipe: `configASSERT(data_length <= 4)`. The assert is a `while(1)` in a critical section | Received length > 4 → whole system hangs | Keep `length` ≤ `Length` (2). Never write a larger remaining count than requested |
| H13 | usb_hub.cc:252-257,406 | The hub pipe is re-armed only from `handle_irqdata` when `irq_data[0] != 0` | Reporting a zero bitmap → pipe stays PAUSED → hub never reports again (no hang) | NAK (no report) when no port changed |
| H14 | usb_device.cc:157-166,221-247; usb_hub.cc:265-270 | Exact lengths: device descriptor 18, config = wTotalLength, port status 4 | Mismatch → init retries 3× (usb_base.cc:125-132) → device reset loop / "Failed to read port status" | Return exact descriptor lengths |
| H15 | nano_minimal_io.vhd:104,127-128; usb_host_nano.vhd:268-274; itu.vhd:238-239,275 | IRQ is a 1-cycle pulse per push → ITU bit 2 must be edge-latched | Level model with a pulse → lost IRQs → pipe-0 timeouts (H7) | Set the ITU bit-2 edge flag on every FIFO push |
| H16 | usb_hub.cc:406-407 | `handle_irqdata` resumes the status pipe and only then clears `irq_data[0]`. The real nano needs a scan and a transaction before it answers | A model that runs the resumed pipe before the next instructions writes the new bitmap, the driver wipes it, H13 leaves the pipe paused: after the first port reset the hub never reports again (seen in the emulator: enumeration stops after "Issuing reset on port 1.") | Start a pipe no earlier than about one frame (125 µs) after the firmware's write |

## Interrupts

- **Line**: nano `OUTP SEND_INTERRUPT` ($28) → `interrupt_out` pulse (nano_minimal_io.vhd:127-128) → `pulse_synchronizer` to sys_clock (usb_host_nano.vhd:268-274) → ITU `irq_in(2)` (ultimate_logic_32.vhd:536). This is ITU low IRQ bit **0x04**, edge (itu.vhd:238-239,275; doc 02 table). No high-IRQ is used.
- **Raise**: once after every FIFO push:
  - a pipe report: push the pipe number, set DONE in command, pulse (nan:1028-1047);
  - a status change: push `0xFFF0 | RAM_STATUS`, pulse (nan:1073-1077).
- **Enable**: disabled in the ctor and in `initHardware` (usb_base.cc:43,201). CLEAR then ENABLE in `init` just before NANO_START = 1 (344-346).
- **Ack**:
  1. The ISR reads `ITU_IRQ_ACTIVE` and writes `ITU_IRQ_CLEAR ← pending` (riscv_main.c:86-87), then `usb_irq()` (107).
  2. `irq_handler` pops until TAIL == HEAD (usb_base.cc:936-947). The TAIL write is the only feedback to the nano: it stops reporting while the FIFO is full (nan:950-951,1007-1008,1053-1058). Status pushes do not check for full (1073-1077).
- **Word routing**:
  - 0x0000 → `xSemaphoreGiveFromISR(commandSemaphore)` (937-939).
  - Anything else → `xQueueSendFromISR(queue)` (944) → input task `process_fifo`:
    - 0xFFFx → `handle_status(x)` (254-258), which acts only when x differs from `prev_status` (275-286);
    - 1..7 → driver callback (261-268).

## Functional model

### F1. Blob and run control

- The blob is `nano_minimal.nan`, assembled by `parse_nano.py` (rules.mk:156-158) into 16-bit LE words (parse_nano.py:170-177), with the immediates in a literal pool after the code (241-248, 274-277). It is linked as `.rodata` with `_nano_minimal_b_start/_size` (rules.mk:164-173).
- An HLE emulator may ignore the code words. It must still keep the 2 KB RAM semantics, because the FW reads and writes all fields there.
- `0x10080800` bit0: 0 = nano held in reset (HLE idle: no RAM writes, no IRQ).
- 0→1 = nano starts at word 0. HLE: write `0x100807DE ← 1` and `pipe0.command ← 0` (nan:124-127), then enter the link state machine.

### F2. Link / status state machine (nan:147-227, 240-268, 366-453)

| Event | Nano effect (CPU-visible) | FW reaction |
|---|---|---|
| START, device absent | Nothing. It re-checks linestate forever (190-194) | – |
| Device present, ~700 ms after START or after a detach (178-185) | SPEED ← line state; `RAM_STATUS = 1`; push 0xFFF1; IRQ (196-203) | `attach_root` in the input task (usb_base.cc:94-114) |
| `DO_RESET ≠ 0` while attached (207-208, 263-264) | `DO_RESET = 0`, `RAM_STATUS = 0` → bus reset (FS ≈ 120 × 125 µs, nan:392-407; HS chirp → `LinkSpeed = 2`, 418-453) → `LinkSpeed`, `RAM_STATUS = 3`, SOF on (216-227). **No IRQ** | Polls 0x100807CC bit1 every 50 ms ×3, reads 0x100807DC (usb_base.cc:303-311) |
| Detach (disconnect latch, 240-247) | `RAM_STATUS = 0x8000`; SOF off; push 0xFFF0; IRQ; back to the attach wait (211-213, 261-262) | `queueDeinstall(rootDevice)` → `disable` frees pipes → deinstall in the USB Task (usb_base.cc:280-298, 155-161) |

### F3. Pipe scheduler (nan:941-1047)

**Frame counter**: 16 bits, +1 per 7500 ULPI clocks = 8000 Hz. It runs regardless of link speed and wraps (host_sequencer.vhd:186-193,379). HLE: `frame = ⌊t_emu / 125 µs⌋ mod 65536`.

Each scan, for `n = 0 .. NUM_PIPES-1`:

1. `cmd = command` (read until stable, 968-973). Skip if `cmd == 0` or `cmd & 0x0200` (975-978).
2. Skip if `(frame − lastFrame) mod 2¹⁶ < interval` (994-1004).
3. Skip if the FIFO is full (1007-1008). Skip if `memHi == 0` (1010-1011).
4. If `started == 0`, set `started = frame` (604-607).
5. Path: `cmd & 0x8000` → OUT/SETUP (807-937), else IN (613-770). Token = `cmd & 3`, toggle = `cmd & 0x0800`, address/endpoint from devEP.

| Path / device response | Nano writes | Report? |
|---|---|---|
| SETUP/OUT → ACK or NYET | `sent = min(length, maxTrans)` read from memHi:memLo (772-796); toggle ^= 1; mem += sent; length −= sent (884-898). NYET also sets PING_ENABLE (879-882) | Only once `length == 0` (900-904). Otherwise the next chunk goes out on a later scan. Result ≈ 0x9400 (ACK, no_data) |
| IN → DATA n, toggle ok | If MEMWRITE: copy `min(n, length)` bytes to memHi:memLo (715-729); mem += n; toggle ^= 1; length −= n (735-750). If `length > 0 && n == maxTrans` → IN again at once (752-758) | Yes. `result = 0x8000 \| t<<11 \| (n==0)<<10 \| n` |
| IN → DATA, toggle mismatch | Nothing consumed (701-708) | Yes (→ H10) |
| NAK (OUT also sets PING_ENABLE, 906-910) | If ABORT_REQ: `result = (result & 0x0FFF) \| 0x6000` → report (584-591). Else if `timeout == 0`: leave, retry next scan (593-594). Else if `frame − started ≥ timeout`: `result \|= 0x0800` → report (595-601) | As stated |
| STALL, or other unexpected code | Result stored | Yes (676-678, 869-870) |
| ERROR | Retry at once, up to 3 times (526, 680-685, 872-877) | After the retries |
| HS OUT with PING_ENABLE | PING first; ACK → clear flag, send data; NAK → timeout rule (916-937) | HLE: skip PING |
| splitCtl ≠ 0 | SSPLIT only if the TT is idle and `frame & 7 < 5` (484-511); CSPLIT on the next FRAME_TICK (942-956); NYET ≥ 4 frames → give up (687-694) | HLE: execute as non-split |

**Report** (1028-1047): `FIFO[head] = n; head = (head+1) & 0xF`; if n == 0, `ReportedAt = frame`; `command |= 0x0200`; IRQ.

**HLE simplification that is safe for this FW**: a whole transfer may complete in one scan.
- Copy all bytes, advance mem by the total, and leave `length` = the remaining count.
- Flip the toggle once per packet.
- Result = last packet.
- The FW only consumes `result`, `length`, `command & 0x0800` (usb_base.cc:608, 798, 809, 863, 891; 725-730).

### F4. Firmware-side transfer helpers (pipe 0 unless noted)

| Helper | What FW writes | Waits (FreeRTOS ticks, 200 Hz) | Accepts |
|---|---|---|---|
| `control_exchange` (usb_base.cc:561-642) | SETUP: devEP, length=8, maxTrans, mem=`setupBuffer`, splitCtl, started=0, command **0x8040** (576-586) | 100 (+100 abort) | ACK, else -1 (589-593) |
| ″ data/status IN | length=inlen, mem=in (if non-NULL; adds MEMWRITE), started=0, command **0x0C42** / **0x4C42** (595-604). `timeout` is **not** written, so it keeps the last value | 100 | STALL → -4 (615-618). Anything else passes. `transferred = inlen − length` (608) |
| ″ status OUT (only if transferred > 0) | length=0, result=0xFFFF, command **0x8C41** (620-625) | 100 | ACK/NYET; others print only |
| `control_write` (645-699) | SETUP with timeout=15000 (659); OUT data **0x8841** (674); IN status length=4, **0x0842** (686-688) | 500 each | ACK, ACK, DATA (0x0000) |
| `bulk_out` (747-821) | Per ≤ 49152-byte chunk: devEP, maxTrans, splitCtl, mem, length, timeout, started=0, command = `toggle \| 0x8441` (776-792) | `timeout` (default 20000) | ACK/NYET; 5 error retries; next toggle = `command & 0x0800` (798) |
| `bulk_in` (823-903) | devEP, maxTrans=min(len,MaxTrans), splitCtl, mem, timeout (846-851); per chunk length, started=0, command = `toggle \| 0x4442` (853-860). An unaligned buffer gets a bounce copy (834-837) | `timeout` | DATA; STALL → -4; others count down retries → -1 (867-878) |
| `allocate_input_pipe` → `open_pipe` + `activate_autopipe` (702-723, 386-400, 426-445) | First slot 1..7 with command == 0; raise NUM_PIPES; devEP, maxTrans, interval, splitCtl, command = `(saved \| 0x4042) \| 0x0200` | – | index, or -1 |
| `resume_input_pipe` (462-477) | length=`Length`, memHi/Lo=buffer, started=0, command &= ~0x0200 | report → callback (input task) | `getReceivedLength = Length − length` (725-730) |
| `free_input_pipe` (732-745) | command = 0 | – | – |

Units: FW wait arguments are ticks. The `descr->timeout`/`interval` fields are 8 kHz frames (comment usb_base.cc:793). `doPing` (541-558) is never called.

### F5. Enumeration (FW side)

1. `attach_root` (usb_base.cc:94-114): `bus_reset`; `new UsbDevice(speed)` (control MaxTrans 8 for LS, else 64, usb_device.cc:80-86); `init_device` picks an address 1..127 (usb_base.cc:81-90) and tries `dev->init` up to 3 times (125-132); then `init2`; then `install`.
2. `init` (usb_device.cc:374-384): GET_DESCRIPTOR(device, 18) at address 0. It must return exactly 18 bytes, and `bMaxPacketSize0` must be in {8,16,32,64} (157-179). Then SET_ADDRESS (193-209). Failure → `device_reset` (usb_device.h:300-305): `bus_reset` for the root, the hub's `reset_port` for children.
3. `init2` (386-402):
   - LANGID via string 0, 4 bytes (140-151);
   - manufacturer/serial/product strings with a 256-byte buffer (119-138);
   - `get_configuration(0)`: 9 bytes, then exactly wTotalLength (211-247);
   - parsing (262-324): interfaces keyed by bInterfaceNumber < 6; endpoints of length 7/9, max 4 per interface (usb_device.h:172-181); HID class descriptor (0x21, length 9).
4. `install`: for each interface, `Factory::create` tries the testers in registration order; the first non-NULL wins (factory.h:32-40; usb_device.h:214-223). The testers are:
   - hub: device class 9 and protocol 1\|2 (usb_hub.cc:63-82);
   - BOT: device class 0\|8, interface class 8, protocol 0x50 (usb_scsi.cc:45-73);
   - CBI: interface class 8, protocol 0 (usb_ms_cbi.cc:44-62);
   - HID: interface class 3 (usb_hid.cc:868-875);
   - AX88772: VID 0x0B95, PID 0x772A/0x772B/0x7720 (usb_ax88772.cc:118-136).
5. `find_endpoint(code)` matches `(bmAttributes & 3) | (bEndpointAddress & 0x80)`, not the endpoint number (usb_device.h:242-255). So 0x82 = bulk IN, 0x02 = bulk OUT, 0x83 = interrupt IN.
6. Names come from `get_pathname` (usb_device.cc:404-418): "USB" + one digit per hub level (0-based port). A root device is named "USB".

### F6. Mass storage (Bulk-Only) contract

- **install** (usb_scsi.cc:90-138):
  - SET_CONFIGURATION(bConfigurationValue) (95; usb_device.cc:335-351);
  - SET_INTERFACE only if alt ≠ 0 (usb_device.cc:353-357);
  - GET_MAX_LUN (H11);
  - bulk IN/OUT pipes (112-117);
  - for each LUN: `UsbScsi::reset` and a root entry in the file manager (121-134).
- **reset** (236-256): `vTaskDelay(100)`, then INQUIRY with 36 bytes (481-482; timeout 8000 frames, 3 retries, 409-414), then REQUEST SENSE with 18 bytes (295-341).
  - sense[2] == 0 → ready.
  - ASC 0x28/0x04 → not ready; 0x3A → no media (343-366).
- **Command**: CBW 31 bytes, signature "USBC", tag, data length, flags 0x80/0x00, LUN, CB length (104-108, 375-392).
  - Data phase: `bulk_out`/`bulk_in` with a 24000-frame timeout (409, 419, 428).
  - CSW: `bulk_in` 13 bytes, which must be "USBS" (258-276). Status 1 → REQUEST SENSE → -7 (451-457).
- **Poll** (USB Task, 150-213): TEST UNIT READY (524-526) at a state interval: unknown 10, no media 250, not ready 20, ready/error 500 ms, measured with `getMsTimer` (doc 02 H5). When newly ready: READ CAPACITY(10) returns 8 bytes, BE last LBA + BE block length, with block size ≤ 4096 (550-570). Then `attach_disk`.
- **I/O**: READ(10) 0x28 / WRITE(10) 0x2A with exact byte counts, `ITU_USB_BUSY` pulsed (837-888). No other SCSI opcodes are sent by this driver.

### F7. HID keyboard contract

- **install** (usb_hid.cc:877-1037):
  1. SET_CONFIGURATION; SET_INTERFACE(alt ≠ 0 only).
  2. GET_DESCRIPTOR(HID report). This happens only if a 0x21 descriptor was parsed; wLength comes from its bytes 7-8 (usb_device.cc:425-459).
  3. Parse → report or boot protocol (usb_hid_selection.h:47-66).
  4. An interrupt-IN endpoint is required (959-968).
  5. Pipe: Interval 160 frames (20 ms), `Length = MaxTrans = wMaxPacketSize` (≤ 64) (969-977); allocate (979).
  6. Subclass 1 → SET_PROTOCOL(report ? 1 : 0) (989-991).
  7. SET_IDLE (993-995); GET_IDLE with 1 byte when the idle units are ≠ 0 (998-1008; accepted only if it echoes, usb_hid_selection.h:87-92).
  8. Resume the pipe (1036).
- **Data** (1178-1492): the report is turned into an 8-byte boot report (`copyKeyboardReport` hid_decoder.h:299-309, or `synthesizeBootKeyboardReport`) → `usb_hid_update_keyboard_source` → `system_usb_keyboard.process_data` (usb_hid.cc:222-245; keyboard_usb.cc:464-466). That feeds the key buffer (menu `getch`, ultimate.cc:172) and MATRIX_KEYB (keyboard_usb.cc:214-229). The pipe is resumed at the end (usb_hid.cc:1492).
- **Device side**: when there is no new report, NAK (no FIFO push). A report is 8 bytes: modifiers, reserved, 6 keycodes.

### F8. Hub contract (faithful topology)

- **install** (usb_hub.cc:118-204):
  1. SET_CONFIGURATION.
  2. GET_HUB_DESCRIPTOR `A0 06 0029`, wLength 64 (29, 127-136). The FW uses [2] bNbrPorts (≤ 7: `children[7]`, and the status bitmap is a single byte, 254, 264), [3] characteristics, [5] power-good ×2 ms, [6] mA.
  3. GET_HUB_STATUS (148-150).
  4. SET_PORT_FEATURE(POWER) for each port (161-167); `wait_ms(power_on_time)` (168).
  5. Status pipe on an interrupt-IN endpoint: Interval 1160, Length 2, MaxTrans 64 (193-203).
- **Change handling** runs in the USB Task via `poll` → `handle_irqdata` (228-237, 252-408). For each bitmap bit j+1: GET_PORT_STATUS with exactly 4 bytes (265-270), then:
  - Connect change + connected + no reset busy → CLEAR C_PORT_CONNECTION, `wait_ms`, SET PORT_RESET (278-298).
  - Reset change + enabled → speed from wPortStatus high byte (bit2 HS, bit1 LS) (353-354), `wait_ms`, new device, `set_parent` (split from the hub address/port, usb_device.h:289-298), `init_device`, CLEAR C_PORT_RESET, `init2`, install (355-379).
  - Reset change without enable → 4-poll timeout (381-397).
  - Disconnect → deinstall the child (299-316).
  - Afterwards always resume the pipe and clear `irq_data[0]` (406-407).
- **Hub model duties**:
  - Report the change bitmap only when non-zero (H13).
  - After SET PORT_RESET, set ENABLE + C_RESET and the speed bits, then report.
  - The root hub itself must be HS (H9).

### F9. Low-level alternative: run the real nano code

The nano CPU is fully specified.
- Opcodes: bits 15:11 opcode, 9:0 address (parse_nano.py:182-206; nano_cpu_pkg.vhd:11-44).
- LOADI/STORI use a base slot $3F8+bits2:0 plus offset bits10:3 (parse_nano.py:117-118; nano_cpu.vhd:128-132).
- Flags: Z/N are updated by ALU ops, INP and LOADI. C is updated by ADD/SUB/CMP/ADDC, and SUB carry = no borrow (nano_alu.vhd:34-79). Branches: nano_cpu.vhd:59-68. CALL/RET use a hardware stack (nano_cpu.vhd:204-217).

A model at this level also needs everything in the nano I/O table:
- a ULPI PHY: function control $04, OTG control $0A, IRQ status $13 VBUS bits, product-ID-low $02; value 0x06 skips the power check, nan:132-171;
- the RX-CMD status byte (linestate, RxEvent);
- the chirp filter;
- a packet-level sequencer with device responses;
- the DMA controller with `MEM_CTRL_READY`.

Its busy-wait delays (`loop_timer`, `ResetDelay`) cost millions of nano instructions. HLE (F2/F3) is recommended.

## Emulator model tiers

**T0: boot, USB reports "no devices", never hangs**
- **CAPAB bit23** (0x1000000D bit7): either value.
  - 0 → log "No USB2 hardware found. (…)", no USB tasks.
  - 1 → the USB Task and input task run idle. Logs "Nano CPU based USB Controller: 1422 bytes loaded from 00121482. First DW: 83ef0a9d" and "@" every 25 s.
- **0x10080000-0x10080FFF** is needed in both cases (H1):
  - recommended: 2048-byte RAM + a write-only START latch that reads 0;
  - minimum: writes ignored, reads 0 (HEAD = TAIL = 0).
- **Never assert ITU bit 2** (H5).
- **0x1010000D / 0x1010000F**: latch bit0; any read value.
- **I2C master**: BUSY = 0 always. NACK (status 0x04) for 0x58/0x5A, or ACK everything (H2).
- **Result**: `rootDevice = NULL`, no USB drives in the browser, HID status "Not connected" (usb_hid.cc:82). Menu input still works via the C64 matrix, REST and VT100 (doc 05).

**T1: mass-storage device + HID keyboard (HLE of the nano)**
1. T0 RAM + START latch. On START 0→1, run HLE `begin` (F1). On 1→0, go idle.
2. 8 kHz frame counter from emulated time (F3).
3. Link state machine (F2) with a "device plugged" input:
   - First 0xFFF1 about 700 ms after START.
   - Answer DO_RESET within ≤ 50 ms with STATUS = 3, SPEED = 2 (H8, H9).
   - Unplug → STATUS = 0x8000, push 0xFFF0.
4. Pipe scheduler exactly as F3: interval, memHi ≠ 0, `started`, `timeout`, ABORT_REQ, DONE bit, FIFO-full back-pressure. Run it at ≥ 1 kHz emulated (FW timeouts are ≥ 20 ms).
5. FIFO push + ITU bit-2 edge per push (H6, H15).
6. **DMA** to guest RAM at `(memHi<<16) | memLo`. The reference controller keeps 26 bits (usb_memory_ctrl.vhd:47,138), so mask with 0x03FFFFFF (Q3). Byte copies are fine; the FW needs no alignment for OUT and bounces unaligned IN buffers (usb_base.cc:834-837). `cache_load` is a no-op (DATACACHE undefined, usb_base.cc:25-39).
7. **Device layer** as a transaction interface keyed by devEP:
   - `setup(8 bytes) → ACK|STALL`
   - `in(max) → DATA(bytes)|NAK|STALL`
   - `out(bytes) → ACK|NAK|STALL`
   - Address 0 until SET_ADDRESS; honour exact byte counts (H10, H14). splitCtl can be ignored.
8. **Topology**, pick one:
   - **T1a, minimal**: a single **HS** composite root device (H9).
     - Device: class 0, bMaxPacketSize0 64.
     - Configuration: 2 interfaces, wTotalLength 57.
     - If0 MSC: class 08, subclass 06, protocol 0x50; EP 0x81 bulk 512; EP 0x02 bulk 512.
     - If1 HID: class 03, subclass 01, protocol 01; HID descriptor (0x21, report length 63); EP 0x83 interrupt 8.
     - The drive appears as "USB". The FW must not trap on the NULL read at 0 (H9).
   - **T1b, faithful**: an HS hub root (VID 0x0424, PID 0x2513, class 09, protocol 01, 3 ports, bPwrOn2PwrGood 0x32, bHubContrCurrent 1) implementing the F8 port state machine. Devices hang on its ports; FS/LS children get SplitCtl ≠ 0, which the HLE ignores. Drives appear as "USB0".."USB2". The `wait_ms` calls in hub handling need the ITU timer (doc 02 H2).
9. **MSC backend** (F6):
   - INQUIRY (36), REQUEST SENSE (18; key 0 when ready, 02/3A no media, 06/28 media changed);
   - TEST UNIT READY; READ CAPACITY(10);
   - READ(10)/WRITE(10) on an image file;
   - GET_MAX_LUN = 0 (H11); CSW status 0/1.
10. **HID backend** (F7): host key events → 8-byte boot report on the interrupt pipe. NAK when unchanged. Accept SET_IDLE/SET_PROTOCOL; GET_IDLE may echo or STALL.

## Open questions

- **Q1** Closed U64-II top: is `usb_host_nano` really at `IOBASE + 0x80000` with the same bit-11 BRAM/regs split, and does 0x10080800-0x10080FFF (or beyond) alias? The FW only touches 0x10080000-0x100807FF and 0x10080800. `g_big_endian` is presumed false (LE RISC-V, usb_base.cc:333-335).
- **Q2** Is ITU low IRQ bit 2 edge-configured on U64-II? The U2+ reference edge mask 0x85 includes it (doc 02). A level model would miss the 1-cycle pulse.
- **Q3** DMA address mapping. The reference memory controller holds 26 bits (usb_memory_ctrl.vhd:47,135-138), while the FW passes raw CPU pointers (usb_base.cc:580-581, 849-850). The U64-II DDR placement and the width of the USB master's address are unknown; assume `& 0x03FFFFFF`, identity-mapped.
- **Q4** U64-II semantics of U2PIO 0x1010000D/0x1010000F are taken from the U2+ `u2p_io.vhd`: bit0 hub reset / ULPI reset, bit7/bit6 buffer enable. Which buffer does bit7 enable on U64-II?
- **Q5** Which USB2513 downstream ports are wired to U64-II connectors, and is any on-board device attached? This affects T1b port numbering and drive names.
- **Q6** Actual CAPAB value on U64-II hardware (bit23 expected 1; reference top `g_usb_host2 := true`, ultimate_logic_32.vhd:55,311).
- **Q7** ULPI PHY identity on U64-II (the `ULPI_R_PRODUCT_LOW == 0x06` branch, nan:139-141). Only relevant for a low-level (F9) model.
- **Q8** Depth of the nano hardware return stack (`distributed_stack`, file not read). The program nests at most 3 CALLs; only relevant for F9.
