# S15 — Ultimate Command Interface (UCI): the integration

The UCI block is C64 hardware, and it lives in TRX64. **TRX64 Spec 852** models `command_protocol.vhd` on the `u64`
machine profile (Spec 851), on the expansion-port device interface of Spec 850. UE2 serves the firmware side: it maps
`CMD_IF_BASE`, drives the ITU bits, and routes the window.

What UE2 once planned to build itself — its own model of `command_protocol.vhd` in ue2-core, a bridge window decode,
a fake `CartProxy` for the no-cartridge case — is not built and is not needed. S15's old open question 9 ("where does
the state live") is answered: in TRX64.

Reference prefixes:

- **FW** = `firmware/1541ultimate` in this repo (v3.15-9, `b617777c`). Short names:
  - `cp` = FW/fpga/io/command_interface/vhdl_source/command_protocol.vhd
  - `pkg` = FW/fpga/io/command_interface/vhdl_source/command_if_pkg.vhd
  - `sl` = FW/fpga/cart_slot/vhdl_source/slot_slave.vhd; `ss` = .../slot_server_v4.vhd; `sm` = .../slot_master_v4.vhd
  - `intf` = FW/software/io/command_interface/command_intf.cc
  - `c64.cc` = FW/software/io/c64/c64.cc; `u64_config.cc` = FW/software/u64/u64_config.cc
- **Bridge** = `crates/c64-bridge/src/` in this repo. **Core** = `crates/ue2-core/src/`.
- **TRX** = trx64-core, `uci.rs` (the block), `expansion.rs` (the port), `lib.rs` (the machine API).

## 1. What UCI is

UCI lets a C64 program send a command to the Ultimate firmware and read back data and status, through five register
bytes in the cartridge I/O area (`$DF1B-$DF1F` by default).

The first command byte selects a target in the firmware (dos.cc:12-13, network_target.cc:16, control_target.cc:25,
softiec_target.cc:9, http_target.cc:6): 1 and 2 DOS, 3 network, 4 control, 5 SoftIEC, 6 HTTP. The targets are firmware
code and run unmodified in UE2. The interface is off by default (c64.cc:115).

Register API and target commands, converted from Gideon's PDFs, in the 1541ultimate repo:
`doc/command-interface-v1.1.md`, `doc/uci-control-target-v1.0.md`, `doc/uci-http-target-v0.2.md`,
`doc/uci-network-target-v1.0.md`, `doc/uci-softiec-target-v1.0.md`.

## 2. How it works on the hardware

The U64-II top level is closed (Bridge slot.rs:16-18). Everything below comes from the open cartridge-side VHDL and
the firmware. **Everything in §2.1 §2.3 §2.4 §2.5 is TRX64's now** (Spec 852 §2 ports `cp` one to one); it is kept
here because the firmware side and the routing below only make sense against it.

### 2.1 Window and enable — TRX64

- **Address match.** SLOT_BASE bits 6:1 are compared with C64 address bits 8:3, and the access must have IO1 or IO2
  active (cp:108, 141; sl:143-144). Address bits 2:0 select the register (pkg:27-31).
- **Where the window goes.** Only these writes set SLOT_BASE; none of them looks at GAME/EXROM:

  | SLOT_BASE | Window | Set by |
  |---|---|---|
  | 0x47 | `$DF18-$DF1F` | config "Command Interface" (c64.cc:328-331), CRT flag `CART_UCI` (c64.cc:1305-1309), U64 unlock (u64_config.cc:1012-1019), `CommandInterface` constructor (intf:45) |
  | 0x7F | `$DFF8-$DFFF` | CRT flag `CART_UCI_DFFC`: SID/MUS player carts (c64.cc:1311-1315; filetype_sid.cc:73, 85) |
  | 0x07 | `$DE18-$DE1F` | CRT flag `CART_UCI_DE1C`: EasyFlash (c64.cc:1317-1321; c64_crt.cc:504-512) |

