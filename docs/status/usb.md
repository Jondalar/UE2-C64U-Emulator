# S13 status — USB: mass storage and HID keyboard

Spec: `docs/specs/S11-S14-later.md` §S13. Hardware: `docs/hw/09-usb.md` tier T1b (hub root). **Reached.** The
unmodified firmware (`ultimate.elf`, V1.01 3.15) enumerates the USB2513 hub, a mass-storage device and a HID
keyboard behind it, lists the stick as `USB0` in the file browser, reads and writes files on it, and takes menu
input from the USB keyboard.

## Commands

From the repo root (or a worktree, where the firmware lives outside the checkout):

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate
FW="--firmware $UE2_FIRMWARE/target/u64ii/riscv/ultimate/result/ultimate.elf --roms $UE2_FIRMWARE/roms"
cargo build --release
rm -rf run
scripts/make-sd-image.sh run/usb.img 48          # the SD image script makes USB sticks too
target/release/ue2emu run --headless --speed max $FW --flash run/flash.bin --usb run/usb.img --usb-keyboard \
    --script scripts/smoke-usb.ctl > run/smoke-usb.log
grep -q '^USB0    UE2EMU   USB Disk Imag Ready' run/smoke-usb.log &&
grep -q '^demo.d64                      D64  171K' run/smoke-usb.log &&
grep -q '^hello.prg                     PRG   29' run/smoke-usb.log &&
grep -q '^readme.txt                    TXT   71' run/smoke-usb.log &&
grep -q '^HELLO                         PRG  254' run/smoke-usb.log && echo S13 PASS
```

- `--usb <image>`: a raw image as a USB mass-storage device. Repeatable; images take hub ports 1.. in order.
- `--usb-keyboard`: a HID keyboard on the next free port. In the window, host keys go to it instead of the C64
  matrix (F12 stays the menu button, Page Up the C64 RESTORE key). Scripts use `usbkey <name> [ms]` (`crates/ue2emu/src/usb.rs` lists the
  names: `a`, `return`, `f10`, `down`, `lshift`, …).
- `--usb-dir <path>[,size=SIZE][,ro]`: a host directory as a stick, with the guest's changes written back
  (`docs/status/usb-dir.md`). Repeatable; on the ports after the images, before the keyboard.
- `usb-replug [port]` unplugs a device and plugs it back in (hot-plug through the hub, `docs/status/usb-dir.md`
  §Hot-plug); `usb-sync` is for `--usb-dir` sticks.
- More than 3 devices is an error (the USB2513 has 3 ports). With any device the capability word gets
  `CAPAB_USB_HOST2` (bit 23); without `--usb`/`--usb-dir`/`--usb-keyboard` it stays `0x34000226` (`0x34000222` before S27 added drive B's bit).

## Results

### Smoke (`scripts/smoke-usb.ctl`, 11.9 s emulated, about 2 s wall)

```
USB0    UE2EMU   USB Disk Imag Ready          <- root, 4th entry after SD, Flash, Temp
demo.d64                      D64  171K       <- /USB0/, entered with the matrix keys
hello.prg                     PRG   29
readme.txt                    TXT   71
UE2EMU DEMO       UE 2A       VOLUME          <- /USB0/demo.d64/, entered with `usbkey right`
HELLO                         PRG  254
```

The stick is `Ready` about 5 s after power-on (checked with a `screen` every second from 5 s on). Enumeration as
the firmware logs it:

```
USB Other IRQ. Status = 01
Attach root!!
Reset status: 03 Speed: 02
Vendor/Product: 0424 2513
** USB HUB Found! **
Found a hub with 3 ports.
HUB 1 IRQ data: 02
Port 1 of hub on addr 1 status:0501 0001  Port in reset: 0
Issuing reset on port 1.
HUB 1 IRQ data: 02
Port 1 of hub on addr 1 status:0503 0010  Port in reset: 1
Port reset to high speed
Installing USB0 Parent = 0031C5F0, ParentPort = 0
Vendor/Product: 1209 0001
Interface descriptor #00:00, with 2 endpoints. Class = 8:6:80
Device: UE2EMU  USB Disk Image  1.00 (Removable)
Path Dev 0031CA4C 0031CCE0. Current lun 0. BS = 512. CAP = 98304
MBR Start: 2048 Size: 96256 Type: 12            <- on entering /USB0/
```

With `--usb-keyboard` the keyboard follows on port 2 (`Port reset to full speed`, `Installing USB1`,
`Vendor/Product: 1209 0002`, `Class = 3:1:1`, `Interface has a HID descriptor with length 63!`).

### Writes

Checked with a throw-away copy of the image (`run/usbw.img`), both devices attached:
- **Steps:** in `/USB0/`, F5 → Create → D64 Image; the name `usb` typed with `usbkey u`, `usbkey s`, `usbkey b`,
  `usbkey return`.
- **Firmware:** `Result of save: 0.`, `State USB0 reloaded. # of children = 4`, and `/USB0/` lists
  `usb.d64 D64 171K`.
