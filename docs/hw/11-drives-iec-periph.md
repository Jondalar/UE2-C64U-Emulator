# Drives (1541/71/81 + WD177x), GCR codec, IEC processor, UltiCommand (UCI), C2N tape, ACIA 6551, printer busy

Scope: U64-II RISC-V `ultimate` build (`-DRISCV -DU64=2 -DOS -DIOBASE=0x10000000`, target/u64ii/riscv/ultimate/Makefile:246).
Paths are relative to `firmware/1541ultimate/`. The U64-II top level is closed. Register semantics come from the open VHDL
(fpga/1541, fpga/io/iec_interface, fpga/io/command_interface, fpga/io/acia, fpga/io/c2n_*), checked against the C driver.
Where the U64-II could differ, the text says OPEN QUESTION.
Symbol values below come from `target/u64ii/riscv/ultimate/result/ultimate.elf` (riscv64-elf-nm).

**Bus rule used throughout:** a 16- or 32-bit CPU load/store into IO space becomes 2 or 4 sequential 8-bit IO accesses.
They run in ascending address order, lowest byte first (little endian) (fpga/cpu_unit/rvlite/vhdl_source/bus_converter.vhd:56,82-93,159-185).
FIFO-pop registers therefore pop once per byte. Shift registers shift once per byte.

## Sources read

| File | Key content |
|---|---|
| software/system/iomap.h, itu.h, itu.c | Base addresses, ITU IRQ bits, `CAPAB_*`, `getFpgaCapabilities` (ITU+0x0C..0x0F) |
| software/portable/riscv/riscv_main.c, crt0.S | Trap dispatcher (low + high IRQs), `install_high_irq`, ctor call before `main` |
| software/components/init_function.cc, application/ultimate/ultimate.cc | Init ordering (ascending `ordering`), boot order |
| target/u64ii/riscv/ultimate/linker.x | `__drive_a_area` etc. (shared drive memory in main RAM) |
| software/drive/c1541.h/.cc | `C1541` ctor, `init`, `effectuate_settings`, `set_drive_type`, `insert_disk`, `remove_disk`, `poll`, `task`, `wait_for_writeback`, MFM glue |
| software/drive/disk_image.h/.cc | `GcrImage` (buffer, G64 load, bin↔GCR via hardware codec), `BinImage` |
| software/drive/wd177x.h/.cc, mfmdisk.h/.cc, mm_drive.h | WD177x register struct, IRQ handler, command handler, DMA completion, MFM track table |
| software/io/iec/iec_interface.h/.cc, iec_code.iec, tools/parse_iec.py | IEC processor registers, microcode load, slot patching, IEC server task, master mode |
| software/io/iec/iec_drive.cc, iec_ulticopy.cc, io/printer/iec_printer.cc, mps_printer.cc | IEC slaves (SoftIEC drive, printer), UltiCopy warp, printer busy LED |
| software/io/command_interface/command_intf.h/.cc, softiec_target.cc, control_target.cc | UCI registers, ctor, ISR, server task, reply copy |
| software/io/c64/c64.cc, c64_subsys.cc, u64/u64_config.cc | UCI slot enable/base, ACIA re-init, unguarded `c1541_A` use |
| software/io/tape/tape_controller.h/.cc, tape_recorder.h/.cc, filetypes/filetype_tap.cc | C2N playback/record registers and flows |
| software/io/acia/acia.h/.cc, modem.cc | ACIA registers, init/deinit, IRQ handler, modem config |
| fpga/1541/vhdl_source/{mm_drive,drive_registers,c1541_pkg,floppy,floppy_param_mem,floppy_stream,wd177x,gcr_codec,gcr_encoder,gcr_decoder}.vhd | Drive register semantics, dirty flags, param RAM, WD177x DMA, GCR codec |
| fpga/io/iec_interface/vhdl_source/{iec_processor_io,iec_processor}.vhd | IEC register semantics, FIFOs, microcode engine |
| fpga/io/command_interface/vhdl_source/{command_protocol,command_if_pkg,command_interface}.vhd | UCI state machine, buffer constants |
| fpga/io/acia/vhdl_source/{acia6551,acia6551_pkg}.vhd | ACIA register map, ring RAM mapping |
| fpga/io/c2n_playback/vhdl_source/c2n_playback_io.vhd, fpga/io/c2n_record/vhdl_source/c2n_record.vhd | Tape register semantics |
| fpga/io/itu/vhdl_source/itu.vhd | High IRQ mask/active, edge vs level, printer busy |

## Address map

### Drive A (base `DRIVE_A_BASE` = 0x10020000, iomap.h:11). Drive B identical at +0x4000 (0x10024000, iomap.h:12)

The window is split on address bits 12:11 into regs / dirty / param / WD177x (mm_drive.vhd:142-160).
The register decode inside each part uses address(3:0), (6:0) or (10:0).

