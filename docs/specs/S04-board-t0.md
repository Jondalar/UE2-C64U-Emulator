# S04 — Board T0 models

**Owns:** `crates/ue2-core/src/devices/{board.rs,i2c.rs,c64.rs,usb.rs,drives.rs,iec.rs,misc.rs,rmii.rs}`
**Reads:**
- `docs/hw/00-memory-map.md` §1b (T0 column), §1c, §2 (all hazards) and §3 C4, C11, C12, C14
- Per block: `03-board-init.md`, `09-usb.md`, `10-c64-machine.md`, `11-drives-iec-periph.md`, `12-gaps.md`, `07-sd-card-filesystems.md` (RTC only), `08-network-rmii.md` (T0 only)

## Scope

Implement the **T0 column** of 00-memory-map §1b for every IO window **not** owned by another spec. Owned
by others: ITU (S03), SD `0x10060000` (S09), flash `0x10060200` (S06), WiFi `0x10060900` (S05),
U64 IO page `0x10100400` (S07), overlay `0x10140000-0x1014FFFF` (S07).

Map each window at 256-byte grain. Use a small table-driven helper (constant / latch / RAM / RAZ-WI per
offset) where it keeps things readable. Every non-trivial behaviour gets a comment citing the hazard ID
(e.g. `// 00 §2 C12: STOP bit1 follows bit0 immediately`).

| Module | Windows (examples, see §1b for exact extents) | Key T0 behaviour |
|---|---|---|
| `board.rs` | U2PIO `0x10100000`, DDR2 PHY `0x10100100`, CLOCKMEAS `0x10100200`, MATRIX_KEYB `0x10100300`, mixers `0x10100500`, LED strip `0x10100600`, Blingboard `0x10100800/0900`, MMCM `0x10200000` | BOARDREV `0x1010000C` reads 0xB8, independent of writes (M1). GET_MDIO 0. HUB/ULPI reset latches. MATRIX_KEYB RAM incl. 32-bit write at `0x1010030B`. |
| `i2c.rs` | HW I2C `0x10100700` | Status `0x10100701` bit7 = 0; 0x700/0x704/0x705 read 0xFF; writes ignored (NACK/ACK both boot). Structured so T1 can add devices later. |
| `c64.rs` | cart/machine `0x10040000`, legacy SID `0x10042000`, CART_TIMING `0x10046000`, sampler `0x10048000`, EEPROM `0x1004C000`, DMA window `0x10050000-0x1005FFFF`, core config `0x10180000`, palette `0x10180800`, PLD `0x10181000`, debug `0x10181800`, glyph `0x10182000`, SID `0x10184000-0x10185FFF`, ROM windows `0x10188000-0x1018CFFF`, UDP `0x10190000` | Latches with read-back. STOP R = req\|req<<1 (C12, C17). CLOCK_DETECT reads 0x01 (C14). CART_DETECT is in S07. DMA window: 64 K array; `$D012` reads a per-read counter reaching 0xFF (C16); `$DC00/$DC01` 0xFF (C18); `$D019` 0; SID `$D400-$D7FF` 0. ROM windows are RAM with read-back (M8). CORE_VERSION constant. |
| `usb.rs` | nano `0x10080000-0x100807FF` + NANO_START `0x10080800-0x10080FFF` | RAM with 8/16/32-bit writes; never raise ITU bit 2; HEAD/TAIL stable (C31, B5). |
| `drives.rs` | drive A `0x10020000-0x10023FFF`, drive B `0x10024000-0x10027FFF` | RAZ/WI, but the WD177x status `0x10021806`/`0x10025806` = 0, DIRTY area reads 0 (not RAM), `0x1002000D` read-back (C28). |
| `iec.rs` | IEC `0x10028000-0x10028FFF`, UCI `0x10044000-0x10044FFF`, ACIA `0x1004A000-0x1004AFFF`, tape play `0x100A0000-0x100A0FFF`, tape record `0x100C0000-0x100C0FFF` | IEC R0=0x25, R1=0x01, R2=0x01; code RAM; stray `0x100287FF` write. UCI constants (+4..+9), RAM 0x800-0xFFF. ACIA latches + ring RAM. Tape play status 0x80, record 0x00. |
| `misc.rs` | RTC `0x10060100`, TRACE `0x10060300`, RTC timer `0x10060400` (host UTC epoch, LE32), GCR `0x10060500`, ICAP `0x10060600`, audio select `0x10060700` | As §1b T0. |
| `rmii.rs` | RMII MAC `0x10060800` | T0 RAZ/WI with TX_BUSY = 0 and ALLOC_VALID = 0, no IRQ (S12 replaces this later). |

Each module exposes `install(map, cfg)`; `devices::install_all` already calls them.

## Tests

One unit test per boot hazard implemented, named after its ID:
- `c12_stop_ack_immediate`, `c16_d012_reaches_ff`, `c18_dc01_stable`, `m1_boardrev_independent`
- `b3_i2c_not_busy`, `c28_wd177x_idle`, `c22_iec_registers`, `c31_usb_ram_widths`
- `c14_clock_detect`, `m8_rom_window_readback`, `c21_rtc_epoch`

## Acceptance

`cargo test -p ue2-core devices` passes. No window overlaps any other spec's window: `IoMap::map` asserts
on overlap, and a test calls `devices::install_all` with all modules.
