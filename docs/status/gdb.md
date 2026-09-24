# GDB on the firmware's RISC-V

The emulator serves the GDB remote protocol for the RISC-V the Ultimate's application runs on, so the firmware can
be debugged with symbols, breakpoints and a task list. It is the `gdbstub` crate with `gdbstub_arch::riscv::Riscv32`
as the architecture, in `crates/ue2emu/src/gdb.rs`, and it has been there since S11
(`docs/specs/S11-S14-later.md`).

There is no GDB for the C64 side: GDB has no 6502 target upstream. The C64 core is served by the VICE binary monitor
protocol instead (`--vice-monitor`, `docs/status/monitor.md`, S23 §8), which is what the C64 debuggers speak.

## 1. Turning it on

```
ue2emu run --firmware firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf --gdb 127.0.0.1:1234
```

The machine waits at reset — at the ELF entry, with no instruction executed — until a debugger connects and
continues. A port probe does not release that hold; only a resume or a detach does. A debugger that attaches later,
while the machine runs, stops it where it is.

`--gdb` turns the CPU trace ring on (`--trace`, the last 256 PCs, about 3 % of host MIPS) and, because breakpoints
and the trace ring both disable it, idle skipping stays off while a debugger is attached (S19 §3).

## 2. Attaching

```
riscv32-unknown-elf-gdb firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf
(gdb) target remote 127.0.0.1:1234
(gdb) break ultimate_main
(gdb) continue
```

The stub answers `qXfer:features:read` with the rv32i target description, so the architecture needs no `set
architecture`. Symbols come from the same ELF the emulator loads; `.app` and `.ue2` images carry none, and a
symbol-less image is worth debugging only at the address level.

## 3. What the stub answers

| Area | Behaviour |
|---|---|
| Registers | x0-x31 and pc. A write keeps x0 = 0 and clears pc bits 1:0, as every rvlite jump target does (`fetch.vhd:58`). No CSRs — they are not in the target description. |
| Memory reads | Decoded like the data bus (`ue2_core::bus`): DDR and its mirrors out of RAM, IO bytes through `IoDevice::peek8`, so a read has no side effects and `x/` on an ITU or U64 register is safe. The boot BRAM page and unmapped addresses read 0. |
| Memory writes | DDR only. A write that touches any other address fails with `EFAULT` and changes nothing. |
| Breakpoints | Software breakpoints are `Machine::breakpoints`, checked before the instruction; RAM is never patched, so a breakpoint in the boot BRAM or in flash-resident code works like any other. No hardware breakpoints and no watchpoints. |
| Execution | `step` is `Machine::run(1)`, so a pending interrupt is taken as the step (`docs/hw/01` H6). `continue` runs runner slices until a breakpoint, a fault hook or Ctrl-C. Signals the debugger sends are ignored. |
| Faults | A firmware fault hook stops the debugger with SIGABRT and prints the halt message and the trace ring, instead of ending the run. |
| Monitor | `monitor tasks` lists the FreeRTOS tasks — TCB, priority, state and name, read off `pxCurrentTCB` and the kernel's state lists. `monitor trace` prints the CPU trace ring. Anything else prints the help line. |
| End | `detach` lets the machine run on. `kill` ends the emulator. A connection that closes without detaching leaves the machine running if a debugger had resumed it, and stopped if none had. |

The stub runs on the emulation thread, between `Machine::run` slices, so the runner keeps applying control commands
and publishing the display while the debugger holds the machine — the overlay stays on screen, the control port
still answers, and REST requests wait (`docs/status/network.md`).

## 4. Not covered

- **Watchpoints and hardware breakpoints** (`Z2`/`Z3`/`Z4`). Watching an address means using the monitor.
- **Threads.** The stub is single-threaded, so `info threads` shows one thread. The FreeRTOS tasks are visible
  through `monitor tasks` but cannot be selected, and there is no per-task backtrace.
- **Writes outside DDR**, so `set var` on an IO register fails.
- **CSRs.**
- **The C64.** See the VICE port.

## 5. Its relation to the monitor

S23's monitor holds the firmware through the same machinery: `fw halt`, `fw step` and `fw go` on the control port
are a second door onto the hold and the breakpoint set the stub uses, and the run loop asks for the held state
before every slice, so a held machine still serves commands. Stopping the firmware stops the C64 with it, because
the C64 only advances while the bridge drives it (S23 §3).

## 6. How it is verified

`crates/ue2emu/src/gdb.rs` carries the tests, three of which drive the protocol itself over a socket: a session that
sets a breakpoint, steps, interrupts, runs a monitor command and kills; the reset hold until a debugger continues;
and a detach followed by a second debugger that stops the machine where it is. The rest cover the memory and
register rules and the task-list walk.

No RISC-V GDB is installed on the development machine, so those in-repo RSP sessions — not a real client — are what
keeps the stub honest. LLDB is not supported: it speaks the same protocol but expects its own extensions.
