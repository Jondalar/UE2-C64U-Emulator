# S46 — MultiCore update packages (.txu): run the app, install into the flash

**Status:** BUILT (2026-11-20). T1-T3 pass; T4 measured here with MultiCore's pre-release t239.1-b4730f57; T4 in
full and T5 are the MultiCore session's. MultiCore's CI itself runs `run --c64-roms` and does not need S46.
**Repo:** UE2 (u64-emulator). Reference: TRX64U-MultiCore `main` at cc729e64d.
- `host-app/software/hostapp/txu.h` and `txu.cc`: the layout, the reader `txu_probe` and the checks `txu_stage`.
- `tools/txu_pack.py`: the writer, with the same table as txu.h.
- `host-app/software/hostapp/flashfw.h` and `flashfw.cc`: the slots, `fw_write_parts` and the AXSS walk.
- `host-app/software/hostapp/cfw.cc`: `cfw_bit_idcode`.
- `tools/trx_pkg.py`: `check_app_record`.
- `ci/trxapps.sh:142-148`: `hostapp_emu`.

**Origin:** 2026-11-20. MultiCore's GitHub CI has no vendor firmware, and it tried to fill a flash with its own updater
through `ue2emu install`. That could not work: `install` ran on the 100T FPGA type and the S25FL128L flash, and the
updater wants the 50T type and a W25Q (S47 changed that). The CI does not need the updater, because `run --firmware` loads the
application straight into DDR. Owner: "den Emulator um .txu zu erweitern … Schreib dir eine Spec".
**Depends on:** S44 (`--icap`, the IPROG back to the host), S45 (the MultiCore host's video) and S47 (`--board c64u`
gives the W25Q and the XC7A50T, `install --caps/--board`).
**Code:** `crates/ue2-core/src/txu.rs` (format, checks, writing, the writer `pack`), `loader::load_txu`,
`crates/ue2emu/src/install.rs` `install_txu`.

---

## §1 The problem

A `.txu` is what MultiCore ships to a C64U. It holds the host bitstream, the host app, or both (txu.h:1-20). ue2emu
reads neither part:
- `run --firmware X.txu` failed, because `load_firmware` knew ELF, `.app` and `.ue2` only.
- `install --update X.txu` failed, because there is no updater in a `.txu`; the host app writes the parts itself.

MultiCore ran the separately built `hostapp_emu.elf`. The `.txu` that goes to the board could not be run as it is.

## §2 The format, read as txu_probe reads it

Little endian. A `.txu` is recognised by its content: `TXU1` at offset 0, the way `load_firmware` recognises the other
formats.

| Offset | Field | Rule |
|---|---|---|
| 0 | `TXU1` | |
| 4 | hdr_len | 80 + 48 × nparts |
| 8 | flags | 0 |
| 12 | host_key | ≠ 0 if and only if there is a host part |
| 16 | iface_provided | ≠ 0 if and only if there is a host part |
| 20 | iface_needed | ≠ 0 if and only if there is an app part |
| 24 | nparts | 1 or 2 |
| 28 | origin[8] | `trx` |
| 36 | version[24] | text |
| 60 | commit[16] | text |
| 76 | parts | nparts × { kind, off, size, flash_addr, sha256[32] } |
| hdr_len − 4 | CRC-32 | zlib CRC-32 of the bytes before it |

The rules for each part:
- kind is 1 (host) or 2 (app), and no kind appears twice.
- flash_addr is 0x000000 for the host and the app slot's start for the app.
- size is from 1 up to the slot: the bitstream slot for the host, the app slot for the app.

**The FPGA table (D2).** The slots, the IDCODE and the FPGA type come from one table, `txu::FPGAS`, with the layouts
of w25q_flash.cc:47-59:

| FPGA type | FPGA | IDCODE | Bitstream slot | App slot |
|---|---|---|---|---|
| 2 | XC7A50T | 0x0362C093 | 0x000000-0x21FFFF | 0x220000, 0x1E0000 bytes |
| 3 | XC7A100T | 0x03631093 | 0x000000-0x3BFFFF | 0x3C0000, 0x1C0000 bytes |

A TXU1 header has no FPGA field: every TXU1 package is read as the XC7A50T's, as the host app's constants say
(flashfw.h:15-18, cfw.h:21). A universal package costs a table row and a field in a later format; none is invented
here.
- off is 4-aligned, not before the end of the previous part, and the part lies inside the file.
- The last part ends at the end of the file.

A file that breaks a rule is refused with txu_result_str's words (txu.cc:9-20), for example "the update file is damaged
or incomplete", followed by the rule it broke.

