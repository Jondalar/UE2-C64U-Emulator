# S33 — USB devices plugged in and out while the machine runs

**Status:** built (2026-09-24).

**Owns:**
- `crates/ue2-core/src/devices/usb/mod.rs` (`Usb::plug`, `Usb::unplug`, `UsbDevice`), `hub.rs` (`take`,
  `insert_unplugged`); `machine.rs` (`usb_plug`, `usb_unplug`)
- `crates/ue2emu/src/control.rs` (`usb-plug`, `usb-unplug`), `runner.rs` (`Command::UsbPlug`), `usb.rs` (`--usb-hub`)

## 1. What it does

- `usb-unplug <port>` unplugs the device on a hub port and takes it away: the firmware sees the disconnect, and
  the port is empty.
- `usb-plug <port> image <path> | keyboard | mouse` puts a new device on an empty port and plugs it in, after the
  gap and the firmware's acknowledgement the hot-plug path already waits for (`REPLUG_GAP`,
  `REPLUG_ACK_TIMEOUT`). The device is added unplugged first, so the firmware sees exactly one connection change.
- `--usb-dir` sticks keep their own path: `usb-replug` unplugs and plugs one with its host sync, and `usb-plug` /
  `usb-unplug` refuse their ports.
- `--usb-hub` announces the USB host with no device on it (`CAPAB_USB_HOST2`), so a machine started without USB
  devices can still take one later. Without any USB option the firmware never starts its USB stack.

The window routes keys and the mouse by the start options (`--usb-keyboard`, `--usb-mouse`); a keyboard or mouse
plugged in later is for control scripts (`usbkey`, `usbmouse`).

## 2. Checks

- Unit: a plugged device waits for its plug-in; a busy port, a missing image and an empty port are refused.
- `--usb-hub`, a firmware boot with no devices: `usb-plug 1 mouse` and `usb-plug 2 image run/stick.img` install
  "UE2EMU USB Mouse" and "UE2EMU USB Disk Image"; `usb-unplug 2` disconnects the stick; `usb-plug 2 keyboard`
  installs "UE2EMU USB Keyboard"; `usb-unplug 1` of an empty port fails.