| Abs addr (A) | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10020000 | 8 | RW | POWER | bit0 drive powered (drive_registers.vhd:70-71,104-105). FW: c1541.cc:326 |
| 0x10020001 | 8 | RW | RESET | bit0 hold drive CPU in reset, bit1 also reset on C64 reset, bit2 stop when C64 frozen (drive_registers.vhd:72-75). HW reset value `drv_reset=1, bit1=1, bit2=1` (:140-149) |
| 0x10020002 | 8 | RW | HW_ADDR | bits1:0 → device 8+n (VIA jumpers) (drive_registers.vhd:76-77; c1541.cc:351-355) |
| 0x10020003 | 8 | RW | SENSOR | bit0 = write_prot_n: 1 = light/writable, 0 = dark/protected (drive_registers.vhd:177; c1541.h:82-83) |
| 0x10020004 | 8 | RW | INSERTED | bit0 disk present. Read back by FW (c1541.cc:423) |
| 0x10020005 | 8 | RW | RAMMAP | bits7:1 `bank_is_ram`. FW writes 0x80 (Extra RAM) or 0x00 (c1541.cc:189-190). mm_drive uses bit7 as `extra_ram` (mm_drive.vhd:227) |
| 0x10020006 | 8 | R | SIDE | bit0 head side (1571) (drive_registers.vhd:127-128; c1541.cc:773) |
| 0x10020007 | 8 | W | MAN_WRITE | any write restarts the 2 s write-busy timer (drive_registers.vhd:84-85,162-164; c1541.cc:857) |
| 0x10020008 | 8 | R | TRACK | bits6:0 head half-track position (drive_registers.vhd:125-126; c1541.cc:772, wd177x.cc:159,280) |
| 0x10020009 | 8 | R | STATUS | bit0 motor on, bit1 writing (`not mode`), bit2 write busy (drive_registers.vhd:129-132,156-171) |
| 0x1002000A/B | – | – | MEMMAP/AUDIOMAP | Constants exist (c1541_pkg.vhd:17-18) but are not decoded (drive_registers.vhd) and not used by FW |
| 0x1002000C | 8 | RW | DISKCHANGE | bit0 disk-change flag (→ `disk_change_n`), bit1 force_ready (drive_registers.vhd:86-88,118-120) |
| 0x1002000D | 8 | RW | DRIVETYPE | bits1:0: 0=1541, 1=1571, 2=1581. Latched/readable only when `g_multi_mode` (drive_registers.vhd:89-92,121-124) |
| 0x1002000E | 8 | W | SOUNDS | write pulses: bit0 insert sample, bit1 remove sample (drive_registers.vhd:93-95; c1541.cc:424,511,590) |
| 0x10020800–0x1002087F | 8 | RW | DIRTY | **R:** bit7 = any_dirty, bit0 = dirty_bits[off & 0x7F]. **W:** data bit7=1 clears any_dirty; data bit7=0 clears dirty_bits[off] (floppy.vhd:148-189). Index = side<<6 \| halftrack>>1 (floppy.vhd:157) |
| 0x10021000–0x100217FF | 32 (as bytes) | W | PARAM RAM | 512 LE words (pseudo_dpram_8x32). Word index = side<<8 \| halftrack<<1 \| w. w=0: track start address (26 bits). w=1: bits13:0 max_offset, bits25:16 bit_time (floppy_param_mem.vhd:35-36,68-79). Reads return 0x00 (:53) |
| 0x10021800 | 8 | RW | WD_COMMAND | **W:** bit0 index_enable, bit1 index_polarity (wd177x.vhd:225-227). **R:** command FIFO head, low 8 bits (:267-268) |
| 0x10021801 | 8 | RW | WD_TRACK | WD track register, shared with the 6502 side (wd177x.vhd:229-230,270-271) |
| 0x10021802 | 8 | R | WD_SECTOR | (wd177x.vhd:273-274) |
| 0x10021803 | 8 | R | WD_DATA | (wd177x.vhd:276-277) |
| 0x10021804 | 8 | RW | WD_STATUS_CLEAR | **W:** status &= ~data. **R:** status (wd177x.vhd:232-233,279-280) |
| 0x10021805 | 8 | RW | WD_STATUS_SET | **W:** status \|= data. **R:** status (wd177x.vhd:235-236) |
| 0x10021806 | 8 | RW | WD_IRQ_ACK | **R:** bit0 completion flag of FIFO head, bit6 reset, bit7 FIFO valid. **W:** pop FIFO (wd177x.vhd:238-239,282-285) |
| 0x10021807 | 8 | RW | WD_DMA_MODE | 0 off, 1 appl→6502 (read), 2 6502→appl (write), 3 = write complete (set by HW) (wd177x.vhd:241-242,287-288,365) |
| 0x10021808–B | 32 LE | W | WD_DMA_ADDR | bytes 8..A = 24-bit transfer address (byte B ignored) (wd177x.vhd:244-249,150) |
| 0x1002180C–D | 16 LE | RW | WD_DMA_LEN | 14 bits. Read = remaining count (wd177x.vhd:250-253,296-299) |
| 0x1002180E | 8 | RW | WD_STEPPER_TRACK | **W:** goto_track (7 bits). **R:** bit0 step_busy (wd177x.vhd:254-255,300-301) |
| 0x1002180F | 8 | RW | WD_STEP_TIME | 5 bits, ms/step (wd177x.vhd:256-257,302-303). Reset value 12 (:394) |

Drive B: regs 0x10024000.., dirty 0x10024800.., param 0x10025000.., WD177x 0x10025800...

### Shared main-RAM areas used by the drives (linker.x:268-272)

| Abs addr | Size | Use |
|---|---|---|
| 0x00EE0000–0x00EEFFFF | 64K | Drive A 6502 memory image (`memory_map`, c1541.cc:96-99). ZP $77/$78 at 0x00EE0077/78 (c1541.cc:370-371). ROM loaded at 0x00EE8000–0x00EEFFFF, 16K images mirrored to +0xC000 (c1541.cc:929-940) |
| 0x00ED0000–0x00EDFFFF | 64K | Drive B, same layout |
| 0x00EC0000–0x00ECBFFF | 48K | Drive A sound bank `snds154x/1571/1581.bin`, zeroed on load failure (c1541.cc:920-924). Offsets per mm_drive.vhd:318-331 |
| 0x00EB0000–0x00EBBFFF | 48K | Drive B sound bank |
| inside `ucHeap` 0x001594F0–0x009594EF (ELF) | – | GCR image buffer: `GCRIMAGE_MAXSIZE` = 635,932 B per drive (disk_image.h:37-41, disk_image.cc:116). Dummy track 0x1E0C B (c1541.cc:218). WD177x DMA `buffer` (wd177x.h:79). All below 16 MB, so they fit the 24-bit DMA and 26-bit track pointers |

### GCR codec (`GCR_CODER_BASE` = 0x10060500, iomap.h:28)

| Abs addr | Width | R/W | Name | Meaning (gcr_codec.vhd:23-76) |
|---|---|---|---|---|
| 0x10060500–0x1006050F | 8 | W | SHIFT_IN | Any write shifts one byte into a shared 40-bit shift register (:27-29) |
| 0x10060500–03 | 8 | R | BIN_OUT0..3 | 4 decoded bytes of the last 5 bytes written (:33-40,70-76) |
| 0x10060504–07 | 8 | R | ERRORS | bit(7-i) = nibble i invalid (:41-42) |
| 0x10060508–0C | 8 | R | GCR_OUT0..4 | 5 GCR bytes encoding the last 4 bytes written (:43-52,63-68) |

FW: encode = one 32-bit store to 0x10060508, then read +8..+C (disk_image.cc:218-237).
Decode = 5 byte stores to +0, then read +0..+3 and +4 (disk_image.cc:382-397, iec_ulticopy.cc:279-290).

### IEC processor (`IEC_BASE` = 0x10028000, iomap.h:13; iec_interface.h:32-54)

| Abs addr | Width | R/W | Name | Meaning (iec_processor_io.vhd:170-247) |
|---|---|---|---|---|
| 0x10028000 | 8 | R | VERSION | 0x25 in open VHDL (:181-182). FW only prints it (iec_interface.cc:73) |
| 0x10028001 | 8 | R | TX_FIFO_STATUS | bit0 down-FIFO empty, bit1 full OR flushed-and-not-released (:184-186) |
| 0x10028002 | 8 | R | RX_FIFO_STATUS | bit0 up-FIFO empty, bit1 full, bit7 head entry is a control code (:188-191) |
| 0x10028003 | 8 | W | RESET_ENABLE | bit0: 1 = run, 0 = hold. Any write pulses processor reset. Reset flushes up- and down-FIFO (:143,157,175,217-219) |
| 0x10028006 | 8 | R | RX_DATA | head data byte, pops (:193-194,246) |
| 0x10028007 | 8 | R | RX_CTRL | bit0 ctrl flag of head, no pop (:196-197) |
| 0x10028008–B | 8 (32) | R | RX_DATA_32 | each byte read pops. 32-bit LE read = 4 pops, first byte → LSB (:193,246) |
| 0x10028008 | 8 | W | TX_DATA | push {eoi=0, ctrl=0, byte} (:245,247) |
| 0x10028009 | 8 | W | TX_CTRL | push control byte (tag 01 → bit8) |
| 0x1002800A | 8 | W | TX_LAST | push byte with EOI (tag 10 → bit9) |
| 0x1002800C | 8 | RW | IRQ | **R:** bit0 irq_status (set by microcode `IRQ` opcode, iec_processor.vhd:174-175). **W:** clear status, irq_enable = bit0 (:199-200,220-222) |
| 0x1002800D | 8 | W | TX_FIFO_RELEASE | clears the down-FIFO flush state (:223-224; iec_processor.vhd:135-137) |
| 0x1002800E/F | 8 | R | UP_FIFO_COUNT_LO/HI | not decoded in open VHDL, reads 0. FW value unused (iec_ulticopy.cc:277) |
| 0x10028800–0x10028FFF | 8 | W | CODE RAM | 32-bit LE microcode words. FW loads `_iec_code_b_size` = 0x768 bytes (474 instr.) byte-wise into 0x10028800–0x10028F67 (iec_interface.cc:71-81) |

