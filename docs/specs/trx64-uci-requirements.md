# TRX64 requirements for the Ultimate Command Interface (UCI)

Requirements from UE2-C64U-Emulator to the TRX64 C64 core. The TRX64 spec that implements them is written on the TRX64
side. How UCI works and what UE2 builds itself is in `docs/specs/S15-uci.md`.

Reference prefixes:

- **TRX** = trx64-core at the pin in `crates/c64-bridge/Cargo.toml`. The requirements were written against rev
  69c9b30; they are built in TRX64 v0.6.0 (rev 2b145c9), Specs 850-852. The pin is now tag v0.9.2. Paths are under
  `crates/trx64-core/src/`.
- **Bridge** = `crates/c64-bridge/src/` in UE2.
- **cp** = `fpga/io/command_interface/vhdl_source/command_protocol.vhd`, **sl** = `fpga/cart_slot/vhdl_source/slot_slave.vhd`,
  **sm** = `fpga/cart_slot/vhdl_source/slot_master_v4.vhd`, **ss** = `fpga/cart_slot/vhdl_source/slot_server_v4.vhd`,
  all in the 1541ultimate firmware tree (UE2: `firmware/1541ultimate`, v3.15-9).

## 1. Background

UE2 runs the Ultimate firmware and uses TRX64 as the C64. UCI is a register block the firmware serves in the C64's
expansion-port I/O area: five bytes at `$DF1B-$DF1F` by default, relocatable to `$DFFB-$DFFF` or `$DE1B-$DE1F`, so the
whole range `$DE00-$DFFF` matters. UE2 implements the block (S15); TRX64 has to deliver the C64's accesses and a few
line effects. The facts the requirements rest on:

- **Reads have side effects.** A read of `$DF1E` or `$DF1F` advances a queue pointer and clears the IRQ enable, byte
  available or not (cp:176-185). Every PHI2 cycle with R/W and IO1/IO2 active counts (sl:143-144).
- **It works with or without a cartridge**, and next to a cartridge that also uses IO1/IO2. On reads the UCI answer
  wins over cartridge IO data (sl:292-301).
- **IRQ.** Rises when the firmware answers, falls on C64 accesses (cp:109, 161, 166, 177, 182); wired-OR into the C64
  IRQ (ss:1077).
- **Freeze.** A C64 write to the control register can hold the 6510 until the firmware releases it (cp:153; sm:115-118,
  142-145).
- **Trigger and unlock.** Freeze can also start on a C64 write to `$FF00` (cp:192-195; ss:863). The U64 unlock is a
  write of `$AB` to `$D038` then `$CD` to `$D036` (megabyter.tas:342-348).
- **No reset on C64 reset.** Only the FPGA system reset clears the block (cp:292-306).

## 2. Priority

- **MUST**: without it UE2 can serve UCI only by attaching a fake cartridge.
- **SHOULD**: UE2 can work around it with today's API (S15 §4 step 2).

## 3. Requirements

### R1 — Expansion-port I/O device independent of the cartridge (MUST, generic)

- **Requirement.** Add an optional I/O device on `Machine`, separate from `Machine::cartridge`. FullBus calls it for
  every read and write of `$DE00-$DFFF` while I/O is banked in. That includes dummy reads, RMW dummy writes and the
  live host accesses (`read_full_live`, `write_full`), the same set of accesses that reaches the cartridge today.
  - **Read:** it gets the address, the clk and the cartridge's answer (`Option<u8>`). It returns the byte the CPU sees,
    or None. None keeps today's result: the cartridge's byte, or `vic.last_read_phi1`.
  - **Write:** it gets address, value and clk, and is called in addition to the cartridge. It does not change the
    cartridge's "consumed" result.
  - **No side effects on TRX64 state.** Attaching the device changes none of these: `cartridge.is_some()`, the PLA
    index, `BankInfo`, VSF export, reset behaviour. No C64 reset calls into the device.
