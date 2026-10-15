# S42 — A rocker press on the ESP32 reaches the ITU buttons and the C64 reset through the WiFi UART, as on the board

**Status:** DONE (2026-10-15): S1-S10 green — the unit tests in `cargo test`, S1 and S9 in
`scripts/smoke-trx-rocker.sh` with trx64/main. Local branch `s37-trx-pixel-layer` only, like S37-S41.
**Repo:** UE2 (u64-emulator). Hardware reference: TRX64-Ultimate `designs/c64_basic/ultimate_sys.vhd` (f7fb900) and
TRX64U-MultiCore `host/ultimate_sys.vhd` (f512c3d), identical in this part.
**Origin:** TRX64-MC session (TRX64U-MultiCore), 2026-10-15: "ue2emu is missing the ESP32 rocker event decode, which
is the one thing that would let us test the Freeze/Menu button without the board." Owner: "ja".
**Depends on:** S05 (WiFi DMA UART + u64ctrl stub), S41 (core dev mode, for the reset path into an absent core).

---

## §1 The problem

**Who:** the core-application developers (TRX64-FW, TRX64-MC) and the owner at the workbench without a board.

**What they are trying to do:** press the C64U's rocker and see their app open its menu (up short), freeze
(down short) or reset the C64 (up long), and prove the chain module → UART → FPGA → app works before anyone presses
the real rocker.

**What happens on the board.** The ESP32 owns the rocker (`software/u64ctrl/main/button_handler.c`). Each gesture is
sent as one SLIP packet holding a 4-byte RPC header whose command byte is the event (`rpc_dispatch.c:389-402`):

| Gesture (`button_handler.h`) | Code | FPGA (`ultimate_sys.vhd:772-795`, `:1320`) |
|---|---|---|
| UP_SHORT (1) | 0x82 EVENT_MENU | ITU button 1 (0x1000000A bit 6) high for 200 ms |
| UP_LONG (2) | 0x83 EVENT_RESET | C64 reset held for 200 ms, ORed with C64_MODE's reset; C64_MODE reads unchanged |
| DOWN_SHORT (3) | 0x81 EVENT_FREEZE | ITU button 2 (bit 7) high for 200 ms |
| DOWN_LONG (4) | 0x80 EVENT_BUTTON | decoded, then nothing (`when others => null`) |

`uart_dma` runs with `g_events => true`. `rx_dma` looks at the first byte of each packet while it is idle or has just
latched an address (`rx_dma.vhd:79-104`): bit 7 set makes it an event, the packet is dropped and never reaches RAM.
In `idle` it peeks without an armed RX buffer, so an event at the head of the stream is decoded even when the app has
armed nothing. A data packet at the head blocks until a buffer is armed, and events behind it wait. ITU button 0 is
tied to 0; there is no other path to buttons 1 and 2 on this board.

**What happens in UE2 today:** `wifi.rs` writes every ESP32 frame into RAM; nothing decodes bit 7. The ITU button
register only knows bit 6, driven straight from F12 and the control command `button [ms]` (`itu.rs:148`,
`host.rs:16`), which no board does. There is no way to send a rocker gesture.

**Why it matters:** TRX64-MC rebuilt its app's ESP32 bring-up on `DmaUART` (including `SetBaudRate(115200)`) and
cannot show it works without the owner at the board. A wrong rx-buffer handshake looks the same as a dead rocker.

**Parts (MECE):**
- **P1 Send:** the host can make the ESP32 send a rocker gesture.
- **P2 Decode:** the WiFi UART turns a bit-7 first byte into an event and keeps it out of RAM, with the board's rules
  for when it is looked at.
- **P3 Effect:** the event drives ITU buttons 1 and 2 or the C64 reset for 200 ms.
- **P4 Data path unchanged:** frames without bit 7 still land in RAM exactly as today.
- **P5 Without the board's decode:** the normal C64U, whose top level is unknown, behaves as before, and what cannot
  happen there is refused by name.

