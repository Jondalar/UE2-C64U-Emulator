# S23 — The monitor: TRX64's verbs, our own, and a VICE-compatible port

**Status:** built (2026-09-20).

**Owns:**
- `crates/c64-bridge/src/monitor.rs` (new): the C64 and drive views, and the accessor the host needs
- `crates/ue2emu/src/monitor/` (new): the `MonitorHost` implementation, our verb table, the VICE server
- `crates/ue2emu/src/control.rs`: the `monitor <cmd>` line
- `crates/ue2-mcp/src/tools.rs`: `emu_monitor`
- `docs/status/monitor.md` (new)

**Reads:** TRX64 Spec 864 (`docs/864-the-monitor-as-a-library.md`) and `crates/trx64-monitor/src/host.rs` in the
TRX64 tree; S14 (the bridge), S08 (the control protocol), S15 (UCI), S21 (settings); for §8 the VICE tree's
`doc/vice.texi` chapter "Binary monitor" and `src/monitor/monitor_binary.c` (VICE 3.10).

UE2 has no way to look at the 6510 today: no registers, no memory, no disassembly, no breakpoints. TRX64 has all
of that in its daemon's monitor, and Spec 864 moves it into `trx64-monitor`, a library over a host trait. UE2
becomes its second host. We add the verbs for the Ultimate side, which is ours alone, and one compatibility
surface: the VICE binary monitor protocol, so third-party C64 debuggers can attach.

## 1. Stages

| Stage | Result |
|---|---|
| M1 | The bridge host: `machine()`, the C64, drive A and firmware views, `step`. `monitor <cmd>` on the control port, `emu_monitor` over MCP. |
| M2 | Our verbs: `fw`, `itu`, `cart`, `flash`, `sd`, `usb`, `net`, `audio`, `clock`, `config`. |
| M3 | Run control: `resume`/`set_halted`/`on_stop`, breakpoints through TRX64's debug gates. |
| M4 | The VICE binary monitor port, for the C64 core only (§8). |

M1-M3 wait for the tag TRX64 promised: the point where the verb table has moved and the golden transcript is
still byte-identical. Until then `host.rs` is stable enough to build against and not stable enough to pin
(their words, and our experience with `--c64 none` says the same about any moving surface).

## 2. Where the host lives

`ue2-core` must not learn what TRX64 is — that is what keeps `--c64 none` and the T0 register tables honest — so
nothing of the monitor goes there.

| Piece | Crate | Why |
|---|---|---|
| `CpuView` for the C64 and drive A, and an accessor for `trx64_core::Machine` | `c64-bridge` | It owns the TRX64 machine; drive A *is* `Machine::drive8` (drive.rs swaps the real drive in and out of it). |
| `MonitorHost`, our verb table, `config`, the VICE server | `ue2emu` (feature `trx64`) | Only here are both machines visible: `ue2_core::Machine` for the firmware and its devices, and the bridge for the C64. |
| `monitor <cmd>` | `ue2emu/src/control.rs` | One more line of the protocol S08 already carries. |
| `emu_monitor` | `ue2-mcp` | A thin wrapper over that line, as `emu_cart_info` is over `cart-info`. |

## 3. Run control

A debug monitor for an Ultimate has to stop two things: the runtime the Ultimate's own application runs on — the
firmware's RISC-V — and the C64 behind it. They are not symmetric, and the asymmetry is the machine's, not a
choice made here.

