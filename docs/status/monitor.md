# The monitor (S23)

TRX64's monitor runs against UE2's C64. The verbs are TRX64's own, from `trx64-monitor` (their Spec 864); UE2 is
its second host. Design and the division of labour: `docs/specs/S23-monitor.md`.

**M1 works.** `crates/ue2emu/src/monitor/mod.rs` implements `MonitorHost`:

- `machine()` hands over the TRX64 machine. `ue2-core` still knows nothing about TRX64: `C64Backend` grew a
  `as_any_mut` hook, `C64Port` a `backend_mut`, and the bridge a `trx64()` accessor, so the frontend downcasts and
  the core stays ignorant (that is what keeps `--c64 none` honest).
- `cpu(Device::Host("fw"))` is the firmware's RISC-V: 32-bit addresses, `pc` and the 32 ABI-named registers,
  memory through the same `peek8` the GDB stub reads with, DDR writable and its IO not. No flag register, no
  disassembler, no debug gates — the watch tables are a 6502 shape.
- The marked address spans (TRX64 Spec 804) are stripped on the way out, so the control protocol carries what a
  reader wants and the library keeps the marked form.

**Surfaces.** `monitor <cmd>` on the control port (S08), and `emu_monitor` over MCP on top of it.

**M2 is done.** The library sees every line first and returns `None` for what it does not own; our dispatch takes
it from there and appends its own section to `help`. TRX64 gains no notion of this emulator — the dependency stays
one-way.

| Verb | What it answers |
|---|---|
| `fw [tasks]` | The firmware's RISC-V: `pc` with its symbol and the 32 ABI registers, or the FreeRTOS task list. |
| `clock` | The emulator's clock, the firmware's instructions and idle skips, the C64's cycle. |
| `config …` | The settings, in all the ways the menu has them (below). |
| `itu` | The capability word, the FPGA version, the ms/us timers, and the interrupt controller by source name. |
| `cart` | What the firmware programmed for the C64: mode, stop, cartridge type and variant, the cart ROM window, KERNAL, REU and its size, the sampler, serve-while-stopped, the clock-detect lines, and the command interface. |
| `flash` | The image behind the chip, sectors not written out yet, config pages in use. |
| `sd` | The card: sectors, size, write protect. |
| `usb` | The hub ports and what is on each. |
| `net` | The MAC the firmware programmed, the RX filter, the buffer queues, the TX register. |
| `audio` | Socket 1, the engines built with their model, and the address windows routed to each chip. |

The device verbs read what the firmware reads — through the side-effect-free `peek8` the GDB stub uses, or out of
the device's own state where the register is write-only — and decode it with the firmware's own names
(`itu.h`, `c64.h`). Nothing is interpreted beyond that: when a line looks wrong, the machine is wrong. A machine
without the device says which one is missing rather than inventing zeroes.

**`config` is the whole cycle** (`crates/ue2emu/src/monitor/config.rs`), and it works the way the menu works:

| Line | What happens |
|---|---|
| `config [cat [item]]` | The settings as the flash holds them, decoded through each item's own definition and marked `flash` or `default` (S21). |
| `config flash` | The raw page decode: which pages a store claimed, and how many records each holds. |
| `config set <cat> <item> <value>` | Checked against the item's definition here, then handed to the running firmware as a two-line `.cfg`, which applies and effectuates it. |
| `config write` | The items `set` changed in this session, into the config pages — where the next boot finds them. |
| `config write <path>` | Every item of every store as a `.cfg`, at a path inside the machine: `/temp`, `/flash`, `/Usb0`, the SD card. |
| `config read <path>` | That `.cfg` handed to the firmware, which applies and effectuates it. |

Reading is ours alone; changing is not. The firmware holds its own copy of every store and writes that copy back
over anything we put in a page, so `config set` comes before `config write` and `config write` says so when nothing
was set.

**Why the firmware writes the files.** `crates/ue2emu/src/monitor/uci.rs` pushes a command into the UCI block (S15)
as a host access — not a C64 bus cycle, so the unlock detector and the `$FF00` trigger stay untouched — runs the
firmware until the block has the reply, and reads the reply data and the status string back out. Every `.cfg` the
monitor puts inside the machine goes through the firmware's own DOS target on that transport
(`OPEN_FILE`/`WRITE_DATA`/`CLOSE_FILE`), because FatFs keeps one sector of each mounted volume in a window it will
not read again while it holds it: a directory entry we added behind its back can simply go unseen, and that is
exactly what happened when this was tried with our own FAT writer. Writing through the firmware also means every
medium works with no writer of ours per medium. One rule came out of it: a write command carries less than one
512-byte sector, because FatFs hands a full sector straight to the block device and the USB controller fetches
that data by physical address — which cannot reach the command interface's register RAM.

