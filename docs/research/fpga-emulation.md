# FPGA emulation study: U64-II / C64 Ultimate

Status: research study, 2026-09-13. It is not a spec and nothing here is implemented.

**Question.** How far can ue2emu get towards emulating the FPGA itself, rather than modelling its registers
behaviourally? What is inside the FPGA, what source exists for it, what the shipped bitstreams are, which
paths exist, what each path costs and yields, and what the licensing situation is.

**Legend**

| Tag | Meaning |
|---|---|
| **[V]** | Verified in this study: file, git, bitstream decode, tool run, or a fetched primary source |
| **[S]** | From a cited third-party source, not re-checked beyond the citation |
| **[E]** | Estimate or inference. The derivation is given or referenced |

**Conventions.**
- Paths are relative to `firmware/1541ultimate/` unless they start with `crates/`, `docs/` or `/`.
- Firmware clone HEAD at the time of writing: `b617777c` (2026-09-12).
- Bitstream tool runs were made on scratch copies. No project file other than this document was changed.

---

## 1. Short answer

1. **The whole U64-II design cannot be simulated from source.** The C64 machine is closed, and so is the
   integration layer [V]:
   - C64 machine: VIC-II, PLA, 6510 port, UltiSID.
   - Integration layer: top level, video/HDMI, clocking, DDR2 PHY, U64 IO page, audio mixer, UDP streamer.
   - About two thirds of the firmware-visible register blocks do have open RTL candidates in the GPLv3 repo:
     rvlite, IEC processor, USB, RMII, I2C, UART-DMA, cartridge/REU, drives and overlay [E].
2. **The shipped bitstreams are fully readable** [V]:
   - Plain Vivado `.bit` files: not encrypted, not compressed, no authentication, single image.
   - Project X-Ray decodes them to FASM in about 13 s with about 0.01 % unexplained bits.
   - BRAM contents come out byte-exact. The overlay font ROM matches the open `font_pkg.vhd` in 1024/1024 entries.
3. **Turning that FASM into a simulatable netlist is possible in principle, but costly** [V/E]:
   - It is not an off-the-shelf flow. fasm2bels has no MMCM or DSP support and does not list GTP; the design
     uses 2 MMCMs, 27 DSP tiles and all 4 GTPs [V].
   - Estimated cost: 2–4 person-months to boot the RISC-V to the UART prompt [E].
   - Estimated speed: 10⁻³ to 2·10⁻⁵ of real time [E], i.e. one emulated second takes 17 min to 14 h.
   - It can serve as an offline oracle. It cannot be a runtime.
4. **Recommendation** (§8):
   - Keep today's behavioural models plus TRX64 as the runtime.
   - Use the open RTL as a **test oracle** in CI (rungs L1/L2), and mine the bitstream for facts (L1b).
   - Run RTL live only for a concrete fidelity problem (L3).
   - The gate-level netlist (L6) is a separate research project.

---

## 2. What ships: the bitstreams

### 2.1 Where they are [V]

A `.ue2` is one `.app` record: `{load_addr, length, start_addr}` as 32-bit LE, then the payload
(`crates/ue2-core/src/loader.rs:119`). The bitstreams sit in the updater's rodata
(`software/application/update_u2p/update_binaries_u64ii.s`).

| Image | `.bit` header (file offset) | Bitstream data | Sync word `AA995566` | Design; tool | Part | Build date | Data length |
|---|---|---|---|---|---|---|---|
| `update.ue2` #1 | `0x2F64C` | `0x2F6B9` | `0x2F6E9` | `u64_mk2_artix;UserID=0XFFFFFFFF;Version=2024.1` | xc7a50tfgg484 | 2026/09/10 11:00:57 | 2,192,012 B (`0x21728C`) |
| `update.ue2` #2 | `0x24694C` | `0x2469BA` | `0x2469EA` | same | xc7a100tfgg484 | 2026/09/10 11:05:11 | 3,825,788 B (`0x3A607C`) |
| `c64u_v1.1.0.ue2` | `0x28CBC` | `0x28D29` | `0x28D59` | same | xc7a50tfgg484 | 2026/02/28 17:51:28 | 2,192,012 B |

**Relation to the files in the repo.**
- The two bitstreams in `update.ue2` are byte-identical to `external/u64e2_50t.bit` (sha256 `9ff97291…`) and
  `external/u64e2_100t.bit` (`93e18ee9…`).
- The C64U image is a different build of the **same top-level design** (`u64_mk2_artix`).

**The `.swp` files are not byte-swapped.** `target/u64ii/riscv/update/Makefile:143-147` only runs `cp` on the
`.bit`. The swap step (line 149) is commented out, a leftover from the Altera U64 mk1 flow.

**How the updater installs them** (`software/application/update_u2p/update_u64ii.cc:108-110,174-180`):
1. Detect the FPGA type (`XC7A50T` / `XC7A100T`).
2. Write the bitstream to SPI flash at `0x000000`.
3. Write `ultimate.app` to `0x220000` (50T) or `0x3C0000` (100T).

**U64 mk1** ships as `external/u64.sof` only: Quartus 18.1, Cyclone V 5CEBA4F23C8, binary.

### 2.2 Configuration packet decode [V]

The packets were decoded against the UG470 register tables. All three bitstreams have the same structure.

| Property | Finding | Consequence |
|---|---|---|
| IDCODE | `0x0362C093` = XC7A50T, `0x03631093` = XC7A100T | Exact part confirmed |
| Frame data | One Type-2 FDRI write: 547,420 words (5,420 frames × 101 words) on 50T; 955,864 words on 100T. No MFWR writes | **Uncompressed.** The length equals the nominal device size, so file size says nothing about utilisation |
| Encryption | `CTL0=0x501` has DEC=0, SBITS=00, and there is no CBC/IV write | **Not encrypted, no readback restriction, no authentication** |
| Fallback | ConfigFallback=1 (disabled), WBSTAR=0, TIMER=0, no IPROG | Single image. No golden or multiboot image |
| CRC | Enabled (50T U64-II `0x3F5D1844`, C64U `0x0953E23E`, 100T `0x95017C57`) | Matters only if a bitstream were modified. Simulation does not need that |

