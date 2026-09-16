# Coverage gaps: Sampler (0x10048000), Audio select (0x10060700), legacy SID_BASE (0x10042000)

Scope: the three IO-window regions the coverage critic found without a doc. Build: `target/u64ii/riscv/ultimate/Makefile`
(`-DU64=2 -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000`, Makefile:246). Base macros: `SID_BASE = IOBASE+0x42000`
(`software/system/iomap.h:15`), `SAMPLER_BASE = IOBASE+0x48000` (iomap.h:19), `AUDIO_SEL_BASE = IOBASE+0x60700` (iomap.h:30).
Access is only through `ioWrite8`/`ioRead8` = plain byte (volatile uint8_t*) access (iomap.h:38-39).

All paths below were checked against the linked ELF (`target/u64ii/riscv/ultimate/result/ultimate.elf`,
disassembled with `tools/bin/riscv32-unknown-elf-objdump`). Searched for: every `lui` of 0x10042/0x10043/0x10048/0x10049,
every `lui 0x10060` + offset 1792/1793, every `lui 0x10061` + offset -2304/-2303, and every 32-bit word in the three
windows in all alloc sections. Only the uses listed below exist. The word hits were all instruction encodings in `.text`
(for example 0x100607b7 = `lui a5,0x10060`); no `.rodata`/`.data` pointer points into the windows.

Paths below are relative to `firmware/1541ultimate/`. Where "reference RTL" is cited, it is the open U2+ top
(`fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd`) and the shared cart-slot/sampler IP. The U64-II top level is
closed, so whether it instantiates the same IP the same way is an OPEN QUESTION (see end).

---

## Region A — SAMPLER_BASE 0x10048000 (+0x100, aliased over +0x2000)

### Sources read
- `software/io/audio/sampler.h` (macros :7-26, `Sampler::reset` :35-39). `sampler.cc` (global `Sampler sampler;` :4) is **not** in SRCS_CC (Makefile:44-222); no `sampler` symbol in the ELF.
- `software/io/audio/audio_select.h:41-47` `AudioConfig::clear_sampler_registers` (static inline; `audio_select.cc` itself is **not** compiled, Makefile:44-222).
- `software/io/command_interface/command_intf.cc` (ctor :39-66, ISR hook :29-36, `run_reset_task` :98-109).
- `software/filetypes/filetype_reu.cc` (`fetch_context_items` :37-51, `execute_st` :63-147, `start_modplayer` :149-160).
- `software/api/route_runners.cc:237-253, 268-277` (REST `/v1/runners:modplay` PUT/POST).
- `software/io/c64/c64.cc:305-327, 1302-1304, 1361-1366` (C64_SAMPLER_ENABLE), `software/io/c64/c64.h:14-15,68`.
- `software/portable/riscv/riscv_main.c:56-95` (IRQ dispatch), `portable/riscv/crt0.S:197-218` (ctor order).
- `software/system/itu.c:19-29`, `software/system/itu.h:40,58,67,70`.
- RTL: `fpga/io/sampler/vhdl_source/sampler_regs.vhd`, `sampler_pkg.vhd`, `sampler2.vhd`, `sampler_accu.vhd`; `fpga/cart_slot/vhdl_source/slot_server_v4.vhd:480-505, 946-1017, 1044-1077`, `slot_to_io_bridge.vhd`, `cart_slot_registers.vhd:86-87,125-126`, `fpga/io/.../io_dummy.vhd`, `io_bus_splitter.vhd:46-63`.
- `software/ModPlayer_16k/module.bin` (16384 bytes; embedded as `_module_bin_start`, filetype_reu.cc:13,151). This is 6502 code; only an opcode scan was done, no source.

