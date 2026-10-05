# S39 — trxmon in UE2: the debug block served from TRX64

**Status:** built (2026-10-05), local branch `s37-trx-pixel-layer`: phases A1-E against t21-p4 a85f741 (with the D-page
fix df6d71c, the OWNER block rule 3302c85) and t21-p4-app, smoke-tested with the app build 165c22fe; as built: §11. Phase F waits for TRX64 (§5 T1, T2). Updated the same day from the FPGA
session's answers (`run/trx/s39-answers.md`, t21-p4 RTL and app; they override this text where they differ) and S40.

**Why:** the TRX64-Ultimate firmware session must test trxmon (the T21 monitor app) in UE2 as on the board: the
monitor screen on the overlay (T21 §8), opened from the task menu or Ctrl-O and reopened by `trx->ui_request` when a
stop fires, and every verb giving real data. UE2 runs the unmodified firmware and the unmodified `trxmon.u2a`. It
presents the FPGA's register interface that trxmon uses (pages 0, 1, 3, 5) and serves it from TRX64 (`trx64-core`,
pinned v0.12.5).

**Reads** (code and RTL are the truth; comments are not evidence):
- RTL, TRX64-Ultimate branch **`t21-p4`** (fa09f06, **not merged into main**): `designs/c64_basic/trx_sys.v`,
  `trx_dbg.v`, `drv_dbg.v`, `freeze_resp.v`, `c64_core.v`, `trx_io.v`, `top.v`. Line numbers below are of that branch.
- App, `software/ultimate-app` branch **`t21-p4-app`** (ec4a7327, **not merged into `trx64/main`**; `t21-p4` pins
  it): `apps/trxmon/src/{hw_trx.c, hw.h, mach.c, main.c, mon.c, rpc.c, ring.c, trace.c, ui.c}`,
  `software/io/c64/trx_lens.c`, `software/app_loader/{app_api.h, app_api_net.cc, app_registry.cc}`,
  `software/io/c64/{trx_io.cc, trx_ui.cc, trx_lens_fw.cc, c64.cc}`.
- Specs (for intent only): `docs/specs/T21-monitor.md` (t21-p4), `T12-freeze-overlay.md` §14.8,
  `ARCH-lean-state-access.md`.
- UE2: S37, S38, `crates/ue2-core/src/devices/{trx.rs, trx_session.rs, c64.rs, drives.rs}`, `crates/c64-bridge/src/lib.rs`,
  `crates/ue2emu/src/monitor`.

**Owns:**
- `crates/ue2-core/src/devices/trx.rs`: pages 0 (0x104-0x13F), 1, 3 and page 5's `LK_REQ`/`LK_RES`.
- `crates/ue2-core/src/devices/trx_dbg.rs` (new): `trx_dbg.v` and `drv_dbg.v` as a model: commands, the stop record,
  the bitmap, the two watch units, the counters, the drive comparator.
- `crates/ue2-core/src/devices/c64.rs`: OWNER and the debugger's hold in the gate, the shadows, the capture.
- `crates/ue2-core/src/devices/trx_session.rs`: the debugger's stop as a second session cause.
- `crates/c64-bridge`: the stop sources on TRX64's run loop, steps, the CPU write, the drive's comparator and step,
  the shadow words, the class look.
- `crates/ue2emu/src/monitor`: `monitor trx` shows pages 1 and 3; run control refused while trxmon owns the C64.

## 1. What trxmon touches

trxmon reaches the block only through `hw_trx.c` and `trx_lens.c`. Every register below is one of theirs; nothing
else in the app touches pages 0-7. All 32-bit registers follow `trx_sys.v`'s rule (trx_sys.v:281-292): bytes 0-2
written are collected, byte 3 commits the word; reading byte 0 runs the read and latches the word, bytes 1-3 come
from the latch. A commit answers 0, which the latch then holds (trx_sys.v:347, :387). Pages 0 (from 0x100), 1, 2,
3, 5, 7 share one collect buffer and one latch (trx_sys.v:194-195). S37 built this rule; S39 adds registers to it.

### 1.1 Page 0

| Off | Name | trxmon | RTL |
|---|---|---|---|
| 004 | `CAPS` | read through `trx->caps` (`hw_init` needs b3) | trx_io.v:231 |
| 005 | `CAPS2` | b2 (`hw_has_cpu_write`), b3 (`hw_session_read`), b4 (`hw_has_lk`) | trx_io.v:232 |
| 100 | `GATE_CTRL` | not by trxmon; the firmware's freeze (S37) | trx_sys.v:218, :316 |
| 118 | `OWNER` | W b0 (`hw_owner`, and `trx->acquire`/`release`) | trx_dbg.v:334; masks C64_STOP: top.v:1072 |
| 120 | `CYCLES` | R; the byte-0 read latches bits 47:32 into `CYCLES_HI` | trx_dbg.v:296, :331, :393 |
| 124 | `CYCLES_HI` | R | trx_dbg.v:394 |
| 128 | `INSTRS` | R: IBs passed | trx_dbg.v:297-298, :395 |
| 130 | `CYC_CMP` | W: arms, one shot | trx_dbg.v:165, :307, :335 |
| 134 | `CYC_CMP_EN` | W 0: disarm | trx_dbg.v:336 |

`CYCLES` counts every phi2 falling edge the gate lets through: runs, steps and the window's bus cycles while halted
(trx_dbg.v:296, edge = `ce && phi2n`). It is not reset by a C64 reset. Write-only registers read 0
(trx_dbg.v:390-404).

### 1.2 Page 1, the state bus

| Off | Name | trxmon | RTL |
|---|---|---|---|
| 000 | `SB_ADDR` | W `{chip b11:7, word b6:0}` | trx_sys.v:319 |
| 004 | `SB_DATA` | R: the word, then `SB_ADDR` + 1 (a block read of n words). W: staged word, then + 1; only while the gate is shut, else `SB_STATUS` b1 | trx_sys.v:320-326, :361-367 |
| 008 | `SB_STATUS` | R b1 refused; W clears b1 (`hw_cpu_write`) | trx_sys.v:226, :335 |
| 00C | `SB_LOAD` | W b1: the CPU's staged words load at the next opening edge. R: still armed (`hw_cpu_write_pending`) | trx_sys.v:224, :327; c64_core.v:402-403 |

The words trxmon and `trx_lens.c` read (c64_core.v:600-629, :742-780):

