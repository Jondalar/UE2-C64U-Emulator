# S38 — TRX64's rings on the C64 (time travel)

**Status:** built (2026-10-04), local branch `s37-trx-pixel-layer`, not pushed. See "As built" at the end.

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
- `crates/c64-bridge/src/ring.rs` (new): the checkpoint ring, capture, restore, dump and load.
- `crates/c64-bridge/src/lib.rs`: `advance_to` records; a reset drops the anchors.
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
- A key press or a firmware DMA write while it stands does not cut. The daemon cuts on a key.

**From the anchor (the C64's own state):**
- the 64 KB RAM and colour RAM;
- the CPU and the CPU port;
- CIA 1 and 2;
- the VIC, with `$D030`/`$D031`;
- the SID registers in TRX64;
- the IEC lines and the interrupt state;
- the keyboard, then UE2's held keys and joysticks are put back on top;
- the POT lines.

**Kept live (the firmware's; the restore must not touch them):**

| Part | Why |
|---|---|
| Cartridge: UE2's cart logic, banking, flash, EEPROM, the physical slot | the FPGA's cart model, set by the firmware. TRX64's restore detaches a cartridge the anchor does not carry: the bridge takes it out first and puts it back |
| REU / GeoRAM registers and RAM | the RAM is DDR, the firmware's memory. The anchor's `expansion` node is replaced by the live one |
| UCI block | half of the handshake lives in the firmware. TRX64 resets it to power-on: the live block is put back |
| Drives A and B, the IEC processor, folders, 1581 controllers | the firmware mounts and owns them. Live nodes go into the anchor, so TRX64's device map comes out right, and the live drives are put back exactly |
| Turbo enable word and menu speed | the firmware's registers. Only `$D030`/`$D031` come from the anchor |
| Model (PAL/NTSC) | C64_VIDEOFORMAT is the firmware's. An anchor of another model is refused |

The bridge then re-anchors its clock, the SID stream and the physical cartridge's timers at the anchor's cycle, as
after a reset release.

**Limits:**
- A program sees the live cartridge banking, REU contents and drive state with the anchor's RAM and CPU. Going
  back across a bank switch, an REU transfer, a serial transfer or a UCI command leaves the two sides out of step.
- The IEC lines come from the anchor and the drives are live: a drive mid-transfer can see a line it did not expect.
- UE2's reSID engines keep their voice state; the SID registers in TRX64 are the anchor's. The sound follows once
  the program writes the SID again.
- The anchors carry no drive state, as in the daemon (`drive1541` null).
- The picture is not redrawn while the C64 stands: the window shows the last frame before `goto`. The daemon runs a
  frame and restores again. Here that frame would reach the cartridge, REU and UCI, which a second restore cannot
  undo. Screen RAM (`c64screen`, `m`) is right at once.
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
| `ringdump <path>`, `ringload <path>` | ours | TRX64's `.c64rering` |
| `asm-file <path> [addr]`, `pwd`, `cd`, `ls` | ours | the library's assembler; files under `--monitor-dir` |

- Not here: `play`, `pause`, `rawframe`, `diff`, `cdiff`, thumbnails, `buildtrace`. They need wall-clock pacing,
  pictures or traces that UE2 does not keep.
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
    `trx64Hw` nodes and a constant `turbo` node. It refuses compressed files, so UE2's files do not load on
    `trx64/main`.
  - Whether a board file restores in UE2 is untested.
- **Clients:**
  - To the board: REST `/v1/timewarp:*` and `/v1/machine:readmem?lens=`, plus trxmon's WebSocket JSON-RPC on port
    4312.
  - To UE2: the control port's `monitor` lines or the MCP tool `emu_monitor`, as text.
  - On UE2 the firmware's own `/v1/timewarp:*` answers 503 "No frame ring in this bitstream": the routes need CAPS
    b2 (engine), and the lens route needs CAPS b1. UE2's CAPS (`$E1` with `--trx-io`) has neither, and trxmon needs
    CAPS b3. UE2 does not fake them: the engine is not modelled.
- **The board ring does not pause in turbo or on a reset** (no such code in `trx_ring.cc`). UE2 follows TRX64 and
  Alex's ruling: it stands in turbo, while held and on a reset.

## 8. Cost

60 s emulated at `--speed max`, upstream ELF, a copy of `run/flash.bin`, `--c64-roms`, the C64 at BASIC READY. Three
runs each:

| | wall s | max RSS |
|---|---|---|
| `--ring=off` | 5.55, 5.54, 5.59 (mean 5.56) | 98 MiB |
| `--ring` | 6.50, 6.20, 6.18 (mean 6.29) | 478 MiB |

- Wall time is +13 %, about 12 ms of host CPU per emulated second.
- Memory is +380 MiB: the delta ring and the CPU history (about 116 MiB, allocated at once), and about 260 MiB of
  anchors with a full 60 s window.

## 9. Checks

- **Bridge unit tests** (`c64-bridge`):
  - records at 1 MHz, one anchor per frame; stands in turbo, under C64_STOP, in the machine pause and in reset; a
    reset drops all three rings;
  - `--ring=off` records nothing;
  - `goto` restores RAM, registers and cycle, holds while time passes, gives the same machine twice; the release
    cuts after the anchor, keeps the mark, and runs on from the anchor's cycle;
  - the UCI and the turbo enable word stay live; an anchor of another model is refused;
  - `ringdump`/`ringload` round-trip anchors, marks and both rings.
- **Monitor tests** (`ue2emu`):
  - the ring verbs on a booted C64;
  - files confined to their directory (`..`, `/..`, a symbolic link, a host path);
  - `asm-file` assembles;
  - `ring = false` in TOML;
  - the port audit's list.
- **Smoke** `scripts/smoke-c64-ring.ctl` (in `smoke-c64-all.sh`), with the firmware:
  - a BASIC loop counts `$0400`; `mark`, then `goto` the mark twice gives the same byte, not the live one;
  - `ringdump`/`ringload`;
  - `asm-file` assembles `ring.asm`.

- `cargo test --workspace --all-features` (573 passed), `cargo build --release` (only the old `transparent` warning),
  `smoke-all.sh`, `smoke-c64-all.sh`, `smoke-trx.sh` and `smoke-trx-freeze.sh` pass.

## 10. TRX64 gaps (met on UE2's side; TRX64 not patched)

1. **A restore that keeps the host's parts.** `restore_runtime_checkpoint` detaches the cartridge and resets the
   UCI block. A `RestoreOpts { keep_cartridge, keep_uci, keep_drives, keep_expansion }` would replace the bridge's
   take-out-and-put-back and the drive clones.
2. **`Machine::expansion_ram_uncovered`** has no setter. After a UE2 restore it reads true although the REU is
   live and whole.
3. **The ring verbs in `trx64-monitor`.** `goto`, `frame`, `mark`, `marks`, `unmark`, `rstep`, `chis`, `cadence`,
   `window`, `rewind`, `ringdump`, `ringload` and `asm-file` are daemon arms. UE2 copies them. The `Timeline` trait
   has no frame step, cadence or window, and no verb calls it or `Files`.
4. **Ring sizes from the environment.** `Machine::new` reads `TRX64_CPUHISTORY` and `TRX64_REVERSE_SECONDS`. The
   bridge sets them process-wide to avoid a 110 MiB allocation it would throw away; a constructor parameter would do.
5. **Cutting the per-instruction rings with the anchors** (a `DeltaRing::truncate_after(cycle)`), so `whowrote`
   after a release does not name the abandoned future. The daemon has the same hole.
6. **A whole-machine hold** (S37's gap): the transport hold is the bridge's.

## 11. Open

- Redraw the picture on `goto` without reaching the firmware's parts (needs gap 1, or a VIC-only re-run).
- A board `.c64rering` into UE2, and UE2's into the board once its loader takes compressed gzip.
- Whether `ringdump` should also write the board's subset, so a file from UE2 loads on `trx64/main`.

## As built

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