- **App part:** an `.app`, a 12-byte record { dest, length, run } and its data. It must pass `check_app_record`
  (trx_pkg.py:194-205): length 1..0x1E0000 − 12, dest 4-aligned and from 0x10000 to 0x04000000 − length, run inside
  [dest, dest + length), and size = 12 + length rounded up to 4. The host app links at 0x00030000
  (`host-app/target/u64ii/riscv/hostapp/linker.x:7`), which is ue2emu's `APP_BASE`.
- - **Host part:** the `.bit` file, header included, as written at flash 0.

## §3 `run --firmware X.txu`

`load_firmware` gets a fourth format, `ImageFormat::Txu`. Every path that loads a firmware goes through it, so it
covers all of these:
- `run`
- `settings`
- `firmware =` in a `--config` file
- the IPROG back to the host (`Machine::reload_firmware`, machine.rs:1117; multicore.rs:218)

What happens:
1. The file is read as in §2. The SHA-256 of the app part must equal its header entry.
2. The app part's record is loaded as a `.app` (`load_records`), which gives the entry and the segments.
3. There are no symbols, as with `.app` and `.ue2`. `hostapp_emu.elf` stays the way to get symbols.
4. `--trx-io=auto` decides from the loaded image as now. The host app carries `[TRX UI]` (lvgl_port/trx_lv_port.cc),
   so it resolves to on.
5. One line: `X.txu: TRX update <version> (<commit>), app <n> bytes at <dest>, entry <run>`; n is the record's
   length.
6. A host part is not used, because ue2emu's FPGA is its device models. One line says so: `X.txu: host part (key
   <host_key>, <n> bytes) not used: the FPGA is the emulator's`.
7. A `.txu` without an app part is refused: `X.txu holds only a host bitstream; ue2emu runs the app`.
8. `install`'s updater loader refuses a `.txu`: it holds no updater.

**The board's app on ue2emu:** `hostapp_emu` is the same build with `-DHOSTAPP_EMU_FLASH`, which only flashfw.cc
reads (ci/trxapps.sh:142-148). With that flag, `fw_chip_ok` also accepts the S25FL128L, "Infineon S25FL128L"
(flashfw.cc:124-138). On `--board c64u` the flash is the W25Q128 (S47), so the board build from a `.txu` writes
firmware as on the board.

## §4 `install --update X.txu`

The point is a flash image that holds what a board holds after this `.txu`, written by a reference that does not
depend on the host app. This gives the host app's own installer (the `.txu` flow, `fw_write_parts`) a deterministic
test (D1, T5):
- **A known start:** `install` A, then the host app updates to B from its menu.
- **A known result:** the slots the host app wrote equal what `install` B writes on the same start.

The host app also reads the slots itself:
- `fw_flash_equals_bit` and `fw_flash_equals_app` in host_core_flow.cc:693-696 ("the flash already holds this").
- `fw_flash_equals` in host_core_flow_restore.cc:318.

No updater runs and no machine is built. ue2emu writes the parts the way `fw_write_parts` does
(flashfw.cc:393-473):
- **App:** the sectors that cover [0x220000, 0x220000 + size) are erased and written. The rest of the app slot stays
  as it was.
- **Host:** the sectors that cover [0, size) are erased and written, then the rest of the bitstream slot
  [size, 0x220000) is erased.
- A part the package does not carry leaves its region as it was.
- A missing flash file is created erased, at 16 MiB, as now.

**Checks before the first write.** These are txu_stage's checks (txu.cc:180-237). If one fails, nothing is written.
- §2, including the SHA-256 of each part.
- The app record check of §2.
- **Host part:**
  - The `.bit` header's length equals the part size (`bit_length_from_header`, txu.cc:145-164).
  - The IDCODE in the configuration packets is the XC7A50T's, 0x0362C093 under the mask 0x0FFFFFFF
    (`cfw_bit_idcode`, cfw.cc:207-211).
  - The AXSS stamp, the value written to register 0x0D found by the walk of flashfw.cc:67-113, equals host_key.

**Not checked:** `txu_check`, which compares the interface numbers against the running system. In `install` no system
runs, and the numbers of the part already in the flash are compiled into it rather than stored. One line prints the
package's iface_provided and iface_needed.

**After writing**, the two regions are read back and compared, with one line for each part:
- `install: flash 0x220000: app, <n> bytes, identical to X.txu`
- `install: flash 0x000000: host bitstream, <n> bytes, key <host_key>`

**The FPGA type:** without `--caps` or `--board` it is the package's (type 2 for every TXU1). Given ones must agree
with it; otherwise nothing is written: `X.txu is for the XC7A50T (FPGA type 2); this machine's FPGA type is 3, the
XC7A100T`. `--flash-chip` and the board model pick the chip as on `run` (S47 §2); it only matters for `--c64-roms`.