| Chip.word | Contents | Reader |
|---|---|---|
| 0.0 | b0 halted, b1 GAME, b2 EXROM, b3 cartridge present, b4 held, b31:16 capture count | `lens_snapshot` |
| 1.0 | `{S, Y, X, A}` | `hw_cpu` |
| 1.1 | `{sync b24, P b23:16, PC}` | `hw_cpu` |
| 1.2 | `{$01 as read, $01 latch, $00}` | `hw_cpu`, `lens_snapshot` |
| 1.3 | `{IB b24, opcode at IPC b23:16, IPC}`, live | `hw_cpu` |
| 1.4, 1.5 | `{S, Y, X, A}`, `{P}` after the CPU's pending update (the settled registers), live | `hw_cpu` with CAPS2 b2, gate shut, at an IB |
| 1.8-1.14 | the 6502N's 209 flip-flops; trxmon uses A 7:0, X 15:8, Y 23:16, S 31:24, P 39:32, PC 55:40, IR 87:80, proc_control from 102 (+12 `copy_d2p`, +15 `flags_update`, +16 `trigger_upd`, +18 `intg`), `din_r` 137:130 | `hw_cpu_write` (read-modify-write), `hw_cpu_fetch` (ring land only) |
| 2.0-2.11 | VIC `$D000-$D02F` as a read returns them, without the read's side effect | `lens_snapshot` |
| 3.0-3.3, 4.0-4.3 | CIA 1, 2 registers 0-15 as a read returns them | `lens_snapshot` |
| 3.4, 4.4 | `{DDRB, DDRA, PB out, PA out}` | `lens_snapshot` (CIA 2: the VIC bank) |
| 7.0 | `{reg_1, reg_0}` of icepi's cartridge model (meaning per type, `c64_cartridge.v`) | `bank` verb (`reg_0 & $7F`) |

**When the words move.** Words 0.0 (but b0), 1.0-1.2, 1.8-1.14, 2.x, 3.x, 4.x, 7.0 are shadows: loaded one clock
after a frame's snap when a capture is due, and on every clock while the gate is shut (c64_core.v:660-664,
:696-706). While the machine runs without captures they keep the state of the last halt. 0.0 b0, 1.3, 1.4, 1.5 are
live (c64_core.v:746-749, :774).

### 1.3 Page 3, the debug block

| Off | Name | trxmon | RTL |
|---|---|---|---|
| 000 | `CMD` | 1 PAUSE_CB, 2 PAUSE_IB, 3 RUN, 5 STEP_INSTR, 6 STEP_CYCLES, 0x10 DRV_PAUSE (not used), 0x11 DRV_RUN (not used), 0x12 DRV_STEP, 0x20 CLEAR_STOP | trx_dbg.v:340-363; drv_dbg.v:55-61 |
| 004 | `ARG` | W step count, 0 = 1 (written before CMD) | trx_dbg.v:337, :374; drv_dbg.v:53 |
| 008 | `STATUS` | R b0 gate shut, b2 at an IB, b3 STOP_NEW, b4 held by DMA, b5 C64_STOP, b6 hold off but still shut, b7 step running or requested, b11:8 REASON, b12 RUN/step requested, b16/b17 units on, b18 bitmap on, b19 CYC_CMP armed, b20 OWNER, b31:24 stops | trx_dbg.v:396-397 |
| 010 | `STOP0` | `{POS 31:16, DATA, FLAGS 3, UNIT 1, REASON 4}` | trx_dbg.v:319-328, :398 |
| 014 | `STOP1` | `{ADDR, IPC}` | trx_dbg.v:399 |
| 018 | `IPC_SET` | W `{IB b24, opcode, IPC}` after a CPU redirect | trx_dbg.v:301-306 |
| 040 | `BMP_IDX` | W | trx_dbg.v:338 |
| 044 | `BMP_DATA` | W the word, then `BMP_IDX` + 1 | trx_dbg.v:173-182, :333 |
| 048 | `DBG_CTRL` | W b0 bitmap on | trx_dbg.v:339 |
| 080 + 0x20 n | unit n: CTRL, RANGE, DATA, REG, PASS, HITS-clear | `hw_unit`, `hw_unit_off` | trx_dbg.v:195-248 |
| 0B4 | `HITS` | not used by trxmon | trx_dbg.v:401 |
| 0C0 | `DRV_CMP` | W `{type b17, enable b16, address}`; trxmon writes exec only | drv_dbg.v:54, :73-74 |
| 0C4 | `DRV_STAT` | R `{DPC 31:16, b3 comparator on, b2 step, b1 held, b0 halted}` | drv_dbg.v:123 |
| 0C8 | `DCYCLES` | R | drv_dbg.v:124 |

**Rules that trxmon depends on** (trx_dbg.v):
- **IB** = the phi2 falling edge that ends an opcode fetch the CPU completes (sync, RDY, CPU on the bus) with no
  interrupt decided (`intg` 0) (:152). A fetch replaced by an interrupt is no IB; the next IB is the handler's first
  opcode. IPC, the opcode and the IB flag are taken at every IB (:297-305).
- **Stops close the gate on the edge they see** (`brk`, :268; c64_core.v:368-369): exact, no stop is late.
  Sources: watch unit stop actions, bitmap hit at an IB, `CYC_CMP` (the edge that passes while `CYCLES[31:0]`
  equals it, :165), a step's last counted edge, PAUSE_IB at the next IB, PAUSE_CB at the next edge, the drive
  comparator (at the next edge).
- **The stop record** is written only while STOP_NEW is 0 (:285, :319-328). PAUSE_CB on a shut gate sets STOP_NEW
  and REASON 1 only, and leaves the other fields as they were (:344-345).
- **RUN, STEP_INSTR, STEP_CYCLES are requests** (t21-p4): STOP_NEW clears at once; the release or the step starts
  only when `freeze_resp` reports no session and no commit (`sess_busy`, :365-381). Steps need the gate shut and no
  step running (:355). STEP_INSTR counts IBs, STEP_CYCLES counts edges (:258).