- **Enable** is firmware register +1 bit 0 (cp:203-205). While disabled, the block does not answer reads and ignores
  writes (cp:108, 141), so the window reads the open bus — the firmware's own power-on default.
  - Cleared at every cartridge start (c64.cc:1201), and by a CRT `prohibit` that contains a UCI bit
    (c64.cc:1353-1360). `CART_PROHIBIT_IO` contains `CART_UCI` (c64.h:255-257).
  - EasyFlash: subtype 1 keeps UCI at `$DE1C`; subtype 0 requires it and prohibits it in the same definition, so it
    ends up off (c64_crt.cc:504-512). `$DF00-$DFFF` stays EasyFlash RAM in both cases.

### 2.2 Who serves the I/O area: GAME/EXROM and bus sharing — the bridge

A cartridge in the external port does not move the window. It can hide it, and that multiplexer is the U64's, not the
block's, so it stays on the UE2 side (§4).

- `ConfigureU64SystemBus` (c64.cc:1509-1596) reads `U64_CART_DETECT` (u64.h:68: GAME bit 0, EXROM bit 1). An external
  cartridge is present when either line is low (c64.cc:1514).
- "Cartridge Preference" decides C64_BUS_INTERNAL / C64_BUS_EXTERNAL (u64.h:132-133; bit 0 IO1, bit 1 IO2, bit 2 ROM,
  bit 3 IRQ):
  - Automatic: with an external cartridge internal = 0, external = 15, so all internal I/O including UCI is off the
    bus; without one internal = 15.
  - Internal: 15 / 0. External: 0 / 15. Manual: per range internal, external or both (c64.cc:1536-1590).
- With an external cartridge at start, no internal cartridge is started and the UCI enable stays 0 (c64.cc:1201,
  1209-1213, 1457-1464). A later settings change calls `ConfigureU64SystemBus` and `set_emulation_flags` again, which
  sets the enable from the config (c64.cc:270-281); the window is still hidden while C64_BUS_INTERNAL has no IO bit.
- **Unlock is the exception.** `unlock_irq` sets C64_BUS_INTERNAL |= 0x02 and UCI at `$DF1C`, also with an external
  cartridge (u64_config.cc:1012-1019). Then both sides drive IO2 (open question 2).

### 2.3 C64-side registers (base 0x47) — TRX64

| Addr | Read | Write | Ref |
|---|---|---|---|
| `$DF18-$DF1A` | `$FF` | ignored | cp:103, 171 |
| `$DF1B` | Bus ID. Bits 4:0 are set by the firmware; bits 7:5 read 0. | ignored | cp:83, 102, 207 |
| `$DF1C` | Status. b7 DATA_AV, b6 STAT_AV, b5:4 state, b3 ERROR, b2 abort pending, b1 data-accepted pending, b0 new command. | Control. b0 PUSH_CMD: also latches b7 DMA, b6 TRIGGER and b5 IRQ. b1 DATA_ACC, b2 ABORT, b3 CLR_ERR. | cp:84-89, 148-170 |
| `$DF1D` | `$C9`, or `$49` while the C64 IRQ is active | Command byte into RAM[command_pointer]; the pointer increments and stops at 895. | cp:99, 111, 121, 144-147 |
| `$DF1E` | Response byte if DATA_AV, else `$00` | ignored | cp:100, 105 |
| `$DF1F` | Status byte if STAT_AV, else `$00` | ignored | cp:101, 106 |

- A read of `$DF1E` increments the response pointer (stops at 1791) and clears the IRQ enable; `$DF1F` the same with
  the status pointer (stops at 2047). Both happen on every read, byte available or not (cp:176-185).
- A read the VIC stretched advances the pointer `stalled_on_bus + 1` times, not once per stolen cycle (Spec 852 D5;
  Spec 850 hands the device both stall counts). That is an assumption about the closed U64 bus, measured in TRX64's
  gate as 4 advances on a badline, never 44.
- Writes reach cartridge IO RAM as well (sl:105). On reads the UCI answer wins over cartridge IO data (sl:292-301).

### 2.4 Protocol state — TRX64

