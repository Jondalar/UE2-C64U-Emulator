# S21 — Firmware settings from a .cfg file

A fresh flash starts with the firmware defaults: REU off, Command Interface off (so no UCI), Freeze UI unless UE2
seeds the overlay. Tests that need other settings keep a pre-configured flash file around. Firmware 3.15 loads a
`.cfg` next to a started PRG/CRT/disk (`ConfigIO::S_load_associated_config`), the C64U firmware 1.x does not, and
neither helps before the first menu.

S21 adds `run --settings FILE.cfg` (repeatable; TOML key `settings`). Before the firmware runs, UE2 writes the
settings the file names into the flash config pages, at every start, as if the menu had saved them. The file is the
firmware's own text format, so a `.cfg` saved on hardware works, and it only needs the settings that differ.

```
[C64 and Cartridge Settings]
RAM Expansion Unit=Enabled
Command Interface=Enabled

[User Interface Settings]
Interface Type=Freeze
```

## 1. How the firmware keeps settings

- A store registers with `register_store(page_id, name, defs)` (config.cc:89-185). Its page is the first of the 24
  config pages (4 KiB sectors from 0xFE8000, 512-byte logical page, w25q_flash.cc:251-267) whose first word is the
  page id; if none has it, the first erased page (id 0xFFFFFFFF) is claimed and written with the defaults.
- After the id come records `id type len payload` up to id 0xFF (`ConfigStore::pack`, config.cc:305-325, 706-758):
  ENUM 1 byte, VALUE 4 bytes big-endian, STRING/STRFUNC/STRPASS the characters. FUNC, SEP and INFO are not stored.
- Reading (`ConfigStore::unpack`, config.cc:398-424; `ConfigItem::unpack` 671-704) walks the records, a length that
  does not fit ends the walk, a type mismatch is skipped, a value outside `min..max` becomes the default, and a later
  record with the same id wins.
- Several stores can share a page: "Audio Mixer", "SID Sockets Configuration", "UltiSID Configuration", "SID
  Addressing" and "U64 Specific Settings" all live on `U64C` (u64_config.cc). Their item ids do not overlap.

The flash holds ids only. The names, the choices of an enum and the ranges are in the firmware image.

## 2. Names from the image

The definitions are arrays of `t_cfg_definition` (config.h:87-96, 28 bytes on RV32: `id`, `type`, 2 bytes padding,
pointers to the item text, the format and the choice list, `min`, `max`, `def`), ended by id 0xFF or type 0xFF.
UE2 scans the loaded firmware segments for them:

- an entry has zero padding, a type 1-8, an item text that is a printable C string (empty for a separator) and a
  format string; the first entry of a table has a `%` in its format;
- an ENUM's choices are the strings at `items[min..=max]`;
- a STRING type's `def` is a pointer to the default text.

This works on an ELF, an `.app` and a `.ue2` alike, symbols are not needed. Measured on 3.14, 3.15 (`update.ue2` of
this tree) and C64U 1.1.0: 197 items in all three, none with a different id; 3.15 adds 33, C64U has 4 of its own.
The choice lists do change: `LedStrip Mode` index 4 is "Rainbow Sparkle" in 3.14 and C64U, "Programmatic" in 3.15.
So the choices always come from the image that runs.

## 3. Stores

The store name and page id are arguments at the call site, not in the table. UE2 keeps a catalog, taken from the
firmware source and checked against the call sites in all three images (the `lui/addi` pairs that load the page id,
the name and the table next to each `register_store` call):

| Store | Page id | Anchor item |
|---|---|---|
| C64 and Cartridge Settings | `C64 ` 0x43363420 | RAM Expansion Unit |
| User Interface Settings | `GEN.` 0x47454E2E | Interface Type |
| Network Settings | `NET\0` 0x4E455400 | Host Name |
| WiFi settings | `WIFI` 0x57494649 | Connected to |
| Ethernet Settings | `Netw` 0x4E657477 | Use DHCP |
| Tape Settings | `TAPE` 0x54415045 | Tape Playback Rate |
| Keyboard Lighting | `DREW` 0x44524557 | LedStrip Pattern with the choice "Circular" |
| LED Strip Settings | `LEDS` 0x4C454453 | LedStrip Mode |
| Data Streams | `Data` 0x44617461 | Stream VIC to |
| Speaker Mixer | `U64D` 0x55363444 | Speaker Enable |
| Audio Mixer | `U64C` 0x55363443 | Vol UltiSid 1 |
| SID Sockets Configuration | `U64C` | SID Socket 1 |
| UltiSID Configuration | `U64C` | UltiSID 1 Filter Curve |
| SID Addressing | `U64C` | SID Socket 1 Address |
| U64 Specific Settings | `U64C` | System Mode |
| Modem Settings | `MODM` 0x4D4F444D | Modem Interface |
| SoftIEC Drive Settings | `IEC\0` 0x49454300 | IEC Drive |
| Printer Settings | `MPS\0` 0x4D505300 | IEC printer |
| Drive A Settings | 0x10020000 (`DRIVE_A_BASE`) | Drive Type |
| Drive B Settings | 0x10024000 (`DRIVE_B_BASE`) | Drive Type |