### 2.3 What the design uses: Project X-Ray decode [V]

Setup: `prjxray` `bitread` was built locally, and `prjxray-db` was sparse-checked out for `xc7a50tfgg484-1` and
`xc7a100tfgg484-1`. `utils/bit2fasm.py` then took 12.9 s (50T) and 14.0 s (100T).

**Unexplained bits** (about 2.14 M bits are set in the 50T bitstream):

| Bitstream | Unknown bits |
|---|---|
| U64-II 50T | 233 |
| C64U | 235 |
| 100T | 232 |

**Where the 233 bits are (U64-II 50T):**
- 185 in RIOB33 tiles. Probably the DDR2 SSTL18 I/O standard, which prjxray-db does not document [E].
- 39 in DSP tiles.
- 9 in CMT, CFG and BUFG tiles.

Resources used by the U64-II 50T build (from FASM). Device totals are from the prjxray tilegrid and DS180:

| Resource | U64-II 50T | C64U 50T | XC7A50T total |
|---|---|---|---|
| LUT positions with INIT (includes route-thrus) | 23,844 (≈73 %) | 23,276 | 32,600 |
| Flip-flops (ZINI features) | ≈18.5k–18.9k | ≈17.9k | 65,200 |
| LUTRAM / SRL LUT positions | 1,362 / 352 | 1,347 / 349 | – |
| RAMB18 halves in use | 108 | 107 | 150 (75 RAMB36) |
| DSP tiles configured | 27 | 23 | 60 tiles (120 DSP48E1) |
| MMCME2_ADV / PLLE2_ADV | 2 / 2 | 2 / 2 | 5 CMTs |
| GTPE2_CHANNEL (+ GTPE2_COMMON) | **4 (+1)** | **4 (+1)** | 4 |
| Variable IDELAYE2 / ISERDES in DDR mode | 18 / 18 | – | – |
| ODDR / OSERDES | ≥50 / ≥42 | – | – |
| PHASER (MIG DDR3 PHY) | **0** | 0 | – |
| BSCAN | JTAG_CHAIN_4 | – | – |

**Cross-checks against the firmware.**
- The 18 variable IDELAYs match the 32-tap IDELAY calibration in `software/portable/riscv/ddr2_calibrator_u64ii.c:8-9` [V].
- The MMCM is retuned at runtime over DRP at `0x10200000`
  (`software/system/u64ii_init.cc:173-199`, M=20 / M=24 / fractional modes) [V]. The bitstream only holds the
  power-on setting.
- The 4 GTP channels are **probably** HDMI TMDS (3 data lanes + clock) [E]. No firmware reference to GTP or TMDS
  was found.

**The C64U build is nearly identical in resources.** The 100T build has the same MMCM, PLL, GTP and IOB feature
set [V]. It was not compared at netlist level.

### 2.4 BRAM contents are recoverable exactly [V]

- The RAMB36 at `BRAM_R_X37Y85` of the U64-II 50T matches `fpga/ip/video/vhdl_gen/font_pkg.vhd` (`c_font`) in
  1024/1024 36-bit entries, address-aligned.
  - Layout: interleave the Y0/Y1 halves (even bits from Y0, odd bits from Y1), per fasm2bels
    `bram_models.py:805-860`.
- 38 RAMB18 halves have non-zero INIT. LUTRAM and SRL initial values are readable from the LUT INIT features.
- **Open:** the rvlite boot ROM (`fpga/cpu_unit/rvlite/vhdl_source/bootrom_u64ii_pkg.vhd`, 2048 × 32 bit) was
  **not** found as BRAM words. Possible reasons: it is a LUT ROM, it uses another layout, or the shipped content
  differs.

---

## 3. What is inside the FPGA

### 3.1 The U64-II source is a separate, unpublished tree [V]

**The public repo has no U64 top level.**
- No U64, U64-II or C64U top-level RTL, Vivado project, XDC or build script is in `fpga/` or `target/fpga/`.
  `target/u64ii/` holds only RISC-V software.
- CI (`.github/workflows/build.yml`) builds only the U2, U2+ and U2+L FPGAs.

**Build files point at a sibling tree `../ult64/`:**
- `fpga/1541/sim/makefile:62` (`ult64/proglogic/c64/`)
- `target/u64/nios2/updater/getbuild.sh:1` (`ult64/target/u64_a4/work/u64.sof`)
- `target/u64/nios2/loader/Makefile:9-10`
- commented `ult64/proglogic/{network,ip,tl_packages}` entries in the U2+ `.qsf` files

The project docs already treat the top level as closed (`docs/hw/00-memory-map.md` §A, `docs/hw/01-cpu-boot-memory.md:52,409`).

**Evidence that open IP is inside the closed design** [V]:

| Evidence | Detail |
|---|---|
| Commit coupling | `8a4e5771` (2026-09-03, "Make FENCE instruction a NOP. U64-alike FPGAs will follow shortly", rvlite) is followed by `883f608d` ("updated U64* bitfiles to support FENCE"). `59594060` (REU) and `eaa7751b` (cartridge) change open RTL and the U64E2 bitfiles in the same commit |
| Fork merge | `bcc05e7a` (2021-07-08) "Moved U64 dependencies here to end the fork" added `6502n/` and `cia_{registers,timer,pkg}.vhd` |
| Files created for U64-II | `bootrom_u64ii_pkg.vhd` (`7da85a0a`), `mem32_to_mem64.vhd` (`7542d102`), `jtag_client_xilinx.vhd`, `char_generator_*_12` (overlay), `spdif_encoder.vhd` |
| Driver ↔ RTL match | `hw_i2c.h` matches `i2c_master.vhd:149-316`. `dma_uart.h` matches `uart_dma.vhd`. The same `nano_minimal.nan` (USB) and `iec_code.iec` blobs are linked for U64-II and U2+ |
| Hooks in the open integration block | `ultimate_logic_32.vhd`: `g_ultimate_64` (:16) → capability bit 26 (:314); `g_direct_dma` (:24) removes the physical slot master; `hdmi_irq` → `irq_high(5)` (:528) = firmware `ITU_IRQHIGH_HDMI` |
| Bitstream | The font ROM match in §2.4 |

**Caveat [E].** This proves that the open IP is shared. It does not prove that every public block is identical to
the private build, or which generics that build uses.

### 3.2 Firmware-visible blocks (register map, `docs/hw/00-memory-map.md` §1b)

| Block (FW address) | Open RTL | Evidence it is on U64-II |
|---|---|---|
| CPU rvlite "Frenix" (reset `0x80000000`) | `cpu_unit/rvlite` (~2.3k lines) | **Strong** (FENCE coupling, U64-II boot ROM package) |
| Boot ROM `0x8000xxxx` | `bootrom_u64ii_pkg.vhd`, generated from `bootloader_u64ii.c` + `ddr2_calibrator_u64ii.c` | Strong (but see §2.4) |
| ITU IRQ/timers/UART/caps `0x10000000` | `io/itu` | Strong. Generics unknown (00 Q-B2) |
| Drives, WD177x `0x10020000`/`0x10024000` | `1541/vhdl_source` | Likely |
| IEC processor `0x10028000` | `io/iec_interface` | Strong (shared microcode) |
| Cart/UCI/sampler/ACIA/DMA `0x10040000-0x1005FFFF` | `cart_slot/slot_server_v4` etc. | Strong |
| SD/flash SPI, RTC, GCR codec, audio select | `io/spi`, `ip/clock`, `1541/gcr_codec`, `io/audio_select` | Likely / unknown |
| RMII MAC `0x10060800` | `io/rmii` + `ip/free_queue` | Likely |
| WiFi DMA UART `0x10060900` | `io/uart_lite/uart_dma` | Strong |
| USB host + nano `0x10080000` | `io/usb2` + `ip/nano_cpu` | Strong |
| C2N playback/record | `io/c2n_*` | Likely |
| HW I2C ×4 `0x10100700` | `io/i2c/i2c_master.vhd` | Strong |
| Overlay chargen `0x10140000` | `ip/video/char_generator_peripheral_12` | Strong (bitstream-proven font) |
| ICAP `0x10060604` | Only a Spartan-3A version | **7-series wrapper closed** |
| U2PIO page `0x10100000` | U2+ variant only | U64-II variant closed |
| DDR PHY `0x10100100` + controller | U2+ (Altera) / U2+L (Lattice) only | **Artix DDR2 PHY/controller closed** |
| U64 IO page `0x10100200-0x1010040F` (keyboard, joystick, HDMI/HPD, LEDs, power) | none | **Closed** |
| Audio/speaker mixer, resampler `0x10100500` | none (generic `io/audio` parts exist) | **Closed** |
| LED strip, Blingboard | none | Closed |
| HDMI timing/palette, VIC cropper `0x10144000-0x10148000` | none | **Closed** |
| C64 core config, palette, UltiSID curves, ROM windows `0x10180000-0x1018CFFF` | none | **Closed** |
| UDP stream headers `0x10190000` | U2+ copy `io/debug/eth_debug_stream.vhd` only | Closed |
| MMCM DRP `0x10200000` | none | Closed (Xilinx primitive) |

### 3.3 Parts the firmware never sees directly

| Part | Open RTL | Status |
|---|---|---|
| 6510 CPU | `6502n` `cpu6502(cycle_exact)` (2,504 lines) | Core **likely** (fork merge). The 6510 I/O-port wrapper is closed |
| CIA 6526 ×2 | `cia_registers/timer/pkg` (~980 lines), lockstep-tested against real chips | **Likely** |
| **VIC-II** | none, not in any git history | **Closed** |
| **C64 PLA, glue, bus timing** | none (`mk7_pla.vhd` is a cartridge PLA) | **Closed** |
| **UltiSID ×2** | `sid6581` (2010, used on U2+) | **Closed.** Relation to `sid6581` unknown [E: probable ancestor] |
| **Video pipeline** (palette, scaler, overlay mix, HDMI/TMDS, analog video) | overlay chargen only | **Closed** |
| 1541/1571/1581 drives ×2 | `1541/vhdl_source` (7,251 synth lines: 6502n + 2× via6522 + cia + wd177x) | Likely |
| Cartridges, freezer, REU | `cart_slot` | Strong |
| Clocking (MMCM + external I2C PLL), physical port pins | none | Closed |
| neorv32 | submodule; its instance is commented out (`u2p_riscv.vhd:323`) | **Not used** |

### 3.4 Processors inside the FPGA [V]

| Core | Details | Program |
|---|---|---|
| **rvlite "Frenix"** | RV32I + Zicsr + multiply, M-mode, one IRQ, fetch/decode/execute/writeback + icache | `ultimate.elf` |
| **6502n cycle_exact** | Drive CPUs; probably also the 6510 [E] | Drive ROMs from firmware |
| **nano_cpu** | 16-bit data, 1K-word RAM | `nano_minimal.nan` (USB) |
| **IEC processor** | Microcoded, 30-bit instructions, 512 addresses | `iec_code.iec` |

