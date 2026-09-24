# S36 — Physical joysticks on both control ports

**Status:** built (2026-09-24).

**Owns:**
- `crates/ue2-core/src/host.rs` (`HostInput::JoystickPort`), `machine.rs` (`Machine::input`)
- `crates/ue2-core/src/devices/c64.rs` (`C64Port::set_joystick`, `joy_lines`, `apply_joysticks`)
- `crates/ue2-core/src/devices/u64io.rs` (`U64II_KEYB_JOY`), `devices/mod.rs` (`install_all` shares the lines)
- `crates/ue2emu/src/control.rs` (`joy`, `joy-hold`, `joy-release`)
- `scripts/smoke-joy.ctl`, `scripts/smoke-joy-swap.ctl`, `scripts/smoke-joy-swap.cfg`

**Reads:** `u64.h:71`, `u64_config.cc:1060-1066`, `keyboard_c64.cc:204-227` (and the SPEC-07 branch of the firmware,
`keyboard_c64.cc:285-305`), `joystick_output.cc`.

## 1. What it does

- `HostInput::JoystickPort { port, lines }` sets the physical stick on port 1 or 2: five active-low lines, bit 0 up,
  1 down, 2 left, 3 right, 4 fire. `HostInput::Joystick(lines)` stays and means port 2.
- The C64 sees each port as the wired AND of the physical stick and the firmware's `C64_JOY1/2_SWOUT`. `C64Port`
  computes both ports on every stick change and every SWOUT write, hands them to TRX64 and keeps them in a cell it
  shares with `U64Io`.
- `U64II_KEYB_JOY` (`0x10100406`): a write latches the select, bit 0: 0 port 2, 1 port 1. A read returns the five
  lines of the selected port as the C64 sees them, bits 5-7 high. The firmware writes `swap & 1` from the
  joystick swapper setting (`u64_config.cc:1064`). The read includes the software output because the menu scan
  compares it with the REST joystick it injected to tell a physical stick from its own (`keyboard_c64.cc:220`).
- The firmware branch for SPEC-07 flips the select to the other port while the selected one is idle and back; the
  model serves that as it is, a select write takes effect at the next read.
- The C64's ports are not swapped by the select (`docs/status/gaps.md`).

## 2. Control commands (S08)

- `joy <port> <dirs> [ms]`: hold, default 80 ms, then release and the 40 ms release gap. `<dirs>` is `up`, `down`,
  `left`, `right`, `fire` joined by `+`.
- `joy <port> none` and `joy-release <port>`: release the port.
- `joy-hold <port> <dirs>`: hold exactly these directions until the next `joy*` for the port.

## 3. Checks

- Unit: the register select and read (`u64io.rs`), the wired AND and the shared cell (`c64.rs`), routing of both
  inputs and the register through `install_all` (`machine.rs`), parser and timing (`control.rs`).
- `scripts/smoke-joy.ctl` (in `smoke-c64-all.sh`): BASIC waits for `PEEK(56321)` and `PEEK(56320)`; `joy-hold 1
  up+fire` prints 14, `joy-hold 2 down+fire` prints 13. Then `joy 2 down` twice and `joy 2 right` enter Temp in the
  menu.
- `scripts/smoke-joy-swap.ctl` with the swapper on: the same menu walk with port 1.
- The upstream firmware in `firmware/1541ultimate` reads only the selected port; port 1 without the swapper does not
  move the menu there (checked, the walk times out). The SPEC-07 flip needs a firmware build that has it.
- Not covered: joystick keys in the window.
