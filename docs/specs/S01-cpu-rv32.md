# S01 — RV32 CPU (rvlite semantics)

**Owns:** `crates/rv32/**`, `scripts/build-riscv-tests.sh`
**Reads:** `docs/hw/01-cpu-boot-memory.md` (CPU sections, H2-H8, T0/T1), `docs/hw/00-memory-map.md` §2 A1, B7-B10, §Interrupts

## Scope

Implement `Cpu::step`, `csr_read`, `csr_write` in `crates/rv32/src/lib.rs`. Keep the public API from the
skeleton; add private modules freely.

- **Instruction set:** RV32I (all base opcodes incl. FENCE/`MISC-MEM` as NOP). M extension: MUL, MULH,
  MULHSU, MULHU, plus DIV/DIVU/REM/REMU with standard semantics (the firmware has none; rvlite decodes them
  as MUL — note it, don't emulate it). Zicsr: CSRRW/S/C and the immediate forms.
- **CSRs (rvlite, doc 01):**
  - Stored: `mstatus` (MIE bit 3, MPIE bit 7 only), `mie` (MEIE bit 11), `mtvec` (writes mask bits 1:0 to 0,
    direct mode only), `mepc`, `mcause`, `mscratch`.
  - `mip` reads MEIP from `self.meip`; writes are ignored.
  - `mtval` reads 0.
  - Every other CSR reads 0 and ignores writes; never trap.
  - Follow doc 01 wherever it is more specific (e.g. zimm handling, mepc masking).
- **Traps:**
  - ECALL: `mcause = 11`.
  - EBREAK: `mcause = 3`, return `Exit::Ebreak`.
  - Illegal instruction: `mcause = 2`, return `Exit::Illegal(word)`.
  - For all three: `mepc = pc`, `MPIE = MIE`, `MIE = 0`, `pc = mtvec`.
  - MRET: `pc = mepc`, `MIE = MPIE`, `MPIE = 1`.
  - WFI: NOP, return `Exit::Wfi`.
- **Interrupt:** checked before fetch. If `meip && mie.MEIE && mstatus.MIE`: `mepc = pc`,
  `mcause = 0x8000000B`, `MPIE = MIE`, `MIE = 0`, `pc = mtvec`, return `Exit::Interrupt` (no instruction
  executed). Implement the rvlite `inhibit_irq` latency only if doc 01 shows the firmware depends on it.
- `x0` is hardwired to 0. PC-relative targets with low bits set: follow doc 01 (no C extension, no
  misaligned trap unless documented).
- **Performance:** the hot path must not allocate. Target ≥ 150 MIPS in a release build on Apple Silicon
  (add a `benches`-free timing test marked `#[ignore]` that reports MIPS).

## Tests

- `scripts/build-riscv-tests.sh`:
  - Builds `tools/riscv-tests` `rv32ui-p-*` and `rv32um-p-*` with `tools/bin/riscv32-unknown-elf-*`
    (`-march=rv32im_zicsr` for GCC 11 or whatever the toolchain accepts).
  - Linker script at 0x80000000.
  - Output ELFs under `target/riscv-tests/`.
- `crates/rv32/tests/riscv_tests.rs`:
  - Loads each ELF (dev-dependency `object`) into a simple RAM bus.
  - Runs until the `tohost` symbol address is written. Pass = value 1.
  - Skips with a message when the ELFs are absent.
  - Ignore any test that needs S/U mode or a feature rvlite lacks, with a stated reason.
- Unit tests:
  - ECALL/MRET round trip, interrupt entry gating (MIE, MEIE, meip), `mtvec` masking.
  - `mip` read-only, unknown CSR RAZ/WI, `x0` writes ignored.
  - `csrci mstatus,8` / `csrsi`.

## Acceptance

`scripts/build-riscv-tests.sh && cargo test -p rv32` — every rv32ui/rv32um test passes (skips listed with
reasons), unit tests pass, MIPS figure reported.
