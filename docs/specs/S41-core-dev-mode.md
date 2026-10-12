# S41: core dev mode — FPGA blocks absent

Status: built (2026-10-12), as below; what the code does differently from the draft is in §8. Local branch only, like
S37-S40.

## Goal

TRX64-Ultimate builds bitstreams with optional blocks switched off (`TRX_FEATURES` in
`designs/c64_basic/build.tcl`). On the board, the app can stop in init when a block it talks to is missing.
When the stop is an I/O read that never gets an ack, the RISC-V hangs, and the trial bitstream has no JTAG
console. UE2 should boot the same app with the same blocks absent and report where init stops, so the core
team can find these stops without flashing.

Today UE2 cannot do that, for two reasons:
- every block is always present;
- an unmapped read returns 0 at once (`bus.rs`), so a missing ack never stalls anything.

## 1. Interface

```
--absent <list>          reu,sampler,uci,jtag,turbo,apply,streams   (the TRX_FEATURES names; "none" = all present)
--absent-mode <mode>     noack | ack0, or per block: sampler=noack,uci=ack0
--absent-continue        log every stall and go on instead of halting
```

- The names are the build switches' names, so a build profile maps to the flag 1:1 (dfx-trial's `streams`
  included, Alex 2026-10-12).
- `--absent` needs `--trx-io` (on or auto-detected); otherwise it is an error.
- The default mode is the one the trial bitstream has today for each block (§2).

## 2. Per block (TRX64-Ultimate, branch dfx-trial; apply on t80-lean-ring)

| Block | RTL switch | App side when absent | C64 side | IRQ/DMA |
|---|---|---|---|---|
| sampler | NO_SAMPLER, c64_core.v | 0x10048000-0x100480FF: `noack` before bd16bee, `ack0` from bd16bee on | `$DF20-$DFFF` not mapped | irq 0, no DDR fetch |
| uci | NO_UCI → `gen_no_uci`, ultimate_sys.vhd | 0x10044000 (`CMD_IF_BASE`): `ack0` | never drives the C64 bus | uci_irq 0, C64 irq 0 |
| reu | NO_REU, c64_top.v | no app window | `$DF00..` reads $FF, no DMA | irq 0, no DDR |
| turbo | NO_TURBO, c64_top.v | registers answer as usual | the engine never engages | none |
| jtag | NO_JTAG_DBG, top.v | no bus effect | none | none |
| apply | NO_APPLY, trx_engine.v + trx_io.v | AP_* writes ignored; AP_CMD never busy; CAPS2 b5 clear | none | none |
| streams | NO_STREAMS, top.v | no bus difference: the template writes (`r_udp`) are still acked and go nowhere, the enables do nothing | none | none |

- **`noack`:** the access does not complete. UE2 halts the RISC-V on that load or store and reports it (§3).
- **`ack0`:** reads return 0 and writes are dropped.
- **CAPS:** the trial clears no capability bits; UE2 copies that. The only exception is `apply`, which clears
  CAPS2 b5. The ITU word stays as it is, unless `--caps` is also given.