| From | To | Caused by | Ref |
|---|---|---|---|
| 00 | 01 | C64 PUSH_CMD | cp:155-157 |
| 01 | 10 (last) or 11 (more) | Firmware writes HANDSHAKE_OUT b4 (b5 = more). Also resets the reply pointers and clears freeze and trigger. | cp:220-226 |
| 10 | 00 | C64 DATA_ACC | cp:163-167 |
| 11 | 01 | C64 DATA_ACC | cp:163-167 |
| any | 00 | Firmware writes HANDSHAKE_OUT b7: abort, no-reply commands, reset. | cp:227-232; intf:142, 171 |

- PUSH_CMD outside state 00 sets ERROR (cp:158-159). ABORT only sets a flag; the firmware returns the state to 00
  (cp:168-169; intf:136-144).
- The spec says DATA_ACC empties the queues (command-interface-v1.1.md §2.4.1). The VHDL only leaves the data state;
  the pointers are reset at the next validate (cp:225).

### 2.5 Lines into the C64 — TRX64

- **IRQ** = state(1) AND cmd_irq_en (cp:109), wired-OR into the C64 IRQ line (ss:1077). In TRX64 it is a line on
  `INT_SRC_EXPANSION`, sampled per cycle (Spec 850 D6), so a `$DF1E` read drops it at that cycle.
- **Freeze (DMA)** starts on PUSH_CMD with b7 = 1, or on PUSH_CMD with b6 = 1 followed by any C64 write to `$FF00`
  (cp:153-154, 192-195; ss:863), the latter through Spec 850's write snoop. In TRX64 it is `Hold::Cpu` (Spec 850 D7),
  entered at the next instruction boundary, which is exact: PUSH_CMD and the `$FF00` write are the last cycle of
  their instruction. It ends when the firmware validates or resets (cp:221-222, 228-229). The firmware reads it back
  as HANDSHAKE_OUT b7 (cp:258); the SoftIEC target then skips its own C64 stop (intf:208-211;
  softiec_target.cc:366-368).
- **Reset.** Only the FPGA system reset clears the block (cp:292-306), which in TRX64 is `Machine::new` and a power
  cycle. A C64 warm or cold reset leaves the block alone and ERROR survives it. The firmware hears about the reset
  through ITU low bit 7 (§3).

### 2.6 Path to the application

- A 2048-byte dual-port BRAM, one port for the C64-side logic and one for the firmware (command_interface.vhd:74-91).
  Firmware registers at 0x10044000 (iomap.h:16). Buffers (pkg:33-41; intf:50-53): command 0x10044800 (896 bytes),
  response 0x10044B80 (896), status 0x10044F00 (256). Register table: `docs/hw/11-drives-iec-periph.md` §UltiCommand,
  which Spec 852 ports exactly, quirks included (STATUS_LENGTH reads the pointer's low byte; RESPONSE_LEN_L/H read
  the pointer; writes to +4/+5 are mask set/clear while reads return the buffer bounds).
- The window decodes as `command_interface.vhd` splits it: 0x000-0x7FF the sixteen registers, repeating every 16
  bytes, 0x800-0xFFF the RAM, above that 0.
- **Firmware IRQ:** ITU low bit 4 (0x10) = (status b2:0 AND NOT irq_mask) != 0, as a level (cp:310; itu.h:37). The ISR
  masks the active sources and queues them (intf:85-96); the "UCI Server" task handles abort, data accepted and new
  command (intf:111-186) and writes replies with `copy_result` (intf:188-206).

## 3. What UE2 builds

### 3.1 The host interface (`ue2-core/src/c64host.rs`)

`C64Backend` gains five defaulted methods and one small type, so backends that have no UCI — and the test mock by
default — keep compiling:

```rust
pub struct UciEvents { pub c64_reset: bool, pub unlock: bool }

fn has_uci(&self) -> bool { false }
fn uci_read(&self, off: u16) -> u8 { 0 }      // side-effect free: TRX64's fw_read takes &self
fn uci_write(&mut self, off: u16, val: u8) {}
fn uci_irq(&self) -> bool { false }            // ITU low bit 4, a level
fn uci_take_events(&mut self) -> UciEvents { UciEvents::default() }
```

