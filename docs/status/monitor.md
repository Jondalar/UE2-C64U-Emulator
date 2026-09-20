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

**Not yet:** our own Ultimate verbs including `config` (M2), and run control — `g`, `step`, breakpoints (M3).
Until then the library's own sentence answers: "run control is not available in this host".

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
| TRX64 repin 0.7.3 → 0.8.1 | no code change needed; `smoke-all.sh` 7/7, `smoke-c64-carts.ctl` 27/27 with the freeze and both SID loads, UltimateDemo2026 detection all `[ OK ]` |

### Open

- `device fw` is unreachable for now: the library's `device` verb validates against a fixed `c64|drive8` instead of
  `host.devices()` (reported to TRX64, their `verbs.rs:845`). The view is tested directly in the meantime.
- The verbs of the second half of the extraction (`g`, `until`, `step`, `bk` hits) answer through our fall-through
  until they land upstream.