### Address map
Per voice v = 0..7, base Bv = 0x10048000 + v*0x20 (`VOICE_CONTROL(x)`, sampler.h:11). The **firmware header macros are coarser
than the RTL**: sampler.h:14-20 names START +4, LENGTH +8, RATE +0xC, REPEAT_A +0x10, REPEAT_B +0x14. The RTL decodes
`address(4:0)` as follows (sampler_pkg.vhd:59-77, sampler_regs.vhd:90-163). The module.bin opcode scan writes exactly these
RTL offsets for voice 0 at $DF20: +0,+1,+2,+4..7,+9..B,+E,+F,+11..13,+15..17,+1F.

| absolute addr | width | R/W | name | meaning (RTL) |
|---|---|---|---|---|
| Bv+0x00 | 8 | W | CONTROL | bit0 enable, bit1 repeat, bit2 IRQ-on-end, bits5:4≠0 → mono16 else mono8, bit6 interleave (sampler_regs.vhd:91-102; firmware bits sampler.h:22-26) |
| Bv+0x01 | 8 | W | VOLUME | bits5:0 (sampler_regs.vhd:104-105). Power-up 0x20 (:53) |
| Bv+0x02 | 8 | W | PAN | bits3:0, 0=left, 15=right (:107-108; sampler_pkg.vhd:37). Power-up 8 (:54) |
| Bv+0x04..0x07 | 8 ×4 | W | START | big-endian 26-bit: +4 bits1:0 = addr[25:24], +5 [23:16], +6 [15:8], +7 [7:0] (:110-120) |
| Bv+0x09..0x0B | 8 ×3 | W | LENGTH | 24-bit big-endian (:140-147). +0x08 not decoded |
| Bv+0x0E..0x0F | 8 ×2 | W | RATE | 16-bit big-endian divider (:149-153; sampler.h:19-20). +0x0C/0D not decoded |
| Bv+0x11..0x13 | 8 ×3 | W | REPEAT_A | 24-bit loop-back position (:122-129) |
| Bv+0x15..0x17 | 8 ×3 | W | REPEAT_B | 24-bit loop-end position (:131-138) |
| Bv+0x1F | 8 | W | CLEAR_IRQ | data bit0=1 clears voice v's IRQ; data 0xFF clears all voices (:155-159) |
| any even addr | 8 | R | IRQ status | bit v = voice v finished with IRQ enabled (:81-83) |
| any odd addr (e.g. +0x01 = `SAMPLER_VERSION`, sampler.h:10) | 8 | R | version | constant 0x10 (:84-85) |

Decode notes (reference RTL): voice = `address(7:5)` (sampler_regs.vhd:58). Only bits 7:0 are used, and the splitter passes
the full address (io_bus_splitter.vhd:34-35), so the 256-byte block aliases through the 8K port 0x10048000-0x10049FFF
(slot_server_v4.vhd:480-495, `reqs(4) => io_req_samp_cpu -- 4048000`). The regs process has **no reset branch**
(sampler_regs.vhd:73-167): register contents survive a C64 reset. Every read/write is acked one clock later (:78).
If the FPGA is built without `g_sampler`, the port is an `io_dummy` (slot_server_v4.vhd:1010-1017) that acks and reads 0
(io_dummy.vhd:16-22).

**Firmware READS: none.** `SAMPLER_VERSION` has no user; neither the ELF nor the sources read the window.

