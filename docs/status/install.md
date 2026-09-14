# Usage: build, `run`, config files, `install`, MCP

How to build the emulator, what each `ue2emu run` option does, how a `run --config` TOML file works, how
`ue2emu install` fills a flash image from a `.ue2` updater, and how a Claude Code session drives the emulator through
`ue2-mcp`. Platforms, what is not shipped, ROMs and the TRX64 dependency: `README.md`, "0.1.0".

## 1. Build

**Homebrew:** `brew install jondalar/ue2emu/ue2emu` builds the tagged release from source with Homebrew's Rust and
libslirp and installs `ue2emu` and `ue2-mcp` (tap: https://github.com/Jondalar/homebrew-ue2emu). Platform notes:
`README.md`, "Platforms". Building from a checkout:

| Prerequisite | Needed for | Notes |
|---|---|---|
| Rust (stable) with cargo | everything | built and tested with rustc 1.98.1 |
| C++ compiler | `trx64-core` compiles the vendored reSID (default feature `trx64`) | macOS: Xcode command line tools |
| libslirp | `ue2-net` links `slirp` | macOS: `brew install libslirp`; search order below |
| Network on the first build | cargo fetches `trx64-core` from GitHub | a local checkout instead: `README.md`, "TRX64 dependency" |
| `firmware/1541ultimate` (optional, untracked) | default `--firmware` and `--roms`; the firmware tests | clone of GideonZ/1541ultimate with submodules (neorv32, software/lwip, software/httpd) |
| `tools/bin` (optional, untracked) | `scripts/build-firmware.sh` | `riscv32-unknown-elf-*` links to xPack riscv-none-elf-gcc 11.3.0-1 |

libslirp search path (`crates/ue2-net/build.rs`; the script reruns when `SLIRP_LIB_DIR` changes):
- `SLIRP_LIB_DIR` set: only that directory.
- Otherwise `/opt/homebrew/lib` when it exists (Homebrew on Apple silicon).
- Otherwise the linker's default paths, e.g. a Linux distribution's libslirp development package. Not tested, nor is
  Homebrew on Intel (`/usr/local/lib`).

```sh
cargo build --release -p ue2emu -p ue2-mcp               # target/release/ue2emu, target/release/ue2-mcp
cargo build --release -p ue2emu --no-default-features     # without TRX64 and C++: --c64 none only
scripts/build-firmware.sh                                 # firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf
UE2_FIRMWARE=/path/to/firmware/1541ultimate cargo test --workspace
scripts/smoke-all.sh                                      # release build, every smoke script in a temporary directory
```

- The firmware tests take the tree from `$UE2_FIRMWARE`, default `firmware/1541ultimate` under the repo root; the
  loader tests skip when its ELF is missing (`crates/ue2-core/src/loader.rs:238-246`). With the firmware, main
  `8218a0a` gives 388 passed, 1 ignored.
- `scripts/make-sd-image.sh` (used by `smoke-all.sh`) needs a macOS login session.

## 2. `ue2emu run` options

From `ue2emu run --help`. The defaults under `firmware/1541ultimate` are relative to the repo root the binary was built
from (`crates/ue2emu/src/main.rs:140-155`). In the window F12 is the menu button, the cursor keys navigate and Page Up
is RESTORE.

**Firmware and flash**

| Option | Meaning |
|---|---|
| `--config FILE` | Read `run` flags from a TOML file (section 3) |
| `--firmware ELF` (alias `--elf`) | Firmware image: `ultimate.elf` (with symbols), `ultimate.app` or a `.ue2`; default `firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf` |
| `--roms DIR` | Firmware roms directory: overlay font `chars.bin`, TRX64 ROM seeds, `--c64-roms` source; default `firmware/1541ultimate/roms` |
| `--flash FILE` | Persistent SPI flash image, created erased if missing |
| `--caps HEX` | ITU capability word; default `34000222` |
| `--no-overlay-ui` | Do not seed the overlay user interface into blank flash config |

**C64 (TRX64)**

| Option | Meaning |
|---|---|
| `--c64 trx64\|none` | C64 behind the cart/DMA registers: TRX64, or the T0 register stub (no C64 picture, DMA loads time out); default `trx64` when built with the `trx64` feature |
| `--c64-roms [DIR]` | Put KERNAL, BASIC and CHAR from DIR (default `--roms`) into `/flash/roms` of the flash image before boot; needs `--flash` (`README.md`, "ROMs") |
| `--c64-roms-force` | With `--c64-roms`: replace a `/flash/roms` file whose content differs instead of keeping it |

**Cartridges**

Internal cartridges are loaded from the file browser ("Run Cart", `docs/status/carts.md`); only the physical port has an
option.

| Option | Meaning |
|---|---|
| `--cart-slot SPEC` | `FILE.crt[,rw\|,save=OUT.crt][,flash-decode=11\|15\|both]`: a cartridge in the physical expansion port, independent of the internal one; `rw` writes flash changes back into FILE, `save=` into OUT; needs `--c64 trx64` (`docs/status/cart-slot.md`) |

**SD and USB**

| Option | Meaning |
|---|---|
| `--sd IMAGE` | SD card image (`scripts/make-sd-image.sh`, `scripts/add-sd-files.sh`) |
| `--usb IMAGE` | Raw disk image as a USB mass-storage device; repeatable; images take the hub ports first (`docs/status/usb.md`) |
| `--usb-dir PATH[,size=SIZE][,ro]` | Host directory as a FAT32 USB stick, the guest's changes written back; repeatable, on the hub ports after the images (`docs/status/usb-dir.md`) |
| `--usb-dir-work DIR` | Where `--usb-dir` keeps its volume images, manifests and snapshots; default `run/usb-dir` |
| `--usb-keyboard` | Attach a USB keyboard; the window sends host keys to it instead of the C64 matrix (F12 stays the menu button) |

**Network and web UI proxy** (`docs/status/network.md`)

| Option | Meaning |
|---|---|
| `--net MODE` | `user` (libslirp NAT), `vmnet-bridged[:IFACE]` (LAN address, needs sudo and macOS; IFACE defaults to the default route's interface), `socket-vmnet[:PATH]` (LAN address through the socket_vmnet daemon; PATH defaults to `/opt/homebrew/var/run/socket_vmnet`); default no network |
| `--hostfwd FWD` | Port forwards for `--net user`, `PROTO:[ADDR:]HOSTPORT:GUESTPORT`, comma separated or repeated, ADDR default 127.0.0.1; default `tcp:2323:23,tcp:2121:21,tcp:6464:64` |
| `--web-port PORT` | Web UI proxy for `--net user`: HTTP on 127.0.0.1:PORT to guest port 80, with the web UI's API URLs made to carry the port; `0` turns it off (without `--hostfwd`, guest port 80 is then forwarded plainly from 8080); default 8080 without `--hostfwd`, off with it |

**Audio** (`docs/status/sid-audio.md`)

| Option | Meaning |
|---|---|
| `--audio on\|off` | SID audio through the default output device (needs `--c64 trx64`); default on with a window, off with `--headless` |
| `--audio-wav PATH` | Write the SID sample stream to a WAV: mono, 16 bit, the device rate with audio on, else 44100 Hz |
| `--sid-socket1 none\|armsid` | What SID socket 1 holds (needs `--c64 trx64`); on a flash that has not saved an ARMSID yet the firmware asks to review the settings; default `none` |

**Speed, window, control, debugging**

| Option | Meaning |
|---|---|
| `--speed realtime\|max` | Wall-clock pacing or as fast as the host allows; default `realtime` |
| `--clocks-per-insn N` | Emulated 100 MHz clocks per instruction, at least 1; default 4 |
| `--headless` | Run without a window |
| `--max-seconds S` | Stop after S wall-clock seconds |
| `--script FILE` | Execute a control script (`docs/specs/S08-frontend-control.md`, `docs/status/tooling.md`) |
| `--control ADDR` | Serve the TCP control protocol on ADDR, e.g. `127.0.0.1:6400` (`docs/status/mcp.md`, "Direct API") |
| `--gdb ADDR` | GDB remote stub, e.g. `127.0.0.1:1234`; the machine waits at reset until the debugger continues |
| `--log LIST` | Logging: `unmapped`, `io`, `irq`, comma separated |
| `--no-halt` | Keep running when a firmware fault hook fires |
| `--trace` | Record the last 256 PCs, printed when a fault hook halts the machine (implied by `--gdb`; about 3 % MIPS) |

## 3. Config files: `run --config FILE.toml`

Only `run` takes `--config`. `crates/ue2emu/src/config.rs` turns the file's entries into `--flag=value` arguments after
`run`, leaves out the flags the command line gives, and parses the whole command line again.

- **Keys** are the long flag names without `--`, spelled like the flag: `firmware`, `roms`, `flash`, `sd`, `c64`,
  `caps`, `clocks-per-insn`, `speed`, `headless`, `script`, `control`, `max-seconds`, `gdb`, `log`, `no-overlay-ui`,
  `no-halt`, `trace`, `net`, `hostfwd`, `web-port`, `usb`, `usb-dir`, `usb-dir-work`, `usb-keyboard`, `audio`,
  `audio-wav`, `sid-socket1`, `c64-roms`, `c64-roms-force`, `cart-slot`.
  - The alias `elf` works for `firmware`; setting both is an error.
  - An unknown key is an error, and so is `config`. `help` is not a key.
- **Values** are written as on the command line:
  - A flag with a value: a string, integer or float (`web-port = 8080`, `max-seconds = 5`, `caps = "34000222"`,
    `speed = "max"`).
  - A flag without a value (`headless`, `trace`, `no-halt`, `no-overlay-ui`, `usb-keyboard`, `c64-roms-force`): `true`
    gives it, `false` leaves it out.
  - A repeatable flag (`log`, `hostfwd`, `usb`, `usb-dir`): an array, one occurrence per element; a single string or
    number counts as one occurrence. `log` and `hostfwd` split on commas as on the command line, so `log = "io,irq"`
    works.
  - `c64-roms`: `true` is the bare `--c64-roms`, a string is `--c64-roms=DIR`, `false` leaves it out.
  - Anything else is an error: tables, datetimes, nested arrays, an array for a single-value flag, a non-boolean for a
    flag without a value.
  - A value of the right kind that the flag rejects (`speed = "slow"`) gets clap's normal error, which names
    `--speed` but not the file.
- **The command line wins.** A flag given on the command line replaces the file's value for that flag entirely; for a
  repeatable flag its occurrences replace the file's whole list. A `requires` that only the file meets still counts:
  `--web-port 9000` on the command line works with `net = "user"` in the file.
- **Paths** in the file (command-line values are never rewritten):
  - A relative path resolves against the directory containing the TOML file.
  - A leading `~` alone or `~/…` expands to `$HOME`. `~user` is not expanded and counts as relative.
  - Absolute paths and empty strings stay as they are.
  - This applies to `firmware`/`elf`, `roms`, `flash`, `sd`, `script`, `usb`, `usb-dir-work`, `audio-wav`, a string
    `c64-roms`, and to the path parts of specs: PATH in `usb-dir` `PATH[,size=SIZE][,ro]`; FILE.crt and the OUT.crt
    after `save=` in `cart-slot` (options are taken from the end, so FILE may contain commas); PATH in
    `net = "socket-vmnet:PATH"`. A spec that does not parse is passed through unchanged for clap to report.
- **Startup line** on stderr, before the machine starts: `config: loaded <path> (<N> keys)`.
  - `<path>` is canonical, with symlinks resolved (`/tmp` shows as `/private/tmp` on macOS).
  - `<N>` counts the file's top-level keys, including those the command line overrides; one key reads `(1 key)`.
- `run --config FILE --help` prints the help without reading the file.

Errors, printed as `Error: …` with the cause chain:

| Message | When |
|---|---|
| `config: read <path>` | the file is missing or unreadable |
| `config: parse <path>` | TOML syntax; the cause gives line and column |
| `config: <file>: unknown key '<key>'` | not a long flag of `run` |
| `config: <file>: 'config' cannot be set in a config file` | a `config` key |
| `config: <file>: '<key>' needs true or false` | a flag without a value |
| `config: <file>: '<key>' needs a string or a number` | a flag with one value |
| `config: <file>: '<key>' needs a string, a number or an array of them` | a repeatable flag |
| `config: <file>: '<key>' needs true, false, a string or a number` | `c64-roms` |
| `config: <file>: '<a>' and '<b>' are the same flag` | `elf` and `firmware` both set |

The example, `docs/examples/ue2emu.example.toml`, is checked by `config::tests::the_example_file_is_valid`
(`crates/ue2emu/src/config.rs:214`). Its relative paths follow the file: used in place, `flash = "run/flash.bin"` is
`docs/examples/run/flash.bin`, so copy it next to your images and edit it.

```toml
# Example for `ue2emu run --config ue2emu.example.toml`.
#
# Every long flag of `ue2emu run` is a key, spelled like the flag without its dashes (`ue2emu run --help`).
# Values are written as on the command line:
#   - a flag with a value takes a string or a number     flash = "run/flash.bin"
#   - a flag without a value takes true or false         headless = true
#   - a repeatable flag takes an array, one per use      usb-dir = ["~/c64/usb", "~/c64/tools,ro"]
#   - c64-roms takes true (bare flag) or a directory     c64-roms = "~/c64/roms"
# Relative paths are relative to the directory of this file; a leading ~ or ~/ is your home directory.
# A flag given on the command line replaces its value here: `ue2emu run --config ue2emu.toml --flash other.bin`.

# Firmware: ultimate.elf, ultimate.app or a C64 Ultimate .ue2 update file.
firmware = "~/c64u/c64u_v1.1.0.ue2"

# Persistent SPI flash image, created erased if missing.
flash = "run/flash.bin"

# C64 KERNAL, BASIC and CHAR ROMs into /flash/roms of the flash image before boot: true takes them from the firmware
# roms directory, a string names a directory.
c64-roms = true

# Host directories as USB sticks, PATH[,size=SIZE][,ro]; the guest's changes are written back.
usb-dir = ["~/c64/usb"]

# Wired Ethernet through libslirp NAT; the firmware web UI on http://127.0.0.1:8080.
net = "user"
web-port = 8080

# A cartridge in the physical expansion port: FILE.crt[,rw|,save=OUT.crt][,flash-decode=11|15|both].
# cart-slot = "~/c64/carts/game.crt,save=run/game-saved.crt"

# Wall-clock pacing ("realtime" or "max") and SID audio through the default output device ("on" or "off").
speed = "realtime"
audio = "on"

# Without a window, stopping after a minute.
# headless = true
# max-seconds = 60
```

```sh
target/release/ue2emu run --config ue2emu.toml                       # config: loaded /…/ue2emu.toml (8 keys)
target/release/ue2emu run --config ue2emu.toml --flash other.bin --headless
```

## 4. `ue2emu install`: populating the flash with a `.ue2` updater

Hardware gets its flash contents from the updater inside `update.ue2`, not from a copy of the application.
`ue2emu install` does the same: it runs the updater record inside the emulator with the normal device set, answers its
questions, and lets it write the FPGA image, the application and the `/flash` files (drive ROMs, sound banks, web UI)
into the flash image. **Works for the upstream `update.ue2` and for the Commodore `c64u_v1.1.0.ue2`**, both without a
hang, including the ESP32 flashing step.

```sh
target/release/ue2emu install --update <path>/update.ue2 --flash run/flash.bin --yes --c64-roms
# then boot as usual; the first run seeds the overlay-UI page
target/release/ue2emu run --flash run/flash.bin
```

| Option | Meaning |
|---|---|
| `--update FILE` | `update.ue2`, a Commodore `.ue2`, or the updater's `update.elf` (adds symbols and fault hooks) |
| `--flash FILE` | SPI flash image to write, created erased if missing |
| `--yes` | Answer the updater's questions instead of asking on the terminal: Yes, but No to "Reformat Flash Disk?" and "Reset Configuration?". Without it each question is asked on the terminal (`y`/`n`, or the button name) |
| `--reformat-flash` | Answer Yes to "Reformat Flash Disk?": erases everything on `/flash` (ROMs, carts, web UI, your files) |
| `--reset-config` | Answer Yes to "Reset Configuration?": resets all saved settings |
| `--c64 trx64\|none` | C64 behind the cart/DMA registers, as for `run` |
| `--roms DIR` | Firmware roms directory that seeds the TRX64 ROMs; default `firmware/1541ultimate/roms` |
| `--timeout S` | Emulated seconds without the updater's power-off request before giving up; default 600 |
| `--c64-roms [DIR]` | After the updater and the flash check, put KERNAL, BASIC and CHAR from DIR (default `--roms`) into `/flash/roms`. It runs afterwards because the updater reformats a `/flash` that holds files |
| `--c64-roms-force` | With `--c64-roms`: replace a `/flash/roms` file with other content |
| `--cart-slot SPEC` | A cartridge in the physical expansion port while the updater runs: `FILE.crt[,flash-decode=11\|15\|both]`; never written back |

The firmware console goes to stdout. The answered questions, the final C64 screen, the machine stats and the flash
check go to stderr. The exit status is 0 only when the updater has switched the machine off and the flash check passes.
Code: `crates/ue2emu/src/install.rs`, `loader::load_updater` and `loader::embedded_app` in
`crates/ue2-core/src/loader.rs`, the ESP32 loader and `U64Ctrl::power_event` in `crates/ue2-core/src/devices/wifi.rs`.

### How it works

1. **Load** (`loader::load_updater`). Every record from the start of the file up to the one with a start address is
   loaded: the updater at 0x03000000 (`target/u64ii/riscv/update/linker.x`), not the embedded `ultimate.app` that
   `run --firmware x.ue2` boots. No overlay-UI page is seeded, so the flash receives only what the updater writes.
2. **User interface.** The cart registers report PHI2, so `C64::exists` holds (c64.cc:352-360). The updater's UI is
   therefore the C64 text screen at $0400 with the CIA1 keyboard (update_common.h:198-222, c64.cc:144-145), not the
   overlay and not the UART. `install.rs` reads the screen through `C64Port::dma_peek` (TRX64's RAM, or the T0 stub's
   byte array with `--c64 none`) and sends keys through `Machine::input` to `U64Io` and `C64Port::set_key`.
3. **Questions.** `UIPopup` draws a bordered window with a button row (ui_elements.cc:29-126, screen.cc:463-484).
   `install.rs` scans the C64 screen every 100 ms emulated, recognises that layout, and taps the button key
   (`o`/`y`/`n`/`a`/`c`, userinterface.cc:648-649) for 80 ms. Single-button popups get their button; a popup still
   visible 2 s after its answer gets the key again.
4. **Stop.** Both updaters end in `turn_off`, "Turning OFF machine in 5 seconds....", then `wifi_machine_off`
   (update_common.h:53-76, wifi_cmd.cc:259-264). The ESP32 stub records CMD_MACHINE_OFF / CMD_MACHINE_REBOOT in
   `U64Ctrl::power_event`, and install stops at that request. Neither updater writes ICAP.
5. **Check.** install searches the flash at every page boundary for the embedded `ultimate.app` (the bytes from
   `_ultimate_app_start` to `_ultimate_app_end`, found by `loader::embedded_app`) and looks for the Xilinx sync word
   `AA 99 55 66` in the first 4 KiB of the FPGA image.

**ESP32 step.** `update_esp32` asks the module for its identity twice and flashes it when major/minor differ from the
u64ctrl version the updater carries (update_u64ii.cc:75-99). The WiFi stub identifies as V1.14.
- The upstream `update.ue2` carries u64ctrl V1.14 (software/u64ctrl/main/rpc_dispatch.h:30-37): "No WiFi module
  update needed!".
- Commodore 1.1.0 carries "ESP32 WiFi Bridge V1.11" and flashes. `wifi.rs` answers the ROM serial loader: flowctrl
  b6:4 is the module mode; entering `ESP_MODE_BOOT` (1, esp32.h:21-27) with SLIP off queues the ROM banner as raw
  text, which switching SLIP on cuts into a frame with an appended 0x0A (slip_decoder.vhd:55-75), so `Esp32::Download`
  finds "DOWNLOAD" (esp32.cc:247-268). In boot mode every TX frame gets `01 op 04 00 | value 0 | status 00 00 00 00`,
  the SYNC reply `Download` compares (esp32.cc:240-241) and the success test of `Esp32::Command` (esp32.cc:204-214).
  ATTACH_SPI, CHANGE_BAUDRATE, SET_FLASH_PARAMS, FLASH_BEGIN and FLASH_DATA succeed; other opcodes get status `01 05`.
  The flashed data is not kept.

### Results

All runs start from an erased flash; install runs flat out (`--speed` does not apply). They were recorded before
`--yes` kept data (`6e807c6`): the "Reset Configuration? -> Yes" below is what `--reset-config` gives today.

- **Upstream `update.ue2`** (8.4 MB, identical to `update.app`), with `--c64 trx64` (default) and `--c64 none`: the same
  questions, power request after 15.692 s emulated, application at 0x3C0000, sync word at 0x9e; 5.3 s and 4.4 s wall.

  ```
  install: <path>/update.ue2 (Updater), entry 0x03000000
  install: updater asks "About to update. Continue?" -> Yes
  install: updater asks "Reset Configuration? (Recommended)" -> Yes
  install: power request Off after 15.692 s emulated
  392296853 instructions, 15.692 s emulated, 3147 IRQs taken, pc 0x03000c64
  install: flash 0x3c0000: ultimate.app, 1237788 bytes, identical to the update file
  install: flash 0x000000: FPGA bitstream, sync word at 0x9e
  ```

  - C64 screen: `Creating '/flash/roms'`, `'/flash/carts'`, `'/flash/html'`; `Writing` 1581.rom, 1571.rom, 1541.rom,
    snds1541.bin, snds1571.bin, snds1581.bin, index.html, api.html, openapi.yaml; `Flashing Runtime FPGA..`,
    `Flashing Ultimate Application..`; `WiFi module detected: ESP32 WiFi Bridge V1.14 (1.14)`,
    `No WiFi module update needed!`; `Turning OFF machine in 5 seconds....`
  - Flash: FPGA bitstream at 0; application at 0x3C0000, the XC7A100T slot for the default capabilities 0x34000222
    (update_u64ii.cc:179-180); FAT flash disk at 0x580000 (`MSDOS5.0`); all 24 config pages erased (0xFE8000 reads FF,
    the answer to "Reset Configuration").
  - On a populated flash "Reformat Flash Disk?" comes first (update_common.h:244-253), then the same two questions.
  - Without `--yes`, with `maybe`, `y`, `no` on stdin: the unknown answer is asked again, the update runs, and "Reset
    Configuration" gets No. The run exits 0 with the same flash check.
- **Commodore `c64u_v1.1.0.ue2`** (4.4 MB), `--c64 trx64`: "** Commodore 64 Ultimate Updater **", the same drive ROMs,
  sound banks and `index.html` (no `api.html`/`openapi.yaml`), application slot 0x220000 with the same capability word.

  ```
  install: <path>/c64u_v1.1.0.ue2 (Updater), entry 0x03000000
  install: updater asks "About to update. Continue?" -> Yes
  install: updater asks "Flashing ESP32 Success!" -> Ok
  install: updater asks "Reset Configuration? (Recommended)" -> Yes
  install: power request Off after 18.848 s emulated
  471194518 instructions, 18.848 s emulated, 5482 IRQs taken, pc 0x03000c40
  install: flash 0x220000: ultimate.app, 1025900 bytes, identical to the update file
  install: flash 0x000000: FPGA bitstream, sync word at 0x9d
  ```

  ESP32 console: `Boot message: … rst:0x1 (POWERON),boot:0x4 (DOWNLOAD(USB/UART0/1)) waiting for download`,
  `Setting up Flashing ESP32.`, 21 blocks to 0x0, 3 blocks to 0x8000, 820 blocks to 0x10000,
  `Flashing ESP32 Status: 0.`
- **Booting the result.** `run --headless --speed max --flash <installed flash>` with the upstream ELF: the first boot
  seeds config page 0 with `2E 4E 45 47 08 02 01 01` and the overlay menu opens. F4 (System Information,
  tree_browser.cc:513-517) shows `Drive A: Enabled`, `Drive type: 1541`, `Drive ROM: 1541.rom`; an erased flash shows
  `Drive A: Disabled` (c1541.cc:929-935). `run --firmware <path>/c64u_v1.1.0.ue2 --flash <that flash>` boots the
  embedded Commodore application.
- **KERNAL/BASIC/CHAR.** `Failed to load KERNAL ROM; loading default.` (and CHAR, and BASIC on the Commodore
  application) still prints: the application loads `/flash/roms/kernal.bin`, `basic.bin` and `chars.bin` (defaults
  c64.cc:84-86, loads c64.cc:1091-1107), and neither updater writes them (update_u64ii.cc:161-169). `--c64-roms` on
  `install` or `run` writes them.

### Known gaps

- **Flash protection is not modelled.** `S25FLxxxL_Flash::protect_disable` / `protect_configure` read the register
  with opcode 0x65 and write it with 0x01 (s25fl_l_flash.cc:193-287); the model answers 0xFF and ignores the write
  (`Status register before locking: FF, requested: 34`). No flow depends on it.
- **ESP32 flash contents are discarded.** The stub keeps identifying as V1.14, so the Commodore updater flashes the
  module again on every install. There is no FLASH_DATA checksum check.
- **The ROM banner is the ESP32-C3 text.** The module type on U64-II is not confirmed in doc 04; the firmware only
  needs "DOWNLOAD" in the first 100 bytes.
- **`run` ignores power requests.** MACHINE_OFF / MACHINE_REBOOT are recorded but only `install` acts on them
  (doc 04 T1).
- **FPGA image check is shallow.** Only the Xilinx sync word is checked. The bitstream bounds are symbols of the
  updater (`_u64e2_100t_swp_start/_end`) that a `.ue2` file does not carry.

## 5. MCP server (`ue2-mcp`)

`ue2-mcp` is a stdio MCP server that lets a Claude Code session boot a firmware build in headless emulator instances
and drive them: overlay UI, screen text, screenshots, UART console, REST, USB directories, a physical cartridge. It
takes no arguments; each instance is a child `ue2emu run --headless --control 127.0.0.1:<port> …` with its files in
`run/mcp/<id>/`. It does not build firmware. Reference (instances, flash modes, parameters, results, the control
protocol): `docs/status/mcp.md`.

**Build:** `cargo build --release -p ue2emu -p ue2-mcp`, or install both with Homebrew (section 1). An installed
`ue2-mcp` outside a checkout starts the `ue2emu` next to it and keeps its instances in `~/.ue2emu/run/mcp/`.

**Register** it in the project that uses the emulator (for example a 1541ultimate checkout), as `.mcp.json`:

```json
{
  "mcpServers": {
    "ue2emu": {
      "type": "stdio",
      "command": "<emulator checkout>/target/release/ue2-mcp",
      "args": [],
      "env": {
        "UE2_FIRMWARE_TREE": "<1541ultimate checkout>"
      }
    }
  }
}
```

or with the Claude Code CLI, run in that project (`--scope project` writes `.mcp.json`):

```sh
claude mcp add ue2emu --scope project -e UE2_FIRMWARE_TREE=<1541ultimate checkout> -- <emulator checkout>/target/release/ue2-mcp
```

With Homebrew the command is `$(brew --prefix)/bin/ue2-mcp`, e.g. `/opt/homebrew/bin/ue2-mcp` on Apple silicon.

**Environment** (all optional, `crates/ue2-mcp/src/config.rs`):

| Variable | Default | Meaning |
|---|---|---|
| `UE2_REPO` | the repo containing the binary, else `~/.ue2emu` | emulator checkout (`run/`) |
| `UE2EMU_BIN` | `$UE2_REPO/target/release/ue2emu`; outside a checkout the `ue2emu` next to `ue2-mcp` | emulator binary |
| `UE2_FIRMWARE_TREE` | `$UE2_REPO/firmware/1541ultimate` | default 1541ultimate checkout: `target/u64ii/riscv/ultimate/result/ultimate.elf` and `roms/` |
| `UE2_MCP_RUN` | `$UE2_REPO/run/mcp` | instance directories |

**Tools** (`crates/ue2-mcp/src/tools.rs`). `hold_ms`, `settle_ms` and `ms` are emulated milliseconds, `timeout_ms` is
wall clock. A failed assertion is a normal result starting `FAIL:`.

| Tool | What it does |
|---|---|
| `emu_start` | Start an instance (firmware, flash, `c64_roms`, SD, `usb_dirs`, `cart_slot`, `net`, speed) and wait until the console shows `All linked modules have been initialized` |
| `emu_stop` | Stop an instance (or `all`) with `quit`, so the flash is saved; after `timeout_ms` SIGTERM, then SIGKILL |
| `emu_list` | The instances: alive, exit, ports, paths, command line |
| `emu_console` | Read the UART console (or `stream: "stderr"`) from an offset or as a tail |
| `emu_screen` | Overlay text rows, with whether the overlay is visible |
| `emu_screenshot` | The display as a PNG (scale 2), optionally saved to a file |
| `emu_button` | Press and release the Ultimate menu button |
| `emu_key` | Press keys by name in order (`return`, `down`, `f2`, `runstop`, `a`, …) |
| `emu_type` | Type text, one key per character (`\n` = RETURN) |
| `emu_wait` | Run for `ms` emulated milliseconds |
| `emu_expect` | Assert that text appears (or with `absent`, disappears) on the screen or the console within `timeout_ms` |
| `emu_rest` | HTTP request to the firmware web server, REST under `/v1/` (needs `net: true`) |
| `emu_control` | One raw line of the control protocol |
| `emu_usb_sync` | Sync a `usb_dirs` stick to its host directory now (`force`, `replug`, `discard`) |
| `emu_cart_info` | Describe the cartridge in the physical expansion port (needs `cart_slot`) |
| `emu_cart_save` | Write that cartridge, as it is now, to a CRT file |

**A typical session**, the steps of `scripts/smoke-menu.ctl`:

1. `emu_start {}` → `STARTED emu1: firmware booted (… ms)` and JSON with `id`, `ports`, `run_dir`, `console_offset`.
2. `emu_button {"id": "emu1"}`, then `emu_expect {"id": "emu1", "text": "Flash Disk", "require_visible": true}` →
   `PASS: "Flash Disk" is on the screen` with the screen. `emu_screen {"id": "emu1"}` dumps it again;
   `emu_screenshot {"id": "emu1", "save_to": "run/menu.png"}` also shows the selection bar, which the text dump
   does not.
3. `emu_key {"id": "emu1", "keys": ["down"]}` twice, then `emu_key {"id": "emu1", "keys": ["right"]}`: each call
   settles 300 ms emulated after its last key, as the script's `wait 300`. This enters the third entry, Temp.
   `emu_type` enters text (`\n` = RETURN).
4. `emu_expect {"id": "emu1", "text": "/Temp/", "require_visible": true}` → `PASS: …`.
5. `emu_stop {"id": "emu1"}` → `graceful_quit: true`, `exit` code 0 and the stats line.

`scripts/mcp-smoke.py --server target/release/ue2-mcp` runs a similar session over stdio as an MCP client
(`docs/status/mcp.md`, "Verification").
