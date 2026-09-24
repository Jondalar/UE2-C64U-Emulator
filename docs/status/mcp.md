# MCP server (`ue2-mcp`) and the direct control API

`ue2-mcp` lets a Claude Code session (for example firmware work in a 1541ultimate checkout) test
firmware in the emulator on its own: boot a firmware build, drive the overlay UI, read the screen and the
UART console, take screenshots, call REST, and tear everything down. It is a stdio MCP server
(`crates/ue2-mcp`, official Rust SDK `rmcp` 3.3) that runs each emulator as a child process
`ue2emu run --headless --control 127.0.0.1:<port> …` and talks to it through the TCP control protocol, which is
also usable directly (last section).

## Setup

```sh
cd <emulator checkout>
cargo build --release -p ue2emu -p ue2-mcp      # target/release/ue2emu, target/release/ue2-mcp
# or: brew install jondalar/ue2emu/ue2emu       # $(brew --prefix)/bin/ue2emu, $(brew --prefix)/bin/ue2-mcp
```

The emulator does not build firmware. The session that owns the firmware tree builds it with its own build; the
server only boots the resulting image (ELF, `.app` or `.ue2`) and needs the tree's `roms/`.

`.mcp.json` in the project that wants to use the emulator (here: the 1541ultimate checkout):

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

With `UE2_FIRMWARE_TREE` set, `emu_start` boots that checkout's
`target/u64ii/riscv/ultimate/result/ultimate.elf` by default. Without it, it uses the emulator repo's
`firmware/1541ultimate`.

| Variable | Default | Meaning |
|---|---|---|
| `UE2_REPO` | the repo containing the binary, else `~/.ue2emu` | emulator checkout: `run/` |
| `UE2EMU_BIN` | `$UE2_REPO/target/release/ue2emu`; outside a checkout the `ue2emu` next to `ue2-mcp` | emulator binary |
| `UE2_FIRMWARE_TREE` | `$UE2_REPO/firmware/1541ultimate` | default firmware checkout (boot image, roms) |
| `UE2_MCP_RUN` | `$UE2_REPO/run/mcp` | instance directories |

## Instances

- **Id:** `emu1`, `emu2`, … An id is claimed atomically through `<run>/<id>.claim`, which holds the server pid.
  Several servers (several Claude sessions) can share `run/mcp`; claims of dead servers are taken over.
- **Run directory** `run/mcp/<id>/`, cleared when the id is reused:
  - `console.log`: the full UART console.
  - `stderr.log`: emulator diagnostics and the stats line.
  - `flash.bin`
  - `shots/`
  - `instance.json`: pid, ports, argv.
- **Ports:** every instance gets free localhost ports.
  - `control` always.
  - With `net: true` also `http`, `telnet`, `ftp`, `dma`: `--net user --web-port <http> --hostfwd …` forwards
    `telnet`/`ftp`/`dma` to guest ports 23/21/64. `http` is the emulator's web UI proxy to guest port 80
    (`docs/status/network.md`, "Web UI proxy"). REST passes through it unchanged, so `rest_url` and `emu_rest` use it,
    and `rest_url` opened in a browser shows a working firmware web UI.
- **USB directories:** `usb_dirs` shares host directories as sticks (`docs/status/usb-dir.md`). Their volumes live in
  `run/mcp/usb-dir/`, outside the instance run directories, so an image with unsynced guest writes survives the reuse
  of an id and the next instance on that directory resumes it. One running instance per directory.
- **Flash:**
  - `fresh` (default): a new image in the run dir, with the overlay UI seeded.
  - `persist`: `run/mcp/persist-flash.bin`, kept across instances. Only one running instance may use a given image.
  - `none`: in memory only.
  - A path to an image file.
  - The image is written back when the emulator quits cleanly.
  - `c64_roms: true` puts the C64 KERNAL, BASIC and CHAR from the roms directory into `/flash/roms` of the image
    before boot (`ue2emu run --c64-roms`, `docs/status/c64.md`), so the C64 boots to BASIC `READY.`. A string names
    another ROM directory. A differing file stays; `extra_args: ["--c64-roms-force"]` replaces it. Refused with
    `flash: "none"`.