### Init / boot sequence as seen from the bus
1. `CommandInterface cmd_if` is a global (command_intf.cc:14). Its ctor runs from the crt0 init_array loop (crt0.S:197-206) before `main` (crt0.S:218), and therefore before the scheduler (riscv_main.c:162). It reads capabilities 0x1000000C-F (itu.c:19-29). **Only if** `CAPAB_COMMAND_INTF` (0x00040000, itu.h:67) **and** `CAPAB_CARTRIDGE` (0x00000200, itu.h:58) are set (command_intf.cc:44), it creates the "UCI Reset Server" task (:60) and `resetSemaphore` (:62). There is no CAPAB_SAMPLER gate on this path.
2. After scheduler start, `run_reset_task` W 0x10000004←0x80, W 0x10000001←0x80 (command_intf.cc:101-102; ELF 0x9d53c-0x9d548), then blocks on `resetSemaphore` (:105). **No sampler access at boot.**
3. On every ITU IRQ bit7 (C64 reset): the ISR reads ITU_IRQ_ACTIVE and clears it (riscv_main.c:86-87), then calls `ResetInterruptHandlerCmdIf` (riscv_main.c:91-92 → command_intf.cc:29-36, strong symbol at ELF 0x9d270), which gives `resetSemaphore`. The task then runs `Sampler::reset()` (command_intf.cc:107): **W 0x00 to 0x10048000, 0x10048020, 0x10048040, 0x10048060, 0x10048080, 0x100480A0, 0x100480C0, 0x100480E0** (ELF 0x9d55c-0x9d578, loop at 0x9d57c). This stops all voices, including voice 7, which the C64 cannot reach (see Functional model).
4. Runtime only (user action). "Play MOD" is offered only if `CAPAB_SAMPLER` (0x00200000, itu.h:70) is set (filetype_reu.cc:41-45). `execute_st(REUFILE_PLAYMOD=0x5202)` (filetype_reu.cc:20,96-98) → `clear_sampler_registers()`: re-reads capabilities, and if bit21 is set, **W 0x00 to 0x10048000..0x1004803F** (64 bytes: voice 0 and voice 1; audio_select.h:41-46; ELF cap test 0x88e2c-0x88e38, loop 0x88e3c-0x88e4c). Then it loads the file into REU memory 0x01000000 (filetype_reu.cc:117,127; c64.h:14-15) and calls `start_modplayer()` (:139; see Region B).
5. REST `PUT/POST /v1/runners:modplay` (route_runners.cc:237-253, 268-277; ELF 0xb8b88/0xb8870) → HTTP 501 if bit21 is clear (:239-243, :270-274); otherwise it calls `start_modplayer()` directly, **without** `clear_sampler_registers`.
6. Indirect (block owned by doc 10): `C64_SAMPLER_ENABLE` 0x1004000E ← 0, then ← 1 if CAPAB_SAMPLER && cfg `CFG_C64_MAP_SAMP` (c64.cc:310, 319-326). The MOD cart's `require CART_SAMPLER` sets 1 (c64.cc:1302-1304, filetype_reu.cc:154); prohibit reads it back and clears it (c64.cc:1361-1366). This bit gates C64 access at $DF20-$DFFF (slot_server_v4.vhd:962).

### Boot hazards
- **None read-dependent.** The window is never read, so no returned value changes control flow.
- The writes in steps 3-4 must be accepted silently. There is no bus-error exception on the real hardware (doc 01 H16), so raising an access fault on 0x10048000-0x10049FFF is wrong.
- Capability bit21 (`CAPAB_SAMPLER`, byte 0x1000000D bit5) is the only feature switch. 0 → no "Play MOD" menu item, REST modplay answers 501, no clear/select writes. No hang either way. With bit21 = 1 and no C64/6502 + sampler model, "Play MOD" loads REU memory and starts a 16K cartridge (filetype_reu.cc:149-159, C64_START_CART, doc 10) that cannot produce audio. **T0: return bit21 = 0.** **Superseded by S16** (docs/specs/S16-ultimate-audio.md): with the block modelled the TRX64 build sets bit21, so "Play MOD" appears and the C64 window at `$DF20-$DFFF` answers.
- Consistency requirement on the ITU (doc 02). ITU_IRQ_ACTIVE must be masked by ITU_IRQ_ENABLE: bit7 is enabled only by `run_reset_task` (command_intf.cc:102). If an emulator reported bit7 while it is not enabled and bit18/bit9 are clear, `ResetInterruptHandlerCmdIf` would call `xSemaphoreGiveFromISR(NULL)`. That hits `configASSERT(pxQueue)` (FreeRTOS/Source/queue.c:1120) → `vAssertCalled` loops forever (system/assert.c:23-29). No race when the caps are set: the semaphore is created at ctor time, before any task runs (crt0.S:197-218, riscv_main.c:162).