- **Defaults** (dfx-trial 0a130e1): sampler `ack0` (bd16bee is in the branch), uci `ack0`, core `ack0`
  (the host's decoupler, §5). reu, turbo,
  jtag, apply and streams have no app window, so no mode; `--absent-mode` refuses one for them. The UDP streams'
  sent counters are on the JTAG map only, so UE2 has nothing to model for `streams`: it is accepted as a no-op.

## 3. Stall report

A `noack` access is reported with:
- address, width, read or write, and the PC;
- a backtrace from the ELF symbols (the existing `halted:` fault path);
- the last console line, and the time since boot.

By default UE2 halts, as the board does. With `--absent-continue`, UE2 logs the stall, completes the access as
`ack0`, and runs on, so a single run lists every stall. The end-of-run summary counts the stalls per address.

## 4. Trial differences that need no model (FW session, 2026-10-12)

- `cw` acks one `cpu_clk` later on the trial. The I/O bus is ack-driven, so this only adds latency, and no app
  code times cw accesses. UE2 adds no wait state.
- SID `redip2`:
  - `sid_fhit` belongs to the turbo CPU's fast SID path on the C64 side, not to the app.
  - The level read-out (`lv_addr`/`lv_data`, behind CAPS3 b0) reads 0 on the trial; at most the level meters
    stay flat. Only the register answers matter, not the reDIP sound.
  - UE2 follows t21-p4, which has neither CAPS3 nor the level read-out. Both already read 0 in UE2, which is
    exactly the `redip2` answer. When UE2 follows the reSID8 RTL (T25.5), `--absent` gains `resid8`.
- The 1541 drive and GCR codec sit outside `ultimate_sys`. The regions and answers stay the same, so this needs
  no change.

## 5. Core absent: the grey box (T79 partial reconfiguration; phase B)

`--absent core` makes every register window of the swappable core answer as absent. Which windows those are is
ultimate_sys.vhd's decode (dfx-trial bc97bbe), the regions whose ack comes from the core (Alex, 2026-10-12):
- `r_drive`: drive A, 0x10020000-0x10023FFF;
- `r_gcr`: the GCR codec, 0x10060500-0x100605FF;
- `r_cw`: the cartridge's EEPROM 0x1004C000-0x1004CFFF, the sampler's bytes 0x10048000-0x100480FF, trx_io
  0x101A0000-0x101AFFFF (the trx_sys pages, the ring engine, the pixel layer);
- `r_dma`: the DMA window 0x10050000-0x1005FFFF; without the decoupler a read of `$D400-$D7FF` without MEMONLY is
  answered by ultimate_sys itself with $FF (`dma_stub`), with it the decoupler acks it with 0 (`core_to`);
- `r_rom`: the ROM windows 0x10188000-0x1018FFFF (see §8).

ultimate_sys acks everything else itself (`ack_r`), core or not, so those never stall: the cart registers (`r_page`
page 0, 0x10040000), the core config (page 3, 0x10180000), the palette (`r_pal`), the UltiSID curves (`r_sidcv`), the
IEC processor's idle constants (`r_iec`) and UE2's drive B (0x10024000, `r_none`). UE2 keeps serving them from its own
register models while the C64 stands still.

**A swap is a full cold boot** (Alex, 2026-10-12): nothing of the old core survives. On the board the host's DFX
decoupler does it (T79.1; RTL in dfx-trial a75c356, ultimate_sys.vhd and top.v; the decoupler came in 975a728;
the debug register `core_decouple` is JTAG 0x100029 bit 0, read back in the same register). A swap is `core_decouple` = 1, the partial bitstream loaded, `core_decouple` = 0. The
board PoC 3 (46afe1f) did that over JTAG in 2.8 s with REST answering 200 throughout; after
`PUT /v1/machine:reset` the new core booted to BASIC V2.
- **On the rise** (`core absent`), once: the latches behind pages 0 and 3 go back to their power-up values, with the
  C64's reset held (`c64_reset_i` = 1), and a 64-cycle sweep rewrites the palette to Colodore and holds `sidcv_we` at
  0. The held reset is the reset sense: `c64_reset_req`, top.v's `core_reset_req` (top.v:1225, 1257), `rst_sense_s`,
  ITU low bit 7, all on the host side. There is no sweep over the page RAM (a75c356; 2565e6d had one): every byte
  that reads back from it keeps what the firmware last wrote until the firmware writes it again. Per offset:
  - page 0 (cart registers): C64_MODE (+0: reset, ULTIMAX, NMI), C64_STOP (+1) and C64_CARTRIDGE_TYPE (+5) read their
    latches, which reset (MODE reads $04, STOP 0, type 0); C64_CLOCK_DETECT (+3) reads the reset sense ($11),
    CARTRIDGE_ACTIVE (+6) the core; REU_ENABLE (+8), REU_SIZE (+9) and C64_SAMPLER_ENABLE (+E) have latches that
    reset (off, 16 MB, off) behind read-back bytes that keep their values; every other offset is a read-back byte
    only (STOP_MODE +2, +4, +7, +A-+D, +F).
  - page 3 (core config): every offset reads back from the page RAM except CORE_VERSION (+10, $01) and the voice
    levels (+80-+9F). Latches that reset sit behind +00 (scan lines), +01 (video format), +02/+2D (turbo), +03
    (MEMONLY), +0A/+0B/+0E/+0F (SID base and mask), +20-+23, +27-+29 (SID waves, filter, digis, split), +2B/+2C (bus
    sharing) and +30/+31 (the software joystick outputs); +2E (speed update) is a strobe.
- **While it is 1:** `r_drive`, `r_gcr`, `r_cw`, `r_dma` and `r_rom` are answered by the host at once, ack and data
  0 (the DMA window's `$D400-$D7FF` included), no DMA starts; the host takes no DDR2 request from the core, the
  toggle handshakes, slot, IEC and the core's IRQs are held (top.v, unfinished). Pages 0 and 3 take writes as always,
  so the firmware's `C64::hard_stop` sees its stop.
- **On the fall** (`core present`): nothing is raised. The new core comes up with the host's latches as they stand
  (not the read-back bytes): its C64 stays in reset until the firmware releases it through C64_MODE, as at power-up;
  the REU, the sampler, MEMONLY and the joystick outputs are at power-up unless written since the rise. The FW side accepts
  this as the cold-boot behaviour: the board PoC releases it by hand, later the T81 core selector app as its last
  swap step.
- Pages 1, 2, 4 and 5 of `r_page` (MATRIX_KEYB 0x10100300, the U64 board IO 0x10100400, C64_PLD_ACC 0x10181000,
  U64_DEBUG_REGISTER 0x10181800) and the UCI (`gen_uci`, reset by the host's reset only) are never touched.

`core absent` is the rise, `core present` the fall. The core's mode:
- **`ack0`, the default:** the decoupler.
- **`noack`:** a host without the decoupler: a request into the core never completes (the board before bc97bbe, or
  the case the host timeout is the backstop for: bc97bbe, 70bd378 `r_drive`, `r_gcr`, `r_cw`, `r_dma`, `r_rom`, acked
  with 0 after 65,536 clocks), and nothing is reset or sensed.

Whether the app can write `core_decouple` itself (for a swap from the menu) is open on the FW side; UE2 maps nothing
for it.

The host side keeps running: RISC-V, USB, SD, flash, Ethernet/UDP, overlay, HDMI.

- **Mode:** `noack`/`ack0` work as in §1.
- **While absent:** the C64 stops (no clock, no audio, no picture). The TRX output shows the overlay over black.
- **At run time:** the control commands `core absent` and `core present` swap the core while the app runs.
  `core present` brings the core back from power-up (a fresh core image), as a reconfiguration would: a new TRX64
  backend, a fresh TRX block and GCR codec, handed the host's registers as they stand (held in reset unless the app
  released it); the UCI block and the host's pages 1, 2, 4 and 5 as they were.
  `--absent core` at start is the same as `core absent` before the first instruction.
- **Ring:** goes with the core. A swap ends the ring (S38).
- **Check:** `smoke-core-swap.sh` swaps the core while the RC1 app runs. The app's swap handling is logged.
  `core present` brings back a C64 that boots once it is released.
- **The app has no swap handling yet** (FW session, 2026-10-12: the RC1 package integ-m1-692f643d, app 3c8e4aa10bb9,
  has no commit for it). The smoke therefore asserts what the emulator does and only logs how the unmodified app
  reacts when the core's windows go away and come back.

## 6. Checks

- `smoke-core-dev.sh`: the RC1 app with `--absent sampler --absent-mode sampler=noack` halts with the sampler
  address and PC in the report; with `ack0` the run goes past that point.
- Unit tests per block: absent reads and writes, IRQ lines held low, REU `$DF00` reads $FF, CAPS2 with apply
  absent reads $D7 on t80-lean-ring (its $F7 without b5). UE2's CAPS2 is t21-p4's $BD, so it reads $9D (§8).
- With `--absent none`, the existing TRX smokes behave exactly as before.

## 7. Source

TRX64-FW session answers of 2026-10-12, quoted above. RTL paths are in the TRX64-Ultimate repo.

## 8. As built

### Results (2026-10-12, RC1 app `run/trx/ultimate-3c8e4aa.elf`, CI package integ-m1-692f643d)

- `smoke-core-dev.sh`: all five runs pass. `sampler=noack` halts at the app's first sampler access, a write in the
  UCI reset task (`CommandInterface::run_reset_task()+0x30`, 0x000a7d54: the voices cleared on a C64 reset, eight
  bytes 0x10048000-0x100480E0) 1.45 s after boot, console "Speed regs: 00 80". With `ack0` the app boots to the
  parked UI; with `--absent-continue` those eight writes are the only stalls of a boot. All seven blocks absent in
  their default modes: the app boots, CAPS2 reads $9D.
- `smoke-core-swap.sh`: all six runs pass (decoupler: a75c356).
  - With the decoupler, a swap during a run: right after `core absent` the cart registers read
    `04 00 02 11 00 00 00 00 00 04 00 00 00 00 00 00`: MODE, STOP, CLOCK_DETECT and the type from their latches (the
    reset held and sensed), STOP_MODE ($02) and REU_SIZE ($04) still what the app wrote. The unmodified app
    reacts to the reset sense at once with its reset handler ("U64 reset handler. Resetting Settings"), writing the
    core config again. After `core present` the C64 is still in reset (C64_MODE $04), nothing is raised, pages 1, 2,
    4 and 5 read as before; the app does not release the reset by itself, so the C64 stands until something writes
    C64_MODE: the smoke's `monitor c64 reset` (the firmware's own write) boots it to READY, twice in a row, and so
    does `PUT /v1/machine:reset` over REST, which answers all through the swap (the board's PoC 3 sequence).
  - With the decoupler and `--absent core` from power-on the app runs its whole init ("All Init functions called")
    and stays up; its cart init reads "Mode: 04" (the C64 held in reset) and the reset handler runs twice. The
    `C64::hard_stop` hang of the held version (d6b9cd8) is gone.
  - With the core in `noack`, the app halts on its first access after `core absent`: drive A's 0x10020000
    (`C1541::task()+0x28`); with `--absent core` from power-on, in init on the trx_io ID 0x101A0000
    (`trx_io_caps()+0x20`, from `rtc_seconds()` through `Rtc::get_fat_time()` while `f_mkfs` formats the RAM disk).
    With `--absent-continue` one swap logs 22 addresses: drive A, CAPS2 (`trx_ring_poll`) and the gate and engine
    pages (`trx_io_read32/write32`).

### Deviations from the draft

- **CAPS2 with apply absent reads $9D, not $D7.** $D7 is t80-lean-ring's word ($F7 with b5 clear, trx_io.v
  `NO_APPLY`); UE2's CAPS2 is $BD (t21-p4 without b1, S40), so clearing b5 gives $9D.
- **turbo, jtag and streams change nothing.** UE2's UDP streams stay on with `streams` absent (no bus difference
  for the app; the FW session, 2026-10-12). UE2's TRX model has no turbo engine (S40 §0: `main` has no T29), so "the engine
  never engages" holds already; UE2 has no JTAG debug master, and the switch has no bus effect. All three are
  accepted and say so in one line.