- **Why.**
  - With no cartridge, no host code runs on an access to `$DE00-$DFFF` (TRX full.rs:486-490, 659-661; Bridge
    lib.rs:163-170, 236-241).
  - The only workaround is a fake `CartProxy`, and TRX64 then treats the machine as having a cartridge (TRX full.rs:268;
    vsf_export.rs:136-137; lib.rs:983-985).
  - A C64 reset must not reset UCI state (cp:292-306).
- **Acceptance.**
  - No cartridge, device attached, `$01=$37`: running `LDA #$41 / STA $DF1D / LDA $DF1C`, the device sees the write
    (`$DF1D`, `$41`, clk) and the read (`$DF1C`, clk, None). A holds the device's byte.
  - The device returns None: A = `vic.last_read_phi1`, as today.
  - `$01=$34`: the device is not called.
  - Cartridge and device both attached: the device receives the cartridge's byte, and its return value decides.
  - `LDX #$1F / LDA $DEFF,X`: the device is called for the dummy read at `$DE1E`, then for the read at `$DF1E`.
  - `cold_reset` and `warm_reset`: the device gets no call, and `cartridge` stays None.
- **TRX64 code:** `Machine::cartridge` lib.rs:530; FullBus fields full.rs:210-228; `io_read` full.rs:485-491;
  `io_write` full.rs:612-620; FullBus constructions lib.rs:1149, 1200, 2091 (plus the bus tests, full.rs:941);
  `cold_reset` lib.rs:983-985; `warm_reset` lib.rs:1054-1058.
- **Generic:** yes. REU, ACIA, sampler, or any other register device on the port could use it.

### R2 — Side-effect-free peek of `$DE00-$DFFF` (SHOULD, generic)

- **Requirement.** `Machine::read_full` and `peek_lens` ("io", "cart") ask the cartridge's `peek` and a side-effect-free
  peek of the R1 device for `$DE00-$DFFF`. They return `vic.last_read_phi1` only when neither answers.
- **Why.** Reads of `$DF1E`/`$DF1F` consume data (cp:176-185), so a debugger must not use the live read. Today these
  paths return `last_read_phi1` without asking anyone (lib.rs:1631, 1691, 1701-1706). `CartMapper::peek` exists
  (cart.rs:362) but is not called there. The bridge's `dma_peek` inherits this (Bridge lib.rs:484-491).
- **Acceptance.** `read_full($DF1C)` returns the device's peek value. 100 calls to `read_full($DF1E)` leave the device
  state unchanged. With no device and no cartridge, the result is as today.
- **TRX64 code:** lib.rs:1617-1631, 1680-1706; cart.rs:362.
- **Generic:** yes.

### R3 — Stop request from the device (SHOULD, generic)

- **Requirement.** The R1 device can request that the current run stops at the next instruction boundary. This works on
  the plain run path (`run_for_full_capped`), with no access-watch table and no Observer. The stop reason can be told
  apart from a budget stop.
- **Why.** Some C64 accesses change the IRQ or freeze and must take effect before the next instruction: the IRQ is
  cleared by a `$DF1E`/`$DF1F` read or DATA_ACC (cp:166, 177, 182); freeze is set by PUSH_CMD b7 (cp:153). Today only
  `run_for_full_capped_dbg`, with an access_watch table and `Observer::on_access`, can stop there (lib.rs:2017-2023,
  2146-2148; full_sc.rs:209-212, 274-277). R3 is not needed if R5 and R6 can be driven from inside the device.
- **Acceptance.** `STA $DF1C` (the device requests a stop) followed by `INC $D020`: the run ends with PC at the INC,
  and `$D020` is unchanged.
- **TRX64 code:** lib.rs:165 (`RunStop`), 1991-2001, 2017-2150; full_sc.rs:195-279.
- **Generic:** yes.

### R4 — Write snoop by address, independent of banking (SHOULD, generic)

- **Requirement.** The host registers addresses outside `$DE00-$DFFF`. For each C64 write to them it sees address,
  value and clk, whatever the banking, on the plain run path. Both write cycles of an RMW instruction are reported.
  Needed addresses: `$FF00`, `$D036`, `$D038`.
