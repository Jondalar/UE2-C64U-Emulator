# S44 — The MultiCore core application runs in ue2emu as any core, and the host app starts cores and takes them back

**Status:** DONE (2026-11-07). S1-S6, S8-S10 and S12 pass in `cargo test`. S7 passes in `scripts/smoke-s44-cores.sh`
and S11 in `scripts/smoke-s44-host.sh`, both with MultiCore's kit at 4b181c16f (coreapp with the `trx_io_caps` fix,
the host app built with `HOSTAPP_EMU_FLASH`, vc20.cor and amiga1200.cor). Neither smoke is in `smoke-all.sh`: the
kit is not in this repo.
**Repo:** UE2 (u64-emulator). Hardware and software reference: TRX64U-MultiCore at ae84e44da:
- `host/ultimate_sys.vhd:1454-1507`: the ICAP loader.
- `coreapp/src/board.cc`, `hw.cc`, `start.cc`, `coreapp.cc`.
- `host-app/software/hostapp/handover.h/.cc`, `coreboot.h/.cc`, `legacy.h/.cc`.
- `app/video/videomode.h/.cc`.
- `host-app/software/io/c64/trx_io.cc` (`trx_io_alt_id`).
**Origin:** the MultiCore session (TRX64U-MultiCore), 2026-11-07. Every UI change of its host app and coreapp is to
run in ue2emu with the UX review before a human sees it, so coreapp must run as an A1200 first, then C128, Atari 800,
NES, Apple II and the rest. Owner: "ja, damit das mit der UX besser wird braucht die session mit ihren agents den
emulator, also bitte ja, mach 1-5 und dann 6."
**Depends on:** S37 (`--trx-io`, the pixel layer) and S41 (`--absent core`).

---

## §1 The problem

**Who:** the MultiCore session and its agents (T187: the core app menus, UX gate).

**What they are trying to do:** run coreapp, the one application image for every core, in ue2emu as the core it
describes. They drive it with scripted keys and take a `png` per page.

**What coreapp reads at start** (code read, ae84e44da):

1. **The ID word.** `hw_read_id` reads four bytes at `TRX_ID` = 0x101A0000. `core_find` picks the descriptor whose
   `d.id` is equal, for example "AMIG" (`coreapp/cores/amiga.cc:295`) or "C128". `trx_io_caps` takes the block as
   ours only for "TRX2" or for that ID (`trx_io_alt_id`). Then it reads CAPS at +0x004, which needs b6 for the pixel
   layer.
2. **The board profile.** `board_detect` reads the ICAP ID byte at 0x10060600. 0x7A
   (`BOARD_ICAP_ID`) means the MultiCore host: "Return to host menu" is offered and the settings live beside the .cor.
   Any other value means the monolith. `HwPartialPort::present` and `coreboot.cc core_icap_present` ask the same byte.
3. **The hand-over page** at `HANDOVER_ADDR` 0x03EFF000 (`struct Handover`):
   - magic "TRXH", version 1;
   - `handover_sum` over bytes 12 up to `id_sum`;
   - `video` = `video_pack(sys, hdmi)`;
   - the .cor path, `core_name`, `core_version`, `host_build`;
   - `id_sum` and `core_id`.

   `start_identify` uses it to choose between descriptors that share an ID word (A500 and A1200 are both "AMIG").
   For a shared ID without a name it uses the feature register (`id_feat_reg`, A1200: `AM_FEAT` b3 `FBWIN`).
   `video_init` takes the mode from it.
4. **The core's registers.** `CoreIo::rd/wr` (`hw.cc HwIo`) is `ioRead8/ioWrite8(TRX_IO_BASE + reg)`. Every core has
   its registers in the same 64 KB block as the host's pixel layer:
   - the A1200: 0x00C-0x1F3;
   - the C128: its own page 0xE000;
   - Apple II, Atari, NES and the rest: 0x0A0-0x1FF.
   The ID word, CAPS, IRQ_EN/IRQ_CAUSE and page 6 are the host's.
5. **The screen** is the pixel layer (S37), as for the C64 app.

