# S32 — USB mouse

**Status:** built (2026-09-24) on TRX64 v0.9.2 (Spec 876). The POT conversion of the mouse position is assumed,
not measured (§4).

**Owns:**
- `crates/ue2-core/src/devices/usb/mouse.rs`: the HID mouse; `device.rs`, `mod.rs`: `Peripheral::Mouse`,
  `UsbConfig::mouse`, `Usb::mouse`
- `crates/ue2-core/src/host.rs`, `machine.rs`: `HostInput::UsbMouse`
- `crates/ue2emu/src/usb.rs` (`--usb-mouse`), `window.rs` (capture), `control.rs` (`usbmouse`), `monitor/devices.rs`
  (`joy`)
- `crates/c64-bridge/src/lib.rs` (`apply_pots`: core registers 0x32-0x37 → `Machine::set_pot`)

**Reads:** `software/io/usb/usb_hid.cc:870-1077, 1216-1483`, `usb_hid_selection.h:47-81`, `hid_decoder.h:311-336,
680-684, 909-928`, `io/c64/joystick_output.cc:26-105`, `system/u64.h:121-144`, `u64/u64_config.cc:355-360, 419`.

## 1. What the firmware does

Any HID interface whose report descriptor has relative X and Y under a Generic Desktop Mouse application is a mouse
(buttons 1-3 and a relative wheel optional). The firmware selects report protocol, sets idle 0 and polls the
interrupt endpoint. Each report moves an accumulated position (`mouse_x += x; mouse_y -= y`, scaled by "Mouse
Sensitivity", clamped to ±63 per report) and becomes:

- `C64_PADDLE_1_X/Y` (core + 0x32/0x33) = position & 0x7F;
- `C64_JOY1_SWOUT` (+0x30), active low: left button = fire (0x10), right = up (0x01), middle = down (0x02);
- `C64_MOUSE_EN_1` (+0x36) = 1 while a mouse is plugged in.

The mouse is always port 1. "Mouse Mode" Cursor (0) sends cursor keys instead; the default is Mouse (1).

## 2. The device

A full-speed HID boot mouse (subclass 1, protocol 2), VID 0x1209 PID 0x0003, interrupt IN 0x81 of 4 bytes. The report
descriptor is HID 1.11 E.10 with a relative wheel after Y: buttons, X, Y, wheel. Motion accumulates between polls and
goes out in steps of at most ±127; a report only when something changed (idle 0).

## 3. The host

- `--usb-mouse` puts the mouse on the next free hub port (after the images, storage slots and the keyboard).
- The window: a left click captures the host mouse (cursor locked, or confined where locking is not available, and
  hidden); raw motion (`DeviceEvent::MouseMotion`), the buttons and the wheel then go to the USB mouse. PageDown or
  losing focus lets it go, with the buttons released. The title says which.
- `usbmouse <dx> <dy> [buttons]` on the control port (bit 0 left, 1 right, 2 middle).
- `monitor joy` shows both ports as the firmware drives them.

## 4. The C64 side

TRX64 Spec 876: `Machine::set_pot(port, x, y)`, the SID's POTX/POTY answered from the port CIA1 PA6/PA7 selects,
latched every 512 cycles, $FF when nothing is set. The bridge latches PADDLE_n_X/Y and MOUSE_EN_n and hands each
port's pair over: with the mouse enable as `position << 1`, without it as written (the firmware's extra fire buttons
write 0x80 released, 0x00 pressed, joystick_output.cc:26-37).

**To measure on a C64U with a USB mouse:** how the 7-bit position becomes the POT byte. A real 1351 puts its
position in bits 1-6 (even values, bit 0 noise); if the U64 does the same, the byte is `position << 1`. Until then the
bridge assumes that, since GEOS's 1351 driver works on the C64U. The C64 program:
`10 POKE 56322,192:POKE 56320,64` / `20 PRINT PEEK(54297);PEEK(54298):GOTO 20`, then the mouse moved slowly.

## 5. Checks

- Unit: reports carry motion in steps of at most 127, a button change is a report, no report when nothing changed;
  the descriptor is a relative mouse.
- Firmware: `--usb-mouse` enumerates as "UE2EMU USB Mouse"; `usbmouse 20 0` twice moves port 1's paddle x to 0x28
  (sensitivity 8), `usbmouse 0 0 1` presses fire on port 1, `monitor joy` shows `mouse on`.
- Bridge unit test: paddle bytes as written, mouse position shifted, port 2 without a mouse enable.
- Firmware run: `usbmouse 20 0`, `usbmouse 20 -10`, then `PRINT PEEK(54297);PEEK(54298)` on the C64 prints `80  20`
  (0 0 before).
- Not run yet: GEOS with its 1351 driver.

## 6. Not in this spec

"Mouse Mode" Cursor and Mouse + Wheel are the firmware's; the menu mouse navigation works through the same device.