- **`r_rom` is a core window**: ultimate_sys hands a ROM byte to the core as a DMA request (`dma_rom`,
  `dma_req_tgl`) and acks it on the core's `dma_done_tgl`. 70bd378 added it to the host timeout ("found by ue2emu
  S41"), and the decoupler answers it. Without either, the DMA watchdog (`dma_wd`) acks it after 2^25 clocks
  (335 ms) with $FF; UE2 does not model that wait (`noack` halts).
- **The decoupler follows the RTL** (dfx-trial a75c356) on the reset of the latches behind pages 0 and 3 on the
  rise with the read-back bytes kept, the held reset and its sense, the windows' ack with 0 and the quiet fall. UE2
  models the latches it hands a core (`Latches` in c64.rs: REU, sampler, MEMONLY, joystick outputs; C64_MODE,
  C64_STOP and the type are the port's own); the other page 3 latches are the core config offsets written since the
  rise, which the new core gets, and the rest stays at its power-up value in the new core. Two parts are UE2's own:
  the palette is the core's in UE2 (TRX64's), so the new core starts with its default palette and a palette write
  while the core is absent goes to the old one and is lost, where the RTL's sweep rewrites `pal_ram` to Colodore;
  the SID curves are a write sink in UE2 (S17), so there is nothing to clear. The read-back of REU_ENABLE and
  C64_SAMPLER_ENABLE in UE2 is bit 0 only (`CART_REGS`), where the page RAM keeps the whole byte; unchanged here. top.v's half (DDR2, handshakes, pins, IRQ masks) needs no
  model: an absent core in UE2 runs nothing.
