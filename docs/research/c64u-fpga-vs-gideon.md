# C64U 1.1.0 FPGA vs Gideon U64-II 3.15

Status: research note, 2026-09-14. Read-only analysis; no repository was modified.

Scope:
- How far Commodore's C64 Ultimate (C64U) FPGA bitstream from update 1.1.0 is from Gideon's U64-II 3.15 FPGA.
- How the app-to-FPGA register interface differs.
- Whether Gideon's 3.15 `ultimate.app` can run on Commodore's FPGA.
- Whether "3.15 brought large cartridge support, otherwise the C64 core barely changes" holds.

Tags:
- **[V]** verified from a file, an offset, a commit or command output.
- **[E]** estimate or inference.

Repo shorthand: `1541u` = `firmware/1541ultimate`. Local checkout: `b617777c` = `v3.15-9-gb617777c`. The tag `v3.15` is `68b78f50` (2026-09-08).

---

## 1. Short answer

1. **The Commodore bitstream is Gideon's own build [V].**
   - `c64u_v1.1.0.ue2` has exactly one bitstream. It is byte-identical to `1541u/external/u64_mk2_artix.bit` at commit `e87ac3d8` (2026-02-28, "Initial attempt to support 1351 mouse over USB").
   - It is the XC7A50T variant. There is no Commodore-specific FPGA logic in 1.1.0.
   - The C64U-specific features (keyboard "Bling" LEDs, LED-strip extensions, HDMI Tx Swing) are app-side writes to blocks that already exist in Gideon's design.
2. **The physical FPGA delta is small; the functional delta is not.**
   - Physically: the pinout, I/O standards, MMCM/PLL/GTP settings and 33 of 37 BRAM contents are identical. Resources grow by about +2.4 % LUT, +3.3 % FF, +4 DSP48 and +1 BRAM18 [V].
   - Functionally: 16 distinct 50T images were built after `e87ac3d8` up to `fd756470` (15 up to the `v3.15` tag) [V].
3. **Gideon's 3.15 app does not run correctly on the C64U 1.1.0 FPGA as built.** There are two independent blockers:
   - **CPU:** the 3.15 app is compiled for `rv32im` and contains 337 multiply instructions. The C64U's rvlite CPU predates the multiply extension and silently executes `MUL` as `ADD` and `MULHU` as `SLTU`, without a trap [V source, E runtime effect].
   - **Cartridge DDR contract:** 3.15 puts cartridge ROM at DDR `0x03C00000`. The C64U FPGA almost certainly reads it at `0x00F00000` [V app side, E strong FPGA side].
   - **What still matches:** register windows and identity reads, and there is no version gate in the app [V].
   - **What a rebuild would fix [E]:** an app built with `MARCH=rv32i` and the old cart linker symbols removes both blockers. Commodore-specific LED/Bling features and all post-February FPGA fixes would still be missing.
4. **"Only large cartridge support changed" is refuted [V commit list, E content].** Between the C64U snapshot and 3.15 the FPGA gained:
   - RV32M in the CPU and a new boot ROM.
   - Badline timing fixes and badline elimination, eDMA-during-badline fix, overlay fix.
   - Cartridge compatibility mode, bridge/REU timing, REU start delay removal.
   - C64-side LED control with the VIC-key unlock mechanism.
   - Large carts plus Megabyter, moved USB DMA tags, and FENCE as NOP.
   - After the tag: an Ultimax rendering fix.

   The app-visible register map, by contrast, barely changed. The breaking changes are the CPU instruction set and the cart ROM address, not new registers.

---

## 2. Sources and method

| Input | Location | Notes |
|---|---|---|
| Commodore update | `<path>/c64u_v1.1.0.ue2` | Bitstream record near `0x28CBC`; app record at `0x23FFBC`, header `{0x30000, 0xFA760, 0x30000}` |
| Gideon bitstreams | `1541u/external/` and its git history (38 historical 50T revisions) | Fork update `<1541ultimate checkout>/update.ue2`: 50T at ~`0x2F64C`, 100T at ~`0x24694C` |
| Gideon app | `1541u/target/u64ii/riscv/ultimate/result/ultimate.elf` (local build of `b617777c`) | Code `0x30000`–`0x1035BC` |
| Toolchain | `tools/bin/riscv32-unknown-elf-{objdump,readelf}` | |
| Bitstream decode | prjxray `bitread` + `bit2fasm` (as in `docs/research/fpga-emulation.md` §2.3) | |