## §2 Scenarios

```gherkin
Scenario: the app gets the rocker (S1, P1+P3)
  Given UE2 runs trx64/main with --trx-io and the menu on the overlay, the C64 at READY. with "HELLO" typed
  When  `rocker up-long` is sent, then `rocker up`
  Then  the C64 screen shows READY. without HELLO, then the menu shows DISK FILE BROWSER

Scenario: an event is decoded with no RX buffer armed (S2, P2)
  Given the app has armed no RX buffer
  When  the ESP32 sends [0x82, 0xFF, 0x00, 0x00]
  Then  the menu event fires at once
  And   the RX status still says "needs address" and no length is reported

Scenario: an event waits behind a data frame (S3, P2)
  Given a data frame from the ESP32 is waiting for an RX buffer
  When  an event frame follows it
  Then  the event fires only after the app has armed a buffer and the data frame was written

Scenario: down short holds the freeze button (S4, P3)
  When  `rocker down` is sent
  Then  ITU button register bit 7 reads 1 for 200 ms, then 0

Scenario: up long resets the C64 for 200 ms (S5, P3)
  Given the C64 is running and C64_MODE has its reset clear
  When  `rocker up-long` is sent
  Then  the C64 is held in reset for 200 ms and then boots, the cartridge rebuilt first
  And   C64_MODE reads back without the reset bit; C64_CLOCK_DETECT senses the reset

Scenario: down long does nothing (S6, P2+P3)
  When  `rocker down-long` is sent
  Then  the frame does not reach RAM and no button or reset changes

Scenario: a second press restarts the 200 ms (S7, P3)
  When  `rocker up` is sent, and again 150 ms later
  Then  bit 6 stays 1 until 200 ms after the second press

Scenario: data frames are untouched (S8, P4)
  When  u64ctrl answers IDENTIFY and other RPCs, with and without the decode
  Then  every existing wifi.rs test passes unchanged, and IDENTIFY round-trips with the decode on

Scenario: the address waits for a data frame (S10, P2)
  Given the app has armed an RX buffer and the decode is on
  When  an event arrives, then a data frame
  Then  the status keeps "has an address" through the event; the data frame's first byte takes it

Scenario: refused where the board cannot do it (S9, P5)
  When  `rocker up` runs without --trx-io, or `button 30` with it
  Then  the script fails naming --trx-io, or the board's 200 ms hold
  And   without --trx-io a frame starting 0x82 is data, and F12 sets bit 6 directly
```

| # | Solves | Scenario | Test | Where it runs |
|---|---|---|---|---|
| S1 | P1, P3 | the app gets the rocker | `scripts/smoke-trx-rocker.sh` run `rocker` | by hand, needs the TRX app ELF (not checked in) |
| S2 | P2 | event without a buffer | `wifi::tests::event_needs_no_rx_buffer` | `cargo test` |
| S3 | P2 | event behind a data frame | `wifi::tests::event_waits_behind_a_data_frame` | `cargo test` |
| S4 | P3 | freeze button | `machine::tests::rocker_events_hold_the_itu_buttons_200_ms` | `cargo test` |
| S5 | P3 | C64 reset 200 ms | `machine::tests::rocker_up_long_holds_the_c64_in_reset_200_ms` | `cargo test` |
| S6 | P2 | down long and 0x90 dropped | `wifi::tests::event_0x80_is_dropped` | `cargo test` |
| S7 | P3 | re-press restarts | `machine::tests::rocker_events_hold_the_itu_buttons_200_ms`, `wifi::tests::a_new_event_restarts_the_hold` | `cargo test` |
| S8 | P4 | data path unchanged | `wifi::tests` (all), `rpcs_still_land_in_ram_with_events` | `cargo test` |
| S9 | P5 | refusals, no decode | `control::tests::trx_button_and_rocker_are_esp32_events`, `machine::tests::without_the_trx_block_the_rocker_is_dropped_and_the_button_is_direct`, `wifi::tests::without_events_a_bit_7_frame_is_data`; smoke runs `off`, `button` | `cargo test` / by hand |
| S10 | P2 | address waits | `wifi::tests::the_address_waits_for_a_data_frame` | `cargo test` |