### Interrupts
- **To the RISC-V: none.** The sampler IRQ (`irq <= '1' when interrupt /= 0`, sampler2.vhd:101) feeds `slot_to_io_bridge.irq_in` → `slot_resp.irq = irq_in and enable` (slot_to_io_bridge.vhd:51), which is OR-ed into the **C64 /IRQ** output (slot_server_v4.vhd:1044-1045, 1077). There is no ITU bit and no firmware handler.
- Raise: in state `playing` when position == LENGTH and CONTROL bit2 = 1 (sampler2.vhd:149-153). Ack: write +0x1F (bit0 → that voice, 0xFF → all; sampler_regs.vhd:155-159). The C64 reset (`actual_c64_reset`, slot_server_v4.vhd:994) clears all IRQ flags (sampler2.vhd:229-233).
- ITU bit7 (C64 reset) is only the **trigger** for the firmware's `Sampler::reset` writes (above); its raise/ack protocol is in doc 02 / doc 10.

### Functional model (only meaningful once a C64 core runs module.bin)
- **Bus sharing:** CPU port 0x10048000 and the C64 IO2 range $DF20-$DFFF go through a priority arbiter, CPU first (slot_server_v4.vhd:971-985). C64 address → sampler offset = addr − $DF20 (slot_to_io_bridge.vhd:61-68, 70-77; `g_slot_start "100100000"`, slot_server_v4.vhd:955-957). So $DF20 = voice 0 CONTROL, and $DFFF = offset 0xDF (voice 6 +0x1F). **Voice 7 (offsets 0xE0-0xFF) is CPU-only.** C64 access requires `C64_SAMPLER_ENABLE` (0x1004000E bit0). A C64 read returns the value latched from the previous acked IO read (slot_to_io_bridge.vhd:52,75-79). module.bin reads $DF20 (IRQ status) and $DF21 (version 0x10).
- **Voice engine** (sampler2.vhd:110-235). Voices are time-multiplexed, one per clock (voice_i 0..7, :110-114).
  - Prescale num/den = 1/2 at 100 MHz (:55): the divider advances every 2nd visit, i.e. at 100 MHz / 16 = 6.25 MHz per voice (same effective rate on every platform, :38-60).
  - `idle`: if enable → `fetch1`, position 0, divider = RATE (:129-135).
  - `playing`: when the divider reaches 0: output ← fetched sample, divider ← RATE, go `fetch1`. If position == REPEAT_B and enable && repeat → position ← REPEAT_A; else if position == LENGTH → `finished` (+IRQ). Leaves to `idle` when !enable && !repeat (:137-165).
  - `finished` → `idle` only when enable = 0 (:167-170).
  - Fetch: mono8 reads the byte at START+pos into the sample high byte, pos += 1 (+2 if interleave). mono16 reads the low byte, then the high byte, pos += 2 (+4 if interleave). Little-endian, signed (:172-198, 218-227).
  - Resulting sample rate ≈ 6 250 000 / (RATE+1) Hz (derived; the fetch states add ≤ 2 visits of 80 ns). Loop/end tests are exact equality on the post-increment position.
- **Memory:** one-byte DMA reads of `start_addr + position` (26-bit address, mem tag "110" & voice & hi/lo; sampler2.vhd:237-265). The default START 0x01000000 (sampler_pkg.vhd:46) equals `REU_MEMORY_BASE` (c64.h:14) where firmware loads the MOD. Model it as a direct read of emulated DDR at that address (mapping inferred).
- **Mix** (sampler_accu.vhd:62-84): s = (sample16 × VOLUME) >> 5. Pan: PAN<8 → L×7, R×PAN[2:0]; PAN≥8 → L×(~PAN[2:0] & 7), R×7. Saturating sum over the 8 voices, output = top 18 of 21 bits per 8-clock frame. In the reference top these go to `aud_samp_l/r`, labelled "direct outputs for mixing in U64" (ultimate_logic_32.vhd:1387-1393). Firmware mixer channels 4/5 are "Sampler L/R" (u64_config.cc:441-442, 451-452; doc 10 U64_AUDIO_MIXER 0x10100508-0x1010050B).
- **Reset:** `actual_c64_reset` forces all voices to `finished` and clears IRQs, but not the registers (sampler2.vhd:229-233, sampler_regs.vhd:73-167). That is why firmware writes CONTROL = 0 per voice on each C64 reset: it moves voices to `idle` so a stale enable does not restart playback.