**Stopping the firmware stops everything.** The C64 only advances because `run_cpu` in the bridge drives it, so a
held RISC-V holds the C64 with it. On hardware the C64 has its own crystal and would run on; here it does not.
That makes a firmware breakpoint a consistent snapshot of both — and it makes the bugs that live at the seam
between the two clocks (issue #2 was one: the C64 polls the UCI while the firmware is late) invisible while
stopped. Those are only visible running. The monitor says so when it holds, rather than leaving someone to wonder.

The machinery is the GDB stub's, which already does breakpoints, stepping and resume on the RISC-V; the monitor is
a second door onto it. The held state lives in the monitor and the run loop asks for it before every slice, so a
held machine still serves commands — there would be no way back otherwise.

**Stopping the C64 leaves the firmware running**, which is what a monitor wants when the question is about the C64.
And it **is** the stop the machine already has: `Hold::Cpu` in TRX64 is documented as "DMA / freeze / C64_STOP" —
one mechanism, which the firmware reaches by writing `C64_STOP`. So does the monitor, through the same register
and the same side effects. Two consequences follow, and both are deliberate:

- **The last writer wins.** The firmware releasing its DMA stop releases the monitor's halt too, and a monitor
  release ends a stop the firmware wanted. On hardware, poking that register does exactly this.
- **Every answer reports the state it read**, never the state it wrote. That is the only way the first point is
  survivable, and it makes the halt visible where the firmware looks: `cart` shows it.

Under `Hold::Cpu` the VIC, the CIAs, the SID and drive 8 keep running — only the 6510 stands. A halted C64's cycle
therefore keeps advancing, and that is correct, not a leak.

`resume(Forever)` is the one case this host cannot finish: the C64 runs because the firmware drives it, so the
monitor lets go of the stop and answers `Resumed`. The stop, when it comes, arrives asynchronously — that is what
the library's `on_stop` is for, and it is the piece that waits for TRX64 to move the run-control verbs into
`trx64-monitor`. The bounded forms (`Pc`, `Cycles`) and `step` are finished here and now, out of band, because a
bound is a question about the C64 alone. Reset stays the firmware's: the monitor does not reach around it.

Verbs: `fw halt | go | step [n]` for the firmware, `c64 [halt | go | step [n]]` for the C64, `status` for both.

TRX64's own spelling answers too, on the C64 — `g [addr]`, `x`, `until <addr>`, `z`/`step [n]`, `n`/`next [n]`,
`ret` — because the library does not carry them yet: it owns `bk`, but nothing in `trx64-monitor` calls
`MonitorHost::resume` or `::step`, so the run verbs are still in TRX64's daemon. Our dispatch only ever sees a line
the library declined, so when they land upstream the library answers first and ours fall away on their own. That
they mean the C64 is not a choice: `RunUntil::Pc` and `StopInfo::pc` are 16 bits.

## 4. Devices

| `device` | View | Address width | Flags | Debug gates | Notes |
|---|---|---|---|---|---|
| `c64` | the 6510 in the bridge | 16 | `NV-BDIZC` | yes | The default. |
| `drive8` | `Machine::drive8` | 16 | `NV-BDIZC` | yes | Same device on both hosts; drive A's power and hold come from the firmware. |
| `fw` | the RISC-V of `ue2_core::Machine` | 32 | none | no | Registers and memory yes; `d` answers that this device has no disassembler (we have none, and TRX64 will not take a second instruction set into `trx64-static`). Breakpoints stay with the GDB stub. |

`supports_debug_gates()` is false for `fw`, so the watch tables (`[u8; 0x10000]`, a 6502 shape) are refused by
name instead of watching the wrong 64 KB.

## 5. Our verbs

All read-only in the first cut except `config`, all `MachineEffect::Observes`. The library sees every line first
(it owns the modal state: `a`, a pending prompt) and returns `None` for a line it does not own; our dispatch takes
it from there, and appends its own section to `help`. No registration API upstream: TRX64 must not gain a notion of
this emulator — the dependency stays one-way, and their port audit stays their verbs while we audit ours.

| Verb | Shows |
|---|---|
| `fw` | RISC-V registers, the FreeRTOS task list (the GDB stub builds both today) |
| `itu` | interrupt controller by source name, the timers, the capability word |
| `cart` | the firmware's C64 register file at 0x10040000: mode, stop, cartridge type and variant, the cart ROM window, KERNAL, REU enable and size, sampler, serve-while-stopped, the clock-detect lines, and the command interface (today's `cart-info` is the physical cartridge only) |
| `flash` | SPI flash: the image behind it, what is not written out yet, how many config pages are in use (`config` reads their contents) |
| `dir [path]` | What is on a medium, read by the firmware's own file manager over the UCI transport, so every mount works and the answer is what its menu would show |
| `sd` | the card: sectors, size, write protect |
| `usb` | the hub ports and what is on each |
| `net` | the MAC the firmware programmed, the RX filter, the buffer queues, the TX register |
| `audio` | socket 1, the engines built with their model, the address windows routed to each chip |
| `clock` | the emulator clock, idle skip, and the lag between C64 and firmware time (invisible today, and the cause of issue #2) |
| `config` | §6 |

Every device verb reads what the firmware reads — the side-effect-free `peek8` of the GDB stub, or the device's own
state where the register is write-only — and decodes it with the firmware's own names (`itu.h`, `c64.h`,
`c64.cc`). Nothing is interpreted beyond that, so when a line looks wrong the machine is wrong. A machine without
the device names the missing one instead of printing zeroes.