Method:
- **Bitstream:** all 38 historical 50T bitfiles plus the C64U image were decoded to FASM. The scripts (`scan.py`, `hdr.py`, `rawdiff.py`, `fasmdiff.py`, `bramdiff.py`, `boot2.py`, `pins.py`, `series.py`, output `series.txt`) are in the session scratchpad under `bitdiff/`, which is not persistent.
- **App:** the Commodore app was disassembled as a raw binary with `objdump -D -b binary -m riscv:rv32 --adjust-vma=0x30000`. A simple extractor (`mmio.py`) tracks `lui`/`addi` constants and records every load, store and pointer into `0x10000000`–`0x10FFFFFF`. The same heuristic ran on both apps.
  - **Limit:** accesses through object-member base pointers or computed indices are not resolved. This covers drive and I²C classes, the LED RAM index and the timing struct copy.

---

## 3. Identity of the Commodore bitstream [V]

- **Location and size:** `c64u_v1.1.0.ue2` contains one `.bit`. Its header is at `0x28CBC`, data starts at `0x28D29` (2,192,012 bytes), and the only sync word is at `0x28D59`.
- **Device:** IDCODE `0x0362C093` = XC7A50T (`xc7a50tfgg484`). The design name is `u64_mk2_artix`, built `2026/02/28 17:51:28`. The update carries no 100T image.
- **Match:** `cmp` against `git show e87ac3d8:external/u64_mk2_artix.bit` is identical, sha256 `992c65786730160d50bba48109f184de945fc570f12a42ff3179a009c8165999`. The build time is 36 minutes before the commit time (18:27 +0100).
- **Changelog match:** the Commodore 1.1.0 FPGA items (USB mouse, CIA timing, turbo-off after reset, ACIA fixes) correspond to Gideon commits `fa3abc4e`, `433c53c8` (2026-01-11), `e24d911b`…`12a4a556` (01-28..31) and `e87ac3d8` (02-28).
- **The fork's `update.ue2`:**
  - 50T image (sha256 `9ff97291…`) and 100T image (IDCODE `0x03631093`, sha256 `93e18ee9…`).
  - Both are byte-identical to `external/u64e2_50t.bit` / `u64e2_100t.bit` at `fd756470` (2026-09-10).
  - That is after the tag: `v3.15` carries the `883f608d` 50T image, sha256 `0962f99c…`.

Consequence: every statement about "the Commodore FPGA" below is a statement about Gideon's `e87ac3d8` design, whose open-source parts can be read at that commit.

---

## 4. FPGA differences: C64U (`e87ac3d8`) vs 3.15 (50T)

The 3.15 side is measured on `fd756470` (HEAD 50T). The `v3.15` tag image `883f608d` has almost the same utilisation (`series.txt`: LUT 23,611, FF 18,537, DSP48 41).

### 4.1 Raw bits say nothing about design distance [V]

- **Set bits:** 2,079,840 (C64U) vs 2,112,392 (HEAD). Only 618,432 are common.
- **Differing bits:** 2,955,368 — 82.7 % of the union of set bits, or 16.9 % of all configuration bits.
- **Frames:** 4,696 of 4,772 used frames differ, including all 949 BRAM-content frames.
- **Why this is not a distance measure:** every consecutive Gideon rebuild differs by 75.1–83.6 % (`series.txt`), which is place-and-route churn. The only 0.0 % pair is `eaa7751b`→`bbd254f7`, where the file was renamed but not rebuilt.

### 4.2 FASM features at the same location with the same value [V]

| Class | C64U | HEAD 50T | Identical |
|---|---|---|---|
| Routing PIPs | 336,640 | 339,810 | 6.8 % |
| LUT INIT | 23,276 | 23,844 | 1.4 % |
| FF / mux | 78,880 | 81,265 | 40.9 % |
| CARRY | 8,203 | 8,261 | 74 % |
| BRAM INIT | 2,144 | 2,180 | 13 % |
| DSP config | 592 | 660 | 43 % |
| IOB | 1,259 | 1,259 | **100 %** |
| IOI | 3,565 | 3,556 | 99.2 % |
| GTP | 519 | 519 | **100 %** |
| BUFG | 380 | 380 | **100 %** |
| CMT | 234 | 236 | 97.9 % |

