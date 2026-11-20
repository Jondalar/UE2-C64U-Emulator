# S47 — The C64U's flash and installs on a 50T: W25Q128JV, `--flash-chip`, `install --caps/--board`

**Status:** BUILT (2026-11-20). T1-T6 pass in this repo; T7 is the MultiCore session's.
**Repo:** UE2 (u64-emulator). References:
- Firmware: `software/io/flash/w25q_flash.cc` and `.h`, `s25fl_l_flash.cc` and `.h`, `application/update_u2p/update_u64ii.cc` and
  `update_common.h`.
- The Winbond W25Q128JV datasheet, revision F (2018-03-27): §7.1 the status registers, tables 7.1.8/7.1.9 the
  protection, §8.2 the instructions.
- TRX64U-MultiCore `main` at 5f41ebc32:
  - `vendor/gideon/software/application/update_u2p/update_trx.cc`, the tester updater.
  - `host-app/software/hostapp/flashlock.h` and `flashlock.cc` (T152), the volatile lock.
  - `docs/c64u-flash-layout.md`, the dump of Alex's C64U.

**Origin:** 2026-11-20. Alex wants MultiCore's real update files tested end to end in ue2emu before a release. MultiCore's
tester updater (update_trx.cc, `ci/update.sh`) has two checks:
- It wants a W25Q128 (update_trx.cc:2123-2132). ue2emu only had the S25FL128L, so the updater said "Not the expected
  flash (W25Q128). Nothing written."
- It wants FPGA type 2 (update_trx.cc:2133-2136). `install` always ran on type 3.

Its emulator test build accepts the S25FL128L but still stops at the type. **Depends on:** S06 (the SPI flash), S46
(installs of a `.txu`).

---

## §1 What is known about the boards

| Board | Flash | Source | FPGA |
|---|---|---|---|
| C64U | W25Q128JV, JEDEC `EF 40 18` | measured on Alex's board: MultiCore `docs/c64u-flash-layout.md` (flash dump) and `docs/archive/board-tests/board-tests-pending.md:134` (the host's log: "JEDEC ef4018, status 34 01") | XC7A50T: IDCODE 0x0362C093 in Commodore's c64u_v1.1.0 update (`docs/research/c64u-fpga-vs-gideon.md:65`) |
| Ultimate 64 Elite II | not measured; MultiCore's docs count it as the C64U's mainboard | — | 50T or 100T |
| 100T machine | not measured; 100T machines are showing up at users (Alex, 2026-11-20) | — | XC7A100T |

ue2emu has modelled the S25FL128L (JEDEC `01 60 18`) since S06, because the board's chip was open
(`docs/hw/06-spi-flash-config.md` Q1). For the C64U that question is now answered.

## §2 Choosing the chip (D1)

- **`--flash-chip s25fl128l|w25q128`**, on `run` and `install`, and as `flash-chip` in a `--config` file. Without a
  board model, this is how the chip is picked.
- **The board model picks what is known** when `--flash-chip` is not given:
  - `--board c64u`: W25Q128.
  - `--board u64ii`, or no `--board`: S25FL128L, as before. The Elite II's chip is not measured.
- An explicit `--flash-chip` wins over the model.
- The start-up line names the chip, where it came from and the FPGA type, for example
  `flash: W25Q128 (--board c64u), FPGA type 2` (`ModelArgs::apply`, crates/ue2emu/src/main.rs).

**The FPGA type follows the model too (D2):** `--board c64u` puts FPGA type 2 (bits 29:28) into the default capability
word, 0x24000226; the rest of the word is unchanged and the frontend still adds its own feature bits. An explicit
`--caps` is used as it stands.

**The 100T machine (D4)** needs no model of its own: the default word is FPGA type 3, and either chip goes with it.
- `--flash-chip w25q128`: a 100T with the W25Q.
- `--board c64u --caps 34000226`: a C64U with a 100T, which keeps the model's W25Q.

