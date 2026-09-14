# Network status: backends, LAN access, C64U 1.1.0 services

`--net MODE` attaches a host backend to the RMII MAC (docs/hw/08-network-rmii.md T1). All modes set
`CAPAB_ETH_RMII`, plug the PHY cable and let the firmware run its own DHCP client. Code: `crates/ue2-net`
(backends behind `host::NetBackend`), `crates/ue2emu/src/net.rs` (CLI, attach, pump).

| MODE | Host side | Privileges | Guest address |
|---|---|---|---|
| `user` | libslirp NAT; `--hostfwd` (default `tcp:2323:23,tcp:2121:21,tcp:6464:64`); web UI proxy to guest port 80 on `--web-port` (default 8080) | none | 10.0.2.15 |
| `vmnet-bridged[:IFACE]` | vmnet.framework `VMNET_BRIDGED_MODE` onto IFACE; default = interface of `route -n get default` | root (`sudo`) or the `com.apple.vm.networking` entitlement | DHCP from the LAN |
| `socket-vmnet[:PATH]` | client of lima's socket_vmnet daemon; default PATH `/opt/homebrew/var/run/socket_vmnet` | none (the daemon runs as root) | DHCP from the daemon's network: the LAN when the daemon runs bridged, 192.168.105.x in its shared mode |

`--hostfwd` is accepted only with `user`; a bridged guest is reached at its own address.

## Web UI proxy (`--net user`)

**Problem.** The firmware web UI builds every API URL from the page's host name, which has no port:
`var serverIP = window.location.hostname;` (firmware/1541ultimate/html/index.html:12), then `"http://" + serverIP +
"/v1/…"` for every call (index.html:22, 89, 95, 111, 121, 233, 271, 410, 417, 775, 802, 836, 877). The `index.html`
of the Commodore C64U 1.1.0 release (written by `c64u_v1.1.0.ue2`, served by its application) has the same line 12
and 12 uses of the same form. On hardware the page comes from port 80, so the URLs are right. Behind a forward from
127.0.0.1:8080 every button calls port 80 of the Mac and fails. Binding 127.0.0.1:80 needs root on macOS, and
0.0.0.0:80 would put the UI on the LAN. The API explorer `api.html` already uses `window.location.host`
(api.html:166).

**What the proxy does.** ue2emu listens on 127.0.0.1:`--web-port` and connects every client through an internal
libslirp forward to guest port 80. That forward listens on a 127.0.0.1 port the OS picks; the banner names it.

- **Requests** go to the guest byte for byte: method, headers, bodies, `Expect: 100-continue`, chunked request
  bodies. The proxy follows the framing only to know which responses answer a `HEAD`.
- **Responses** pass byte for byte (binary `readmem` bodies, `Connection: close`, close-delimited bodies), with one
  exception. The `Content-Type` is `text/html` or a JavaScript type (`application/javascript`, `text/javascript`,
  `application/x-javascript`, `application/ecmascript`, `text/ecmascript`), and there is no `Content-Encoding` other
  than `identity`. Then every `location.hostname` that is not part of a longer identifier becomes `location.host`, so
  `window.location.hostname` (`127.0.0.1`) turns into `window.location.host` (`127.0.0.1:8080`).
  - A changed body goes out without `Transfer-Encoding`, with a new `Content-Length` (4 bytes less per replacement).
  - A body without a match goes out unchanged, as does a compressed one or one over 16 MiB.
- **What the firmware sends.** It never compresses. Static files are close-delimited: `HTTP/1.1 200 OK`,
  `Connection: close`, a `Content-Type` from the extension (`.html` `text/html`, `.js` `application/javascript`, others
  such as `openapi.yaml` `text/plain`), no length (software/httpd/c-version/lib/middleware.c:17-34,82-83,111-120).
  REST answers carry `Content-Length` and `Connection: close` (software/api/routes.h:102-103,110-127). Measured on `/`:
  65961 bytes close-delimited from the guest, 65957 bytes with `Content-Length: 65957` from the proxy; the only
  difference is line 12.
