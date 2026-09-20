# The monitor (S23)

TRX64's monitor runs against UE2's C64. The verbs are TRX64's own, from `trx64-monitor` (their Spec 864); UE2 is
its second host. Design and the division of labour: `docs/specs/S23-monitor.md`.

**M1 works.** `crates/ue2emu/src/monitor.rs` implements `MonitorHost`:

- `machine()` hands over the TRX64 machine. `ue2-core` still knows nothing about TRX64: `C64Backend` grew a
  `as_any_mut` hook, `C64Port` a `backend_mut`, and the bridge a `trx64()` accessor, so the frontend downcasts and
  the core stays ignorant (that is what keeps `--c64 none` honest).
- `cpu(Device::Host("fw"))` is the firmware's RISC-V: 32-bit addresses, `pc` and the 32 ABI-named registers,
  memory through the same `peek8` the GDB stub reads with, DDR writable and its IO not. No flag register, no
  disassembler, no debug gates — the watch tables are a 6502 shape.
- The marked address spans (TRX64 Spec 804) are stripped on the way out, so the control protocol carries what a
  reader wants and the library keeps the marked form.

**Surfaces.** `monitor <cmd>` on the control port (S08), and `emu_monitor` over MCP on top of it.

**M2 has started.** The library sees every line first and returns `None` for what it does not own; our dispatch
takes it from there and appends its own section to `help`. TRX64 gains no notion of this emulator — the dependency
stays one-way. Done: `fw` (the RISC-V registers, `fw tasks` for the FreeRTOS list) and `clock` (the emulator's
clock, the firmware's instructions, the C64's cycle). Still to come: `itu`, `cart`, `flash`, `sd`, `usb`, `net`,
`audio` and `config`.

**Not yet:** run control — `g`, `step`, breakpoints (M3). Until then the library's own sentence answers: "run
control is not available in this host".

### Verified

| Check | Result |
|---|---|
| `cargo test --workspace` | green (5 new in `ue2emu::monitor`, 82 in ue2emu) |
| `cargo clippy` on the changed crates | no new warnings |
| Booted 3.15, `monitor r` over the control port | the register panel with the flow line, `.;e5cd 00 00 0a f3 nv-bdiZc  MAIN`, the port and vector lines |
| `monitor m 0400 0407` | `>C:0400  20 20 …`, screen RAM |
| `monitor d e000 e001` | `$e000  85 56     STA $56` |
| `monitor wr` then `m` | the bytes read back |
| `monitor nonsense` | `unknown monitor command 'nonsense'` — our dispatch takes what the library declines |
| `monitor device` on a booted 3.15 | `device: c64   (c64 \| drive8 \| fw — anything but c64 is read-inspect r/m/d)`; `device fw` selects it |
| `monitor fw` | the 32 registers with `pc  00035da8  prvIdleTask+0x2c` |
| `monitor clock` | the emulator's ms and clocks, the firmware's instructions and idle skips, the C64's cycle |
| TRX64 repin 0.7.3 → 0.8.2 | no code change needed; `smoke-all.sh` 7/7, `smoke-c64-carts.ctl` 27/27 with the freeze and both SID loads, UltimateDemo2026 detection all `[ OK ]` |

### Open

- `r`/`m`/`d` still read the C64 after `device fw`: the library selects the device but does not route those verbs
  through `CpuView` yet (TRX64 is wiring that next; the `device` validation itself was fixed in v0.8.2 after we
  reported it).
- The verbs of the second half of the extraction (`g`, `until`, `step`, `bk` hits) answer through our fall-through
  until they land upstream.