`uci_read` is side-effect free because the firmware side has no read with a side effect in the VHDL, which is why
TRX64 can give it `&self` — so `C64Port::peek8` is the same call.

### 3.2 The window (`ue2-core/src/devices/c64.rs`)

`0x10044000-0x10044FFF` is a `C64Port` window (offset `0x4_4000`, size `0x1000`), mapped through `WINDOWS` /
`IoMap::map_origin` like the other nine.

- With a UCI behind the backend: reads, writes and peeks go to it. Reads and writes first advance the C64 to the
  accessing instruction's clock, as the cart registers do, because the block is C64-side state.
- Without one: exactly the T0 behaviour of before. The `UCI` register table and its test `a5_uci_buffer_bases` moved
  from `devices/iec.rs` into `devices/c64.rs` as `UCI_T0`, and `iec.rs` no longer maps the window — otherwise the two
  would overlap.
- `C64Port::reset` (the emulator's power-on) resets `UCI_T0`; it does not reset the backend's block, which only the
  FPGA system reset clears.

### 3.3 The ITU bits

| Bit | Kind | Source | Where |
|---|---|---|---|
| low 4 | level | `uci_irq()` | recomputed after every UCI access, after every backend sync, and in `tick` |
| low 7 | edge (`pulse`) | `uci_take_events().c64_reset` | same points; `IrqState::pulse` latches the edge flag |
| high 6 | level (`set_high`) | `uci_take_events().unlock` | same points; dropped by the handler's ack, below |

Low bit 4 is a level because the open VHDL is (cp:310), which closes S15's old open question 6 for the open side; the
U64-II top is still closed. Low bit 7 is an edge in the ITU's fixed edge mask 0x85 (docs/hw/02 §Low IRQ byte).

High IRQ 6 has no ack register in the ITU (docs/hw/02 H8), so a stuck level would storm the ISR. `unlock_irq` acks it
by writing `C64_POKE(0xD038, 0)` (u64_config.cc:1012-1014), which reaches the emulator as a DMA write; `C64Port`
drops the source on exactly that write. This follows `devices/wifi.rs`, whose high bit 3 drops when the ISR drains
the source.

### 3.4 The bridge (`c64-bridge/src/lib.rs`)

- **Profile.** `Machine::set_machine_profile(vic::SpeedProfile::U64)` right after `Machine::new()`, before the
  power-on reset — a cartridge probes in its boot stub, so a claim set afterwards arrives too late (Spec 851 §2 D1).
  The UCI block comes with the profile; nothing is attached from outside.
- **The five methods** go to `Machine::uci()` / `uci_mut()` → `Uci::{fw_read, fw_write, fw_irq, take_events}`.
  `uci_mut` hands the block any C64 reset since the last call first, so `take_events` reports it.
- **Routing.** `Uci::set_routed(io1, io2)` from C64_BUS_INTERNAL (core config `0x2B`) bit 0 and bit 1. The bridge
  already models C64_BUS_INTERNAL/EXTERNAL for `--cart-slot`, and it is the U64's bus multiplexer, not block state, so
  it stays here (Spec 852 §5). Standalone TRX64 routes both.
- **Turbo.** C64_TURBOREGS_EN (`0x02`) and C64_SPEED_PREFER (`0x2D`) are latched; the C64_SPEED_UPDATE strobe (`0x2E`)
  applies them with `Machine::set_u64_turbo(regs_en, speed_prefer)`, which is the order `setCpuSpeed` writes them in
  (u64_config.cc:1634-1636). The latches start at the profile's own defaults, so a strobe before any write is a no-op.
- **Interrupts.** The cartridges' IRQ and NMI move from `INT_SRC_RESTORE` to `Machine::set_expansion_lines`
  (Spec 850 D6), where the UCI block's own IRQ is ORed in. The host RESTORE NMI stays on `INT_SRC_RESTORE`, which is
  RESTORE again.
- **Hold.** The bridge's own `run_chips` loop is gone. `Machine::set_hold` takes `Hold::Cpu` for C64_STOP and
  `Hold::Reset` for the held reset line; the reset wins over the stop. TRX64 then runs the chips itself
  (Spec 850 D7). Two things stay on the bridge side: the Epyx capacitor hold time, and drive A — `Hold::Reset` leaves
  the drive standing, while the U64's drive registers decide for themselves through RESET bit 1 (`use_c64_reset`), so
  the bridge clocks it from the reference the hold skipped.
- **Capability.** `CAPAB_COMMAND_INTF` = 0x00040000 (itu.h:67), next to `CAPAB_EEPROM` in `cart.rs`.

### 3.5 ue2emu

`runner::attach_trx64_audio` ORs `CAPAB_COMMAND_INTF` into the ITU capabilities when the attached backend reports
`has_uci()`. Without it the firmware starts no "UCI Server" task (intf:44) and skips every enable write
(c64.cc:311, 328, 1306). `--c64 none` changes nothing: the capability word stays 0x34000222.

## 4. What is deliberately not built

- A ue2-core model of `command_protocol.vhd` (the old §3 item 1) — Spec 852 is that model, inside TRX64.
- A bridge window decode and a fake `CartProxy` for the no-cartridge case (the old §4 steps 1 and 2). Spec 850's
  device interface replaced the need; the polling-only and watch-table stages never existed.
- An UCI server inside TRX64. The targets are firmware and run unmodified here.

## 5. Acceptance

1. The firmware reports capability bit 18 and starts its "UCI Server" task, and the menu offers "Command Interface".
2. `cargo build --workspace` and `cargo test --workspace` are clean, and `--c64 none` is byte-for-byte the T0
   behaviour of before (the moved `a5_uci_buffer_bases` covers the register table).
3. The C64 smokes of `docs/status/c64.md` stay green — the interrupt and hold changes touch all of them.
4. The upstream end-to-end suite `uci-targets`
   (FW/tests/e2e/io/command_interface/uci_targets_test.py) drives `$DF1B-$DF1F` over REST
   (machine:readmem / machine:writemem), so no 6502 code is involved: the transport state machine, the control
   target, issue #740's LOAD_REU / SAVE_REU, and SoftIEC single-part reply framing.

Results: `docs/status/c64.md` §UCI.

## 6. Open questions

Questions 5, 6, 8 and 9 of the old spec are answered (freeze is an instruction-boundary hold, ITU bit 4 is a level, a
stretched read advances `stalled_on_bus + 1` times, and the state lives in TRX64). What is left:

1. **Gating.** Is the window gated by C64_BUS_INTERNAL bit 1 (IO2) and bit 0 (IO1)? The only direct evidence is the
   comment at u64_config.cc:1016.
2. **Both sides on IO2.** When an external cartridge also drives the window (unlock with an external cartridge, or
   Manual "Both"), does the C64 read the UCI or a wired AND? sl:292-301 is the U2+ answer inside the FPGA; the bridge
   ANDs the two today (Bridge slot.rs:1010-1016).
3. **Disabled read value.** What does the C64 read in the window on the U64-II while UCI is disabled or locked?
   megabyter.tas:334-339 only requires `$DF1D` ≠ `$C9`. TRX64 answers the open bus.
4. **Unlock rule.** What exactly may happen between `$D038=$AB` and `$D036=$CD`? megabyter.tas:342 says "nothing in
   between"; the logic is in the closed core. TRX64 re-arms on any other C64 write it is shown.
5. **C64 reset.** How is ITU low bit 7 raised on the U64-II? The open top uses `irq_in(7) => c64_reset_in`
   (ultimate_logic_32.vhd:531). Does anything besides the system reset clear enabled, ERROR or bus_id?
6. **Unlock ack.** `C64_POKE(0xD038, 0)` is taken as the ack for high IRQ 6 because it is what the handler does and
   the ITU has no ack register. Whether the real source is the write or something else in the closed core is unknown.
   TRX64 confirmed its own side (2026-09-16): both events are one-shot — `take_events` drains the struct
   (TRX `uci.rs:435`) and nothing re-raises it, and the only level it computes is the command handshake
   (`uci.rs:428-430`). The ITU level, and dropping it on the `$D038` write, are UE2's policy, not hardware.