Nios II, MicroBlaze/mblite/ZPU and neorv32 are **not** in the U64-II FPGA. The ESP32 running `u64ctrl` is a separate chip.

### 3.5 Size picture

| Measure | Value |
|---|---|
| Whole U64-II 50T design [V, §2.3] | ≈23.8k LUT positions, ≈18.5k FFs, 108 RAMB18 halves, 27 DSP tiles |
| Open reference system: U2+ RISC-V Quartus project [V] | ≈40k HDL lines (without neorv32), targets EP4CE22 (22,320 LEs) |
| Open, firmware-facing blocks [E] | Well under 20k LUT-class cells |
| Closed remainder [E] | Everything in the "Closed" rows above; it fits in what the 50T has left |

---

## 4. What real time demands, and what simulators deliver

### 4.1 Clock domains

| Domain | Rate | Tag |
|---|---|---|
| System / CPU | 100 MHz (ITU tick every 499,968 clocks) | [V] `CLOCK_FREQ`, docs/hw/00. Physical confirmation still open (00 Q-A5) |
| C64 PHI2 | 0.985248 MHz PAL | [V] |
| C64 core master clock | U64 mk1: 31.53 / 32.73 MHz (`software/u64/fpll.cc:24-27`). U64-II value closed | [V]/closed |
| Drive logic | tick_16MHz / 4MHz / 1kHz enables. The 6502 runs at 1 MHz (`c1541_timing.vhd:37-75`) | [V] |
| IEC processor | tick_1MHz (`ultimate_logic_32.vhd:1211`) | [V] |
| HDMI pixel clock | 27 / 40 / 65 / 74.25 / 108 / 148.5 MHz. TMDS serial rate is 10× | modes [V], MHz values [E] |
| USB ULPI | 60 MHz | [E] (standard) |

### 4.2 Today's baselines on this Mac (Apple M4) [V]

| Component | Speed |
|---|---|
| rvlite interpreter | ≈200 MIPS ≈ 8× real time (README, `docs/status/boot.md`) |
| TRX64, PAL C64 | 13.435 MHz = 13.6× real time |
| TRX64 with the true-drive 1541 | 11.179 MHz = 11.35× real time (`TRX64/docs/perf-compare.md`) |
| HDL tools | None installed: no verilator, ghdl, nvc, yosys or iverilog |

### 4.3 Simulator throughput anchors [S]

| Anchor | Speed |
|---|---|
| Verilator single thread, booting Linux (GSIM, arXiv 2508.02236, Table I, i9-9900K) | stuCore (10k IR nodes) ≈900 kHz; Rocket (235k) ≈30 kHz; BOOM (571k) ≈9 kHz; XiangShan (6.2M) ≈0.9 kHz |
| Small SoC, Verilator | Murax ≈1.2 M cycles/s (SpinalHDL docs); VexRiscv ≈2.2 MHz (cxxrtl_eval) |
| Other simulators, relative to Verilator | CXXRTL ≈3.2× slower; GHDL and Icarus about 10× slower |
| NVC | ≈3.5–6× faster than GHDL mcode |
| GSIM / ESSENT / Arcilator | 2–20× over Verilator on large, low-activity designs. They failed or ran out of memory on some large designs |
| Gate-level vs RTL | Typically 10–100× slower, because words become per-bit cells [E, rule of thumb] |
| M4 single thread | ≈1.3–1.9× an i9-9900K [S] |

### 4.4 Projected live speeds on the M4 [E]

**Anchor.** Small blocks (about 10k IR nodes) reach ≈1.5–4 MHz of clock edges per second.

**Edge skipping.** Gideon's VHDL does its work on clock enables. A wrapper that generates the ticks itself only
needs to evaluate edges that carry a tick or a bus strobe. Each block must be audited for free-running logic
before relying on this.

| Block | Every 100 MHz edge | With edge skipping |
|---|---|---|
| IEC processor (`iec_processor_io`) | 3–6 % RT | ≥1–6× RT |
| 1541 CPU part (6502 + 2 VIAs; GCR stays behavioural) | 1.5–4 % RT | ≈0.4–1× RT |
| Full `mm_drive` (floppy_stream on 16 MHz) | 1–3 % RT | ≈6–20 % RT |
| Cartridge `slot_server_v4` | 2–4 % RT | ≈10–40 % RT |
| USB nano + usb2 + ULPI BFM | 2–5 % RT | CI only; enumeration takes tens of seconds of wall time |
| rvlite RTL | – | 0.2–1.5 M instructions/s (100–1000× slower than the interpreter) |
| Whole open U2+ SoC (not the U64-II) | 0.06–0.4 % RT | 1 emulated s ≈ 4–30 min |
| **U64-II gate-level netlist from the bitstream** | see §5.3 | **10⁻³ … 2·10⁻⁵ RT** |

---

## 5. Paths: the options ladder

### 5.1 Overview