- **Host registers read their latches, not 0**: ultimate_sys reads the cart registers and the core config from its
  own registers and constants (`page_data`), and UE2's register models give that answer.
- **The UCI moves to the new core.** UE2's UCI is TRX64's block inside the C64 backend; `core present` takes it off
  the old backend and puts it into the new one with its state, and drops the new machine's power-up reset event, so
  the swap behaves like the host's block on the board. Its routing (C64_BUS_INTERNAL) follows what the core config
  hands the new core.
- **A new core is handed** the written core config offsets (all but the C64_SPEED_UPDATE strobe), the cartridge
  type, the REU and the sampler, the held reset, ULTIMAX, NMI, the stop and the keyboard rows: the levels the core
  reads from the host.
- **One stall line per site.** `--absent-continue` logs the first stall of each (address, direction, PC) and counts
  every one; a polling loop does not flood the log.
- **The backtrace is a stack scan.** The firmware keeps no frame pointer, so the report lists the words above `sp`
  that point into a symbol right after a `jal ra` / `jalr ra`; a stale slot of a returned call can show up.
- **No audio while the core is absent** means no samples: the audio device plays silence, a WAV file (`--audio-wav`)
  has a gap of the absent time.
- **`core present` on a present core, `core absent` on an absent one** are errors, not reloads.