- **CMT:** only 8 HCLK_CMT `BUFHCLK` routing choices differ. All MMCME2/PLLE2 parameter features match.
- **Location-independent check:** the 664 MMCM + PLL + GTP parameter features are identical across C64U-50T, HEAD-50T and HEAD-100T.
- **LUT-INIT multiset Jaccard vs C64U:** 0.24–0.26 for every revision back to May 2025. Placement noise dominates, so this metric cannot discriminate.

### 4.3 Utilisation [V]

| Design | LUT positions | FF | DSP48 | RAMB18 halves configured | RAMB18 with INIT |
|---|---|---|---|---|---|
| C64U (`e87ac3d8`) | 23,276 | 17,889 | 37 | 132 | 37 |
| HEAD 50T (`fd756470`) | 23,844 (+2.4 %) | 18,472 (+3.3 %) | **41** | 126 | 38 |
| HEAD 100T | 23,831 | 18,496 | 41 | 162 | 38 |

- XC7A50T capacity is 32,600 LUT, 120 DSP48 and 150 RAMB18. Both designs use about 72 % of the LUTs.
- Unknown bits: 235 (C64U) vs 233 (HEAD). 214 of them sit at identical positions, 176 of those in segment `0x1580`.
- **DSP48 history:** 37 in all 21 builds up to and including `e87ac3d8`, and 41 from `7b8de2d0` on. `7b8de2d0` is the first 50T rebuild after `0cee3a59` (2026-03-21, "Upgrade to Risc-V CPU: Added multiply instructions"). The +4 DSP48 are the RV32M multiplier (§5.6).

### 4.4 BRAM contents [V]

- **Shared contents:** 33 of 37 C64U halves have identical content in HEAD, at different locations. They include:
  - The 12×24 overlay font (C64U `BRAM_L_X30Y130`, HEAD `BRAM_R_X37Y85`).
  - An 8×8-style glyph table, six pattern tables and 20 all-ones halves.
- **rvlite boot ROM** (answers `fpga-emulation.md` §9 question 3):
  - It is stored in 2 × RAMB36 as 16-bit lanes.
  - C64U lane 0 (`BRAM_L_X30Y10`) matches `bootrom_u64ii_pkg.vhd@e87ac3d8` in 822/822 non-zero words, but the HEAD package in only 106.
  - HEAD 50T (`BRAM_R_X37Y20`) matches the HEAD package in 842/842 words.
  - The two packages differ in 754 of 2048 words (`094d1674`, 100T support). The new ROM first appears in the 50T bitfile at `604d2058`.
  - The upper lane is not decoded.
- **Extra RAMB18 in HEAD:** one 18-bit RAMB18 with a repeating `0x000E`-style init. It first appears at `eaa7751b` (large cartridges). Probably a cart bank/map table [E].
- **Version constants are not in BRAM:** ITU `g_version` (0x22 → 0x25) and `C64_CORE_VERSION` (`C64_IO_BASE+0x10`, `u64.h:118`) are LUT logic.

### 4.5 I/O and clocking [V]

- **Pins:** all 250 IOB-configured package pins have identical standard, drive, slew, pull and IN_ONLY settings in C64U-50T vs HEAD-50T, and by package pin also vs HEAD-100T. The board wiring contract is the same.
- **IOB history:** the IOB hash has been constant from `e87ac3d8` to HEAD. The last change was `bbf0bd32`→`e87ac3d8`: 10 bank-14 pins (T21 U21 U22 V22 W21 W22 Y21 Y22 AA20 AB21) went from LVCMOS33 DRIVE 4 SLOW outputs to IN_ONLY. Their board function is unknown.
- **Clocking:** the MMCM/PLL hash has been constant since `b77e8af7` (2025-11-15), and the GTP hash across all 38 revisions. Power-on clocking is identical; video modes are retuned at runtime over DRP.

### 4.6 Functional FPGA changes after the C64U snapshot [V commit list; E content]

`git log e87ac3d8..HEAD -- external/u64e2_50t.bit external/u64_mk2_artix.bit` lists 17 commits. `bbd254f7` is byte-identical to `eaa7751b` (a rename), so there are **16 distinct new 50T images**. 15 of them come before the `v3.15` tag, and `fd756470` comes after it. In total there are 297 commits between `e87ac3d8` and `v3.15`.

