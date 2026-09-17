# S18 — WiFi: the u64ctrl model joins a network

S05 built the ESP32 link and a stub control module (doc 04 tier T0): the firmware identifies the module, reads the
voltages and power settings, and settles in "Link Down". S18 is doc 04 tier T1 for the network half: a virtual
access point, the module's connect state machine, and Ethernet frames bridged to the same host backends the wired
interface uses (`--net`, S12).

The ESP32-S3 is not emulated. The firmware only sees the SLIP/RPC protocol, the module's side is open source, and the
module runs no TCP/IP: it forwards raw Ethernet frames, and lwIP, DHCP, Telnet, FTP and REST all run on the Ultimate
CPU. A functional model of `u64ctrl` is therefore enough.

References: **FW** = `firmware/1541ultimate/software`. `wifi.cc` = FW/io/wifi/wifi.cc; `net_esp` =
FW/io/network/network_esp32.cc; `modem` = FW/u64ctrl/main/wifi_modem.c; `dispatch` = FW/u64ctrl/main/rpc_dispatch.c;
`rpc` = FW/u64ctrl/main/rpc_calls.h; **doc 04** = `docs/hw/04-esp32-wifi.md`, which has the full command and event
tables.

## 1. The module, as the firmware drives it

### 1.1 Connector

The module keeps its WiFi state in a connector task (`modem:849-1003`). States: LastAP, Scanning, ScannedAPs,
StoredAPs, Connected, Disconnected, Disabled. It starts in LastAP when the module boots, which is before the FPGA loads.

- **LastAP** connects to the AP stored as "last". Failing that, **ScannedAPs** tries every scanned AP that is in
  the store, and **StoredAPs** tries all 16 stored slots (`modem:880-942`). A success stores the slot as last.
- **Disconnected** retries from LastAP after 5 s, doubling up to 60 s; Connected resets the delay
  (`modem:52-53,958-972`). No retry happens after a disconnect the user asked for (`user_disconnected`,
  `modem:871-877`), until the next CONNECT or AUTOCONNECT clears it (`modem:810,817`).
- **Disabled** waits for a command (`modem:989-993`).

The AP store is NVS: 16 slots of SSID, password and auth mode, plus a "last" index (`modem:457-528`). Only a
successful CONNECT writes a slot (`modem:829-838`).

### 1.2 Events

The ESP-IDF event handler turns association into events at once (`modem:273-308`):

- associated → `EVENT_CONNECTED` with the SSID of the last attempt (`modem:250-254`);
- disassociated, or an attempt failed → `EVENT_DISCONNECTED`.

`EVENT_DISABLED` comes from DISABLE (`modem:768`), `EVENT_GOTIP` from the module's own DHCP (`modem:210-239`).

### 1.3 Commands with a WiFi effect

Replies echo the header (doc 04 §RPC protocol). "Later" means the connector does it after the reply.

