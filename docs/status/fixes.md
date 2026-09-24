# Fixes: fault-hook fast path, get_mem PANIC hook, clock validation, ITU peek, EDID

These are fixes for review findings and small board gaps, made on top of `main` @ d51fa61. Every item was checked in a real
run of the unmodified firmware as well as by unit tests.

Commands run from a worktree, with the firmware outside the checkout:

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate
FW="--firmware $UE2_FIRMWARE/target/u64ii/riscv/ultimate/result/ultimate.elf --roms $UE2_FIRMWARE/roms"
cargo build --release
UE2_FIRMWARE=$UE2_FIRMWARE cargo test --workspace
```

## 1. Fault-hook check in the step loop (`machine.rs`)

- **Before:**
  - With the hooks armed (the normal case: `halt_on_fault` and an ELF with symbols), `Machine::run` called
    `stop_at(pc)` for every instruction.
  - `stop_at` built a closure, compared three `Option<u32>` and searched the breakpoint list.
- **Now:**
  - `FaultHooks` holds plain `u32` addresses. An absent hook is `NO_HOOK = u32::MAX`, which never equals a PC:
    every jump target has bits 1:0 cleared (fetch.vhd:58).
  - The loop copies the hooks into a local and calls `FaultHooks::hit`: four branch-free compares, inlined.
  - `stop_at` is `#[cold]`. It runs only on a hit or while breakpoints are set.
  - Resuming from a stop works as before.
- **Measurement:** A/B of the parent-commit binary against this branch.
  - Workload: a 40 s emulated ELF boot (`--headless --speed max`, script `wait 40000`), about 1 000 M instructions
    per run, in ABBA order.
  - Host: Apple M4, shared with other agents' builds and emulators (load average 3-12 during the runs).
  - MIPS = instructions / wall time of the whole process (including start-up), and instructions / user CPU time.

| Batch | Rounds | base wall MIPS median (max) | new wall MIPS median (max) | base user-time median | new user-time median |
|---|---|---|---|---|---|
| 1 | 6 | 190.7 (192.3) | 185.8 (219.8) | 192.3 | 201.8 |
| 2 | 10 | 177.2 (197.3) | 215.6 (222.3) | 181.3 | 216.8 |

- **Batch 1:** the new wall median includes two runs slowed by host contention: 7.9 s and 10.0 s wall at
  5.6 s and 5.3 s user.
- **Uncontended runs:** base takes 5.07-5.25 s wall per run and new takes 4.50-4.68 s. The new loop is about 12 %
  faster; it is never slower.

## 2. `get_mem` PANIC hook (`machine.rs`)

- **What the firmware does:** when `pvPortMalloc` returns NULL, `get_mem` (behind `new` and `new[]`) prints
  `** PANIC **: Error allocating <caller>..` and spins on `while(1);` (system/memory_wrap.cc:31-40; docs/hw/01 H14,
  T0 diagnostics hooks). Before this fix the emulator kept running that loop silently.
- **Finding the loop:** `FaultHooks::resolve` finds `_Z7get_memj` by symbol and scans its loaded body for two words.
  - The `j .` word `0x0000006F` is the hook PC.
  - The prologue's `sw ra, off(sp)` gives the stack slot of the return address.
  - In the ELF, `get_mem` is at 0x43294: `sw ra,28(sp)`, and `j .` is at 0x432C4. The unit test pins both values
    against the real ELF.
- **Halt message:** `get_mem PANIC: pvPortMalloc returned NULL, get_mem called from <symbol>`. The caller comes
  from the saved `ra` slot, because `printf` has already overwritten `a1`.
- **Real run:** GDB forces a failed allocation.
  - Stop at 0x432B4, just after `pvPortMalloc` returns (a0 = 0x159500), set `$a0 = 0`, continue.

```
halted: get_mem PANIC: pvPortMalloc returned NULL, get_mem called from W25Q_Flash::W25Q_Flash()+0x44
console: ** PANIC **: Error allocating 00044710..
gdb:     Program received signal SIGABRT ... => 0x432c4 <_Z7get_memj+48>: j 0x432c4
```

- **Check:** 0x44710 is `W25Q_Flash::W25Q_Flash()+0x44`, the instruction after `jal operator new[]`. The symbolized
  caller matches the firmware's own `%p`.

## 3. `clocks_per_insn > 0` (`main.rs`, `machine.rs`)

- With 0 clocks per instruction, emulated time stands still: `wait_ms`, the ms timer and the FreeRTOS tick never
  advance.
- **CLI:** `--clocks-per-insn` has the clap range `1..`.
  - `ue2emu run --headless --clocks-per-insn 0` prints
    `error: invalid value '0' for '--clocks-per-insn <CLOCKS_PER_INSN>': 0 is not in 1..18446744073709551615`.
- **Library:** `Machine::new` rejects the value before loading anything:
  `clocks_per_insn must be at least 1, or emulated time stands still`.

## 4. `Itu::peek8` (`itu.rs`)

- **Before:** `x/` in GDB on 0x10000000 showed only zeros, because the ITU had no `peek8`.
- **Now:** the registers that do not read the IRQ core come from one function, `Itu::get(off, now)`. Both `read8` and
  `peek8` use it:
  - capabilities, FPGA version, buttons, UART FLAGS;
  - the ms timer, ITU_TIMER, and IRQ timer enable and counter.
