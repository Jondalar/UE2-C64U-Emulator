# S19 — Idle skip: the RISC-V CPU fast-forwards loops that cannot change anything

**Status:** built (2026-09-17).

The firmware has no WFI (00 Q-D1). When nothing is to be done, FreeRTOS spins in `prvIdleTask`, and UE2 executes those
instructions one by one: 25 million a second in realtime. In UltimateDemo2026 at TRX64 `a6e0465` the part of the
emulation thread outside the C64 is 12-16 %, while the firmware touches a C64 window 0.7 times a second and writes
nothing the C64 uses (measured 2026-09-17, `docs/status/xander-tests.md` §2).

S19 skips such loops without changing what the emulation does: after a skip, time, registers, RAM and every device
are exactly where they would have been had the instructions run.

## 1. The loop

`prvIdleTask` (FreeRTOS/Source/tasks.c:3346-3382) with the firmware's configuration (`FreeRTOSConfig.h`: preemption
1, idle hook 0, tickless 0, idle should yield 1). The upstream ELF compiles its loop to seven instructions
(`prvIdleTask+0x2c`..`+0x44`):

```
35da8  addi s2,gp,-2028      # &uxDeletedTasksWaitingCleanUp
35dac  lw   a5,0(s2)
35db0  bnez a5,cleanup
35db4  addi a5,s4,-4         # &pxReadyTasksLists[0]
35db8  lw   a4,0(a5)
35dbc  li   a5,1
35dc0  bgeu a5,a4,35da8      # back to the head unless another idle-priority task is ready
```

It reads two words and writes nothing. Until something outside the CPU changes those words or raises an interrupt,
every pass is the same pass.

## 2. What can change the loop's world

- **An interrupt:** only when the ITU line rises, which happens at a device tick or an IO access.
- **RAM:** written by the CPU (not in this loop), by a device in its tick (DMA: RMII, WiFi UART, C64 cartridge), or by
  the host between run slices (USB, network pump, control commands).
- **Device state:** changes only at a tick (`Machine::next_deadline`), an IO access, or between run slices.

So a loop that has reached a **fixed point** — same registers, same CSRs, no RAM write, no IO access and no interrupt
since the last pass — repeats unchanged until the next deadline or the end of the run slice.

## 3. Detection, without symbols

Symbols are not needed: the Commodore C64U images (`.ue2`) have none, and the rule in §2 holds for any loop.

- **Head:** the target of a taken backward branch or jump, i.e. `pc_after <= pc_before` after a step that took no
  interrupt (`j .` lands on itself).
- **One candidate slot:** head PC, the executed-instruction count, `x[0..32]`, the CSRs and the IRQ count at the
  last arrival, and the period in instructions.
- **Arrival at a different head:** the slot is replaced.
- **Arrival at the same head:** a fixed point needs all of these since the last arrival:
  - no RAM write and no IO access by the CPU (a sticky flag in `SystemBus`);
  - no device tick (`SystemBus::tick_due` sets the same flag: a device writes DDR through `IoCtx::ram`, not through
    the bus);
  - no run-slice boundary (`Machine::run` sets it on entry: the host may have changed RAM or devices in between);
  - no interrupt taken;
  - registers and CSRs unchanged;
  - the same period as the arrival before.

  The cheap conditions are checked first, so a loop that writes memory never compares registers. The flag is
  cleared at every arrival.
- **Off** when breakpoints are set, trace is on (`--trace`, implied by `--gdb`), `--log io` is on, or
  `--no-idle-skip` is given.

## 4. The skip

At a confirmed fixed point, with period `k` instructions, `c = clocks_per_insn`, remaining budget `r` instructions:

```
n = min(r / k, (next_deadline - now) / (k * c))     # whole passes only
now      += n * k * c
budget   -= n * k
idle_insns += n * k
```

- The CPU stays at the head: after whole passes that is exactly where it would be.
- The instructions up to the deadline then run normally, so `pre_step` ticks the devices at the same `now` as without
  the skip.
- The C64 is caught up at the same points.
- No tick collapse (02 §Tick collapse), because the skip never crosses a deadline.
- The run slice's budget still counts, so timed inputs (`InputTimeline`) land where they did.

## 5. Counting and display

- `cpu.insns` stays the count of executed instructions.
- `Machine::idle_insns` counts the skipped ones.
- The stats line becomes `<n> instructions, <m> skipped idle, <t> s emulated, ...`; the scripts only grep
  `s emulated`.
- MIPS stays host work: executed instructions per wall second. In realtime it no longer reads 25 when the firmware
  idles, so the window title adds the speed against realtime: `ue2emu — 12.3 s — 100 % — 4 MIPS`, and the headless
  end line prints the same.

## 6. Code

| File | Change |
|---|---|
| `crates/rv32/src/lib.rs` | `Csrs` derives `PartialEq` |
| `crates/ue2-core/src/bus.rs` | `SystemBus::idle_dirty`, set by RAM writes (8/16/32), IO reads/writes and `tick_due` |
| `crates/ue2-core/src/machine.rs` | `MachineConfig::idle_skip` (default true), the candidate slot, `idle_insns`, the budget loop in `run`, the stats line |
| `crates/ue2emu/src/main.rs` | `--no-idle-skip` |
| `crates/ue2emu/src/runner.rs`, `window.rs` | realtime percentage beside MIPS |

Impact (GitNexus): `Machine::pre_step` / `post_step` CRITICAL (15), `SystemBus::io_write8` CRITICAL (15), `io_read8`
HIGH (6). They are the step path of every run; the tests below compare whole runs with the skip on and off.

## 7. Not built

- Loops that change a register every pass (delay counters, checksums): not fixed points.
- Busy-waits that poll IO (for example a timer register): the read is an IO access. Skipping them would need the device
  to say when its value changes.
- A second RISC-V thread.

## 8. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean.
2. Unit tests in `machine.rs`, with small programs:
   - a `j .` loop with a timer deadline: skip ends exactly at the deadline, the IRQ is taken at the same `now` and with
     the same `mepc` as with the skip off;
   - a loop that writes RAM, one that reads IO, one that changes a register each pass: never skipped;
   - breakpoints, trace, `--log io`, `idle_skip = false`: never skipped;
   - the budget: `insns + idle_insns` equals the budget of a run without the skip.
3. Equivalence on the firmware: the upstream ELF, M2 for 60 s emulated, skip on and off. Console output, `now`, the
   CPU registers and CSRs, and a hash of all DDR must be identical, and `insns + idle_insns` must equal the executed
   count of the run without the skip.
4. The same equivalence with TRX64 attached (`--c64 trx64`), plus the C64 frame hash.
5. Smokes: `smoke-c64-carts.ctl` 27 PASS, `scripts/smoke-all.sh` green.
6. UltimateDemo2026, run D conditions (realtime, headless, audio on, the four windows): realtime in each window, and
   the host CPU per window against run D (76 / 85 / 69 / 97 %).
