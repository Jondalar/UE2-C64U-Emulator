# S38 — TRX64's rings on the C64 (time travel)

**Status:** built (2026-10-04), local branch `s37-trx-pixel-layer`, not pushed. §4.1 (UE2's part around the C64
rewinds with it), §4.3 (the picture comes back with the anchor) and §7's board files built the same day. See "As
built" at the end.

**Why:** UE2's C64 is TRX64 (`trx64-core`, tag v0.12.3). TRX64 has its own time travel: a delta ring per
instruction, a CPU history, and a checkpoint ring with marks. Its daemon runs all three by default. UE2 runs the
same machinery the same way, so `rstep`, `whowrote`, `goto` and marks work on a C64 that runs behind the U64
firmware. TRX64 is the reference for Alex's FPGA, which gets a ring and a monitor of its own (TRX64-Ultimate T20,
T21). This is TRX64's Rust ring, not the FPGA engine: UE2 does not model `trx_engine.v` or the engine pages.

**Reads** (code is the truth):
- `trx64-core`: `delta_ring.rs`, `cpu_history.rs`, `checkpoint_ring.rs`, `ring_dump.rs`, `c64re_snapshot.rs`
  (`capture_runtime_checkpoint_with`, `restore_runtime_checkpoint`, `turbo_node`), `lib.rs` (`reverse_step`,
  `set_reverse_depth`, `turbo_engaged`).
- `trx64-daemon/src/main.rs`: `checkpoint_capture_every_frames`, `checkpoint_ring_seconds`,
  `stream_maybe_autocapture`, `capture_recorder_anchor_payload`, `transport_*`, `timeline_truncate`,
  `timeline_discard`, `ringbuffer_dump_to_path`, `ringbuffer_restore_from_path`, and its monitor arms (`rstep`,
  `chis`, `mark`, `marks`, `unmark`, `frame`, `goto`, `rewind`, `cadence`, `window`, `ringdump`, `ringload`,
  `asm-file`, `run`).
- `trx64-monitor`: `host.rs` (`MonitorHost`, `Timeline`, `Files`), `verbs.rs` (`classify_in`, `assemble_block_at`,
  `asm_file_origin`, `whowrote`, `triage`, `revdepth`).
- TRX64-Ultimate (`trx64/main` 029da8ef): `docs/specs/T20-delta-ring.md`, `T21-monitor.md`; app
  `software/io/c64/trx_ring.cc`, `trx_c64re.cc`, `api/route_timewarp.cc`, `apps/trxmon/src/{mon,ring,rpc}.c`.

**Owns:**
- `crates/c64-bridge/src/ring.rs` (new): the checkpoint ring, capture, restore, dump and load; UE2's node (§4.1).
- `crates/c64-bridge/src/hwlog.rs` (new): UE2's write log (§4.1).
- `crates/c64-bridge/src/lib.rs`: `advance_to` records; a reset, a new cartridge type and a cartridge put into or taken
  out of the expansion port drop the anchors.
- `cart.rs`, `cart_eeprom.rs`, `reu.rs`, `slot.rs`: the log taps and the register nodes.
- `crates/ue2emu/src/monitor/ring.rs` (new): the ring verbs and the monitor's files.
- `crates/ue2emu`: `--ring`, `--monitor-dir`.

## 1. What runs

With `--ring` (the default; `--ring=off`, TOML `ring = false`, switches it off), the daemon's defaults:

| Ring | Size | Fed |
|---|---|---|
| Delta ring | 10 s (`TRX64_REVERSE_SECONDS`): 3 116 300 instructions, 3 895 375 writes, about 110 MiB | per retired instruction, by `Machine` |
| CPU history | 262 144 instructions (`cpu_history::DEFAULT_CAPACITY`), 6 MiB | per retired instruction, by `Machine` |
| Checkpoint ring | one anchor per frame, 60 s: 3000 anchors at 50 Hz, about 290 MiB when full | by the bridge, at the frame boundary |

- `revdepth`, `cadence` and `window` change the sizes at run time, as in the daemon.
- With `--ring=off` nothing records: the per-instruction rings stay at one entry (as before S38) and there is no
  checkpoint ring. Every ring verb then says `the rings are off (--ring=off)`.

## 2. When it records

All three rings record only while the C64 runs at 1 MHz with nothing holding it. They stand while:

- **turbo is engaged** (`Machine::turbo_engaged()`: a speed index above 1 MHz);
- **the 6510 is held**: C64_STOP (the firmware's DMA, the menu, `c64 halt`) or the reset line;
- **the machine pause stands** (S37);
- **the transport holds the C64 on an anchor** (§4).

The bridge decides this at the start of each slice and again at each frame boundary. A turbo switch inside a frame is
seen at the end of that frame.

**A C64 reset drops the anchors and both per-instruction rings** (the daemon's `timeline_discard`): the anchors
describe a machine that no longer exists. The firmware resets the C64 for every program it starts, so history begins
at the last start.

## 3. Capture

- The daemon's pump runs one frame and then captures. Here the firmware's slices drive the C64, so a recording slice
  stops at each frame boundary (line 0) and the anchor is taken there.
- The anchor is the daemon's: `capture_runtime_checkpoint_with(…, omit_framebuffer, omit_expansion_ram)`, with no
  disk and no cartridge blobs. UE2 also leaves out `driveB`, which would store drive B's whole disk every frame.
- Ids are TRX64's `cp_{frame}_{seq}`, with `frame` = the VIC's frame count and `cycles` = the C64's cycle count
  since power-on.

## 4. Going back while the firmware runs

`goto`, `frame -N` and `ringload` restore an anchor. Then:

- The C64 stands on the anchor: no C64 cycle runs (the S37 pause mechanism). The firmware, its UI, USB and the
  network run on.
- **Release.** `run`, `g`, `c64 go`, `c64 step`, or any monitor line that changes the C64 (`classify_in` says
  `Mutates`), lets it go. The anchors after it are cut, marks excepted (the daemon's `timeline_truncate`).
  - A change (`wr`, `r a=…`) leaves the C64 stopped by the monitor's halt (C64_STOP), as the daemon's machine
    stays paused.
  - `run`, `g` and `c64 go` then run it on from there.
- A key press or a firmware DMA write while it stands does not cut. The daemon cuts on a key. Such a DMA write into
  the cartridge or the REU is not in the write log (§4.1, which stands while the C64 does), and it stays.

**From the anchor (the C64's own state):**
- the 64 KB RAM and colour RAM;
- the CPU and the CPU port;
- CIA 1 and 2;
- the VIC, with `$D030`/`$D031`;
- the SID registers in TRX64;
- the IEC lines and the interrupt state;
- the REU's registers (TRX64's `expansion` node, 16 bytes);
- the keyboard, then UE2's held keys and joysticks are put back on top;
- the POT lines.

### 4.1 UE2's part around the C64

On the U64 the FPGA holds more for the C64 than TRX64's anchor carries: the cartridge logic and its memories, the REU's
memory, the GMOD2 EEPROM, the UCI block. TRX64-Ultimate T20 §4.2 logs these as stream B ("other memories"). UE2 builds
its own part the same way, and `goto`, `frame` and `ringload` restore it together with TRX64's anchor.

- **The node.** Each anchor carries a key `ue2Hw`: where the write log stood, the internal cartridge logic's
  registers, and the physical cartridge's node. TRX64's restore reads only the keys it knows; a test loads a UE2
  `.c64rering` through TRX64's own `read_ring_dump`, `from_dump` and `restore_runtime_checkpoint`.
- **The write log** (`hwlog.rs`). Every byte the C64 side writes into one of these memories, with the byte it
  overwrote and the one it wrote (8 bytes a record; an unchanged byte is no record). Going back writes the old bytes in
  reverse, going forward the new ones in order. It records while the rings are on, in turbo and with the 6510 held as
  well, and stands while the transport holds the C64 on an anchor. Bounded by the anchors: a sweep every 64 captures
  drops the records older than the oldest anchor, and at most 16 Mi records (128 MiB) are kept.
- **The log's taps:**
  - `CartLogic::ddr_write`: the internal cartridge's ROM written in place (EasyFlash, `$DE09 = $65` in mode 101), cart
    RAM and GeoRAM, by guest DDR offset; the physical cartridge's U64 logic into its own memory;
  - `ReuRam::write`: every byte of the REU's DMA into guest DDR;
  - the internal GMOD2 EEPROM's microwire writes;
  - a physical flash cartridge's image (flash, then EEPROM): TRX64's chips write inside the mapper and finish erases
    lazily, so at each anchor, when the mutation counter moved, the image is compared with the one the log last saw.
- **Guest DDR is lent for the restore.** The monitor lends it around `goto`, `frame` and `ringload`, as a firmware
  access does. Without it the memories stay and the restore says so.

**What goes back, and what stays live:**

| Part | Restore | Why |
|---|---|---|
| Internal cartridge registers: bank, mode, RAM bank, GeoRAM page, `cart_en`, EasyFlash write key, IO2 enable, Retro Replay bank allow, freezer state, Epyx timer, EEPROM state machine and pins | from the anchor | what the C64 program set. Refused when the anchor's cartridge type differs (a note) |
| Cart ROM written in place, cart RAM, GeoRAM | write log | the C64's writes |
| GMOD2 EEPROM memory | write log | the C64's microwire writes. A byte put back sets the dirty flag, so the firmware saves again |
| Physical cartridge: registers, I/O RAM, the flash chips' command state and erase timer, its cycle epoch | from the anchor | the C64's writes; only into the same CRT (hardware type and a print of the file) |
| Physical cartridge flash and EEPROM, U64 logic RAM | write log | as above. `generation` counts a restored image, so the `--cart-slot` write-back sees it |
| REU registers | from the anchor | TRX64's node. Size and presence stay live |
| REU memory | write log | the REU's DMA |
| UCI block | the anchor's copy, when both the anchor and the live block are idle | §4.2 |
| GeoRAM mask (C64_REU_SIZE), cartridge ROM place, freeze button, the type itself | live | the firmware's and the host's |
| REU fitted, its size, the parked REU and the enable grace | live | C64_REU_ENABLE and C64_REU_SIZE are the firmware's |
| Bus sharing (C64_BUS_INTERNAL/EXTERNAL/BRIDGE), forced ULTIMAX, the C64_MODE NMI | live | the firmware's registers |
| Drives A and B, the IEC processor, folders, 1581 controllers | live | the firmware mounts and owns them. Live nodes go into the anchor, so TRX64's device map comes out right, and the live drives are put back exactly |
| Turbo enable word, menu speed, the bridge's turbo latches | live | the firmware's registers. Only `$D030`/`$D031` come from the anchor |
| Ultimate Audio sampler | live | one register file for the C64 (voices 0-6) and the firmware (Play MOD drives the same voices); FPGA-clocked, it runs on while the C64 stands, like reSID's voices |
| UltiSID and mixer settings, SID decode, palette | live | the firmware's core registers |
| Paddles and mouse, joysticks, keys | live | the host's input; put back on top after the restore |
| TRX S37 block (`--trx-io`) | live | the firmware's registers on the RISC-V side; the C64 does not reach it |
| Model (PAL/NTSC) | refused | C64_VIDEOFORMAT is the firmware's. An anchor of another model is refused |

**Writes from outside are not logged.** The firmware's own CPU writes guest DDR directly: a REU preload, the CRT
loader, the EEPROM load and save, the firmware's REU save reads. They are the outside world, as T20 marks them, and
going back does not undo them. A byte the firmware wrote and the C64 wrote later comes back as the C64's old byte.

**A new machine.** A cartridge type the firmware sets that differs from the live one, and a cartridge put into or taken
out of the expansion port, drop the anchors and the log, as a reset does (T20 §4.5: insert/eject is a discard).

### 4.2 The UCI block

TRX64 resets the block to power-on at every restore (Spec 852 D7: its other half is the firmware's). The bridge keeps a
copy of the block with each anchor while it is enabled (copies that agree are shared). The anchor's copy goes back
only when:
- the anchor's block is idle: command state idle, no handshake bit, no data valid, no error, no event for the
  firmware;
- the live block is idle the same way and the C64 is not waiting for the firmware;
- the firmware's setup is the same (enable, place, bus id, routing, interrupt mask).

Then the firmware's half holds nothing either, and what comes back is the C64's: a command half written into the
buffer, the command interrupt enable. Otherwise the live block stays and the restore says why. The copy is the
session's: a `.c64rering` does not carry it.

### 4.3 The picture

TRX64's anchor carries no picture (`omit_framebuffer`, TRX64 Spec 807). The daemon draws one after a restore: it runs
one or two frames and restores again (Spec 808, `transport_move_to`). Here such a frame would reach the cartridge, the
REU, the UCI, the drives and the sampler, which a second restore does not undo. So the bridge keeps the picture itself:

- **At each anchor** it keeps the VIC's `displayed` buffer (520 × 312 colour indices, the last complete frame), packed
  (PackBits), in the anchor's session entry beside the UCI copy. A picture equal to the one before shares it.
- **At `goto`, `frame` and `ringload`**, after the restore, it unpacks that picture into `displayed`. Nothing runs. The
  VIC's accumulator gets the same picture, except the line the VIC draws now, which is the anchor's (TRX64's VIC node
  carries it). The frame count already went up with the restore (§4), so the window, `png`, the UDP stream and, with
  `--trx-io`, the composite and the still take the new picture.
- **What it shows:** the frame that ended at the anchor, as it was on screen then, with every raster split, sprite
  multiplexer and border trick. The daemon shows the frame after the anchor instead (it draws on from there).
- **No picture kept** (an anchor from another session's `.c64rering`): TRX64's `render_index_buffer` draws one from the
  anchor's VIC registers, RAM, colour RAM and character ROM, and a note says so. It is one register state for the
  whole frame: no raster splits, and the VIC reads RAM where on the machine it may read a cartridge's ROM (Ultimax).

`rewind` prints the pictures held: how many distinct ones and their packed size.

**Notes.** What a restore could not put back is listed under the status line (`note: …`): no `ue2Hw` node, an anchor
the log no longer reaches, no DDR lent, another cartridge type, another CRT in the port, the UCI kept live, a picture
drawn from the registers (§4.3).

The bridge then re-anchors its clock and the SID stream at the anchor's cycle, as after a reset release. A physical
cartridge whose node came back has the anchor's epoch; otherwise its flash timers carry the cycles the C64 went back.

**Limits:**
- A command in flight in the UCI stays live (§4.2): going back across it leaves the C64 and the firmware out of step.
- Going back across a drive's serial transfer: the IEC lines come from the anchor and the drives are live; a drive
  mid-transfer can see a line it did not expect.
- The firmware's own writes into guest DDR are not undone (above).
- An anchor older than the log (more than 16 Mi changed bytes ago, about 18 s of REU DMA that changes every byte), an
  anchor of another session's file, and a mark that outlived a cut get TRX64's state and the cartridge registers, not
  the memories; the restore says so.
- The REU's shadow registers and its `$FF00` arming are not in TRX64's 16-byte node.
- GMod4's serial-flash command state is not restored (TRX64's `restore_cart_state` leaves `spi` out).
- UE2's reSID engines and the sampler keep their voice state; the SID registers in TRX64 are the anchor's. The sound
  follows once the program writes the SID again.
- The anchors carry no drive state, as in the daemon (`drive1541` null).
- The picture of an anchor from another session's file is drawn from its registers (§4.3): no raster splits.
- On a wrapped window (NTSC) the frame in progress at the anchor is not kept, only the one shown. The first frame after
  a release has, from its vsync line down, the lines of the frame before: one frame.
- `rstep` undoes C64 RAM from TRX64's delta ring, as before; UE2's memories stay where they are.
- The delta ring and the CPU history are not cut with the anchors (as in the daemon). After a release, `whowrote` can
  name a write of the abandoned future.

## 5. Monitor verbs

Over the control port (`monitor <line>`) and the MCP tool `emu_monitor`, the same text as the daemon:

| Verb | Where | Notes |
|---|---|---|
| `whowrote`, `triage`, `revdepth` | the library | read the machine's rings |
| `rstep [n]`, `reverse` | ours (the daemon's arm) | halts the C64 first. Refused while on an anchor: the delta ring ends at the head |
| `chis [cycles] \| chis s e` | ours | the live CPU history; no trace fallback |
| `mark`, `marks`, `unmark` | ours | TRX64's marks: a name and a pin, at most 32, carried by `ringdump` |
| `goto <frame\|c<cycle>\|@<cycle>\|mark>`, `frame [-N\|+N]`, `rewind`, `run` | ours | `frame +N` at the head is refused (the firmware runs the C64 there; `c64 step` steps it) |
| `cadence [frames] [seconds]`, `window [seconds]` | ours | retune the cap and budget together |
| `ringdump <path> [board]`, `ringload <path>` | ours | TRX64's `.c64rering`; `board`: the board's format (§7) |
| `asm-file <path> [addr]`, `pwd`, `cd`, `ls` | ours | the library's assembler; files under `--monitor-dir` |

- Not here: `play`, `pause` (wall-clock pacing), `rawframe` (UE2 shows the frame that ended at the anchor, §4.3, and
  draws no other), `diff`, `cdiff`, thumbnails, `buildtrace`.
- `on_effect` is implemented, and `files()` gives the library our `Files`. The library calls neither `timeline()`
  nor `files()` in v0.12.3, so the verbs are ours.
- The VICE binary monitor port has no time-travel commands and is unchanged.

## 6. Files

`--monitor-dir DIR` (default: the directory ue2emu was started in) is the root of every monitor file verb:

- A path is relative to `cd`, or starts at the root with `/`.
- `..` above the root is refused, and so is a symbolic link that leads out. A host path is only a name under the
  root.
- `mount` and `eject` through the `Files` trait are refused: the firmware owns the drives.

## 7. The board (TRX64-Ultimate) and clients

- **Verbs and output:** UE2 and trxmon both copy TRX64's daemon text. The status line
  (`MODE glyph  frame i/n   now|-x.xs`), `MARK … @ Cycle … Frame …`, `RINGBUFFER SIZE …`, `reverse-step: …` and
  `ringdump: …` read the same.
- **Where trxmon differs from UE2:**
  - trxmon has no `cadence`/`window`: the board captures every frame;
  - its `rstep` replays from a frame;
  - its marks are not pinned and not in files;
  - its `chis`/`whowrote` wait for the trace engine;
  - its `ringdump` takes `[seconds] [stride]`.
- **`.c64rering`:** both write `C64RERNG`, version 1, gzip JSON `{checkpoint_ring, delta_ring, cpu_history}`.
  - UE2 writes TRX64's own: compressed gzip, TRX64's checkpoint nodes, a real `turbo` node (`enable`, `menuIndex`,
    `menuStalls`, `d031`, `d030`, `resetHold`), labels, the delta ring and the CPU history.
  - The board (`trx_c64re.cc`) writes stored-block gzip, at most 64 anchors, no labels, empty delta and CPU rings,
    `trx64Hw` nodes and a constant `turbo` node. It refuses compressed files, so UE2's own files do not load on
    `trx64/main`.
- **Board files in UE2** (built 2026-10-04, see "As built"):
  - `ringload` takes a board file. Each anchor gets its RAM under the I/O back from `trx64Hw.ramUnderIo` (the board
    puts colour nibbles at `$D800` of `ram`). The notes say what the file lacks: no SID, IEC, interrupt-line,
    keyboard, expansion-port, `ue2Hw` node; the VIC node has registers and colour RAM only (raster compare 0,
    pipelines cold); TRX64 converts the v2 CIA records; no turbo node (or the board's constant). The picture is
    drawn from the registers (§4.3).
  - `ringdump <path> board` writes the board's format, but only for the anchors a board file brought. The board
    loads nothing without a `trx64Hw` node, and that node is the FPGA's chip block: the CPU's and the CIAs'
    flip-flops, staged straight into them. UE2 cannot build it from TRX64's state, so UE2's own anchors stay out.
- **Clients:**
  - To the board: REST `/v1/timewarp:*` and `/v1/machine:readmem?lens=`, plus trxmon's WebSocket JSON-RPC on port
    4312.
  - To UE2: the control port's `monitor` lines or the MCP tool `emu_monitor`, as text.
  - Since S40 UE2 serves the board's engine and state bus (CAPS `$E7` with `--trx-io`): the firmware's own
    `/v1/timewarp:*` and the lens route answer as on the board. trxmon needs CAPS b3 (S39 phase A2).
- **The board ring does not pause in turbo or on a reset** (no such code in `trx_ring.cc`). UE2 follows TRX64 and
  Alex's ruling: it stands in turbo, while held and on a reset. With `--trx-io` there is no turbo (S40 §0).

## 8. Cost

60 s emulated at `--speed max`, upstream ELF, a copy of `run/flash.bin`, `--c64-roms`, the C64 at BASIC READY. Three
runs each, `--ring=off` and `--ring` taken in turn:

| | wall s | max RSS |
|---|---|---|
| `--ring=off` | 6.16, 6.45, 6.01 (mean 6.21) | 98 MiB |
| `--ring` | 7.36, 7.05, 6.80 (mean 7.07) | 481-487 MiB |

- Wall time is +14 %, about 14 ms of host CPU per emulated second (the first S38 measurement, before §4.1, was +13 %
  at 5.56 s / 6.29 s; this machine measured both modes slower on the day).
- Memory is +385 MiB: the delta ring and the CPU history (about 116 MiB, allocated at once), about 260 MiB of anchors
  with a full 60 s window, and about 1 KiB of `ue2Hw` node and side entry per anchor.

With a 128 KB REU (`scripts/smoke-c64-ring-reu.cfg`), a BASIC loop stashing 64 KB per pass, 78 s emulated:

| | wall s | max RSS | write log |
|---|---|---|---|
| stash from RAM that hardly changes, `--ring=off` | 8.87, 8.63, 8.74 (mean 8.75) | 98 MiB | |
| the same, `--ring` | 9.51, 9.13, 9.20 (mean 9.28) | 416 MiB | 162 K records, 1.2 MiB |
| two stashes per pass from different RAM, nearly every byte changes, `--ring=off` | 8.50, 8.66, 10.68 (mean 9.28) | 98 MiB | |
| the same, `--ring` | 9.47, 10.01, 11.01 (mean 10.16) | 636 MiB | at its cap: 16 Mi records, 128 MiB |

- An unchanged byte is no record, so a REU that rewrites what it holds costs nothing in the log.
- The worst case fills the cap: about 900 K changed bytes per second, so the log reaches about 18 s back, not the
  anchors' 60 s. Older anchors restore without the memories and say so.
- Fewer anchors than in the idle run: a 64 KB transfer is one instruction of 65 536 cycles, and the anchor lands after
  it (the range line shows the gaps).
- An EasyFlash that writes was not measured with the firmware (no test cartridge writes continuously). Its writes take
  the same tap as cart RAM, GeoRAM and the REU (`CartLogic::ddr_write`), one record each.

The pictures (§4.3), 66 s emulated at `--speed max`, a BASIC loop that counts `$0400`, so nearly every frame is a
new picture. Two runs each, without the pictures (the commit before) and with them, taken in turn:

| | wall s | max RSS | pictures |
|---|---|---|---|
| without | 8.08, 7.86 | 481-483 MiB | |
| with | 8.49, 8.02 | 508-515 MiB | 2 741 distinct, 17.3 MiB packed |

- About 6.5 KiB a text-screen picture packed; an unchanged picture costs a 162 KB compare and no memory.
- A full-screen picture that changes every frame (bitmaps, scrollers) packs worse; at most 162 KB a frame plus 1/128,
  about 470 MiB for 3000 anchors. Not measured with such a program.

## 9. Checks

- **Bridge unit tests** (`c64-bridge`):
  - records at 1 MHz, one anchor per frame; stands in turbo, under C64_STOP, in the machine pause and in reset; a
    reset drops all three rings;
  - `--ring=off` records nothing;
  - `goto` restores RAM, registers and cycle, holds while time passes, gives the same machine twice; the release
    cuts after the anchor, keeps the mark, and runs on from the anchor's cycle;
  - the UCI and the turbo enable word stay live; an anchor of another model is refused;
  - `ringdump`/`ringload` round-trip anchors, marks and both rings;
  - §7: the board's file (fixture `crates/c64-bridge/tests/data/board_2026-10-04.c64rering.gz`, the TRX64-Ultimate
    test file gzipped, 110 KB) loads: 9 anchors, the current one at READY, the RAM under the I/O, the board's
    border and background drawn, the notes, the firmware's turbo menu kept; `goto` reaches the others; the ring
    records past the file's cap of 9 afterwards. With the ROMs the C64 runs a line from the keyboard buffer.
    `ring_dump_board` writes the board anchors only (marks kept, the transport's anchor as current) and refuses a
    ring of UE2's own; the file passes `check_board_ring` and loads in UE2 again;
  - `board.rs`: `check_board_ring` passes the board's own file and refuses what the board's loader refuses
    (compressed body, no `trx64Hw`, layout, short arrays, snap position, missing current entry); the writer's
    stored blocks read in TRX64 too; 65 entries are refused, 64 pass;
  - §4.1: going back across a bank switch (Ocean) restores the bank and the C64 reads and runs in the old one; across a
    REU STASH restores the REU's bytes, and forward again redoes them; across an EasyFlash program restores the flash
    byte in DDR; the GeoRAM page and its bytes come back; an idle UCI with two command bytes written comes back, and a
    command in flight keeps the live block with a note; a physical EasyFlash's bank and programmed byte come back and
    `generation` moves; a new cartridge type, an insert and an eject drop the anchors, the same type does not; the
    `ue2Hw` node is in a `.c64rering`, TRX64's own loader restores an anchor with it, and a load in the same session
    puts the REU back through the log;
  - `hwlog.rs`: undo and redo, pokes for the other spaces, nothing moves without DDR; a cut, the cap and a new log
    uncover the anchors they leave behind;
  - §4.3: `goto` an anchor taken before the border changed shows that anchor's picture; showing it again, kept or
    drawn from the registers, leaves TRX64's whole checkpoint, the write log, the cartridge registers and the UCI
    status as they were; a release runs on to the anchor's picture; a `ringload` in the same session shows the
    session's picture of the file's anchor; PackBits round-trips runs, literals and refuses a wrong size.
- **Monitor tests** (`ue2emu`):
  - the ring verbs on a booted C64; `ringdump … board` refuses UE2's own anchors;
  - files confined to their directory (`..`, `/..`, a symbolic link, a host path);
  - `asm-file` assembles;
  - `ring = false` in TOML;
  - the port audit's list.
- **Smoke** `scripts/smoke-c64-ring.ctl` (in `smoke-c64-all.sh`), with the firmware:
  - a BASIC loop counts `$0400`; `mark`, then `goto` the mark twice gives the same byte, not the live one;
  - `ringdump`/`ringload`;
  - `asm-file` assembles `ring.asm`;
  - §4.3: a `png` while `20 goto20` runs (a still screen) at `mark still`; at the end `goto still` gives the same png,
    not the counter's live one.
- **Smoke** `scripts/smoke-c64-ring-board.ctl` (in `smoke-c64-all.sh`, `--settings smoke-c64-ring-board.cfg`,
  System Mode PAL): `ringload` of the board's file (unpacked from the fixture), `goto 4`, `ringdump … board` (header
  and first stored block checked), `run`, then `poke1024,1` typed runs: the keyboard scan works on the board's CIA
  state.
- **Smoke** `scripts/smoke-c64-ring-reu.ctl` (in `smoke-c64-all.sh`, `--settings smoke-c64-ring-reu.cfg`, a 128 KB
  REU): a BASIC loop keeps its counter in the REU (fetch, add one, stash). After `goto` a mark it runs on from the
  mark's count, not from the live count it would fetch from an REU that stayed live; no `note: UE2 memories` line.

- `cargo test --workspace --all-features` (585 passed with §4.3; 583 with §4.1; 573 before), `cargo build --release` (only the old
  `transparent` warning), `smoke-all.sh`, `smoke-c64-all.sh` (16 runs, `ring-reu` among them), `smoke-trx.sh` and
  `smoke-trx-freeze.sh` pass. With §7: 596 tests, `smoke-c64-all.sh` 17 runs (`ring-board` added).

## 10. TRX64 gaps (met on UE2's side; TRX64 not patched)

1. **A restore that keeps the host's parts.** `restore_runtime_checkpoint` detaches the cartridge and resets the
   UCI block. A `RestoreOpts { keep_cartridge, keep_uci, keep_drives, keep_expansion }` would replace the bridge's
   take-out-and-put-back and the drive clones.
2. **`Machine::expansion_ram_uncovered`** has no public setter (`pub(crate)`). After a UE2 restore it reads true
   although UE2's log put the REU's memory back.
3. **The ring verbs in `trx64-monitor`.** `goto`, `frame`, `mark`, `marks`, `unmark`, `rstep`, `chis`, `cadence`,
   `window`, `rewind`, `ringdump`, `ringload` and `asm-file` are daemon arms. UE2 copies them. The `Timeline` trait
   has no frame step, cadence or window, and no verb calls it or `Files`.
4. **Ring sizes from the environment.** `Machine::new` reads `TRX64_CPUHISTORY` and `TRX64_REVERSE_SECONDS`. The
   bridge sets them process-wide to avoid a 110 MiB allocation it would throw away; a constructor parameter would do.
5. **Cutting the per-instruction rings with the anchors** (a `DeltaRing::truncate_after(cycle)`), so `whowrote`
   after a release does not name the abandoned future. The daemon has the same hole.
6. **A whole-machine hold** (S37's gap): the transport hold is the bridge's.
7. **The REU's whole continuation in the anchor.** `expansion.regs` is the 16 readable registers; the shadow
   registers (autoload) and the `$FF00` arming are not in it.
8. **A UCI snapshot.** `Uci` has no serialized form and no setter for its fields, so UE2 keeps whole clones per
   anchor in memory and a `.c64rering` carries none. A `uci` node (`capture`/`restore` of the C64 side, refused while
   a command is in flight) would let files carry it.
9. **`restore_cart_state` leaves `spi` out**, so GMod4's serial-flash command state does not come back.
10. **`CartMapper::get_state` is `&self`**, while `Flash040::snapshot_state` needs `&mut` to catch the erase alarm up:
    a busy chip is cloned whole (array included) for its state. A `&self` snapshot of a caught-up chip would do.

## 11. Open

- REU and cartridge writes in the `.c64rering`: the log is the session's, so a file restores TRX64's state and the
  cartridge registers but not the memories (a note says so). TRX64's `RingBufferDump` has no place for it.

- UE2's own anchors on the board: **accepted limit for now** (Alex, 2026-10-04). The board's loader refuses a payload
  without `trx64Hw` (§7), so `ringdump … board` writes only anchors that came from a board file. The two ways, if it
  is taken up again:
  - UE2 builds the chip block from TRX64's state: the 6502's 209 flip-flops (`cpu6510.v`/`proc_core.vhd`) and each
    CIA's 363 (`cia_core.sv`, reDIP-CIA's timer, TOD, serial and interrupt pipelines) at a snap edge, line 0
    cycle 12 or later. Verifiable only in the RTL bench (`tb_ring`).
  - The board builds the block from the nodes (T20 §9.4 plans it), which is a board patch.

**Accepted limits** (Alex, 2026-10-04): a UCI command in flight stays live; a serial transfer across a goto (drives
live, IEC lines from the anchor); the firmware's own writes into DDR are not undone; reSID and sampler voices keep
their live state; `rstep` undoes C64 RAM only; the REU's shadow registers and `$FF00` arming; GMod4's serial-flash
command state.

## As built

### §7 (board files)

Read from the app's `trx64/main` at 029da8ef, `software/io/c64/trx_c64re.cc` (code, not its comments):

| Rule | Where |
|---|---|
| Writer: at most 64 entries | `MAX_RING_ENTRIES`, 576; the loop 591 |
| Container `C64RERNG` 1, gzip header without flags, stored blocks of 16 KiB, CRC-32 and ISIZE | 644, 180, 144 and 202-215, 327-341 |
| Entries: `id`, `frame`, `cycles`, `pinned: false`, no `label`; empty delta and CPU rings | 657-674 |
| Turbo node: a constant | `write_turbo`, 458-461 |
| Loader: `C64RERNG` 1, at least 27 bytes | 848 |
| A block with BTYPE other than stored is refused; ISIZE bounds the body | `gunzip_stored`, 723, 737-748 |
| Only the entry `checkpoint_ring.current_id` names is loaded | 895-915 |
| Keys are found by name; others are passed over | `key`, 768-779 |
| Turbo node: read only for a message | 922-927 |
| `trx64Hw` required, `layout` 1; `ram` 65 536 bytes, `block` 232, `ramUnderIo` and `colorRam` 1024 | 928-943, `tok_ta` 800-827 |
| `ram[$D800..]` ← `ramUnderIo` | 945 |
| Snap position `x` 96-504 | 949-951 |

- The block's CPU and CIA words go straight into the flip-flops (`trx_ring.cc` `stage_and_load`, 697; `put_back`,
  873); `c64_core.v` loads `s_cpuf[208:0]` and `s_cia1f`/`s_cia2f[362:0]` from them.
- `ue2Hw` and other unknown keys would not hurt the board (`key` by name), but the board needs `trx64Hw` anyway, so
  the writer only carries board anchors; they have no `ue2Hw`.
- `board.rs`: `adopt` (ring_load), `notes` (restore_c64), `write_board_ring` (stored gzip, the board's block size),
  `check_board_ring` (the loader's rules in order, with its messages).
- `ring_load` raises the file's entry cap to the ring's window and cadence: the board's file says 9.
- `restore_c64`: an anchor without a turbo node gets `$D030`/`$D031` = `$00` and keeps the firmware's enable word and
  menu speed (TRX64 would reset them); TRX64's own restore notes (a v2 CIA converted) now reach the monitor.
- **Smoke** (upstream ELF, PAL): `ringload` 9 anchors with the notes, `goto 4`, `ringdump … board` 9 anchors in
  874 034 bytes, the header and first stored block as the board's own; after `run`, `poke1024,1` typed reads back
  `01`.

- `Trx64Backend::set_ring` sizes the rings and `ring_recording` gates them. `run_recording` stops at each frame
  boundary. `ring_goto`, `ring_release`, `ring_dump` and `ring_load` are the transport. `restore_c64` is §4.
- **The depth.** `Trx64Backend::new` still builds the rings off (one entry). It keeps the user's
  `TRX64_REVERSE_SECONDS` before it sets its own, and `set_ring` uses it.
- **The range line** counts a gap only when anchors are more than a raster line further apart than the cadence. An
  anchor lands on the first instruction boundary after line 0, a few cycles either way; the daemon's strict check
  would call every second anchor a gap.
- **Smoke** (upstream ELF, NTSC by the flash's System Mode):
  - 666 anchors over 11.1 s, one gap of two frames (a stop of the firmware's own);
  - `$0400`: 93 live, 5b after `goto here`, 5b after 500 ms held, 5b after `run` and `goto here` again;
  - `ringdump`: 726 anchors, 3 116 300 delta entries, 262 144 CPU history entries, 35.9 MB;
  - `asm-file`: `a9 01 60` at `$c000`.

### §4.3 (the picture)

- `Ring::picture` packs `vic.displayed` at `ring_frame` (PackBits, `Picture`); the side entry holds it. `ring_goto`
  calls `show_picture` after `restore_c64`; `ring_load` keeps this session's pictures by anchor id.
- `show_picture` writes only `vic.displayed` and `vic.dbuf` (minus the draw line the anchor carries). A unit test
  checks that TRX64's whole checkpoint, the write log, the cartridge registers and the UCI status do not move.
- **Smoke** (upstream ELF, NTSC): `ring-goto.png` equals `ring-mark.png` byte for byte, and differs from
  `ring-live.png`. The commit before shows the live frame (`ring-goto.png` equals `ring-live.png`). With the TRX
  firmware and `--trx-io` (960 × 540 composite) the same holds.

### §4.1 (UE2's part)

- `hwlog.rs` is the log; `Ring` keeps a side table (anchor id, log position, UCI copy), swept every 64 captures.
  `ring_frame` builds the `ue2Hw` node after the physical cartridge's image went to the log, so the node's position
  covers it. `ring_goto` takes the image once more when it leaves the head.
- `restore_c64` takes the expansion node from the anchor with the live size (live port when either side has none),
  restores TRX64's state, puts back the UCI copy or the live block, then `restore_ue2`: the log to the anchor's
  position (DDR here, the EEPROM and the physical cartridge through pokes), the cartridge registers, the physical
  node and its epoch.
- `ring_release` cuts the log at the anchor; a mark that outlives the cut is uncovered for the memories.
- `ring_load` rebuilds the side table from the file's nodes whose log is this session's, so a dump loaded back in the
  same session reaches the memories.
- The flash boards in `slot.rs` got `get_state`/`set_state` and `set_writable_image`; before, they returned an
  empty `CartState` and ignored a written image.
- **Smoke** (upstream ELF, NTSC): the REU counter read `51` live, `1c` after `goto here`, `1e` 100 ms after `run`.
  The log held 30 records after the cut.