| Command | Reply | Effect |
|---|---|---|
| WIFI_SCAN 0x04 | from the connector when the scan is done: `esp_err`, `num_records`, 42-byte records, size `10 + n·42` (`modem:792-805`) | a Disabled radio is started first, with `EVENT_DISCONNECTED` (`modem:731-745`); the connector state stays Disabled |
| WIFI_CONNECT 0x05 | `esp_err` 0x102 for a request under 98 bytes (`dispatch:100-104`); otherwise after the attempt has started | clears `user_disconnected`; starts a Disabled radio; if associated, disassociates first (`EVENT_DISCONNECTED`, `modem:532-548`). Auth modes above 7 become 7 (`modem:553-556`). Later: `EVENT_CONNECTED` and the slot stored as last, or `EVENT_DISCONNECTED` (`modem:816-842`) |
| WIFI_AUTOCONNECT 0x12 | `ESP_OK` at once (`dispatch:123-142`) | clears `user_disconnected`, starts a Disabled radio, then LastAP. An associated module disassociates and reconnects |
| WIFI_DISCONNECT 0x06 | result of `esp_wifi_disconnect` | sets `user_disconnected`; `EVENT_DISCONNECTED` if associated (`dispatch:144-154`) |
| CLEAR_APS 0x11 | erase result, then `esp_wifi_disconnect` | erases all slots and last; disassociates without setting `user_disconnected` (`dispatch:261-270`) |
| WIFI_DISABLE 0x0E | `ESP_OK` at once (`dispatch:205-225`) | later: disassociate (`EVENT_DISCONNECTED`), stop the radio, `EVENT_DISABLED` (`modem:754-775`) |
| WIFI_ENABLE 0x0D | `ESP_OK` at once | later, only from Disabled: start the radio, `EVENT_DISCONNECTED`, LastAP (`modem:776-791`) |
| WIFI_IS_CONNECTED 0x0B | `status` = associated | then `EVENT_CONNECTED` if associated, else `EVENT_DISABLED` if the radio is off (`dispatch:239-249`, `modem:264-271`) |
| MODEM_ON / OFF 0x09 / 0x0A | `ESP_OK` | installs or removes the frame hook (`dispatch:187-203`, `modem:310-326`) |
| SEND_PACKET 0x08 | none | `length u32 @4, data @8` goes out on the station interface (`dispatch:158-163`, `rpc:161-165`) |

`esp_wifi_disconnect` on a stopped radio returns `ESP_ERR_WIFI_NOT_STARTED` (0x3002, ESP-IDF), which the firmware
shows as a failed "Forget APs" (`net_esp:245-246`).

### 1.4 Frames

- **To the Ultimate** only while the hook is installed: a frame whose destination is the station MAC or broadcast
  becomes `EVENT_RECV_PACKET`, `len u16 @4, data @6`, size `len + 7`. Multicast is dropped. A frame for which the
  module has no free buffer is dropped (`modem:100-128`, `rpc:132-136`).
- **From the Ultimate** only in `eWifi_Connected` (`wifi.cc:450-470`).

### 1.5 Firmware behaviour the model keeps

These look like emulator bugs and are not:

- **A connect from the AP list is reported twice.** When CONNECT returns 0, the list's Connect action posts its
  own `EVENT_CONNECTED` into the firmware's queue (`wifi.cc:441-446`), so the netif goes up before the module has
  associated. The module's own `EVENT_CONNECTED` then takes the link down and up again (`wifi.cc:318-331`).
- **Opening the AP list drops the link.** `EVENT_RESCAN` takes the link down and ends in `eWifi_NotConnected`
  (`wifi.cc:304-309,287-293`), while the module stays associated and sends no event. The network is dead until the
  next Connect. The rescan is sent when the WiFi entry opens with an empty AP list, and by "Show APs"
  (`net_esp:109-115,290-308,356-364`).
- **At boot the firmware asks, the module does not tell.** The module associates long before the Ultimate listens;
  `WIFI_IS_CONNECTED` and its trailing `EVENT_CONNECTED` are what bring the link up (`wifi.cc:269-285`).

## 2. What UE2 builds

### 2.1 The virtual access point

With a WiFi backend attached (§2.5), exactly one AP is on the air:

| Field | Value |
|---|---|
| SSID | `UE2-Emulator` |
| BSSID | `02:15:41:FF:00:01` |
| Channel, RSSI | 6, -40 |
| Auth | 3 (WPA2 PSK), password `ultimate` |

An attempt associates when the SSID matches, the requested auth mode is at most 3 (the ESP's threshold,
`modem:557`) and the password matches.

The module starts with this AP stored in slot 0 as last, like a module that was set up on this network before. It
therefore associates at boot, and the firmware comes up connected. "Forget APs" and "Connect to.." exercise the
setup path.

Without a backend nothing is on the air and the store is empty: the firmware settles in "Link Down" as today. The only
visible change is that WIFI_SCAN answers `ESP_OK` with 0 records instead of `ESP_ERR_NOT_SUPPORTED`.

### 2.2 Module state and time

