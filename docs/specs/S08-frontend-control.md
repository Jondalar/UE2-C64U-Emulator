# S08 — Frontend: window, keymap, control/script

**Owns:**
- `crates/ue2emu/src/{window.rs,keymap.rs,control.rs}`
- the `[dependencies]` of `crates/ue2emu/Cargo.toml` (additions)

**Reads:** `docs/ARCHITECTURE.md` §Host side, `docs/specs/S07-overlay-u64io-render.md` (matrix convention), `docs/hw/05-ui-overlay-input.md` §C, firmware `software/io/c64/keyboard_c64.cc` (keymaps)
**Uses (do not change):** `runner::{spawn, EmuHandle, Command, RunOptions}`, `ue2_core::render::{Renderer, text_dump}`, `ue2_core::host::*`

## Scope

**`window.rs` — `run_window(cfg, opts)`**
- winit 0.30 `ApplicationHandler` + softbuffer 0.4; this pairing is proven in TRX64's `crates/trx64-cli` (https://github.com/Jondalar/TRX64).
- Spawns the emulator via `runner::spawn`. Renders the latest `DisplaySnapshot` at 50 Hz, filling the window
  with the aspect kept; a resize snaps the window back to the image's ratio (`aspect_snap`, after trx64-cli).
- Title shows emulated time and MIPS.
- Keyboard events go through `keymap` → `Command::Input(HostInput::Key{..})`.
- **F12** = menu button (press/release → `MenuButton`). ESC may map to RUN/STOP.
- Closing the window sends Quit, joins, and prints stats.

**`keymap.rs`** — Mac keys → C64 matrix positions per `keyboard_c64.cc`:
- letters, digits, space, return, backspace → DEL, cursor keys → CRSR with SHIFT for up/left, F1-F8
  (shifted variants), Home, shift keys, Ctrl, Commodore (Option);
- a name → (row, col) table for the control language (`return`, `down`, `up`, `left`, `right`, `f1`, `space`,
  `runstop`, …).

**`control.rs`**
- `run_script(handle, path)` and `serve(handle, addr)` (TCP, one client at a time, line protocol, `ok`/
  result lines back).
- **Commands:**

| Command | Effect |
|---|---|
| `wait <ms>` | Waits on emulated time via `handle.now_ms`. |
| `button [ms]` | Default 100 ms. |
| `key <name> [ms]` | Default 80 ms hold, then a 40 ms release gap. Names joined with `+` are a chord (`key cbm+z`): pressed in order 20 ms apart, held together, released at once. |
| `hold <names>` / `release <names>` | Presses or lets go of matrix keys (`cbm`, `ctrl+c`) with no timed release; `release` also ends a `--hold-key`. |
| `type <text>` | Types the text through the keymap. |
| `screen` | Prints `text_dump` of the current snapshot between `--- screen ---` markers. |
| `usbkey <name> [ms]` | Holds a key of the USB keyboard (`--usb-keyboard`, `usb::usage_by_name`), default 80 ms, then the release gap. |
| `usbmouse <dx> <dy> [buttons]` | Moves the USB mouse (`--usb-mouse`) and sets its buttons, bit 0 left, 1 right, 2 middle (S32). |
| `png <path>` | Renders the snapshot to a PNG (`png` crate), font from `ControlHandle::rom_dir` (`--roms`). |
| `expect <text> [ms]` | Waits until `text_dump` contains the text; default timeout 5000 ms emulated. |
| `expect-not <text> [ms]` | Waits until `text_dump` no longer contains the text. |
| `expect-console <text> [ms]` | Waits until the console output after the previous `expect-console` match contains the text. |
| `expect-c64 <text> [ms]` | Waits until the C64 text screen (`c64screen`) contains the text; a timeout prints that screen. |
| `monitor <cmd>` | Runs one line of TRX64's monitor against the C64 and prints its text (S23; needs `--c64 trx64`). |
| `quit` | Quits. |

- Lines starting with `#` are comments.
- `<text>` is one word or a double-quoted string (`\"`, `\\` escapes).
- Errors name the line number. A timed-out `expect*` prints the screen first; a failing headless script exits
  non-zero.
- `button`, `key`, `type` and `usbkey` are sent as one timed sequence (`Command::Inputs`) and applied by the emulation thread
  at exact emulated times, so `--speed max` cannot stretch a hold. Status: `docs/status/tooling.md`.

## Tests

- Keymap table covers all names used by the control language.
- Script parser: every command, bad input.
- Control commands against a fake handle, if practical. Otherwise parse-only tests plus a doc comment on
  manual verification.

## Acceptance

`cargo build -p ue2emu` and `cargo test -p ue2emu` pass. Window verification happens in S10: an agent may
build the window but must not require a person. The PNG path makes rendering verifiable headless.