| Date | Commit | Change | Area |
|---|---|---|---|
| 03-21 | `0cee3a59` (source), first image `7b8de2d0` | rvlite gains `MUL`/`MULH*` (DSP48 37→41) | SoC CPU |
| 03-29/30 | `7b8de2d0`, `15ae6a8a`, `98d3b429` | [ISSUE-665] badline timing fixes, badline elimination | C64 core (VIC) |
| 04-11 | `f2a14e51` | eDMA during badlines when turned off | C64 core |
| 04-19 | `8b0ad2e9` | Overlay rendering fix on U64E2 | Video |
| 05-02 | `c4be69a2` | Cartridge compatibility mode | C64 core / cart |
| 05-11 | `77f3b381` | Bridge timing and REU | C64 core / REU |
| 05-14..17 | `6bfc38e9`, `72d6be8c`, `16e14651` | C64-side LED control, VIC-key mechanism, extra functions | C64 core / IO |
| 07-25 | `eaa7751b` | Large cartridge images (`g_max_cart_bits`, cart ROM at `0x3C00000`) | Cart |
| 07-30 | `bbd254f7` / `094d1674` | 100T support, new boot ROM | Boot |
| 08-09/10 | `a749334d`, `604d2058` | USB DMA tags `0x0B`→`0x14`, V1.4E-123 | USB, core version |
| 08-28 | `59594060` | REU without initial delay (turbo mode) | REU |
| 09-02 | `70c831a3` | Version 0x124 | |
| 09-03 | `883f608d` (`8a4e5771` source) | FENCE executes as NOP | SoC CPU |
| 09-10 | `fd756470` | Ultimax cartridge rendering fix (after `v3.15`) | C64 core / cart |

- **Source visibility:** the open `fpga/` diff is 46 files, +711/−2787, mostly the mblite removal. The C64 core itself lives in Gideon's private `ult64` tree (`fpga-emulation.md` §3.1), so its changes are known only from commit messages and the bitstream.
- **Not a U64-II change:** `99524bcd` (2026-05-10) removed the multiplier only for the U2+L Lattice build (`u2p_riscv_lattice.vhd`, `target/u2plus_L`). The U64-II keeps it: DSP48 stays 41 and the 3.15 Makefile says `rv32im`.

---

## 5. Register interface differences

### 5.1 Identical in both apps [V]

Both apps use identical address sets and structurally 1:1 access sites in these windows:
- ITU, Drive A/B, IEC, UCI, CartTiming, Sampler, ACIA, EEPROM.
- SD SPI, SPI flash, TRACE, RTC timer, GCR, ICAP, AudioSel.
- RMII, WiFi UART, C2N play/record, U2PIO, MATRIX, HW I²C.
- Overlay registers and RAM, HDMI palette, VIC cropper, C64 palette, PLD, DEBUG, GLYPH, UltiSID, UDP, MMCM.

Example of a matching site: `getFpgaCapabilities` is at `0x33AFC` in Gideon's app and `0x339FC` in Commodore's.

Extractor totals: Gideon has 2161 hits and 342 distinct R/W addresses; Commodore has 2153 hits and 378.

### 5.2 Written only by the Commodore app

| Address | Block | Commodore app code | Present in Gideon's FPGA? |
|---|---|---|---|
| `0x10100645`–`0x101006FB` (39 extra constant addresses; 92 writes vs 39) | LED strip `0x10100600` | `0x88BC8`–`0x89BFC`; strings "LedStrip Controller", "Music Detect", "Case Lights" | Yes [V]. Gideon writes the same RAM through an index (`led_strip.cc:150-174`, `led_strip.h:25-28`) |
| `0x10100900`–`0x101009FF` (14 addresses, 43 writes), `0x10100803` | Bling Board keyboard LEDs | `0x8A650`–`0x8B6E0`; "Bling Controller", "Keyboard Lights" | Yes: the bitstream is Gideon's. Gideon defines `U64II_BLINGBOARD_LEDS` (`u64.h`) but never accesses it |
| `0x10144011` | HDMI timing `tx_swing` | `0x8DE3C`, config "HDMI Tx Swing" | Yes. Gideon writes it only inside the timing struct (`u64.h:190`); `CFG_HDMI_TX_SWING 0xAB` exists without a menu item (`u64_config.cc:184`) |
| `0x10080618` (read) | USB nano area | `0x78C6C` | Yes. A driver-side difference |