**Older images:** a flash image made with `--board c64u` before S47 changes in two ways:
- Its settings sit in the S25FL128L's config pages, so the firmware no longer finds them all.
- With firmware 3.15, `/flash` moves from 0x580000 to 0x400000.

`--flash-chip s25fl128l --caps 34000226` keeps such an image as it was. install.md says so.

**`--c64-roms` on a 50T** (crates/ue2emu/src/c64roms.rs `locate_flash_disk`): every firmware on FPGA type 2 mounts
`/flash` at 0x400000, so a volume at 0x580000, left by a type-3 run, is no longer taken there. The fallback to the 50T
region stays for type 3 only, where firmware before 3.15 keeps `/flash` at 0x400000.

## §3 The W25Q128JV model

It is a second personality of the S25FL128L model in `devices/flash.rs` (`FlashChip`, `MachineConfig::flash_chip`). The
controller and the 16 MiB array stay the same, and so do 256-byte pages and 4 KiB sectors.

**Identity:** RDID `9F` gives `EF 40 18`. `W25Q_Flash::tester` takes it (w25q_flash.cc:125-176, capacity 0x18: 4096
sectors), and `get_type_string` gives "Winbond W25Q128" (w25q_flash.cc:182-196).

**Commands,** as the firmware's W25Q driver (w25q_flash.h:10-26) and MultiCore's flashlock send them:

| Opcode | Command | Notes |
|---|---|---|
| `03`, `0B` | read, fast read | 3-byte address; `0B` with one dummy byte |
| `02` | page program | wraps within the page |
| `20`, `52`, `D8` | erase 4 KiB, 32 KiB, 64 KiB | |
| `C7`, `60` | chip erase | |
| `06`, `04` | write enable, write disable | |
| `50` | write enable for volatile status register | good for the next status register write only |
| `05`, `35`, `15` | read SR1, SR2, SR3 | repeated on every byte of the frame |
| `01` | write SR1, plus SR2 when a second byte follows | other byte counts are not executed (datasheet §8.2.5) |
| `31`, `11` | write SR2, SR3 | one byte |
| `4B` | read unique ID | four dummy bytes, then the 8-byte UID (w25q_flash.cc:236-249) |
| `66`, `99` | reset enable, reset | as the S25FL128L model; the reset also loads the non-volatile status bits |

The S25FL128L's 4-byte forms (`0C`, `12`, `13`, `21`, `53`, `DC`) and its `07` are no W25Q commands: their frames do
nothing. BUSY always reads 0, as in the S25FL128L model: every operation is complete within the access.