| Rung | What | Speed | Effort | Value | Main blockers |
|---|---|---|---|---|---|
| **L0** (today) | Behavioural register models + rv32 interpreter; TRX64 as the C64 (S14) | 8× RT CPU, 11–13.6× RT C64 [V] | ongoing | Runs the firmware interactively. The development target | Fidelity only as good as `docs/hw` and firmware reading |
| **L1** | CI differential tests: behavioural model vs Verilated open RTL, lockstep over the same bus-op sequence | sub-second per test [E] | 2–4 wk tooling + 2–5 d per block [E] | **High.** Pins models to the RTL that ships | Toolchain (`ghdl synth` of `-relax` VHDL, macOS arm64); U64-II generics unproven |
| **L1b** | Bitstream forensics without simulation (FASM queries, BRAM content extraction, MMCM power-on parameters) | minutes | days per question [E] | **Medium-high, cheap.** Hard facts from the shipped design | Meaning is limited to what can be matched (BRAM, hard-block settings) |
| **L2** | rvlite RTL lockstep vs `crates/rv32` (riscv-tests + bounded firmware windows) | 0.2–1.5 M instructions/s [E] | 1–2 wk [E] | Medium-high: CSR/trap timing, bus_converter byte order | Riviera-oriented testbenches |
| **L3** | One open RTL block live in ue2emu behind `IoDevice` (IEC processor, 1541 CPU part, full drive) | IEC ≥1×; 1541 CPU part 0.4–1×; mm_drive 0.06–0.2× RT [E] | 3–6 wk first block, 1–3 wk per further block [E] | Real drive and IEC logic without HLE | The whole emulator slows to the block's speed while it is active; DDR controller latency closed |
| **L4** | Multi-block RTL island (drives, IEC, cart, USB + ULPI BFM), threaded | 0.02–0.2× RT [E] | 2–4 months [E] | CI scenarios; low interactive value | `IoCtx.ram` is a `&mut` borrow, so shared DDR needs an interface change |
| **L5** | Full open U2+ RTL SoC | 0.06–0.4 % RT [E] | 1–2 months [E] | Reference for shared IP in SoC context | **It is a different product**, not the U64-II |
| **L6** | Gate/LUT netlist recovered from the U64-II bitstream, simulated with Verilator | 10⁻³ … 2·10⁻⁵ RT [E] | 2–4 person-months to the UART prompt; C64/video oracle work beyond that [E] | **The only software oracle for the closed blocks** (C64 core, top decode, video) | fasm2bels gaps (MMCM, DSP, GTP), GTP model is secureip, DDR2 PHY calibration, no names or hierarchy, licensing questions (§7) |
| (L7) | Lift the netlist to readable RTL (hierarchy labelling, word-level recovery) | – | open-ended, months to years [E] | Understanding the closed design | Research-grade tooling (HAL, word-level lifting) |

**Why the C64 machine is always TRX64 below L6.** No RTL exists for VIC-II, PLA or UltiSID. So "FPGA emulation from
source" is always a hybrid: open RTL blocks + TRX64 + behavioural models for the closed glue [V/E].

### 5.2 Hybrid co-simulation in this repository (L1–L4 design sketch) [V interfaces / E effort]

**Interfaces that already exist.**
- Rust side: `IoDevice` (`crates/ue2-core/src/io.rs`): `read8`/`write8`, `IoCtx{now, pc, ram, irq, console}`,
  `next_event`/`tick`/`reset`. The machine adds 4 clocks per instruction (`machine.rs:250-275`).
- VHDL side: `t_io_req`/`t_io_resp` (`io_bus_pkg.vhd:7-18`) and `t_mem_req_32`/`t_mem_resp_32`
  (`mem_bus_pkg.vhd:43-55`).

**Build flow.**
- `ghdl synth --out=verilog <entity>`, then `verilator --cc -O3`.
- A C-ABI shim (`new`, `set_inputs`, `clock_edges`, `get_outputs`) linked from Rust through `build.rs` + `cc`.
  `marlin-verilator` is an alternative.
- The neorv32-verilog project uses the same GHDL→Verilog route.

**Time bridge.**
- The wrapper keeps `sim_now` and catches up to `ctx.now` on every call, skipping edges where it can.
- A bus access strobes `io_req` and runs until `ack`. That takes ≤2 clocks, inside one instruction's 4-clock budget.

**DMA.** `mem_req_32` is served from `ctx.ram` with a 1-clock ack. `c1541_timing` tolerates `mem_busy`
(`behind` counter, `c1541_timing.vhd:50-75`).

**IRQ and scheduling.** `io_irq` → `IrqState::set_high`. `next_event` returns a quantum (e.g. 10k clocks) while
the block is active.

**IEC to TRX64.**
- A wired-AND bus (bit set = released), stepped once per C64 cycle (≈101.5 system clocks) with a fixed-point
  clock ratio, as VICE `sync_factor` does.
- Use `trx64-core` in-process. `trx64-ffi` is JSON-RPC and too slow for per-cycle lockstep.
- Open question: is per-cycle exchange fine enough for fast loaders?

**L1 harness.**
- Replay the same op sequence into the behavioural `Box<dyn IoDevice>` and the Verilated block, and abort at the
  first divergence (the arc-tests pattern).
- Stimuli: `--log io` traces from firmware boots, `board::rig::Rig` sequences, constrained-random register fuzzing.
- Keep a list of allowed divergences, each with a doc citation.

### 5.3 The bitstream path (L1b and L6) in detail

| Step | Status | Effort / time |
|---|---|---|
| 0. Cut the `.bit` out of the `.ue2` (offsets in §2.1) | **done** [V] | trivial |
| 1. Build prjxray `bitread`, fetch prjxray-db for the part | **done** [V] | ≈10 min |
| 2. `bit2fasm` | **done** [V]: 13–14 s, ≈233 unknown bits | seconds |
| 3. Build the fasm2bels connection database for `xc7a50tfgg484-1` | not run | hours of CPU, several GB [E] |
| 4. Run fasm2bels; add MMCME2_ADV, DSP48E1 and GTPE2 support (prjxray already names the MMCM features; the 39 DSP bits may need Vivado fuzzing) | not started | 2–6 wk [E] |
| 5. Round-trip check: re-import into Vivado, write a bitstream, diff against the original | not started | 1–2 wk [E] |
| 6. Make it simulatable: unisim models, GTP stub (capture TXDATA), DDR2 memory model or PHY stub, testbench (SPI flash with bitstream + `ultimate.app`, UART, USB/RMII/SD/I2C pins, C64 ports) | not started | 2–4 wk [E] |
| 7. Verilator build + C++ library for co-simulation with ue2emu | not started | 1–2 wk [E] |
| 8. Optional: label hierarchy by matching against synthesized open IP; HAL / word-level lifting | – | open-ended [E] |