### 5.3 Used only by Gideon's 3.15 app

| Address | Function | Source | Needs FPGA newer than `e87ac3d8`? |
|---|---|---|---|
| `0x1018002F` `C64_VIC_SPLIT` | Written with 0 on every LED effectuate | `led_strip.cc:413,422` (`6406b877`) | **Yes**, `16e14651` (05-17). On the C64U: undecoded or alias, unknown [E] |
| `0x10180031/34/35/37` | Port-2 joystick-out, paddle, mouse enable | REST API (`0f2d10ee`) | Probably no: the Commodore app uses port-1 siblings `0x30/32/33/36`, so the block exists [V]; port 2 is likely present [E] |
| `$D038` unlock via high IRQ 6 | `U64Config::unlock_irq` | `8596576f` | **Yes** (VIC-key mechanism, 05-14..17). IRQ source likely absent [E] |
| `0x10100541`–`547` | Speaker mixer mute | `u64_mute_sids` (`e546f5bb`) | No, the block already exists |
| `0x1004000D`, `0x10188000/01` | `C64_SERVE_CONTROL`, BASIC ROM window | Machine monitor (`f91b7799`) | No, the register dates from 2016 |
| `0x10100407`, `0x1010040C` | BLACKBOARD read, direct `LEDSTRIP_EN` | `Assembly::connect_to_server`, `LedStrip::run` | No [E] |

### 5.4 DDR contract: cartridge ROM base (breaking) [V app side; E strong FPGA side]

- **Linker change:**
  - `e87ac3d8` `target/u64ii/riscv/ultimate/linker.x:277-278` has `__cart_rom_start = 0x00F00000`, `__cart_rom_limit = 0x01000000`.
  - `v3.15` and HEAD `linker.x:284-287` have `__updater_limit = 0x03C00000`, `__cart_rom_start = 0x03C00000`, `__cart_rom_limit = 0x04000000`.
- **Binary evidence:**
  - Commodore app: `lui 0xF00` ×7, `lui 0x3C00` ×0.
  - Gideon app: `lui 0x3C00` ×8, `lui 0xF00` ×0. The sites are `C64::set_cartridge`, `C64_CRT::load_crt`, `FileTypeCRT::execute_st`, `ControlTarget::parse_command` and REST `run_crt`.
  - All other fixed DDR bases are identical in both apps: kernal, drive areas `0xEB..0xEF0000`, REU `0x01000000`, RAM disk `0x02000000`, updater `0x03000000`.
- **FPGA side:**
  - At `e87ac3d8`, `slot_server_v4.vhd:19` defaults `g_rom_base_cart` to `X"0F00000"`, and `ultimate_logic_32.vhd:845` hard-wires `X"0F00000"`. There is no `g_max_cart_bits`, so the window is 1 MB.
  - At HEAD, `g_max_cart_bits` exists. The open Lattice/NIOS tops set `g_rom_base_cart => X"3C00000"`, `g_max_cart_bits => 22` (`u2p_riscv_lattice.vhd:603-604`, `u2p_nios2_solo.vhd:530-531`).
  - The U64-II top is private. Together with the Commodore app's `0xF00` constants, its value on the C64U is `0x0F00000` [E strong].
- **No gate in the app:** the maximum size comes from the linker symbols (`c64.h:432-435`, `c64_crt.cc:292,668`). Megabyter IDs 86/87 write type `0x0F` straight to `C64_CARTRIDGE_TYPE`.
- **Paths that read `get_cartridge_rom_addr()`:**
  - All `.crt` loads (`filetype_crt.cc:69`, REST).
  - Freezer and utility carts (`c64.cc:1245-1256`).
  - Built-in SID/MUS player carts (`c64.cc:1285-1287`, `filetype_sid.cc:69-84`).
  - The Run-PRG boot cart in `C64_Subsys::dma_load` (`c64_subsys.cc:591,620-621`).
  - Kernal-from-cart (`c64.cc:1340-1342`).

### 5.5 Version and feature gating [V]

