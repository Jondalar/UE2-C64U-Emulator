# E2E status: upstream test suite against the emulator

The upstream end-to-end suite (`firmware/1541ultimate/tests`, `./run-tests`, `tests/README.md`) was run against
the emulator with the unmodified upstream `ultimate.elf`. `/v1/info` reports Ultimate 64-II, firmware 3.15, git
b617777c. The runner detected no need for recovery in any run.

The tables below were measured with the T0 C64 stub, before TRX64 became the default C64. After the wave-3 merge,
`E2E_REST_SHIM=1 scripts/run-e2e.sh smoke` with the default `--c64 trx64` passes 12 of 12 suite runs again (19.0 s
total in the runner's summary). The quick profile, and with it E1 (no C64 CPU), was not re-run.

## Result

| Profile | Raw | With REST shim | Suite time, raw / shim |
|---|---|---|---|
| smoke | 11 of 12 OK, exit 3 | **12 of 12 OK, exit 0** | 35 s / 19 s |
| quick (default) | 18 of 27 OK, exit 3 | 21 of 27 OK, exit 3 | 522 s / 792 s |

- **Raw** is the upstream harness unchanged. The smoke failure and 3 of the 9 quick failures come from suites that
  build REST URLs without the port override (H1).
- **Shim** is the same run with `E2E_REST_SHIM=1`, which sends loopback port 80 to the REST forward (see Setup).
- The 6 quick failures left with the shim break down as follows:
  - 5 emulator gaps (E1-E5);
  - 1 test assumption about the network (H2).
- No firmware bug was found.

## Reproduce

```sh
export UE2_FIRMWARE=/path/to/firmware/1541ultimate
scripts/run-e2e.sh smoke                    # report: run/e2e/runs/smoke/index.md
scripts/run-e2e.sh quick                    # report: run/e2e/runs/quick/index.md
E2E_REST_SHIM=1 scripts/run-e2e.sh smoke    # report: run/e2e/runs/smoke-shim/index.md
E2E_REST_SHIM=1 scripts/run-e2e.sh quick    # report: run/e2e/runs/quick-shim/index.md
```

A profile run executes these steps:

1. Build the release binary.
2. Create `run/e2e/venv` from `tests/requirements.txt` (Python 3.12.11) and the SD image
   (`scripts/make-sd-image.sh`), if they are missing.
3. Boot the emulator:

   ```sh
   target/release/ue2emu run --headless --speed realtime \
       --firmware $UE2_FIRMWARE/target/u64ii/riscv/ultimate/result/ultimate.elf --roms $UE2_FIRMWARE/roms \
       --flash run/e2e/flash.bin --sd run/e2e/sd.img --control 127.0.0.1:16400 --net user \
       --hostfwd tcp:18080:80,tcp:18021:21,tcp:18023:23,tcp:18064:64,tcp:51000:51000,...,tcp:52999:52999
   ```

4. Wait for `/v1/version`. REST answered 2 s after start in every run.
5. Run the suite and render the report:

   ```sh
   U64_REST_PORT=18080 U64_FTP_PORT=18021 U64_TELNET_PORT=18023 U64_DMA_PORT=18064 \
       run/e2e/venv/bin/python $UE2_FIRMWARE/run-tests --profile smoke -o run/e2e/runs/smoke 127.0.0.1
   run/e2e/venv/bin/python $UE2_FIRMWARE/tools/e2e_report.py run/e2e/runs/smoke
   ```

6. Send `quit` on the control port, so the flash image is flushed.

The firmware console goes to `run/e2e/emu-<run>.log`. `scripts/run-e2e.sh up` and `down` boot and stop the same
machine for single suites; export the four `U64_*_PORT` variables before starting a suite by hand. All runs here
shared one `run/e2e/flash.bin`. The runner writes back every setting a run changed.

## Setup

- **Ports.**
  - macOS 27 refuses an unprivileged bind of 127.0.0.1 on ports 80, 21, 23 and 64 (EACCES, checked with a Python
    bind). It allows the bind on 0.0.0.0, but that would put the device on the LAN.
  - The suite moves every port through `U64_REST_PORT`, `U64_FTP_PORT`, `U64_TELNET_PORT` and `U64_DMA_PORT`
    (tests/lib/targets.py:77-80) and accepts `127.0.0.1` as the target.
- **FTP passive ports.**
  - ftplib opens the data connection to the control connection's peer, on the port from the PASV reply.
  - The firmware hands out passive ports in sequence from 51000 and wraps at 61000 (ftpd.cc:324-327,400-413).
  - Forwarding all 10 000 ports failed: 127.0.0.1:55555 was already taken on the host (ephemeral range), and a single
    failed hostfwd stops the machine.
  - The script forwards 51000-52999, which covers 2000 passive transfers per boot.
  - With these 2004 forwards the emulator holds realtime: 25 MIPS emulated, about 24 % of one host core.
- **REST shim.**
  - Six suites connect to port 80 whatever the target says (H1).
  - With `E2E_REST_SHIM=1` the script writes `run/e2e/shim/sitecustomize.py` and puts it on `PYTHONPATH`. It wraps
    `socket.create_connection` and sends 127.0.0.1:80 to `U64_REST_PORT`.
  - urllib, http.client and httpx/httpcore all connect through that function.
- **Firmware tree stays read-only.**
  - The script sets `PYTHONDONTWRITEBYTECODE=1` and `RUFF_CACHE_DIR=run/e2e/ruff-cache`.
  - Without the second, the lint suite writes `.ruff_cache` into the checkout. The first run did; that directory was
    deleted.
- **Health sweep.**
  - Before every device suite the sweep reads `ping rest ftp telnet ident dma heap raster -> OK`, with `jiffy=skip`.
  - Two entries prove less than on hardware:
    - `ping` reaches the Mac's own loopback.
    - In the tables below, measured on the T0 stub, `raster` moved because the stub's DMA window advanced `$D012` on
      each read, not because a VIC ran, and `jiffy` was skipped by the harness's own rule: `$00A2` static under a
      moving raster (tests/lib/health.py). With TRX64 a VIC and the KERNAL run.

## Smoke profile

Device = the suite drives the emulator (health sweep in front of it). The check counts are from the shimmed run.
The raw run has the same counts, except readmem-writemem, which failed its first check (0/1/0) in all 3 attempts.

| Suite | Device | Raw | Shim | Checks OK/FAIL/SKIP | Triage |
|---|---|---|---|---|---|
| lint | - | OK | OK | 1 verdict | |
| registry | - | OK | OK | 6/0/0 | |
| transport-usage | - | OK | OK | 1 verdict | |
| esp-depends | - | OK | OK | 2/0/0 | |
| input-batching | - | OK | OK | 5/0/0 | |
| navigation-keys | - | OK | OK | 12/0/0 | |
| ui-backend-parse | - | OK | OK | 9/0/0 | |
| ui-backend-smoke | yes | OK | OK | 44/0/2 | skips: Telnet and REST/Freeze scenarios run from quick up |
| readmem-writemem | yes | FAIL | OK | 9/0/0 | H1 |
| menu-screen | yes | OK | OK | 5/0/1 | skip: no password on this bench |
| cfg-loader-log | - | OK | OK | 6/0/0 | |
| ftp-server | yes | OK | OK | 5/0/0 | |

## Quick profile

Every FAIL used all 3 attempts. The check counts are from the last attempt of the shimmed run. rest-api-coverage
varies between attempts (E5).

| Suite | Device | Raw | Shim | Checks OK/FAIL/SKIP | Triage |
|---|---|---|---|---|---|
| lint | - | OK | OK | 1 verdict | |
| registry | - | OK | OK | 6/0/0 | |
| transport-usage | - | OK | OK | 1 verdict | |
| esp-depends | - | OK | OK | 2/0/0 | |
| input-batching | - | OK | OK | 5/0/0 | |
| runner-policy | - | OK | OK | 99/0/0 | |
| stale-gates | - | OK | OK | 7/0/0 | |
| openapi-validator | - | OK | OK | 16/0/0 | |
| navigation-keys | - | OK | OK | 12/0/0 | |
| ui-backend-parse | - | OK | OK | 9/0/0 | |
| telnet-drain | - | OK | OK | 8/0/0 | |
| ui-backend-smoke | yes | OK | OK | 46/0/0 | |
| readmem-writemem | yes | FAIL | OK | 78/0/0 | H1 |
| menu-screen | yes | OK | OK | 5/0/1 | skip: no password |
| input | yes | FAIL | FAIL | suite aborts | H1, then E1 |
| create-disk-image | yes | OK | OK | 23/0/0 | |
| rest-api-coverage | yes | FAIL | FAIL | 72/11/2 | E1, E3, E4, E5; skips: no password, `--allow-global-reset` |
| openapi-contract | yes | FAIL | OK | 15/0/3 | H1; skips: E3 (2), no password (1) |
| browser-long-filename | yes | FAIL | FAIL | 4/1/0 | H1, then E3 |
| ftp-client | yes | FAIL | FAIL | 9/1/0 | H1, then H2 |
| prg-load-path-trim | yes | OK | OK | 9/0/0 | |
| cfg-single-group | yes | OK | OK | 1/0/0 | |
| cfg-loader-log | - | OK | OK | 6/0/0 | |
| printer | yes | FAIL | FAIL | 4/1/0 | E1 |
| ftp-server | yes | OK | OK | 5/0/0 | |
| assembly64 | yes | FAIL | OK | 46/0/0 | H1 |
| freezer-audio | yes | FAIL | FAIL | 0/1/0 | E2 |

## UCI (`uci-targets`), after S15

`tests/e2e/io/command_interface/uci_targets_test.py` drives the UCI registers `$DF1B-$DF1F` over REST
`machine:readmem` / `machine:writemem`, which are DMA cycles on the cartridge bus, so no 6502 code is involved. It is
the end-to-end check of `docs/specs/S15-uci.md`: the firmware's targets talk to TRX64's UCI block through the C64
registers.

Upstream the suite carries no profile tag, so it runs from `standard` up (`run-tests`, `Suite.profile`) and `smoke`
and `quick` would skip it. `scripts/run-e2e.sh` therefore names it in a second `run-tests` pass of the same boot
whenever the profile is `smoke` or `quick`, with its own report under `runs/<profile>[-shim]-uci/`. `E2E_UCI=0`
switches that pass off, and a caller who selects suites with `-s` decides alone. A single run is still
`[E2E_REST_SHIM=1] scripts/run-e2e.sh quick -s uci-targets`:

| Run | Result |
|---|---|
| raw | FAIL at `[01] reset the C64 so the command interface starts idle`: `PUT http://127.0.0.1/v1/machine:reset` refused — **H1**, the suite builds that one URL without the port. All 9 scenarios then SKIP. The health sweep's REST on the forwarded port is fine in the same run (`rest=11ms`). |
| shim | **OK — 46 checks, 38.4 s**, all three attempts unnecessary (first attempt passed). |
| shim, on the public pin | Re-run after the pin moved to TRX64 v0.6.0 (`2b145c9`): **OK — 46 checks, 35.2 s**, 1 of 1 suite runs passed. |

With the shim every scenario passes: transport, control-target, palette, issue-740-matrix,
save-reu-offset-past-end, load-reu-disabled, save-reu-disabled, softiec-single-part-reply, interface-usable-after,
and the cleanup that hands a pending reply back and restores the settings. That covers the transport state machine
(empty command, unregistered target, no-reply command, ERROR on a push while busy, ABORT back to Idle), the control
target's IDENTIFY and RGB palette commands, issue #740's `CTRL_CMD_LOAD_REU` / `CTRL_CMD_SAVE_REU` matrix, and
SoftIEC single-part reply framing.

## Triage

### H1: REST URL without the port (test assumption: the device serves port 80)

These suites format `http://{host}{path}` themselves, so `U64_REST_PORT` never reaches them. Raw runs fail with
`Connection refused` on 127.0.0.1:80. The shared client adds the port (tests/lib/rest.py:389-390). The fix belongs
upstream: use `rest.url_for` or `target.rest_port`. A device that serves port 80 never shows the problem.

| Suite | Where (tests/) | With the shim |
|---|---|---|
| readmem-writemem | e2e/api/readmem_writemem_test.py:131 | OK, 78 checks |
| input | e2e/api/input_test.py:228 | FAIL, E1 |
| openapi-contract | e2e/api/openapi_contract_test.py:110 (`base_url="http://%s"`) | OK |
| browser-long-filename | e2e/filemanager/browser_long_filename_test.py:76,108,291 | FAIL, E3 |
| ftp-client | e2e/filesystem/ftp_client_test.py:168 (`HTTPConnection` without a port) | FAIL, H2 |
| assembly64 | e2e/io/c64/assembly64_test.py:243,257 | OK, 46 checks |

### H2: ftp-client needs the device to reach the host at the host's own address

- **Setup.** The suite serves pyftpdlib on 0.0.0.0:2121 and makes the device connect to it.
- **Address.** It advertises the local end of a UDP socket connected to the target (ftp_client_test.py:2100-2117),
  which is 127.0.0.1 here. Behind slirp that is the guest's own loopback; the host is 10.0.2.2.
- **Shim run.** The host entry is created through the New Host form, then
  `[10] enter host: server sees login + TYPE I + LIST ... FAIL (missing FTP commands (PASS=False LIST=False))`.
- **By hand.** With the shim and `--ftp-advertised-host 10.0.2.2`, all 6 smoke-stage operations pass: create host,
  list host, enter/LIST, root entries, RETR README (52 bytes), remove host. The runner cannot pass that option.

### E1: no C64 CPU (doc 10 T0) — TRX64 is the default since S14, quick not re-run

The T0 C64 is a 64 K byte array. Nothing executes 6510 code, so there is no KERNAL and no READY prompt.

- **input:** `BASIC READY prompt not visible; device may be running a cartridge`. tests/lib/api.py:222-235 polls
  screen RAM for READY.
- **rest-api-coverage [49] machine:reset and [50] machine:reboot:** `the C64 did not reach the BASIC prompt`.
- **printer [05] epson/bitmap:** `FAIL_TIMEOUT` after 60 s. `printer_e2e.prg` must run and fill a status block that
  the suite polls with readmem (printer/printer_test.py:365-404). Checks [01]-[04] pass: REST, reset, load PRG,
  capture settings.

### E2: the audio and video streams — **built** (S24, 2026-09-21)

The FPGA's stream generators are emulated: `U64_UDP_BASE`'s four header templates and `ETHSTREAM_ENA` are a device
(`devices/streams.rs`), a finished VIC frame becomes 68 datagrams of 780 bytes, the SID's samples become 770-byte
datagrams, and both go out through the same backend the MAC's frames use — so the source address, the ports and
the source MAC are the guest's, as a receiver requires. `docs/specs/S24-udp-streams.md` has the contract and what
is deliberately not built (the bus and IEC streams, which carry cycle-level traces the emulator has no equivalent
for, and which no suite asks for).

Measured with `--net user`, the firmware armed over its own REST (`PUT /v1/streams/video:start?ip=10.0.2.2:11000`
and the same for `audio`), both streams at once, captured on the host for 10 s:

| | result |
|---|---|
| video | 34068 datagrams, none malformed, 501 frames = **50.1 fps** |
| audio | 2502 datagrams, none malformed, **48038 stereo frames/s**, no sequence gap |

The frame the receiver assembles is the picture: 272 lines, 4-bit indices, the BASIC screen with its border
(`run/shots/stream-frame.png` is one, decoded by the harness's own packing rules).

Under NTSC (S34) a frame is 60 datagrams of 240 lines, the middle of the NTSC canvas, and the audio runs at the
NTSC rate.

Two things to know about what arrives. The samples are the SIDs' stereo mix (S29), panned by the mixer; the
sampler is not in the stream, as it joins only on the way to the audio device. And the
generator sends a frame's datagrams as one burst rather than spread across the frame, which the receiver's
assembler does not depend on (S24 §5).

Left for the suites themselves: `ultimax-cartridge`, which compares a whole frame against
`tests/e2e/io/c64/jupiter_lander.png` index for index, and `av/stream_test`.

### E3: Flash Disk not provisioned

- **Cause.**
  - The firmware loads drive ROMs from `/flash/roms` (c64.h:12). The default file is `1541.rom` (c1541.cc:51); a
    failed load returns `SSRET_NO_DRIVE_ROM` (c1541.cc:927-935).
  - On hardware the U64-II FAT preparer writes `1541.rom`, `1571.rom`, `1581.rom` and the drive sound banks there
    (u64ii_prepare_fat/flash_disk_prep.cc:83-92).
  - `run/e2e/flash.bin` starts erased.
- **rest-api-coverage [54]-[57]:** drive mount answers HTTP 412 `Drive ROM not found`.
- **rest-api-coverage [58]:** set_mode answers `Invalid Drive Type 'Unknown'`.
- **browser-long-filename [05]:** browser mount fails with
  `drive_a={'enabled': False, 'type': 'Unknown', 'rom': 'Unable to load!'}`.
- **openapi-contract skips [03], [04]:** `this firmware predates the update that writes /Flash/html/openapi.yaml`
  (and `api.html`). Same missing update step; the firmware version is not the cause.
- **Console:** `Failed to load KERNAL ROM; loading default.` and `Failed to load CHAR ROM; loading default.`

Closing this needs a Flash Disk laid out the way the updater leaves it: `ue2emu install` writes one
(`docs/status/install.md` §5), but `scripts/run-e2e.sh` creates its flash erased.

### E4: second drive not advertised — closed by S27

rest-api-coverage [51] failed with `drive b missing from the listing: ['IEC Drive', 'Printer Emulation', 'a']`. The T0
capability word 0x34000222 left out `CAPAB_DRIVE_1541_2` (itu.h:51), and drive B is only created when that bit is set
(c1541.cc:1260-1265; docs/hw/00 §3 C4, Q-B1). Since S27 the default word is 0x34000226 and drive B is TRX64's second
drive: `/v1/drives` lists `b` (bus ID 9, disabled), as on a C64 Ultimate. The suite was not re-run.

### E5: REST connections reset when they overlap (network path, S12)

- **In the suite.**
  - rest-api-coverage runs its cases on 3 workers (e2e/api/rest_api_coverage_test.py:1112-1123). Upstream records
    that the U64 and C64U "answer three at a time" (tests/lib/machine.py:390-397).
  - 8 checks fail on every attempt, for E1, E3 and E4. On top of those, cases fail with `ConnectionResetError` or
    `BrokenPipeError` on the forwarded port: [08]/[09] GET /v1/info, [44] writemem, [45] input, [60]-[78] refusals,
    [82] streams stop.
  - Total FAILs per attempt: raw 28, 8, 11; shimmed 12, 12, 11. Which cases get reset changes every time.
- **Measured outside the suite.** http.client, `GET /v1/info`, `Connection: close`, 30 requests per row, realtime
  emulator:

  | Clients | Stagger between connection opens | Result |
  |---|---|---|
  | 1 | - | 30 × HTTP 200 |
  | 2 | 0 | 17 × 200, 13 reset or broken pipe |
  | 3 | 0 | 11 × 200, 19 reset |
  | 3 | 100 ms | 30 × 200 |
  | 3 | 300 ms | 30 × 200 |

- **Speed.**
  - At `--speed max` (about 8× more instructions per wall-clock second), 60 serial requests still all succeed.
  - 3 unstaggered clients still lose 43 of 60: 17 × 200, 31 reset, 6 broken pipe, 6 other OSError.
  - At realtime the same test lost 59 of 60.
  - Emulated CPU speed makes it worse, but is not the cause.
- **Firmware limits do not explain it.**
  - `MAX_HTTP_CLIENT` is 5, with `listen(sock, 5)` (httpd/FreeRTOS/lib/server.h:14, server.c:65).
  - `DEFAULT_ACCEPTMBOX_SIZE` is 8 and `MEMP_NUM_TCP_PCB` is 30 (network/config/lwipopts.h:203,937).
- **Root cause: not isolated.** Two unverified candidates:
  - frames that arrive as one burst per pump slice, where the RMII model drops a frame when no free buffer ID is
    queued (rmii.rs:170);
  - libslirp's handling of a guest that accepts slowly.

  A packet capture on the slirp side would decide between them.
- **Unrelated.** The console lines `ERROR reading from socket -1. Errno = 104` belong to the Telnet UI session
  (`User Interface on stream returned`). The health sweep's Telnet probe connects and closes at once, which is the
  reset they report.

## Not covered

- Only the overlay transport (the smoke and quick default). The freeze and telnet sweeps of deep and exhaustive
  were not run.
- One run per profile, plus the raw smoke repeated with the final script. E5 makes the exact failing set of
  rest-api-coverage vary.
- The passive-port window allows 2000 FTP transfers per boot.
