# Architecture — UE2-C64U-Emulator

## Goal

Run the **unmodified** U64-II / C64 Ultimate firmware (`ultimate.elf`, upstream GideonZ/1541ultimate,
target `u64ii/riscv/ultimate`) on macOS, so the menu UI, file browser, config, and network services can be
developed and tested without flashing. The C64 core inside the FPGA is replaced by TRX64 (`trx64-core`, S14) behind
the same registers, with SID, cartridges and drive A; `--c64 none` keeps the T0 stub that only satisfies the handshakes.

## Ground truth

| Source | Role |
|---|---|
| `docs/hw/00-memory-map.md` | Consolidated address map, boot hazards in execution order, IRQ model, T0/T1 tiers. **Authoritative for register behaviour.** |
| `docs/hw/01..12-*.md` | Per-block detail with firmware `file:line` citations. |
| `firmware/1541ultimate` | Upstream clone (untracked), built by `scripts/build-firmware.sh`. Never patched. |
| `docs/specs/S*.md` | Implementation step specs (scope, owned files, acceptance). |

If a doc turns out wrong, fix the doc (with a citation) in the same change as the code.

## Hardware facts that shape the design

- **CPU:** rvlite RV32IM (the firmware uses no DIV/REM), M-mode only, direct-mode traps. One external
  interrupt line from the ITU (`mcause 0x8000000B`). No CLINT/MTIME. The idle task spins without WFI, so
  **emulated time advances per executed instruction**.
- **Memory:** 64 MB DDR at 0 (bit 28 = 0, mirrored modulo 64 MB), except `0x8000xxxx`, which is the rvlite boot
  BRAM (bus_converter.vhd:96,118). Instruction fetches ignore bit 28 (rvlite_wrapper.vhd:101-106). 8-bit IO bus `0x10000000-0x10FFFFFF`.
  16/32-bit IO accesses are little-endian byte sequences, each byte with its own side effects.
  No bus faults. Unmapped reads return 0, unmapped writes are ignored.
- **Tick:** FreeRTOS 200 Hz tick = ITU timer edge IRQ bit 0 every 499 968 clocks (100 MHz clock).
- **UI:** overlay chargen — 4 K screen RAM `0x10141000`, 4 K colour RAM `0x10142000`, palette `0x10145000`.
  Shown only when HPD = 1 and `CFG_USERIF_ITYPE = 1`.
- **Input:** 8×8 keyboard matrix (COL `0x1010040A` write, ROW `0x1010040B` read). Menu button = ITU
  `0x1000000A` bit 6.
- **Capabilities** (T0): `0x34000222`. `--net` adds CAPAB_ETH_RMII (bit 24), a USB device CAPAB_USB_HOST2 (bit 23),
  the TRX64 C64 CAPAB_EEPROM (bit 22, GMOD2 carts), so the default boot banner reads `34400222`.

## Workspace