### UltiCommand interface (`CMD_IF_BASE` = 0x10044000, iomap.h:16; command_intf.h:10-59)

| Abs addr | Width | R/W | Name | Meaning (command_protocol.vhd:197-290, command_if_pkg.vhd) |
|---|---|---|---|---|
| 0x10044000 | 8 | RW | SLOT_BASE | bits6:1 = C64 IO offset(8:3). 0x47 → $DF18–$DF1F, 0x7F → $DFF8, 0x07 → $DE18 |
| 0x10044001 | 8 | RW | SLOT_ENABLE | **W:** bit7=0 → enable = bit0. bit7=1 → C64-visible bus-ID = bits4:0. **R:** bit0 enabled |
| 0x10044002 | 8 | RW | HANDSHAKE_OUT | **W:** bit0 = clear new-cmd + reset cmd pointer, bit1 = clear data-accepted, bit2 = clear abort, bit4 = validate (state←1\|bit5, reset reply ptrs), bit7 = state←00, unfreeze. **R:** bit7 freeze (`CMD_HS_DMA_ACTIVE`), bit6 trigger, bits5:4 state |
| 0x10044003 | 8 | R | STATUSBYTE | C64 status: bit7 resp avail, bit6 status avail, 5:4 state, bit3 error, bit2 abort, bit1 data accepted, bit0 new command (command_protocol.vhd:54-61,84-89) |
| 0x10044004 | 8 | R / W | COMMAND_START / IRQMASK_SET | **R:** 0x00. **W:** mask \|= data(2:0) |
| 0x10044005 | 8 | R / W | COMMAND_END / IRQMASK_CLEAR | **R:** 0x6F. **W:** mask &= ~data(2:0) |
| 0x10044006 | 8 | R | RESPONSE_START | 0x70 (896>>3) |
| 0x10044007 | 8 | R | RESPONSE_END | 0xDF |
| 0x10044008 | 8 | R | STATUS_START | 0xE0 (1792>>3) |
| 0x10044009 | 8 | R | STATUS_END | 0xFF |
| 0x1004400A | 8 | RW | STATUS_LENGTH | **W:** length(7:0), resets status read pointer. **R:** status read pointer(7:0). It idles at 0x700, so it reads 0x00 (command_protocol.vhd:124-128,275-276; command_if_pkg.vhd:35) |
| 0x1004400B | 8 | RW | IRQMASK | bits2:0, reset 0b111 |
| 0x1004400C/D | 8 | RW | RESPONSE_LEN_L/H | **W:** length (10:0), resets pointer. **R:** response read pointer (10:0). It idles at 0x380, so +C/+D read 0x80/0x03 (command_protocol.vhd:124-128,277-280; command_if_pkg.vhd:34) |
| 0x1004400E/F | 8 | R | COMMAND_LEN_L/H | bytes written by C64 (10:0) |
| 0x10044800–0x10044FFF | 8 | RW | RAM | command 0x10044800 (896), response 0x10044B80 (896), status 0x10044F00 (256) (command_if_pkg.vhd:33-41; command_intf.cc:50-53). Accessed directly by `memcpy` and by parsers |

### ACIA 6551 (`ACIA_BASE` = 0x1004A000, iomap.h:20; acia.h:8-36)

| Abs addr | Width | R/W | Name | Meaning (acia6551.vhd:250-324, acia6551_pkg.vhd:19-29) |
|---|---|---|---|---|
| 0x1004A000 | 8 | RW | rx_head | app → C64 ring write index (app owns) |
| 0x1004A001 | 8 | R | rx_tail | HW consume index |
| 0x1004A002 | 8 | R | tx_head | C64 → app ring write index (HW owns) |
| 0x1004A003 | 8 | RW | tx_tail | app consume index. Writing it = TX IRQ ack |
| 0x1004A004 | 8 | R | control | C64-written 6551 control |
| 0x1004A005 | 8 | R | command | C64-written 6551 command |
| 0x1004A006 | 8 | R | status | 6551 status |
| 0x1004A007 | 8 | RW | enable | bit0 visible on C64 bus, bit1 rx-irq, bit2 tx-irq, bit3 ctrl-irq, bit4 handshake-irq |
| 0x1004A008 | 8 | RW | handsh | bit0 CTS, bit1 RTS (R), bit2 DSR, bit3 DTR (R), bit4 DCD, bit5 RTS-disable, bit6 RX pushback |
| 0x1004A009 | 8 | RW | irq_source | **R:** bit1 rx room, bit2 tx data pending, bit3 control changed, bit4 DTR changed. **W:** bit3/bit4 clear |
| 0x1004A00A | 8 | W | slot_base | bit0 turbo enable, bits6:1 = C64 IO offset(8:3), bit7 NMI (not IRQ). Reads not decoded (acia.h:150) |
| 0x1004A800–0x1004A8FF | 8 | R | TX RAM | C64 → app bytes (mirror at 0x1004A900) (acia6551.vhd:189-195,439-440) |
| 0x1004AA00–0x1004AAFF | 8 | W | RX RAM | app → C64 bytes (mirror at 0x1004AB00) (acia6551.vhd:201-208,439-440) |

### C2N tape (`C2N_PLAY_BASE` = 0x100A0000, `C2N_RECORD_BASE` = 0x100C0000, iomap.h:34-35)

| Abs addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x100A0000 (0x000–0x7FF) | 8 | W | PLAYBACK_CONTROL | bit0 enable, bit1 clear error, bit2 flush FIFO, bit3 mode (0x00 byte = 24-bit escape), bit4 SENSE out, bit5 NTSC rate, bits7:6 sel (01 = drive READ line, 10 = drive WRITE line) (c2n_playback_io.vhd:74-86,221-230; tape_controller.h:18-28) |
| 0x100A0000–0x100A0FFF | 8 | R | PLAYBACK_STATUS | Every read in the window returns it, the FIFO range included (c2n_playback_io.vhd:87-89). bit0 enabled, bit1 error (underrun), bit2 FIFO full, bit3 almost full (≥1536), bits5:4 state, bit6 stream_en, bit7 FIFO empty (c2n_playback_io.vhd:207-214) |
| 0x100A0800–0x100A0FFF | 8 | W | PLAYBACK_DATA | FIFO push, 2048 deep (c2n_playback_io.vhd:180-205) |
| 0x100C0000 (0x000–0x7FF) | 8 | W | RECORD_CONTROL | bit0 enable, bit1 clear error, bit2 flush, bits5:4 edge (00 rising, 01 falling, 1x both), bit6 sel (0 READ line, 1 WRITE line), bit7 IRQ enable (c2n_record.vhd:100-115) |
| 0x100C0000 | 8 | R | RECORD_STATUS | bit0 enabled, bit1 overflow, bit2 full, bit3 almost full (≥512 = "block available"), bits5:4 state, bit6 stream_en, bit7 byte available (c2n_record.vhd:233-240) |
| 0x100C0800–0x100C0FFF | 8 (32) | R | RECORD_DATA | FIFO pop per byte. `RECORD_DATA32` = 4 pops (c2n_record.vhd:207; tape_recorder.h:20-21) |