A store's table is the one that contains its anchor (and the choice, where given). Where two tables qualify, a table
that another store already has to itself is taken out: "Keyboard Lighting" claims its table by the choice, "LED
Strip Settings" gets the other one; "WiFi settings" claims its table by "Connected to", "Ethernet Settings" the
other; "Speaker Mixer" by "Speaker Enable", "Audio Mixer" the other. Drive A and B share one table. A store whose
anchor no table has does not exist in that firmware ("Keyboard Lighting" in 3.15).

Store names match case-insensitively, as `ConfigManager::find_store` does.

## 4. The file

Read as `ConfigIO::S_read_from_file` / `S_read_store_element` read it (configio.cc:227-411):

- lines end at LF, CR is dropped; a line starting with `#` or `;` is a comment; empty lines (also only blanks) are
  skipped;
- `[name]` starts a store section;
- `Item=Value`: the item name is trimmed and matched case-insensitively; an ENUM value matches a choice
  case-insensitively ignoring outer spaces (`" ~45"` = `~45`); a VALUE is a decimal integer (`%d`); a string is taken
  exactly as written after the `=`.

Where UE2 is stricter than the firmware, because a wrong start is worse than a refused one:

| Case | Firmware | UE2 |
|---|---|---|
| store not on this machine, item not in the store | skipped, logged | warning, skipped |
| no `=`, no item name, item outside a section | line fails | error |
| ENUM value not a choice | line fails | error, the choices listed |
| VALUE outside `min..max` | taken, reset to default at the next boot | error |
| string longer than `max` | truncated | error |
| FUNC, SEP or INFO item | silently nothing | error (not a setting) |

An error stops the start before the firmware runs. Several files apply in order, and a later setting of the same item
wins.

## 5. Writing

In `Machine::new`, after the firmware is loaded and after the overlay seed (so `Interface Type=Freeze` in a file
beats the seed):

- per page: the page with the id if one exists, else the first erased page, as `register_store` would pick;
- records of the ids being set are replaced in place (a later duplicate is dropped), new ones are appended before
  the 0xFF;
- the result has to fit in the 512-byte logical page, else error; the rest of the sector is untouched;
- a flash image is written back at once; a volatile flash is simply seeded again at the next start;
- each setting is printed as `settings: [Store] Item=Value`, warnings as `settings: warning: ...`.

A store the firmware registers only on some hardware (Drive B) gets its page anyway; the firmware ignores a page no
store claims.

## 6. `ue2emu settings`

`ue2emu settings [--firmware F]` prints every setting of the image as a `.cfg`: stores in catalog order, each item
with its default, the choices of an enum and the range of a value in a comment line above it. Copy the lines needed.

## 7. Code

| File | Change |
|---|---|
| `crates/ue2-core/src/settings.rs` (new) | table scan, catalog, `.cfg` parser, resolution to records; `stored`/`dump`/`cfg` read the pages back out for the monitor (S23 §6) |
| `crates/ue2-core/src/devices/flash.rs` | `SpiFlash::write_settings` (page edit) |
| `crates/ue2-core/src/machine.rs` | `MachineConfig::settings`; apply in `Machine::new` |
| `crates/ue2emu/src/main.rs` | `run --settings`, `settings` subcommand |
| `docs/status/install.md`, `docs/examples/ue2emu.example.toml` | the flag |

## 8. Not built

- The SID device stores (SwinSID, FPGASID, ARMSID, PDsid, SIDKick): registered per detected chip, some keep their
  settings in the chip.
- "Machine Monitor Bookmarks", "Audio Output Settings" (Ultimate-II only).
- Changing settings while the firmware runs (a control command).
- Settings given inline on the command line.

## 9. Acceptance

1. `cargo test --workspace` green; `cargo build -p ue2emu --no-default-features` clean.
2. Unit tests: the parser rules and the strictness table of §4; the table scan on a synthetic image; the store
   resolution with two LED tables; the page edit (existing page, erased page, duplicates, shared page, too full).
3. On the upstream ELF: every catalog store except "Keyboard Lighting" resolves; REU 0xC3, Command Interface 0x71 and
   Interface Type 0x08 have their source ids; the `U64C` stores have disjoint ids.
4. `ue2emu settings` on 3.14, 3.15 and C64U 1.1.0 lists every store the image has.
5. Headless, fresh flash, upstream ELF: a file with `Interface Type=Freeze` gives the freeze menu without
   `--no-overlay-ui`; REU and Command Interface enabled show in the firmware's own view of the settings.
6. The same on C64U 1.1.0: the Command Interface comes up.

## 10. Measured (2026-09-18)

- `ue2emu settings`: upstream ELF (v3.15-9) and 3.15 `update.ue2` 19 stores ("Keyboard Lighting" absent), 207 and
  208 settings; 3.14 and C64U 1.1.0 all 20 stores, 198 and 201 settings.
- Fresh flash, REU 16 MB and Command Interface enabled: the firmware's cart init prints `REU: 01. REU_SZ: 07, UCI:
  01` on the upstream ELF and on C64U 1.1.0 (without the file `REU: 00 ... UCI: 00`), and the upstream menu shows
  the three values (`scripts/smoke-settings.ctl`).
- `Interface Type=Freeze` without `--no-overlay-ui`: the freeze menu (`Frozen on Bad line`, browser on `c64screen`).
- Tests: workspace 449 pass; `scripts/smoke-all.sh` passes with the new `settings` run.