**What a recovered netlist contains.**
- A flat Verilog netlist of primitives (LUT6_2, FDxE, CARRY4, RAMB18/36E1, DSP48E1, MMCME2_ADV, IDELAYE2,
  I/OSERDESE2, GTPE2, BSCANE2 …) with tile/site-derived names.
- Recovered exactly: every truth table, FF initial value and set/reset type, all memory INIT contents, all
  routing, and the power-on clocking.

**What is lost.**
- All designer names and the hierarchy: there are no boundaries between 6510, VIC, SID, rvlite and the DDR controller.
- The RTL structure: FSMs and buses are dissolved into LUTs, possibly retimed or replicated.
- Timing constraints and clock-domain intent.
- Logic the synthesizer optimized away.

**Simulation blockers** [V unless marked]:

| Area | Problem |
|---|---|
| Primitive models | XilinxUnisimLibrary (Apache-2.0, archived 2022-12-06) has plain-Verilog models, including MMCME2_ADV with DRP. They carry `specify` blocks and `#` delays, so they need cycle-based handling for Verilator [E] |
| GTP | `GTPE2_CHANNEL.v` only wraps `B_GTPE2_CHANNEL`, which is Xilinx secureip and not in the library. **The GTPs must be stubbed** |
| DDR2 PHY | The firmware calibrates the IDELAY taps. A zero-delay simulation needs a DDR2 memory model that lets the calibration pass, or a PHY stub at the netlist boundary [E] |
| Clocks | 100 MHz system clock, plus MMCM/PLL outputs retuned at runtime over DRP, plus SERDES/GTP rates |

**Speed [E].**
- The netlist is ≈24k LUTs and ≈18.5k FFs, flat, at bit level. Estimated 10⁴–10⁵ evaluated edges per second.
- Evaluating only the 100 MHz domain gives ≈10⁻³–10⁻⁴ of real time.
- Evaluating the union of all domains (>250 M edges/s before SERDES) gives ≈2·10⁻⁴–2·10⁻⁵.

| Emulated span | Wall time |
|---|---|
| 1 s | ≈17 min to ≈14 h |
| One PAL frame (20 ms) | ≈20 s to ≈17 min |
| A boot of a few seconds | roughly an hour to more than a day |

Multi-threading does not help at this size: ESSENT needs ≳1M nodes before threads pay off [S].

**Prior art** [S]:
- Note & Rannaud 2008 (Debit).
- Zhang et al., IEEE Access 2019: a 7-series bitstream→netlist→RTL chain.
- Swierczynski et al., ePrint 2015/768: reverse engineering of a commercial FIPS-140-2 USB drive bitstream.
- Puschner et al., ASHES'24 (arXiv 2411.11060): prjxray + fasm2bels on 7-series. "the resulting netlist is not
  entirely complete".
- This study found no public example of **simulating** a recovered commercial 7-series netlist. That is a search
  result, not proof that none exists.

---

## 6. Value by question type

| Question you want answered | Cheapest reliable source |
|---|---|
| Register behaviour of rvlite, IEC, USB, RMII, I2C, UART-DMA, cart/REU, drives, overlay | Open RTL: read it (today) or run it differentially (L1/L2) |
| Does block X with generics Y exist in the shipped U64-II? | Firmware boot log / capability word on hardware; for BRAM-bearing IP, bitstream BRAM matching (L1b) |
| Power-on clock configuration | FASM MMCM/PLL features (L1b). Absolute frequencies also need the oscillator frequency [E] |
| Closed register decode, VIC-II/UltiSID/video behaviour | Physical U64-II/C64U if available (real-time oracle); otherwise only L6, over short windows |
| Accurate drive/IEC timing on U64-specific paths | L3 (RTL drive) vs TRX64's VICE-derived 1541 |

---

## 7. Licensing and legal facts (neutral, not legal advice)

**Repository** [V]:
- The root `LICENSE.txt` is GPLv3. The README calls the repo the official archive.
- File headers vary (census over 424 synthesizable HDL files, excluding sim, nios, altera and lattice):

| Header class | Files | Example |
|---|---|---|
| Explicit GPL | 1 | `fpga/1541/vhdl_source/via6522.vhd:11`: "License: GPL 3.0 - Free to use, distribute and change to your own needs." |
| Restrictive notice | 22 (sid6581 ×16, copper ×4, lp_filter ×2) | `fpga/sid6581/vhdl_source/sid_top.vhd:9-10`: "this file is copyrighted, and is not supposed to be used in other projects without written permission from the author." |
| Copyright line only | 76 (includes all of rvlite, rmii, usb2, ip/video) | `fpga/cpu_unit/rvlite/vhdl_source/core.vhd:2`: "Gideon's Logic B.V. - Copyright 2023" |
| No header | 325 | – |

- How the root licence and the restrictive file notices interact is a legal question. This study does not assess it.

**U64-II / C64U FPGA design** [V]:
- The design (top `u64_mk2_artix`) is not published. It ships only as bitstreams (`external/*.bit`, embedded in
  `.ue2`).
- The README's U64E-II build prerequisites list no FPGA toolchain (`README.txt:27-29`).
- No separate licence statement for the bitstream files was found. The search was not exhaustive, and the C64U
  `.ue2` terms were not inspected.
- The closed design contains IP that is also published in the GPLv3 repo (§2.4, §3.1). Whether obligations follow
  from that, and for whom, is not assessed.
- Third-party forum reports describe the U64/C64U FPGA sources, UltiSID included, as closed [S: MiSTer forum].