### ITU bits owned by this block group (itu.h:11-47)

| Abs addr | R/W | Use here |
|---|---|---|
| 0x10000001 ITU_IRQ_ENABLE | W | 0x08 tape recorder (tape_recorder.cc:47), 0x10 UCI (command_intf.cc:118), 0x80 C64 reset (command_intf.cc:102) |
| 0x10000004 / 0x10000005 | W / R | dispatcher clear / active (riscv_main.c:86-87) |
| 0x10000027 ITU_IRQ_HIGH_EN | RW | read-modify-write: bit0 ACIA, bit1 drive A WD177x, bit2 drive B WD177x (riscv_main.c:38-54; acia.cc:67,75; wd177x.cc:63) |
| 0x10000028 ITU_IRQ_HIGH_ACT | R | = irq_high & imask_high (itu.vhd:228) |
| 0x10000029 ITU_PRINTER_BUSY | W | bit0 printer busy → busy LED (mps_printer.cc:1807,1839; itu.vhd:210-211,327) |
| 0x10000010 UART_DATA | W | tape ISR writes '-' (0x2D) and '+' (0x2B) debug characters (tape_recorder.cc:25,194) |
| 0x10060304 PROFILER_SUB | W | 15 on tape recorder error (tape_recorder.cc:288; profiler.h:12) |

## Init / boot sequence as seen from the bus

**Phase 0 – C++ global constructors (crt0.S:197-207, before `main`, before the scheduler and before `custom_hardware_init`).**
Order inside `__init_array` is not determined here. The blocks are independent.

1. `Acia acia(ACIA_BASE)` (acia.cc:217). Reads capabilities 0x1000000C–0x1000000F (itu.c:19-29).
   If `CAPAB_ACIA`: W 0x1004A00A=0x00, W 0x1004A007=0x00 (acia.cc:18-20).
2. `CommandInterface cmd_if` (command_intf.cc:14,39-66). If `CAPAB_COMMAND_INTF && CAPAB_CARTRIDGE`:
   - W 0x10044000=0x47, W 0x10044002=0x87 (HANDSHAKE_RESET).
   - R 0x10044006 → response_buffer = 0x10044800+8·v. R 0x10044008 → status_buffer. R 0x10044004 → command_buffer.
   - Creates queue and tasks "UCI Server" and "UCI Reset Server".
3. `SoftIECTarget softIecTarget(5)` (softiec_target.cc:9,23-37). No HW.

**Phase 1 – scheduler start.** `vPortSetupTimerInterrupt` writes ITU_IRQ_HIGH_EN=0, disables/clears all low IRQs, enables timer only (riscv_main.c:178-186).
When the UCI tasks first run:
- W 0x10044005=0x07 (IRQMASK_CLEAR), W 0x10000001=0x10 (command_intf.cc:117-118).
- W 0x10000004=0x80, W 0x10000001=0x80 (command_intf.cc:101-102).

**Phase 2 – `ultimate_main` → `InitFunction::executeAll()`, ascending order (ultimate.cc:94; init_function.cc:29-36).**

4. **(11) SoftIEC Drive** (iec_drive.cc:34-38) → `IecInterface::get_iec_interface()` → ctor (iec_interface.cc:18-40). If `CAPAB_HARDWARE_IEC`:
   - `get_patch_locations()` scans the ELF copy `_iec_code_b_start` (0x00120D1A), not HW. It finds 3 slots: talker/listener words at byte offsets 40/68, 96/124, 152/180 (verified on the ELF; iec_code.iec:81-131).
   - R 0x10028000 (printf). W 0x10028003=0x00. W 0x768 bytes → 0x10028800–0x10028F67 (iec_interface.cc:71-81). Creates task "IEC Server".
   - `IecDrive` ctor → `effectuate_settings` (iec_drive.cc:153,196-204): W 0x10044001 = 0x80\|11 = 0x8B (default bus ID 11, iec_drive.cc:28).
   - Then `configure()` (iec_interface.cc:128-145): W 0x10028003=0x00. Per slot write listener byte then talker byte:
     - slot 0: 0x10028844=0x3F, 0x10028828=0x5F
     - slot 1: 0x1002887C=0x3F, 0x10028860=0x5F
     - slot 2: 0x100288B4=0x3F, 0x10028898=0x5F
     - slot 3 (`loc = -1`): **0x100287FF=0x3F then 0x5F**
   - The drive is disabled by default (iec_drive.cc:27), so RESET_ENABLE is not set to 1.
   - `register_slave` → slot 0, then `configure()` again (iec_drive.cc:171-172).
5. **(12) Printer** (iec_printer.cc:111-118). `effectuate_settings` → `iec_if->configure()` (iec_printer.cc:142-143). `register_slave` → slot 1, `configure()` (iec_printer.cc:373-375). Printer is disabled by default (iec_printer.cc:88), so RESET_ENABLE stays 0.
6. **(13) UltiCopy** (iec_ulticopy.cc:16-22). No HW access at init.
7. **(60) Tape Playback** (tape_controller.cc:11-16). If `CAPAB_C2N_STREAMER`: `stop()`: W 0x100A0000=0x06, W 0x100A0000=0x00 (tape_controller.cc:111-115). Task "TapePlayer" (PRIO_REALTIME) reads 0x100A0000 every 250 ticks (5 if bit0) (tape_controller.cc:59-72).
8. **(61) Tape Recorder** (tape_recorder.cc:16-21). If `CAPAB_C2N_RECORDER`: `stop(OK)`: W 0x100C0000=0x00, then W 0x100C0000=0x06 (tape_recorder.cc:138-158). Task "TapeRecorder". W 0x10000001=0x08 (tape_recorder.cc:45-48).
9. **(65) C1541/71/81 Init** (c1541.cc:1257-1275). For A if `CAPAB_DRIVE_1541_1`, for B if `CAPAB_DRIVE_1541_2`:
   - ctor: W 0x1002000D=0x02, **R 0x1002000D** → `multi_mode = ((v&3)==2)` (c1541.cc:119-123). `new WD177x(regs+0x1800, regs, 1+(letter-'A'))` (c1541.cc:115).
   - `init()` (c1541.cc:210-235):
     - W param RAM 0x10021000–0x100217FF: 256 × {dummy_track ptr, 0x028A0100}. bit_time = (100 000 000/20)/0x1E0C = 650 (c1541.cc:214-225).
     - W 0x10020003=0x01 (SENSOR light), W 0x10020004=0x00.
     - `WD177x::init` (wd177x.cc:50-64): W 0x10021800=0x03. **Loop: while (R 0x10021806 & 0x80) W 0x10021806=0x01.** Then `install_high_irq(1)` → R/W 0x10000027 \|= 0x02 (B: irq 2, \|= 0x04).
   - `effectuate_settings` (c1541.cc:180-207):
     - W 0x10020005=0x00. W 0x10020002=bus_id&3 (A 0, B 1). RAM 0x00EE0078=0x48, 0x00EE0077=0x28 (c1541.cc:351-373).
     - `set_drive_type(cfg, default 0)` (c1541.cc:906-948): W 0x1002000D=0. Load sound bank → 0x00EC0000 (0xC000 B), zero on failure. Load ROM file → 0x00EE8000 (0x8000 B). On failure the drive is powered off (c1541.cc:202-204).
     - `drive_reset(1)`: W 0x10020001=0x01, `wait_ms(1)`, W 0x10020001=0x06 (defaults: stop-on-freeze=1 → 4, reset-with-C64=1 → 2) (c1541.cc:337-349).
     - **R 0x10020000** ≠ cfg? Drive A defaults to powered (c1541.cc:90) → `drive_power(true)`: W 0x10020000=0x01, `drive_reset(1)` again, W 0x10020800=0x80 (c1541.cc:324-330). Drive B defaults off → no write.
   - Task "Drive A": R 0x10020000 bit0. If on: `xQueueReceive(50)`, `poll()` → R 0x10020800, 0x10020008, 0x10020006 (c1541.cc:738-818).
