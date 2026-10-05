# S40 — The board's engine in UE2: frame ring, Timewarp and trace for the firmware

**Status:** spec (2026-10-05), local branch `s37-trx-pixel-layer`, not built. Waits for review and the answers in §12.

**Why:** Alex decided (2026-10-05) that UE2 behaves like the TRX64-Ultimate board for the firmware, so its UI and UX
can be tested without the board. On the board the app's frame ring, Timewarp (freeze menu and REST), `ringdump`,
`.c64re` dump/load and trxmon's ring and trace verbs run on the FPGA engine: CAPS b2, page 2, `trx_engine.v`, and on
t21-p4 the trace former (CAPS2 b7). UE2 presents that register interface and writes the streams into DDR exactly in
the app's format, fed from the embedded TRX64 core (`trx64-core`, pinned v0.12.3). The app is unmodified.

S38 (TRX64's own rings for UE2's monitor) stays as it is. S40 adds the board's engine next to it.

**Reads** (code and RTL are the truth; comments are not evidence):
- RTL, TRX64-Ultimate `main` (f88a95b): `designs/c64_basic/trx_engine.v` (all of it), `trx_sys.v`, `trx_io.v`,
  `c64_core.v`, `c64_mem.v`, `top.v`; `cores/icepi/cpu6510.v`, `cores/icepi/6502n/proc_{core,registers,control,
  interrupt}.vhd`, `cores/icepi/c64_redip_cia/cia_core.sv` and `reDIP-CIA/gateware/CIA/cia_*.sv` (the flip-flop
  images). Branch `t21-p4` for the trace former (`trx_engine.v`, `trx_dbg.v`, `trx_io.v`). Branch `t29-turbo` for
  turbo (`top.v`, `c64_core.v`). Line numbers are of `main` unless a branch is named.
- App, `software/ultimate-app` branch `trx64/main` (a58f1a6e, the ELF `run/trx/ultimate-a58f1a6e.elf`):
  `software/io/c64/{trx_ring.cc, trx_ring.h, trx_c64re.cc, trx_timewarp_ui.cc, trx_io.h, c64.cc}`,
  `software/api/route_timewarp.cc`, `software/app_loader/app_api_net.cc`. Branch `t21-p4-app` for the trace
  (`trx_ring.cc`, `apps/trxmon/src/{hw_trx.c, trace.c}`). Branch `t29-app` for turbo.
- Specs (intent only): `T20-delta-ring.md` (§4, §8, §16, §17, the decisions), `ARCH-lean-state-access.md`,
  `T21-monitor.md` §4.10-§4.11, `T29-turbo.md` §6.1.
- UE2: S37, S38, S39; `crates/ue2-core/src/devices/{trx.rs, trx_session.rs, c64.rs}`, `crates/c64-bridge/src/{lib.rs,
  ring.rs, hwlog.rs, board.rs, cart.rs}`. TRX64: `delta_ring.rs`, `full_sc.rs`, `c64re_snapshot.rs`, `ciacore.rs`.

**Owns:**
- `crates/ue2-core/src/devices/trx_engine.rs` (new): page 2, the record former, the engine store, the mover, APPLY,
  the trace's stream 3.
- `crates/ue2-core/src/devices/trx.rs`: CAPS, CAPS2; page 0 `STOP_POS`, `RESUME_PHASE`; page 1 staging of chips 3/4,
  `SB_LOAD` b3/b4; IRQ causes b1, b2.
- `crates/ue2-core/src/devices/c64.rs`: the position hold, the snap, moving the bridge's records into the link.
- `crates/c64-bridge/src/engine_tap.rs` (new): the taps on TRX64, the snap point, the CPU-held run to a position.
- `crates/c64-bridge/src/trx_ff.rs` (new): the 6502N and reDIP-CIA flip-flop images, both ways, and the side table.
- `crates/c64-bridge/src/board.rs`: `ringdump … board` writes UE2's own anchors through `trx_ff.rs` (§8).

## 1. What the app does with the engine

`trx_ring.cc` is the only user of page 2 on `trx64/main`; `trx_c64re.cc`, `route_timewarp.cc`,
`trx_timewarp_ui.cc` and the `trx->ring` table (`app_api_net.cc:290-339`) go through it. Nothing else touches the
engine. What it does, in the order UE2 must serve it:

| Step | App code | Registers |
|---|---|---|
| Start (6 s after boot, and after a load) | `start_locked` 1672 | `LOG_CTRL` 0; `LOG_BASE`/`LOG_MASK` × 3 (the reset layout); `CAP_CTRL` 1; `LOG_CTRL` `0x10F` (restart, on, every tap) |
| Poll, every 100 ms | `trx_ring_poll` 1770, `parse_locked` 248 | `LOG_CNT` 0, 1, 2; COPY of stream 2 into a bounce buffer; parse F1-F4, B1, words, B2; `CAP_CTRL` back to 1 |
| Read any area | `fetch` 194, `eng_copy` 171 | `ENG_SRC`, `ENG_DST`, `ENG_LEN`, `ENG_CMD` 1, poll `ENG_CMD` b0 |
| Stop the machine | `stop_machine` 465 | `STOP_POS` arm cycle 12, poll b16; `SH_CTRL` 0; `GATE_CTRL` 1; `C64_STOP`; one DMA read (`$0002`) with the log on; poll `LOG_STAT` b1/b3 |
| Put a frame back | `put_back` 873 | Ultimax bus writes: cartridge banking, colour RAM, VIC, CIA registers; `GATE_CTRL` 0 (CPU-only pause); `STOP_POS` arm + release at the block's cycle; DMA writes to `$D019`, reads of `$D01E/F` while the VIC runs; poll b16; state bus reads of the VIC latches; RAM through MEMONLY or APPLY; `SB_DATA` staging of chip 1 w8-14, chip 3 w8-19, chip 4 w8-19; `SB_LOAD` `$1A`; release |
| Undo/redo stream 0 | `apply_run` 766 (CAPS2 b5) | `AP_START`, `AP_COUNT`, `AP_CMD`, poll `AP_CMD` b0 |
| Undo stream 1 | `walk_range` 1088 | colour RAM as bus writes, cartridge RAM straight into DDR `0x0EF0000 + a`, then page 4 `CMD` b1 |
| Release | `restore_locked` 1040-1051 | `CAP_CTRL` 1, `LOG_CTRL` `0x00F`, `STOP_POS` b9: the load edge |
| Scrub step | `scrub_show_locked` 1315 | as put back, then one frame with the log on: `STOP_POS` arm + release at cycle 4, poll; `STILL_STATUS` |
| Land (trxmon) | `land_locked` 1445 | as scrub, then `CYC_CMP` = `CYCLES`, `OWNER`, `STOP_POS` release, poll `STATUS` b3 |
| Materialize, `.c64re` | `materialize_locked` 1589 | a pause, RAM by MEMONLY, colour RAM through the session |

