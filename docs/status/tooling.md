# Tooling status: the control language as a test harness

The control language (`crates/ue2emu/src/control.rs`, spec `docs/specs/S08-frontend-control.md`) can now assert.
Smoke scripts check their own results, and one command runs them all. This closes review findings
REVIEW-DEVICES 1 (no assertions; results checked by `grep` on logs) and 2 (key and button holds timed on the
control thread), plus the M4 open issues in `docs/status/storage.md` (no `expect`; `png` finds its font only
through `$UE2_FIRMWARE`).

## Run

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate   # default: firmware/1541ultimate under the repo root
scripts/smoke-all.sh
```

`smoke-all.sh` works in this order and stops at the first failure:
1. `cargo build --release`.
2. Creates a temporary run directory and changes into it, so the scripts' `run/…` PNG paths land there.
3. Runs every smoke script headless at `--speed max`.
4. Runs a negative control.

On a failure it prints the log tail and stderr, keeps the directory and exits 1. On a pass it removes the
directory.

| Run | Script | Options | Checks |
|---|---|---|---|
| menu | `smoke-menu.ctl` | `--flash run/flash.bin` | root browser text; after DOWN, DOWN, RIGHT the path is `/Temp/` |
| sd | `smoke-sd.ctl` | `--flash run/flash.bin --sd run/sd.img` | `SD Card Ready`; `/SD/` and `/SD/demo.d64/` listings; console `3 children fetched from SD.`, `2 children fetched from demo.d64.` |
| flash-1 | `smoke-flash-1.ctl` | `--flash run/flash-ui.bin` (fresh) | Color Scheme = C128 Style; save popup; console `Writing config store 'User Interface Settings' to flash`, `Page: 0 done.` |
| flash-2 | `smoke-flash-2.ctl` | `--flash run/flash-ui.bin --no-overlay-ui` | browser on the overlay; `Interface Type Overlay on HDMI`; `Color Scheme C128 Style` |
| negative | generated | `--flash run/flash.bin` | `expect "NO SUCH TEXT ON THE SCREEN" 500` must exit non-zero and name line 2 |

`make-sd-image.sh run/sd.img` runs between menu and sd. Result on this Mac (upstream `ultimate.elf` V1.01 3.15,
10 CPUs):

```
PASS menu (2.204 s emulated, 1 s wall)
PASS sd (1.436 s emulated, 0 s wall)
PASS flash-1 (4.556 s emulated, 1 s wall)
PASS flash-2 (2.572 s emulated, 0 s wall)
PASS negative (exit code 1)
all smoke tests passed
```

The suite also passed with one busy loop per CPU running beside it, with the same emulated times to within
12 ms. The fixed `wait 4000` at the start of each script is now `expect "F3=HELP" 10000`: the
firmware takes the menu button as soon as the browser has drawn its help line, at about 1.1 s emulated. The menu
script needed 5.8 s emulated before.

## New commands

| Command | Passes when | Timeout message |
|---|---|---|
| `expect <text> [ms]` | `render::text_dump` contains `text` | `expect "…": not on the screen within <ms> ms emulated` |
| `expect-not <text> [ms]` | `text_dump` no longer contains `text` | `expect-not "…": still on the screen after <ms> ms emulated` |
| `expect-console <text> [ms]` | the console output after the previous `expect-console` match contains `text` | `expect-console "…": not in the console output within <ms> ms emulated` |

Merged from other branches: `usbkey <name> [ms]` (a timed sequence like `key`, `docs/status/usb.md`) and `c64screen`
(`docs/status/c64.md`).

- **Text:** `<text>` is one word, or a double-quoted string with `\"` and `\\` escapes, e.g.
  `expect "SD      SD Card                Ready" 3000`. Unquoted text with spaces is rejected at parse time.
- **Timeout:** 5000 ms emulated by default. Checks poll the display snapshot (published every 20 ms emulated) or
  the console log with a 1 ms wall-clock sleep between checks. A check that fails at or after the deadline is the
  last one.
- **Failure:** the screen block (`--- screen ---`) goes to stdout, and the error names the script line. Headless,
  `ue2emu` exits 1. Over TCP the client gets the screen lines, then `error line <n>: …`. A real run of
  `smoke-menu.ctl` with a wrong text on line 7 printed:

  ```
  Error: script …/wrong.ctl

  Caused by:
      0: line 7
      1: expect "Flash   Flash Disk             Broken": not on the screen within 1500 ms emulated
  ```