10. **(105) Modem** (modem.cc:923-928). If `CAPAB_ACIA`: `Modem` ctor + tasks, no HW (modem.cc:93-123). When settings are effectuated with the default "ACIA mapping = off" (modem.cc:49,69; `acia_base[0]=0`): `acia.deinit()` → 0x10000027 &= ~1 twice, W 0x1004A007=0x00 (acia.cc:72-84; modem.cc:867-870).
11. After init functions, if `CAPAB_CARTRIDGE` → `C64::init` (ultimate.cc:100-104) → `set_emulation_flags`:
    - W 0x10044001=0x00, later = `CFG_CMD_ENABLE` (default 0, c64.cc:115). W 0x10044000=0x47 (c64.cc:311-313,328-331).
    - Cart flags may set UCI base/enable (c64.cc:1305-1321). `modem->reinit_acia(0xFFFF)` (c64.cc:1324-1331; modem.cc:895-910).
    - U64 unlock IRQ also sets 0x10044001=1, 0x10044000=0x47 (u64_config.cc:1012-1019).

## Boot hazards

| # | Where | What happens with 0 / 0xFF | Required emulator response |
|---|---|---|---|
| H1 | itu.c:19-29 read at acia.cc:10, command_intf.cc:44 (ctor time), iec_interface.cc:20, c1541.cc:1260-1264, tape_*.cc:13/18, modem.cc:924 | Every block is gated on capability bits, and the gate is evaluated in crt0 ctors before `main`. **`CAPAB_HARDWARE_IEC` (0x20) missing:** `IecInterface` ctor returns early, leaving `slaves[]`, `available_slots` and `talker/listener_loc` uninitialised (`operator new` does not zero, memory_wrap.cc:44-60). `IecDrive`/`IecPrinter` still call `register_slave`/`configure` (iec_drive.cc:171-172; iec_printer.cc:142-143,373-375), which gives wild writes/calls → crash. **`CAPAB_DRIVE_1541_1` (0x02) missing:** `c1541_A` is NULL and dereferenced unguarded in socket_dma.cc:204 and c64_subsys.cc:415 | ITU 0x1000000C–0x1000000F valid from reset, with at least 0x02\|0x04\|0x20\|0x80\|0x100\|0x200\|0x800\|0x00040000\|0x08000000 set (DRIVE_1541_1/2, HARDWARE_IEC, C2N_STREAMER/RECORDER, CARTRIDGE, MM_DRIVE, COMMAND_INTF, ACIA). Also any other bits the product needs (see ITU doc) |
| H2 | wd177x.cc:58-61 (init function 65, before UI creation) | `while (R 0x10021806 & 0x80) W …=1`. **0xFF → infinite loop, boot hangs** (same for 0x10025806) | T0: return bit7=0 (0x00). T1: bit7 = command FIFO non-empty; a write pops one entry |
| H3 | c1541.cc:119-123 | `multi_mode` needs a readback of the value 2 written to 0x1002000D. 0x00 or 0xFF (&3=3) → 1541-only: 1571/1581 modes and ROM entries disabled (c1541.cc:121-136,909-911) | Latch bits1:0 and read back |
| H4 | c1541.cc:397-403, 646-650, 779-781; floppy.vhd:165-180 | DIRTY 0x10020800 must NOT be plain RAM. After `drive_power()` writes 0x80 (c1541.cc:329), RAM would read 0x80 → `check_if_save_needed` = true → "About to remove a changed disk. Save?" on every mount/remove, `SSRET_DISK_MODIFIED` for REST/UCI mounts (c1541.cc:998-1007,1076-1085). **`wait_for_writeback` (disk swap) never exits** | T0: reads 0x00. T1: bit7 any_dirty (write data bit7=1 clears), bit0 per-track bit (write data bit7=0 clears) |
| H5 | c1541.cc:199,282,742,334 | POWER bit0 must read back. Stuck 0 → drive task never polls, no write-back, UI stuck on "Turn On". Not a hang | Latch |
| H6 | command_intf.cc:50-53 (ctor time) | Buffer location reads. 0 → command/response/status all alias 0x10044800 → UCI replies overwrite the command, silent corruption. 0xFF → buffers at 0x10044FF8 → `copy_result` writes up to 896 B beyond the 2K RAM window | R 0x10044004=0x00, 0x10044006=0x70, 0x10044008=0xE0 (and 0x10044005=0x6F, 0x10044007=0xDF, 0x10044009=0xFF). Writes to +4/+5 are IRQ-mask ops, not stores |
| H7 | command_intf.cc:85-96,117-118; command_protocol.vhd:310 | ITU 0x10 is enabled at UCI task start. The line is level = (STATUSBYTE[2:0] & ~IRQMASK)≠0. Garbage in 0x10044003 low bits, or a mask that does not gate the line → IRQ storm; queue floods ('\\' on UART) | Idle: 0x10044003 = 0x00, line low. The mask must gate the line |
| H8 | tape_recorder.cc:23-29,47,183-187; c2n_record.vhd:125 | ITU 0x08 is enabled at init 61. Source = control bit7 & FIFO≥512. A spurious level → ISR storm, each ISR writes 0x2D to the UART | Record IRQ low unless enabled + data |
| H9 | wd177x.cc:106-118; wd177x.vhd:422; riscv_main.c:118-129 | High IRQ 1/2 is level = WD command FIFO valid. A stuck asserted line → endless ISR, queue overflow | 0x10000028 bits1/2 = 0 at idle. T1: deassert when FIFO empties via 0x10021806 writes |
| H10 | iec_interface.cc:121-126,141 | Microcode has 3 patch slots, but the loop runs `MAX_SLOTS`=4 → `dst[-1]` = **write to 0x100287FF** (0x3F, 0x5F) on every `configure()` | Accept and ignore. No bus fault (VHDL: bit11=0, offset 0xF = no-op, iec_processor_io.vhd:215-228) |
| H11 | iec_interface.cc:182-189 (every 2 ticks) | 0x10028002 = 0x00 → task reads 0x10028006 500 times each loop (CPU burn, bogus bytes routed to slaves). 0xFF → 500 bogus ctrl codes | Idle 0x10028002 = 0x01 |
| H12 | iec_interface.cc:285-286 (jiffy), 410-411, 419-420 (master open), iec_ulticopy.cc:316-317 | Unbounded loops on TX full / not empty / RX empty. Jiffy path busy-spins without delay | Idle 0x10028001 = 0x01 (empty, not full). Only reached after IEC activity or user action |
| H13 | wd177x.cc:66-73 | `wait_head_settle` polls 0x1002180E, up to 500 ticks per sector command. RAM model (returns goto_track) → 0.5 s per 1581 sector | Read bit0 step_busy, 0 when idle |
| H14 | softiec_target.cc:366,390,402,432,450,461; command_intf.cc:208-211 | `is_dma_active` = R 0x10044002 bit7. A RAM model returns last write 0x87 → "DMA active" → changes the SoftIEC load/save path | R 0x10044002 = freeze\|trigger\|state (idle 0x00) |
| H15 | tape_controller.cc:66-70 | PLAYBACK_STATUS bit0 = 1 → 5-tick polling at PRIO_REALTIME (no hang). bit7 never 1 → `poll` state 2 never ends playback (tape_controller.cc:219-227) | Idle 0x100A0000 = 0x80 |
| H16 | tape_recorder.cc:250-262 | `flush()` loops while RECORD_STATUS bit7, reading 0x100C0800. Stuck 1 → hang (only after a capture was started) | bit7 = FIFO non-empty, cleared by pops |
| H17 | disk_image.cc:218-237, 382-397; c1541.cc:599,801-804; disk_image.cc:1238 | GCR codec is not used during boot. Returning 0 → a mounted D64 becomes all-zero GCR (no sync), and **D64 write-back decodes zeros into the user's .d64 (data loss)** | Implement gcr_codec.vhd exactly before enabling writable D64 mounts |
| H18 | riscv_main.c:43,50; acia.cc:75 | 0x10000027 is read-modify-write. Must read back the mask. 0xFF readback → all high IRQs enabled. The dispatcher disables unhandled ones only if HIGH_ACT is correct (riscv_main.c:124-127) | Latch (ITU doc) |
| H19 | c1541.cc:921-943 | Missing `1541.rom`/sound files on the flash FS → `SSRET_NO_DRIVE_ROM` → drive A powered off (not a hang) | Provide ROMs in the flash FS image (flash doc) |