- **Same identity reads, same sites:**
  - Capabilities `0x1000000C-F`: Gideon `0x33AFC`, Commodore `0x339FC`.
  - `FPGA_VERSION` `0x1000000B`.
  - `BOARDREV` `0x1010000C`.
  - `C64_CORE_VERSION` `0x10180010`: Commodore `0x44F74`, `0x97DAC`, `0x98C40`, `0x9B4C0`.
- **Version reads are display-only:** in the Commodore app they feed only sprintf ("C64U FPGA version:  V1.%02X", "Ultimate FPGA core: 1%02X"). In Gideon's source they appear only in `product.cc:132,139`, `system_info.cc:174-176`, `routes.cc:199-202` and `socket_dma.cc:595-598`.
- **Gating is by capability bits** (`itu.h:49-73`). `BOARDREV` selects PLL/I²C channel and the Elite/joyswap option (`product.cc:42-81`, `u64ii_init.cc:152,164`). `getProductId()` is compile-time Elite II for `U64==2` (`product.cc:84-97`).
- **Gideon already knows the C64U:** "C64U Specific Settings" (`u64_config.cc:943`) and `.CFW` "Commodore compatible multi-target firmware" (`filetype_u2p.cc:78-84`).
- **No refusal logic in the 1.1.0 app:** neither app refuses to run on any FPGA version, board revision or core version. Commodore's changelog says a future update "may introduce safeguards"; nothing like that is in the 1.1.0 app.

### 5.6 CPU instruction-set contract (breaking) [V source; E runtime effect]

- **3.15 app is `rv32im`:**
  - Build flags: `target/u64ii/riscv/ultimate/Makefile:6` has `MARCH ?= rv32im` at HEAD and at `v3.15`; it was `rv32i` at `e87ac3d8`. The change came with `0cee3a59`.
  - ELF: `readelf -A ultimate.elf` reports `Tag_RISCV_arch: "rv32i2p0_m2p0"`.
  - Disassembly: `objdump -d` finds 331 `mul` and 6 `mulhu` (no div/rem, `-mno-div`) in 186 functions. They include boot-path code such as `IndexedList<InitFunction>::sort`, `W25Q_Flash::read_config_page` / `get_sector_size`, `mount_volume` and `uxTaskGetSystemState`.
- **Commodore app is `rv32i`:** 0 M-extension words (opcode `0x33`, funct7 `0x01`) in its code range `0x30000`–`0xDBF64`, and 7 such bit patterns in data.
- **What the C64U CPU does with M instructions** (`fpga/cpu_unit/rvlite/vhdl_source/decode_comb.vhd@e87ac3d8`):
  - The header (lines 4-8) says "RV32I+Zicsr … many others are not [marked illegal]".
  - For `ALU_REG` (lines 140-146), `alu_operation <= func3` (line 85) and `alu_func <= func7(5)`. `func7(0)` is never checked, and `reg_write_sel` stays at the `c_decoded_nop` default `WB_ALU` (`core_pkg.vhd:87`).
  - So `MUL` (func3 000) computes `rs1+rs2`, and `MULHU` (func3 011) computes the RV32I func3-011 operation, `SLTU` [E for the ALU mapping, which follows the standard encoding].
  - There is no illegal-instruction trap.
  - `0cee3a59` adds exactly the missing branch (`if inst_func7(0) = '1' and g_mult` → `WB_MUL_L/H`).
- **Expected effect [E]:** silent arithmetic corruption from the first multiply on, likely during init, config-page access or filesystem mount. A clean trap message is unlikely.
- **FENCE:** there are no opcode-`0x0F` words in either app's code. The local build does not depend on `8a4e5771`; the official release binary was not checked.

### 5.7 USB DMA tag move [V scope, E effect]

`a749334d` touches only four VHDL files (`ultimate_logic_32.vhd`, `usb_host_nano.vhd`, `usb_memory_ctrl.vhd`, an ECP5 test top). It changes tags on the internal memory bus. No software file changed, so the app has no visible dependency on it.

---

## 6. Can Gideon's 3.15 app run on the C64U 1.1.0 FPGA?

**Verdict: not as built.** An app-only swap hits the CPU mismatch first, and cartridge-backed functions would fail even if the CPU matched.

