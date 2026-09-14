# UE2-C64U-Emulator

Mac-side emulator for the **Ultimate 64 Elite II / Commodore 64 Ultimate** (U64-II, `update.ue2`)
firmware from [1541ultimate](https://github.com/GideonZ/1541ultimate). Goal: develop and test
the firmware — menu UI, file browser, config, REST/web/FTP/telnet — without flashing hardware.

- Runs the **unmodified** `ultimate.elf` (or `ultimate.app` / `.ue2`) on a CPU interpreter. The CPU is Gideon's
  **rvlite** (RV32I + Zicsr + the multiply half of M, M-mode only), not neorv32 (`docs/hw/01-cpu-boot-memory.md`).
- FPGA peripherals are modelled at register level (ITU, UART, overlay chargen, SPI flash, SD, WiFi UART, RMII,
  USB, …).
- The C64 core inside the FPGA is replaced by [TRX64](https://github.com/Jondalar/TRX64) (`trx64-core`) behind the
  same registers (`--c64 trx64`, the default), with SID audio, cartridges and a 1541 as drive A. `--c64 none` keeps
  the register stub.

Usage guide — build prerequisites, every `run` option, `--config` files, `install`, the MCP server:
`docs/status/install.md`.

## 0.1.0

### Platforms

- **macOS:** supported; developed and tested on Apple silicon.
- **Linux:** probably works, untested. Install the distribution's libslirp development package, version 4.8 or newer
  (`slirp_pollfds_fill_socket`; Debian 13 has 4.8.0, Ubuntu 24.04 only 4.7.0);
  `crates/ue2-net/build.rs` links `slirp` from the linker's default paths, or from `SLIRP_LIB_DIR=DIR` when that is
  set. Known gap: `--net vmnet-bridged` needs macOS. `ue2-mcp` stops an emulator through `nc`; without it the
  stop falls back to SIGTERM after about 3 seconds.
- **Windows:** not supported, and not only because of libslirp: the code uses Unix APIs in `ue2-vfat` (`--usb-dir`),
  `ue2-mcp` (instance control), `ue2-net` and `crates/ue2emu/src/c64roms.rs`.

### Not included: firmware and ROMs

The repository ships no firmware and no ROMs; `.gitignore` keeps firmware, ROM and media images out of it. Bring your
own:

- **Firmware:** an `ultimate.elf` (your build, or `scripts/build-firmware.sh`), an `ultimate.app`, or a `.ue2` update
  file (the upstream `update.ue2`, the Commodore `c64u_v1.1.0.ue2`), passed with `--firmware`.
- **Roms directory:** `roms/` of a GideonZ/1541ultimate clone, passed with `--roms` (default
  `firmware/1541ultimate/roms`). The window and the `png` control command read the overlay font `chars.bin` there
  (`crates/ue2emu/src/window.rs:46`), also when the firmware is a `.ue2`.

### ROMs

- **C64 KERNAL, BASIC, CHAR.** No updater writes them, so on a blank flash the C64 shows the firmware's "shipped
  without System ROMs" screen. `--c64-roms [DIR]` (on `run` and `install`, needs `--flash`) puts them into
  `/flash/roms` of the flash image before the firmware runs, as the menu's "Set as … ROM" would. DIR defaults to the
  `--roms` directory, `firmware/1541ultimate/roms`, where GideonZ/1541ultimate ships them. Per ROM the first file with
  the right size is taken (`crates/ue2emu/src/c64roms.rs`):

  | ROM | File names, in order | Size |
  |---|---|---|
  | KERNAL | `kernal.901227-03.bin`, `kernal.bin` | 8192 |
  | BASIC | `basic.901226-01.bin`, `basic.bin` | 8192 |
  | CHAR | `characters.901225-01.bin`, `chars.bin` | 4096 |

  - The 2048-byte `chars.bin` in the firmware tree is the overlay font; its size rules it out.
  - An erased `/flash` is formatted first, exactly as the firmware would format it.
  - A file already there with other content is kept; `--c64-roms-force` replaces it.
  - When the ROMs are already there, nothing is written, so the flag can stay on every run.

  The first boot reaches BASIC `READY.`; the control command `c64screen` prints the C64 text screen
  (`docs/status/c64.md`).
- **1541 drive ROM.** It comes from the `.ue2`: `ue2emu install` runs its updater, which writes `1541.rom`,
  `1571.rom` and `1581.rom` into `/flash/roms` (`docs/status/install.md`). A blank flash has no drive ROM and drive A
  stays off. "Set as 1541 ROM" on a `1541.bin` in the file browser also works (`docs/status/drive.md`).

### TRX64 dependency

`crates/c64-bridge` takes `trx64-core` from GitHub, pinned in its `Cargo.toml` to rev
`69c9b30add4ea3ae7a46a53e81c56887585fe001` (head of TRX64 `main`, `trx64-core` 0.5.0); cargo fetches it on the first
build. Its build.rs compiles the vendored reSID C++, so a C++ compiler is needed.
`cargo build --release -p ue2emu --no-default-features` builds without TRX64 (`--c64 none` only). The bridge drives
TRX64 internals, so run the tests and the C64 smokes (`docs/status/c64.md`) before moving `rev`.

To build against a local TRX64 checkout, create an untracked `.cargo/config.toml` in the repo root:

```toml
[patch."https://github.com/Jondalar/TRX64"]
trx64-core = { path = "<TRX64 checkout>/crates/trx64-core" }
```

- The patch key must be exactly `https://github.com/Jondalar/TRX64` (no trailing slash, no `.git`), the URL in
  `crates/c64-bridge/Cargo.toml`. The local crate's version must still be 0.5.0.
- `cargo tree -p c64-bridge -i trx64-core` shows the path source while the patch is active.
- Cargo rewrites `Cargo.lock` while the patch is active (the `source = "git+…"` line of `trx64-core` goes). Do not
  commit that `Cargo.lock`, nor `.cargo/`, which `.gitignore` does not cover; after removing the config,
  `git checkout Cargo.lock`.

### License

GPL-3.0-or-later (`LICENSE`).

- `crates/c64-bridge/src/cart.rs` ports `all_carts_v5.vhd` (with `freezer.vhd`) from GideonZ/1541ultimate (GPL v3),
  `crates/c64-bridge/src/cart_eeprom.rs` its `microwire_eeprom.vhd`.
- SID audio is reSID, vendored and compiled by TRX64's `trx64-core` (GPL).

## Quick start

Prerequisites (`docs/status/install.md`, "Build"): Rust, a C++ compiler, libslirp (`brew install libslirp`), and the
untracked `firmware/1541ultimate` (a clone of GideonZ/1541ultimate with submodules). `scripts/build-firmware.sh`
builds its `ultimate.elf` with a `riscv32-unknown-elf-*` toolchain in the untracked `tools/bin`.

```sh
scripts/build-firmware.sh     # builds firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf
cargo build --release
mkdir -p run

# Window, realtime. F12 = menu button, cursor keys navigate, Page Up = C64 RESTORE.
# --c64-roms puts the C64 KERNAL/BASIC/CHAR from the roms directory into the flash first: the C64 boots to READY.
target/release/ue2emu run --flash run/flash.bin --c64-roms

# With the network on: open http://127.0.0.1:8080 for the firmware web UI (REST API on the same port).
target/release/ue2emu run --flash run/flash.bin --c64-roms --net user

# A cartridge in the physical expansion port, read-only by default; dump or flash it over REST (docs/status/cart-slot.md).
target/release/ue2emu run --flash run/flash.bin --c64-roms --net user --cart-slot run/game.crt

# The same flags from a TOML file (an edited copy of docs/examples/ue2emu.example.toml); command-line flags win.
target/release/ue2emu run --config ue2emu.toml

# Headless: open the menu, check the screen, move the cursor, write run/menu1.png and run/menu2.png.
target/release/ue2emu run --headless --speed max --flash run/flash.bin --script scripts/smoke-menu.ctl

# Every self-checking smoke script (menu, SD, flash persistence), in a temporary directory.
scripts/smoke-all.sh
```

`--flash` keeps config across runs (created erased if missing). Firmware paths default to `firmware/1541ultimate`
under the repo root; elsewhere pass `--firmware` and `--roms` (`png` reads its font from `--roms`), and set
`UE2_FIRMWARE` for the tests. `target/release/ue2emu run --help` lists all options; `docs/status/install.md` groups
them.

### Flash contents from an updater

A blank flash has no drive ROMs and no `/flash` files. `ue2emu install` runs the updater inside a `.ue2`, the way
hardware gets them (`docs/status/install.md`):

```sh
target/release/ue2emu install --update <path>/update.ue2 --flash run/flash.bin --yes --c64-roms
```

A `.ue2` also boots directly: `run --firmware <path>/c64u_v1.1.0.ue2 --flash <installed flash>` runs the
application inside it. Without `--yes` each question is asked on the terminal. `--yes` answers Yes, except
"Reformat Flash Disk?" and "Reset Configuration?", which get No; `--reformat-flash` and `--reset-config` answer
those with Yes.

### SID audio, cartridges, disks

- **Audio:** `--audio on|off` plays the SID on the default output device (on with a window, off with `--headless`).
  `--audio-wav PATH` writes the mono sample stream, also headless. `--sid-socket1 armsid` fits an ARMSID in socket 1;
  on a flash that has not saved it, the menu asks once to review the SID settings (OK, then save). Without it
  UltiSID 1 at `$D400` plays (`docs/status/sid-audio.md`).
- **Cartridges:** RETURN on a `.crt` in the file browser, then "Run Cart". Freezer carts freeze with F11 on the USB
  keyboard (`--usb-keyboard`, menu closed); `.sid` and `.mus` files offer "Play Main Tune".
  `scripts/make-test-crts.py DIR` writes a test CRT for each supported type (`docs/status/carts.md`).
  `--cart-slot FILE.crt[,rw|,save=OUT.crt][,flash-decode=11|15|both]` puts a cartridge in the physical expansion port,
  with its own flash and EEPROM, for dumpers and flash writers over DMA and REST; `,rw` writes flash changes back into
  the CRT (`docs/status/cart-slot.md`).
- **Disks:** put `1541.bin` and a `.d64` on the SD image. Choose "Set as 1541 ROM" on `1541.bin` once per flash,
  then "Mount Disk" on the D64, and `load"$",8` on the C64. The firmware writes changed tracks back into the D64 file.
  `scripts/d64tool.py` builds, lists and extracts D64s, also straight out of an SD image (`docs/status/drive.md`).

```sh
# A 1000 Hz BASIC tone into a WAV, checked on the host.
target/release/ue2emu run --headless --speed max --flash run/flash.bin --sid-socket1 armsid \
    --audio-wav run/sid-tone.wav --script scripts/smoke-sid-tone.ctl
scripts/wav-tone.py run/sid-tone.wav --expect 1000
```

Self-checking C64 scripts, with their setup in the header: `smoke-sid-tone.ctl`, `smoke-c64-carts.ctl` (27 carts,
the freezer, the SID and MUS players), `smoke-c64-drive.ctl` (directory, LOAD, SAVE and write-back).

### USB, network, E2E, MCP

- `--usb IMAGE` (repeatable) attaches a USB stick, `--usb-keyboard` a HID keyboard that takes the window's keys.
  `scripts/make-sd-image.sh` makes stick images too (`docs/status/usb.md`).
- `--usb-dir DIR[,size=SIZE][,ro]` (repeatable) shares a host directory as a FAT32 stick. The firmware may write to
  it; changes are synced back safely (deletions go to `DIR/.ue2-trash`, conflicts become copies, a mass-deletion guard
  asks for `usb-sync --force`), and host changes reach the guest by an automatic replug. `scripts/smoke-usb-dir.sh`
  tests it (`docs/status/usb-dir.md`).
- `--net user` is libslirp NAT with `--hostfwd` (default `tcp:2323:23,tcp:2121:21,tcp:6464:64`) and a web UI proxy
  on `--web-port` (default 8080) to guest port 80, e.g. `curl http://127.0.0.1:8080/v1/info`. The proxy passes HTTP
  through and makes the web UI's API URLs carry the port. `--net vmnet-bridged[:IFACE]` (needs sudo) and
  `--net socket-vmnet[:PATH]` (lima's socket_vmnet daemon) put the device on the LAN (`docs/status/network.md`).
- `scripts/run-e2e.sh smoke` runs the upstream E2E suite against the emulator (`docs/status/e2e.md`).
- `target/release/ue2-mcp` is a stdio MCP server that lets another Claude Code session boot its own firmware build
  in emulator instances and drive them, with C64 ROMs (`c64_roms`) and a physical cartridge (`cart_slot`,
  `emu_cart_info`, `emu_cart_save`). It does not build firmware. Setup and tools: `docs/status/install.md`, "MCP
  server"; reference: `docs/status/mcp.md`.

## Status

- **M1-M3** (`docs/status/boot.md`): boot log with `*** FPGA Capabilities: 34000222 ***`
  (`34400222` with the TRX64 C64, which adds CAPAB_EEPROM), 60 s emulated in the
  idle loop with 0 unmapped IO accesses, overlay menu driven by the menu button and cursor keys.
- **M4** (`docs/status/storage.md`): config persists in the flash image; an SD image is listed, browsed and written.
- **M5** (`docs/status/network.md`, `docs/status/e2e.md`): REST, Telnet and FTP through libslirp. The upstream E2E
  smoke profile passes 12 of 12 with the REST shim.
- **M6** (`docs/status/usb.md`): USB stick in the file browser (read and write), HID keyboard input, hot-plug on the
  hub ports, and a host directory as a stick with safe write-back (`--usb-dir`, `docs/status/usb-dir.md`).
- **M7** (`docs/status/c64.md`): TRX64 boots to BASIC `READY.` under the overlay, runs a PRG from the file browser
  through the boot cartridge, and shows the Freeze UI (phase A). Wave 4 added an ARMSID in socket 1 and UltiSID 1 on
  reSID with audio out (`docs/status/sid-audio.md`); 27 cartridge types with the Action Replay freezer, the GMOD2
  EEPROM, EasyFlash writes and the SID/MUS players (`docs/status/carts.md`); and drive A as a 1541 with directory,
  LOAD and SAVE on a mounted D64 (`docs/status/drive.md`). `--c64-roms` writes the C64 ROMs into the flash image
  before boot. A cartridge in the physical expansion port (`--cart-slot`, `docs/status/cart-slot.md`) is served by
  TRX64's mappers, flash boards (EasyFlash, GMod2, MegaByter, C64MegaCart) or the U64 cart logic, next to the internal
  cartridge; firmware DMA and REST dumps and flash writes reach it, and the TREX CRT Tool dumps it. Open: UCI, REU,
  the IEC processor, drive B and 1571/1581, a second SID, NTSC.
- **Install** (`docs/status/install.md`): the upstream `update.ue2` and the Commodore `c64u_v1.1.0.ue2` populate the
  flash; the Commodore 1.1.0 application boots and opens its menu. Its network services ship disabled.
- **Speed** (60 s emulated at `--speed max`, upstream ELF): about 124 host MIPS with `--c64 trx64` (12.1 s wall), 111
  with drive A on and the ARMSID playing into a WAV (13.5 s), and 208 with `--c64 none` (7.2 s). Realtime needs
  25 MIPS; with drive, SID and the audio device on it keeps up at 26 % of one core.

Architecture: `docs/ARCHITECTURE.md`. Specs: `docs/specs/`.