## Interrupts

| Source | ITU line | Raise | Ack / deassert |
|---|---|---|---|
| Tape recorder | low bit 0x08 (`ITU_INTERRUPT_TAPE`) | `irq_en(ctrl bit7) & FIFO≥512` (c2n_record.vhd:125) | ISR `tape_recorder_irq` → reads 128×RECORD_DATA32 = 512 B (tape_recorder.cc:183-208). If not recording: W 0x100C0000=0 |
| UCI | low bit 0x10 (`ITU_INTERRUPT_CMDIF`) | `(handshake_in & ~irq_mask)≠0` (command_protocol.vhd:310) | ISR: R 0x10044003, R 0x1004400B, W 0x10044004 = flags&~mask (masks them). Task clears mask bit after handling: W 0x10044005 (command_intf.cc:85-96,143,153,183) |
| C64 reset | low bit 0x80 (`ITU_INTERRUPT_RESET`) | closed top (C64 reset) | ISR `ResetInterruptHandlerCmdIf`: sets pending, posts `CMD_ABORT_DATA` to UCI queue + semaphore → `Sampler::reset`. Also `ResetInterruptHandlerU64` (riscv_main.c:91-95; command_intf.cc:29-36; u64_config.cc:2114-2118) |
| ACIA | high 0 | `(rx room & rx_en) \| (tx pending & tx_en) \| (ctrl change & ctrl_en) \| (dtr change & hs_en)` (acia6551.vhd:147-150). FW never sets rx_en (acia.cc:57-66) | ISR (acia.cc:141-188): ctrl → W 0x1004A009 = 0x08. handshake → W 0x10 there. TX → copy ring, W 0x1004A003 = tx_head |
| Drive A WD177x | high 1 (`ITU_IRQHIGH_1581`) | command FIFO valid (wd177x.vhd:422) | ISR: R 0x10021800 (cmd), R 0x10021806 (bit0 → `WD_CMD_DMA_DONE`), queue, W 0x10021806=1 (pop) (wd177x.cc:106-118) |
| Drive B WD177x | high 2 | same at 0x10025800 | same |
| IEC processor | none | `irq` output exists (iec_processor_io.vhd:233) but is not in the FW dispatcher | polled: R 0x1002800C, cleared by W 0x1002800C=0 (iec_ulticopy.cc:96,261,310,322) |

Dispatcher: R 0x10000005, W 0x10000004 (same value), call handlers, then R 0x10000028 and call high handlers (riscv_main.c:82-134).
Low lines 0x08/0x10/0x80 are level-sensitive with the default `g_edge_init="00000001"` (itu.vhd:16,275). FW never writes `ITU_IRQ_EDGE`.

## Functional model

### Drive (1541/1571 GCR path) – what the external 1541 emulator must share

- **6502 memory:** RAM/ROM image in main RAM at 0x00EE0000 (A) / 0x00ED0000 (B).
  - FW loads the ROM at +0x8000..+0xFFFF (c1541.cc:929-940) and pokes ZP $77/$78 (c1541.cc:370-371).
  - FW reads $78 for the effective IEC address while powered (c1541.cc:386-387).
  - Extra RAM = RAMMAP bit7 (mm_drive.vhd:227).
- **Control inputs from FW registers:**
  - POWER; RESET (bit0 hold, bit1 follow C64 reset, bit2 stop while C64 frozen).
  - HW_ADDR → VIA1 PB5/PB6 device jumpers (device 8+n).
  - SENSOR → write-protect photo sensor. INSERTED. DISKCHANGE bit0 → disk_change_n, bit1 force_ready.
  - DRIVETYPE. SOUNDS pulses.
- **Outputs to FW:**
  - TRACK = half-track 0..127.
  - SIDE.
  - STATUS: bit0 motor, bit1 writing (VIA mode=0), bit2 write-busy = 1 from start of write until 2047 ms after the last write (or MAN_WRITE pulse) (drive_registers.vhd:156-171).
- **Disk surface:**
  - For head position (side, halftrack) take param words at index `side<<8 | halftrack<<1`: word0 = 26-bit main-RAM address of the track's first GCR byte, word1[13:0] = track_len−1 (wrap point).
  - Use word1[25:16] bit_time (floppy_param_mem.vhd:68-79). FW sets bit_time = 5 000 000/len (c1541.cc:215-216,461,470) = 2× bit-cell in 10 ns clock ticks (floppy_stream.vhd:50,92-101). The whole track spans 200 ms (300 RPM) regardless of speed zone.
  - The emulator may equivalently use cell = 200 ms/(8·len).
  - Read bytes from main RAM [addr + offset].
  - On write (mode=0 & motor & inserted): store into the same RAM, set `dirty_bits[side<<6 | halftrack>>1]` and `any_dirty` (floppy.vhd:156-163).
- **Track programming by FW:**
  - `insert_disk` writes side-0 entries i=0..83 from 0x10021000 and side-1 entries i=84..167 from 0x10021400. Missing tracks point at the zero dummy track (c1541.cc:456-514).
  - G64: `track_address = gcr_data + offset + 2`, `track_length = w & 0x3FFF`, bit15 = MFM track (disk_image.cc:716-722).
  - D64: converted with the HW encoder (c1541.cc:597-603).
  - Remove: all 512 words reset to dummy (c1541.cc:441-448).
  - Sequence around inserts: SENSOR dark → wait → params → SENSOR light (if writable) → INSERTED=1, DISKCHANGE=1, SOUNDS=1 (c1541.cc:456-514).
  - Mount commands first write RESET=0 and end with `drive_reset(0)` → RESET=0 then 0x06 (c1541.cc:572,605,610,624).