## §3 Decision

- **D1 Decode in `Wifi`.** `service_rx` checks the head frame before the buffer test: due, first byte bit 7 →
  pop it and raise the event; never written, no length. Otherwise as today. Matches `rx_dma` `idle`/`first`.
- **D2 One set of holds for the board.** `wifi::EspButtons` keeps the three holds as clocks (atomics in an `Arc`),
  shared by `Wifi` (writes), `Itu` (reads buttons 1 and 2) and `C64Port` (pulls the reset); `devices::install_all`
  wires them with `--trx-io`. A new event reloads its hold to 200 ms (`ESP_EVENT_HOLD`).
- **D3 ITU bits.** `BUTTONS` reads `button2 << 7 | button1 << 6`; bit 5 stays 0.
- **D4 C64 reset.** The ESP reset is ORed into the C64's reset line beside C64_MODE's (`C64Port::reset_held`);
  C64_MODE's read value does not include it (`page_data` uses `c64_reset_i` only), the reset sense does
  (`core_reset_req` → `c64_reset_sense`, top.v:562): C64_CLOCK_DETECT b4, and ITU low bit 7 through the backend's
  reset event as for every reset. `C64Port` applies the edges at its tick, at the hold's clocks (`next_event`).
- **D5 Entry points.** `HostInput::Rocker(host::Rocker)` → `Wifi::rocker` queues `[code, 0xFF, 0, 0]` as an ESP32
  frame, due at once. Control command `rocker up|up-long|down|down-long`, which returns after the 200 ms hold. For
  other code: `Machine::input(HostInput::Rocker(..))`.
- **D6 `--trx-io` only** (owner, 2026-10-15). There the wiring is read from `ultimate_sys.vhd`; the closed U64-II
  top level is unknown (doc 04 open questions 1-2), so the normal C64U keeps today's behaviour: no decode, frames
  land in RAM, `rocker` is refused.
- **D7 F12 and `button` are the rocker's up short in `--trx-io`** (owner, 2026-10-15): the board has no direct menu
  line, so both send 0x82 and the button is held the board's 200 ms; `button`'s `ms` is refused there. The TRX smokes
  then run through the new path. The normal C64U keeps the direct bit 6.

## §4 Out

- ESP32 events other than 0x80-0x83 (EVENT_JOYSTICK 0x90 and others with bit 7): decoded and dropped like 0x80, as the
  board does; nothing more.
- Power gestures (ON, OFF, ON2): the regulators are not modelled.
- Byte-level SLIP timing: frames stay whole, as in S05.

## §5 Decisions taken while building

- **B1 The address waits in the register with `g_events`.** `rx_dma` latches it in `first` but acknowledges it only
  with a data byte, so until a data frame arrives the status says it has an address and the ISR arms no second one.
  The model takes it only when a data frame is at the head (`service_rx`); without `g_events` it stays the two-slot
  model (H11).
- **B2 The control client asks the machine.** `button` and `rocker` depend on `--trx-io`, which `Machine::new`
  resolves (auto); `Target::trx_io` asks the emulation thread (`Command::TrxIo`), so a script sees the resolved mode.
- **B3 F12 in `--trx-io`:** the press sends `rocker up`, the release does nothing (`Machine::input`).
- **B4 The holds survive the FPGA reset** (`Wifi::reset`, `C64Port::reset`, `power_up`): the top level's counters have
  no reset term.
- **B5 The smoke** uses `smoke-trx-overlay.cfg` (menu on the overlay) so `expect` sees DISK FILE BROWSER, and
  `type hello`: `type HELLO` types shifted letters, PETSCII graphics.