### Emulator model tiers
- **T0 (pre-S16):** decode 0x10048000-0x10049FFF, ignore writes, read 0 (never read). Capability bit21 = 0. **Since S16** the window is `C64Port`'s and the backend's block serves it: even offsets read the IRQ status vector, odd ones the version 0x10, and writes reach the voices.
- **T1 (needs the separate C64 emulator):** per-voice register store per the map above, reads (even → IRQ bits, odd → 0x10), the voice state machine and mix above, a DDR fetch at START+pos, IRQ → C64 /IRQ gated by 0x1004000E bit0, the C64 $DF20-$DFFF window → offsets 0x00-0xDF, and C64 reset → voices finished + IRQs cleared. Then set capability bit21 = 1 and feed L/R into the mixer model's channels 4/5.

---

## Region B — AUDIO_SEL_BASE 0x10060700 (+0x2)

### Sources read
- `software/io/audio/audio_select.h:7-17` (AUDIO_SELECT_LEFT/RIGHT, SOUND_* codes), `:49-55` `set_sampler_output`.
- `software/filetypes/filetype_reu.cc:149-160`; `software/api/route_runners.cc:251,275`.
- `software/io/audio/audio_select.cc:232-238` (`effectuate_settings` also writes these). **Not compiled** (Makefile:44-222), so it is not a live user.
- RTL: `fpga/io/audio_select/vhdl_source/audio_select.vhd`; `ultimate_logic_32.vhd:1009-1030` (split3, `reqs(7) => io_req_aud_sel -- 4060700`), `:1372-1381`, `:1397-1447`.

### Address map
| absolute addr | width | R/W | name | meaning (reference RTL) |
|---|---|---|---|---|
| 0x10060700 | 8 | W (R = bits3:0) | AUDIO_SELECT_LEFT | 4-bit source for the U2/U2+ analog left output: 0 drive A, 1 drive B, 2 tape read, 3 tape write, 4 SID L, 5 SID R, 6 sampler L, 7 sampler R, others silence (audio_select.vhd:33-42; ultimate_logic_32.vhd:1405-1424; codes audio_select.h:10-17) |
| 0x10060701 | 8 | W (R = bits3:0) | AUDIO_SELECT_RIGHT | same codes for right (audio_select.vhd:38-39; ultimate_logic_32.vhd:1426-1445) |

Decode: split3 selects on `addr[11:8]` = 7 (ultimate_logic_32.vhd:1009-1013); inside, only `addr[3:0]` is decoded
(audio_select.vhd:35,45), so the pair aliases every 0x10 up to 0x100607FF. Reads/writes of other offsets are acked and read 0.
Reset → both 0 (audio_select.vhd:55-58). **Firmware READS: none.**

### Init / boot sequence as seen from the bus
- **Boot: no access.** The only compiled writer is `AudioConfig::set_sampler_output()`, inlined in `FileTypeREU::start_modplayer` (ELF 0x88ba0).
- Runtime: `start_modplayer` reads capabilities (ELF 0x88be8). If bit21 (`CAPAB_SAMPLER`) is set: **W 0x10060700 ← 6, W 0x10060701 ← 7** (audio_select.h:52-53; ELF 0x88c04, 0x88c0c). It then issues C64_START_CART (filetype_reu.cc:157-159).
- Callers (ELF): `FileTypeREU::execute_st` 0x88ff0 (menu "Play MOD", filetype_reu.cc:139), `Do_POST_runners_modplay` 0xb88b8, `Do_PUT_runners_modplay` 0xb8c88 (route_runners.cc:251,275).