Deliberately not ours: `reu`, `uci` and `turbo`. Those devices are the real ones here, and TRX64's own verbs
answer for them truthfully.

## 6. `config`

What the menu does, from the monitor. Paths are the emulated machine's, not the host's: `/flash/…`, `/Usb0/…`,
`/Temp/…`, the SD card.

| Command | Effect |
|---|---|
| `config [category [item]]` | Show. Values are decoded from the flash config pages with the S21 code (`ue2-core/src/settings.rs`). |
| `config set <cat> <item> <value>` | Change it in the running firmware: a two-line `.cfg` in `/temp` handed to the firmware with UCI `CTRL_CMD_LOAD_CONFIG` (0x50), which parses it and calls `effectuate()` per store. No USB stick and no network needed. |
| `config write` | Permanent, where the menu puts it: the config pages in flash. |
| `config write <path>` | The menu's "save to file": a `.cfg` at that guest path (`/Usb0/mine.cfg`, `/flash/mine.cfg`, `/temp/…`, the SD card). |
| `config read <path>` | The menu's load: that `.cfg` through UCI 0x50, so the firmware applies and effectuates it. |
| `config flash` | The raw page decode — "what is actually stored", the question Xander's two flash images raised. |

Rule, enforced and documented: `set` before `write`. Writing a page the firmware does not know about is undone the
next time the firmware saves that page from its own copy.

**The monitor does not write a guest filesystem itself.** Every `.cfg` it puts inside the machine goes in through
the firmware's own DOS target (`DOS_CMD_OPEN_FILE`/`WRITE_DATA`/`CLOSE_FILE`, target 2), which is how the cartridge
software does it. Two reasons, and both are decisive:

- The firmware has `/flash`, the stick and the SD card mounted, and FatFs keeps one sector of each volume in a
  window that it does not read again while it holds it (`move_window`). Bytes we change behind its back can go
  unseen — a directory entry we add most of all. A file the firmware writes has no such problem.
- One door serves every medium. `/temp`, `/flash`, `/Usb0` and the SD card all work with no writer of ours per
  medium, and a `--usb-dir` stick even reaches the host directory through the existing sync.

One rule comes with that door: a `DOS_CMD_WRITE_DATA` carries less than one 512-byte sector. FatFs hands a
full sector straight to the block device instead of copying it through the file's own DDR buffer, and the USB
controller fetches its data by physical address (`descr->memHi/memLo`) — which cannot reach the command interface's
register RAM, so a sector-sized write puts the firmware's bus contents on the stick instead of our text.

## 7. Surfaces

- **Control protocol:** one line, `monitor <cmd>`, answering the monitor's text. Marked address spans (Spec 804)
  stay on the wire and are stripped at display, so a later symbol source can use them.
- **MCP:** `emu_monitor { id, command }`, a wrapper over that line, so agents get the monitor without a second
  transport. Same text for a human and an agent, as C64RE does with `runtime_monitor`.