- **Threads.** One thread accepts; each connection has one per direction. The emulation thread still only pumps
  libslirp, so a slow client never stalls it. A guest that closes without answering (web server not up yet) closes
  the client without a response, as a plain forward does, so `emu_rest` keeps retrying.

| Command line | Guest port 80 | Other forwards |
|---|---|---|
| `--net user` | proxy on 127.0.0.1:8080 | `tcp:2323:23,tcp:2121:21,tcp:6464:64` |
| `--net user --web-port N` | proxy on 127.0.0.1:N | the same |
| `--net user --web-port 0` | plain forward `tcp:8080:80` (the earlier default) | the same |
| `--net user --hostfwd LIST` | as LIST says, no proxy | LIST |
| `--net user --hostfwd LIST --web-port N` | proxy on N, plus what LIST forwards | LIST |

- **Errors at startup.**
  - A `--web-port` equal to a `--hostfwd` host port is refused.
  - A taken port fails with `--web-port 18095: cannot listen on 127.0.0.1:18095 (port in use? choose another
    --web-port, or 0 for none)`.
  - With `vmnet-bridged` or `socket-vmnet`, a non-zero `--web-port` is refused: the guest serves port 80 on its own
    address.
- **Banner:** `net: web UI http://127.0.0.1:8080/ (proxy to guest port 80 through 127.0.0.1:56538; location.hostname
  becomes location.host in HTML and JavaScript)`.
- **Callers.** `ue2-mcp` starts instances with `--web-port <http>` (`docs/status/mcp.md`). `scripts/run-e2e.sh` and
  `scripts/cart-slot-acceptance.py` pass `--hostfwd` and keep plain forwards.
- **Code:** `crates/ue2-net/src/web_proxy.rs` (proxy and rewrite), `UserNet::start_web_proxy` in
  `crates/ue2-net/src/lib.rs`, options in `crates/ue2emu/src/net.rs`.

**Verification.** Upstream ELF 3.15 on a scratch flash from
`install --update <1541ultimate checkout>/update.ue2 --yes --c64-roms`, run with
`--web-port 18080 --hostfwd tcp:18081:80,…`, so a plain forward sits next to the proxy.

- **Unit tests** (`web_proxy::tests`):
  - A firmware-style close-delimited HTML page is rewritten and gets a length.
  - Chunked JavaScript is rewritten with the match split across two chunks.
  - These stay byte-identical: octet-stream holding the pattern and high bytes, gzip HTML, HTML without a match,
    chunked `text/plain`.
  - A 300 000-byte upload with `Expect: 100-continue` and a chunked `PUT` arrive unchanged; an interim
    `100 Continue` passes.
  - Pipelined `HEAD` + `GET` on one connection come back correctly framed.
  - A guest that closes without a response gives the client none.
  - `tests::web_proxy_connects_through_a_loopback_forward` runs the path through libslirp.
- **curl:**
  - `GET /v1/info`: 200, 314 bytes.
  - `POST /v1/drives/a:mount?type=d64&mode=readwrite` with a 174 848-byte D64 as `application/octet-stream`: 200,
    `"file" : "/Temp/cache/upload/temp0000"`; `GET /v1/drives` shows it on drive A.
  - `GET /v1/machine:readmem` with the machine paused (`PUT /v1/machine:pause`): `$0000` (53 248 bytes) and `$E000`
    (8 192 bytes) are identical through the proxy and the plain forward (sha256 `62fbc5d4af879dd2…`,
    `83c60d47047d7bea…`). A full 64 KiB read differs only in `$D000-$DFFF`, which also changes between two plain
    reads of the paused machine (VIC raster, CIA timers, cartridge I/O).