- **Write-back** (`poll`, c1541.cc:757-818):
  - If DIRTY bit7 (or skipped writes pending): W DIRTY=0x80.
  - For each track index i (side 1 mapped to 0x80+), if dirty bit0 and (not WRITEBUSY or head elsewhere): W DIRTY[tr]=0, then write the GCR track to .g64 or decode it to the .d64 via the HW decoder (disk_image.cc:1238).
  - Otherwise count as skipped and retry next poll (50 ticks).
- **Speed:** FW needs no timing beyond write-busy. `wait_for_writeback` relies on HW clearing write-busy (c1541.cc:641-651).

### WD177x / 1581 (MFM) path

- **FW state:** MFM track table `MfmDisk` (mfmdisk.cc:11-77). The D81 file is mounted directly (c1541.cc:579-594), or MFM tracks live inside the GCR image through a RAM file (c1541.cc:516-568).
- **Read sector / Read address:**
  1. 6502 writes a command → HW sets busy and pushes {completion=0, cmd} → IRQ.
  2. The FW task handles it (c1541.cc:746-750; wd177x.cc:192-382): W 0x10021804=0x10 (clear RNF).
  3. `wait_head_settle` (R 0x1002180E).
  4. Look up using R 0x10021802 (sector), 0x10021801 (track), drive SIDE and TRACK. Read the file into `buffer`.
  5. W DMA_LEN, DMA_ADDR(=&buffer), DMA_MODE=1 → HW feeds bytes to the 6502 data register and clears busy when len hits 0 (wd177x.vhd:310-350).
  6. Sector not found: W 0x10021805=0x10, W 0x10021804=0x01.
  7. Read Address supplies 6 bytes {T, side, S, size, CRC16-CCITT(0xB230)} (wd177x.cc:338-361).
- **Write sector / Write track:**
  1. FW sets DMA_MODE=2 (buffer prefilled 0x77) and HW asserts DRQ.
  2. At the end HW sets mode=3 and pushes {completion=1} → IRQ → ISR `cmd | 0x100`.
  3. `handle_wd177x_completion` writes the file (sector) or decodes 6250 raw bytes (track) and sets DMA_MODE=0. HW clears busy 255 ticks of 4 MHz later (wd177x.vhd:352-378; wd177x.cc:384-454).
  4. If the disk is a GCR image, the callback sets MAN_WRITE and the MFM dirty bits (c1541.cc:849-884).
- **Type I (restore/seek/step)** (wd177x.cc:207-270):
  - W 0x10021801 (track), W 0x1002180F (step time), W 0x1002180E (target track).
  - Spin-up: W 0x10021805=0x20. Clear busy: W 0x10021804=0x01.
  - Drive DISKCHANGE register = 0.
- **Abort (0xD):** DMA_MODE=0, clear busy.
- **HW reset values:** track 1, sector 0, step_time 12 (wd177x.vhd:384-398).

### GCR codec

- One 40-bit shift register shared by encode and decode, written by any byte write in 0x10060500–0F (gcr_codec.vhd:18,27-29). HW reset value 0x5555555555.
- **Encode:** after 4 byte writes (one LE 32-bit store: bin[0] first), GCR_OUT0..4 = 10 standard GCR nibbles (MSB first) of shift_reg[8..39].
- **Decode:** after 5 byte writes, BIN_OUT0..3 = 8 nibbles of shift_reg[0..39]. ERRORS bit(7−i) = nibble i not a valid GCR code.
- FW decodes UltiCopy data via one LE 32-bit store + 1 byte store (iec_ulticopy.cc:279-290).

### IEC processor

- **FIFO protocol, FW side** (iec_interface.cc:165-336).
  - Up-FIFO entries {ctrl flag, byte}:

    | Code | Meaning |
    |---|---|
    | 0x41 | ATN begin |
    | 0x42 | ATN end |
    | 0x43 | ready to TX |
    | 0x46 | JiffyDOS load |
    | 0x45 | EOI |
    | 0x47 | byte transmitted |
    | 0x81 / 0x82 / 0x83 | device slot 0 / 1 / 2 addressed |
    | 0x5A / 0x5B / 0xE5 / 0xE6 / 0xE9, 0x57 / 0xAD / 0xAE / 0xDA / 0xDE | defined constants (iec_interface.h:56-84) |

  - Data bytes under ATN ≥0x60 → secondary address to the slave. Data bytes without ATN → `push_data`.
  - On 0x43/0x46: `talk()`, W 0x1002800D, then push data (0x10028008) / last (0x1002800A) while TX not full.
  - Error → W 0x10028009=0x10 (TX_TERM).
- **Master mode** (iec_interface.cc:399-542), TX_CTRL codes:

  | Code | Meaning |
  |---|---|
  | 0x1D | GO_MASTER |
  | 0x1C | ATN_TO_TX |
  | 0x1B | ATN_RELEASE |
  | 0x1A | ATN_TO_RX |
  | 0x57 | GO_WARP |

  Used for UltiCopy and `run_drive_code` (M-W/M-E).
- **Addressing:** device numbers live in the patched microcode bytes (listener = dev\|0x20, talker = dev\|0x40, 0x1F = unused → 0x3F/0x5F) at CODE RAM offsets 0x44/0x28 (slot 0), 0x7C/0x60 (slot 1), 0xB4/0x98 (slot 2).
- **HLE option:** keep CODE RAM as RAM, learn addresses from those bytes, speak the byte-level IEC protocol on the external C64 bus, and generate the up-FIFO codes above.
- **LLE option:** run the 474-instruction microcode with an iec_processor.vhd-exact engine (opcodes iec_processor.vhd:46-62,160-240; encoding tools/parse_iec.py:162-167).
- **Default config:** processor held in reset (RESET_ENABLE=0) because neither SoftIEC drive nor printer is enabled.

### UCI (command interface)

- **C64 side** (slot base 0x47): $DF1B bus-ID (R), $DF1C control/status, $DF1D command write (reads 0xC9, or 0x49 when IRQ), $DF1E response data (R), $DF1F status data (R) (command_protocol.vhd:97-121,141-190).
- **States:** 00 idle, 01 processing, 11 data-more, 10 data-last.
- **Command:**
  1. C64 writes bytes to $DF1D (command pointer++), then $DF1C bit0 → state 01, handshake_in bit0 → FW IRQ.
  2. FW: length = R 0x1004400E/F, parse the RAM at 0x10044800 in place, W 0x10044002=0x01.
  3. If no reply: W 0x10044002=0x87.
  4. Else `copy_result`: memcpy response → 0x10044B80, status → 0x10044F00, W 0x1004400D/0x1004400C (resp len), W 0x1004400A (status len), W 0x10044002 = 0x10 (last) / 0x30 (more) (command_intf.cc:156-206).
- **More data:** C64 writes $DF1C bit1 → if state 11, handshake_in bit1 → FW `get_more_data` → copy_result → W 0x10044002=0x02.
- **Abort:** C64 $DF1C bit2 → FW `abort(R resp pointer − 0x380)` → W 0x10044002=0x87.
- **C64 reset:** any C64 reset posts an abort (command_intf.cc:29-36,136-144).
- **Bus ID:** 0x10044001 written with bit7 = SoftIEC bus ID (iec_drive.cc:199).
- **Enable:** `CFG_CMD_ENABLE` or cart requirements (c64.cc:328-331,1305-1321).

### ACIA 6551 (modem)