**Technical protection** [V]: no bitstream encryption, no authentication, no readback restriction.

**Vendor stance** [S: Time Extension]:
- 2026-04-20: Commodore announced an FPGA lockdown against unofficial firmware.
- 2026-04-24: it reversed that: "No FPGA lockdown. Instead: clear disclaimer, free experimentation, just no free
  support/replacement for bricked modded units."
- The article says nothing about source availability or licensing.

**Tool licences:**

| Tool | Licence | Tag |
|---|---|---|
| Project X-Ray | ISC | [V] |
| prjxray-db | CC0-1.0 | [S] |
| f4pga-xc-fasm2bels | Apache-2.0 | [V] |
| XilinxUnisimLibrary | Apache-2.0 wrapper; `B_GTPE2_CHANNEL` secureip not included | [V] |
| Verilator | LGPL-3.0 / Artistic-2.0 | [S] |
| ESSENT | BSD | [S] |

**General legal context** (statutes only; not checked against this product's terms, and not assessed for whether
they apply to FPGA bitstreams):
- EU Software Directive 2009/24/EC Art. 6 and German UrhG §69e allow decompilation of computer programs under
  narrow interoperability conditions.
- Trade Secrets Directive (EU) 2016/943 Art. 3(1)(b) and German GeschGehG §3(1) Nr. 2 treat reverse engineering
  of a lawfully acquired, publicly available product as lawful, unless a valid contractual restriction applies.
- Private analysis, and publishing a recovered netlist or derived RTL, are separate questions.

---

## 8. Recommendation

1. **Do not pursue full FPGA emulation as a runtime.**
   - From source it is impossible: the C64 machine and the integration layer are closed.
   - From the bitstream it is 3–5 orders of magnitude too slow and costs months before the first useful answer.
   - L0 (behavioural models + TRX64) stays the product.
2. **Next steps with the best value for effort: L1 + L2, plus L1b by-products.**
   - Install `ghdl`/`nvc` + `verilator` and check `ghdl synth` on macOS arm64 against Gideon's VHDL.
   - Build one differential harness. Start with blocks that have strong U64-II evidence and non-trivial behaviour:
     `iec_processor`, `usb_host_nano`/`nano_cpu`, `i2c_master`, `uart_dma`, `slot_server_v4`/REU, drive
     registers/WD177x.
   - Add the rvlite RTL lockstep against `crates/rv32`.
   - From the bitstream, extract what is cheap and exact: BRAM contents, MMCM/PLL power-on settings, resource and
     IOB facts. These settle some `docs/hw/00` open questions without any simulation.
3. **L3 only when there is a concrete fidelity problem.** Example: a fast loader on a U64-specific drive path that
   TRX64 plus HLE cannot reproduce. Accept the slowdown while the block is active.
4. **L6 is a separate research project.** Start it only for a closed-block question that neither firmware reading
   nor physical hardware can answer.
   - The first milestone would be step 3–5 of §5.3: a round-trip-verified netlist. Simulation comes after that.
   - Settle the licensing questions in §7 before publishing anything derived from the bitstream.

---

## 9. Open questions (merged)

1. Do the public copies of the open blocks match the private build's versions and generics? Which generics does
   the U64-II top use: drive count, SID voices, `g_sampler`, ITU `g_edge_init`/`g_version`, clock frequency?
   (00 Q-A2, Q-B2)
2. Is the 6510 the `6502n` core with a closed port wrapper? Are both C64 CIAs `cia_registers`? Is UltiSID derived
   from `sid6581`?
3. Where is the rvlite boot ROM in the bitstream: LUT ROM, another layout, or different content?
4. What are the 4 GTPs used for (HDMI TMDS is the working assumption)? What do the 39 undocumented DSP bits and the
   185 RIOB33 bits mean? (Is SSTL18 the explanation?)
5. DDR2 size on U64E-II vs C64U: CNX reports 128 MB for C64U [S], the firmware map uses 64 MB modulo mirroring.
6. What does the 100T build add over the 50T? Is the C64U build functionally the same design at an older revision?
7. Real Verilator throughput of `cpu_part_1541`, `mm_drive`, `iec_processor_io` and `nano+usb2` on the M4. Every
   live-speed number above is extrapolated.
8. Does `ghdl synth` accept the `-relax` VHDL unchanged, and is the GHDL synth path available on macOS arm64?
9. Is edge skipping safe per block (free-running synchronisers, handshakes, arbiters)?
10. fasm2bels on this design: how long does the connection-database build take, and which sites are rejected
    beyond MMCM/DSP/GTP?

---

## 10. Contradictions between the research inputs, and how they were resolved