- **Safari** (safari-mcp):
  - The console logs `Requesting URL: http://127.0.0.1:18080/v1/info?`. The network log has `GET / 200 text/html
    65957` and `GET http://127.0.0.1:18080/v1/info? 200 application/json`. The page shows "Ultimate 64-II HTTP
    Server" and "firmware v3.15" from `/v1/info`.
  - The one 404 is `/favicon.ico` (firmware console `Not found: '/Flash/html/favicon.ico'`).
  - "Run PRG / CRT / Disk" opens. It calls the API only when a file is chosen; `serverIP` in the page is
    `127.0.0.1:18080`, so its upload goes to `http://127.0.0.1:18080/v1/runners:run_prg`. Not clicked: it changes the
    machine.
  - "Live Monitor": `m 0400 04c0` and `d fce2 fd00` sent 13 `GET http://127.0.0.1:18080/v1/machine:readmem?…`
    requests, all 200 `application/octet-stream`. The terminal shows the screen codes of `**** COMMODORE 64 BASIC V2
    ****` / `38911 BASIC BYTES FREE` and the KERNAL reset code `LDX #$ff`, `SEI`, `TXS`, `CLD`, `JSR $fd02`. No
    console warnings or errors.
- **Default options:** the banner above; `GET /v1/info` on 8080 answers 200; `/` has `Content-Length: 65957` and
  `var serverIP = window.location.host;`. Listening: 8080, 2323, 2121, 6464 and the internal forward. `--web-port 0`
  prints the old list `127.0.0.1:8080 -> 80, …`.
- **C64U 1.1.0**, booted with `--firmware c64u_v1.1.0.ue2 --web-port 18090` on a scratch flash from
  `install --update c64u_v1.1.0.ue2 --yes`, with the services enabled by the script in the C64U section below:
  - `/v1/info` returns `"product" : "C64 Ultimate"`, `"firmware_version" : "1.1.0"`.
  - `/` is 56 840 bytes through the plain forward and 56 836 through the proxy; only line 12 differs.
  - Safari: `GET http://127.0.0.1:18090/v1/info? 200` and the page title "C64 Ultimate HTTP Server".
  - Live Monitor: `m 0400 0440` shows the READY screen, read by four `GET http://127.0.0.1:18090/v1/machine:readmem?…`
    requests, all 200 `application/octet-stream`. The only 404 is `/favicon.ico`, as upstream.

**Limits.**
- The rewrite is textual: it replaces `location.hostname` anywhere in an HTML or JavaScript response, strings and
  comments included. Neither firmware uses it for anything but `serverIP`.
- Like the plain forwards, the proxy listens on 127.0.0.1 only.
- The web UI loads jQuery and jquery.terminal from cdn.jsdelivr.net (index.html:6-8), so the Mac needs internet access
  for the UI.
- While the emulation is stopped (for example at a GDB breakpoint), responses wait, as with a plain forward.

## Guest MAC per instance

- The firmware builds its MAC from the flash unique ID (`RUID` 0x4B): `02:15:41:(u1^u5):(u2^u6):(u3^u7)`
  (rmii_interface.cc:119-128). The default hostname and the REST `unique_id` come from the same ID.
- The flash model's ID `UE2C64U\x01` gives `02:15:41:71:67:42`; C64U 1.1.0 names itself `C64-Ultimate-716742`
  with it. Every emulator would have that MAC.
- In the two bridged modes the emulator replaces the ID before the first instruction
  (`SpiFlash::set_unique_id`): FNV-1a of the absolute `--flash` path in bytes 1-3, bytes 5-7 zero. The same image
  keeps its MAC and DHCP lease across runs; without `--flash` every run is a new device. `user` mode keeps the
  default ID. The banner prints it, e.g. `net: guest MAC 02:15:41:f3:be:d7, address by DHCP from the host network`.
- A hostname already saved in the image's "Network Settings" store keeps its old suffix; only the MAC changes.
- vmnet is started with `vmnet_allocate_mac_address_key = false`, so the firmware's MAC goes onto the wire.

## `vmnet-bridged`

- IFACE must be in `vmnet_copy_shared_interface_list` (works without root; here `en0, en7`). Otherwise:
  `vmnet cannot bridge 'nosuch0'; bridgeable interfaces: en0, en7`.