- C64-visible when enable bit0=1 at base $DE00/$DF00/$DF80 (`acia_base[]`, modem.cc:49). slot_base = (base−$DE00)>>2 \| 0x80 if NMI (acia.cc:42-48).
- 5 C64 registers +0 data, +1 status (write = soft reset), +2 command, +3 control, +7 turbo (only with turbo) (acia6551.vhd:119-140).
- **TX (C64→app):** HW writes 0x1004A800[tx_head++] paced by baud and CTS. FW ISR copies tx_tail..tx_head, then W tx_tail (acia.cc:169-180).
- **RX (app→C64):** FW writes 0x1004AA00[rx_head…] and W rx_head (acia.cc:86-98,198-207). HW moves one byte per baud tick into rx_data and sets the C64 IRQ unless disabled (acia6551.vhd:201-209,326-337).
- **Handshake:** FW sets CTS/DSR/DCD via 0x1004A008 (acia.cc:100-139; modem.cc:380-450).
- **HW reset/C64 reset values:** command 0x02, rings 0, nmi_selected=1, rx_pushback=1 (acia6551.vhd:363-393).

### C2N tape

- **Playback start** (tape_controller.cc:126-161):
  1. W ctrl 0x06, W ctrl 0x00.
  2. Push preamble 0x00,0x95,0x95,0x25 (24-bit pause).
  3. Preload ≤16×512 B while not almost full.
  4. W ctrl = 0x01 \| mode<<3 \| sel<<6 \| (0x10 if READ) \| (0x20 if NTSC).
- **Playback steady state:** refill 512-byte blocks whenever almost-full is clear (tape_controller.cc:198-234).
- **Playback end:** push 123, wait for FIFO empty, stop.
- **HW pulse generation:** byte n → count n×8 tape ticks, pulse active for the first half. With mode=1, byte 0 → next 3 bytes 24-bit. Tick only while motor on and C64 running (c2n_playback_io.vhd:92-166). Sense output = ctrl bit4.
- **Record start:** W ctrl 0x06, W 0x10, then 0x01\|0x10\|0x80 (read line, falling) or 0x01\|0x40\|0x80 (write line, rising) (tape_recorder.cc:160-181).
- **Record HW:** counts phi2 ticks between edges while `sense & enabled`, emits TAP bytes (>2040 → 0x00 + 3 bytes), IRQ at ≥512 (c2n_record.vhd:79-192).
- **Record FW:** caches 512-byte blocks and writes the .tap. `flush` drains the remaining bytes and patches the length at file offset 16 (tape_recorder.cc:234-273).
- **External C64 emulator must supply:** cassette motor, sense, read and write lines.

### Printer

- IEC slave (slot 1, default address 4, disabled by default) (iec_printer.cc:88-89,373-375). Output via MPS emulation to a file.
- HW visible only as ITU_PRINTER_BUSY (0x10000029) bit0 → busy LED (mps_printer.cc:1803-1841).

## Emulator model tiers

| Block | T0 (boots, present but idle) | T1 (functional) |
|---|---|---|
| Drive regs A/B | Latch 0,1,2,3,4,5, C(2 bits), D(2 bits). Read 6/8/9/A/B/7/E = 0x00 | Drive C64-side 1541/1571 state from the latches; update TRACK/SIDE/STATUS from the external drive; write-busy 2 s timer; sound triggers optional |
| Dirty 0x..800 | **Reads 0x00**, writes ignored (H4) | bit7/bit0 semantics of floppy.vhd:148-189, set by external drive writes |
| Param RAM 0x..1000 | Accept writes into a 512-word table. Reads 0x00 | Export the table to the external drive (track pointer, wrap, bit time). Head reads/writes main RAM at those addresses |
| WD177x 0x..1800 | 0x..806 reads 0x00 (H2). 0x..80E reads 0x00. Latch 1/7/C/D/F; status register with set/clear ops. High IRQ 1/2 low | Full wd177x.vhd: command FIFO (depth 7) + IRQ, DMA state machine reading/writing main RAM, stepper busy, index pulse |
| Drive memory 0x00EB0000–0x00EEFFFF | Plain main RAM (must exist) | Shared with the external 1541 CPU (RAM, ROM, ZP $77/$78) |
| GCR codec 0x10060500 | Implement (pure logic, a few lines). Returning 0 does not block boot | Required for D64 mount/write-back and UltiCopy (H17) |
| IEC proc 0x10028000 | R0=0x25, R1=0x01, R2=0x01, others 0. CODE RAM writable. Ignore 0x100287FF (H10–H12) | HLE IEC slave on the external bus using patched address bytes, or LLE microcode engine. FIFOs with tags, flush/release, RESET_ENABLE flush, IRQ status |
| UCI 0x10044000 | Constant START/END reads (H6). HANDSHAKE_OUT/STATUSBYTE read 0x00. Idle read pointers: 0x1004400A = 0x00 (status 0x700), 0x1004400C/D = 0x80/0x03 (response 0x380). IRQMASK reset 0x07 with set/clear. 2K RAM. IRQ low | command_protocol.vhd state machine bridged to the external C64 ($DF1C–$DF1F, slot base/enable, bus-ID, freeze/trigger via `write_ff00`) |
| ACIA 0x1004A000 | Latch regs. irq_source 0x00. 1K ring RAM. High IRQ 0 low | acia6551.vhd incl. baud pacing, ring RAM, C64 IRQ/NMI to the external emulator |
| C2N play 0x100A0000 | Status 0x80 for every read in the window. Control/FIFO writes accepted | FIFO + pulse generator, sense/motor/read/write lines to the external C64 cassette port |
| C2N record 0x100C0000 | Status 0x00, data 0x00, IRQ low | Edge timer + TAP encoder + FIFO + IRQ (≥512) |
| ITU bits | Capability bits (H1). HIGH_EN latch. PRINTER_BUSY latch | Route 0x08/0x10/0x80 low and high 0/1/2 as levels |

## Open questions

1. What capability word does the closed U64-II top level actually return? Are both drives multi-mode (`g_multi_mode`, DRIVETYPE readback)? Is `CAPAB_DRIVE_SOUND`/`CAPAB_HARDWARE_GCR` set?
2. The GCR codec map is inferred as gcr_codec.vhd (shared shift register, ERRORS at +4, encoder at +8). gcr_decoder.vhd would return decoded bytes at +4. The FW's use of +4 as errors (disk_image.h:19) matches gcr_codec only.
3. Do ITU lines 0x08/0x10/0x80 run level-sensitive on U64-II (open default `g_edge_init="00000001"`, itu.vhd:16)?
4. Does the drive window mirror above offset 0x1FFF inside the 0x4000 slot? Does the regs part mirror every 16 bytes in 0x000–0x7FF?
5. RAMMAP bits 6:1 in the U64-II drive CPU: which banks do they map? FW only writes 0x80/0x00.
6. Does the U64-II implement IEC UP_FIFO_COUNT (0x1002800E/F) and which IEC processor version does it have? The open VHDL gives 0 and 0x25; FW never branches on either.
7. What source and timing does the C64-reset ITU line 0x80 have in the U64-II top level? That matters when the C64 is an external emulator.
8. How is the tape tick generated (`tape_speed_control`, motor gating) and how does phi2 feed the recorder when the C64 is external?
9. Firmware quirk, not emulated: `insert_disk` clears DIRTY indices `i/2` (42..83) for side-1 tracks instead of 64..127 (c1541.cc:498). Side-1 dirty bits are only cleared through `poll`.