`U64Ctrl` gains the connector: radio on/off, associated, hook, the 16-slot store with last, `user_disconnected`, the
retry delay, and timers. The connector's ladder collapses to one question, because one AP is on the air: is a slot
stored that associates? Stored slots always do, because only a successful connect writes one.

Delays, all emulated time:

| What | Delay |
|---|---|
| Reply to a command | `REPLY_DELAY` (1 ms, as today) |
| Association, success or failure | 300 ms after the attempt starts |
| Scan reply | 1 s |
| Retry from Disconnected | 5 s, doubling to 60 s |

A retry is scheduled only when something stored could associate: without an AP, or with an empty store, the module
sets no timer, and a run without `--wifi` costs nothing extra.

### 2.3 Replies and events in time order

`Wifi::from_esp` is kept ordered by due time, stable among equal times. Today a late frame at the front would hold
back an earlier reply queued behind it.

`U64Ctrl::handle(req, now)` returns frames with due times. `U64Ctrl::next_event()` and `advance(now)` run the timers
and return the frames they produce. `Wifi::next_event` is the earlier of the next deliverable frame and the module's
next timer. The module's timers fire even while a frame is held by a missing or unpopped RX buffer.

A frame the module produces on its own (an event, a received packet) while flowctrl SLIP (b2) is off is dropped:
nothing listens before `wifi_command_init`. This is the model's choice; what the real module does with frames sent
before the FPGA loads is not in the sources. Replies are not affected, because the firmware only sends requests
with SLIP on.

`Wifi::reset` keeps the module as today. A firmware reboot therefore finds the module still associated, with the hook
still installed, and asks again at boot.

### 2.4 Frames

- **SEND_PACKET:** `data[..length]` is queued for the host while associated, and dropped otherwise. The length is
  capped at what the request frame holds.
