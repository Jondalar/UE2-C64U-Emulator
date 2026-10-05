# S40 — The board's engine in UE2: frame ring and Timewarp for the firmware

**Status:** spec (2026-10-05), local branch `s37-trx-pixel-layer`. Updated the same day from the FPGA session's
answers (`run/trx/s40-answers.md`, from RTL and app code), the TRX64-Ultimate architecture reset
(`docs/architecture-reset-2026-10-05.md` there) and Alex's decisions of 2026-10-05 (§0). Phases A-C are M1 (§13) and
are built (as built: §10).

**Why:** Alex decided (2026-10-05) that UE2 behaves like the TRX64-Ultimate board for the firmware, so its UI and UX
can be tested without the board. On the board the app's frame ring, Timewarp (freeze menu and REST) and trxmon's
ring verbs run on the FPGA engine: CAPS b2, page 2, `trx_engine.v`. UE2 presents that register interface and writes
the streams exactly in the app's format, fed from the embedded TRX64 core (`trx64-core`, pinned v0.12.5). The app is
unmodified.

S38 (TRX64's own rings for UE2's monitor) stays as it is. S40 adds the board's engine next to it.

**Reads** (code and RTL are the truth; comments are not evidence):
- RTL, TRX64-Ultimate `main` (f88a95b): `designs/c64_basic/trx_engine.v` (all of it), `trx_sys.v`, `trx_io.v`,
  `c64_core.v`, `c64_mem.v`, `top.v`; `cores/icepi/cpu6510.v`, `cores/icepi/6502n/proc_{core,registers,control,
  interrupt}.vhd`, `cores/icepi/c64_redip_cia.v`, `cores/icepi/c64_redip_cia/cia_core.sv` and
  `reDIP-CIA/gateware/CIA/cia_*.sv` (the flip-flop images), `cores/icepi/c64_cartridge.v` (`reg_0`/`reg_1`),
  `cores/icepi/vicii-kawari/hdl/registers_scaled.v` (`$D02F-$D03F`). Line numbers are of `main`.
- App, `software/ultimate-app` branch `trx64/main` (a58f1a6e, the ELF `run/trx/ultimate-a58f1a6e.elf`):
  `software/io/c64/{trx_ring.cc, trx_ring.h, trx_timewarp_ui.cc, trx_io.h, trx_ui.cc, c64.cc}`,
  `software/api/route_timewarp.cc`.
- The FPGA session's answers: `run/trx/s40-answers.md` (they override the draft where they differ).
- UE2: S37, S38, S39; `crates/ue2-core/src/devices/{trx.rs, trx_session.rs, c64.rs}`, `crates/c64-bridge/src/{lib.rs,
  ring.rs, hwlog.rs, cart.rs}`. TRX64: `full_sc.rs`, `full.rs`, `c64re_snapshot.rs`, `ciacore.rs`.

**Owns:**
- `crates/ue2-core/src/devices/trx_engine.rs` (new): page 2, the record former, the engine store, the mover, APPLY,
  the boundary model (arming, snaps, captures, `CAP_GEN`).
- `crates/ue2-core/src/devices/trx_sb.rs` (new, shared with S39): the state bus words, the shadows, the staged words.
- `crates/ue2-core/src/devices/trx.rs`: CAPS, CAPS2; page 0 `CAP_CTRL`, `CAP_STAT`, `RESUME_PHASE`, `STOP_POS`; page 1;
  IRQ causes b1, b2, b3.
- `crates/ue2-core/src/devices/c64.rs`: the position hold, the ordered register writes, moving the bridge's tap events
  into the engine.
- `crates/c64-bridge/src/engine_tap.rs` (new): the taps on TRX64, the RAM and colour RAM mirror, the boundaries, the
  CPU-held run to a position, the side table, the restore.
- `crates/c64-bridge/src/trx_ff.rs` (new): the 6502N and reDIP-CIA flip-flop images (encode).

## 0. M1 scope and the decisions of 2026-10-05

- **M1 Timewarp** (architecture reset §5.3): RAM, CPU, VIC registers, CIAs, cartridge banking. UE2 builds what the
  RTL and the app do for these.
- **REU:** "REU on = no ring, no Timewarp" is the app's M1 work and not in `trx64/main` (answer 8). UE2 models what
  is built: the REU's DMA into C64 RAM is an ordinary stream 0 record, F1 b6 marks a frame in which the REU moved
  data, and there is no snap while a REU transfer waits (§4.2). When the app gates the ring on the REU, UE2 needs
  nothing new.