The app reads `LOG_STAT`, `DROPPED` (status) and never `FRAME`. It never writes `CAP_TAB_*`, `LOG_PUT_*` or
`ENG_ARG`, never uses FILL, and never enables IRQ causes b1/b2 (no handler on `trx64/main`). UE2 builds them anyway,
from the RTL.

The Timewarp UI ("C64 Machine" → "Timewarp", "Save Timewarp Ring") appears only with CAPS b2
(`trx_timewarp_ui.cc:409`). REST `/v1/timewarp:{status, frame, start, stop, rewind, scrub, scrubend, dump, ringdump,
load}` answer 503 without it (`route_timewarp.cc` `refuse_absent`).

## 2. CAPS and CAPS2

| Bit | Board | UE2 before | S40 | Turns on |
|---|---|---|---|---|
| CAPS b2 engine | 1 | 0 | **1** | `trx_ring_init` (`trx_ring.cc:1994`), the ring task, Timewarp UI, REST, `trx->ring` for trxmon |
| CAPS2 b0 F4 order | 1 | 0 | **1** | the parser does not look for F4s after block records (`trx_ring.cc:1997`) |
| CAPS2 b5 APPLY | 1 | 0 | **1** | `apply_hw`: stream 0 is undone by the engine, not walked by the CPU (`trx_ring.cc:1998`) |
| CAPS2 b7 trace (t21-p4) | 1 on t21-p4 | 0 | **1, phase E** | the t21-p4 app's `trace_ready()`; trxmon's trace verbs |
| CAPS2 b6 turbo (t29) | 1 on t29 | 0 | 0 | T29's registers; not built (§7) |
| CAPS2 b1 BUILD | 1 | 0 | 0 | unchanged (S37 reads `BUILD` as 0) |

**CAPS = `$EF`** (S39 `$EB` + b2). **CAPS2 = `$3D`** (S39 `$1C` + b0 + b5), **`$BD`** with the trace (phase E).

- `trx64/main` (a58f1a6e) does not read b7 or b6. The t21-p4 app reads b7.
- S39 §2's table row "CAPS b2: off" and its Q1 are replaced by this spec. S38 §7's "UE2 does not fake them" ends for
  b2: the engine is served, not faked.

## 3. Page 2 registers

32-bit, `trx_sys.v` byte rule (S37/S39: bytes 0-2 collect, byte 3 commits, byte-0 read latches; one buffer and one
latch for all far pages). `r_addr` is the word index; offsets below are bytes. Write-only registers read 0
(`trx_engine.v:525-538`).

| Off | Name | RTL | UE2 |
|---|---|---|---|
| 000 | `LOG_CTRL` W | :463-471: b0 on, b3:1 taps (b1 outside, b2 cartridge RAM, b3 colour RAM), b8 restart: `FRAME`, `DROPPED`, gap, ext, stream counts to 0 (not the cycle counter). t21-p4 :551-552: b4 trace on, b9 drop the pending trace record. Reads 0 (§12 Q6) | as RTL. Log off drops nothing UE2 holds (§4.6) |
| 004 | `LOG_STAT` R | :528 `{kmiss b4, FIFO b3, mover b2, block b1, on b0}`; t21-p4 :616 b5 trace FIFO | b0 = on; b1-b5 0: UE2 forms, copies and drains in the same step (§4.6) |
| 008 | `FRAME` R | :529, :370 markers since restart (also dropped ones) | count of markers (§4.2) |
| 00C | `DROPPED` R | :530 b15:0 | 0, except §7 |
| 020/030/040 | `LOG_BASE[s]` W | :472-477 bits 26:4 | as RTL |
| 024/034/044 | `LOG_MASK[s]` W | the same | as RTL |
| 028/038/048 | `LOG_CNT[s]` R | :531-533 `{4'd0, count 28}` records written since restart | as RTL |
| 050 | `LOG_PUT_LO` W | :478 | as RTL |
| 054 | `LOG_PUT_HI` W | :332-343: with on, puts `{HI, LO}` into stream 1; a put still held counts as dropped | as RTL, never held |
| 058 | `LOG_CNT[3]` R (t21-p4) | :622, :841 trace records mod 349,525 × 768 | phase E |
| 060 | `CAP_TAB_IDX` W | :479 b5:0 | as RTL |
| 064 | `CAP_TAB_DATA` W | :480-483 `{count b22:16, first word b11:0}`, then IDX + 1; refused while a block is copied | as RTL, never refused |
| 068 | `CAP_TAB_LEN` W | :484 b6:0, clamped to 64; reset 9 | as RTL |
| 080 | `ENG_CMD` W/R | :485-488 1 COPY, 2 FILL, refused while busy or APPLY runs; R b0 busy | §6.1; busy reads 0 |
| 084, 088, 08C | `ENG_SRC`, `ENG_DST`, `ENG_LEN` W | :489-491 bits 26:4 (16-byte units) | as RTL |
| 090 | `ENG_ARG` W | :492 the FILL word | as RTL |
| 094, 098 | `AP_START`, `AP_COUNT` W | :493-494 28 bits | as RTL |
| 09C | `AP_CMD` W/R | :518-522 b0 1 undo / 0 redo, starts unless a job runs or count is 0; R b0 busy | §6.2; busy reads 0 |