**Status registers and protection,** as in the datasheet with WPS = 0:
- **SR1:** SRP, SEC, TB, BP2:0, WEL, BUSY.
- **SR2:** SUS, CMP, LB3:1, QE, SRL.
- **SR3:** WPS, DRV1:0 and reserved bits.
- A write changes SRP SEC TB BP2:0, CMP LB3:1 QE SRL and DRV1:0 WPS (§8.2.5; SRP from §7.1.1's table). LB3:1 never
  go back from 1 to 0. BUSY, WEL, SUS and the reserved bits read 0.
- After `50` a write changes only the volatile copy and WEL stays. After `06` it changes both copies and clears WEL.
  Without either nothing changes.
- A program or erase that touches a protected address is not executed. The datasheet says nothing about WEL for such
  a command; no cycle runs, so WEL stays. The range follows tables 7.1.8 and 7.1.9, including CMP = 1, which protects
  the complement. Example: TB = 1, BP = 101 protects 0x000000-0x3FFFFF, the range flashlock.h:4-6 expects. SEC = 1
  with BP = 110 is in neither table; the model takes 32 KiB, as for BP = 10x.
- **SRL = 1** (power-supply lock-down) refuses every status register write until power-off, which is the end of the
  ue2emu process. An IPROG resets only the controller, so the lock-down survives it, as on the board.
- **SRP** refuses writes only while /WP is low; the model's /WP is high, so SRP alone does nothing.
- **Not modelled:**
  - The one-time program lock: the datasheet enables it with a prefix command `AA 55` by special order (§7.1.1, note
    2), not with SRP = 1 and SRL = 1. MultiCore's flashlock guards against SRP + SRL anyway.
  - WPS = 1, the block locks (`36`/`39`/`3D`/`7E`/`98`) and the security registers. WPS is stored, but the protection
    stays the BP scheme. No firmware here sends these commands, and their frames do nothing.

**Power-up (D3):** every run starts with SR1, SR2 and SR3 at 0, unprotected. The status registers are not kept in the
image or beside it. That is the state every known flow leaves on a board:
- Gideon's `protect_disable` writes 0 (w25q_flash.cc:390-424).
- His `protect_configure` writes only when the status already equals the target (w25q_flash.cc:462), so it never
  changes one.
- MultiCore's flashlock writes the volatile copy only.

**Config pages:** the W25Q driver has 16 config pages (`W25Q_NUM_CONFIG_PAGES`, w25q_flash.h:8). They are the last
16 sectors, page p at 0xFF0000 + p × 0x1000 (w25q_flash.cc:256-283). The S25FL128L has 24, at 0xFE8000
(s25fl_l_flash.h:9). The dump of Alex's C64U agrees: "config: 16 of 24 sectors" (c64u-flash-layout.md:12). Every
place in ue2emu that finds a config page follows the chip (`FlashChip::config_page_at`):
- the overlay-UI seed;
- `config_pages` and `config_page`, for the monitor's `config` and `devices`;
- `--settings` (`SpiFlash::write_settings`);
- the C64 store page of `--c64-roms` (c64roms.rs `store_page`).

The S25FL128L's 24 pages end with the W25Q's 16: its page 8 is the W25Q's page 0.

**The flash layout** (bitstream, application and `/flash`) does not depend on the chip. Both drivers use
w25q_flash.cc's tables chosen by FPGA type (w25q_flash.cc:30-60). `--c64-roms` keeps choosing by capabilities.

**The monitor's `flash`** names the chip and its JEDEC ID; for the W25Q it adds the three status registers. Its
`config pages` line says how many of the chip's pages are in use.

## §4 `install --caps`, `--board`, `--flash-chip`

`install` takes the three options with the meaning they have on `run` (§2). Its machine is then built with that
capability word, board model and chip; until now it always used MachineConfig::new's defaults. The later step
`--c64-roms` uses the same capability word and chip.

Example, a C64U with Commodore's update:

```sh
ue2emu install --board c64u --update c64u_v1.1.0.ue2 --flash f.bin --yes
```

**MultiCore's tester updater** passes its two checks on `--board c64u` (W25Q128, XC7A50T), also in its board build.
It then needs an SD card or a USB stick with room for the cores, which `install` does not attach. MultiCore runs it
the way a user does: `run --board c64u` with the vendor firmware, the update started from a USB stick
(MultiCore `tests/updater/emu_t135.sh`).

## §5 Not in this spec

- **A 100T update package.** MultiCore's updater and its `.txu` are 50T only (update_trx.cc:6 and 2133; S46 §2). The
  machine is here (D4); a 100T package is MultiCore's work, and S46 keeps the FPGA table-driven for it.
- **The Elite II's chip** stays S25FL128L until someone measures it.
- **The `.txu`:** that is S46.

## §6 Tests

Unit tests in crates/ue2-core/src/devices/flash.rs, crates/ue2emu/src/main.rs, install.rs and c64roms.rs.
- **T1** `w25q_identity_for_the_testers`: the frames of `W25Q_Flash::tester` give `EF 40 18`. The S25FL-K and S25FL-L
  testers reject the W25Q, and the W25Q tester rejects the S25FL128L.
- **T2** `w25q_read_program_erase`: read, fast read, page program, the three erases and both chip erases, WEL, the
  UID; the 4-byte forms do nothing.
- **T3** (protection):
  - `w25q_protection_table`: every row of tables 7.1.8 and 7.1.9.
  - `w25q_status_registers_and_protection`: the SR1/SR2/SR3 writes after `06` and after `50`, the read-only and LB
    bits, the reset, an ignored program and an ignored erase.
  - `w25q_flashlock_holds_until_power_off`: MultiCore's flashlock sequence reads back `34 01`; a program at 0x100000
    is ignored; Gideon's `protect_disable` frames change nothing; a controller reset keeps the lock; a new machine
    starts at 0.
- **T4** `w25q_config_pages_at_ff0000` and `c64_roms_store_page_selects_the_names`: config pages at 0xFF0000 (16) on
  the W25Q and at 0xFE8000 (24) on the S25FL128L, for the overlay seed, `config_pages`, `--settings` and the
  `--c64-roms` store page.
- **T5** `the_model_picks_chip_and_fpga`, `the_model_from_a_config_file`, `install_takes_the_model`: the chip and the
  FPGA type from `--board` and `--flash-chip` (§2), on `run`, `install` and in a `--config` file. `--caps` wins over
  the model.
- **T6** (firmware, this repo; measured 2026-11-20):
  - 3.15 `ultimate.elf` with `--board c64u` on a fresh image: scripts/smoke-flash-1.ctl saves Color Scheme = C128
    Style, scripts/smoke-flash-2.ctl finds it in the next run. The pages are at 0xFF0000, nothing is at
    0xFE8000-0xFEFFFF, and `/flash` is the 50T volume at 0x400000.
  - `install --board c64u --update c64u_v1.1.0.ue2` passes: popups, power-off, the app at 0x220000. `run --board
    c64u` boots it to Commodore's menu, with the config pages at 0xFF0000.
  - The 100T with the W25Q: `install --flash-chip w25q128 --update update.ue2 --c64-roms` (an upstream U64-II update)
    puts the app at 0x3C0000 and `/flash` at 0x580000, and 3.15 keeps its config pages at 0xFF0000.
- **T7** (MultiCore's session, with its build):
  - The tester updater (`update_trx.cc` without `TRX_EMU_TEST`) in MultiCore's emu_t135 flow with `run --board c64u`:
    it passes the flash and FPGA checks, writes the bitstream at 0 and the app at 0x220000, and reaches "Update OK" and
    the power-off. Its `.cfw` restore bundle passes the same way.
  - The T152 lock in `run --board c64u`: the host's log line reads "JEDEC ef4018, status 34 01", and a write into
    0x000000-0x3FFFFF is ignored until the next run.

## §7 Docs

- `docs/hw/06-spi-flash-config.md`: Q1 answered for the C64U, plus the W25Q personality.
- `docs/status/install.md`: `--flash-chip`, the options on `install`, and older images (§2).
- `docs/FEATURES.md` and `docs/ARCHITECTURE.md`: the spec table and the `flash.rs` row.

## §8 Decisions

- **D1 (Alex, 2026-11-20):** the chip comes from the board model as far as it is known, or without a model from the
  chipset: "beides wäre sinnvoll … also per Modell und dann was wir wissen oder OHNE modell per chipset".
- **D2 (from D1, "per Modell … was wir wissen"):** `--board c64u` also means FPGA type 2.
- **D3 (Alex, 2026-11-20, Q1):** the non-volatile status bits are not kept between runs; every run starts at 0, with no
  sidecar file.
- **D4 (Alex via MultiCore, 2026-11-20):** the 100T machine is built too: FPGA type 3 with either chip. T53 ("no 100T
  in the tester package") is lifted on MultiCore's side; a universal 50T+100T package format is theirs to define.