- **No `.c64re` / `.c64rering` dump or load in M1** (the app's code goes). §8 (files) and the decoder of images UE2
  did not make (§5.1, §5.2 "Decode") move to M2/M3 with phase D. The encoder stays: the block's words are what the app
  reads and stages back.
- **Turbo with `--trx-io`: off**, as on the board's `main` (no T29 RTL). `$D030`/`$D031` read `$FF` and take no
  writes (Kawari's default branch, `registers_scaled.v:1063-1067`: no `WITH_EXTENSIONS`), the firmware's turbo
  strobe (`C64_SPEED_UPDATE`) has no effect, the CPU always runs at 1 MHz. Without `--trx-io` nothing changes.
- **Stream 0 `old`** is the RAM block's own old byte (`c64_mem.v:91-105`), also under ROM; a store to `$D000-$DFFF`
  with I/O visible makes no record (answer (a)).

## 1. What the app does with the engine

`trx_ring.cc` is the only user of page 2 on `trx64/main`; `route_timewarp.cc`, `trx_timewarp_ui.cc`, `trx_c64re.cc`
and the `trx->ring` table go through it. What it does, in the order UE2 must serve it:

| Step | App code | Registers |
|---|---|---|
| Start (6 s after boot, and after a load) | `start_locked` 1672 | `LOG_CTRL` 0; `LOG_BASE`/`LOG_MASK` × 3 (the reset layout); `CAP_CTRL` 1; `LOG_CTRL` `0x10F` (restart, on, every tap) |
| Poll, every 100 ms | `trx_ring_poll` 1770, `parse_locked` 248 | `LOG_CNT` 0, 1, 2; COPY of stream 2 into a bounce buffer; parse F1-F4, B1, words, B2; `CAP_CTRL` back to 1 |
| Read any area | `fetch` 194, `eng_copy` 171 | `ENG_SRC`, `ENG_DST`, `ENG_LEN`, `ENG_CMD` 1, poll `ENG_CMD` b0 |
| Stop the machine | `stop_machine` 465 | `STOP_POS` arm cycle 12, poll b16; `SH_CTRL` 0; `GATE_CTRL` 1; `C64_STOP`; one DMA read (`$0002`) with the log on; poll `LOG_STAT` b1/b3 |
| Put a frame back | `put_back` 873 | Ultimax bus writes: cartridge banking, colour RAM, VIC, CIA registers; `GATE_CTRL` 0 (CPU-only pause); `STOP_POS` arm + release at the block's cycle; DMA writes to `$D019`, reads of `$D01E/F` while the VIC runs; poll b16; state bus reads of the VIC latches; RAM through APPLY; `SB_DATA` staging of chip 1 w8-14, chip 3 w8-19, chip 4 w8-19; `SB_LOAD` `$1A`; release |
| Undo/redo stream 0 | `apply_run` 766 (CAPS2 b5) | `AP_START`, `AP_COUNT`, `AP_CMD`, poll `AP_CMD` b0 |
| Undo stream 1 | `walk_range` 1088 | colour RAM as bus writes, cartridge RAM straight into DDR `0x0EF0000 + a`, then page 4 `CMD` b1 |
| Release | `restore_locked` 1040-1051 | `CAP_CTRL` 1, `LOG_CTRL` `0x00F`, `STOP_POS` b9: the load edge |
| Scrub step | `scrub_show_locked` 1315 | as put back, then one frame with the log on: `STOP_POS` arm + release at cycle 4, poll; `STILL_STATUS` |
| Land (trxmon) | `land_locked` 1445 | as scrub, then `CYC_CMP` = `CYCLES`, `OWNER`, `STOP_POS` release, poll `STATUS` b3 (S39, page 3) |
| Materialize | `materialize_locked` 1589 | a pause, RAM by MEMONLY, colour RAM through the session (M1: only the dump uses it) |

The app reads `LOG_STAT`, `DROPPED` (status) and never `FRAME`. It never writes `CAP_TAB_*`, `LOG_PUT_*` or
`ENG_ARG`, never uses FILL, and never enables IRQ causes b1/b2 (no handler on `trx64/main`). UE2 builds them anyway,
from the RTL.

The Timewarp UI ("C64 Machine" → "Timewarp") appears only with CAPS b2 (`trx_timewarp_ui.cc:409`). REST
`/v1/timewarp:{status, frame, start, stop, rewind, scrub, scrubend}` answer 503 without it (`route_timewarp.cc`
`refuse_absent`); `dump`, `ringdump`, `load` are M2/M3 (§0).

## 2. CAPS and CAPS2

Each bit is reported only when its pages are built.

| Bit | Board `main` | UE2 before | Now | Turns on |
|---|---|---|---|---|
| CAPS b1 state bus | 1 | 0 | **1** (S39 phase A1) | the splash's READY watch by captured PC (`trx_ui.cc:219`); the REST lens (`trx_lens_fw.cc:100`) |
| CAPS b2 engine | 1 | 0 | **1** | `trx_ring_init` (`trx_ring.cc:1994`), the ring task, Timewarp UI, REST |
| CAPS b3 debug | 1 | 0 | **1** (S39 phase A2, built) | `app_trx_table()`, `rebase_cycles`, `dbg_handover` |
| CAPS2 b0 F4 order | 1 | 0 | **1** | the parser does not look for F4s after block records (`trx_ring.cc:1997`) |
| CAPS2 b5 APPLY | 1 | 0 | **1** | `apply_hw`: stream 0 is undone by the engine (`trx_ring.cc:1998`) |
| CAPS2 b2, b4 | 1 | 0 | **1** (S39 B/C, built) | trxmon's CPU write path, the lens unit |
| CAPS2 b6 turbo (t29) | 0 on `main` | 0 | 0 | not on `main` (answer 1) |
| CAPS2 b7 trace (t21-p4) | 0 on `main` | 0 | 0 | phase E |
| CAPS2 b1 BUILD | 1 | 0 | 0 | read by the boot ROM only (answer 10) |

**CAPS = `$E7`** (b0, b1, b2, b5, b6, b7), **CAPS2 = `$29`** (b0, b3, b5). With S39 A2 CAPS becomes `$EF`; with S39 B/C
CAPS2 `$3D`; with the trace `$BD` (answer 10: valid builds for both apps; b1, b6 not read by either).

## 3. Page 2 registers

32-bit, `trx_sys.v` byte rule (bytes 0-2 collect, byte 3 commits, byte-0 read latches; one buffer and one latch for
all far pages). `r_addr` is the word index; offsets below are bytes. Write-only registers read 0
(`trx_engine.v:525-538`); `LOG_CTRL` reads 0 as built (answer 6; the header's "RW" is stale).

| Off | Name | RTL | UE2 |
|---|---|---|---|
| 000 | `LOG_CTRL` W | :463-471: b0 on, b3:1 taps (b1 outside, b2 cartridge RAM, b3 colour RAM), b8 restart: `FRAME`, `DROPPED`, gap, ext, stream counts to 0 (not the cycle counter) | as RTL |
| 004 | `LOG_STAT` R | :528 `{kmiss b4, FIFO b3, mover b2, block b1, on b0}` | b0 = on; b1-b4 0: UE2 forms, copies and drains in the same step (§4.6) |
| 008 | `FRAME` R | :529, :370 markers since restart (also dropped ones) | as RTL |
| 00C | `DROPPED` R | :530 b15:0 | 0 (UE2 never drops) |
| 020/030/040 | `LOG_BASE[s]` W | :472-477 bits 26:4 | as RTL |
| 024/034/044 | `LOG_MASK[s]` W | the same | as RTL |
| 028/038/048 | `LOG_CNT[s]` R | :531-533 `{4'd0, count 28}` records written since restart | as RTL |
| 050 | `LOG_PUT_LO` W | :478 | as RTL |
| 054 | `LOG_PUT_HI` W | :332-343: with on, puts `{HI, LO}` into stream 1 | as RTL, never held |
| 060 | `CAP_TAB_IDX` W | :479 b5:0 | as RTL |
| 064 | `CAP_TAB_DATA` W | :480-483 `{count b22:16, first word b11:0}`, then IDX + 1; refused while a block is copied | as RTL, never refused |
| 068 | `CAP_TAB_LEN` W | :484 b6:0, clamped to 64; reset 9 | as RTL |
| 080 | `ENG_CMD` W/R | :485-488 1 COPY, 2 FILL, refused while busy or APPLY runs; R b0 busy | §6.1; busy reads 0 |
| 084, 088, 08C | `ENG_SRC`, `ENG_DST`, `ENG_LEN` W | :489-491 bits 26:4 (16-byte units) | as RTL |
| 090 | `ENG_ARG` W | :492 the FILL word | as RTL |
| 094, 098 | `AP_START`, `AP_COUNT` W | :493-494 28 bits | as RTL |
| 09C | `AP_CMD` W/R | :518-522 b0 1 undo / 0 redo, starts unless a job runs or count is 0; R b0 busy | §6.2: busy until the C64 side has applied it |

**The table** (`CAP_TAB_*`, `tab[]` :184-197): 64 entries `{count 7, state-bus address 12}`, reset
glue 1 / 6510 3 / VIC 12 / CIA 1 5 / CIA 2 5 / cartridge 1 / 6510 w8+ 7 / CIA 1 w8+ 12 / CIA 2 w8+ 12 = 58 words.
UE2 walks it as the sequencer does (:393-452) over the state bus model (S39 §1.2, `trx_sb.rs`).

**IRQ causes** (`trx_io.v`): b1 when a COPY, FILL or APPLY job ends (`eng_tgl`, :218, :556); b2 at the first lost
record after a marker (`gap_tgl`, :309), never in UE2; b3 at every capture and every halt (`cap_tgl`, c64_core.v:675).

**Ordering.** Page 0 (from 0x104), page 1 (but `SB_ADDR`, `SB_DATA`, `SB_STATUS`) and page 2 writes (but the
mover's registers and `ENG_CMD`) take effect on the C64 side at the access's clock: UE2 queues them with the clock
and the C64 port applies them in order as it runs the C64 up (as `GATE_CTRL` since S37). A queued write makes the port
tick before the firmware's next instruction, so a read after a write sees its effect, as the block's own sequencing
has it (the app's `STOP_POS` release then poll of b16 depends on it). Reads answer from the state as of the last C64
sync.

## 4. The streams and how UE2 makes them

### 4.1 Where the records go

- **The engine store.** The areas sit at `0x5C00000-0x7FFFFFF` (reset layout :198-204; t21-p4 adds stream 3 at
  `0x6800000`). UE2's guest DDR is 64 MB (`bus::RAM_SIZE`); the app cannot address above it either and reads only
  through COPY (`trx_ring.cc:31-33`). UE2 keeps DDR addresses `0x4000000-0x7FFFFFF` as an engine-only store,
  allocated in 1 MB chunks on first write; an unwritten chunk reads 0. Below 64 MB the engine writes guest DDR.
- **Placement**, as the drain (:621-624): record n of stream s at `{base | (off[26:4] & mask), off[3:0]}`,
  `off` = n × 4 (stream 0) or n × 8. Bitwise OR, as built, not an add.
- **Counts** `LOG_CNT[s]` = records written, mod 2^28; stream 2 counts its F4 too.

### 4.2 Stream 2: the frame boundary

**The snap point** (`c64_core.v:627-674`). Armed at line 16 of every frame while `LOG_CTRL` b0 or `CAP_CTRL` ≠ 0.
The snap is the first phi2 fall on line 0 from cycle 12 (x ≥ 96) that ends an opcode fetch, with: machine running
and not halted, border flip-flops set, no sprite pixel on its way, no VIC access in the ending cycle, the REU idle
(:643-644). No snap by line 16: the boundary is at line 16 without a capture (`mk_snap` 0). A `STOP_POS` stop
disarms it (:662). A frame of the `CAP_CTRL` cadence captures at its snap (`snap_cap`, :645): `CAP_GEN` + 1, the
shadows load, IRQ cause b3.

**In UE2** the snap is the first TRX64 instruction boundary on line 0 at raster cycle ≥ 12 where the conditions
hold. Of the conditions TRX64 exposes the REU (a transfer pending at the boundary, `Reu::dma_pending`); the VIC's are
treated as met (gap T3). The machine state at a TRX64 boundary is the state before the fetch; the board's snap is the
edge after it, so F2 counts the fetch. The CPU-held pause and the reset hold give no boundary, so the marker comes at
line 16.

**Markers** (:351-371). One marker per boundary, three records then the F4 the drain adds (:730-737):

| Rec | Bits | UE2 |
|---|---|---|
| F1 | `{F1, flags 8, CAP_GEN 16, frame 32}` | flags below; `CAP_GEN` after this snap's capture; frame = `FRAME` before the increment |
| F2 | `{F2, 32'd0, cycles 24}` | the engine counter's low 24 bits after the snap edge (`cycles + phi2_tick`, :360): UE2's count at the boundary + 1 |
| F3 | `{F3, 7'd0, line 9, 6'd0, x 10, 24'd0}` | line 0 (16 at a line-16 boundary); x = 8 × cycle + 7, cycle = TRX64's `raster_cycle` at the boundary (answer 4: RTL cycle c = TRX64 `raster_cycle` c) |
| F4 | `{F4, stream 1 count 28, stream 0 count 28}` | the counts after every record of the frame that ends here |

F1 flags (:359): b0 snap; b1 a block follows (`mk_cap`); b2 records lost since the last marker (never); b3 outside
writes in the frame (§4.3); b4 the snap is a fetch an interrupt drops (`mk_intg`: TRX64's next `execute` takes an
interrupt, UE2's own check, gap T4); b5 capture skipped (never); b6 the REU moved data in the frame.

**Order** (:48-50, :255-258, :285): every stream 0/1 record made at or before the boundary is counted in its F4,
every later one after. In UE2: records of instructions that ended at or before the snap boundary, then the marker,
then the rest.

**The engine counter** (`cycles`, :211, :292) counts phi2 edges from power-on and is not reset by a C64 reset or a
restart (answer 11). UE2 counts the cycles TRX64 runs (CPU running or held); the board's DMA cycles while halted
stand no TRX64 cycle in UE2 and are not counted.

### 4.3 Streams 0 and 1

| Stream / space | RTL source | Record | UE2 source |
|---|---|---|---|
| 0, machine | `c64_mem` port A RAM writes (`c64_mem.v:91-105`), whoever drives the bus: CPU, REU, a DMA bus cycle | `{new 8, old 8, addr 16}` LE, old = the RAM array's own byte | every TRX64 write that lands in the 64 KB RAM, with the RAM's old byte from UE2's mirror: CPU stores and RMW dummy writes, `$00`/`$01` (the RAM byte, answer 7), the firmware's window writes without MEMONLY, the REU's DMA (found by the mirror compare) |
| 0, outside (`LOG_CTRL` b1) | port B: JTAG, the freeze responder's commit, the app's MEMONLY writes (`top.v:1299`, `!ap_gnt`) | same; marker flag b3 | firmware MEMONLY writes, the S37 session's RAM writes, **S38 transport restores** (§9) |
| 1 space 1, colour RAM (b3) | port A (`cram_tap`), :318 | `{0, cycle 15, 0, new 4, 0, old 4, 1, 18'd0, addr 10}` | TRX64 writes into colour RAM (CPU with I/O visible, DMA bus cycles), old nibble from the mirror |
| 1 space 1 outside (b1 & b3) | port D, :320 | same, b63 = 1 | the session's commit to `$D800-$DBFF`; S38 transport restores |
| 1 space 2, cartridge RAM (b2) | `cart_slot` RAM writes, :329; old byte from DDR (`D_K`, :708-720) | `{0, cycle 15, new 8, old 8, 2, 13'd0, addr 15}` | the cartridge logic's writes into its RAM window `0xEF0000-0xEFFFFF` (cart.rs `RAM_BASE`, the board's address too), offset in [15:0], old byte from DDR (never a miss) |
| 1 space 3, flash | `LOG_PUT` only | | `LOG_PUT` |
| 1 space 8, debug log | `trx_dbg.v:235` | | with S39 phase B |

- **The cycle field** is the engine counter's raw low 15 bits at the write (:302, :329), not relative to F2
  (answer 5; the header's "minus F2's" is stale). No app reads it.
- **Not logged**: APPLY's writes (`ap_gnt` masked, `top.v:1299`), anything while `LOG_CTRL` b0 is 0, writes the
  firmware's CPU makes straight into DDR, the REU's own memory, flash and EEPROM contents, GeoRAM, the SID, drives.
- **Taps as the RTL**: stream 0 machine writes need only b0; outside ones b0 and b1; colour RAM b0 and b3; outside
  colour b0, b1 and b3 (:295-296); cartridge RAM b0 and b2 (:321).
- **Where a write lands** (TRX64's `FullBus::write`, `full.rs:1087-1150`, with the memory configuration of the
  instruction): `$00`/`$01` and `$0002-$CFFF` land in RAM but the Ultimax holes; `$D000-$DFFF` lands in RAM with I/O
  hidden, else colour RAM at `$D800-$DBFF` and no record elsewhere; a window a cartridge may take (`$8000` ROML,
  `$A000` ROMH, `$E000` Ultimax ROMH) is decided after the instruction by the RAM itself (gap T1).

### 4.4 Stream 2: the chip block

After a marker with F1 b1 (:394-452):

| Rec | Bits |
|---|---|
| B1 | `{B1, 0, table entries 7, 8'd0, CAP_GEN at the mark 16, frame 24}` |
| words | the table's state-bus words in order, two per record, word 2i in bits 31:0; an odd last word padded with 0 (`Q_PAD`) |
| B2 | `{B2, 7'd0, torn, words 16, CAP_GEN now 16, CAP_GEN at the mark 16}` |

UE2 reads the words from the shadows loaded at that snap in the same step: never torn, never skipped, `CAP_GEN`
equal at both ends (answer 12: fine for both apps).

### 4.5 Stream 3: the trace (t21-p4, phase E)

Unchanged from the draft: one record per instruction into `0x6800000`; `LOG_CNT[3]` (byte 058); CAPS2 b7. Not M1.

### 4.6 Timing and the drain

- UE2 forms each record when TRX64 makes it and writes it into the store as the C64 port runs the C64 up; no FIFO,
  no drop, `LOG_STAT` b1/b3 read 0, the app's `drain()` returns at once.
- The areas are circular. Lapping is the app's business (`reachable`, `trim`, "stream 2 lapped").

## 5. The chip block's words from TRX64

The block is the state bus (S39 §1.2): visible words from TRX64 as S39 §3.2 says, the hidden ones from §5.1/§5.2.

| Words (table order) | Contents (`c64_core.v:584-612`, :754-763) | From TRX64 |
|---|---|---|
| 0 glue `0.0` | `{CAP_GEN, 0, held, cart present, EXROM, GAME, halted}` | S39 §3.2 |
| 1-3 `1.0-1.2` | `{S,Y,X,A}`, `{sync, P, PC}`, `{$01 read, $01 latch, $00}` | at a snap PC = the address of the opcode just fetched, sync 1 (answer (b)): TRX64's `reg_pc` at the boundary |
| 4-15 `2.0-2.11` | VIC `$D000-$D02F` as read, no side effect | `io_peek` |
| 16-20, 21-25 `3.0-3.4`, `4.0-4.4` | CIA registers as read, `{DDRB, DDRA, PRB, PRA}` (`c64_top.v:652`: the port registers) | `io_peek`, `cia_regs` |
| 26 `7.0` | `{reg_1, reg_0}` of icepi's cartridge model (`c64_cartridge.v:171-211`: the bytes written to the type's registers, masked) | for the types the app replays (`replay_cart` 664): EasyFlash `{$DE02, $DE00}`, MegaByter the same, Ocean / Magic Desk / Magic Desk 16 / Ocean 16K `$DE00`; Normal and the rest 0 |
| 27-33 `1.8-1.14` | the 6502N's 209 flip-flops | §5.1 |
| 34-45, 46-57 `3.8-3.19`, `4.8-4.19` | each CIA's 363 flip-flops | §5.2 |

### 5.1 The 6502N image (209 bits)

Layout (`cpu6510.v:126-177`, `proc_core.vhd`, `proc_registers.vhd:347-368, :472`, `proc_control.vhd:552-587`,
`proc_interrupt.vhd:60`): A 7:0, X 15:8, Y 23:16, S 31:24, P 39:32, PC 55:40, adl 63:56, adh 71:64, dreg 79:72, IR
87:80, h 95:88, ready_d1 96, adh_clash 97, so_d 98, pc_carry_i 99, pc_carry_d 100, index_carry 101, control 123:102
(state 4:0, a_mux 6:5, dout_mux 9:7, rwn 10, latch_dreg 11, copy_d2p 12, dummy 13, irq_inhibit 14, flags_update 15,
trigger_upd 16, vectoring 17, intg 18, a16 19, vect_reg 21:20), interrupt 129:124 (irq_c, nmi_c, nmi_d1, nmi_d2,
nmi_act, resetting), din_r 137:130, pin_r 143:138, pdir 151:144, pout 159:152, pfloat 167:160, fall6 186:168, fall7
205:187, rdy 206, irq 207, nmi 208.

The board's file (fixture, 9 blocks): PC = the fetched opcode's address, din_r = that opcode, IR = the previous
opcode. A/X/Y/P are pre write-back (answer (b)): the settled values exist only in the live words 1.4/1.5.

**Encode** (TRX64 → image), at a snap:
- A, X, Y, S, P: TRX64's registers (settled); `copy_d2p`, `flags_update`, `trigger_upd` 0: nothing left to write
  back. The fixture's `$8D` template has `trigger_upd` = 1, which would apply a write-back a second time (answer 2),
  so UE2 clears it.
- PC = `reg_pc`; din_r = the byte there through the PLA; IR = the previous opcode (TRX64's last opcode).
- control: the fixture's after-fetch values with `trigger_upd` 0 (`ctl` = `11 1 0 0 0 0 0 0 0 1 1 000 11 00000`,
  answer 2); intg and vectoring = "an interrupt is decided at this fetch" (F1 b4).
- interrupt bits: irq_c = the IRQ line asserted; nmi_d2/nmi_act = an NMI edge pending (TRX64's `IntStatus`).
- pdir, pout = `$00`, `$01` latch; pin_r = the port's input pins; pfloat, fall6, fall7 0 (gap T7); rdy 1, irq/nmi
  the lines as TRX64 drives them.
- adl, adh, h, dreg (loaded only with `SB_FULL`): the previous operand bytes.

### 5.2 The reDIP-CIA image (363 bits)

Layout (`cia_core.sv:140-352`, `c64_redip_cia.v:108-115`, `cia_*.sv`): [1:0] `{res, phi2_prev}`, [4:2] CNT edge,
[7:5] /FLAG edge, [42:8] ports `{prb_rw 3, PRA, PRB, DDRA, DDRB}` (DDRB 15:8, DDRA 23:16, PRB 31:24, PRA 39:32),
timer A [82:43] (prescaler = latch 58:43, counter 74:59, then hi_w_prev, start_prev, reload_prev, count_prev, intr,
intr_up, toggle_prev, os_loaded_prev 82:75), timer B [122:83] the same, TOD [243:123] (alarm 154:123, clock 186:155
as `{10ths, sec, min, hr}` with hr in the low byte, h12 187, then the latch states, `clatch` 223:192, the divider
and the TOD edge), serial [286:244] (SDR 251:244), interrupt [316:287] (sources_prev, ir_set_prev, icr_prev,
icr7_prev, irq_prev, flags_prev 307:303, ir_clr_prev, ir_set_phi2, ir_clr, mask 315:311, w_mask_prev 316), control A
[326:317] (CRA, cr_w_prev, cnt_prev), control B [336:327], the TOD tick divider [362:337].

**Encode** from TRX64's `CiaSnapshot` (`capture_cia`): registers, latches, counters, mask, flags and IR, TOD clock,
latch and alarm, SDR; the timer pipeline bits for the steady states of answer 3 (stopped: all 0 but
`os_loaded_prev`; running and phi2-counted: `start_prev`, `count_prev` 1; `os_loaded_prev` always 1); `hi_w_prev`,
`cr_w_prev`, `w_mask_prev` 0 (answer 3: always 0 at a snap). What the encoder cannot know from TRX64 (`toggle_prev`,
"just underflowed", the serial shifter, the TOD divider phase) is 0.

### 5.3 The side table: exact restores in UE2

The images are lossy against TRX64's state (VICE's alarm clocks, the CPU's cycle-level state). So UE2 keeps, for
each block it made, the exact TRX64 parts: the CPU (`CpuSnapshot` and the core's last-opcode state), `IntStatus`,
both `CiaSnapshot`s, the port, the cycle. The keys are hashes of the staged words, one per chip (chip 1's 7, each
CIA's 12), so `SB_LOAD` b1, b3, b4 each find their part. At `SB_LOAD` staged words equal to a kept image load the
kept parts, their clocks moved to now (gap T5). The table holds 4096 entries per chip (the app's `TL_CAP`), oldest
dropped first. A staged image that is no UE2 block (a board file, a hand-edited block) needs the decoder: M2/M3;
in M1 it loads nothing and UE2 says so on stderr.

Each kept CPU part also holds a hash of RAM and colour RAM at the snap. A load checks the memories the app put back
(APPLY, the colour RAM bus writes) against it and says on stderr: `SB_LOAD: the 6510 and the CIAs of the snap N cycles
back; RAM and colour RAM equal to the snap's` (or `DIFFER`). The firmware smoke counts on it.

## 6. The mover, APPLY and the restore path

### 6.1 COPY and FILL

`ENG_CMD` 1: `ENG_LEN` bytes `ENG_SRC` → `ENG_DST`; 2: `ENG_ARG` repeated over `ENG_LEN` bytes at `ENG_DST`
(:677-761). Addresses and length in 16-byte units, 27 bits. UE2 runs the job inside the commit: below `0x4000000`
guest DDR, above it the engine store; an unwritten chunk reads 0. Busy never reads 1; cause b1 is raised. A command
while APPLY runs is refused, as :485.

### 6.2 APPLY

`AP_CMD` (:499-523, ui_clk :654-700): `AP_COUNT` stream 0 records from index `AP_START`; b0 1: downwards, the old
byte; 0: upwards, the new byte; each record at `LOG_BASE[0] | ((n × 4) & LOG_MASK[0])`. Written through the RAM's
second port, not logged. UE2: the C64 port applies the job at the write's clock, each byte into TRX64's RAM (and the
mirror), in index order, last write wins; `AP_CMD` b0 reads 1 until then. Count 0 does nothing; refused while a
mover job runs (never in UE2); cause b1 at the end.

### 6.3 What the restore needs beyond page 2

| Need | RTL | UE2 |
|---|---|---|
| `STOP_POS` (page 0 0x114) | `trx_sys.v:326-331`, R :220 `{hold b16, arm b8, cycle}`; `c64_core.v:638-639` `pos_hit` on line 0 one edge before the phi2 fall ending cycle `pos_cyc`; the gate shut until b9 | With the CPU held (CPU-only pause, reset): the chips run cycle by cycle and stop at line 0, `raster_cycle` = the cycle: exact. With the CPU running: the first TRX64 boundary on line 0 at or after the cycle, or the first after it when an instruction steps over it (gap T2). The hold stands the whole machine like the pause (S37 `set_paused`); `GATE_CTRL` b8/b9 read 1, `IRQ_CAUSE` b0 and b3 are raised, the shadows follow, the mute holds (`top.v:1059`) |
| `RESUME_PHASE` (0x110) | :325, R :219 pending | accepted, reads 0. Not used by `trx64/main` |
| CPU-only pause | `GATE_CTRL` b0 = 0 with `C64_STOP`: RDY low, the VIC and CIAs run | `Hold::Cpu` (lib.rs `apply_hold`), there since S14 |
| Bus writes in Ultimax, DMA reads while the VIC runs | `C64_MODE`, the DMA window | S14/S28/S37 paths |
| Staging chips 1, 3, 4 w8+ | `trx_sys.v:317-323`, `c64_core.v:691-695`; refused unless the gate is shut | S39 §3.4's model for all three chips; refused under the same rule (`SB_STATUS` b1) |
| `SB_LOAD` b1, b3, b4 | `trx_sys.v:324`; `c64_core.v:385-386`: the staged words load at the first opening edge | at the next point the C64 moves (a release, or a DMA bus cycle while halted): the side table (§5.3), before TRX64 runs. `SB_LOAD` 0 disarms |
| The load edge | `STOP_POS` b9 release: the snap edge itself, CPU post-fetch | TRX64 runs from the boundary; its fetch is that edge |
| VIC write-only registers, internals, latches | not restored; the app clears the latches by DMA while the VIC runs | the same: the VIC is not touched by a load |

## 7. Turbo, pause, reset, REU

**Turbo:** off with `--trx-io` (§0). The engine always records.

**Pause** (as the RTL: nothing runs, nothing is recorded):

| State | Markers | Records |
|---|---|---|
| Machine pause (FRZ), position hold, S38 transport hold | none | outside writes only (MEMONLY, session commit), with b1 |
| CPU-only pause (`C64_STOP`, FRZ 0), reset held | at line 16, no snap, no block | DMA bus cycle writes; colour RAM by bus cycles |
| Running | at the snap | all |

A C64 reset does not stop or restart the engine (no such code on the board, S38 §7).

**REU:** §0.

## 8. Files (M2/M3)

`timewarp:dump`, `timewarp:ringdump`, `timewarp:load`, "Save Timewarp Ring", S38 `ringdump … board` for UE2's own
anchors and loading board files through the decoder are M2/M3 (§0). The draft's §8 stands for then: the board's
`.c64re`/`.c64rering` carry `trx64Hw` with the 58-word block (`trx_c64re.cc:495-511`), and the board app's own
writer has a bug to fix first (answer (b): it writes A/X/Y/S/P from w0/w1, one write-back stale).

## 9. With S37, S38, S39

- **S37 session**: the app ends it (`SH_CTRL` 0) before a restore. Its RAM writes and its commit to colour RAM are
  outside records.
- **S38 rings** run as before, beside the engine. A transport restore (`goto`, `frame`, `ringload`) changes RAM and
  colour RAM behind the firmware: UE2 logs every changed byte as an outside record (stream 0 port B, stream 1 b63),
  as JTAG writes are logged on the board. An engine restore (SB_LOAD) is a C64 that changes under S38, as a firmware
  load is: S38 keeps recording.
- **S39**: one state-bus model (`trx_sb.rs`). The snap is S39's capture point; the block's words are S39's shadows;
  `SB_LOAD` b1 adds S39 §3.4's CPU write path with phase B.
- **UE2's monitor**: `monitor trx` also shows the state bus and page 2: on, taps, counts, frames, the areas, the
  position hold.

## 10. In the code (as built)

- **c64-bridge** (`engine_tap.rs`): TRX64's run is cut at the engine's points (`run_tapped`: the snap window from line
  0 cycle 12, line 16, the stop position; one instruction at a time on line 0 while a REU transfer waits). While the
  log is on every run goes through an observer wrapper (`Tapped`): the writes in order, a halt after a `$00/$01` write
  or a write into a cartridge's window, the run then going on to its end; the writes are resolved after it under the
  memory configuration they were made in (§4.3), with old bytes from the RAM and colour RAM mirror. A compare of the
  mirror at each boundary and at the end of each slice logs what no tap saw (the REU, a monitor poke). The firmware's
  window writes, the session's commit and S38's restores are tapped where the bridge serves them. The side table and
  the load (§5.3). Events go to the port in order.
- **c64-bridge** (`hwlog.rs`): the write log's engine sink: the cartridge logic's and the REU's guest DDR writes.
- **ue2-core** (`c64.rs`): the queued register writes at their clocks, the bridge's events into the state bus model
  and the engine after each advance and window access, APPLY through the RAM's second port, the halt (pause or
  position hold), `SB_LOAD` at the next opening edge, the engine's records below 64 MB into guest DDR.
- **ue2-core** (`trx_engine.rs`, `trx_sb.rs`): registers, store, placement, markers, blocks, mover, APPLY; the state
  bus words, the capture cadence and arming, staging.
- **trx_ff.rs**: the encoder; its layout is checked on the board's fixture.

## 11. TRX64 gaps

| # | Gap | Until it closes |
|---|---|---|
| T1 | **A memory write tap**: every write that lands in the 64 KB RAM or colour RAM, with the old byte of that memory and the master (CPU, RMW dummy, REU DMA). `on_bus` gives the CPU-visible old byte (ROM under BASIC/KERNAL, 0 from `$D000`, `full_sc.rs:260-301`) and the REU writes RAM without it. (The same CPU-view `old` makes TRX64's own delta-ring undo write ROM bytes into RAM under ROM: answer (a).) | UE2's mirror: old from the mirror, landing by TRX64's memory configuration (§4.3), a halt after a `$00/$01` write; a compare after the run finds the writes no tap saw (the REU) and logs them before the marker |
| T1' | TRX64 v0.12.5 (released 2026-10-05) records the RAM's own overwritten byte in its delta ring and trace, and leaves out stores a flash cart consumes: the board's stream 0 `old`. UE2 is pinned to v0.12.3 | UE2 keeps its own mirror as the source of stream 0 until the pin moves; after the bump stream 0 can be fed from the delta ring |
| T2 | **Cycle-exact stop with the CPU running** (S39 T1) | `STOP_POS` with a running CPU lands at the next boundary |
| T3 | **Snap conditions** as reads: VIC border flip-flops, sprite pixel pending, VIC access in the cycle | treated as met; the REU is checked |
| T4 | **"The next `execute` takes an interrupt"** (S39 T2) | F1 b4 and the image's intg from UE2's own check |
| T5 | **Per-chip restore at a later clock**: `restore_cia`, `restore_cpu`, `restore_int_status` take absolute clocks | UE2 moves every clock field of the kept snapshots by (now − snap) itself |
| T6 | **The 6502N and reDIP images** are not TRX64's (by design) | UE2's encoder §5 |
| T7 | **The CPU port's fall-off state** (`data_set_clk_bit6/7`) | 0 |

## 12. The FPGA session's answers (2026-10-05, `run/trx/s40-answers.md`)

1. Turbo: `main` has no turbo RTL, CAPS2 `$3F` there → turbo off with `--trx-io` (§0).
2. No bench loads a synthesized image; settled registers with `trigger_upd`/`flags_update`/`copy_d2p` 0 are
   architecturally a no-op write-back; din_r must be the opcode as read; intg/vectoring must match the interrupt
   decision. Unproven on the board (a tb_ring variant would prove it; M2/M3 with files).
3. Timer pipeline bits: steady states as §5.2; `toggle_prev` and "just underflowed" are not derivable.
4. RTL cycle c = TRX64 `raster_cycle` c (VICE c + 1); not proven against the bus phase.
5. Stream 1 cycle field: raw `cycles[14:0]`; no app reads it.
6. `LOG_CTRL` reads 0, intended by the code; nobody reads it.
7. `$00/$01` stores reach port A; old = the RAM byte, not the port's.
8. REU: no app gating yet; the RTL logs REU DMA as stream 0, F1 b6, no snap while busy (§0).
9. Stream 3 b1/b2 for 1-byte opcodes: masked by the app; any value.
10. CAPS2 `$3D`/`$BD` valid for both apps; b1, b6 unread (§2).
11. Engine `cycles` and debug `CYCLES` count the same edges from FPGA configuration; low 24 bits equal.
12. Torn/skipped blocks are only fewer targets for both apps.

Bugs the answers name, outside UE2 (for the TRX64 and FPGA sessions): TRX64's delta ring takes the CPU-view `old`
(T1); `trx_engine.v:23-24` ("minus F2's") and `:53` ("RW") are stale comments; the app's `.c64re` writer takes
pre-write-back registers (`trx_c64re.cc:469-477`).

## 13. Phases

| Phase | Content | M |
|---|---|---|
| A | Page 2 registers, the store, placement, counts, restart, the table, the mover, APPLY, causes b1/b2/b3; CAPS b2, CAPS2 b0/b5 | M1 |
| B | The taps (T1 mirror), the snap and line-16 boundaries, markers F1-F4, blocks from the shadows with §5's encoder, the side table; pause rules; REU as built; S38 transport restores as outside writes | M1 |
| C | The restore path: `STOP_POS` (CPU held: exact; running: T2), the position hold, staging chips 1/3/4, `SB_LOAD` b1/b3/b4 through the side table. Timewarp UI, REST rewind/scrub/scrubend | M1 |
| D | Files: firmware `dump`/`ringdump`/`load`; S38 `ringdump … board` for UE2's own anchors; board files through the decoder (§5.1/§5.2 "Decode") | M2/M3 |
| E | Stream 3 and CAPS2 b7, with S39's t21-p4 build | later |
| F | TRX64's gaps as they land (T1-T5) | TRX64 |

## 14. Checks

**Unit** (`trx_engine.rs`, `trx_sb.rs`, `trx_ff.rs`, `engine_tap.rs`, `trx.rs`, `c64.rs`), at least one per register
and stream:
- Byte rule on every page 2 register; write-only registers read 0; `LOG_CNT` `{4'd0, 28}`.
- `LOG_CTRL` restart zeroes `FRAME`, `DROPPED`, counts, not the cycle counter; the tap bits gate each source as
  :295-296, :321.
- Placement: records at `base | ((n × size) & mask)`, the wrap of each area, stream 0 lanes in a 16-byte word.
- Markers: one per frame at line 0 from cycle 12; line 16 without a snap under the CPU-only pause; F1 flags b0, b1,
  b3, b4, b6; F2 steps of 19,656 (PAL) between snaps; F3 x = 8 × cycle + 7; F4 = the counts, with a write in the
  instruction before the snap counted and one after it not.
- Stream 0: `STA $0400` → `{new, old, $0400}`; a write under BASIC ROM logs the RAM's old byte; a write to `$D020`
  with I/O visible none; `INC $0400` logs two; MEMONLY firmware write only with b1 and sets F1 b3; APPLY's writes
  none; the REU's fetch into RAM is logged.
- Stream 1: colour RAM nibble; cartridge RAM at `0xEF0000 + a` with the old byte; `LOG_PUT` record.
- Block: B1, 29 word records, B2 for the reset table; a 3-entry table written by `CAP_TAB_*`; odd count padded.
- Mover: COPY area → guest DDR across a wrap (two jobs, as `fetch`), FILL, cause b1.
- APPLY: undo then redo of a random log restores RAM exactly (the T20 §17.3 bench as a unit test); count 0.
- Encoder: the fixture's layout (CIA latches, ports, ICR mask, TOD alarm where the app reads them; the 6502N's
  registers, PC, din_r, port); UE2's own image of a known state.
- Side table: a load of a kept image restores the exact snapshots, clocks moved.
- `STOP_POS`: CPU held → the VIC stands at line 0 cycle c, `STOP_POS` b16, `GATE_CTRL` b8; release opens; with the
  CPU running the stop is at the first boundary ≥ c.
- `SB_LOAD` b3/b4: staged CIA words load at the release; refused while running (`SB_STATUS` b1).
- Turbo with `--trx-io`: `$D030`/`$D031` read `$FF`, a write and the menu strobe change nothing.
- S38 `goto` with the log on: the changed bytes are outside records.

**Firmware smoke** (`scripts/smoke-trx-ring.sh`, `smoke-trx-ring.py`): `run/trx/ultimate-a58f1a6e.elf`, `--board c64u
--trx-io`, a fresh flash with the ROMs, System Mode PAL, Interface Type Freeze (the Timewarp works only over a frozen
machine, `trx_timewarp_ui.cc:302`), `--net user --hostfwd`, the control port:

| Step | Expected |
|---|---|
| boot | console `[TRX] IO block present, CAPS E7`, `[RING] log on: streams at 7000000/6000000/5C00000` |
| `10 a=a+1:poke1024,aand255:poke1025,int(a/256):goto10`, run 5 s | `GET /v1/timewarp:status`: ≥ 250 frames, every one but the newest a target, stream 2 ≥ 35 records a target, `dropped` 0 |
| `PUT /v1/timewarp:rewind?frames=100` | 200, `frames_back` 100; the counter goes back, then on; one load, of the snap 100 frames back (± 2), RAM and colour RAM equal to the snap's (the CPU and CIAs are the snap's own) |
| `PUT /v1/timewarp:scrub` 50, 50, 60, 50, then `scrubend` | the 2nd and 4th step (the timeline stands from the 2nd) leave the same machine (`monitor r`, `$0000-$00FF`, `$0400-$04FF`), the 3rd another; after `scrubend` the counter runs on; every load the snap's |
| freeze button, F1, "C64 Machine" → "Timewarp", LEFT × 25, RETURN | 25 scrub lines on the console; `[RING] scrub rewind to seq`; the counter runs on; every load the snap's |
| `$D030`/`$D031` | read `$FF` |

**Not checked by the smoke:** cycle-exact `STOP_POS` with a running CPU (T2), the transitional CIA states (answer 3),
the board loading anything of UE2's (M2/M3).