- **Time base:** the timers are shown at `synced`, the clock of the last ITU access or tick, which `sync` stores. A
  peek delivers no pulse.
- **Not visible:** GLOBAL, ENABLE, EDGE, ACTIVE and IRQ_HIGH_EN/ACT live in `IrqState` on the bus and peek 0.
- **Real run:** GDB with a breakpoint at `prvIdleTask`, then 400 ticks later (`ignore 2 399` on
  `xTaskIncrementTick`):

```
0x1000000c: 0x34 0x00 0x02 0x22     capabilities
0x1000000b: 0x25                    FPGA version
0x1000000a: 0x00                    buttons
0x10000012: 0x40                    UART FLAGS
0x10000022: 0x02 0x2e               ms timer 558
0x10000000: 0x00                    GLOBAL (IRQ core, not peekable)
0x10000022: 0x09 0xf2               ms timer 2546, 400 ticks later (2.546 s emulated)
```

## 5. EDID EEPROM on I2C bus 0 (`i2c.rs`)

- **Symptom:** the boot log ended with `EDID Header incorrect.`
  - The HPD task reads EDID once at start-up: `read_edid` on channel 0, address 0xA0, 128 bytes from 0x00, then 128
    bytes from 0x80 (u64_config.cc:969-972, 1002-1010, 2577-2627).
  - The T0 master returned 0xFF for every byte, so `IsMonitorHDMI` failed on the header.
- **Master model** (03 §Functional model "HW I2C master (T1)"):
  - STARTED state; the byte after START or a repeated start is the address byte.
  - `data_out` is 0xFF after TX and the received byte after RX.
  - STOP ends the transfer. Soft reset resets the master but not the monitor.
  - BUSY stays 0 and every address ACKs, as at T0, so the codec, hub, expander and PLL transfers are unchanged.
- **DDC device on channel 0:**
  - EDID EEPROM at 0xA0/0xA1: the first data byte of a write sets the pointer; reads are sequential.
  - E-DDC segment pointer at 0x60: selects `segment * 256 + pointer`, cleared by STOP (i2c_drv.cc:356-399).
- **EDID** (256 bytes; the checksums are computed by a `const fn`):
  - EDID 1.3 base block: preferred timing 1920×1080@60 at 148.5 MHz, range limits, name `UE2EMU HDMI`, one
    extension.
  - CEA-861 extension: VICs 16 (native), 4, 3, 2, 1, 31, 19, 18, 17; LPCM audio; the HDMI VSDB (OUI 00-0C-03,
    physical address 1.0.0.0); a 720p60 timing descriptor.
  - A unit test runs `IsMonitorHDMI`'s data-block walk over it.

**Real runs**, parent-commit binary against this branch, same commands:

| Check | Before | After |
|---|---|---|
| ELF, 20 s emulated, `--log unmapped` | `EDID Header incorrect.`, 0 unmapped | line gone, 0 unmapped |
| `c64u_v1.1.0.ue2`, 20 s emulated | `EDID Header incorrect.`, 0 unmapped | line gone, 0 unmapped |
| `--log io`, 3 s emulated: `configure_hdmi_output` | `W 0x10100408 0x00` (DVI) | `W 0x10100408 0x01` (HDMI) |
| `scripts/smoke-menu.ctl` | menu, cursor moves | identical screen dumps |

- **ELF console diff:** only the RTC date line and the removed EDID line.
- **`.ue2` console diff:** additionally one task-list stack high-water mark (WiFi Command Task 1456 → 1454).
- **smoke-menu:** the dumps are byte-identical once the date and EDID lines are removed.

## Tests

- `UE2_FIRMWARE=... cargo test --workspace`: 198 passed, 0 failed (192 before).
- New tests:
  - `machine::tests::get_mem_panic_loop_halts_with_the_caller`
  - `machine::tests::new_rejects_zero_clocks_per_insn`
  - `devices::itu::tests::peek_shows_plain_registers_as_of_the_last_access`
  - `devices::i2c::tests::edid_blocks_are_valid_and_describe_an_hdmi_1080p60_monitor`
  - `devices::i2c::tests::read_edid_gets_both_blocks_on_the_hdmi_bus`
  - `devices::i2c::tests::segment_pointer_selects_edid_pages_until_stop`
- Updated tests:
  - `new_loads_the_firmware_and_resolves_hooks` (hooks as `u32`, get_mem loop 0x432C4 and slot 28);
  - `b3_i2c_not_busy` (STARTED bit; a read from a device without a model).
- `cargo clippy --workspace --all-targets` reports nothing in these files. Its only warnings are the two existing
  `is_multiple_of` suggestions in `io.rs`.

## Known gaps

- **DDC gating:** the EEPROM answers whether or not U64_HDMI_REG has DDC_ENABLE (0x20) or DDC_DISABLE (0x10)
  written; that register belongs to `u64io.rs`. HPD stays 1, so the firmware reads only between those writes anyway.
- **No hot-plug:** high IRQ 5 is never raised, so `read_edid` runs once at boot (docs/hw/05 §Interrupts).
- **Other I2C devices:** NAU8822, USB2513, the expanders and the PLLs still have no models. Their addresses ACK,
  drop data and read 0xFF.
- **The get_mem hook** needs `_Z7get_memj` with a `j .` and a `sw ra, off(sp)` in its body. `.app` and `.ue2` images
  have no symbols, so they get no hooks at all.