- **Why.**
  - Trigger: TRIGGER plus a C64 write to `$FF00` enters freeze (cp:192-195; ss:863); the FPGA sees every write cycle
    (sl:142).
  - Unlock: `$AB` to `$D038`, then `$CD` to `$D036` (megabyter.tas:342-348).
  - Today `$FF00` writes reach the cartridge only in ULTIMAX (full.rs:835-843), `$D036`/`$D038` go to the VIC
    (full.rs:554), and the `$FF00` hook named at c64_6510core.rs:470-472 and 487 does not exist in FullScBus
    (full_sc.rs:226-279, 311-338).
- **Acceptance.**
  - `$01=$37`, snoop on `$FF00`: `STA $FF00` reports one write and RAM is written as today; `INC $FF00` reports two
    writes, first the old value, then the new one.
  - Snoop on `$D036`: `STA $D036` reports the write, and the VIC write still happens.
  - Unregistered addresses report nothing.
- **TRX64 code:** full.rs:554, 835-843; full_sc.rs:226-279, 316-327; c64_6510core.rs:470-472, 487.
- **Generic:** yes. The REU `$FF00` trigger could use it too.

### R5 — Expansion-port interrupt sources (SHOULD, generic)

- **Requirement.** Add separate interrupt sources for the expansion-port IRQ and NMI (`C64_NUM_INT_SOURCES` grows). The
  host can set them between runs; the R1 device can set them at the access clk. The 6510 applies the same delay rules
  as for VIC and CIA sources.
- **Why.** The UCI IRQ rises when the firmware validates and falls on C64 accesses (cp:109, 161, 166, 177, 182). There
  are four sources (c64_6510core.rs:143-155), so the bridge puts both the cartridge IRQ and NMI on the RESTORE source
  3 and updates them only between runs (Bridge lib.rs:204-220, 274-276).
- **Acceptance.** Expansion IRQ asserted and CLI executed: the 6510 takes it with the same delay as a CIA IRQ. An IRQ
  handler drops the line through `LDA $DF1E`: after RTI there is no second IRQ. The RESTORE NMI and the expansion NMI
  are independent.
- **TRX64 code:** c64_6510core.rs:143-155, 176, 218-219; lib.rs:2075-2083.
- **Generic:** yes. Every IRQ/NMI cartridge could use them.

### R6 — CPU hold as a TRX64 run state (SHOULD, generic)

- **Requirement.** A run state in which the 6510 does not execute while VIC, CIAs, SID and drive keep running. Entered
  from the host, or from the R1 device (effective at the next instruction boundary at the latest); released by the
  host. `read_full_live` and `write_full` work while it is held.
- **Why.** UCI freeze holds the 6510 through DMA until the firmware validates (sm:115-118, 142-145; ss:725), and
  C64_STOP needs the same state. The bridge re-implements chip clocking for this (Bridge lib.rs:294-323;
  docs/specs/S14-c64-trx64.md:141, API gap 1). `IK_DMA` is defined but never used (c64_6510core.rs:115). For UCI an
  instruction-boundary stop is exact: PUSH_CMD and the `$FF00` write happen in the last cycle of the instruction.
- **Acceptance.** Hold for 19656 cycles (one PAL frame): `$D012` passes through all lines, CIA1 timer A counts, PC and
  the CPU registers are unchanged, `write_full($0400, x)` lands. After release, execution continues at the same PC.
- **TRX64 code:** lib.rs:2017-2150; full_sc.rs:358-383; c64_6510core.rs:115.
- **Generic:** yes. C64_STOP and DMA could use it.

## 4. Open questions for TRX64

1. **Stretched reads.** The 6510 can stretch a read of `$DF1E`/`$DF1F` over several cycles while RDY is low. On the
   FPGA each PHI2 cycle counts (sl:143-144); TRX64 runs the BA steal before the read and then reads once
   (c64_6510core.rs:461-468; full_sc.rs:340-351). Should R1 report one read or one per cycle?
2. **R1 or the fake cartridge.** Will TRX64 take R1, or does UE2 keep the fake `CartProxy` for the no-cartridge case?