- **Every stop raises `IRQ_CAUSE` b0** (`ev`, :295; top.v:1328), as the pause does.
- **Watch unit match** (:229-235): type 1 IB, 2 read (`cyc && !we && !sync`: operand and dummy reads count, opcode
  fetches do not), 3 write, 4 read or write, 5 every edge; value = address or `{line, cycle}` (b4); range, data mask,
  register qualifier against the registers of the cycle that ends at the edge (on an IB edge that is before the
  previous instruction's write-back), PREV_RET (opcode at the last IB was RTS/RTI), PASS against HITS, chain,
  one-shot (:212). Actions: stop, count, log (engine stream 1).
- **trxmon's own use** (mach.c): the bitmap for `bk` and exec observers; unit type 2/3 over a range for load/store
  observers; `until` = unit 0 type IB one-shot; `n` = unit 0 IB at the return address with S >= S0, unit 1 any IB
  with S >= S0 + 1; `ret` = unit 0 IB, PREV_RET, S >= S0 + 1. No raster, every-edge, count, log, chain, PASS or data
  mask.

### 1.4 Page 5, the lens unit (`freeze_resp.v`)

| Off | Name | trxmon | RTL |
|---|---|---|---|
| 004 | `SH_STATUS` | b0 a session runs (`hw_session_read`); b2 the session is the debugger's stop | trx_sys.v:233 |
| 020 | `LK_REQ` | W `{REAL b18, class b17:16, address}`, ignored while busy | trx_sys.v:337; freeze_resp.v:581-586 |
| 024 | `LK_RES` | R `{busy b31, refused b12, real b11, ext b10, hit b9, done b8, data}`; polled until b31 is 0 | freeze_resp.v:234 |

- **Look** (REAL 0): the cartridge model at the address as class 0 ROML, 1 ROMH, 2 IO1, 3 IO2, whatever the PLA maps;
  no write, no bus cycle, also while running. No hit: data `$FF`. A cartridge on the physical port: `ext`, data `$FF`
  (freeze_resp.v:750-783).
- **REAL**: one bus cycle with the select forced, the app's Ultimax off for it, only while halted; else `refused`
  (freeze_resp.v:599-618, :785-795).

### 1.5 Not on the block

- **RAM**: the DMA window `0x10050000` with `C64_DMA_MEMONLY` (`0x10180003`) set around the access
  (`memonly_on`). UE2 has both (S14, S28).
- **ROMs**: the apertures `0x10188000` (BASIC, KERNAL) and `0x1018C000` (char). UE2 has them.
- **Drive A**: DDR `0x0EE0000`, RAM mirrored below `$1800`, the VIAs `--`. UE2 mirrors the drive's RAM there every
  1 ms (drives.rs `tick`) and keeps the ROM there; exact while the drive stands.
- **I/O writes and old-bitstream sidefx reads**: the window without MEMONLY, only when STATUS b0 and b5 (C64_STOP)
  are set (`bus_ok`). In a session that access is answered by the responder (S37 B2).
- **Colour RAM**: `hw_session_read` = the window without MEMONLY, with CAPS2 b3, `SH_STATUS` b0 and STATUS b0. It
  goes through `hw_bus_read`, which also needs b5. On t21-p4 as built a `$Dxxx` window access without C64_STOP never
  reaches the responder (`ultimate_sys.vhd:914-920`: reads `$FF`, writes dropped; answers 5, 6). Alex decided
  (2026-10-05) that the RTL gets fixed: D-page accesses go to the session while a debugger stop holds. UE2 models the
  fixed behaviour when it builds that path (phase C).

## 2. CAPS and CAPS2

**`CAPS`** grows with the pages built: `$E7` with phase A1 and S40 (b0 gate, **b1 state bus**, b2 engine (S40), b5
session, b6 video, b7 RGB565), **`$EF`** with phase A2 (**b3 debug**, built). **`CAPS2`**: `$29` with S40 (b0 F4 order,
b3 commit list, b5 APPLY), **`$3D`** with phases B/C (**b2 CPU write path**, **b4 lens unit**, built). A bit is reported only
when its pages are built (answer 3: the app reads none of the others; b1 BUILD only the boot ROM).

The board reads `$FF` / `$BF` (trx_io.v:231-232). Each bit UE2 reports turns on firmware and app paths:

| Bit | Turns on | UE2 |
|---|---|---|
| CAPS b1 state bus | trx_ui.cc `have_pc`: the splash waits for READY by state-bus PC with `CAP_CTRL` captures (page 0 0x104) instead of 3 s; trx_lens_fw.cc `trx_lens_present`: REST `machine:readmem?lens=` | report; build `CAP_CTRL`/`CAP_STAT` and the snap capture (§3.2) |
| CAPS b3 debug | `app_trx_table()` non-NULL (the `trx` ABI table, so trxmon starts); `trx_dbg_yield` in `C64::stop`/`hard_stop` reads page 3 and gives OWNER back | report |
| CAPS2 b2 | trxmon's `r a=…`, `g <addr>`, settled registers (w4/w5) | report |
| CAPS2 b4 | the `cart` lens and IO1/IO2 through the look (trxmon, REST lens) | report |
| CAPS b2 engine | `trx_ring_init` (the ring, page 2, DDR streams), `trx->ring` non-NULL, Timewarp UI, `/v1/timewarp` | **on**: S40 serves the engine (Alex, 2026-10-05) |
| CAPS b4 cartridge | trx_cart.cc page 4 write-back and descriptor | off (UE2 runs Gideon's cartridge registers) |
| CAPS2 b7 trace | `trx_ring_trace_present` needs the ring as well | on with S40 phase E (built) |
| CAPS2 b0, b5 | engine F4 order, APPLY | on with S40 |
| CAPS2 b1, b6 | BUILD, T29 turbo | off (no reader in either app; no turbo on `main`) |

**The ring and trace verbs** (answer 1): `trx->ring` needs CAPS b2, which S40 now serves, so trxmon's ring verbs
(`rstep`, `checkpoint/*`, land, commit) answer through the engine once phases A2/B are built. The trace verbs need
CAPS2 b7 (t21-p4 trace former, S40 phase E); until then they answer the board's own "needs the instruction trace".

`trx->video` works without the engine (app_api_net.cc `api_video` reads the System Mode). `GET /v1/info` and the
ident then say `caps "0x000000EB"` and `rpc 4312` while trxmon serves.

S38 §7 ("UE2 does not fake them") changes for b1, b2 and b3: they are served, not faked.

## 3. How UE2 serves each register

### 3.1 The gate and the debugger's hold

The RTL has one gate (c64_core.v:354): the machine runs when no `halt_req`, no `pos_hold` and no `dbg_hold`.
`halt_req` = JTAG or `C64_STOP & FRZ & !OWNER` (top.v:1072-1073). The CPU-only pause is `C64_STOP & !FRZ & !OWNER`
(top.v:1237). The mute stays `halted | C64_STOP` with the raw C64_STOP (top.v:1059; S37 "the mute").

UE2 (`C64Port`):
- **OWNER** masks C64_STOP for both the pause and `Hold::Cpu`. C64_STOP still reads `req | req<<1` (S14) and still
  mutes.
- **The debugger's hold** stands the whole machine with the bridge's pause (S37 `set_paused`), the same mechanism as
  FRZ: CPU, VIC, CIAs, SID, drive A stand; the DMA window, keys and ROM apertures are served.
- **halted** (`GATE_CTRL` b8/b9, chip 0 b0, STATUS b0) = the pause or the debugger's hold in effect.
- **RUN** resumes at once (S37 B1 accepted this for the freeze); STATUS b6 is never seen.

### 3.2 The shadows and the capture

UE2 keeps a shadow set with the RTL's words (chips 0, 1, 2, 3, 4, 7), built from TRX64 at each of these points:
- whenever the gate is shut: at the stop, after every window access, step, CPU load or session commit while halted;
- at a capture: with `CAP_CTRL` = n ≠ 0, on every n-th frame at the first instruction boundary on line 0 from cycle 12
  (S38's frame boundary stop is the same point). `CAP_STAT` low half counts captures and halts (c64_core.v:692), the
  high half the frames that had no snap. `IRQ_CAUSE` b3 is raised at both (trx_io.v cause b3).

Between those points the shadows hold, as on the board. Built from TRX64:

| Word | From |
|---|---|
| 0.0 | `halted` (live), `cart_lines()` (present, EXROM, GAME), held = 0 (§5 T8), capture count |
| 1.0, 1.4, 1.5 | TRX64's `reg_a/x/y/sp`, `status()`. TRX64 has only the settled registers, so 1.0 = 1.4 (§6) |
| 1.1 | PC = TRX64's `reg_pc` (= IPC at an IB, answer 4; at a snap the fetched opcode's address); P; sync = 1 (UE2 always stands at an opcode boundary) |
| 1.2 | `$00` DDR, `$01` latch, `$01` as read (pins for the bits not driven) |
| 1.3 | the model's IPC, opcode, IB flag (§3.3), live |
| 1.8-1.14 | a synthesized image: A, X, Y, S, P, PC = IPC, IR = opcode at IPC, proc_control state = decode, `intg` 0, `din_r` = opcode; every other bit 0 (§3.4) |
| 2.0-2.11 | `io_peek($D000+r)` for r < 48: no IRQ latch, collision or ICR cleared |
| 3.x, 4.x | `cia_regs`, the TOD as a read shows it (latched or running); w4 the CIA's port outputs and DDRs |
| 7.0 | the cartridge logic's banking registers mapped onto `c64_cartridge.v`'s `reg_0`/`reg_1` per type (§7 Q10). Without a mapping: 0 |

Other chips and words read 0 (c64_core.v:772-781).

### 3.3 The debug model on TRX64

`trx_dbg.rs` holds the registers and the stop logic; the bridge runs TRX64 and reports the events. TRX64 runs whole
instructions (`run_for_full_capped_dbg`, `execute_one`), so UE2's edges are TRX64's instruction boundaries:

| RTL | UE2 on TRX64 |
|---|---|
| IB edge | a TRX64 boundary whose next `execute` takes no interrupt. UE2's IB state = TRX64 before the instruction at `reg_pc`: IPC = `reg_pc`, opcode = the byte there (peek through the PLA), IB flag 1. TRX64's registers are then the settled ones (w4/w5) |
| `CYCLES` | UE2's own 48-bit counter: + the cycles of each run and step (TRX64 `clk` deltas), + 1 per window bus cycle while halted. At an IB stop it reads like the RTL: the opcode fetch counted (TRX64 `clk` − base + 1). A ring restore (S38 `goto`) does not move it |
| `INSTRS` | + 1 per retired instruction (`on_instruction`); interrupt sequences do not count |
| bitmap stop | the `exec_watch` table (TRX64's own break-before-execute), skipped at a boundary that takes an interrupt (the RTL's dummy fetch is no IB) |
| unit, type 1 IB | evaluated at each UE2 IB against IPC, the opcode (data) and the settled registers; PREV_RET from the last IB's opcode |
| unit, type 2/3/4 | the `access_watch` table over the range, `on_access` with value and registers; the stop lands at the **next** IB (§5 T1). Record: REASON 5, ADDR and DATA of the access, FLAGS b0 = 1 |
| unit, type 5, raster source | refused: the unit stays disabled and `monitor trx` says so (§5 T1). trxmon does not use them |
| unit actions count, log | count: HITS only. log: HITS only (no engine stream) |
| `CYC_CMP` | the run's cycle budget ends at the first boundary at or after the cycle; the record says REASON 7 and, if that is past the asked cycle, `CYCLES` shows where it stood (§5 T1). trxmon's `session/run` then finds the stop at an IB and needs no extra step |
| PAUSE_IB | stop at the next IB |
| PAUSE_CB | the same as PAUSE_IB (§5 T1); REASON 1; on a shut gate only STOP_NEW and REASON 1, as trx_dbg.v:344-345 |
| STEP_INSTR n | n instructions with the hold kept, stop at the n-th IB after them; REASON 2. An interrupt taken on the way: §5 T2 |
| STEP_CYCLES n | rounded up to whole instructions; REASON 2 (§5 T1) |
| RUN | commit the session list (§3.6), clear the hold, run on |
| `IRQ_CAUSE` b0 | at each stop and each record, as trx_dbg.v:295 |
| `held` (STATUS b4, FLAGS b2) | 0 (§5 T8) |

**The stop record** is written as trx_dbg.v:319-328: only while STOP_NEW is 0; POS = `{raster line, cycle in the
line}` at the stop (TRX64's VIC position); ADDR = the access address for unit stops, else IPC; DATA = the opcode at
an IB, the access's byte for unit stops.

**Ordering.** Commands are applied when their byte 3 is written, at the C64's current cycle (the C64 runs up to the
access first, as S37 does for `GATE_CTRL`).

### 3.4 The CPU write path

- **Settled words** 1.4/1.5 are TRX64's registers (§3.2).
- **Staging**: `SB_DATA` writes to chip 1 words 8-14 are kept while `SB_STATUS` b0 (refused otherwise, b1).
  `SB_LOAD` b1 arms the load; it reads armed until the next opening (RUN or a step), as c64_core.v:402-403.
- **The load**, at the next opening, before TRX64 runs: a staged image UE2 made itself (an S40 block) loads its kept
  TRX64 parts (S40 §5.3); otherwise A, X, Y, S, P from bits 0-39 into TRX64; if the PC field
  differs from the served one (IPC), `set_pc(PC field)`: TRX64 fetches the opcode there itself (`din_r` is what the
  RTL's update takes; trxmon writes the byte at that address, so they agree unless it is I/O, which trxmon refuses).
  IR, `copy_d2p`, `flags_update`, `trigger_upd` are not used: TRX64 has no pending write-back to take out. Then
  `sync_after_monitor`.
- **`IPC_SET`** sets IPC, opcode and the IB flag at once (trx_dbg.v:301-306); STATUS b2 and word 1.3 follow.
- CIA staging (chips 3, 4) and `SB_LOAD` b3/b4 are the engine's restore (S40 §6.3, built with phase A1).

### 3.5 The drive

| RTL (drv_dbg.v) | UE2 |
|---|---|
| `DRV_CMP` exec at the drive's IB: hold the drive, stop the C64 at its next edge (REASON 8, FLAGS b1) | the drive's PC per drive instruction (§5 T5); the C64 stops at its next IB |
| `DRV_CMP` write | refused: disabled, said in `monitor trx` (§5 T5). trxmon does not write it |
| DRV_STEP n: the drive alone, the C64 stays; record REASON 9 | the drive runs n instructions while the C64 stands (§5 T5) |
| DRV_PAUSE, DRV_RUN, RUN releases the drive | the drive's own hold beside the machine's |
| `DRV_STAT` DPC, comparator on, step, held, halted (halted = the machine's pause or the drive's hold) | from TRX64's drive CPU |
| `DCYCLES` | drive CPU cycles UE2 counts |

### 3.6 The debugger's stop is a session (T12 §14.8)

On t21-p4 a debugger stop starts the freeze session (freeze_resp.v:209-224): cause `dbg_stop` = hold, halted, no
step; `SH_MODE` 1 or 2, no C64_STOP and no Ultimax needed. It lasts while the hold does and no RUN or step is
requested. At its end the list is committed as bus cycles before the machine moves. `SH_STATUS` b2 says the session
is the debugger's. `block` is set at every end of such a session (:888) and clears when `dbg_stop`, `halt_req` and a
commit are gone and C64_STOP is off or OWNER is set (:550, 3302c85, T12 §14.8 point 10): while the debugger owns the
machine a foreground app's C64_STOP, held for the whole run, does not take the next stop's session. `cond` and
`keep_app` still take the raw C64_STOP, so an app's Ultimax freeze while OWNER is set stays the app's session.

UE2's `Session` (S37 B2) gets this second cause. The window's non-MEMONLY accesses in the stop are answered as in a
freeze (VIC, CIA, colour RAM, the cartridge look; SID `$FF`); writes go to the overlay and the list (PROTO is not
set). RUN and the steps run `end` (the list as TRX64 banked writes, S37) inside the command's access, then release
or step, so STATUS b12 is never seen. `SIDEFX` (SH_CTRL b2) makes I/O accesses real, as in a freeze.

This is t21-p4 only. On `main` a debugger stop has no session and a window read in the stop is a bus cycle with its
side effects. UE2 builds t21-p4 (§7 Q2).

### 3.7 The lens unit

- **Look**: a class-aware peek of UE2's cartridge logic and the port's devices (the UCI, the REU, the sampler): ROML
  or ROMH as the class says at that address whatever the PLA maps, IO1/IO2 as `cart_peek` answers them today.
  `CartPeek::External` (a physical cartridge in `--cart-slot`) is `ext`. `cart_peek` takes only an address now; the
  class form is new (§5, UE2 gap U1).
- **REAL**: only while halted, else `refused`. One access to the physical slot with the select forced, through
  `C64CartSlot` (its side effects are the cartridge's own).
- Answers come at once: `busy` never reads 1.

### 3.8 One owner of halt and step

| Who | Mechanism | While trxmon owns (OWNER) |
|---|---|---|
| the firmware (menu, DMA load, REST pause) | C64_STOP; `C64::stop`/`hard_stop` call `trx_dbg_yield` first (OWNER 0, RUN) | as on the board |
| the freeze | FRZ + C64_STOP (S37) | the firmware yields first, so it is the freeze |
| trxmon | page 3 hold, steps, RUN | owner |
| UE2's monitor: `c64 halt/go/step`, `g`, `z`, `n`, `until`, `ret` | C64_STOP and out-of-band runs (run.rs) | **refused**: "trxmon owns run control (OWNER); use it, or /quit it". Reading verbs work |
| S38 transport: `goto`, `frame`, `ringload`, `rstep` | the transport hold | **refused**, same sentence |
| S38 rings | record while the C64 runs at 1 MHz | runs and steps of trxmon record; stops stand |
| an OWNER write while the transport holds an anchor | | releases the transport (S38 §4 release: cut after the anchor); the C64 stays where it is, now held by the debugger |

The VICE binary monitor port (S23) follows the same refusal for its run commands.

## 4. The screen and the app around it

Nothing new is needed in UE2 for the screen itself:
- `screen_open_full` draws on Gideon's overlay; with `--trx-io` UE2 draws it as `overlay.v` does (S37 B3), and the
  running layout's transparent rows (TRANSPARENCY `$C0`, colour 0) show the C64 at half brightness, as on the board.
- `trx->ui_request` needs `app_ui_overlay_ready()`: the overlay interface ("Overlay on HDMI"), which UE2 seeds into a
  blank flash (S37 §5).
- The app loader runs `.u2a` files from the SD/USB images already (docs/status/cart-slot.md: `trex_crt.u2a`); "Run
  App" registers trxmon in `/Flash/config/apps.json` and its task under Developer.
- RPC: the firmware's lwIP on `--net user`, `--hostfwd tcp:127.0.0.1:4312:4312`.
- The 80-column screen (T21, decision 2026-10-05) needs an `overlay.v` mode and an ABI call that do not exist yet
  (§7 Q13). UE2 adds them to S37 B3 when the RTL has them.

## 5. TRX64 gaps (and UE2's own)

| # | Gap | Effect in UE2 until it is closed |
|---|---|---|
| T1 | **No stop or step at a cycle boundary** on the full machine: `run_for_full_capped_dbg` and `execute_one` run whole instructions; `on_access` halts are honoured at the next boundary. Needed: run to cycle c and stand mid-instruction, resume from there | PAUSE_CB, STEP_CYCLES (`cs`), `CYC_CMP` and load/store stops land at the next IB; `CYCLES` shows the overshoot; "mid-instruction" never appears; raster and every-edge units refused |
| T2 | **The interrupt sequence and the handler's first instruction are one `execute`** (`c64_6510core_execute`: `do_interrupt`, then the fetch). Needed: return after the sequence (an IB at the handler's first opcode), and a predicate "the next execute takes an interrupt" | a bitmap bit at a handler's first opcode does not stop; `z` across an interrupt runs the handler's first instruction too |
| T3 | **No pre-write-back registers**: TRX64 has the settled registers only | word 1.0 = 1.4; a unit's A/X/Y/P qualifier at an IB sees the settled value where the RTL sees the old one (§7 Q7) |
| T4 | **`on_access` coverage**: the RTL's read type counts operand fetches and dummy reads, not opcode fetches. To confirm in TRX64 that `on_access` sees every CPU read and write cycle | a load observer may miss a dummy read |
| T5 | **The drive alone**: no drive hold of its own beside the C64, no drive-only run of n instructions with the C64 standing (`catch_up_to` pairs the clocks), no per-instruction drive hook (`sample_pc_change` samples), no drive write hook | `bk` under `device drive8` stops one drive instruction late at worst; `z` under `drive8` needs the pair to absorb the drive's lead; the write comparator refused |
| T6 | **No 6502N flip-flop image** (by design: another CPU) | words 1.8-1.14 synthesized (§3.2); `hw_cpu_fetch` (ring land) not reached |
| T7 | **Whole-machine hold** (S37, S38 gap 6) | the bridge's pause, as in S37 |
| T8 | **No RDY park by bus DMA** (`held`) | STATUS b4, FLAGS b2, chip 0 b4 read 0 |
| U1 | UE2: `cart_peek` has no class (ROML/ROMH forced) | to add with §3.7 |
| U2 | UE2: no mapping of its cartridge registers onto `c64_cartridge.v`'s `reg_0`/`reg_1` | word 7.0 reads 0 for types without one (§7 Q10) |

T1 and T2 decide whether trxmon's stops are exact in UE2. Everything else is exact or a documented approximation.

## 6. Differences from the board that stay

- RUN resumes at once, no frame alignment (S37).
- Word 1.0 is the settled registers (T3).
- `CYCLES` counts what TRX64 ran; the board's window bus cycles while halted are counted the same way, the frame
  alignment's left-out edges are not counted on either.
- Answers of LK and of commands are immediate (no busy, no request bit).
- The drive image at `0x0EE0000` follows the drive within 1 ms while it runs.

## 7. The FPGA session's answers (2026-10-05, `run/trx/s39-answers.md`)

1. trxmon without the engine: the ring/checkpoint verbs answer "no frame ring", the trace verbs `NO_TRACE`. Now moot
   for the ring: S40 serves the engine (§2).
2. t21-p4 is not merged; the register map of pages 0/1/3/5 has no announced change; pending items are app-side, plus
   the Q5/Q6 RTL fix, T29 and T55. CAPS2 numbering is per build.
3. CAPS2 `$1C` is valid; each bit gates only its own path (§2).
4. PC at an IB is **IPC**, not IPC + 1 (word 1.1 and the PC field of words 8-14). T21-monitor.md:165-166 is wrong.
5. / 6. RTL gap on t21-p4: a `$Dxxx` window access without C64_STOP never reaches the session. Alex: fix the RTL;
   UE2 models the fixed behaviour (§1.5).
7. trxmon does not rely on the qualifier lag; UE2 compares against the settled registers.
8. / 9. Confirmed as §3.3 says (bitmap at the interrupted PC; STEP_INSTR across an interrupt).
10. Word 7.0 has no fixed per-type contract in the RTL: `reg_0`/`reg_1` are the descriptor's registers, the bytes the
    program wrote (masked). trxmon prints `reg_0 & $7F`.
11. The splash is the only firmware user of `CAP_CTRL` without the engine; `CAP_STAT` has no reader; `IRQ_CAUSE` b3
    has no handler.
12. No lock on t21-p4: one shared collect buffer and latch, a known race. UE2 keeps the one buffer and latch.
13. 80 columns: no RTL change needed (`overlay.v` `r_cpl` is 8 bits); the ABI call is missing.
14. No firmware handler acts on `IRQ_CAUSE` b0.
15. LK REAL in a debugger stop: allowed as built (needs the gate shut only).

## 8. Phases

| Phase | Content |
|---|---|
| A1 | Page 0 0x104-0x117 (`CAP_CTRL` RW, `CAP_STAT`, `RESUME_PHASE`, `STOP_POS`), page 1 with the shadows (captured at the snap and while the machine is halted) and the staging of chips 1, 3, 4; `SB_LOAD`; CAPS b1. One model with S40 (`trx_sb.rs`). **Built with S40.** |
| A2 | Page 0 0x118-0x134, page 3 registers and `trx_dbg.rs` without a CPU (pure register model); CAPS b3; OWNER and the hold in `C64Port`; `monitor trx` for page 3 |
| B | Stop sources and steps on TRX64 (§3.3): IB, bitmap, units (IB, read, write), CYC_CMP, pauses, steps, RUN; the stop record; `IRQ_CAUSE` b0; the CPU write path (§3.4) |
| C | The debugger's session (§3.6); the lens unit (§3.7, U1); word 7.0 (U2) |
| D | The drive (§3.5) |
| E | Ownership with UE2's monitor and S38 (§3.8) |
| F | TRX64's T1/T2 when they land: exact cycle stops, `cs`, raster units; the interrupt boundary |

## 9. Checks

**Unit** (`trx.rs`, `trx_dbg.rs`, `c64.rs`, `c64-bridge`), one test per register at least:
- Byte rule on every new register: bytes 0-2 collect, byte 3 commits, byte-0 read latches, a commit reads 0;
  `SB_DATA` read moves `SB_ADDR`, a block of 7; `BMP_DATA` write moves `BMP_IDX`; `CYCLES` read latches `CYCLES_HI`.
- OWNER: C64_STOP neither pauses nor holds the CPU, still reads 3, still mutes; OWNER 0 gives it back.
- PAUSE_IB then STATUS b0, b2, b3, REASON 1, IPC = PC, `IRQ_CAUSE` b0; PAUSE_CB on a shut gate keeps the old fields.
- STEP_INSTR 1/5: INSTRS + n, REASON 2, the PC the same n instructions reach in a reference run; ARG 0 = 1; a step on
  a running gate is ignored; RUN and steps clear STOP_NEW.
- Bitmap: a bit at `$C000` stops at IPC `$C000` with REASON 3 before it runs; RUN goes on and does not stop there again
  until it is reached again; an interrupt at that boundary does not fire it.
- Units: `until` (IB one-shot) stops once and disables itself; `n` over a JSR stops at the return with S >= S0;
  `ret` with PREV_RET; a store observer over `$0400-$07E7` stops with ADDR, DATA and REASON 5; HITS counts.
- CYC_CMP: armed, stop REASON 7, disarmed after; STATUS b19.
- State bus: words 0.0-7.0 against TRX64 at a stop; shadows hold while running, follow while halted, capture with
  `CAP_CTRL`; 1.4/1.5 = TRX64's registers; `SB_DATA` write refused while running (`SB_STATUS` b1).
- CPU write: `r a=77 x=05` staged, `SB_LOAD` b1 armed until the step, then `STA $0400` stores `$77` in 4 cycles;
  `pc=` redirect with `IPC_SET`: IPC and the IB flag at once, no cycle.
- Session: a debugger stop starts one (`SH_STATUS` `b0 | b2`, count + 1); `$DC0D` and `$D019` read twice keep their
  flags; `wr d020` is read back, not in the chip, committed by the step before it runs; SH_MODE 0: no session. With
  OWNER and C64_STOP held every debugger stop (six, and a step) has its session; without OWNER C64_STOP keeps the
  block; an app's Ultimax freeze with OWNER is the app's session, ended when C64_STOP falls.
- LK: look of ROML/ROMH/IO1/IO2 for an EasyFlash bank 2 equals the bytes; ROMH asked at `$A000` equals `$E000`;
  physical slot `ext`, REAL while halted equals the cartridge, refused while running.
- Drive: `DRV_CMP` exec stops the C64 with REASON 8; DRV_STEP 1 moves DPC, the C64 cycle count stands.
- Ownership: UE2's `c64 halt` and `goto` refused while OWNER; an OWNER write on an anchor releases it.

**Firmware smoke** (`scripts/smoke-trxmon.sh`): the `t21-p4-app` ELF and `trxmon.u2a` (both built in the U64 build
container, `apps/trxmon/build.sh`), `--board c64u --trx-io`, a fresh flash with the ROMs, `--sd` an image with
`trxmon.u2a`, `--net user --hostfwd tcp:127.0.0.1:4312:4312`, System Mode PAL, the overlay interface:

| Step | Expected |
|---|---|
| boot | console `[TRX] IO block present, CAPS eb`; READY |
| `button`, browse to `trxmon.u2a`, Run App | the monitor screen (40 x 23 from row 2), `PAUSED IB $xxxx`; console `trxmon: serving the TRX64 runtime contract on TCP 4312` |
| type `r`, `m c000`, `d e5cd`, `io d000` | registers equal UE2's `monitor r`; `m`/`d` equal `monitor m`; VIC registers equal TRX64's `peek_lens(…, "io")` |
| `z`, `z 5` | the PC equals UE2's own trace of the same steps; cycles + the instructions' cycles |
| `bk e5cd`, `x` (leave with a stop armed) | the menu closes (`ui->close_menu`), the C64 runs; the break opens the monitor screen by itself with `breakpoint 1 hit at .C:e5cd` |
| `m io dc0d` twice | the same value (no flag cleared) |
| `wr d020 05`, `z` | the border colour changes after the step |
| `wr d020 03` in a debugger stop with a session, RUN | on the commit list (the chip keeps its colour), written at RUN |
| `m io d800` in every debugger stop (foreground with C64_STOP held, and background) | UE2's colour RAM (not while `rstep` keeps the machine landed: §11) |
| `r a=42`, `z` over `STA $0400` (a program put in with `a c000`) | `$0400` = `$42` |
| `m cart 8000` with an EasyFlash in `--cart` | the bank's bytes |
| `trace status`, `rstep` | the board's "needs the instruction trace" and "no frame ring" text |
| RPC: `tests/trxmon_client.py 127.0.0.1 selftest` | passes |
| RPC: `read c000 256 cpu` against `monitor m c000` | equal |
| `/quit` | trxmon ends, OWNER 0, the C64 runs; `print 6*7` gives 42 |

**Not checked by the smoke:** cycle-exact stops (T1), interrupt boundaries (T2), the 80-column screen.

## 10. Size

About 3,000 lines with tests: `trx_dbg.rs` and the page 1/3 registers 700, the bridge (stop sources, steps, CPU
write, shadows, drive) 900, session and lens 250, ownership and `monitor trx` 150, unit tests 900, smoke script 100.
Phases A-E without TRX64 changes; F waits for TRX64.

## 11. As built (2026-10-05)

**Update (t21-p4 3302c85, t21-p4-app 165c22fe):** UE2's session takes the new block rule (`trx_session.rs` `step`:
the block clears with C64_STOP off or OWNER set). The app's `api_delay_ms` sleeps `ceil(ms/tick) + 1` ticks, trxmon
waits for its serve loop with `plat_wait_flag`, and `x` calls the new `ui->close_menu` (`run_app` returns `MENU_HIDE`;
Ctrl-O too; not after `/quit`). Builds kept as `run/trx/ultimate-165c22fe.elf` and `run/trx/trxmon-165c22fe.u2a`;
`scripts/smoke-trxmon.sh --elf $PWD/run/trx/ultimate-165c22fe.elf --u2a $PWD/run/trx/trxmon-165c22fe.u2a` passes with
both former findings below turned into PASS checks: after `bk e5cd` and `x` the menu closes and the break opens the
monitor screen by itself; after the selftest and each step in the foreground (C64_STOP held) the stop has its session
and the colour RAM reads UE2's values. One stop in the foreground still has none: after `rstep` the machine is landed,
and the ring's land turns sessions off (trx_ring.cc `land_locked` -> `scrub_begin_locked` -> `stop_machine`: `SH_CTRL`
0) until its commit puts `SH_CTRL` back (`commit_locked`); `m io d800` reads `--` there (INFO). That is the firmware's
design of a land, not the block.

The first build, against t21-p4 a85f741 (the D-page fix df6d71c: STATUS b1, `ultimate_sys` opens the window's D page for a debugger
stop) and t21-p4-app 553e2e17 (trxmon: a debugger stop holds the window like C64_STOP), built by the FPGA session and
kept as `run/trx/ultimate-553e2e17.elf` and `run/trx/trxmon-553e2e17.u2a`:
`scripts/smoke-trxmon.sh --elf $PWD/run/trx/ultimate-553e2e17.elf --u2a $PWD/run/trx/trxmon-553e2e17.u2a` (absolute
paths: the emulator runs in the run directory). CAPS `$EF`, CAPS2 `$3D`, `$BD` with the trace (S40 phase E).

- **The model** (`ue2-core/src/devices/trx_dbg.rs`): trx_dbg.v's and drv_dbg.v's registers, the commands, the stop
  record (written only while STOP_NEW is 0, `ev` per stop), the units' qualifiers, chain and PASS, IPC_SET, the
  drive's registers. Shared (`DbgLink`) by the IO block (reads, with the CYCLES latch and BMP_DATA's increment), the C64
  port (every write at its clock, the run-control commands) and the backend (the stops, the counters).