- **Physical cartridge:** `cart_slot: {path, mode?, save_path?, flash_decode?}` passes `--cart-slot`
  (`docs/status/cart-slot.md`). `mode` `ro` (default) never writes the file; `rw` writes flash and EEPROM changes back
  into it (2 s emulated after the last change and on `emu_stop`, original kept as `FILE.crt.bak`); `save` writes them to
  `save_path` instead. `emu_cart_info` shows the cartridge, `emu_cart_save` writes it as it is now to a CRT file. Dumps
  and flash writes run over `emu_rest` (`PUT /v1/machine:pause`, `PUT /v1/machine:writemem`, `GET /v1/machine:readmem`,
  `PUT /v1/machine:resume`), so the instance needs `net: true`.
- **Lifetime:** instances run until `emu_stop`.
  - `emu_stop` sends `quit`, so the emulator exits cleanly and saves flash. After `timeout_ms` it escalates to
    SIGTERM, then SIGKILL.
  - When the server exits (stdin EOF, SIGTERM/SIGINT/SIGHUP), it stops every instance the same way.
  - If the server dies without cleaning up (SIGKILL, crash), a detached watchdog shell per instance notices at once:
    it blocks on a pipe that only the server holds, even while the dead server is an unreaped zombie. It sends
    `quit` to the control port, so flash is still saved, then SIGTERM/SIGKILL. In the test the orphan was gone after
    0.2 s.
- **Crash:** an emulator that halts on a firmware fault stays listed (`alive: false`), so its console and
  `stream: "stderr"` (the `halted:` report) can still be read. Remove it with `emu_stop`.
- **Speed:** `max` (default) runs several times faster than hardware. Every wait in the control protocol is
  emulated time, so UI scripts behave the same at either speed. Use `realtime` when wall-clock behaviour matters.

## Tool reference

Conventions:

- `hold_ms`, `settle_ms` and `ms` are **emulated** milliseconds. `timeout_ms` and `timeout_s` are **wall clock**.
- Failures of the tool itself come back as MCP tool errors (`isError: true`, text starting `error:`).
- A failed assertion is a normal result whose first line is `FAIL: …` (`PASS: …` otherwise).
- Console offsets are absolute byte positions since the instance started. The server keeps the newest 4 MB.