**`--c64-roms`** then uses that capability word, so a TXU1 gets the 50T layout with `/flash` at 0x400000, and the
chip's config pages.

**Options of the updater run** (`--c64`, `--yes`, `--reformat-flash`, `--reset-config`, `--timeout`, `--cart-slot`)
do nothing for a `.txu`. If one is given, `install` says so in one line.

**Measured** (2026-11-20, `trx64host_update_b4730f57.txu` of MultiCore's pre-release t239.1-b4730f57):
`install --board c64u --update X.txu --c64-roms` passes every check (host key cd508203, 1549789 bytes; app 1106496
bytes) and writes both parts; `run --board c64u --icap --firmware X.txu` shows the host app's boot menu with "app
0.2.0-dev+b4730f57" and "host 0.2.0-dev+b4730f57".

## §5 Not in this spec

- **The host bitstream is never executed.** ue2emu's FPGA stays its device models; `--board`, `--caps`, `--icap` and
  `--trx-io` choose them as before.
- **The flash chip** is S47's.
- **No boot from the flash.** `run` still loads `--firmware`; an installed app slot is data, as after a `.ue2`
  install.
- **No `.cor` or `.cfw`.** A `.txu` carries no cores (txu.h:20), and a `.cfw` is not a `.txu` (txu.h:3).

## §6 Tests

- **T1** `loader::tests::txu_loads_its_app_part_like_the_app`: a `.txu` built in the test from a small `.app` at
  0x30000, with the app alone and with a host part, loads to the same entry, segments and RAM as the `.app` alone.
- **T2** `txu::tests::every_header_rule_is_refused_with_its_words`, `stage_checks_refuse_with_their_words`,
  `loader::tests::txu_refusals`: every rule of §2 is refused with its words: magic, hdr_len, flags, CRC, origin, kind,
  flash_addr, size, alignment, overlap, a kind twice, bytes behind the last part, key and iface consistency, the
  SHA-256, the `.bit` length, the IDCODE, the record check. A host-only `.txu` is refused by `run`.
- **T3** `txu::tests::write_into_matches_fw_write_parts`, `install::tests::install_writes_a_txu_like_the_host_app`:
  written into an erased flash and into a filled one, the regions are as in §4. A one-part package leaves the other
  region. A failing check writes nothing. A type-3 machine is refused. The host checks (`.bit` length, IDCODE, AXSS)
  run on a synthetic `.bit` with a sync word, an IDCODE packet and an AXSS packet.
- **T4** (with MultiCore's build, run by the MultiCore session like S45 T8):
  - A `.txu` from `tools/txu_pack.py` with the real host `.bit` and `hostapp.app`:
    `run --board c64u --icap --firmware X.txu --headless --script <png>` shows the host menu as `hostapp_emu.elf`
    does (measured here with the pre-release's `.txu`).
  - A core start and the IPROG back to the host reload the `.txu`.
  - `install --update X.txu --flash f.bin`: the bytes at 0 and at 0x220000 equal the parts. The host app's
    `.txu` flow on that flash reports the package as already installed.
- **T5** (the host app's installer, with MultiCore's build, run by the MultiCore session):
  1. Packages from `tools/txu_pack.py`: A and B with host and app, and C with the app only.
  2. `install --update A.txu` into an erased flash gives F.
  3. The host app runs on a copy of F and installs B from its menu, driven by a `--script`, which gives F1. With
     `--board c64u` the board build does it as well as `hostapp_emu.elf`.
  4. `install --update B.txu` on another copy of F gives F2.
  5. F1 and F2 are equal byte for byte in [0, 0x400000), the two slots. Bytes beyond the slots, such as config and
     `/flash`, are the run's own and are not compared.
  6. The same again with C in place of B, and with a package whose check fails: `install` writes nothing, and
     the host app writes nothing either.
- `sha2` 0.10 is added to ue2-core; the workspace already has it, through ue2-vfat and ue2emu. The CRC-32 is zlib's,
  a small table in `txu.rs`.

## §7 Docs

- `docs/status/install.md`: §2 adds `.txu` to the firmware list, and §5 gets a `.txu` subsection.
- `docs/FEATURES.md`: MultiCore.
- `docs/ARCHITECTURE.md`: the spec table, and the loader in the `ue2-core` row.

## §8 Decisions

- **D1 (owner, 2026-11-20):** §4 (`install`) is part of this spec, beside §3 (`run`): "dann kann man den installer
  auch deterministisch testen" (T5).
- **D2 (Alex via MultiCore, 2026-11-20):** IDCODE, FPGA type and the app's flash address are table-driven (50T and
  100T rows), so a universal 50T+100T package costs only a row and a format field later. The `.txu` is read as txu.h
  has it today; no field is invented.