| Crate | Content |
|---|---|
| `crates/rv32` | CPU interpreter, `Bus` trait. No dependencies. Verified with the official riscv-tests. |
| `crates/ue2-core` | `SystemBus` (RAM + IO decode), `IoMap`/`IoDevice`, `IrqState` (ITU interrupt core), loader (ELF, `.app`, `.ue2`, updater records), symbolizer, `Machine` run loop, device models (`devices/*`, USB in `devices/usb/`), the C64 backend trait (`c64host`), overlay and C64 renderer (`render`), host types (`host`). No emulator dependency. |
| `crates/ue2-net` | Host network backends behind `host::NetBackend`: libslirp user-mode networking (hand-written FFI, links the system libslirp: `SLIRP_LIB_DIR`, else /opt/homebrew/lib when it exists, else the linker's default paths; build.rs), vmnet.framework bridged mode (`vmnet`, block2 FFI), a client of lima's socket_vmnet daemon (`socket_vmnet`). |
| `crates/ue2-vfat` | `--usb-dir`: FAT32 volume built from a host directory (fatfs crate), snapshot parser with a structure check, guest-to-host sync with its safety rules, host watcher (notify/FSEvents), worker thread (`docs/status/usb-dir.md`). No emulator dependency beyond `usb::block::BlockBackend`. |
| `crates/c64-bridge` | `Trx64Backend`: TRX64 (`trx64-core`, git dependency on https://github.com/Jondalar/TRX64 at rev `f370a56`, v0.7.0) as a `c64host::C64Backend`: `sid` (SID decode, ARMSID identity, reSID sample stream), `cart` and `cart_eeprom` (all_carts_v5.vhd, freezer.vhd and the GMOD2 EEPROM on guest DDR), `slot` (a cartridge in the physical expansion port: TRX64's mappers, flash boards on TRX64's flash and EEPROM chips, or `cart::CartLogic` fed from the CRT; bus sharing, bridge and CART_DETECT), `drive` (drive A on TRX64's drive 8 as a `c64host::C64Drive`), `keys`, `video`, `clock`. The rev is pinned in `crates/c64-bridge/Cargo.toml`; re-run the tests and the C64 smokes before moving it. |
| `crates/ue2emu` | Binary, `ue2emu run` and `ue2emu install`: CLI, emulation thread + pacing (`runner`), window (`window`, `keymap`), scripted/TCP control (`control`), network wiring (`net`), USB options (`usb`), `--usb-dir` controller (`usbdir`), SID audio out (`audio`: cpal and WAV), updater install (`install`), C64 ROMs into the flash image (`c64roms`, `--c64-roms`), physical cartridge write-back and `cart-info`/`cart-save` (`cartslot`, `--cart-slot`), GDB stub (`gdb`). Cargo feature `trx64` (default) links c64-bridge. |
| `crates/ue2-mcp` | Binary `ue2-mcp`: stdio MCP server that starts `ue2emu` instances and drives them through the TCP control protocol (`docs/status/mcp.md`). |

## Execution model

```
 main thread (winit window or headless)            emulation thread
 ┌──────────────────────────────┐   Command   ┌──────────────────────────────────┐
 │ window: render DisplaySnapshot│ ─────────► │ Machine::run(slice)              │
 │ keymap: host key → matrix     │            │  cpu.step ─► SystemBus ─► IoMap  │
 │ control: script / TCP         │ ◄───────── │  devices tick at next_event      │
 └──────────────────────────────┘  snapshot  │  pacing vs wall clock            │
                                   console   └──────────────────────────────────┘
```

**Machine loop:**
1. `bus.now += clocks_per_insn` for each instruction (default 4 clocks, i.e. 25 MIPS emulated; at least 1, checked by
   clap and `Machine::new`).
2. Before each step:
   - If `now >= next_deadline`, tick every device whose `next_event() <= now`.
   - Set `cpu.meip = bus.irq.line()`.
3. After any IO access (`bus.io_touched`), recompute `next_deadline` as the minimum `next_event` over all
   devices.
4. Breakpoints and fault hooks are checked only when armed. The four fault hooks are PC equal to
   `vAssertCalled`, `C_exception_handler`, `__crt0_dummy_trap_handler`, or the `j .` of the `get_mem` PANIC loop
   (found in the loaded code of `_Z7get_memj`; the halt names the caller from the saved `ra`). An absent hook is
   `NO_HOOK` (`u32::MAX`, never a PC), so the per-step check is four inlined compares (`docs/status/fixes.md`).

**Pacing:**
- `realtime` (default): every ~100 k instructions, compare emulated ms with wall-clock ms and sleep when
  ahead. Timers, UI key repeat and network timeouts then behave like hardware.
- `max`: run as fast as possible. Tests use emulated-time waits.

## Bus and devices

- **`SystemBus`** implements `rv32::Bus`:
  - bit 28 = 0 → `ram[addr & RAM_MASK]`, with a fast path for 32-bit words. Exception: the boot BRAM page
    `0x8000xxxx` reads 0 and ignores writes, because the ELF is loaded directly (00 §1 bus rules).
  - Instruction fetches ignore bit 28: every page except `0x8000xxxx` fetches from DDR.
  - `0x10000000-0x10FFFFFF` → `IoMap`.
  - Anything else → read 0 / write ignored, reported to the unmapped log.
  - 16/32-bit IO accesses are split into LE bytes.
- **`IoMap`:** 256-byte decode grain. `add(base, size, Box<dyn IoDevice>)` maps a device's primary window;
  `map(base, size, idx)` adds alias windows. A device sees offsets relative to the window it was accessed
  through. `map_origin(base, size, idx, origin)` maps a window whose offsets count from `origin`, so one device
  can tell several windows apart. `get_mut::<T>()` downcasts, so the machine can deliver host input.
- **`IoDevice`:** `read8`, `write8`, `peek8`, `next_event`, `tick`, `reset`. Each call gets an `IoCtx`:
  `now`, `pc`, `ram` (for DMA masters), `irq`, and `console`.
- **`IrqState`:** the ITU interrupt core (global enable, mask, edge mask `0x85`, latched flags, level
  sources, high enable/sources). Devices only drive sources: `pulse(bit)`, `set_level(bit)`,
  `set_high(bit)`. The ITU device owns the register view. `line()` is a few bit ops, so it is evaluated
  on every instruction.
- **`devices::install_all`** calls one `install(map, cfg)` per device module. Each module owns its windows.
- **C64 (`devices::c64::C64Port`):** cart/machine registers, DMA window, MATRIX_KEYB, core config, palette and ROM
  windows, one device through `map_origin`.
  - Without a backend it is the T0 stub of doc 10 (its CIA1 port B scans the host keys, for firmware UIs on the C64
    screen such as the updater's).
  - `Machine::attach_c64(Box<dyn c64host::C64Backend>)` plugs in a C64. Every access to the cart registers, the DMA
    window or MATRIX_KEYB first advances the backend to the accessing instruction's clock; `tick` advances it every
    1 ms emulated (S14 §4).
  - `ue2emu --c64 trx64` (default with the `trx64` feature) attaches `c64_bridge::Trx64Backend`, with ROMs seeded
    from `--roms`; `--c64 none` attaches nothing (`docs/specs/S14-c64-trx64.md`, `docs/status/c64.md`).
  - SID: every core config write also goes to `C64Backend::core_config_write`. The bridge decodes SID socket 1 (an
    ARMSID with `--sid-socket1 armsid`) and UltiSID 1 onto one reSID. CPU writes reach it at their cycle through a
    TRX64 observer, DMA reads of the SID range answer from it, and its samples go to `ue2emu::audio`
    (`docs/status/sid-audio.md`).
  - Cartridges: every access that can run the C64 lends `IoCtx::ram` to the backend (`C64Backend::lend_ddr`), so the
    cartridge logic serves the firmware's CRT banks, cart RAM and GeoRAM from guest DDR live. The EEPROM window
    `0x1004C000` and MATRIX_KEYB[10] (the freeze button) go to the backend. The bridge ends TRX64 runs where EXROM/GAME
    can change behind its PLA (`docs/status/carts.md`).
  - Expansion port: `--cart-slot` puts a second, physical cartridge on TRX64's bus (`c64_bridge::slot`). The firmware's
    C64_BUS_INTERNAL/EXTERNAL/BRIDGE writes decide which side serves IO1, IO2, the ROM windows and the interrupt lines;
    U64_CART_DETECT reads its lines; DMA reads and writes reach it like CPU accesses. Its flash and EEPROM changes can
    go back into the CRT (`ue2emu::cartslot`, `docs/status/cart-slot.md`).
  - Drive A (`0x10020000`) is a `devices::drives::DriveRegs` inside `C64Port`, so each access syncs the C64 first. It
    drives `C64Backend::drive(0)` (a `c64host::C64Drive`) with the register lines and the GCR half-tracks the firmware
    keeps in DDR, and copies written tracks back into DDR with DIRTY set; the firmware writes them into the image.
    Drive B has registers only (`docs/status/drive.md`).
- **USB (`devices::usb`):** HLE of the nano USB CPU protocol with a USB2513 hub as root (3 ports), mass storage
  (Bulk-Only Transport, SCSI) on a `block::BlockBackend` and a HID boot keyboard, configured by `MachineConfig::usb`
  (`--usb IMAGE` repeatable, `--usb-keyboard`). CAPAB_USB_HOST2 is set only with a device (`docs/status/usb.md`).
  - Hub ports support hot-plug (`HostInput::UsbPlug`). `Machine::usb_attach_storage` and `usb_replace_backend` let
    the frontend put media on ports that `UsbConfig::storage_slots` left free.
  - `ue2emu --usb-dir` uses that for host directories (`crates/ue2emu/src/usbdir.rs`, `crates/ue2-vfat`,
    `docs/status/usb-dir.md`).
- **Ethernet:** the RMII MAC and MDIO PHY exchange frames with a `host::NetBackend`, pumped by the emulation thread
  after each slice: `--net user` (libslirp, `--hostfwd`, and the host-side web UI proxy `--web-port` on its own
  threads), `--net vmnet-bridged[:IFACE]` (root or the
  vm.networking entitlement), `--net socket-vmnet[:PATH]`. The bridged modes give each flash image its own MAC
  through the flash unique ID (`docs/status/network.md`).

## Host side

- **`HostInput`** (`Machine::input`):
  - `Key { row, col, down }` → `U64Io::set_key` and `C64Port::set_key` (TRX64's keyboard or the stub's CIA1).
    While the overlay owns the keyboard (TRANSPARENCY bit 6) key-downs do not reach the C64; releases always do.
  - `Joystick(bits)` → `U64Io::set_joystick` and C64 port 2
  - `MenuButton(pressed)` → `Itu::set_menu_button`
  - `UsbKey { usage, down }` → the USB HID keyboard (dropped without `--usb-keyboard`)
  - `UsbPlug { port, connected }` → unplug or plug in the device on a hub port (`usb-replug`, `--usb-dir` replugs)
  - `Restore(held)` → the C64 NMI line
- **Audio** (`ue2emu::audio`, `--audio on|off`, `--audio-wav`): the emulation thread pushes the SID's samples into a
  150 ms ring that drops the oldest samples when full and plays silence on underrun, so `--speed max` never waits;
  the cpal stream lives in `EmuHandle` on the thread that spawned the emulation. The WAV gets every sample.
- **`DisplaySnapshot`:** overlay registers, screen RAM, colour RAM and palette, plus `c64: Option<C64Frame>` (frame,
  palette, text screen and character set of an attached C64). Published by the emulation thread about every 20 ms
  emulated. `render::Renderer` composites the overlay over the 384×272 C64 frame (overlay only without a C64);
  `render::text_dump` and `render::c64_text_dump` turn the overlay and the C64 screen into text.
- **Window:** realtime, 4:3 letterboxed, opens at 768×576. F12 = menu button, Page Up = RESTORE; other keys go to
  the matrix, or with `--usb-keyboard` to the USB keyboard.
- **Control:** `--script file` or `--control 127.0.0.1:PORT`, one command per line (S08):
  `wait <ms>`, `button [ms]`, `key <name> [ms]`, `type <text>`, `usbkey <name> [ms]`, `screen`, `c64screen`,
  `png <path>`, `expect <text> [ms]`, `expect-not <text> [ms]`, `expect-console <text> [ms]`,
  `usb-sync [--force] [port]`, `usb-replug [--discard] [port]`, `quit`.
  - `button`, `key`, `type` and `usbkey` reach the emulation thread as one timed sequence (`Command::Inputs`,
    `control::InputTimeline`), so holds keep their emulated length at any host speed.
  - `expect*` poll the screen text or the console; a timeout prints the screen and fails with the line number, and
    a headless script exits non-zero. `scripts/smoke-all.sh` runs the self-checking smoke scripts
    (`docs/status/tooling.md`).
  - `png` reads its font (`chars.bin`) from `--roms` (`ControlHandle::rom_dir`).

  This is how agents, CI and `ue2-mcp` verify the UI without a person.

## Install

`ue2emu install --update X.ue2 --flash F [--yes] [--timeout S] [--c64 trx64|none] [--roms DIR]` populates a flash
image the way hardware gets its contents (`docs/status/install.md`):
- `loader::load_updater` loads the updater record of the `.ue2` with the normal device set; no overlay-UI seed.
- The updater's UI is the C64 text screen. `install.rs` reads its popups through `C64Port::dma_peek` and answers
  them with matrix keys through `Machine::input`, with TRX64 or the T0 stub.
- The run ends at the updater's power-off request to the ESP32 stub (`U64Ctrl::power_event`). The flash image is
  written, and the application slot is compared with the update file.

## Debugging

- Symbolized logs (ELF symbols, C++ demangled).
- `--log unmapped,io,irq`: first hits per unmapped address with PC and symbol, a full IO trace, IRQ edges.
- Fault hooks halt with a symbolized backtrace hint (ra); the `get_mem` hook names the caller whose allocation failed.
- CPU trace ring (`--trace`, implied by `--gdb`): the PCs of the last 256 steps, printed with symbols when a
  fault hook halts the machine (`monitor trace` in GDB). It costs about 3 % of host MIPS when on and nothing
  measurable when off.
- GDB remote stub (`--gdb 127.0.0.1:1234`, `riscv32-unknown-elf-gdb`, S11, `ue2emu/src/gdb.rs`):
  - The machine waits at reset until the debugger continues; a debugger attaching later stops it where it is.
  - x0-x31 and pc; memory reads without side effects (IO through `peek8`, ITU registers included), memory writes to
    DDR only.
  - Software breakpoints (`Machine::breakpoints`), step, continue, Ctrl-C. A fault hook stops GDB with SIGABRT.
  - Detach lets the machine run on; kill ends the emulator.
  - `monitor tasks` lists the FreeRTOS tasks from `pxCurrentTCB` and the kernel lists.

## Milestones

| M | Name | Acceptance |
|---|---|---|
| M1 | Console | Headless run prints the firmware boot log (capabilities line) with no fault hook. |
| M2 | Main loop | All InitFunctions run, the scheduler idles, no hang or assert for 60 s emulated. Every boot-relevant unmapped access is modelled or documented. |
| M3 | Menu | A script presses the menu button; `screen` shows the overlay menu; cursor keys change the selection. The window shows the same. |
| M4 | Storage | Config persists across runs (flash image). An SD image shows up in the file browser. |
| M5 | Network | Web UI, REST, FTP and Telnet reachable from the Mac through a libslirp port-forward. The upstream E2E smoke profile runs against the emulator. |
| M6 | USB | USB mass storage (image) and HID keyboard. |
| M7 | C64 | TRX64 attached: DMA, reset/stop, cart, drive A, video behind the overlay. |

Status:
- M1-M3 reached (`docs/status/boot.md`), M4 reached (`docs/status/storage.md`).
- M5: REST, Telnet and FTP through libslirp work, and the upstream E2E smoke profile passes 12 of 12 with the REST
  shim (`docs/status/network.md`, `docs/status/e2e.md`).
- M6 reached (`docs/status/usb.md`).
- M7 reached: DMA, reset/stop and video behind the overlay (phase A); SID with audio, the FPGA cartridge types and
  drive A as a 1541 (wave 4); `--c64-roms`; a cartridge in the physical expansion port (`--cart-slot`,
  `docs/status/cart-slot.md`). Open: UCI, REU, the IEC processor, drive B, NTSC (`docs/status/c64.md`).

## Specs

| Spec | Scope | Wave |
|---|---|---|
| S01 | RV32 CPU (rvlite semantics) + riscv-tests | 1 |
| S02 | Core: SystemBus, loader, symbols, Machine loop, runner/pacing | 1 |
| S03 | ITU + UART + capabilities (+ IrqState semantics) | 1 |
| S04 | Board T0 models (I2C, U2PIO, C64 cart/DMA/core, USB nano, drives, IEC/UCI/ACIA/tape, misc) | 1 |
| S05 | WiFi DMA UART + stub u64ctrl | 1 |
| S06 | SPI flash NOR (persistent) + overlay-UI config seeding | 1 |
| S07 | Overlay device, U64 IO page (keyboard matrix), renderer | 1 |
| S08 | Frontend: window, keymap, control/script | 1 |
| S09 | SD card over SPI (image-backed) | 1 |
| S10 | Integration: boot to M1–M3 | 2 |
| S11 | Debug: GDB stub, trace ring | 2 |
| S12 | Network: RMII MAC, MDIO PHY, libslirp bridge | 3 |
| S13 | USB HLE (mass storage, HID) | 3 |
| S14 | C64 via TRX64 (`S14-c64-trx64.md`) | 4 |

## Rules

- The firmware is never patched. The emulator adapts to the firmware, including its documented defects
  (00-memory-map §"Firmware defects").
- A device that is not modelled reads 0 and ignores writes; boot hazards come first.
- Every device model has unit tests against its `docs/hw` behaviour. Tests that need the real ELF skip
  when `firmware/` is absent.
- Wave agents write only the files their spec owns. Needed interface changes are reported, not made.
