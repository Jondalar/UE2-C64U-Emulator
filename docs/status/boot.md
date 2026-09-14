# Boot status: M1, M2, M3

State of `main` after wave 1 (S01-S09); §Known gaps notes what later waves changed. The unmodified `ultimate.elf` boots to the FreeRTOS idle loop, runs all
InitFunctions, shows the overlay menu and reacts to keys. Milestone definitions: `docs/specs/S10-integration.md`.

## Reproduce

Run from the repo root, after `scripts/build-firmware.sh` (only if the ELF is missing) and `cargo build --release`.
Each command starts from a clean `run/` directory (gitignored). Firmware paths default to `firmware/1541ultimate`
under the repo root. From a worktree, pass `--elf <fw>/target/u64ii/riscv/ultimate/result/ultimate.elf --roms <fw>/roms`
and export `UE2_FIRMWARE=<fw>` for the tests. `png` reads its font (`chars.bin`) from `--roms`.

| M | Command | Wall time | Result |
|---|---|---|---|
| M1 | `rm -rf run && mkdir run && target/release/ue2emu run --headless --speed max --max-seconds 20 --log unmapped > run/console.txt` | 20 s | banner `*** FPGA Capabilities: 34000222 ***`, no halt |
| M2 | `rm -rf run && mkdir run && printf 'wait 60000\nquit\n' > run/m2.ctl && target/release/ue2emu run --headless --speed max --log unmapped --script run/m2.ctl > run/console.txt` | 7 s | 60.012 s emulated, no halt, PC in `prvIdleTask`, 0 unmapped |
| M3 | `rm -rf run && mkdir run && target/release/ue2emu run --headless --speed max --flash run/flash.bin --script scripts/smoke-menu.ctl > run/m3.txt` | 1 s | menu text in the dump, `run/menu1.png` and `run/menu2.png` show the cursor on SD and on Temp; the script checks itself with `expect` and exits non-zero on a mismatch |

The firmware console goes to stdout; the stats line and the unmapped summary go to stderr. M3 also works without
`--flash` (the overlay-UI config seed goes into a blank in-memory flash). The window build starts the same way:
`target/release/ue2emu run --flash run/flash.bin` (realtime, F12 = menu button).

## Stats lines (stderr)

```
M1: 4144566940 instructions, 165.784 s emulated, 33060 IRQs taken, pc prvIdleTask+0x2c
    unmapped summary: 0 addresses
    203 MIPS (last interval)
M2: 1500288096 instructions, 60.012 s emulated, 11904 IRQs taken, pc prvIdleTask+0x3c
    unmapped summary: 0 addresses
    209 MIPS (last interval)
M3: 144598942 instructions, 5.784 s emulated, 1058 IRQs taken, pc prvIdleTask+0x44
    163 MIPS (last interval)
```

11904 IRQs in 60 s is close to the 200 Hz FreeRTOS tick (12 000 in 60 s). At the default 4 clocks per instruction
the emulator needs 25 MIPS for realtime, so `--speed max` runs about 8× faster than hardware.

## Boot log excerpt (M1, trimmed)

M1 and M2 print the same 141 console lines; only the RTC date line differs. `[…]` marks cuts.

```
No ACIA found in the FPGA.
-- Custom Hardware Init --
NAU8822 initialization...
NAU8822 initialization complete.
Configuring USB2513.
USB Hub successfully configured.
-- Start Scheduler --
*** Ultimate 64-II (V1.01) 3.15 ***
*** FPGA Capabilities: 34000222 ***

Executing init functions.
----> Initializing SID Cart (0)...
----> Initializing Boot Cart (0)...
----> Initializing U64 Config (1)...
ConfigManager opened flash: 0095DCD4
[…]
*** U64 Configurator Done
----> Initializing RAM Disk (1)...
----> Initializing U64 Palette (9)...
----> Initializing SoftIEC Drive (11)...
IEC Processor found: Version = 25. Loading code...
[…]
----> Initializing LwIP Networking (50)...
----> Initializing RMII Interface (51)...
----> Initializing WiFi Application (52)...
[…]
NIdentify: ESP32 WiFi Bridge V1.14
NResult get voltages: 0 12000 12000 5000 3300 1800 1000 5000
[…]
----> Initializing C1541/71/81 Init (65)...
[…]
----> Initializing Telnet Server (100)...
Starting Telnet Server
Telnet server starting
----> Initializing FTP Daemon (101)...
FTP server starting
----> Initializing Raw Socket 64 (102)...
[…]
----> Initializing HTTP Daemon (103)...
[…]
----> Initializing Modem (105)...
---> All Init functions called.
[…]
No USB2 hardware found. (34000222)
State *Root* reloaded. # of children = 5
All linked modules have been initialized and are now running.
[task list, below]

EDID Header incorrect.
```