| Tool | Parameters | Result |
|---|---|---|
| `emu_start` | `firmware?` (ELF, .app, .ue2 or checkout dir), `roms?`, `flash?` (`fresh`/`persist`/`none`/path), `c64_roms?` (`true` or a ROM directory), `sd?`, `usb_dirs?` (`["PATH[,size=SIZE][,ro]"]`), `cart_slot?` (`{path, mode?: "ro"/"rw"/"save", save_path?, flash_decode?: "11"/"15"/"both"}`, a .crt in the physical expansion port, `docs/status/cart-slot.md`), `net?`, `speed?` (`max`/`realtime`), `extra_args?`, `wait_for_boot?` (true), `boot_marker?`, `boot_timeout_ms?` (90000) | `STARTED emuN: firmware booted (… ms)`, then JSON with `id`, `pid`, `ports`, `rest_url`, `run_dir`, `usb_dirs`, `cart_slot`, `banner`, `console_offset`, `command`. A boot timeout or emulator exit is an error that includes console and stderr tails |
| `emu_stop` | `id` (or `"all"`), `timeout_ms?` (10000; 120000 with `usb_dirs`, whose last sync runs on quit) | per instance: `graceful_quit`, `forced`, `exit`, `stderr_tail` (stats line) |
| `emu_usb_sync` | `id`, `port?` (1-3, default all `usb_dirs` sticks), `force?`, `replug?`, `discard?` (with `replug`), `timeout_ms?` (120000) | `PASS: usb-sync …` with one line per action (written, moved to `.ue2-trash`, conflict copies), or `FAIL: …` for a sync the mass-deletion guard refused, an image that did not parse, or an incomplete sync (`NOT WRITTEN` lines; the image stays unsynced). It first waits until the guest has not written for 2 s emulated (at most 10 s). `replug` unplugs, syncs, rebuilds from the host and plugs in |
| `emu_cart_info` | `id` | the physical cartridge's `cart-info` lines: `type`, `name`, `hardware`, `model`, `banks`, `exrom`, `game`, `mode`, `cart_detect`, `bus_internal`, `bus_external`, `bus_bridge`, `flash_decode`, `source`, `persist`, `writable`, `dirty`, `generation`, `saved_generation`, `unsaved`. An error without `cart_slot` |
| `emu_cart_save` | `id`, `path`, `timeout_ms?` (60000) | `PASS: cart-save <path>` with `saved:`, `bytes:`, `generation:`: the cartridge as it is now (flash and EEPROM) as a CRT; the source file is not touched |
| `emu_list` | — | instances (alive, exit, ports, paths, `cart_slot`, command line), configured paths |
| `emu_console` | `id`, `since_offset?`, `tail_lines?` (60), `max_bytes?` (65536), `stream?` (`stdout`/`stderr`) | text, then `start_offset`, `next_offset`, `end_offset`, `more_available`, `lost_bytes`, `alive` |
| `emu_screen` | `id`, `check_visible?` (true) | `overlay: VISIBLE` or `HIDDEN`, then the overlay text rows |
| `emu_screenshot` | `id`, `scale?` (2), `save_to?` | PNG image content, then `path`, size, `overlay_visible` |
| `emu_button` | `id`, `hold_ms?` (100), `settle_ms?` (300) | confirmation |
| `emu_key` | `id`, `keys` (names), `hold_ms?` (80), `settle_ms?` (300) | confirmation |
| `emu_type` | `id`, `text` (`\n` = RETURN), `settle_ms?` (300) | confirmation |
| `emu_wait` | `id`, `ms` | emulated vs wall time |
| `emu_expect` | `id`, `text`, `source?` (`screen`/`console`), `timeout_ms?` (10000), `since_offset?`, `ignore_case?`, `absent?`, `require_visible?` | `PASS:`/`FAIL:`, evidence (screen, or console around the match or its tail), JSON with `elapsed_ms`, `match_offset`, `next_offset` |
| `emu_rest` | `id`, `path`, `method?` (GET), `body?`/`body_file?`, `content_type?`, `headers?`, `timeout_ms?` (20000), `save_body_to?` | `HTTP <status> <reason> (…)`, headers, body. Retries while the web server does not answer yet |
| `emu_monitor` | `id`, `command` (one monitor command), `timeout_ms?` (60000) | the monitor's text: `r`, `m`, `d`, `bk`, `flow`, `help`, `device c64\|drive8\|fw` (S23). Needs a C64 in the instance. Run control: `g`, `z`/`step`, `n`, `ret`, `until`, `c64 halt\|go\|step`, `fw halt\|go\|step` (`docs/status/monitor.md`) |
| `emu_control` | `id`, `command` (one protocol line), `timeout_ms?` (60000) | raw result lines + `ok` |

**Key names** (`emu_key`, control `key`):

- Named keys: `return`, `down`, `up`, `left`, `right`, `f1`-`f8`, `space`, `del`, `inst`, `home`, `clr`, `runstop`,
  `ctrl`, `cbm`, `lshift`, `rshift`, `pound`, `uparrow`, `larrow`.
- Any single character: `a`; `A` is SHIFT+A.

**Menu behaviour**, as used by `scripts/smoke-flash-1.ctl`:

- The menu button opens the file browser: SD, Flash, Temp, Ftp, Net0 with `net`, WiFi.
- `up`/`down` move; separators are skipped.
- `f2` opens the configuration menu.
- `right` enters a category.
- `return` on a value opens its choices; a typed letter jumps to the first match.
- `left` goes back. Leaving config may ask "Save changes to Flash?"; `return` = Yes.
- `f3` shows help.
- `runstop` closes the menu.

The selection bar is colour-only: it shows in `emu_screenshot`, not in `emu_screen`.

## Worked example: boot my firmware build, open the menu, verify a string, screenshot

The calls a session makes, with results trimmed from the verification run below (upstream ELF):