**What UE2 does today:**
- The ID is the constant "TRX2", so `core_find` always picks the C64.
- 0x10060600 reads 0, so the profile is the monolith.
- The page is whatever DDR holds.
- Page 0 outside the C64's registers reads 0, and every write there is lost. S41 `--absent core` drops writes and
  reads 0.

**What the session needs:** coreapp started as any core, with the core's ID, the multicore profile, a valid hand-over
page and registers that read back. Later, the real flow: the host app starts a .cor, its coreapp runs, and "Return to
host menu" brings the host back (§7).

## §2 Scope

- **Phase A (§3-§6):** a core profile (`--core`), the ICAP loader (`--icap`) and the hand-over page (`--handover`).
  This is items 1-5 of the request.
- **Phase B (§7):** the host app's start of a layer-2 core (ICAP) and of a large core (IPROG), and the way back.
  This is item 6.
- **Not in this spec:**
  - The core's own machine. No A1200 runs; the core's picture (its frame buffers in DDR) stays black. Only the
    menu and the pixel layer are drawn.
  - The decouple's effect on the core's windows. On the board, while `core_decouple` is set, a request into the
    core gets an ack with 0 at once (`ultimate_sys.vhd:1553`). Here the bit reads back and the windows answer as
    before. Phase B models it, because the swap is where it counts.

## §3 The core profile (`--core FILE.toml`)

A TOML file per core. The parser is `crates/ue2emu/src/coreprofile.rs`; the model is
`crates/ue2-core/src/coreprof.rs`.

```toml
name = "amiga1200"          # for the log; default: the file's name
id = "AMIG"                 # the ID word at 0x101A0000-03, four printable characters
caps = 0xC0                 # CAPS at +0x004 [default 0xC0: b6 pixel layer, b7 RGB565]
board = "multicore"         # multicore: the ICAP loader is there (§4); monolith: it is not [default multicore]
[regs]                      # reset values: writes read back, a reset of the block restores these
0x0A0 = 0x01
[ro]                        # read-only: reads give this value, writes are dropped
0x00C = 0x18
[handover]                  # the page (§5), keys as in --handover
core = "amiga1200"
id = "amiga1200"
path = "/Usb0/cores/amiga1200.cor"
video = "1080p"
```

**The block with a profile** (`devices/trx.rs`, `CoreProfile::owns`):