- Without root or entitlement, `vmnet_start_interface` returns a handle but completes with `VMNET_FAILURE` (1001),
  measured as uid 501 on macOS 27. The emulator checks euid and the entitlement (`SecTaskCopyValueForEntitlement`)
  and reports:
  `vmnet refused bridged mode on en7 (VMNET_FAILURE (general failure)): it needs root or the com.apple.vm.networking
  entitlement, and this process runs as uid 501 without it. Run ue2emu with sudo, or use --net socket-vmnet with a
  bridged socket_vmnet daemon`.
- Runtime: vmnet's event callback (private dispatch queue) only raises a flag. `poll` on the emulation thread reads
  up to 32 packets per `vmnet_read` until the interface is empty; `send` is one `vmnet_write`. The first read or
  write error is printed once. Drop disables events and stops the interface.
- The bridge is bound to the interface chosen at start. After switching between dock Ethernet and Wi-Fi, restart
  the emulator (the default then follows the new default route).
- The com.apple.vm.networking entitlement is restricted (Apple-granted); in practice this mode means `sudo`.

## `socket-vmnet`

- Wire format both ways: u32 big-endian length, then the Ethernet frame without FCS (socket_vmnet main.c; QEMU
  `-netdev stream`). The socket is non-blocking: `poll` reads everything available and delivers complete frames;
  guest frames the socket cannot take wait in a 1 MiB backlog, beyond it they are dropped. A length above 0xFFFF,
  EOF or a socket error prints one line and the backend goes silent until restart.
- Connect errors name the cause: no socket (daemon not installed/started), connection refused (stale socket),
  permission denied (socket group).
- The daemon floods frames between vmnet and all clients, so several emulators can share it; the per-image MAC
  above keeps them apart.
- Daemon setup (socket_vmnet README). Shared mode, NAT on 192.168.105.0/24, not the LAN:
  `sudo /opt/homebrew/opt/socket_vmnet/bin/socket_vmnet --vmnet-gateway=192.168.105.1 /opt/homebrew/var/run/socket_vmnet`.
  Bridged mode, a LAN address:
  `sudo /opt/homebrew/opt/socket_vmnet/bin/socket_vmnet --vmnet-mode=bridged --vmnet-interface=en0 /opt/homebrew/var/run/socket_vmnet.bridged.en0`,
  then `--net socket-vmnet:/opt/homebrew/var/run/socket_vmnet.bridged.en0`. A daemon serves one interface; for dock
  and Wi-Fi run one per interface and pick the socket.

## Verification without root

- Unit tests: `ue2-net` socket_vmnet framing across every split, oversize header, a mock daemon (burst + split
  frame, guest frame received), daemon close, connect errors; vmnet `route -n get` parsing, error texts,
  unknown interface, and the real non-root `vmnet_start_interface` failure (skipped when privileged); `ue2emu`
  `--net` syntax, `--hostfwd` rule, per-image MAC; `ue2-core` `flash_uid_can_be_replaced`.
- Real runs, error paths (`target/release/ue2emu run --headless --net ...` with the upstream ELF): `vmnet-bridged`,
  `vmnet-bridged:nosuch0`, `socket-vmnet` without a daemon, `socket-vmnet --hostfwd ...`, `--net tap`. Each exits with
  the message quoted above.
- Real run, full path: `crates/ue2-net/examples/mock_socket_vmnet.rs` speaks the socket_vmnet format on a unix socket
  and puts the frames onto libslirp.

  ```sh
  cargo run --release -p ue2-net --example mock_socket_vmnet -- $TMPDIR/mock-vmnet.sock tcp:18580:80,tcp:18523:23 &
  target/release/ue2emu run --headless --firmware c64u_v1.1.0.ue2 --roms $UE2_FIRMWARE/roms --flash run/c64u.bin \
      --net socket-vmnet:$TMPDIR/mock-vmnet.sock
  curl http://127.0.0.1:18580/v1/info
  ```

  With the C64U image below (services enabled): banner `net: guest MAC 02:15:41:f3:be:d7`, `Status update IP =
  10.0.2.15`, `/v1/info` answers (`"firmware_version" : "1.1.0"`, `"unique_id" : "4DD700"`), and Telnet on 18523
  sends `*** C64 Ultimate (V1.01) 1.1.0 *** Remote ***`.