- **Host → module:** accepted while associated and hooked, when the destination is the station MAC or broadcast, and
  when the frame is at most 1529 bytes (the SLIP decoder's 1536, doc 04 §Link layer, minus 7). It becomes
  `EVENT_RECV_PACKET` due now, `thread 0xFF`, sequence counting up. At most 12 frames (`NUM_RX_BUFFERS`,
  `cmd_buffer.h:22`) wait for the Ultimate; a received packet that finds 12 waiting is dropped.
- `Wifi::exchange(&mut self, net: &mut dyn NetBackend, now)` is the host round trip: frames for the host go to
  `net.send`, `net.poll` delivers into the model. It always polls, so a backend never backs up while the module is
  not associated.

`EVENT_GOTIP` is not sent: the Ultimate only logs it (`wifi.cc:357-363`), and it would need a second DHCP client.

### 2.5 Host

- **`--wifi MODE`** takes the modes of `--net`: `user`, `vmnet-bridged[:IFACE]`, `socket-vmnet[:PATH]`. It conflicts
  with `--net`, so one interface is on the network.
  - `--hostfwd` and `--web-port` apply to `--wifi user` as to `--net user`.
  - The TOML key `wifi` comes from the flag (`crates/ue2emu/src/config.rs:3`).
- **With `--wifi`:**
  - `CAPAB_ETH_RMII` stays clear and the PHY stays unplugged, as without `--net`.
  - The AP goes on the air, and slot 0 is preseeded (§2.1).
  - In the bridged modes the flash unique ID is replaced as for `--net` (`net.rs:100,169-171`), so the hostname
    stays per instance. The station MAC becomes `guest_mac(uid)`, the address the wired interface would have had.
  - In `user` mode the station MAC stays `02:15:41:00:00:01`.
- **Pump:** `net::pump` exchanges with `Rmii` or `Wifi`, depending on the interface the options name
  (`runner.rs:341`).

### 2.6 Impact

`U64Ctrl::handle` (3 impacted) and `Wifi::service_rx` (23) are rated HIGH. Every caller is in `devices/wifi.rs` and
its tests: the register writes, `tick`, the flashing test and the firmware-shaped `Board`. Nothing outside the file
calls either. The frontend touches `configure`, `attach` and `pump` in `net.rs`, all LOW.

## 3. Deliberately not built

- An emulated ESP32-S3 (last section).
- More than one AP, or AP settings on the command line.
- Store persistence across runs; slot 0 is preseeded on every start.
- `EVENT_GOTIP` and the module's own DHCP and SNTP.
- WiFi and wired interfaces on the network at the same time.
- Wake-on-WiFi: it acts only while the machine is off, and the emulator has no powered-off machine.
- The button events 0x80-0x83: the firmware drops them (`wifi.cc:365-368`).
- Signal strength changes, roaming, an AP that goes away.

## 4. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean.
2. Unit tests in `devices/wifi.rs`, through the firmware-shaped `Board`:
   - boot with the AP: `WIFI_IS_CONNECTED` → status 1, then `EVENT_CONNECTED "UE2-Emulator"`;
   - boot without it: status 0, no event (as today); scan `ESP_OK`, 0 records, size 10;
   - scan with the AP: one record, size 52, after 1 s;
   - CONNECT under 98 bytes → 0x102; with the wrong password: reply 0, `EVENT_DISCONNECTED` 300 ms later; with the
     right one: `EVENT_CONNECTED`, slot stored;
   - CONNECT while associated: `EVENT_DISCONNECTED` before the reply;
   - DISCONNECT: `EVENT_DISCONNECTED`, no retry after 60 s; AUTOCONNECT reconnects;
   - CLEAR_APS: disassociates, and retries find nothing;
   - DISABLE: `EVENT_DISCONNECTED`, `EVENT_DISABLED`; then IS_CONNECTED → status 0 + `EVENT_DISABLED`; DISCONNECT →
     0x3002; ENABLE: `EVENT_DISCONNECTED`, then `EVENT_CONNECTED`;
   - SEND_PACKET reaches the backend only while associated;
   - received frames need association and the hook, pass unicast-to-station and broadcast, drop multicast, oversized
     frames and the 13th waiting frame;
   - events while SLIP is off are dropped;
   - a reply due before a queued event is delivered first.
3. Firmware in the loop, `--wifi user`:
   - the WiFi entry reads "Link Up" with 10.0.2.15;
   - REST through the web UI proxy answers, and `/v1/info` carries `wifi_mac` `02:15:41:00:00:01`;
   - Telnet on 2323 answers.
4. Menu flow on the same machine: "Forget APs" → Link Down; "Connect to.." `UE2-Emulator` / WPA2 PSK / `ultimate` →
   Link Up; "Disable" → Disabled; "Enable" → Link Up.
5. The e2e smoke profile (`docs/status/e2e.md`) with `--wifi user` in place of `--net user` gives the result of the
   wired run.
6. `--net user` unchanged: the e2e smoke profile as before; `scripts/smoke-all.sh` green.
7. Docs:
   - `docs/status/network.md`: a WiFi section;
   - `docs/status/boot.md:162`, doc 04 T1 and `docs/specs/S11-S14-later.md` §S12: WiFi L2 is built;
   - `docs/status/install.md`: the `--wifi` row;
   - `docs/ARCHITECTURE.md`.

## 5. Open questions

1. Whether the real module queues or loses frames it sends before the FPGA loads (§2.3).
2. `esp_wifi_disconnect` while not associated: the model returns `ESP_OK` without an event; ESP-IDF may post a
   disconnect event anyway.
3. The emulated delays (§2.2) are round numbers, not measurements.

## Why not an emulated ESP32

- Espressif's `esp-emulator` runs only the RISC-V chips (C3, C5, C6, H2, P4, S31). The U64-II module is an ESP32-S3
  (Xtensa LX7, `FW/u64ctrl/sdkconfig:360`).
- Espressif's QEMU fork runs the S3 but emulates no WiFi.
- A home-built S3 would need the LX7 core, the ROM, the peripherals and the closed WiFi driver's MAC registers, and
  would still have no radio.