## In the code

- `crates/ue2-core/src/absent.rs`: `Block` (names, windows with `CORE_WINDOWS`, default modes), `AbsentConfig` (the three flags),
  `AbsentMap` (per 256-byte grain, the core first), the stall counts.
- `crates/ue2-core/src/bus.rs`: `SystemBus::absent`, `absent_access` in `io_read8`/`io_write8`: an absent window
  reaches no device; `noack` records `SystemBus::noack`, and the rest of that instruction reaches nothing; a DMA read
  of `$D400-$D7FF` without MEMONLY reads $FF without the decoupler (`DMA_SID_START`). The monitor's `poke_io8` refuses an absent window.
- `crates/ue2-core/src/machine.rs`: `MachineConfig::absent`; `Machine::new` refuses the flags without the TRX block;
  `start_absent` (§1, one line per block); `take_noack` after each step (halt: registers, PC and count put back;
  continue: log and count); `noack_report` (§3), `absent_summary`; `core_absent`, `core_present`,
  `take_c64_backend` (§5); `display` and `picture_counter` without a core.
- `crates/ue2-core/src/devices/c64.rs`: `C64Port::set_core_absent` (no sync, no tick, no frame, drives' WD177x IRQs
  low; with the decoupler cart registers and core config reset once, C64_MODE's reset held, ITU low bit 7 pulsed),
  `power_up` (the port's core side new, pages 0, 1 and 3 kept and handed to the new C64, its power-up UCI events
  dropped), `core_written`, `set_absent`. U64 board IO, C64_PLD_ACC and U64_DEBUG_REGISTER are devices of their own
  and are not touched.
- `crates/ue2-core/src/devices/trx.rs`: `CAPS2_APPLY`, `Trx::set_apply_absent` (AP_* ignored, CAPS2 b5 clear, kept
  over a reset), `Trx::set_core_absent` (IRQ 2 low, no polling).
- `crates/ue2-core/src/c64host.rs`: `C64Backend::set_absent`.
- `crates/c64-bridge/src/lib.rs`: `Trx64Backend::set_absent` (sampler off the port; UCI disabled, unrouted, IRQ and
  events 0; REU off the port), `take_slot_cart`/`insert_slot_cart` and `SlotCart` (the physical cartridge moves to
  the next core); `crates/c64-bridge/src/reu.rs`: `AbsentReu` ($DF00-$DFFF read $FF while C64_REU_ENABLE is set).
- `crates/ue2emu/src/main.rs`: `--absent`, `--absent-mode`, `--absent-continue`. `crates/ue2emu/src/runner.rs`:
  `Command::Core`, `CoreFactory` and `core_present` (a fresh `Trx64Backend` on the run's one audio sink,
  `audio::SharedSink`; the old one dropped first, its rings with it; the monitor session reset);
  `report` prints the absent summary. `crates/ue2emu/src/control.rs`: `core absent` / `core present` (S08).
  `crates/ue2emu/src/monitor/devices.rs`: `monitor trx` shows CAPS and CAPS2 as the app reads them.
  `crates/ue2emu/src/monitor/mod.rs`: `fw m ADDR [END]` (the firmware's IO read without side effects, also while the
  core is absent, for the swap smoke's page checks); the monitor says the core is absent instead of asking for
  `--c64 trx64`. `crates/ue2emu/src/monitor/run.rs`: `c64 reset` (reset and release through C64_MODE).
  `crates/c64-bridge/src/lib.rs`: `take_uci`/`insert_uci` and `UciBlock`; `crates/ue2emu/src/runner.rs`:
  `CoreFactory::build` moves the UCI block and the slot cartridge.
- Tests: `absent::tests`, `bus::tests::absent_windows_reach_no_device`,
  `machine::tests::{a_noack_access_halts_on_its_instruction, core_absent_and_present_swap_the_c64}`,
  `devices::trx::tests::{without_apply_caps2_b5_is_clear_and_no_job_starts,
  an_absent_core_drops_the_interrupt_and_stops_polling}`, c64-bridge `absent_blocks_take_away_the_c64_side`,
  ue2emu `control::tests::core_swaps_reach_the_target`.
- Smokes: `scripts/smoke-core-dev.sh`, `scripts/smoke-core-swap.sh` (default ELF `run/trx/ultimate-3c8e4aa.elf`).