**The table** (`CAP_TAB_*`, `tab[]` :184-197): 64 entries `{count 7, state-bus address 12}`, reset
glue 1 / 6510 3 / VIC 12 / CIA 1 5 / CIA 2 5 / cartridge 1 / 6510 w8+ 7 / CIA 1 w8+ 12 / CIA 2 w8+ 12 = 58 words.
UE2 walks it as the sequencer does (:393-452) over the S39 state bus model (§5).

**IRQ causes** (`trx_io.v` b1, b2): b1 when a COPY, FILL or APPLY job ends (`eng_tgl`, :218, :556); b2 at the first
lost record after a marker (`gap_tgl`, :309). UE2 raises b1 at each job's end (the job ends inside the commit) and b2
only under §7.

## 4. The streams and how UE2 makes them

### 4.1 Where the records go

- **The engine store.** The areas sit at `0x5C00000-0x7FFFFFF` (reset layout :198-204; t21-p4 adds stream 3 at
  `0x6800000`, :227). UE2's guest DDR is 64 MB (`bus::RAM_SIZE`); the app cannot address above it either and reads
  only through COPY (`trx_ring.cc:31-33`). UE2 keeps DDR addresses `0x4000000-0x7FFFFFF` as an engine-only store,
  allocated in 1 MB chunks on first write. Below 64 MB the mover reaches guest DDR.
- **Placement**, as the drain (:621-624): record n of stream s at `{base | (off[26:4] & mask), off[3:0]}`,
  `off` = n × 4 (stream 0) or n × 8. Bitwise OR, as built, not an add.
- **Counts** `LOG_CNT[s]` = records written, mod 2^28.

### 4.2 Stream 2: the frame boundary

**The snap point** (`c64_core.v:627-674`). Armed at line 16 of every frame while `LOG_CTRL` b0 or `CAP_CTRL` ≠ 0.
The snap is the first phi2 fall on line 0 from cycle 12 (x ≥ 96) that ends an opcode fetch, with: machine running
and not halted, border flip-flops set, no sprite pixel on its way, no VIC access in the ending cycle, the REU idle
(:643-644). No snap by line 16: the boundary is at line 16 without a capture (`mk_snap` 0). A `STOP_POS` stop
disarms it (:662). A frame of the `CAP_CTRL` cadence captures at its snap (`snap_cap`, :645).

**In UE2** the snap is the first TRX64 instruction boundary on line 0 at raster cycle ≥ 12 where the conditions hold
(S39 §3.2 uses the same point for the capture; one implementation). The conditions come from TRX64 (gap T3). The
machine state at a TRX64 boundary is the state before the fetch; the board's snap is the edge after it. The block
(§4.4, §5) encodes the post-fetch form, F2 and `CYCLES` count the fetch (S39 §3.3: `clk − base + 1`). The CPU-held
pause, the reset hold and turbo (§7) give no boundary, so the marker comes at line 16.

**Markers** (:351-371). One marker per boundary, three records then the F4 the drain adds (:730-737):