| Topic | Conflict | Resolution [V] |
|---|---|---|
| Bitstream offsets | One input gave header offsets `0x2F65C` / `0x24695C` / `0x28CCC` and a 50T length of "2,191,500 B". Another gave the C64U sync at `0x28D58` | Re-parsed: headers at `0x2F64C` / `0x24694C` / `0x28CBC`; syncs at `0x2F6E9` / `0x2469EA` / `0x28D59`; 50T length `0x21728C` = 2,192,012 B |
| DDR type | One input assumed a DDR3 MIG PHY (with PHASER) | **DDR2 with a custom PHY.** `ddr2_calibrator_u64ii.c:228-230` writes EMR3, EMR2, then EMR with additive latency 3 (`0x405A`, DDR2 EMR layout). FASM has **zero** PHASER features. The PHY uses 18 variable IDELAYE2 + ISERDES |
| Utilisation | One input: "not decoded". Another decoded it from FASM | Recounted from FASM: 23,844 LUT INIT positions, 108 RAMB18, 18 variable IDELAY, 4 GTP, 2 MMCM, 2 PLL match. FF count ≈18.5k (18,472 slice FF features; a broader ZINI grep gives 18,858) |
| 100T tool coverage | "Not confirmed" vs "decoded fine" | `bit2fasm` decodes the 100T with 232 unknown bits. Netlist-level coverage is untested. The prjxray README says its focus is "the Artix-7 50T part" |
| Gate-level speed | 10⁻³–10⁻⁴ vs 10⁻⁴–2·10⁻⁵ RT | Both are estimates with different scopes (100 MHz domain only vs all clock domains). Merged as a range |
| Gate-level effort | "2–4 months" vs "many months, high risk" | Different milestones: UART prompt vs a useful C64/video oracle. Both kept |
| Licence of the open HDL | "GPLv3" (root) vs per-file notices | Both are facts. The root is GPLv3, and headers include an explicit GPL-3.0 notice and a "no use without written permission" notice (§7) |
| HDL line counts | e.g. drives 7,251 vs 11.6k; sid6581 2,446 vs 3.1k | Synthesizable-only counts vs counts including simulation files |
| BRAM/DSP totals of 7A50T | Quoted "from memory" | The prjxray tilegrid gives 150 RAMB18 halves (75 RAMB36) and 60 DSP tiles (120 DSP48E1), consistent with DS180 |

---

## Sources

**Local** [V]:
- `firmware/1541ultimate` at `b617777c`: files and commits cited inline.
- `<1541ultimate checkout>/update.ue2`
- `<path>/c64u_v1.1.0.ue2`
- `docs/hw/00-memory-map.md`, `docs/status/boot.md`, `crates/ue2-core/src/{loader,io,machine}.rs`
- `<TRX64 checkout>/docs/perf-compare.md`

**Device and bitstream format:**
- AMD UG470 (7-series configuration)
- [AMD DS180 (7-series overview)](https://docs.amd.com/api/khub/documents/2LByHkO~nSZXcei2D55fTg/content)

**Bitstream tools:**
- [Project X-Ray](https://github.com/f4pga/prjxray)
- [prjxray-db artix7](https://github.com/f4pga/prjxray-db/tree/master/artix7)
- [prjxray issue #1285 (BRAM INIT)](https://github.com/f4pga/prjxray/issues/1285)
- [f4pga-xc-fasm2bels](https://github.com/chipsalliance/f4pga-xc-fasm2bels)
- [XilinxUnisimLibrary](https://github.com/Xilinx/XilinxUnisimLibrary)
- [verilator-unisims](https://github.com/uwsampl/verilator-unisims)
- [openXC7 nextpnr-xilinx](https://github.com/openXC7/nextpnr-xilinx)

**Bitstream reverse-engineering prior art:**
- [Note & Rannaud 2008](https://www.researchgate.net/publication/200065272_From_the_bitstream_to_the_netlist)
- [Zhang et al. 2019](https://ieeexplore.ieee.org/document/8653869/)
- [Swierczynski et al., ePrint 2015/768](https://eprint.iacr.org/2015/768)
- [Puschner et al., ASHES'24](https://arxiv.org/pdf/2411.11060)
- [HAL](https://arxiv.org/pdf/1910.00350)
- [Word-level recovery](https://arxiv.org/pdf/2303.02762)

**Simulation performance:**
- [GSIM](https://arxiv.org/pdf/2508.02236)
- [ESSENT (WOSET'21)](https://scottbeamer.net/pubs/beamer-woset2021.pdf)
- [cxxrtl_eval](https://github.com/tomverbeure/cxxrtl_eval/blob/master/README.md)
- [SpinalHDL simulator notes](https://spinalhdl.github.io/SpinalDoc-RTD/master/SpinalHDL/Simulation/simulator_specifics.html)
- [fpga-board-sim](https://github.com/Machai-Kydoimos/fpga-board-sim)
- [MicroNova NVC](https://blog.micro-nova.com/posts/moving-to-nvc/)
- [neorv32-verilog](https://github.com/stnolting/neorv32-verilog)
- [Arcilator slides](https://llvm.org/devmtg/2023-10/slides/techtalks/Erhart-Arcilator-FastAndCycleAccurateHardwareSimulationInCIRCT.pdf)
- [arc-tests](https://github.com/circt/arc-tests)
- [Verilator guide](https://verilator.org/guide/latest/simulating.html)
- [verilator-benchmarks](https://github.com/ManavA/verilator-benchmarks)
- [arXiv 2303.12269](https://arxiv.org/pdf/2303.12269)
- [cpu-monkey i9-9900K vs M4](https://www.cpu-monkey.com/en/compare_cpu-intel_core_i9_9900k-vs-apple_m4_10_cpu)

**Products and vendor stance:**
- [CNX Software: C64U hardware](https://www.cnx-software.com/2025/07/16/fpga-based-commodore-64-ultimate-keyboard-pc-is-compatible-with-original-c64-games/)
- [Tom's Hardware: C64U 77 edition, XC7A100T](https://www.tomshardware.com/video-games/retro-gaming/nda-5pm-et-tuesday-commodore-77-special-edition-c64u-uses-more-powerful-amd-artix-xc7a100t-processor-preorders-for-cyberpunk-2077-inspired-design-open-today-at-usd377)
- [Time Extension: no FPGA lockdown](https://www.timeextension.com/news/2026/04/we-listened-we-agree-no-fpga-lockdown-commodore-backpedals-on-unofficial-c64-ultimate-firmware)
- [MiSTer forum](https://misterfpga.org/viewtopic.php?p=114534), [MiSTer C64U thread](https://misterfpga.org/viewtopic.php?t=9721)

**Law** (statute texts, not fetched): Directive 2009/24/EC Art. 6; UrhG §69e; Directive (EU) 2016/943 Art. 3; GeschGehG §3.