1. Build the firmware in its own session with its own build (not through the emulator).
2. `emu_start {"net": true}`:
   ```
   STARTED emu1: firmware booted (258 ms wall clock)
   { "id": "emu1", "banner": ["*** Ultimate 64-II (V1.01) 3.15 ***", "*** FPGA Capabilities: 35640226 ***", …],
     "ports": {"control": 64602, "http": 64603, "telnet": 64604, "ftp": 64605, "dma": 64606},
     "console_offset": 5150, … }
   ```
3. `emu_button {"id": "emu1"}`, then `emu_expect {"id": "emu1", "text": "Flash Disk", "require_visible": true}`:
   ```
   PASS: "Flash Disk" is on the screen
   --- screen ---
     *** Ultimate 64-II (V1.01) 3.15 ***
   ----------------------------------------
   SD      SD Card                No media
   Flash   Flash Disk             Ready
   Temp    RAM Disk               Ready
   Ftp     Remote FTP Servers     Ready
   Net0    IP: 0.0.0.0            Link Up
   WiFi    MAC 02:15:41:00:00:01  Link Down
   …
   /                              -F3=HELP-
   ```
4. `emu_screenshot {"id": "emu1", "save_to": "run/menu.png"}` returns the PNG (640×450 at scale 2) with the
   selection bar on `SD`.
5. Optional: `emu_rest {"id": "emu1", "path": "/v1/info"}`:
   ```
   HTTP 200 OK (314 body bytes, 1 attempt(s)) for GET /v1/info via 127.0.0.1:64603
   { "product" : "Ultimate 64-II", "firmware_version" : "3.15", "git_commit_hash" : "b617777c", … }
   ```
   To check what an action logged, pass the `console_offset` (or a `next_offset`) you had before it:
   `emu_console {"id": "emu1", "since_offset": 5150}` returns
   `… Accept client 0 on socket 5.  10.0.2.2:64608 / HTTP GET /v1/info`.
6. `emu_stop {"id": "emu1"}`:
   ```
   "graceful_quit": true, "exit": {"code": 0},
   "stderr_tail": "… 1156890831 instructions, 46.276 s emulated, 9169 IRQs taken, pc prvIdleTask+0x2c …"
   ```

## Verification

`scripts/mcp-smoke.py` drives the server over stdio exactly like an MCP client. It runs, in order:

1. `initialize`, `tools/list`
2. `emu_start` with `net`
3. `emu_expect` console `All linked modules`
4. `emu_button`
5. `emu_expect` screen `Flash Disk` (visible)
6. `emu_screenshot` (PNG checked)
7. `emu_rest GET /v1/info` (200)
8. `emu_key`, `emu_console`
9. `emu_stop` (graceful)

It prints the JSON-RPC transcript and exits non-zero on any failed step.

```sh
UE2_FIRMWARE_TREE=/path/to/1541ultimate scripts/mcp-smoke.py --server target/release/ue2-mcp
```

Result on the upstream ELF (3.15): every step passed, 15 tools listed. Boot took 0.26 s wall clock. `/v1/info`
answered on the first attempt after 5.7 s (DHCP and web server start). The server exited 0. After the `--cart-slot`
merge the server lists 16 tools (the two build tools removed, `emu_usb_sync`, `emu_cart_info` and `emu_cart_save`
added) and the smoke passes again; `cart_slot` together with `c64_roms` is checked in `docs/status/cart-slot.md`.
S23 added `emu_monitor`: 17 tools.

With the web UI proxy (`http` = `--web-port`) the smoke passes unchanged: `rest_url` `http://127.0.0.1:56351`, the
emulator's stderr `net: web UI http://127.0.0.1:56351/ (proxy to guest port 80 through 127.0.0.1:56355; …)`, and
`emu_rest GET /v1/info` `HTTP 200 OK (314 body bytes, 1 attempt(s))`.

Further checks run during development (scratch scripts, not in the repo):

- **Remaining tools and error paths:**
  - Hidden vs visible overlay.
  - `emu_type`, `emu_wait`, `emu_control`.
  - FAIL and `absent` expects.
  - Unknown id, REST without net, reserved `extra_args`, a flash image already in use.
  - Two instances side by side. Closing stdin stopped both emulators cleanly (stats lines in `stderr.log`).
- **Two servers on one run base:** a simultaneous `emu_start` got distinct ids. After SIGKILL of one server, the
  watchdog stopped its emulator within 0.2 s; the other server's instance stopped gracefully.