- **The run** (`c64-bridge/src/dbg_run.rs`): `run_obs` looks at each boundary the debugger wants (the exec bitmap and the
  IB units through TRX64's exec watch; every boundary for a pause, an instruction step or the drive comparator), passes
  it with one instruction when nothing fired, and ends the run at a stop. Read and write units watch their ranges
  through `on_access` and stop at the next boundary with the access's ADDR and DATA (REASON 5). CYC_CMP and a cycle
  step cut the budget and stop at the first boundary at or after the count. Count and log actions (log: engine stream 1
  space 8, `TapEvent::DbgLog`). A stop holds the whole machine (the pause's mechanism, S37). PAUSE_IB and PAUSE_CB stop
  at once where the machine stands when that boundary qualifies (UE2 stands between instructions); PAUSE_CB with the
  6510 held stops at once, FLAGS b0 0. Steps run at once inside the command's access (the session's list written
  first); a step that cannot end there (the 6510 held, a UCI wait) is left to the running machine.
- **Counters:** `CYCLES` is the engine's counter (S40, the same edges, answer 11), with the window's bus cycles while the
  machine stands counted on both (+1 each); at a stop the opcode fetch is counted (`ib_bias`, +1 on CYCLES and INSTRS).
  `INSTRS` counts retired instructions.
- **OWNER** masks C64_STOP for the pause and the 6510's stop; C64_STOP still reads 3, mutes and shows in STATUS b5.
- **The CPU write path:** `SB_LOAD` b1 with an image that is the machine's own but for A, X, Y, S, P, PC, IR, the
  write-back controls and `din_r` loads the registers and the PC into TRX64 (`engine_tap.rs` `tap_cpu_write`); UE2's
  snap images still load from the side table (S40 §5.3); anything else is refused as before. Word 1.3 shows IPC_SET until
  the machine moves.
- **The session** (`trx_session.rs`): the debugger's stop is the second cause (modes 1, 2), `SH_STATUS` b2, kept while
  the hold stands, ended by RUN or a step with its list written first, blocked until the stop is gone. In it the window's
  D page is answered by the session and the rest goes to the RAM's second port without C64_STOP (df6d71c).
- **The lens unit:** `LK_REQ` at its clock; the look by class (ROML, ROMH whether asked at `$A000` or `$E000`, IO1, IO2)
  from the internal cartridge logic and the port's devices (U1 closed), `ext` for the physical port; REAL only while
  halted (refused otherwise), a read with the select forced of the physical cartridge or the internal one.
- **The drive:** DRV_STAT (DPC, DCYCLES from TRX64's drive CPU), DRV_PAUSE/DRV_RUN through the drive's own stop
  (`set_stopped`), DRV_STEP n runs drive A alone while the C64 stands (REASON 9); the exec comparator is evaluated on
  the drive's PC once per C64 instruction (T5) and stops the C64 at that boundary (REASON 8, FLAGS b1) with the drive
  held. The write comparator is not served.
- **Ownership:** UE2's monitor refuses `c64 halt|go|step`, `g`, `x`, `z`, `step`, `n`, `next`, `ret`, `until`, `goto`,
  `frame`, `rewind`, `ringload`, `rstep`, `reverse` while OWNER is set ("trxmon owns run control (OWNER); use it, or
  /quit it"); the VICE port refuses ADVANCE_INSTRUCTIONS, EXECUTE_UNTIL_RETURN and EXIT. OWNER taken while S38's
  transport holds an anchor releases the transport and leaves the C64 held by the debugger.
- **`monitor trx`** shows page 3: status, the record, the counters, the bitmap, the units (and why one is refused), the
  drive, `LK_RES`.

**Checks.** Unit tests: the model (`trx_dbg.rs`: counters and the latch, the bitmap index, the commands, the record,
the qualifiers, chain and PASS, OWNER and the drive registers), the IO block (the byte rule on page 3 and LK), the
session (the debugger's cause, its end and block), the port (OWNER masks C64_STOP, the debugger's session, the D page,
RUN writes the list, LK refused while running), the bridge (pause and steps of instructions and cycles, the bitmap,
`until`/`n`/`ret`, a store observer, CYC_CMP, the CPU write path then a step, PAUSE_CB with the 6510 held, a log
action, the lens by class, the drive step and comparator, OWNER taking the C64 from the transport), the monitor (the
refusal). Firmware smoke `scripts/smoke-trxmon.sh` with 553e2e17 (PASS; Navigation Style "Quick Search", so W, A, S, D
can be typed): boot CAPS EF; Run App of `trxmon.u2a` from the SD, the screen `PAUSED IB`; typed `r`, `z`, `z 5`
(registers as `monitor r`, REASON 2, INSTRS + n) and `d e5cd` (UE2's bytes); the RPC selftest; `read c000 256 cpu` =
`monitor m c000`; `m io dc0d` twice the same; `rstep` lands; `wr d020 05` and a step: border 5; typed `r pc=c000`,
`r a=42` and `z` over `STA $0400`: `$42`; `/quit`: OWNER 0, `print 6*7` gives 42. Then Run App again, typed `bk e5cd`
and `x`: trxmon serves in the background and the breakpoint fires (REASON 3 at `$e5cd`); the reopen is reported KNOWN
(below). With C64_STOP off, RPC continue and pause: a debugger stop with a session, the colour RAM reads UE2's pattern,
`wr d020 03` is on the commit list with the chip unchanged, RUN writes it.

**Deviations and findings (for Alex):**
- **(Fixed in 165c22fe.) `bk` then `x`: the screen does not come back. The cause is in the app (trxmon 553e2e17), not in UE2.** Traced with
  `--gdb` on the 553e2e17 ELF:
  1. After `x` the request is queued: `s_uireq_fn` = trxmon's `ui_callback` (app_api_net.cc:446-457). The loader leaves
     the browser up after the app returns (app_loader.cc:470 `appear()`, :473 `MENU_C64_RESUME`), so the request waits
     until the person leaves the menu: ultimate.cc:210-212 turns it into `doIt = 3` only in the init task's loop. HPD
     plays no part (ultimate.cc:213-219 picks the interface for `doIt` 1 only; case 3 calls the overlay's `run_once`).
  2. The menu button closes the browser; `open_request_run` (app_loader.cc:92-111) is reached and starts `ui_callback`
     in the task "app_ui" at `tskIDLE_PRIORITY + 2` (app_loader.cc:104).
  3. `ui_callback` sets `s_want_ui` and waits up to 1000 ms for the serve loop to park (`s_in_ui`, main.c:437) with
     `A->sys->delay_ms(2)` (main.c:412). `api_delay_ms` is `vTaskDelay(pdMS_TO_TICKS(ms))` (app_api_impl.cc:763); at
     `configTICK_RATE_HZ` 200 (FreeRTOSConfig.h:21) that is `vTaskDelay(0)`, which only yields (tasks.c:1338, :1365).
     The serve loop runs in "app_resident" at `tskIDLE_PRIORITY` (app_api_net.cc:285), below "app_ui", so it never
     runs: the callback leaves at the `!s_in_ui` exit with `s_want_ui` 1, `s_in_ui` 0, and the monitor screen never
     opens. The stop record stays read (STOP_NEW 0), so nothing asks again.
  4. With the two `delay_ms(2)` in `ui_callback` patched to `delay_ms(5)` (one tick) in RAM, the same run brings the
     screen back with `breakpoint 1 hit at .C:e5cd`. The fix belongs in the app (a wait that blocks for at least one
     tick, or the serve task at the same priority); the board behaves the same (same FreeRTOS, same priorities).
  The smoke checks UE2's part (the breakpoint after `x`, trxmon alive) and reports the reopen as KNOWN; it turns into
  a PASS when a fixed app brings the screen back.
- **(Fixed in 3302c85.) The colour RAM in a debugger stop needs a session, and the RTL's block keeps it off while C64_STOP stands.**
  `hw_session_read` needs `SH_STATUS` b0. freeze_resp.v:546 clears `block` only when the raw `app_stop` (top.v:1149,
  C64_STOP not masked by OWNER), `halt_req` and `dbg_stop` are all low; a debugger session's end sets it (:884). The
  loader holds C64_STOP for a foreground app (app_loader.cc:334 `MENU_C64_PAUSE`), so only the acquire's first stop
  has a session; every later stop in the foreground has none and `m io d800` reads `--` (the smoke prints it as INFO).
  After `x` the breakpoint at `$e5cd` fires before the loader's `MENU_C64_RESUME`, so that stop has none either. A run
  with C64_STOP off clears the block and the next stop has its session: the colour RAM reads the real values and
  `wr d020` goes to the commit list. UE2 models freeze_resp.v as built; whether the block should ignore C64_STOP while
  OWNER is set is a question for the RTL. In the foreground `wr d020` works through the C64_STOP path (a bus cycle).
- **`rstep` after a CPU write or a bus cycle while halted does not replay** ("the replay from the frame start did not
  come back to the same stop"): a redirect is no part of the frame, and the bus cycle counts on CYCLES and the engine's
  counter (as on the board) but the replay does not make it. The smoke does `rstep` first.
- **`trace status`** reports the trace since S40 phase E (the app turns it on with the ring; the smoke checks records).
- **`m cart`** on the screen is not in the smoke (the lens is covered by the bridge test).

**TRX64 gaps** as §5: T1 (no stop or step at a cycle boundary: PAUSE_CB, STEP_CYCLES, CYC_CMP and access stops land at
the next boundary; raster and every-edge units refused), T2 (the interrupt sequence and the handler's first instruction
are one `execute`: a bitmap bit at `$FF48` never stops; a step across an interrupt runs the handler's first
instruction), T3 (A/X/Y/P qualifiers see the settled registers), T4 (`on_access` gives no operand fetch and no dummy
read or write), T5 (the drive's PC is sampled once per C64 instruction; a drive step leaves the drive ahead of the pair),
T6 (words 1.8-1.14 synthesized), T7 (the bridge's pause), T8 (no RDY park: `held` 0). **Phase F** (T1, T2) waits for
TRX64.