- Not verified here: a real bridged lease (needs `sudo` or the daemon). Steps for that are in the W3-NET hand-off.

## C64U 1.1.0: HTTP, Telnet, FTP stay off

**Symptom.** Under `--net user` the Commodore release gets 10.0.2.15 but prints only `Starting Telnet Server` and
`<http_listen_task>`; `Telnet server starting`, `FTP server starting`, `Socket DMA server starting`, `Listening`
never appear. The upstream ELF prints all of them.

**Cause: a firmware default, not an emulation gap.** The Commodore build disables all network services in the
"Network Settings" store definition (`network_config[]`, config.h:87-96 layout, 28 bytes per item):

| Item (id) | upstream `update.ue2` default | C64U 1.1.0 default |
|---|---|---|
| Ultimate Ident Service (0x21) | 1 (table 0x15358C) | 0 (table 0x11F830) |
| Ultimate DMA Service (0x22) | 1 | 0 |
| Telnet Remote Menu Service (0x23) | 1 | 0 |
| FTP File Service (0x24) | 1 | 0 |
| Web Remote Control Service (0x25) | 1 | 0 |

Read from the `.app` records embedded in each `.ue2` (item text pointer → definition → `def`); upstream source has
`1` (network_config.cc:21-25). With `enabled == false` the listener tasks exist but sleep in
`while (!enabled) vTaskDelay(2000)` (socket_gui.cc:65-71,227-230; ftpd.cc:215-220,243-246; httpd.cc:18-26,52-54),
which matches the log. The menu shows the same: F2 → Network Settings lists all five as `Disabled`. Link, DHCP and
the task list are identical to the upstream ELF.

**Enable them** (once per flash image; the setting persists):

1. Menu button, F2 (config browser), 8 × DOWN to "Network Settings", RIGHT.
2. 3 × DOWN to "Ultimate Ident Service"; for it and the next four: RETURN, `e` (seeks "Enabled"), RETURN, DOWN.
3. LEFT, LEFT; "Save changes to Flash?" → RETURN. Console: `Writing config store 'Network Settings' to flash..Page: 6 done.`

The services start at once (`**** EFFECTUATE NETWORK SETTINGS ****`, `Telnet server starting`, `Listening`).
Headless, as a control script against `--flash`:

```
wait 8000
button
wait 1000
key f2
wait 800
key down
key down
key down
key down
key down
key down
key down
key down
key right
wait 800
key down
key down
key down
key return
key e
key return
key down
key return
key e
key return
key down
key return
key e
key return
key down
key return
key e
key return
key down
key return
key e
key return
key left
wait 500
key left
wait 500
key return
wait 1500
quit
```

(The verified script has `wait 300` after each RETURN.) Next boot with the same image and `--net user`:
`curl http://127.0.0.1:8080/v1/info` returns `"product" : "C64 Ultimate"`, `"firmware_version" : "1.1.0"`,
`"hostname" : "C64-Ultimate-716742"`.

**Flash seeding.** Not done by default: the services are off on purpose in Commodore's release, and the network
password is empty by default, so enabling them unasked would open an unauthenticated menu, FTP and REST API on the
LAN in the bridged modes. An opt-in flag that seeds the five items into a blank "Network Settings" store (store
`0x4E455400`, like the S06 overlay-UI seed) would be the emulator-side fix; it belongs to `devices/flash.rs` and is
not implemented here.

## Known gaps

- No link notion on `NetBackend`: when the socket_vmnet daemon goes away the PHY link stays up, and the firmware
  keeps its address.
- The RX filter drops multicast like the hardware (08 §RX path), so mDNS does not reach the guest in any mode.
- vmnet bridging over Wi-Fi depends on macOS; it was not exercised (no root here).