- **VICE binary monitor:** §8, its own TCP port, off by default.

## 8. The VICE binary monitor port (M4)

A compatibility surface, nothing more: it exists so that C64 Studio, IceBro Lite, VS64, VS65 and the .NET bridge
can attach to UE2 as they attach to VICE (and to Denise, which implements the same server side). It is not our
API and it must not shape the rest of the monitor.

- **Scope: the C64 core only.** Memspace 0 is the 6510, memspaces 1-4 are the drives (we have drive A). The
  RISC-V is not reachable over this port at all — the protocol's memspace byte names machines, not CPUs, and every
  register value on the wire is 16 bits. The firmware side stays on our own protocol.
- **Enabling:** `--vice-monitor [ADDR]`, default `127.0.0.1:6502`, off unless asked for. One client at a time, as
  in VICE.
- **Framing:** 11-byte request header (`0x02`, API version, length LE32, request id LE32, command) and 12-byte
  response header (the same plus response type and error code); events carry request id `0xFFFFFFFF`. We bound-check
  every field before reading it and copy strings out. VICE's own parser drops a byte on a bad magic and desyncs on
  a bad version; that part is not worth copying.
- **Commands.** Implemented: `PING` 0x81, `VICE_INFO` 0x85, `BANKS_AVAILABLE` 0x82, `REGISTERS_AVAILABLE` 0x83,
  `REGISTERS_GET` 0x31, `REGISTERS_SET` 0x32, `MEM_GET` 0x01, `MEM_SET` 0x02, `CHECKPOINT_GET` 0x11,
  `CHECKPOINT_SET` 0x12, `CHECKPOINT_DELETE` 0x13, `CHECKPOINT_LIST` 0x14, `CHECKPOINT_TOGGLE` 0x15,
  `ADVANCE_INSTRUCTIONS` 0x71, `EXECUTE_UNTIL_RETURN` 0x73, `EXIT` 0xAA, `RESET` 0xCC (through the firmware, §3),
  `KEYBOARD_FEED` 0x72. Refused with a clear error: `CONDITION_SET` 0x22 (`0x8F`; it is a wrapper over VICE's text
  grammar, and conditional breakpoints degrade gracefully), `DUMP`/`UNDUMP` 0x41/0x42, `RESOURCE_GET`/`SET`
  0x51/0x52, `CPUHISTORY_GET` 0x86 (VICE itself refuses it when the feature is off), `JOYPORT_SET`/`USERPORT_SET`
  0xA2/0xB2, `AUTOSTART` 0xDD, `DISPLAY_GET` 0x84 and `PALETTE_GET` 0x91 (§10), `QUIT` 0xBB (we detach; a debugger
  does not get to end the emulator).
- **Events.** The first contact sequence matters: clients send a command to interrupt a running machine and wait
  for `REGISTER_INFO` (0x31) then `STOPPED` (0x62) before the reply to their own command; `RESUMED` (0x63) on
  resume; `CHECKPOINT_INFO` (0x11) with the hit flag *before* the stop. We reproduce that order even though our
  "stopped" is the C64 only.
- **Banks.** `BANKS_AVAILABLE` is discoverable, so the C64 names (`default`/`cpu` = 0, `ram` 1, `rom` 2, `io` 3,
  `cart` 4) stay as clients expect them, and the Ultimate's own views (REU, the cartridge slot) can be extra named
  banks. This is the one place where the two designs fit without argument.
- **No disassembler on the wire.** Clients read bytes with `MEM_GET` and disassemble themselves.

