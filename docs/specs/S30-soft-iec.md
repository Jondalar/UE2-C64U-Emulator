# S30 — Software IEC: the IEC processor on the C64's bus

**Status:** the engine is built (`crates/c64-bridge/src/iec_proc.rs`). Attaching it to the bus waits for TRX64 Spec 874
(a generic host IEC device, accepted 2026-09-23, not yet written).

**Owns:**
- `crates/c64-bridge/src/iec_proc.rs`: the engine, its code RAM and both FIFOs
- next: the `C64Port` window for 0x10028000, the clocking in the bridge, the TRX64 attachment; `devices/iec.rs`'s
  T0 table goes
- `docs/status/drive.md`, `docs/status/gaps.md`

**Reads:** `fpga/io/iec_interface/vhdl_source/iec_processor.vhd`, `iec_processor_io.vhd`,
`software/io/iec/iec_interface.cc`, `iec_interface.h`, `iec_drive.cc:22-36,163-188`, `iec_code.iec`.

## 1. What the device has

- **The engine** (`iec_processor.vhd`): a 30-bit instruction word (invert, select, opcode, 12-bit operand, mask/value
  or data byte), 13 opcodes, a 12-bit timer on a 1 MHz tick, a 15-deep return stack, open-collector drivers for CLK,
  DATA, ATN and SRQ. Three system clocks per instruction at 100 MHz. A falling ATN while IRQ_EN is set flushes the down
  FIFO and jumps to address 1.
- **The shell** (`iec_processor_io.vhd`, 0x10028000): VERSION 0x25; the down FIFO (15 entries, firmware → engine,
  written at +8 data, +9 control, +A EOI); the up FIFO (2048 entries, engine → firmware, read at +6/+7, bit 8 control);
  RESET_ENABLE at +3; TX_FIFO_RELEASE at +D; the code RAM, write-only, from +0x800.
- **The program** is the firmware's (`iec_code.iec`, assembled by `tools/parse_iec.py`, linked into the ELF). The
  firmware uploads it at boot and patches the device numbers of its three slots into it (`set_slot_devnum`). UE2
  models no IEC protocol at all: listen, talk, EOI, JiffyDOS and the error paths come with the program.

## 2. The engine in UE2

`IecProc` is the VHDL process clock for clock. `run_us` runs whole microseconds: 100 system clocks each, the tick on
the first; a clock that changes nothing (a POP on an empty FIFO, a PUSH on a full one, a WAIT whose condition is false)
ends the microsecond early. The lines are sampled from the bus once per microsecond and again whenever the engine's
own drivers change.

## 3. On the bus (open)

The engine has to take part in the wired AND of the IEC lines every C64 cycle, see ATN fall at its cycle and keep
running while the C64 CPU is held. TRX64 has no API for a host IEC device today. UE2 alone could drive IecCore's pub
fields (slots 4-7, forcing Conf3) and step the C64 one instruction at a time during transfers; that leaves a line up
to about 7 cycles late, which standard IEC tolerates and JiffyDOS may not. Asked of TRX64 instead: a host IEC device
clocked at the points the drives are (`pair_catch_up`), folded into the bus like a drive, with ATN edges delivered.

TRX64 takes it as Spec 874: `Machine::attach_iec_device`, a device caught up to the exact cycle of every `$DD00`
access and every instruction end (also under `Hold::Cpu` and `Hold::Reset`), folded into the wired AND like a drive,
ATN edges at their cycle. 873's folder device sits in the same slot already. When 874 exists:
- `C64Port` serves 0x10028000-0x10028FFF and forwards to `C64Backend` (as drives and UCI do); the T0 table goes.
- The bridge runs the engine to the C64 clock in microseconds (`cpu_hz`, PAL or NTSC) at every catch-up point.

## 4. Checks

- Built: registers and FIFOs; LOAD/PUSH/SUB/RET/WAIT/COPY_BIT/IF; a blocking POP; the firmware's program answering
  ATN (DATA pulled, CTRL_ATN_BEGIN) and taking `LISTEN 11` under ATN (ATN begin, slot 1 addressed, ATN end) while
  `LISTEN 9` releases the bus.
- Once attached: "IEC Drive" on in the menu, `LOAD"$",11` lists the SD card's directory, `LOAD"file",11` and `SAVE`.

## 5. Expected behaviour

With "IEC Drive" on, a program that holds ATN low without sending a byte hangs the bus: `atn_irq_vec` pulls DATA and
waits for CLK "possibly forever" (`iec_code.iec`). Maniac Mansion's drive-8 fastloader and Green Beret do that. It is
the device's own behaviour and comes with the microcode; the setting is off by default.

## 6. Not in this spec

- The printer, UltiCopy and the drive-code upload (master mode: the engine drives ATN).
- The IRQ line (never enabled by the firmware).