## Direct API: the TCP control protocol

The MCP tools are a layer over `ue2emu`'s control protocol (`crates/ue2emu/src/control.rs`,
`docs/specs/S08-frontend-control.md`). Scripts and tests can use it without MCP:

- Start the emulator yourself:
  `target/release/ue2emu run --headless --speed max --control 127.0.0.1:6400 [--flash f.bin] [--net user]`.
- Or take `ports.control` from `emu_start`/`emu_list`.

Details that matter:

- **Connection:** TCP, one client at a time. The next client is accepted when the current one disconnects.
- **Commands:** one per line. Blank lines and `#` comments are accepted and answered with `ok`. Trailing whitespace
  is trimmed.
- **Replies:** each line is answered by its result lines and then `ok`, or by a single `error line <n>: <message>`
  (lines counted per connection). `screen` output sits between `--- screen ---` markers, and a screen row can
  itself read `ok`.
- **Timing:** waits are emulated time. The reply comes when the command has finished.
- **Stdout:** the emulator's stdout is the firmware console and stderr its diagnostics; neither is on the socket.
- **`quit`:** answers `ok` and stops the emulator, which writes the flash image back.

| Command | Effect |
|---|---|
| `wait <ms>` | run `ms` emulated ms |
| `button [ms]` | hold the menu button (default 100) |
| `key <name> [ms]` | hold a key (default 80), then a 40 ms release gap; SHIFT leads shifted keys by 20 ms |
| `key <a+b> [ms]` | a chord: the keys pressed in order, held together, released |
| `hold <keys>` / `release <keys>` | press C64 matrix keys until `release` (also the keys of `--hold-key`) |
| `type <text>` | tap each character (text after the first space, verbatim) |
| `usbkey <name> [ms]` | a key on the USB keyboard (`docs/status/usb.md`) |
| `usbmouse <dx> <dy> [buttons]` | a move and the buttons of the USB mouse (S32) |
| `joy <port> <dirs> [ms]`, `joy-hold <port> <dirs>`, `joy-release <port>` | the joystick on control port 1 or 2; `<dirs>` is `up`, `down`, `left`, `right`, `fire` joined by `+` (S36) |
| `screen` | overlay text dump between `--- screen ---` markers (ignores visibility) |
| `c64screen` | the C64 text screen (`docs/status/c64.md`) |
| `png <path>` | render the display to an RGB PNG (font from the `--roms` directory) |
| `expect`, `expect-not`, `expect-console`, `expect-c64` `<text> [ms]` | wait for text on the overlay, the console or the C64 screen (`docs/status/tooling.md`) |
| `usb-sync [--force] [port]`, `usb-replug [--discard] [port]` | sync a `--usb-dir` stick (`docs/status/usb-dir.md`) |
| `usb-plug <port> image <path>\|keyboard\|mouse`, `usb-unplug <port>` | plug a USB device in or out while running (S33) |
| `monitor <cmd>` | one monitor command (`docs/status/monitor.md`) |
| `cart-info`, `cart-save <path>` | the cartridge in the physical port (`docs/status/cart-slot.md`) |
| `quit` | stop the emulator |

```sh
$ printf 'button\nwait 500\nscreen\n' | nc 127.0.0.1 6400
ok
ok
--- screen ---
  *** Ultimate 64-II (V1.01) 3.15 ***
…
--- screen ---
ok
```

```python
import socket
s = socket.create_connection(("127.0.0.1", 6400)); f = s.makefile("rw")
def cmd(line):
    f.write(line + "\n"); f.flush(); out, screen = [], False
    while True:
        l = f.readline().rstrip("\n")
        if l == "--- screen ---": screen = not screen
        elif not screen and l == "ok": return out
        elif not screen and l.startswith("error line "): raise RuntimeError(l)
        else: out.append(l)
```

Limits of the protocol today:

- No overlay-visibility flag. `ue2-mcp` infers visibility from a rendered frame: a hidden overlay is uniform
  backdrop.
- `ue2-mcp` polls the screen and console for `emu_expect` (wall-clock timeout) instead of using `expect`.
- The emulator does not notice its parent dying, which is why the watchdog exists.