Since W3-FIXES the last line is gone: an EDID EEPROM answers on I2C channel 0 and the firmware configures HDMI
output (`docs/status/fixes.md` §5).

The `N` prefixes are the raw `N` characters the WiFi reply ISR writes to the UART (`wifi_cmd.cc:23`, doc 02
§Console UART). The lines interleave because tasks print concurrently.

## Task list (printed by the firmware at the end of init)

Columns: name, state (X running, R ready, B blocked), priority, stack high-water mark, task number.

```
U-II Main      	X	0	962	2
IDLE           	R	0	1558	3
Tmr Svc        	B	3	1546	4
tcpip_thread   	B	2	1970	9
HPD Monitor    	B	1	1546	5
SD Card Manager	B	0	1558	1
LedStrip Contro	B	3	1531	11
Socket Gui List	B	1	1466	13
FTP Listener   	B	1	1474	14
DMA Load Task  	B	1	1482	15
UDP Ident Task 	B	1	1362	16
Drive A        	B	1	1546	12
HTTP Listener  	B	1	1474	17
IEC Server     	B	1	1530	7
Virtual Printer	B	0	1538	8
WiFi Command Ta	B	1	1474	10
U64 Reset Task 	B	3	1534	6
```

## M3 screen dumps

`screen` before `button` prints the overlay RAM while the overlay is still hidden (`text_dump` ignores
visibility, doc 05 T1): the title and an empty browser. After `button`:

```
  *** Ultimate 64-II (V1.01) 3.15 ***
----------------------------------------
SD      SD Card                No media
Flash   Flash Disk             Ready
Temp    RAM Disk               Ready
Ftp     Remote FTP Servers     Ready
WiFi    MAC 02:15:41:00:00:01  Link Down
[17 empty rows]
/                              -F3=HELP-
```

The dump after two `key down` is identical, because the selection is colour-only (doc 05 T1). The PNGs show the
highlight bar on `SD` (`menu1.png`) and on `Temp` (`menu2.png`).

## Unmapped summary

`--log unmapped` reports **0 addresses** for both M1 (166 s emulated) and M2 (60 s emulated). Every IO access on
the boot path and in the idle system hits a modelled window.

## Known gaps

Open questions from `docs/hw` that still affect what the boot shows:

- **Capability word** `0x34000222` is the T0 choice, not a measured value (00 Q-B1). USB (bit 23) is set only with
  `--usb`/`--usb-keyboard` and RMII (bit 24) only with `--net`; without them the log says "No USB2 hardware found"
  and there is no Ethernet (`docs/status/usb.md`, `docs/status/network.md`).
- **Closed U64-II top level.** The CPU instance, IP identity and address aliasing come from the open U2+ RTL (00
  Q-A1, Q-A2, Q-A4). BOARDREV 0xB8 (rev 0x17) and ITU `g_version` 0x25 are assumptions (Q-B2, Q-B3). The flash part
  (S25FL128L) is an assumption too (Q-B5).
- **I2C:** the EDID EEPROM on channel 0 is modelled (1080p60 HDMI monitor), so "EDID Header incorrect." no longer
  appears (`docs/status/fixes.md` §5). The codec, hub, expanders and PLLs still ACK every byte and read 0xFF.
- **WiFi** is a frame-level u64ctrl stub (doc 04 T0): identify, voltages and power settings answer; scan returns
  no APs; the link stays down.
- **C64 core:** TRX64 by default since S14 phase A (`docs/status/c64.md`); `--c64 none` is the register stub of doc 10
  T0. With TRX64 the console differs from the stub in the RTC date, the cart register dump of `set_emulation_flags`
  and one stack high-water mark, and stderr notes that the core is PAL-only. SID detection reads zeros in both
  modes.
- **SD card:** no image by default ("No media"); `--sd <image>` attaches one (S09).
- **Overlay rendering** follows the open chargen IP; palette path, `pixel_opaque`, X_ON/Y_ON origin and big-font
  height are open (05 Q2-Q5).
- **Time mapping** is a fixed 4 clocks per instruction with no idle fast-forward (00 Q-D1).
- **Debugging:** the GDB stub and the CPU trace ring exist since S11 (`docs/ARCHITECTURE.md` §Debugging).