| Rec | Bits | UE2 |
|---|---|---|
| F1 | `{F1, flags 8, CAP_GEN 16, frame 32}` | flags below; `CAP_GEN` after this snap's capture (S39's counter); frame = `FRAME` before the increment |
| F2 | `{F2, 32'd0, cycles 24}` | `CYCLES` low 24 bits after the snap edge (`cycles + phi2_tick`, :360). The board's engine counter and the debug block's `CYCLES` count the same edges (the app rebases one by the other, `rebase_cycles` 408); UE2 has one counter |
| F3 | `{F3, 7'd0, line 9, 6'd0, x 10, 24'd0}` | line 0 (16 at a line-16 boundary); x = 8 × cycle + 7 (the board's own file: x 103, 111, 119, 127 for cycles 12-15; §12 Q4 for the cycle origin) |
| F4 | `{F4, stream 1 count 28, stream 0 count 28}` | the counts after every record of the frame that ends here |

F1 flags (:359): b0 snap; b1 a block follows (`mk_cap`); b2 records lost since the last marker (§7); b3 outside
writes in the frame (§4.3); b4 the snap is a fetch an interrupt drops (`mk_intg`: TRX64's next `execute` takes an
interrupt, gap T4); b5 capture skipped because a block was still copied (never in UE2); b6 the REU moved data in the
frame (TRX64's REU DMA active, :374).

**Order** (:48-50, :255-258, :285): every stream 0/1 record made at or before the boundary is counted in its F4,
every later one after. In UE2: records of instructions that ended at or before the snap boundary, then the marker,
then the rest.

### 4.3 Streams 0 and 1

| Stream / space | RTL source | Record | UE2 source |
|---|---|---|---|
| 0, machine | `c64_mem` port A RAM writes (`c64_mem.v:92` `is_ram`), whoever drives the bus: CPU, REU, a DMA bus cycle | `{new 8, old 8, addr 16}` LE, old = the RAM byte overwritten (:315) | every TRX64 write that lands in the 64 KB RAM, with the RAM's old byte: CPU stores and RMW dummy writes, the REU's DMA into C64 RAM, the firmware's window writes without MEMONLY (gap T1) |
| 0, outside (`LOG_CTRL` b1) | port B: JTAG, the freeze responder's commit, the app's MEMONLY writes (`top.v:1299`, `!ap_gnt`) | same; marker flag b3 | firmware MEMONLY writes, the S37 session's commit list, S39's RAM writes, **S38 transport restores** (§9) |
| 1 space 1, colour RAM (b3) | port A (`cram_tap`), :318 | `{0, cycle 15, 0, new 4, 0, old 4, 1, 18'd0, addr 10}` | TRX64 writes into colour RAM (CPU with I/O visible, DMA bus cycles), old nibble from colour RAM |
| 1 space 1 outside (b1 & b3) | port D, :320 | same, b63 = 1 | the session's commit to `$D800-$DBFF` |
| 1 space 2, cartridge RAM (b2) | `cart_slot` RAM writes, :329; old byte read from DDR by `cart_fill` (`D_K`, :708-720) | `{0, cycle 15, new 8, old 8, 2, 13'd0, addr 15}` | `CartLogic::ddr_write` into the cartridge RAM window `0xEF0000-0xEFFFFF` (cart.rs `RAM_BASE`, the board's address too), offset in [15:0], old byte from DDR (never a miss: `LOG_STAT` b4 stays 0) |
| 1 space 3, flash | `LOG_PUT` only (app's ISR, not used on `trx64/main`) | | `LOG_PUT` |
| 1 space 8, debug log | `trx_dbg.v:235`, a unit's log action | `{0, cycle 15, ib ? addr : ipc, 8, 0, ib, we, unit, data, addr}` | S39's units with the log action (S39 §3.3 "log: HITS only" becomes this record) |

- **The cycle field** is the engine's free-running counter's low 15 bits at the write (:302, :329), not relative to
  F2; the header's "minus F2's" is the reader's work (§12 Q5). UE2: `CYCLES` low 15 bits at the write's cycle.
- **Not logged**: APPLY's writes (`ap_gnt` masked, `top.v:1299`), anything while `LOG_CTRL` b0 is 0, writes the
  firmware's CPU makes straight into DDR (cartridge RAM undo, REU memory), the REU's memory, flash and EEPROM
  contents, GeoRAM, the SID, drives.
- **Taps as the RTL**: stream 0 machine writes need only b0; outside ones b0 and b1; colour RAM b0 and b3; outside
  colour b0, b1 and b3 (:295-296); cartridge RAM b0 and b2 (:321).

### 4.4 Stream 2: the chip block

After a marker with F1 b1 (:394-452):

| Rec | Bits |
|---|---|
| B1 | `{B1, 0, table entries 7, 8'd0, CAP_GEN at the mark 16, frame 24}` |
| words | the table's state-bus words in order, two per record, word 2i in bits 31:0; an odd last word padded with 0 (`Q_PAD`) |
| B2 | `{B2, 7'd0, torn, words 16, CAP_GEN now 16, CAP_GEN at the mark 16}` |

UE2 reads the words from the S39 shadows loaded at that snap (S39 §3.2) in the same step: never torn, never
skipped, `CAP_GEN` equal at both ends. The words that S39 does not build yet come from §5.

### 4.5 Stream 3: the trace (t21-p4, phase E)

`trx_engine.v` t21-p4 :260-284, :343-377, :811-842. One record per instruction, written at the next IB, three
8-byte words into the fixed area `0x6800000` (8 MB, 1,048,575 words so a record never straddles the wrap):

| Word | Bits | UE2 |
|---|---|---|
| 0 | `[23:0]` the phi2 count after I's fetch edge | `CYCLES` low 24 at I's boundary + 1 |
| 1 | `[27:0]` write_start = stream 0 records pushed when I began, `[47:32]` pc, `[63:48]` write_count = stream 0 records up to I+1's IB | from UE2's stream 0 count (machine and outside records, as `wcnt`, t21-p4 :345) |
| 2 | A, X, Y, S, P before I (`tr_regs` = chip 1 w4/w5, 12 clocks after the IB), opcode, b1, b2 = the bus data of cycles 2 and 3 | TRX64's registers at the boundary (settled); opcode; b1/b2 = the bytes the CPU read in its 2nd and 3rd cycle (for a 1-byte opcode the dummy read, §12 Q9) |

`LOG_CNT[3]` (byte 058) counts records and wraps at 349,525 × 768. On with `LOG_CTRL` b0 and b4; b9 drops the
pending record. A record still going out when the next IB comes is dropped (:359-363); UE2 never drops.
Interrupt sequences make no record (no IB).

### 4.6 Timing and the drain

- The board's drain empties a 512-deep FIFO into DDR in ui_clk; a full FIFO drops records (`DROPPED`, F1 b2, cause
  b2). UE2 forms each record when TRX64 makes it and writes it into the store at once; the bridge hands them over in
  order (§10). No FIFO, no drop, `LOG_STAT` b1/b3 read 0, `drain()` returns at once.
- The C64 is brought up to the access's time before any page 2 read is answered (S37's rule), so `LOG_CNT` and the
  areas hold everything up to "now".
- The areas are circular. Lapping is the app's business (`reachable`, `trim`, "stream 2 lapped"); UE2 does nothing
  extra.

## 5. The chip block's words from TRX64

The block is the state bus (S39 §1.2, §3.2). S40 adds what S39 leaves at 0 and makes the flip-flop words exact.

| Words (table order) | Contents (`c64_core.v:584-612`, :754-763) | From TRX64 |
|---|---|---|
| 0 glue `0.0` | `{CAP_GEN, 0, held, cart present, EXROM, GAME, halted}` | S39 §3.2 |
| 1-3 `1.0-1.2` | `{S,Y,X,A}`, `{sync, P, PC}`, `{$01 read, $01 latch, $00}` | S39 §3.2; at a snap **PC = the address of the opcode just fetched, sync 1** (board file: `w1.1 = $0122E5CF`, cpu node pc `$E5CF`, `PC_ADJ` 0 in `trx_c64re.cc:34`). This answers S39 Q4 for snaps |
| 4-15 `2.0-2.11` | VIC `$D000-$D02F` as read, no side effect | S39 §3.2 |
| 16-20, 21-25 `3.0-3.4`, `4.0-4.4` | CIA registers as read, `{DDRB, DDRA, PB, PA}` | S39 §3.2 |
| 26 `7.0` | `{reg_1, reg_0}` of icepi's cartridge model | S39 §3.2 (U2 mapping); the app replays only EasyFlash, MegaByter, Ocean, Magic Desk (16), Normal (`replay_cart` 664) |
| 27-33 `1.8-1.14` | the 6502N's 209 flip-flops | §5.1 |
| 34-45, 46-57 `3.8-3.19`, `4.8-4.19` | each CIA's 363 flip-flops | §5.2 |

### 5.1 The 6502N image (209 bits)

Layout (`cpu6510.v:126-177`, `proc_core.vhd:155-289`, `proc_registers.vhd:347-368, :472`): A 7:0, X 15:8, Y 23:16,
S 31:24, P 39:32, PC 55:40, adl 63:56, adh 71:64, dreg 79:72, IR 87:80, h 95:88, flags 101:96, control 123:102
(state 4:0, a_mux, dout_mux, rwn, latch_dreg, copy_d2p, dummy, …), interrupt 129:124 (irq_c, nmi_c, nmi_d1, nmi_d2,
nmi_act, resetting), din_r 137:130, pin_r 143:138, pdir 151:144, pout 159:152, pfloat 167:160, fall6 186:168, fall7
205:187, rdy 206, irq 207, nmi 208.

What the board's own file shows at a snap (fixture `board_2026-10-04.c64rering.gz`, 9 blocks): PC = the fetched
opcode's address, **din_r = that opcode**, **IR = the previous instruction's opcode** (`$A5` before `$E5CF`,
`$8D` before `$E5D4`), adl = the previous operand. The core is one cycle into the next instruction.

**Encode** (TRX64 → image), at a snap:
- A, X, Y, S, P: TRX64's registers (settled); the pending write-back bits (`copy_d2p`, `flags_update`,
  `trigger_upd`) 0, so the image says "nothing left to write back" (§12 Q2).
- PC = `reg_pc`; din_r = the byte there (through the PLA); IR = the previous opcode (UE2 keeps it, S39 §3.3).
- control state, a_mux, dout_mux, rwn and the other control bits: the after-fetch values, taken from the fixture's
  block after an instruction with no write-back (`$8D` STA abs), as a constant template.
- interrupt bits from TRX64's interrupt state (`IntStatus`: IRQ/NMI pending, NMI edge); rdy 1, irq/nmi lines as TRX64
  drives them.
- pdir, pout = `$00`, `$01` latch; pfloat, fall6, fall7 from TRX64's port fall-off state (`data_set_clk_bit6/7`);
  pin_r = the port's input pins.
- adl, adh, h, dreg: loaded only with `SB_FULL` (`proc_registers.vhd:360-368`); UE2 writes the previous operand bytes.

**Decode** (image → TRX64), at a load: A, X, Y, S, P with the pending write-back applied when its bits say one is due
(the rule from `proc_registers.vhd`, checked on the fixture's `$A5` entries); PC; interrupt state; port. TRX64 then
fetches the opcode at PC itself, in the same cycle the board's load edge ends it (§6.3).

### 5.2 The reDIP-CIA image (363 bits)

Layout (`c64_redip_cia/cia_core.sv:148-352`, `c64_redip_cia.v:108-115`): [1:0] reset/phi2, [7:2] edge detectors,
[42:8] ports (`{prb_rw 3, PRA, PRB, DDRA, DDRB}`: DDRB 15:8, DDRA 23:16, PRB 31:24, PRA 39:32 as the app reads them,
`trx_ring.cc:632-635`), [82:43] timer A (prescaler = latch 58:43, counter 74:59, flags 82:75), [122:83] timer B, [243:123]
TOD (alarm 154:123 as the app reads it, clock, latch, flags), [286:244] serial, [316:287] interrupt (mask 315:311),
[326:317] control A, [336:327] control B, [362:337] the TOD tick divider.

**Encode** from TRX64's `CiaSnapshot` (`c64re_snapshot.rs:381` `capture_cia`): registers, latches, counters, mask,
flags, TOD clock, latch, alarm; the timer pipeline flags from `ta_state`/`tb_state` for the steady states (stopped,
running, one-shot armed); serial idle unless shifting; the divider from TRX64's TOD tick phase. Transitional pipeline
states (a write two cycles before the snap) are approximated (§12 Q3).

**Decode** into a `CiaSnapshot` as the app's own writer does it (`write_cia`, `trx_c64re.cc:370-414`), extended with
the counters, the ICR flags, the serial state and the TOD, then `restore_cia` with its clocks moved to now (gap T5).

### 5.3 The side table: exact round trips in UE2

The images are lossy against TRX64's state (VICE's alarm clocks, the CPU's cycle-level state). So UE2 keeps, for
each block it made, the exact TRX64 parts: `CpuSnapshot`, `IntStatusSnapshot`, both `CiaSnapshot`s, the cycle. The
key is a hash of the 31 image words (7 + 12 + 12). At `SB_LOAD`, staged words equal to a kept image load the kept
parts (rebased to now); any other image (a board file, a hand-edited block) goes through the decoder. The table holds
4096 entries (the app's `TL_CAP`), oldest dropped first.

## 6. The mover, APPLY and the restore path

### 6.1 COPY and FILL

`ENG_CMD` 1: `ENG_LEN` bytes `ENG_SRC` → `ENG_DST`; 2: `ENG_ARG` repeated over `ENG_LEN` bytes at `ENG_DST`
(:677-761). Addresses and length in 16-byte units, 27 bits. UE2 runs the job inside the commit: below `0x4000000`
guest DDR, above it the engine store; an unwritten chunk reads 0. Busy never reads 1; cause b1 is raised. A command
while APPLY runs is refused, as :485 (never in UE2).

### 6.2 APPLY

`AP_CMD` (:499-523, ui_clk :654-700): `AP_COUNT` stream 0 records from index `AP_START`; b0 1: downwards, the old
byte; 0: upwards, the new byte; each record at `LOG_BASE[0] | ((n × 4) & LOG_MASK[0])`. Written through the RAM's
second port, not logged. UE2: inside the commit, each byte into TRX64's RAM (the MEMONLY path, no tap), in index
order, last write wins. Count 0 does nothing; refused while a mover job runs.

### 6.3 What the restore needs beyond page 2

| Need | RTL | UE2 |
|---|---|---|
| `STOP_POS` (page 0 0x114) | `trx_sys.v:326-331`, R :220 `{hold b16, arm b8, cycle}`; `c64_core.v:638-639` `pos_hit` on line 0 at cycle `pos_cyc`; the gate shut until b9 | **new**. With the CPU held (CPU-only pause, reset): the chips run cycle by cycle (`run_held`) and stop one edge before the asked cycle's phi2 fall: exact. With the CPU running: the first TRX64 boundary on line 0 at or after the cycle (S39 T1). The hold is a third whole-machine hold beside the pause and the debugger's (S37 `set_paused`); `GATE_CTRL` b8 halted reads 1 under it |
| `RESUME_PHASE` (0x110) | :325, R :219 pending | accepted, reads 0. Not used by `trx64/main` |
| CPU-only pause | `GATE_CTRL` b0 = 0 with `C64_STOP`: RDY low, the VIC and CIAs run | `Hold::Cpu` (lib.rs `apply_hold`), there since S14 |
| Bus writes in Ultimax, DMA reads while the VIC runs | `C64_MODE`, the DMA window | S14/S28/S37 paths |
| Staging chips 1, 3, 4 w8+ | `trx_sys.v:317-323`, `c64_core.v:691-695`; refused unless the gate is shut | S39 §3.4 for chip 1; **chips 3/4 new**; refused under the same rule (`SB_STATUS` b1) |
| `SB_LOAD` b1, b3, b4 | `trx_sys.v:324`; the staged words load at the next opening edge | at the release that opens the gate: §5.1/§5.2/§5.3 decode into TRX64, before it runs. `SB_LOAD` 0 disarms |
| The load edge | `STOP_POS` b9 release: the snap edge itself, CPU post-fetch | TRX64 runs from the boundary; its fetch is that edge. A `CYC_CMP` equal to `CYCLES` at the release stops at that boundary without running, `CYCLES` + 1 as at every IB stop (S39 §3.3): the land (`land_locked` 1490-1499) |
| VIC write-only registers, internals, latches | not restored (T20 decisions); the app clears the latches by DMA while the VIC runs | the same: the VIC is not touched by a load |

## 7. Turbo, pause, reset

**Turbo.** `main`'s RTL has no turbo: the C64 always runs at 1 MHz and the engine always records. The "records at
1 MHz only" rule (T29 §6.1, decided 2026-10-03) is built on `t29-turbo`: while the turbo engine runs, markers are not
armed (`log_on && !turbo_on`, `top.v` t29:1259), no snap (`c64_core.v` t29:680), and the turbo CPU writes through
`c64_mem`'s second port, which the taps do not see; `t29-app` restarts the ring when `TURBO_STAT` says turbo ran
(`trx_ring.cc` t29:1791-1799). The `trx64/main` app has no such code.

UE2 runs turbo through TRX64 (`turbo_engaged`), so it follows `t29-turbo`'s RTL: while turbo is engaged, no boundary,
no marker, no capture, no CPU write records; REU and firmware writes are still logged. S38 stands the same way, so
UE2 has one rule. With `trx64/main` the app does not restart: a rewind across a turbo span leaves RAM written only in
turbo as it is (§12 Q1). CAPS2 b6 stays 0.

**Pause** (as the RTL: nothing runs, nothing is recorded):

| State | Markers | Records |
|---|---|---|
| Machine pause (FRZ), debugger hold, position hold, S38 transport hold | none | outside writes only (MEMONLY, session commit), with b1 |
| CPU-only pause (`C64_STOP`, FRZ 0), reset held | at line 16, no snap, no block | port A writes of DMA bus cycles; colour RAM by bus cycles |
| Running | at the snap | all |

A C64 reset does not stop or restart the engine (no such code on the board, S38 §7).

## 8. `trx64Hw`: UE2 produces it

The board's `.c64re` and `.c64rering` carry `trx64Hw` with the 58-word block (`trx_c64re.cc:495-511`); its loader
refuses a file without it (S38 §7). With §5 the block exists for every UE2 frame:

- **Firmware files from UE2**: `timewarp:dump`, `timewarp:ringdump` and "Save Timewarp Ring" write board files from
  UE2's C64, which the board loader takes (`check_board_ring` passes).
- **S38 `ringdump … board`** builds `trx64Hw` for UE2's own anchors with the same encoder from the anchor's TRX64
  nodes (cpu, cia1/2, vic, the turbo node): S38 §11's limit ends. The block's x and line come from the anchor's VIC
  position.
- **Loading**: a board file in UE2 (firmware `timewarp:load`, S38 `ringload`) goes through the decoder (§5.1-§5.2).
- Fidelity on the board is what the RTL bench says about UE2's images (§12 Q2, Q3).

## 9. With S37, S38, S39

- **S37 session**: the app ends it (`SH_CTRL` 0) before a restore. Its commit list writes are outside records.
- **S38 rings** run as before, beside the engine. A transport restore (`goto`, `frame`, `ringload`) changes RAM and
  colour RAM behind the firmware: UE2 logs every changed byte as an outside record (stream 0 port B, stream 1 b63),
  as JTAG writes are logged on the board, so the app's undo across it is right. An engine restore (SB_LOAD) is a C64
  that changes under S38, as a firmware load is: S38 keeps recording.
- **S39**: one state-bus model. The snap is S39's capture point; the block's words are S39's shadows; `SB_LOAD` b1
  keeps S39 §3.4 for trxmon's CPU write path and adds §5.1's decode for images. `CYCLES` is the engine's counter.
  trxmon's ring verbs (`rstep` replays, `checkpoint/*`, land, commit) now answer through `trx->ring`; the trace verbs
  with phase E and the t21-p4 app. Ownership (S39 §3.8): a land is the app's run control; UE2's monitor and S38's
  transport are refused while OWNER is set, as S39 says.
- **UE2's monitor**: `monitor trx` also shows page 2: on, taps, counts, the last marker, the areas.

## 10. In the code

- **c64-bridge** (`engine_tap.rs`): an observer on TRX64's run that yields, in order, records `{kind, addr, old,
  new, cycle}` and boundary events `{snap or line 16, cycle, line, x, intg, reu}`. A run that records stops at each
  boundary (as S38's `run_recording`) so the S39 shadows are taken there. `CYCLES` comes from the bridge.
- **ue2-core** (`c64.rs`): after each advance, and before every far-page access, the port moves the bridge's events
  into `TrxLink.engine`; the engine forms and writes the records.
- **ue2-core** (`trx_engine.rs`): registers, store, placement, markers, blocks, mover, APPLY, stream 3.
- **trx_ff.rs**: encode/decode and the side table, used by the engine, `SB_LOAD` and `board.rs`.

## 11. TRX64 gaps

| # | Gap | Until it closes |
|---|---|---|
| T1 | **A memory write tap**: every write that lands in the 64 KB RAM or colour RAM, with the old byte of that memory and the master (CPU, RMW dummy, REU DMA). Today `on_bus` gives the CPU-visible old byte (ROM under BASIC/KERNAL, 0 from `$D000`, `full_sc.rs:260-301`) and the REU writes RAM without it | UE2 keeps a mirror of RAM and colour RAM: old from the mirror, landing by TRX64's PLA at the write; a compare at each boundary finds writes no tap saw (the REU) and logs them before the marker, out of order |
| T2 | **Cycle-exact stop with the CPU running** (S39 T1) | `STOP_POS` with a running CPU and `CYC_CMP` land at the next boundary |
| T3 | **Snap conditions** as reads: VIC border flip-flops, sprite pixel pending, VIC access in the cycle, REU busy | UE2 checks what TRX64 exposes (border state, REU active) and treats the rest as met |
| T4 | **"The next `execute` takes an interrupt"** (S39 T2) | F1 b4 from UE2's own check (`nmi_due` and the IRQ line) |
| T5 | **Per-chip restore at a later clock**: `restore_cia`, `restore_cpu`, `restore_int_status` take absolute clocks | UE2 moves the snapshot's clock fields by (now − snap) itself; TRX64 should name them (`rebase(delta)`) |
| T6 | **The 6502N and reDIP images** are not TRX64's (by design) | UE2's codec §5, the fixture as template |
| T7 | **The CPU port's fall-off state** (`data_set_clk_bit6/7`) and pin inputs as plain reads | read where public, else 0 |

## 12. Questions for the FPGA session

1. Turbo with `trx64/main`: UE2 follows t29's RTL (no markers, CPU writes not logged); should the main app restart the ring after turbo, or must UE2 report CAPS2 b6 and `TURBO_STAT`?
2. Will tb_ring load a UE2-encoded 6502N image (settled registers, no pending write-back, IR = previous opcode) and run identically?
3. Which reDIP-CIA pipeline bits are valid for a running, stopped and one-shot timer at a snap (for the encoder's tables)?
4. Which TRX64 raster cycle is the RTL's cycle 0 on line 0 (x = 8 × cycle + 7)?
5. Stream 1's cycle field is raw `cycles[14:0]` (:302) while the header says "minus F2's": which is the contract?
6. `LOG_CTRL` reads 0 although the header says RW: intended?
7. Do CPU writes to `$00/$01` reach `c64_mem` port A (and stream 0) on the board?
8. "REU means no Timewarp" (T20 decision) is not in `trx64/main`'s app: will it be, or does the engine log REU DMA as built?
9. Stream 3's b1/b2 for a 1-byte opcode are the dummy-read bytes; TRX64 has 0 there: does trxmon depend on them?
10. S40 targets `main`'s engine with the `trx64/main` app, S39 builds t21-p4: is CAPS2 `$3D` (`$BD` with the trace, no b1) a valid build for both apps?
11. Do the engine's `cycles` and the debug block's `CYCLES` count the same edges from power-on, as `rebase_cycles` assumes?
12. The board can mark a block torn or skipped; UE2 never does: acceptable?

## 13. Phases

| Phase | Content | Needs |
|---|---|---|
| A | Page 2 registers, the store, placement, counts, restart, the table, the mover, APPLY, causes b1/b2; CAPS b2, CAPS2 b0/b5 | S39 phase A (page 1 model) |
| B | The taps (T1 mirror), the snap and line-16 boundaries, markers F1-F4, blocks from S39's shadows with §5's encoder, the side table; turbo and pause rules; S38 transport restores as outside writes | A |
| C | The restore path: `STOP_POS` (CPU held: exact), the position hold, staging chips 3/4, `SB_LOAD` b1/b3/b4 with the decoder; `CYC_CMP` at the load edge. Timewarp UI, REST rewind/scrub/scrubend, materialize | B, S39 phase B for the land |
| D | Files: firmware `dump`/`ringdump`/`load`; S38 `ringdump … board` for UE2's own anchors; board files through the decoder | C |
| E | Stream 3 and CAPS2 b7, with S39's t21-p4 build | B, S39 |
| F | TRX64's gaps as they land (T1-T5) | TRX64 |

## 14. Checks

**Unit** (`trx_engine.rs`, `trx_ff.rs`, `engine_tap.rs`, `trx.rs`), at least one per register and stream:
- Byte rule on every page 2 register; write-only registers read 0; `LOG_CNT` `{4'd0, 28}`.
- `LOG_CTRL` restart zeroes `FRAME`, `DROPPED`, counts, not the cycle counter; the tap bits gate each source as
  :295-296, :321.
- Placement: records at `base | ((n × size) & mask)`, the wrap of each area, stream 0 lanes in a 16-byte word.
- Markers: one per frame at line 0 from cycle 12; line 16 without a snap under the CPU-only pause; F1 flags b0, b1,
  b3, b4, b6; F2 steps of 19,656 (PAL) and 17,095 (NTSC) between snaps; F3 x = 8 × cycle + 7; F4 = the counts, with
  a write in the instruction before the snap counted and one after it not.
- Stream 0: `STA $0400` → `{new, old, $0400}`; a write under BASIC ROM logs the RAM's old byte; a write to `$D020`
  with I/O visible none; `INC $0400` logs two; MEMONLY firmware write only with b1 and sets F1 b3; APPLY's writes
  none.
- Stream 1: colour RAM nibble with cycle; cartridge RAM at `0xEF0000 + a` with the old byte; `LOG_PUT` record;
  a debug unit's log record (space 8).
- Block: B1, 29 word records, B2 for the reset table; a 3-entry table written by `CAP_TAB_*`; odd count padded.
- Mover: COPY area → guest DDR across a wrap (two jobs, as `fetch`), FILL, cause b1.
- APPLY: undo then redo of a random log restores RAM exactly (the T20 §17.3 bench as a unit test); count 0.
- Codec: the fixture's 9 blocks decode to the cpu nodes the board wrote (A, X, Y, S, P, PC, port), the pending
  write-back applied; UE2's encode → decode → equal TRX64 state; a side-table hit restores the exact snapshots;
  CIA encode → decode keeps registers, latches, counters, mask, TOD.
- `STOP_POS`: CPU held → the VIC stands at line 0 cycle c, `STOP_POS` b16, `GATE_CTRL` b8; release opens; with the
  CPU running the stop is at the first boundary ≥ c.
- `SB_LOAD` b3/b4: staged CIA words load at the release; refused while running (`SB_STATUS` b1).
- Turbo: no markers and no CPU records while engaged, REU records still; back at 1 MHz markers resume.
- S38 `goto` with the log on: the changed bytes are outside records.

**Firmware smoke** (`scripts/smoke-trx-ring.sh`): `run/trx/ultimate-a58f1a6e.elf`, `--board c64u --trx-io`, a fresh
flash with the ROMs, System Mode PAL, the overlay interface, `--sd` an image with room, `--net user`
`--hostfwd tcp:127.0.0.1:8080:80`:

| Step | Expected |
|---|---|
| boot, 8 s | console `[RING] log on: streams at 7000000/6000000/5c00000` |
| `10 poke1024,peek(1024)+1:goto10`, run 5 s | `GET /v1/timewarp:status`: frames ≈ 250, targets = frames, `log_cnt` rising, `dropped` 0 |
| `PUT /v1/timewarp:rewind?frames=100` | 200; `$0400` (`monitor m 0400`) back by about 100 frames' worth, then counting on |
| `PUT /v1/timewarp:scrub?frames=50`, then `scrubend` | console `[RING] scrub to seq …`; the still (S37) changes; after `scrubend` the loop runs on from that frame |
| freeze button, "C64 Machine" → "Timewarp", LEFT × 25, RETURN | the bar shows; each LEFT one scrub line on the console; RETURN closes the menu and the loop runs from 0.5 s back |
| `PUT /v1/timewarp:ringdump?path=/SD/t.c64rering&seconds=5&stride=25` | 200, entries ≈ 10; the file (taken out of the SD image) passes `check_board_ring`; `monitor ringload` of it in a fresh UE2 shows the board notes and `goto` works |
| `PUT /v1/timewarp:dump?path=/SD/t.c64re`, change `$0400`, `PUT /v1/timewarp:load?path=/SD/t.c64re` | `$0400` as at the dump; the ring restarted |
| `monitor ringdump t2.c64rering board` (UE2's own anchors) | passes `check_board_ring` |
| speed menu to 48 MHz for 2 s, back to 1 MHz | no markers in turbo (`FRAME` stands), markers after |

With S39 and the `t21-p4-app` ELF and `trxmon.u2a`: `rstep`, `goto -50`, `checkpoint/list` over RPC answer from the
ring; phase E: `trace status` present, `chis 10` lists ten instructions equal to UE2's `monitor chis`.

**Not checked by the smoke:** the board loading UE2's files (Alex, on the board), cycle-exact `STOP_POS` with a
running CPU (T2), the transitional CIA states (Q3).

## 15. Size

About 4,200 lines with tests: `trx_engine.rs` (registers, store, markers, blocks, mover, APPLY) 700; `engine_tap.rs`
(taps, mirror, boundaries, CPU-held run to a position) 600; `trx_ff.rs` (6502N 250, reDIP-CIA 500, side table 150)
900; page 0/1 additions and the position hold 250; stream 3 250; S38 board writer and transport records 150; unit
tests 1,200; smoke script and fixtures 150. Phases A-E need no TRX64 change; F waits for TRX64.
