# S02 — Core: SystemBus, loader, symbols, Machine loop, runner

**Owns:**
- `crates/ue2-core/src/{bus.rs,loader.rs,symbols.rs,machine.rs}`
- `crates/ue2emu/src/runner.rs`
- the `[dependencies]` of `crates/ue2-core/Cargo.toml`

**Shared, do not change signatures:** `io.rs`, `irq.rs`, `time.rs`, `host.rs` (additive changes only, reported)
**Reads:** `docs/ARCHITECTURE.md`, `docs/hw/00-memory-map.md` §1 bus rules, §1a, `docs/hw/01-cpu-boot-memory.md` (loader, memory)

## Scope

**`bus.rs` — `impl rv32::Bus for SystemBus`**
- **Decode:**
  - `addr & 0x1000_0000 == 0` → RAM `addr & RAM_MASK`. Fast path for `read32`/`write32`/`fetch` when the
    word lies inside RAM.
  - `0x10000000-0x10FFFFFF` → `IoMap::resolve`. Hit: call the device with an `IoCtx { now, pc, ram, irq, console }`
    built from split field borrows. Miss: read 0 / write ignored.
  - Everything else (e.g. `0x80000000` boot BRAM) → read 0 / write ignored.
- 16/32-bit IO accesses = LE byte sequence at addr+0..+3. Set `io_touched = true` on every IO access.
- **Unmapped log** (`cfg.log.unmapped`): the first 4 hits per distinct address go to stderr as
  `unmapped R/W addr [val] @pc sym+off`, plus a summary table on exit (address, R/W counts). The bus has no
  symbols, so keep raw PCs and let the machine symbolize when printing.
- **IO trace** (`cfg.log.io`): every IO byte access, symbolized by the machine.

**`loader.rs`**
- `load_elf(path, ram) -> LoadedElf { entry, segments }`.
- PT_LOAD `filesz` is copied and the rest of `memsz` zeroed, masked into RAM.
- Error on a non-RISC-V or 64-bit ELF.

**`symbols.rs`**
- `Symbols::from_elf(path)`: function + object symbols sorted by address, C++ demangled (`cpp_demangle`),
  sizes kept.
- `lookup(addr) -> Option<(&str, offset)>`, `addr_of(name)` (mangled or demangled), `format(addr)`
  → `name+0x12` or `0x...`.
- `Symbols::empty()` for tests.

**`machine.rs`**
- `Machine::new(cfg)`:
  - Allocates the bus and calls `devices::install_all(&mut bus.io, &cfg)`.
  - Loads the ELF: `cpu.pc = entry`, registers 0. Loads symbols.
  - Resolves the fault-hook addresses (`vAssertCalled`, `C_exception_handler`, `__crt0_dummy_trap_handler`)
    when `cfg.halt_on_fault`.
- `run(max_insns)`: the loop in ARCHITECTURE §Execution model.
  - `bus.pc = cpu.pc` before each step.
  - Deadline recompute when `io_touched`; device ticks.
  - `Exit::Illegal` → `RunExit::Halted`.
  - Fault hook PC → `Halted("vAssertCalled called from <ra symbol>")`.
  - Breakpoints checked only when non-empty.
- `input(HostInput)` → `bus.io.get_mut::<Itu>()` / `get_mut::<U64Io>()` host methods (signatures in the
  device skeletons).
- `display()` → `bus.io.get::<Overlay>().snapshot(now_ms)`, or a default snapshot.
- A `Machine::stats()` string: instructions, emulated seconds, IRQs taken (count `Exit::Interrupt`).

**`crates/ue2emu/src/runner.rs`**
- `spawn(cfg, opts) -> EmuHandle`: an emulation thread running `machine.run(100_000)` slices.
  - After each slice:
    - drain the console to stdout, stripping `\r`;
    - process queued `Command`s;
    - publish the `DisplaySnapshot` every 20 ms emulated into `display`;
    - update `now_ms`;
    - pace (`Speed::Realtime`: sleep while emulated ms is ahead of wall ms by more than 2 ms).
  - `RunExit::Halted(msg)` → print msg + stats and end the thread with an error.
- `run_headless(cfg, opts)`:
  - spawn;
  - if `opts.script` is set, call `crate::control::run_script`;
  - otherwise wait `opts.max_seconds` wall seconds (or forever);
  - send Quit, join, print stats;
  - also start `control::serve` if `opts.control` is set.

## Tests

- **Bus:**
  - RAM word fast path equals byte path.
  - IO 32-bit write reaches a test device as 4 ordered bytes.
  - Unmapped read is 0.
  - RAM mirror above 64 MB.
- **Loader + symbols** (skip if the ELF is missing): entry 0x30000; `addr_of("ultimate_main")` resolves;
  `lookup(0x33700)` = `freertos_risc_v_trap_handler`.
- **Machine** with a hand-assembled program in RAM and a fake ticking device:
  - deadline recompute and tick ordering;
  - `meip` follows `irq.line()`.

## Acceptance

`cargo test -p ue2-core` passes. Once S01 has landed,
`cargo run --release -p ue2emu -- run --headless --max-seconds 3 --log unmapped` executes firmware
instructions and prints a stats line.