- **Host:** `fsck_msdos -n` on the partition reports no errors and 4 files; the mounted `usb.d64` has the BAM disk
  name `USB`.

### USB keyboard

`usbkey f10` opens the overlay menu (the main loop's USB `getch`, ultimate.cc:172-176); `usbkey down` ×3 and
`usbkey right` enter `/USB0/`; the typed disk name above arrives through `Keyboard_USB` (usb_hid.cc:1190-1198,
keyboard_c64.cc:325-326).

### Stability and regressions

- 60 s emulated with `--usb run/usb.img --usb-keyboard --log unmapped`: no halt, no USB error lines,
  `unmapped summary: 0 addresses`, 190 MIPS.
- Without USB flags the boot log still says `No USB2 hardware found. (34000222)` with 0 unmapped addresses.
- `scripts/smoke-sd.ctl` with `--sd` and `--usb` together lists both media; the SD screens are unchanged.
- `cargo test --workspace` passes, including the USB model tests below.
- After the wave-3 merge (default `--c64 trx64`, `usbkey` now a timed input sequence like `key`, docs/status/tooling.md):
  the smoke run above passes its grep block, 11.832 s emulated.

## Model (`crates/ue2-core/src/devices/usb/`)

| File | Content |
|---|---|
| `mod.rs` | The window: BRAM + run register; nano HLE link states (attach 700 ms after START, 10 ms bus reset to high speed), pipe scheduler on the 8 kHz frame counter, FIFO + ITU bit 2, DMA; `install`, `UsbConfig`, `CAPAB_USB_HOST2` |
| `device.rs` | Transactions (`Reply`), the standard control endpoint (descriptors, SET_ADDRESS after the status stage, SET_CONFIGURATION), device tree routing by address through enabled hub ports |
| `hub.rs` | USB2513 root: hub descriptor, port power/reset/clear features, 4-byte port status, 1-byte change bitmap (NAK when nothing changed); per-port plug state for hot-plug |
| `storage.rs` | Bulk-Only Transport; INQUIRY, REQUEST SENSE, TEST UNIT READY, READ CAPACITY(10), READ(10), WRITE(10); sense codes; read-only media are write protected |
| `block.rs` | `BlockBackend` (the medium: blocks, read, write, read-only) and `ImageFile`, the raw image of `--usb` |
| `keyboard.rs` | HID boot keyboard with the HID 1.11 report descriptor; report queue so short taps survive the 20 ms poll; SET_IDLE/GET_IDLE honoured |

Unit tests drive the model the way `UsbBase` does (`control_exchange`, `bulk_in`/`bulk_out`, autopipes, the
ISR's FIFO drain): hub and storage enumeration with SCSI I/O against a temp image, keyboard reports on an
interrupt pipe, NAK timeout and ABORT_REQ, FIFO back-pressure, and the install/port assignment.

One hazard was found in the real run and added to `docs/hw/09-usb.md` as **H16**: the hub driver resumes its status
pipe and then clears `irq_data[0]` (usb_hub.cc:406-407). A pipe answered before the next instructions loses its
bitmap and, by H13, is never resumed again; enumeration stopped after `Issuing reset on port 1.`. The scheduler
now starts a pipe no earlier than one frame after the write that armed it (`SCAN_LATENCY`).

## Known gaps

- **Hot-plug on hub ports only:** devices behind the hub can be unplugged and plugged in (`usb-replug`,
  `HostInput::UsbPlug`, `docs/status/usb-dir.md`). The root port never detaches, so the nano's own disconnect path
  (`RAM_STATUS = 0x8000`, push 0xFFF0) is still not modelled. Do not modify a `--usb` image on the host while the
  emulator runs (`--usb-dir` does that safely).
- **Wire details not modelled:** split transactions to the full-speed keyboard, PING, suspend/resume, error
  retries. Every device answers at once and always with the toggle the pipe expects.
- **No mouse.** HID mouse and other classes (CBI, AX88772) are not modelled.
- **Mass storage:** one LUN, 512-byte blocks, 32-bit LBAs (images above 2 TiB are cut); commands other than the six
  the driver sends fail with ILLEGAL REQUEST.
- **Board wiring open (09 Q5):** 3 hub ports; images get the first ports, the keyboard the next free one, so names
  are `USB0`, `USB1`, `USB2` in that order.
- **Keyboard into the C64:** the firmware also writes the USB keyboard state to MATRIX_KEYB (keyboard_usb.cc:214-229).
  Since S14 `C64Port` forwards MATRIX_KEYB to TRX64 (`docs/status/c64.md`); typing into BASIC from the USB keyboard
  was not tried.
- **Idle reports:** the keyboard accepts the driver's 100 ms idle rate and repeats its report that often, so the
  USB input task never prints its 25 s `@` heartbeat.
- **Window not checked by hand:** `--usb-keyboard` in the window uses the same `HostInput::UsbKey` path as
  `usbkey`, but typing in the window was not tried.