| Offset | Answers |
|---|---|
| 0x000-0x003 | the profile's ID |
| 0x004 | the profile's CAPS |
| 0x010, 0x014 | IRQ_EN, IRQ_CAUSE as with the C64 core (frame start b6, flip b7: the pixel layer's) |
| page 6 (0x6000-0x6FFF) | the pixel layer, still capture and text layer, as S37 |
| every other byte | the core's register file: reads give the last value written, from the profile's reset values; `[ro]` bytes keep their value |

- None of the C64 parts is there with a profile: the gate, state bus, engine and debug block (far pages), the
  session, the slot block, reSID8, CAPS2, CAPS3 and BUILD. Their offsets are core registers.
- A device reset restores the reset values and keeps the profile.
- A profile entry on one of the host's offsets is refused, and so is an offset past 0xFFFF or a value that is not a
  byte.
- A register wider than a byte is written byte by byte (`0x1A4 = 0x78`, `0x1A5 = 0x56`, ...).
- `--core` needs the TRX block (`--trx-io`). A core that is not the C64 is normally run with `--c64 none`: with
  `--c64 trx64` the C64 still runs behind the cart and DMA registers, which the core application of another core
  never touches.
- `--absent core` (S41) still takes the whole core out at the bus, before the profile is asked.

**Which values a profile needs** is the core's business, not UE2's. A register that a core's application polls for a
state the gateware sets (a "ready" or "held" bit) needs that value as `[ro]`, or the application waits. The
MultiCore repo keeps the profiles of its cores; `docs/examples/core-amiga1200.toml` is the example here.

## §4 The ICAP loader (`--icap`)

`devices/misc.rs Icap`, after `ultimate_sys.vhd:1454-1507`. It is there with `--icap` or with a profile whose board
is `multicore`. Without it, the region reads 0 as before (Gideon's bitstream and the TRX bitstream).

| Offset | W | R |
|---|---|---|
| +00 | - | 0x7A |
| +04, +08 | inert (Gideon's W25Q reboot) | 0 |
| +10 | a bitstream byte while the enable is set; four bytes make a word | 0 |
| +14 | b0 the enable; when it rises, the count goes to 0; the byte phase restarts | b0 |
| +18-+1B | - | the word count, little endian |
| +20 | b0 the decouple request | b0 the request, b1 the decouple in effect (the request is its only source here) |
| +21 | b0 the expansion port release (T135 D6) | b0 |

A reset clears the enable and both requests and keeps the count. The bytes streamed since the enable last rose are
kept for phase B (§7.2).

## §5 The hand-over page (`--handover`)

`--handover path=...,core=...,id=...,version=...,host=<8 hex>,video=[SYS:]MODE` sets fields on top of the profile's
`[handover]` table; either one alone is enough.
- `SYS`: pal50 (the default), ntsc60, pal60 or ntsc50.
- `MODE`: 480p (also written 576p), 720p, 1080p, 800x600, 1024x768 or 1280x1024. `none` or `0` means no mode
  (video 0).
- These are `VID_SYS_*` and `VID_HDMI_*`, packed as `video_pack`: 0x80000000 | sys << 8 | hdmi.

The page is written into DDR after the image is loaded and before the first instruction, as `handover_fill` leaves
it:
- the 4 KB page is zeroed;
- magic and version are set;
- the strings are cut to their fields and terminated;
- `host_build` is the key in 8 lowercase hex digits, or empty for 0;
- `sum` is `handover_sum`;
- `core_id` and `id_sum` are set only when an id is given.

A string that does not fit its field with its terminator is refused, because `handover_valid` would reject the page.
The menu marker at +0xFF8 (T135 D3) is not written, as for a core start.

The path also says where coreapp looks for the core's folder (settings, apps, `roms/`): `/Usb0/...` needs a stick
(`--usb-dir`), `/SD/...` an SD image (`--sd`). Without the medium coreapp logs "Device is not ready to fetch children"
and finds no ROM files.

The console says what was set up:
```
core: amiga1200 (ID AMIG, CAPS C0, 1 register(s) set, 1 read-only)
handover: 0x03eff000: core "amiga1200", id "amiga1200", path "/Usb0/cores/amiga1200.cor", video 0x80000002
```

## §6 Screen and modes

The menu draws into the pixel layer (S37), which is placed in the host's 1920 × 1080 frame
(`video_frame_size`) whatever the HDMI mode. A run "at 480p, 720p or 1080p" is the hand-over's `video` word, which
coreapp takes for its own mode logic and layout. The `png` is the composite of the 1920 × 1080 frame, as the
scaler's input; the board's output scaling to 480p or 720p is not modelled.

## §7 Phase B: the host app starts a core and takes it back

Answers of the MultiCore session (2026-11-07):
- (a) the host app runs in ue2emu today (`tests/hostapp/emu_txu.sh`, built with `-DHOSTAPP_EMU_FLASH`), and the
  null core answers "TRX2" (`cores/null/null_regs.v:3`);
- (b) a large core starts through `legacy_jump`, below;
- (c) the core is found by SHA-256 against the manifests of the .cor files in `--cor-dir`;
- (d) IPROG with WBSTAR 0 boots the host bitstream and the host app again, with DDR2 kept.

### §7.1 What the board does

- **Layer-2 core** (`coreboot.cc`):
  1. `core_stage`: the application goes to 0x03000000 and the partial to 0x03400000.
  2. `core_swap`: DC=1, then wait for b1; EN=1; the partial bytes to +10; EN=0; the word count is checked.
  3. `core_jump`: DC=0, PORT_REL=1, the pixel layer off, interrupts off, `handover_fill` at 0x03EFF000, the
     trampoline at 0x03F00000 copies the application to its load address and jumps to its entry.
- **Large core** (`legacy.cc`):
  1. The bitstream is the file `/Flash/legacy/fullcore.bit` in the flash drive, with a stamp.
  2. The application is staged.
  3. `legacy_jump`: interrupts and network off; the boot magic in DDR2 (the entry at 0xFFF8, 0x1571BABE at
     0xFFFC); the trampoline copies the application to 0x30000; ICAP IPROG with WBSTAR = the file's flash address.
  4. The new bitstream's boot ROM (`host/bootrom/bootrom.c`) sees the magic and jumps to the entry.
- **The way back** (coreapp `hw_return_to_host`): `handover_menu_request` at +0xFF8, then IPROG with WBSTAR 0. The
  FPGA loads the host bitstream from flash address 0, and the host app boots. DDR2 keeps the marker, which the host
  reads and clears (`handover_menu_take`).

### §7.2 What UE2 does (`crates/ue2emu/src/multicore.rs`, `devices/misc.rs Icap`)

- **`--cor-dir DIR`**: the .cor bundles UE2 knows. For each one it reads the manifest (`tools/cor_pack.py`'s format):
  name, id, kind, and the SHA-256 and size of the partial or the full bitstream.
- **Profiles by core:** `--core-dir DIR` holds `<core id>.toml` (large cores) or `<manifest name>.toml`. A profile with
  the ID "TRX2" is the C64 core with all of its pages (and the C64 with `--c64 trx64`). A core without a profile
  halts the run with a report: the manifest does not carry the ID word.
- **Symbols:** after a core start another program runs, so the start image's symbols and fault hooks would fire at
  addresses of the old program (seen: a false `vAssertCalled` halt in coreapp under the host app's symbols). A
  profile's `symbols = "coreapp.elf"` (relative to the profile) gives the new program's symbols and hooks. Without it
  the hooks are off and the console says so; WBSTAR 0 loads the host image's symbols again. The get_mem PANIC loop is
  found only in the image `Machine::new` loaded.
- **No C64 in the new core:** the C64 port's power-up without a backend sets the TRX link's VIC frame count to 0, so
  the block times its frame starts on its own (§3 of S37). Otherwise the last count of the C64 that was swapped out
  stood, no frame start came and the pixel layer never flipped.
- **RESTORE:** `U64_RESTORE_REG` (0x10100402) reads 1 while RESTORE is held (`ultimate_sys.vhd:143`), which the
  cores' menu hotkey reads (`hw.cc` `keys_read`); before it read 0 always. The control language has `restore [ms]`
  (S08).
- **When UE2 acts:** every event of the loader ends the run slice right after the access that raised it, so the
  machine acts before the next instruction.
- **Layer-2 start:** when the enable falls after a stream, the SHA-256 of the streamed bytes is looked up among the
  partials. A match makes that core the block's profile, as a swap: the old core's state goes, and the new one
  comes up from its reset values. No match leaves the null core and logs the hash. While DC is set, the core's
  windows answer 0 at once.
- **IPROG** comes through the loader's data port (+10), not Gideon's +04/+08:
  - It is sent by `hw_return_to_host`, by `legacy_tramp.cc` and by `cfw_iprog_seq`.
  - The words are Xilinx configuration packets: dummy, sync 0xAA995566, NOP, a type-1 write of WBSTAR (0x30020001,
    one word), a type-1 write of CMD (0x30008001) with 0x0F (IPROG), NOP, NOP.
  - UE2 follows the stream as the configuration logic does: nothing until the sync word; type-1 headers; type-2
    payloads skipped, so FDRI data in a partial is never read as a command.
  - A CMD write of 0x0F reconfigures with the last WBSTAR. It acts on the word itself, because `legacy_tramp.cc` never
    lowers the enable.
  - **WBSTAR 0:** the machine boots the host firmware again (`--firmware`, its load and entry). The core goes back to
    the run's start profile (none: the null core, "TRX2"), the ICAP and the TRX block reset, the CPU and devices
    reset, and DDR is kept apart from the image's segments.
  - **WBSTAR ≠ 0:** the flash bytes from WBSTAR are hashed over each large manifest's bitstream size (record 5, the
    .bit as the host writes the flash file) and matched. A match makes that core the profile. The boot ROM's step
    follows: the boot magic at 0xFFFC is 0x1571BABE, so the CPU starts at the word at 0xFFF8, with DDR kept. No match,
    or no magic, halts with a report, the way a stall does (S41).
  - **Address map:** a core whose manifest map is `gideon_bl4` sees DDR with CPU address bits 6:3 permuted
    (`cfw_map_off`: ours[3] = theirs[6], ours[6:4] = theirs[5:3]). UE2 permutes the RAM once at the IPROG into the
    core, so the core's CPU sees it linearly, and back at the IPROG out of it. `identity` needs nothing.
- **Smoke:** host app → pick a .cor in the browser → coreapp up as that core → "Return to host menu" → host menu
  back with the marker taken. One layer-2 start and one large start.

Read for phase B (ae84e44da):
- `host/bootrom/bootrom.c:372-379`: the boot ROM checks the magic first. It clears it, prints "Magic: <addr>" and
  jumps to the word at 0xFFF8. Before that it only writes its probe pattern at 0x10000 (`test_access`), which UE2
  leaves out. Without the magic it loads the application from flash, which UE2 does not model: for WBSTAR 0 it loads
  `--firmware`.
- `legacy.cc:331, :405-468`: the flash file is the .cor's record 5 byte for byte, `ci.bit_off`/`ci.bit_len` from
  `c.off/size[COR_PART_FULL]`, checked against the manifest's bitstream SHA-256. So hashing the flash from WBSTAR over
  that length finds the core.

## §8 Scenarios

| # | Scenario | Proof |
|---|---|---|
| S1 | The page's bytes equal `handover_fill`'s: layout (388 bytes), lowercase host key, `handover_sum` and `id_sum` worked by the C loop; a changed byte fails `handover_valid` | `coreprof::tests::layout_and_sums_match_handover_cc` |
| S2 | No id leaves `id_sum` and `core_id` 0; no host key leaves `host_build` empty | `coreprof::tests::no_id_no_id_sum_and_no_host_key` |
| S3 | `--handover` keys, video names, refusals (unknown key, a string without room for its terminator) | `coreprof::tests::handover_spec_and_video` |
| S4 | A profile answers its ID, CAPS 0xC0, CAPS2/3 0; `[ro]` keeps its value; registers read back across page 0, 1 and E; GATE_CTRL is a core register; IRQ_EN and page 6 stay the host's; a reset restores the profile's values | `trx::tests::a_core_profile_answers_for_its_core` |
| S5 | The ICAP loader as `core_swap` drives it: ID, decouple b1, no byte before the enable, count, rising enable clears it; off reads 0 | `misc::tests::icap_loader_as_core_swap_drives_it`, `icap_off_reads_zero` |
| S6 | Profile files: keys, decimal and hex offsets, refusals (ID length, host offsets, beyond 0xFFFF, not a byte, unknown board or key); `--handover` goes over the profile; a monolith profile has no ICAP | `coreprofile::tests` |
| S7 | coreapp (current build) runs as the A1200: "board: multicore (ICAP 0x7A, hand-over valid)", "AMIG amiga1200, cor /Usb0/cores/amiga1200.cor", the menu in the pixel layer at 480p, 720p and 1080p | `scripts/smoke-s44-cores.sh COREAPP` |
| S8 | The loader's events: one per decouple change; a partial's stream on the falling enable; FDRI data that looks like CMD IPROG is no command; the IPROG of `hw_return_to_host` / `cfw_iprog_seq` acts on the word, with the enable still up; a stream with an IPROG is no partial | `misc::tests::icap_events_iprog_partial_and_decouple` |
| S9 | An event ends the slice after the store; `reconfigure` resets the devices and starts the CPU at the entry; the gideon_bl4 remap is `cfw_map_off` and keeps 0xFFF8-0xFFFF in place | `machine::tests::icap_events_end_the_slice_and_an_iprog_reconfigures` |
| S10 | `.cor` manifests of both kinds (DFX: partial, large: bitstream, map, id); a file that is not a bundle is refused; no profile without `--core-dir` | `multicore::tests::manifests_of_both_kinds` |
| S11 | Host app (`HOSTAPP_EMU_FLASH`) → a DFX .cor and a large .cor from the stick → coreapp up as that core → its hotkey (CTRL or C=+RUN/STOP, then RESTORE) → F8 "Return to host menu" → IPROG WBSTAR 0, the host app again with the marker taken ("marker 1"); without `--core-dir` the run halts | `scripts/smoke-s44-host.sh KIT` |
| S12 | A core without a C64 clears the VIC frame count: the TRX block's own frame starts follow (fails without the fix) | `machine::tests::a_core_without_the_c64_clears_the_vic_frame_count` |