### Boot hazards
None. It is write-only from firmware, never at boot, and gated by capability bit21. The emulator must not fault on the writes (doc 01 H16).

### Interrupts
None.

### Functional model
- On U2/U2+ this block picks what reaches the analog audio jacks.
- On the U64 family the sampler, drive, tape and SID sources go to the digital mixer instead (`aud_samp_l/r` etc., ultimate_logic_32.vhd:1387-1395; mixer registers 0x10100500-0x10100513 in doc 03/10). Whether the U64-II top keeps this selector or its output at all is an OPEN QUESTION.
- For audio on the emulated U64-II, use the mixer model; this register has no audible effect to model.

### Emulator model tiers
- **T0:** accept writes, read 0 (or the stored low nibble; firmware never reads).
- **T1:** store the 4-bit values for fidelity/debug; no further behaviour is required for the U64-II audio path.

---

## Region C — SID_BASE 0x10042000 (+0xC; legacy U2 UltiSID control) — dead in this build

### Sources read
- `software/io/audio/audio_select.h:19-30` (SID_VOICES +0, FILTER_DIV +1, BASE_L/R +2/+3, SNOOP_L/R +4/+5, ENABLE_L/R +6/+7, EXTEND_L/R +8/+9, COMBSEL_L/R +0xA/+0xB).
- `software/io/command_interface/control_target.cc:394-417` (`CTRL_CMD_GET_HWINFO` device 1): reads ENABLE_L/R and BASE_L/R under `#ifndef U64` (:395). The U64 build compiles the `#else` branch (:418ff), which uses the `C64_*_BAK` registers of doc 10 instead.
- `software/io/audio/audio_select.cc:240-260` writes all these registers plus filter coefficients at SID_BASE+0x800. The file is **not in SRCS_CC** (Makefile:44-222).
- `software/system/u64.h:55`: the U64 UltiSID block is `C64_SID_BASE = U2P_IO_BASE+0x84000 = 0x10184000` (used at u64_config.cc:1259; doc 10). It is unrelated to SID_BASE.
- RTL reference: slot_server_v4.vhd:492 (`reqs(1) => io_req_sid -- 4042000`).

### Address map
| absolute addr | width | R/W | name | status in u64ii build |
|---|---|---|---|---|
| 0x10042000-0x1004200B | 8 | — | SID_VOICES … SID_COMBSEL_RIGHT | no compiled user; no `lui 0x10042/0x10043` and no data word in the ELF |
| 0x10042800-0x10042FFF | 8 | — | filter coefficient RAM (audio_select.cc:255-257) | no compiled user |

### Init / boot sequence as seen from the bus
None. No read or write happens at any time.

### Boot hazards
None.

### Interrupts
None.

### Functional model
None required.

### Emulator model tiers
- **T0/T1:** nothing. Unmapped policy (read 0, ignore writes) is enough if ever touched.

---

## Open questions
1. Does the closed U64-II top instantiate `slot_server_v4` with `g_sampler = true`? The U2+ reference sets the default to true (ultimate_logic_32.vhd:59-61) and exposes it as capability bit21 (:309). The real capability value is still open in doc 02 (Q1); a boot-log line `*** FPGA Capabilities: %8x ***` settles it. The firmware's REST text calls the sampler "an optional part of the FPGA build" (route_runners.cc:229-230).
2. Is 0x10060700 (audio_select) present in the U64-II top, and does the U64-II have any analog path it drives? The firmware writes it unconditionally under bit21 and never reads it.
3. The sampler DMA address space on U64-II: is the 26-bit `mem_req.address` identical to the CPU DDR address (so 0x01000000 = REU_MEMORY_BASE)? Inferred from sampler_pkg.vhd:46 and c64.h:14, not proven for the closed top.
4. module.bin (the MOD player cartridge) has no source in the tree. Its register usage above comes from an absolute-opcode scan only; Amiga-period→RATE conversion and loop handling are on the 6502 side and unverified.
5. `liblwip.a` was not scanned (inherited from the critic); presumably no MMIO in these windows.
