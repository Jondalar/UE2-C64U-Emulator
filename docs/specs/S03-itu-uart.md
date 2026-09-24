# S03 — ITU, UART, capabilities, IRQ core

**Status:** built.

**Owns:** `crates/ue2-core/src/devices/itu.rs`, `crates/ue2-core/src/irq.rs` (internals; keep public method names)
**Reads:** `docs/hw/02-itu-uart.md` (all), `docs/hw/00-memory-map.md` §1b rows 0x10000000-0x1000003F, §2 B1, B4, B9, C1, C2, §3 C2, C12, §Interrupts; `firmware/1541ultimate/fpga/io/itu/vhdl_source/itu.vhd`

## Scope

- `Itu` device on window `0x10000000-0x100000FF`, decode `addr & 0x3F`. Implement exactly what doc 02 T0 +
  the T1 IRQ core describe:
  - **IRQ registers:** GLOBAL, ENABLE, DISABLE, EDGE, CLEAR, ACTIVE (read) operate on `ctx.irq`. HIGH_EN
    (0x27) latches with read-back; HIGH_ACT (0x28) = `irq.high_active()`.
  - **Timers:** `ITU_TIMER` (0x06) counts down 1 per 500 clocks (5 µs) to 0 and holds. The IRQ timer
    (0x07 enable, 0x08/0x09 reload) raises edge bit 0 every `(reload+1)*256` clocks while enabled
    (0x7A0 → 499 968), expressed through `next_event`/`tick`; the phase resets when it is enabled.
  - **ms timer:** 0x22/0x23 = `(now / 100_000) & 0xFFFF` (HI/LO as documented), stable between reads.
  - **Capabilities:** 0x0C-0x0F = `cfg.capabilities` big-endian. FPGA_VERSION: constant from doc 02.
  - **Menu button:** 0x0A bit 6 from `set_menu_button(pressed)`.
  - Busy LEDs 0x24/0x25/0x29 and MISC 0x26: accept writes.
  - Other offsets: read 0.
- **UART** at 0x10-0x13: DATA write → `ctx.console.push(byte)`; FLAGS read 0x40; GET/ICTRL accept writes.
  0x14-0x1F aliases per doc 02; 0x1F receives the early-trap marker 0x49 — push `"\n[early trap]\n"` to
  the console.
- `irq.rs`: verify `IrqState` against itu.vhd (flag latching while masked, EDGE register writes if
  `g_edge_write`, clear semantics) and correct the internals. Keep the edge-mask default 0x85
  (00-memory-map §3 C2).
- `install(map, cfg)`: add `Itu::new(cfg.capabilities)`.

## Tests

- Program the ITU exactly like `riscv_main.c:178-186` (doc 00 B9). The first tick edge comes after 499 968
  clocks, then periodically. ACTIVE shows bit 0, CLEAR clears it, `line()` follows GLOBAL and mask.
- `wait_ms(2)` pattern: write 200 to 0x06, poll → 0 after 200 × 500 clocks.
- ms timer HI/LO consistent; capabilities byte order; FLAGS = 0x40; DATA reaches the console.
- HIGH_EN read-modify-write; HIGH_ACT = high_src & high_en; menu button bit 6.

## Acceptance

`cargo test -p ue2-core itu` passes.