The block is refused while the C64 has a command in flight, a firmware that never enabled the command interface is
named together with the setting that turns it on, and the firmware's own status string is what the monitor prints,
so a build without `CTRL_CMD_LOAD_CONFIG` (it was added to the firmware fork in `Add UCI control command to load
config file`) says `UNKNOWN COMMAND` instead of pretending.

**Not yet:** run control — `g`, `step`, breakpoints (M3). Until then the library's own sentence answers: "run
control is not available in this host".

### Verified

| Check | Result |
|---|---|
| `cargo test --workspace` | green (10 in `ue2emu::monitor`, 87 in ue2emu) |
| `cargo clippy` on the changed crates | no new warnings |
| Booted 3.15, `monitor r` over the control port | the register panel with the flow line, `.;e5cd 00 00 0a f3 nv-bdiZc  MAIN`, the port and vector lines |
| `monitor m 0400 0407` | `>C:0400  20 20 …`, screen RAM |
| `monitor d e000 e001` | `$e000  85 56     STA $56` |
| `monitor wr` then `m` | the bytes read back |
| `monitor nonsense` | `unknown monitor command 'nonsense'` — our dispatch takes what the library declines |
| `monitor device` on a booted 3.15 | `device: c64   (c64 \| drive8 \| fw — anything but c64 is read-inspect r/m/d)`; `device fw` selects it |
| `monitor fw` | the 32 registers with `pc  00035da8  prvIdleTask+0x2c` |
| `monitor clock` | the emulator's ms and clocks, the firmware's instructions and idle skips, the C64's cycle |
| `monitor config flash` on a `--settings` flash | 11 pages with their names (`GEN.`, `C64.`, `U64C`, …) and record counts |
| `monitor config "C64 and Cartridge Settings" "REU Size"` | `REU Size=16 MB   (flash)`, the value `--settings` wrote; an item nobody stored reads its firmware default |
| `monitor config read /Usb0/change.cfg` on a booted fork build | `  /Usb0/change.cfg  00,OK`, and the firmware console shows `Effectuating settings of store 'C64 and Cartridge Settings' after loading.` |
| `monitor config read` on a file that is not there | the line fails with the firmware's own `88,CANNOT OPEN CONFIG FILE` |
| `monitor config set "C64 and Cartridge Settings" "REU Size" "2 MB"` | `[C64 and Cartridge Settings] REU Size=2 MB`, the firmware effectuates the store, and a bad value is refused here with the item's own choices |
| `monitor config write` after it | the item in the config pages; `monitor config … "REU Size"` then reads `2 MB   (flash)` |
| `monitor config write /temp/all.cfg` and `/Usb0/settings.cfg` | 179 items in ~4.9 KB, written by the firmware; the stick's copy reaches the host directory through the `--usb-dir` sync |
| `monitor config read` on that dump | `00,OK` with an empty parse log: the firmware accepts its own spelling back |
| `scripts/smoke-monitor.ctl` in `smoke-all.sh` | 8/8 (S23 §9.5); the script also greps the log for `REU Size=2 MB   (flash)` |
| `monitor itu` on a booted 3.15 | `capabilities 0x34800222` (with `--usb-dir`, so `USB_HOST2` is in it), `low enabled 0x95 timer usb cmdif reset`, `high enabled 0x6a 1581 wifi hdmi unlock` |
| `monitor cart` | `cartridge type 0x00 variant 0 none, not active`, `cart rom 0x03c00000, 4 MiB`, `reu on, 16 MB` from the `--settings` flash, `command intf on at 0xdf18, bus id 11` |
| `monitor flash` / `sd` / `usb` | the image path and 11 config pages; a 64 MiB card; `port 1 storage in use` for the `--usb-dir` stick, the other ports empty |
| `monitor net` / `audio` | no MAC programmed and RX off on a machine without `--net`; socket 1 empty, engines 1 and 5 as 6581, one window line `$d400-$d7ff -> chip 0` (48 mirrors collapsed) |
| TRX64 repin 0.7.3 → 0.8.2 | no code change needed; `smoke-all.sh` 7/7, `smoke-c64-carts.ctl` 27/27 with the freeze and both SID loads, UltimateDemo2026 detection all `[ OK ]` |

### Open

- `r`/`m`/`d` still read the C64 after `device fw`: the library selects the device but does not route those verbs
  through `CpuView` yet (TRX64 is wiring that next; the `device` validation itself was fixed in v0.8.2 after we
  reported it).
- The verbs of the second half of the extraction (`g`, `until`, `step`, `bk` hits) answer through our fall-through
  until they land upstream.