- **Console:** the emulation thread copies the console (after `\r` removal) into `ControlHandle::console`
  (`control::ConsoleLog`, bounded to the last 1-2 MiB). `expect-console` searches from power-on, and each match
  moves its start past the matched text. So the same line has to appear twice to satisfy two `expect-console`
  lines, and a line printed before an earlier match does not count. Each script or TCP control server keeps its
  own mark.

## Input timing on the emulation thread

`button`, `key` and `type` used to press, sleep on the control thread until `now_ms` advanced, and release. At
`--speed max` the release then landed wherever host scheduling allowed. Now each command is one timed sequence:
`runner::Command::Inputs { seq: control::TimedInputs, done }`.
- **Queue:** the emulation thread queues the sequence in `control::InputTimeline`. It starts on arrival, or when
  the previous sequence ends if that is later.
- **Slicing:** `InputTimeline::slice_insns` shortens the `Machine::run` slice so it ends at the next event. Each
  event applies at its exact emulated millisecond, not at the next 4 ms slice boundary.
- **Completion:** `done` fires at the end of the sequence (after the release gap); the script then goes on.
- **Stop:** if the emulator stops first, the queued sequence is dropped and the command fails with
  `emulator stopped`.

An absolute `at_ms` computed on the control thread (the review's suggestion) was not used. That thread sees
`now_ms` late by host latency × emulation speed. A late schedule shortens a hold, down to zero when both edges
are already past. Anchoring the sequence on the emulation thread keeps both the start and the length exact.

Measured with scratch-only instrumentation that logged the emulated ms of every input the machine received:
- **Setup:** 6 + 6 parallel runs at `--speed max`, 10 busy loops on 10 CPUs.
- **Script:** `type abcdefghijklmnopqrstuvwxyz0123` into the Home Directory string box (userinterface.cc:130,
  config_menu.cc:114-124).

| Binary | Holds per run | Hold lengths (default 80 ms) |
|---|---|---|
| `main` @ d51fa61 | 46 | 80 ms ×25-33, 84 ms ×9-18, 88 ms ×2-4 |
| this change | 46 | 80 ms ×46 in every run |

Both typed the text correctly under this load, because 88 ms is still far below the ~340 ms first repeat
(keyboard_c64.cc:105-106). Nothing bounded the old stretch, though: a longer host stall at 8× speed turned into
a repeat or a long button press.

## `png` font

`ControlHandle::rom_dir` carries `MachineConfig::rom_dir`, the `--roms` value or its default. `png` reads
`chars.bin` there, and `$UE2_FIRMWARE` plays no part in it anymore. Tests that need firmware files still use
`$UE2_FIRMWARE`.

## Tests

`cargo test -p ue2emu` has 39 tests (8 new); `cargo test --workspace` passes 200 tests.
- **Parsing:** the new commands, quoting and bad input.
- **Execution (fake target):** expect passes, times out, and a timeout prints the screen and names the line;
  console marks.
- **`ConsoleLog`:** matches across pushes and after trimming.
- **`InputTimeline`:** back-to-back sequences at exact times, completion signals, and a disconnect when dropped;
  slice lengths.
- **`png`:** the font path comes from the handle.
- **TCP:** the protocol, with a failing expect and exact holds timed through a real `InputTimeline`.

## Known gaps

- **Window exit code:** a failing `--script` in window mode prints the error and stops the emulator, but the
  process exit code is still that of the window (`window.rs` is not part of this change).
- **Colours:** `text_dump` has no colours, reverse video or visibility. A selection is checked by entering it
  (`/Temp/`), and a hidden overlay still matches `expect`.
- **Firmware-specific text:** the scripts' texts come from the upstream `ultimate.elf` (V1.01 3.15). They were not
  run against the Commodore C64U 1.1.0 `.ue2`.
- **Other scripts:** `smoke-usb.ctl` and the `smoke-c64-*.ctl` scripts still use `wait` and screen dumps checked by
  `grep`, and `smoke-all.sh` does not run them. storage.md, boot.md and README were updated in the wave-3 merge.