**Checkpoints stop where they should.** A checkpoint is armed on the machine itself, not checked by the run loop
between slices: the bridge's watch table gains the checkpoint's addresses and its observer ends the run at the
access, which is the only hook that can. The two kinds go through different doors, because an instruction FETCH never
reaches `on_access` — that hook is the load/store path. Load and store are the access watch and the observer; exec
is TRX64's own `exec_watch`, checked at the instruction boundary before the instruction runs. `exec_watch` ends
the run with `RunStop::Observer` and not `Breakpoint` (only the `HashSet` form gives that), and `Observer` is also
what an access watch returns — the UCI's own control-register halt included — so an exec hit is a stop whose
boundary PC is in the exec table, and nothing else. The gate is taken off again when the client leaves, so
a machine nobody debugs runs exactly as it did. Both of the bridge's observers carry the gate — the one for plain
runs and the one for cartridge runs — so a checkpoint fires with a cartridge on the bus as well as without one; the
merged watch table covers every base a run of this machine could otherwise have used, and watching more than a run
needs costs observer calls that answer false, never correctness.

## 9. Acceptance

1. `cargo test --workspace` green, and TRX64's port audit (864 item 13) runs over our bridge host in our own
   tests: every verb in `help` answers, so drift from the daemon shows up here. **Done**, and it found the drift
   at once — 45 verbs `monitor_help_text()` lists that `verbs::try_exec` does not dispatch, because the library
   carries the daemon's help text while the extraction is under way. They are pinned in `STILL_THE_DAEMONS`, so
   the list failing in either direction is the signal: a verb that starts answering, or a new one that does not.
2. `monitor r`, `monitor m c000 c00f`, `monitor d e000`, `monitor device fw` + `monitor r` all answer on a booted
   firmware; `monitor d` on `fw` refuses by name.
3. A breakpoint on the C64 stops it while the firmware keeps running. **Done** over the VICE port, which is the
   only place a breakpoint can be set until TRX64 moves `g` and the stepping verbs: `scripts/vice-client.py` sets
   an exec checkpoint, resumes, and gets `CHECKPOINT_INFO` before `REGISTER_INFO` + `STOPPED`, with the C64
   stopped inside the checkpoint's range and the machine moved across the stop. A checkpoint that is hit again
   afterwards is not a fault: the firmware may clear `C64_STOP` itself, which is the last-writer-wins rule of §3
   doing what it says.
4. `config set` changes a setting in the running firmware (the menu shows the new value without a restart),
   `config write` survives a restart, `config flash` shows what is stored.
5. A smoke script (`scripts/smoke-monitor.ctl`) drives the above headless and is part of `smoke-all.sh`.
6. M4: `scripts/vice-client.py` does the VICE first contact, the discovery commands, the registers, memory
   through a named bank, a checkpoint set and listed, a step with its event order, and the two error cases; it is
   part of `smoke-all.sh` (§9.6). One real client (VS64 or IceBro Lite) attaches by hand once and the result is
   written into `docs/status/monitor.md` — not done yet.
7. No regression: `smoke-all.sh`, `smoke-c64-carts.ctl` and the `uci-targets` e2e stay green.

## 10. Open

- **What the device verbs still do not reach.** Two things live outside the machine or outside what the hardware
  keeps: the `--usb-dir` sync state and the network backend's forwards, which belong to the run loop and not to a
  device; and the audio mixer's gains, which are write-only registers the model does not store. (The filesystems
  were the third and are done: `dir [path]` reads them through the firmware's own DOS target.)
- ~~**A bound on a halt.**~~ Answered by §3: because the monitor's halt is the machine's own stop, the bridge's
  UCI event wait already gates on it, so the firmware does not wait forever for a C64 that stands. No release
  timer; if a firmware path is found that still hangs, it comes back as a real question.
- **`DISPLAY_GET`/`PALETTE_GET`.** Our canvas is the firmware's output composition (S07, after issue #3), not a
  bare VIC-II frame, so `DW/DH/XO/YO/IW/IH` would need a definition of their own. Deferred; not in the useful
  minimum.
- **An RV32 disassembler** would make `d` work on `fw`. Ours to write if we want it; TRX64 will not carry a second
  instruction set.
- **Pin and release.** We repin `c64-bridge` to TRX64's tag when it lands, and the monitor goes out with 0.4.0
  together with the overlay fix that is already on main.