| Aspect | Result | Confidence |
|---|---|---|
| Board, pins, clocks, register windows | Compatible: identical IOB/CMT/GTP config and identical MMIO windows and identity reads | [V] |
| App refuses the FPGA? | No, there is no version or product gate | [V] |
| Boot handoff | The C64U boot ROM (`e87ac3d8` package) loads the app from SPI flash `0x220000` (`bootloader_u64ii.c:173`). HEAD uses `0x220000` too, except `fpgatype_id==3` → `0x3C0000` (line 176). The open bootloader has no signature check; the upper ROM lane is not decoded | [V source, E for the ROM content] |
| RV32M instructions | **Blocker**: 337 M instructions executed as ADD/SLTU | [V source, E effect] |
| Cartridge ROM at `0x03C00000` | **Blocker** for CRT, freezer, SID/MUS player, Run-PRG boot cart, kernal-from-cart | [E strong] |
| Large carts > 1 MB, Megabyter/TwoMegabyter | Not possible: no `g_max_cart_bits`, no cart type `0x0F` | [V source] |
| `C64_VIC_SPLIT` writes, unlock IRQ 6 | Features absent; possible side effects unknown | [E] |
| Keyboard/Bling LEDs, Music Detect, model LED patterns, HDMI Tx Swing | Lost: this is Commodore app logic that Gideon's app does not drive | [V addresses, E effect] |
| FPGA fixes after 02-28 | Absent: badline, eDMA, overlay, cart compatibility, bridge/REU, Ultimax | [V list] |
| Flash config pages | Collision risk between Commodore and Gideon config IDs not analysed | open |

**What an app-only build for this FPGA would need [E]:**
1. Build with `MARCH=rv32i`. The variable is `?=`, and the `e87ac3d8` build used `rv32i` with the same `LIBMARCH`.
2. Restore `__cart_rom_start/limit = 0x00F00000/0x01000000` in `linker.x`. This caps carts at 1 MB.

The unknowns in the table above stay.

**Reverse and full-update directions (facts only):**
- **Commodore's `rv32i` app on Gideon's 3.15 FPGA:** the CPU is a superset, so there is no instruction problem [V]. But the Commodore app would still place carts at `0x00F00000`, while the 3.15 U64-II top very likely reads `0x3C00000` [E].
- **Gideon's own 3.15 update:** it brings its own 50T bitstream, whose pin, IO-standard and clock configuration is identical to the C64U image [V]. That removes both blockers, but none of Commodore's app-side C64U features would be present. Not tested on hardware.

---

## 7. "3.15 brought large cartridge support, otherwise the C64 core barely changes"

**Refuted as stated; partly true in a narrower sense.**

- **True [V]:**
  - The physical footprint barely moved (+2.4 % LUT, +3.3 % FF).
  - Pinout and clocking are unchanged since the C64U snapshot.
  - The app-visible register map gained almost nothing (`VIC_SPLIT`, unlock IRQ).
  - Large cartridge support is the one change that altered the DDR memory contract.
- **False:**
  - **C64 core [V commit messages, E content]:** at least six rebuilds after `e87ac3d8` touch it — VIC badline timing and badline elimination, eDMA during badlines, cartridge compatibility mode, bridge/REU timing, REU start delay, and the Ultimax rendering fix (after the tag).
  - **Non-C64 parts that matter more for compatibility:** the RISC-V CPU gained multiply (+4 DSP48) [V], the boot ROM was replaced (754/2048 words) [V], USB DMA tags moved [V], and FENCE became a NOP [V].
- **Resulting incompatibility:** the change that breaks Gideon's app on the C64U is the CPU instruction set (March 2026), not large carts. Large carts add the second break.

---

## 8. Contradictions between the two analyses, resolved

| Topic | Analysis A (bitstream) | Analysis B (app/registers) | Resolution |
|---|---|---|---|
| Commodore-specific FPGA changes | None: byte-identical to `e87ac3d8` | "Commodore-side changes unknown" | Byte identity settles it: there are none in 1.1.0 [V] |
| App-only swap verdict | Blocker: MUL | "Boots and mostly works [E strong]" | B did not check the instruction set. Makefile, ELF attributes, objdump counts and `decode_comb.vhd@e87ac3d8:140-146` confirm A. Verdict is "not as built" [V source] |
| `g_rom_base_cart` | Existed at `e87ac3d8` with `X"0F00000"` | "New in the large-cart commits" | `git grep` at `e87ac3d8`: `slot_server_v4.vhd:19`, `ultimate_logic_32.vhd:845`. Only `g_max_cart_bits` is new [V] |
| Rebuild count | "15 50T rebuilds up to fd756470" | 16 commits listed, `70c831a3` missing | 17 commits touch the 50T file after `e87ac3d8`; one is a rename, leaving 16 distinct images up to `fd756470` and 15 up to `v3.15` [V] |
| U64-II cart top value `X"3C00000"` | — | "presumably" | Still [E]: the U64-II top is private; only Lattice/NIOS tops show it [V] |
| Multiplier removal `99524bcd` | — | — | Affects only U2+L (Lattice). U64-II keeps it (DSP48 41, `rv32im`) [V] |

---

## 9. Open questions

1. What do the `e87ac3d8` design's capability word, `FPGA_VERSION` and `C64_CORE_VERSION` read on real C64U hardware? The C64U System Info shows them.
2. On hardware, does the C64U slot server read cart ROM at `0x00F00000`? A test: load a CRT with the Commodore app, then dump `0x00F00000` vs `0x03C00000`.
3. How does the 3.15 app fail on a pre-multiply rvlite: hang before video, garbage UI, or flash-config corruption? This matters for any hardware experiment, because `W25Q_Flash::write_config_page`-adjacent code contains `mul`. It could be modelled in `ue2emu` with a pre-M decode.
4. Is `0x1018002F` decoded or aliased in the `e87ac3d8` C64 core, and is high IRQ 6 driven there?
5. Which board function belongs to the 10 bank-14 pins that became IN_ONLY at `e87ac3d8`?
6. What does the upper 16-bit lane of the boot ROM contain, and does the Commodore boot path check anything beyond the open `bootloader_u64ii.c`?
7. What is the 18-bit RAMB18 with the `0x000E` pattern (large carts)?
8. Do Commodore and Gideon config item IDs collide in the flash config pages?
9. Does the official `v3.15` release binary contain FENCE, and does its M-instruction count differ from the local `b617777c` build?
10. Do Commodore updates after 1.1.0 ship a newer Gideon bitfile revision?

---

## 10. Reproduction

```sh
R=firmware/1541ultimate
# bitstream identity
cmp <(dd if=c64u_v1.1.0.ue2 bs=1 skip=$((0x28CBC)) count=2192121 2>/dev/null) <(git -C $R show e87ac3d8:external/u64_mk2_artix.bit)   # re-run 2026-09-14: identical
# CPU contract
grep -n MARCH $R/target/u64ii/riscv/ultimate/Makefile; git -C $R show e87ac3d8:target/u64ii/riscv/ultimate/Makefile | grep MARCH
tools/bin/riscv32-unknown-elf-readelf -A $R/target/u64ii/riscv/ultimate/result/ultimate.elf
tools/bin/riscv32-unknown-elf-objdump -d $R/target/u64ii/riscv/ultimate/result/ultimate.elf | awk '$3=="mul"||$3=="mulhu"' | wc -l
git -C $R show e87ac3d8:fpga/cpu_unit/rvlite/vhdl_source/decode_comb.vhd | sed -n '140,147p'
git -C $R show 0cee3a59 -- fpga/cpu_unit/rvlite/vhdl_source/decode_comb.vhd
# cart ROM contract
git -C $R show e87ac3d8:target/u64ii/riscv/ultimate/linker.x | grep cart_rom
git -C $R grep -n g_rom_base_cart e87ac3d8 -- fpga
# rebuild series
git -C $R log --oneline e87ac3d8..HEAD -- external/u64e2_50t.bit external/u64_mk2_artix.bit
```

The scratch artifacts in the session scratchpad are not persistent:
- Bitstream work: `bitdiff/` (`series.txt` and the scripts).
- App work: `cbm_app.bin`, `cbm.dis`, `gid.dis`, `mmio.py`, `gid.mmio`, `cbm.mmio`, `cbm.strrefs`, `xref.py`.

Related documents: `docs/research/fpga-emulation.md` (bitstream toolchain, §2.4 BRAM, §9 open questions) and `docs/hw/00-memory-map.md` (register map).
