# CPU, traps, boot entry and memory map (U64-II `ultimate` target)

All paths are relative to `firmware/1541ultimate/` (upstream @ b617777c) unless noted.
Build defines evaluated: `-DRISCV -DU64=2 -DUSB2513 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000 -DCLOCK_FREQ=100000000 -DFP_SUPPORT=1`, `-march=rv32im -mabi=ilp32 -mno-div` (target/u64ii/riscv/ultimate/Makefile:245-246).

**Summary**
- **CPU:** Gideon's own **rvlite** core (not neorv32). It runs RV32I + Zicsr + only the multiply half of M, in M-mode only.
- **Traps:** `mtvec` is direct-mode only. The one interrupt input is level-triggered and arrives as MEIP, `mcause=0x8000000B`. There are no FIRQs and no CLINT.
- **Tick:** the FreeRTOS tick comes from the ITU IRQ timer (ITU irq bit 0, 200 Hz). MTIME is not used.
- **Load and entry:** the image loads at `0x00030000` and the entry point is `_start = 0x00030000`. crt0 sets `sp = 0x00E8FFFC`.
- **Registers at entry:** crt0 initialises every register it relies on, so all-zero registers at entry work.
- **RAM:** 64 MB, never probed.
- **Floating point:** soft-float only.

---

## Sources read (files + key functions)

| File | Key content |
|---|---|
| target/u64ii/riscv/ultimate/Makefile | SRCS lists; `LIBMARCH=rv32i` "core has no divider" (:8-9); OPTIONS (:245-246); `--wrap=malloc…` (:252); `SRCS_ASMS = crt0.S port_asm.S` (:234); chip-ext VPATH (:225) |
| target/common/{environment.mk,rules.mk,ld.mk} | VPATH/include order; `.app` = `hex2bin -r` of ihex (rules.mk:57-59,69-71); libc/libgcc from `LIBMARCH` multilib (ld.mk:5-7) |
| target/u64ii/riscv/ultimate/linker.x | MEMORY, sections, crt0 symbols, fixed RAM pools |
| software/portable/riscv/crt0.S | `_start`, CSR init, .data/.bss, ctors, `main`, shutdown, `__crt0_dummy_trap_handler` |
| software/portable/riscv/riscv_main.c | `main`, `freertos_risc_v_application_interrupt_handler`, `vPortSetupTimerInterrupt`, `C_exception_handler`, `_exit`, `_sbrk`, `install_high_irq` |
| software/portable/riscv/do_ctors.c, malloc_lock.c | `_do_ctors` (not called on RISC-V), newlib malloc lock |
| software/FreeRTOS/Source/portable/risc-v/{port.c,port_asm.S,portmacro.h} | `xPortStartScheduler`, trap entry/exit, `xPortStartFirstTask`, `pxPortInitialiseStack`, `portYIELD` |
| …/chip_specific_extensions/RV32I_CLINT_no_extensions/freertos_risc_v_chip_specific_extensions.h | `portasmHAS_MTIME 0`, `portasmHAS_SIFIVE_CLINT 1`, handler name |
| software/FreeRTOS/Source/FreeRTOSConfig.h | tick, ISR stack, MTIME=0, heap size, trace hooks |
| software/FreeRTOS/Source/tasks.c, MemMang/heap_4.c | `vTaskStartScheduler`, idle task, `ucHeap` |
| software/application/ultimate/ultimate.cc | `ultimate_main` (first task) |
| software/system/u64ii_init.cc | `custom_hardware_init` (strong override) |
| software/components/init_function.{cc,h}, components/indexed_list.h | InitFunction registry and sort |
| software/filemanager/ult_syscalls.cc, system/memory_wrap.cc, system/small_printf.cc, system/itu.{c,h}, system/iomap.h, system/u2p.h, system/u64.h, system/assert.c, system/profiler.h | syscalls, heap wrappers, console, IO addresses |
| software/portable/riscv/bootloader_u64ii.c, ddr2_calibrator_u64ii.c, target/u64ii/riscv/bootloader/{Makefile,linker.x} | boot ROM (state left behind, app header format) |
| tools/hex2bin.c | `.app` record format |
| recovery/u64ii/recover.py, recovery/u64ii/ultimate.bin | JTAG recovery load address/magic; first bytes of a raw build |
| fpga/cpu_unit/rvlite/vhdl_source/{core,core_pkg,decode,decode_comb,execute,csr,multiply,fetch,rvlite_wrapper,bus_converter}.vhd | CPU semantics (open IP) |
| fpga/ip/busses/vhdl_source/{mem_bus_pkg,io_bus_pkg}.vhd | bus address widths |
| fpga/io/itu/vhdl_source/{itu,itu_pkg}.vhd, fpga/io/uart_lite/vhdl_source/uart_peripheral_io.vhd | IRQ controller, tick timer (open IP) |
| fpga/fpga_top/ultimate_fpga/vhdl_source/{u2p_riscv,ultimate_logic_32}.vhd | how open tops wire rvlite + ITU (reference only; U64-II top is closed) |
| neorv32/ (submodule) | Checked: not used by the ultimate target (see "Which CPU core") |

---

## Which CPU core (evidence)

- The U64-II boot ROM image is generated into the **rvlite** tree: `DEST = …/fpga/cpu_unit/rvlite/vhdl_source/bootrom_u64ii_pkg.vhd` (target/u64ii/riscv/bootloader/Makefile:11,48).
- The ultimate Makefile comment reads "The core has no divider" and the build uses `-mno-div` (Makefile:8-9,245). This matches rvlite, which implements only MUL/MULH/MULHSU/MULHU (multiply.vhd:1-16,39-50; decode_comb.vhd:149-155).
- crt0.S is neorv32-derived, but its neorv32 IO reset is compiled out (`#if 0`, crt0.S:146-156). No neorv32 header or IO appears in compiled sources: a grep for `neorv32`, SYSINFO or `mtime` found nothing. The ultimate Makefile never references `neorv32/`. Only the bootloader adds `neorv32/sw/common` to VPATH (bootloader/Makefile:21), and it still uses `portable/riscv/crt0.S` because that VPATH entry comes first (:20).
- In open tops, ITU `irq_out → io_irq → rvlite irq_i` (ultimate_logic_32.vhd:538; u2p_riscv.vhd:341-351).
- **Therefore:** there are no neorv32 CSRs, no SYSINFO, no CLINT/MTIME and no FIRQ bits. The U64-II top-level wiring itself is closed (see OPEN QUESTIONS).

## Instruction set and CSRs the emulator must implement

**Instructions (from rvlite decode, decode_comb.vhd:102-270):**
- **RV32I base:** LUI, AUIPC, JAL, JALR, all branches, LOAD/STORE in 8/16/32-bit widths (sign and zero extension), OP-IMM, OP.
  - Quirk, datapath (core_pkg.vhd:235-246,273-277): a 16-bit load at an odd address returns 0 (`h` keeps its `X"0000"` default, :218); a 16-bit store at an odd address gets byte-enable `"0000"` and writes nothing; a 32-bit load returns the bus word unshifted, whatever `addr[1:0]` is (:245-246). The emulator uses little-endian byte sequences at the exact address instead. That is a superset: every aligned access behaves the same.
- **M, multiply only:** MUL, MULH, MULHSU, MULHU (funct7=1, decode_comb.vhd:149-155; multiply.vhd:42-50).
  - DIV/DIVU/REM/REMU are not emitted, because of `-mno-div` and libgcc/libc built for rv32i (Makefile:9,245; ld.mk:5-7).
  - Quirk: rvlite decodes DIV/REM opcodes as MUL-low/MUL-high and does not trap. Implementing real DIV/REM is a safe superset.
- **SYSTEM:**
  - CSRRW/CSRRS/CSRRC and their immediate forms CSRRWI/CSRRSI/CSRRCI.
  - ECALL → cause 11 (:196-199). EBREAK → cause 3 (:200-203). MRET → jump to `mepc` (:204-206).
  - **WFI = NOP** (:207-208). Any other funct3=000 SYSTEM instruction = NOP (:209-211).
  - Quirk: the CSR immediate is **sign-extended** from bit 4 (execute.vhd:98). The firmware only uses immediates below 16 (`csrrci mstatus,8`, crt0.S:53; `csrc/csrs mstatus,8`, portmacro.h:110-111), so zero-extension per the spec gives the same result.
- **MISC-MEM (FENCE.I) = NOP** (:112-113).
- **Illegal instruction → cause 2:** every other major opcode (:256-259), including opcode `0x2F` (A extension), and any instruction whose low 2 bits are not `11`, i.e. compressed (:262-266). No C, A, F or D.
- **No access-fault or misaligned-fetch exceptions exist.** fetch.vhd:56-58 just clears target bits 1:0, and bus_converter.vhd has no error path.
- **Observed in firmware (compiled sources):**
  - `ecall`: `portYIELD`, portmacro.h:97.
  - `mret`: port_asm.S:297 and crt0.S:315.
  - `wfi`: crt0.S:262 only, reached only if `main` returns.
  - `ebreak`: none in compiled firmware sources (grep), but **4 in the ELF**, all in libgcc: `__divdi3` 0xFFB9C, `__moddi3` 0x100220, `__udivdi3` 0x10080C, `__umoddi3` 0x100E44 (objdump of ultimate.elf). Each is libgcc's divide-by-zero trap, skipped unless the divisor is 0 (e.g. `bnez a2` at 0xFFB98). If one fires: cause 3 → `C_exception_handler`.

**CSRs (rvlite csr.vhd:25-74,104-136):**

| CSR | Addr | rvlite behaviour | Firmware use |
|---|---|---|---|
| mstatus | 0x300 | only MIE(bit 3), MPIE(bit 7) stored and read; all other bits RAZ/WI (MPP reads 0) (:47-48,104-106) | crt0.S:53,79; portmacro.h:110-111; port_asm.S:154,243,265,349,423 |
| misa | 0x301 | RO `0x40000100` (the M bit is **not** set) (:38) | not read |
| mie | 0x304 | only MEIE(bit 11) stored (:113-114) | crt0.S:80 (=0); port.c:192 (`csrs mie,0x800`) |
| mtvec | 0x305 | stored with bits 1:0 forced to 00 → direct mode only (:109-110); undefined at reset (never reset) | crt0.S:76; riscv_main.c:171; port.c:161 (read); port_asm.S:309-310 |
| mscratch | 0x340 | R/W (:107-108) | crt0.S:221 (write after main returns) |
| mepc | 0x341 | R/W; trap loads the PC of the trapping/interrupted instruction (:111-112,123) | crt0.S:77,289-304; port_asm.S:163,242,259 |
| mcause | 0x342 | written only by traps: bit31=cause(4), bits 3:0=cause(3:0) (:120-122); software writes ignored | crt0.S:286; port_asm.S:162,241,250 |
| mtval | 0x343 | not implemented → reads 0 (:32,74) | port_asm.S:244 (read, passed to `C_exception_handler` as `value`) |
| mip | 0x344 | bit 11 = live `int_i` (read-only) (:96) | crt0.S:81 (write 0, ignored) |
| marchid/mimpid/mhartid | 0xF12/0xF13/0xF14 | RO 0x13 / 0x475A0001 / 0 (:35-37,57-59) | mhartid only in the MTIME branch of port.c:129, **not compiled** (MTIME=0) |
| everything else | e.g. 0x320 mcountinhibit, 0x306 mcounteren, 0xB00/0xB80 mcycle(h), 0xB02/0xB82 minstret(h) | **RAZ, writes ignored, no trap** (:74,115-116) | crt0.S:83-89 write zeros |

**Trap semantics (csr.vhd:120-129; decode_comb.vhd:54-75,268-270; execute.vhd:102-105):**
- **On trap:** `mepc = PC` (not PC+4, including for ECALL), `mcause = {int, cause[3:0]}`, `MPIE = MIE`, `MIE = 0`, `PC = mtvec`.
- **On `mret`:** `MIE = MPIE`, `MPIE = 1`, `PC = mepc`.
- There is no privilege change: the core has M-mode only.
- **Interrupt:** taken when `int_i & mie.MEIE & mstatus.MIE` (csr.vhd:93) and the instruction in execute is not a CSR access (`inhibit_irq`, execute.vhd:101). The interrupt replaces the instruction in decode with a trap, cause `11011` → `mcause = 0x8000000B`.
  - `int_i` is level-sensitive (core.vhd:8-10).
  - Checking the interrupt before each instruction fetch is an equivalent emulator model.
- **Exception causes used:** 2 (illegal), 3 (EBREAK), 11 (ECALL). Any exception other than cause 11 ends in `C_exception_handler` (port_asm.S:240-247 → riscv_main.c:189-195), which prints a "GURU MEDITATION" and loops forever.

**mtvec mode:** direct.
- rvlite hardwires the low bits (csr.vhd:110).
- FreeRTOS asserts `(mtvec & 3) == 0` (port.c:159-162).
- The FreeRTOS handler is `.align 8`, i.e. on a 256-byte boundary (port_asm.S:121-123).

**How an FPGA interrupt reaches the CPU:**
1. Peripheral line → ITU (`irq_in` bits 7..2, internal timer on bit 0, UART on bit 1, `irq_high` bits 7..0).
2. ITU raises `irq_out` when `irq_en & ((irq_active & imask) | (irq_high & imask_high))` (itu.vhd:245-252).
3. `irq_out` drives rvlite `irq_i`, mirrored to `mip.MEIP` → trap `0x8000000B`.
4. FreeRTOS then demultiplexes in software by reading ITU registers (riscv_main.c:82-134).

There is **no FIRQ n and no vectoring**. This follows the open tops; the U64-II top is closed (OPEN).

**FreeRTOS tick source:** the ITU IRQ timer (ITU bit 0), **not MTIME/CLINT**.
- `configMTIME_BASE_ADDRESS 0` and `configMTIMECMP_BASE_ADDRESS 0` (FreeRTOSConfig.h:62-63); `portasmHAS_MTIME 0` (chip-ext header:58).
- port.c therefore enables only MEIE (port.c:189-193).
- The strong `vPortSetupTimerInterrupt` in riscv_main.c:169-187 programs the ITU timer.
- The tick handler calls `xTaskIncrementTick` on ITU bit 0 (riscv_main.c:114-116).

**Floating point:** none in hardware.
- `-mabi=ilp32` with rv32im means soft-float via libgcc built for rv32i (ld.mk:5-7). No F/D extension and no `fcsr`.
- `FP_SUPPORT=1` only enables `%f` formatting in the firmware's own printf (small_printf.cc:93,108,135).
- Float math such as `calc_pll` (u64ii_init.cc:28-98) is soft-float.

---

## Address map

The CPU bus decode below comes from open rvlite IP (rvlite_wrapper.vhd:101-137; bus_converter.vhd:96,118-137). The U64-II top may decode further (OPEN).

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| any addr with bit 28 = 0 (not 0x8000xxxx) | 32 | RW | DDR2 memory bus | 26-bit address, 64 MB window (mem_bus_pkg.vhd:47) → wraps modulo 0x04000000 in rvlite IP |
| any addr with bit 28 = 1 | 8 | RW | IO bus | 24-bit address (io_bus_pkg.vhd:10), byte-wide; the data bus only (instruction fetches never go to IO, rvlite_wrapper.vhd:101-106) |
| 0x80000000-0x8000FFFF | 32 | RW | rvlite boot BRAM | matched on addr[31:16]=0x8000 (bus_converter.vhd:96,118); reset PC `g_start_addr=0x80000000` (rvlite_wrapper.vhd:21; fetch.vhd:68-69); U64-II image: code 0x80000000/0x1C00 + RAM 0x80001C00/0x400 (bootloader/linker.x:8-9), 8192 bytes (bootloader/Makefile:48) |
| 0x0000FFF8 | 32 | RW | BOOT_MAGIC_JUMPADDR | boot ROM jumps here if magic valid (bootloader_u64ii.c:20,171,219-222) |
| 0x0000FFFC | 32 | RW | BOOT_MAGIC_LOCATION | `0x1571BABE` = warm/JTAG start (bootloader_u64ii.c:19-21,168-171); set by recover.py:165-166 |
| 0x00010000-0x0002FFFF | — | — | boot ROM RAM test scratch | overwritten by `ram_test` (ddr2_calibrator_u64ii.c:55-60) |
| 0x00030000-0x00E8FFFF | — | RWX | linker `memory` region | ORIGIN 0x30000, LENGTH 0xE60000 (linker.x:7) |
| 0x00030000 | — | X | `_start` (.text.crt0 first) | ELF entry (linker.x:14,27; crt0.S:36-42) |
| `__BSS_START__`…`__BSS_END__` | — | RW | .bss (NOLOAD) | includes FreeRTOS `ucHeap[0x800000]` (heap_4.c:102; FreeRTOSConfig.h:24) and `xISRStack[256 words]` (port.c:70; FreeRTOSConfig.h:61) |
| `__BSS_START__+0x800` | — | — | `gp` | linker.x:143; crt0.S:64 |
| `__heap_start`(=end of .bss)…0x00E90000 | — | RW | sbrk heap | `__heap_end = __heap_limit = 0x00E90000` (linker.x:170-175); used only by `_sbrk` (riscv_main.c:216-230) |
| 0x00E8FFFC | — | RW | initial `sp` | `__crt0_stack_begin = __heap_limit-4` (linker.x:255; crt0.S:63); the pre-scheduler stack for `main` |
| 0x00EA8000-0x00EAFFFF | — | RW | `__kernal_area` | linker.x:268; c64.cc:1412-1422 |
| 0x00EB0000 / 0x00EC0000 | — | RW | `__drive_b_sound` / `__drive_a_sound` | linker.x:269-270; c1541.cc:96-103 |
| 0x00ED0000 / 0x00EE0000 | — | RW | `__drive_b_area` / `__drive_a_area` | linker.x:271-272; c1541.cc:96-103 |
| 0x00EF0000-0x00EFFFFF | — | RW | `__cart_ram_start` | linker.x:274-275; c64.h:438-439 |
| 0x01000000-0x01FFFFFF | — | RW | REU | linker.x:277-278; `REU_MEMORY_BASE 0x1000000` c64.h:14 |
| 0x02000000-0x02FFFFFF | — | RW | RAM disk | linker.x:280-281; ramdisk.cc:25-27 |
| 0x03000000-0x03BFFFFF | — | RW | `__updater_start` | linker.x:283-284; no user in compiled sources (OPEN) |
| 0x03C00000-0x03FFFFFF | — | RW | cart ROM | linker.x:286-287; c64.h:428-429 |
| 0x10000000 | 8 | RW | ITU_IRQ_GLOBAL | bit 0 = global IRQ enable (itu.h:11; itu.vhd:134-135,165-166) |
| 0x10000001 | 8 | W(R) | ITU_IRQ_ENABLE | imask \|= data (itu.vhd:136-137) |
| 0x10000002 | 8 | W | ITU_IRQ_DISABLE | imask &= ~data (itu.vhd:138-139) |
| 0x10000003 | 8 | R(W) | ITU_IRQ_EDGE | per-bit edge mode; writable only if `g_edge_write` (itu.vhd:140-143) |
| 0x10000004 | 8 | W | ITU_IRQ_CLEAR | clear edge flags (itu.vhd:144-145) |
| 0x10000005 | 8 | R | ITU_IRQ_ACTIVE | `irq_active & imask` (itu.vhd:171-172,275) |
| 0x10000006 | 8 | RW | ITU_TIMER | countdown, 1 LSB = 5 µs (itu.vhd:50,92-105; itu.c:63-78) |
| 0x10000007 | 8 | RW | ITU_IRQ_TIMER_EN | bit0 en, bit1 select (itu.vhd:148-153) |
| 0x10000008 / 0x10000009 | 8 | W / R | ITU_IRQ_TIMER_HI / _LO | reload value on write; reads return current count (itu.vhd:154-157,178-181) |
| 0x1000000A | 8 | R | ITU_BUTTON_REG | bits 7..5 buttons (itu.vhd:192-196) |
| 0x1000000B | 8 | R | ITU_FPGA_VERSION | itu.vhd:182-183 |
| 0x1000000C-0x1000000F | 8 each | R | CAPABILITIES_0..3 | big-endian 32-bit `getFpgaCapabilities` (itu.c:5-8,19-29) |
| 0x10000010-0x10000013 | 8 | RW | UART DATA/GET/FLAGS/ICTRL | itu.h:105-108; UART sub-block decodes only addr[1:0] (uart_peripheral_io.vhd:170,219) |
| 0x1000001F | 8 | W | (alias of UART_ICTRL) | written with 0x49 by the crt0 dummy trap handler (crt0.S:307-309); aliasing per itu.vhd:279-280 + uart_peripheral_io.vhd:170 |
| 0x10000022 / 0x10000023 | 8 | R | ITU_MS_TIMER_HI / _LO | free-running ms counter (itu.h:23-24; itu.vhd:107-109,223-226) |
| 0x10000027 | 8 | RW | ITU_IRQ_HIGH_EN | imask_high (itu.h:29; itu.vhd:214-215,229) |
| 0x10000028 | 8 | R | ITU_IRQ_HIGH_ACT | `irq_high & imask_high` (itu.h:30; itu.vhd:227-228) |
| 0x10060200 / 0x10060208 | 8/32 | RW | SPI flash DATA / CTRL | boot ROM only in this doc (bootloader_u64ii.c:12-14; iomap.h:25) |
| 0x10060304 / 0x10060305 | 8 | W | PROFILER_SUB / PROFILER_TASK | **written on every context switch** (FreeRTOSConfig.h:98-99; profiler.h:12-13; iomap.h:26) |
| 0x1010000C | 8 | R | U2PIO_BOARDREV | `>>3 == 0x15` = prototype (u2p.h:73; u64ii_init.cc:152,157,164-166; bootloader_u64ii.c:101,103,117) |
| 0x10100100-0x1010010C | 8 | RW | DDR2 PHY regs | boot ROM only (u2p.h:17; ddr2_calibrator_u64ii.c:12-35,213-249) |
| 0x10100400 | 8 | RW | U64_HDMI_REG | written 0x20 in `custom_hardware_init` (u64.h:15,65,93; u64ii_init.cc:126); HPD bit read in main loop (ultimate.cc:183) |
| 0x10100406 / 0x1010040A / 0x1010040B | 8 | RW | U64II_KEYB_JOY / COL / ROW | ultimate.cc:126; u64.h:71,75-76 |
| 0x10100700 | — | RW | U64II_HW_I2C_BASE | `Hw_I2C_Driver` (u64.h:34; u64ii_init.cc:116) |
| 0x10140000 | — | RW | U64II_OVERLAY_BASE | `Overlay` (u64.h:21,25; ultimate.cc:125) |
| 0x10200000-0x102000FE | 16 (array) | W | MMCM | `MMCM[i]` = byte pair at 0x10200000+2i (u64ii_init.cc:173) |
| 0x102000FF | 8 | W | MMCM_RESET | 0xB3 then 0x3B (u64ii_init.cc:174,266-267) |

**IO access width rule (important for register modelling):**
- A 16- or 32-bit load/store to an IO address becomes **2 or 4 separate 8-bit IO strobes**, at `addr, addr+1(, addr+2, addr+3)`, little-endian (bus_converter.vhd:56,82-93,159-185).
- Every byte is its own read or write event, with its own side effects.

---

## Init / boot sequence as seen from the bus

### A. Boot ROM (only if the emulator executes it; not needed when loading the ELF)

rvlite resets to PC=`0x80000000` (rvlite_wrapper.vhd:21). The bootloader is built rv32i with `make_bootloader` (bootloader/Makefile:6,38). `bootloader_u64ii.c:main`:
1. **SPI flash CS toggle:** 0x10060208 ← 0x03, 0x10060200 ← 0xFF, 0x10060208 ← 0x01, 0x10060200 ← 0xFF, 0x10060208 ← 0x03 (:146-150).
2. **Read capabilities** 0x1000000C-F and board revision 0x1010000C (:152-154).
3. **Simulation path:** if `CAPAB_SIMULATION` (0x80000000) is set, run `init_ext_pll`, then loop forever until `[0xFFFC]==0x1571BABE` and jump to `[0xFFF8]` (:156-160,214-227).
4. **Normal path:** I2C `scan_enable = 0` (:162-163), then `initializeDDR2()` (:165; ddr2_calibrator_u64ii.c:223-249, incl. DQS calibration and `ram_test` over 0x10000-0x2FFFF), then `init_ext_pll()`, which does I2C to 0xC8 with NTSC and HDMI-60 blobs and `wait_ms(2)` (:87-131,166).
5. **Warm/JTAG start:** if `[0x0000FFFC]==0x1571BABE`, clear it and `jump_run([0x0000FFF8])` (:168-171).
6. **Flash boot:** else if not `CAPAB_BOOT_FPGA`:
   - Pick the flash offset: `getFpgaType()==3` → 0x3C0000, otherwise 0x220000 (:172-176; matches the `FLASH_ID_APPL` partitions, w25q_flash.cc:53,60).
   - Read a 12-byte header `dest, length, run_address`, each 32-bit LE, via 4 byte-reads (:184-186).
   - Copy `length` bytes word-wise **only if `0 < length < 0x180000`** (:84,194-198). If the length is too big, nothing is copied but it still jumps.
   - Set CS idle (:199). If button bit 7 of 0x1000000A is clear, `jump_run(run_address)` (:201-203); otherwise "Lock" and loop forever (:205-210).
7. **Empty flash** (`length == -1`): print and wait for the magic (:213-227).

**State the boot ROM leaves for the application:**
- **CPU:** `mstatus.MIE=0` (reset, csr.vhd:132-136; never enabled), `mie=0`.
- **Registers:** the jump is a plain function-pointer call (bootloader_u64ii.c:28-34), so register contents are compiler-dependent (OPEN). crt0 does not depend on them: it sets sp and gp (crt0.S:63-64), x1 and x4-x7 (:96-102), x16-x31 (:119-134) and a0/a1 (:216-217), and uses x8-x15 only after writing them (:162-188).
- **Hardware:** DDR2 calibrated with refresh on, external PLL programmed, keyboard I2C scan disabled, SPI CS idle.
- The application never reads `0xFFF8`/`0xFFFC` (grep of compiled sources).
- **Recovery path:** recover.py uploads `ultimate.bin` (raw `objcopy -O binary`) to **0x30000** and writes `{0x00030000, 0x1571BABE}` at 0xFFF8 (recover.py:164-166).

### B. Image format

- **`ultimate.elf`:** a normal ELF32-LE-RISCV with entry `_start` = 0x00030000 (linker.x:7,14,27). `.data` has no `AT>`, so LMA=VMA and crt0's copy is a self-copy (linker.x:258-260; crt0.S:161-174).
  - The assembly labels `_start` (0x30000, GLOBAL), `__crt0_dummy_trap_handler` (0x30178, LOCAL) and `freertos_risc_v_trap_handler` (0x33700, GLOBAL) are `STT_NOTYPE` with size 0 (`readelf -sW`). A symbolizer or fault hook that indexes only `STT_FUNC` misses them.
- **`ultimate.app`:** `hex2bin -r` of the ihex (rules.mk:57-59,69-71). It is a sequence of **records** `{load_addr u32, length u32, start_addr u32, data[length rounded up to 4]}` (hex2bin.c:37-45).
  - A new record starts when the address jumps back or skips more than 32 bytes (hex2bin.c:164-178).
  - `start_addr` is the ihex type-05 entry, which comes before EOF, so it is valid only in the last record; earlier records carry 0 (hex2bin.c:105-113,152-158).
  - There is **no family/product header**: `-F/-P` are not passed for `.app`, only for `.cfw` (rules.mk:61-63; hex2bin.c:344-352).
  - The boot ROM consumes exactly one record.
- **Recommended emulator load:** ELF PT_LOAD segments, PC = `e_entry`. The raw `recovery/u64ii/ultimate.bin` confirms the layout: offset 0 = `csrrci mstatus,8`; `auipc sp,0xE60; addi sp,sp,-8` → sp=0x00E8FFFC; the `csrw` sequence follows as in crt0.S:53-89.

### C. Application start (crt0 → main → scheduler)

1. **crt0.S:53** `csrrci mstatus,8`.
2. **crt0.S:63-64** sp=0x00E8FFFC, gp=`__global_pointer$`.
3. **crt0.S:75-89** `mtvec=mepc=__crt0_dummy_trap_handler`; mstatus=0, mie=0, mip=0; CSR 0x320=0, mcounteren=0, mcycle(h)=0, minstret(h)=0. Unknown CSRs must not trap (see hazards).
4. **crt0.S:96-134** clears registers.
5. **crt0.S:161-190** `.data` self-copy, then `.bss` zeroed **word by word (more than 8 MB, i.e. at least 2M store instructions)**.
6. **crt0.S:197-208** calls `__init_array` (all C++ global constructors, in link order).
   - Each `InitFunction` global only appends itself to a list (init_function.cc:15-23).
   - `HTTPDaemon`'s constructor does `new InitFunction(…)` (httpd.cc:15-26).
   - Constructors run **before** `custom_hardware_init`, with interrupts off. `new` works before the scheduler: heap_4 plus `malloc_lock` no-ops (malloc_lock.c:10-11,22-23).
   - `_do_ctors` (do_ctors.c:10-18) is not called on RISC-V; only nios/microblaze start code calls it.
7. **crt0.S:215-218** a0=a1=0, `jal main`.
8. **riscv_main.c:154** `puts` → small_printf.cc:445-453 → `outbyte` (itu.c:282-292): **poll 0x10000012 until bit 4 (TxFifoFull) = 0**, then write byte to 0x10000010. All console output takes this path; `custom_outbyte` is still NULL (itu.c:278).
9. **riscv_main.c:155 → u64ii_init.cc:114-138** `custom_hardware_init`: `Hw_I2C_Driver` at 0x10100700, `nau8822_init`, `initialize_usb_hub`, `U64_HDMI_REG(0x10100400)=0x20`, I2C expanders 0x40 and 0x42. Runs before the scheduler: no tick and no interrupts.
10. **riscv_main.c:159** `xTaskCreate(ultimate_main, "U-II Main", 1600 words, prio 0)`.
11. **tasks.c:1967** `vTaskStartScheduler`: creates the idle and timer tasks (tasks.c:2014, `configUSE_TIMERS 1`), `portDISABLE_INTERRUPTS` (tasks.c:2039), then `xPortStartScheduler` (tasks.c:2065).
12. **port.c:157-167** asserts: `(mtvec&3)==0`, where mtvec is still the crt0 dummy handler (`.balign 4`, crt0.S:279), and `(xISRStackTop & 15)==0`. Failure → `vAssertCalled` prints and loops forever (assert.c:23-30; FreeRTOSConfig.h:72).
13. **port.c:180 → riscv_main.c:169-187** `vPortSetupTimerInterrupt`:
    - `csrw mtvec, freertos_risc_v_trap_handler` (:171). **Note:** the symbol is declared `extern void *freertos_risc_v_trap_handler;` (:79), so the C rvalue is the **4-byte word stored at the handler address**, i.e. its first instruction `addi sp,sp,-120` = `0xF8810113`, not the address. With rvlite this writes mtvec=0xF8810110. It is overwritten in step 15 before interrupts are enabled; verify on the real ELF (OPEN).
    - ITU writes, in order: 0x10000027←0x00, 0x10000007←0x00, 0x10000002←0xFF, 0x10000004←0xFF, 0x10000008←**0x07**, 0x10000009←**0xA0**, 0x10000007←0x01, 0x10000001←0x01, 0x10000000←0x01 (:178-186).
    - Reload arithmetic: `freq = ((100000000>>8)/200)-1 = 1952 = 0x07A0` (:173-176).
14. **port.c:192** `csrs mie, 0x800` (MEIE).
15. **port_asm.S:309-310** `la t0, freertos_risc_v_trap_handler; csrw mtvec,t0`. This is the final, correct vector, because `portasmHAS_SIFIVE_CLINT 1` (chip-ext header:57).
16. **port_asm.S:313-353** restores the first task frame; `csrrw mstatus` with MIE set (:347-349); `ret` into the task. **Interrupts are live from here.**
17. **ultimate.cc:79-163** `ultimate_main`, in this order:
    - Read capabilities (:87).
    - `getProductVersionString` (:90).
    - `InitFunction::executeAll()` (:94): comb-sort by `ordering`, then call each (init_function.cc:29-38).
    - `custom_outbyte` = syslog/textlog (:95).
    - If `CAPAB_CARTRIDGE`: `C64::getMachine`, `C64_Subsys`, `init`, `start` (:100-107).
    - `usb2.initHardware()` (:109).
    - If `CAPAB_ULTIMATE64`: matrix keyboard at 0x10100300 (:114-117).
    - Overlay at 0x10140000 plus `Keyboard_C64` (:122-127).
    - UserInterfaces (:135-161).
    - Main loop with `vTaskDelay(3)` (:166-207).

**InitFunction order** (ascending `ordering`; default 0 per init_function.h:23).
- **Ties have no defined order:** comb sort is unstable (indexed_list.h:191-216; compare at init_function.cc:40-51).
- **Order:**
  - 0: "SID Cart" (filetype_sid.cc:91), "Boot Cart" (c64_subsys.cc:76)
  - 1: "U64 Config" (u64_config.cc:93), "RAM Disk" (ramdisk.cc:44)
  - 9: "U64 Palette" (u64_config.cc:2880)
  - 11: "SoftIEC Drive" (iec_drive.cc:38)
  - 12: "Printer" (iec_printer.cc:118)
  - 13: "UltiCopy" (iec_ulticopy.cc:22)
  - 20: "Network Config" (network_config.cc:83)
  - 30: "Assembly 64 FS" (filesystem_a64.cc:135)
  - 31: "FTP Filesystem" (filesystem_ftp.cc:483)
  - 50: "LwIP Networking" (network_interface.cc:60)
  - 51: "RMII Interface" (rmii_interface.cc:28)
  - 52: "WiFi Application" (wifi.cc:28)
  - 60: "Tape Playback Controller" (tape_controller.cc:16)
  - 61: "Tape Recording Controller" (tape_recorder.cc:21), "LED Strip" (led_strip.cc:50)
  - 65: "C1541/71/81 Init" (c1541.cc:1275)
  - 70: "Data Streamer" (data_streamer.cc:63)
  - 98: "REU Preloader" (reu_preloader.cc:9)
  - 100: "Telnet Server" (socket_gui.cc:29)
  - 101: "FTP Daemon" (ftpd.cc:1259)
  - 102: "Raw Socket 64" (socket_dma.cc:656)
  - 103: "HTTP Daemon" (httpd.cc:20-26)
  - 105: "Modem" (modem.cc:923-928)
- "Flash Disk" is commented out (blockdev_flash.cc:197).
- All of these run inside a task with interrupts enabled, so a working tick is required.

---

## Boot hazards

| # | Where | What | Emulator must |
|---|---|---|---|
| H1 | itu.c:289 (via small_printf.cc:445-453, riscv_main.c:154) | `outbyte` busy-waits `while (UART_FLAGS & 0x10)`. The very first IO read of the app, and on every character | Read of 0x10000012 must have **bit 4 = 0** (e.g. 0x00 or 0x40). 0xFF hangs at the first `puts` |
| H2 | crt0.S:81-89 | Writes to `mip`, 0x320, 0x306, 0xB00, 0xB80, 0xB02, 0xB82 | **No trap** on unimplemented CSRs (RAZ/WI as rvlite, csr.vhd:74). If it traps, the dummy handler skips the instruction but writes 0x49 to 0x1000001F each time |
| H3 | riscv_main.c:171 | `csrw mtvec` with the handler's first instruction word (0xF8810113, reserved mode bits 11) | Accept any mtvec write; mask bits 1:0 to 0 (csr.vhd:110). Do not abort. Overwritten at port_asm.S:309-310 |
| H4 | port.c:161-162 | `configASSERT((mtvec & 3) == 0)` → `vAssertCalled` infinite loop | Read of mtvec must return a direct-mode value (bits 1:0 = 0) |
| H5 | port_asm.S:229-238, portmacro.h:97 | `portYIELD` = `ecall`; handler adds 4 to `mepc` and requires `mcause == 11` exactly | ECALL trap: `mcause = 11` (M-mode ecall), **`mepc = PC of the ecall`** (not +4). Anything else → GURU loop (riscv_main.c:189-195) |
| H6 | port_asm.S:166,225-226 | Asynchronous trap detected by `mcause < 0` (bit 31); any interrupt code → application handler | Interrupt: `mcause = 0x8000000B`, `mepc = PC of the not-yet-executed instruction` |
| H7 | port_asm.S:423-430,347-349,264-265,297 | Task frames store mstatus with 0x1880 (MPP=11, MPIE=1); first task enabled by `csrrw mstatus` with MIE set; ISR restores mstatus then `mret` | `mret`: MIE←MPIE, MPIE←1, **stay in M-mode** regardless of MPP. Keep MPIE/MIE stacking exact |
| H8 | riscv_main.c:86-116; itu.vhd:114-125,144-145,236-252,259,275; ultimate_logic_32.vhd:507-508 | FreeRTOS tick = ITU bit 0. Handler reads ACTIVE (0x10000005), writes the same value to CLEAR (0x10000004), then calls `xTaskIncrementTick` if bit 0 is set. With no tick, `vTaskDelay` (ultimate.cc:206,212) and all RTOS timeouts never return | Every **5 ms** of emulated time: set edge flag bit 0 (reload 0x07A0FF → period 499 968 sys-clock cycles at 100 MHz, about 199.99 Hz). Raise the CPU irq line while `irq_en && (flags\|levels)&imask`. ACTIVE must return `flags&imask` (bit 0 set). A write-1 to CLEAR drops bit 0 and the line |
| H9 | core.vhd:8-10; riscv_main.c:118-129 | The IRQ line is level-triggered. High IRQs are serviced for bits 0..6 only (`HIGH_IRQS 7`); bit 1 (UART) is not serviced (:110-112) | Never leave `irq_i` asserted without a source the handler can clear. `ITU_IRQ_HIGH_ACT` (0x10000028) must return `irq_high & imask_high` (0 when unused). Otherwise: an interrupt storm and a hang |
| H10 | itu.c:63-78 | `wait_ms` / `wait_10us`: write ITU_TIMER (0x10000006), then poll until the read returns 0 | Read of 0x10000006 must reach 0: count down 1 per 5 µs, or return 0 immediately (T0) |
| H11 | itu.c:80-88 | `getMsTimer` reads LO/HI (0x10000023/0x10000022) twice until both pairs are equal | The ms counter must not change between two consecutive pair reads (advance from emulated time, not per read) |
| H12 | ultimate.cc:100-107,166,209-215 | Without `CAPAB_CARTRIDGE` (0x00000200) on 0x1000000C-F: `c64=NULL`, no C64 UI, main loop skipped → "GUI running on C64 host has terminated?", task suspended | Capabilities must include **0x00000200**. The exact U64-II value is OPEN |
| H13 | ultimate.cc:114-117; w25q_flash.cc:87,213-215; u64_config.cc:904,1723; c64.cc:205 | `CAPAB_ULTIMATE64` (0x04000000) selects U64 keyboard, flash layout and config paths. 0xFFFFFFFF would also set `CAPAB_SIMULATION` (0x80000000), `CAPAB_ULTIMATE2PLUS` (0x02000000) and FPGA type bits 0x30000000 | Return a plausible U64-II capability word with **0x04000000** set and **0x80000000 / 0x02000000 clear**. Neither 0 nor 0xFF…FF is correct |
| H14 | memory_wrap.cc:30-42; heap_4.c:102 | `new` failure → "** PANIC **" infinite loop; FreeRTOS heap is an 8 MB static array in .bss | Provide RAM for the whole 0x00030000-0x00E8FFFF region (and 0x0-0x03FFFFFF for the fixed pools) |
| H15 | FreeRTOSConfig.h:98-99 | Every context switch writes 0x10060305 (PROFILER_TASK) | Accept the IO write silently (no fault, no log spam) |
| H16 | bus_converter.vhd (no error state); decode_comb.vhd (no fault causes) | Hardware has no bus-error exception | Unmapped reads return a value (0), writes are ignored. **Never raise access faults** |
| H17 | u64ii_init.cc:114-137 (and u64ii_init.cc:152,157,164-166) | I2C traffic and board-revision reads (0x1010000C `>>3 == 0x15` = prototype channel swap) run before the scheduler with interrupts off, so a poll here cannot be rescued by a tick | Make I2C busy/ack registers resolve without time-based waits. Return a non-0x15 board revision (`>>3`) for production wiring. Detailed I2C protocol: see the I2C doc (OPEN here) |
| H18 | bootloader_u64ii.c:156-160,194-203,213-227 (only if the boot ROM is executed) | `CAPAB_SIMULATION` → waits forever for magic; flash `length ≥ 0x180000` → jumps without copying; button bit 7 set → "Lock" loop | Preferred: skip the boot ROM and load the ELF. If emulated: capabilities bit 31 = 0, a valid single-record flash image under 1.5 MB at 0x220000, 0x1000000A bit 7 = 0 |

---

## Interrupts

- **CPU side:** a single level input `irq_i`.
  - Taken when `mie.MEIE(bit 11) & mstatus.MIE(bit 3)` (csr.vhd:93). `mip.MEIP` mirrors the pin (csr.vhd:96).
  - `mcause = 0x8000000B`. mtvec is direct; the handler is `freertos_risc_v_trap_handler` (port_asm.S:123).
- **Raise:** ITU `irq_out` = `irq_en & ((irq_active & imask) != 0 | (irq_high & imask_high) != 0)` (itu.vhd:245-252). `irq_active = edge_flag | (irq_c & ~iedge)` (itu.vhd:275).
- **Low IRQ bits and handler behaviour** (itu.h:33-40; riscv_main.c:91-116):

| Bit | Source | Handler action |
|---|---|---|
| 0 | TIMER | `xTaskIncrementTick` |
| 1 | UART | not handled |
| 2 | USB | `usb_irq()` |
| 3 | TAPE | `tape_recorder_irq()` |
| 4 | CMDIF | `command_interface_irq()` |
| 5 | RMII RX | `RmiiRxInterruptHandler()` |
| 6 | RMII TX | not handled |
| 7 | RESET | `ResetInterruptHandlerCmdIf/U64()`, then context switch |

- **High IRQ bits** (itu.h:41-47): 0 ACIA, 1 1541, 3 WIFI, 4 BLING, 5 HDMI, 6 UNLOCK, 7 GURU.
  - `install_high_irq` sets the bit in 0x10000027 (riscv_main.c:38-45).
  - If a bit is active with no handler installed, the handler clears its enable (riscv_main.c:118-129).
  - Bit 7 can never be installed (`irqNr < 7`).
- **Ack protocol:**
  1. Read 0x10000005 (`pending`).
  2. Write `pending` to 0x10000004. This clears edge-latched flags only; level sources must drop by themselves.
  3. Dispatch.
  4. Read 0x10000028 for the high sources; each handler acks its own device.
  5. `vTaskSwitchContext` if requested (riscv_main.c:82-134).
- **Edge configuration** in the open 32-bit top: `g_edge_init = "10000101"` (bits 7, 2, 0 edge-latched), `g_edge_write = false` (ultimate_logic_32.vhd:507-508). The U64-II value is closed (OPEN).
- **Global enable:** `ITU_IRQ_GLOBAL=1` (riscv_main.c:186). The ITU reset value is `irq_en=1`, `imask=0` (itu.vhd:256-257).
- **Before the scheduler:** mie=0 and MIE=0, so no interrupts are taken even if the ITU line is up.
- **ISR stack:** `xISRStack`, 256 words, in .bss (port.c:70-71; FreeRTOSConfig.h:61), switched to before the C calls (port_asm.S:213,225,236).
- **Context frame:** 30 words = 120 bytes on the task stack (port_asm.S:102,124-160).

## Functional model

**CPU (per step):**
1. If `irq_line & MEIE & MIE`: trap(0x8000000B, mepc=PC).
2. Otherwise fetch the 32-bit instruction at PC from RAM. Execute as RV32IM, with DIV/REM implemented to spec (never used).
3. ECALL → trap(11); EBREAK → trap(3); illegal → trap(2); MRET/WFI/CSR per the tables above.
4. Data loads/stores to 0x8000xxxx hit the boot BRAM, matched before the IO bit (bus_converter.vhd:96,118,123). Other loads/stores with bit 28 set go to the IO bus as byte events, LE-sequenced for 16/32-bit. The rest go to RAM, 26-bit (mirror, or bounds-check with RAZ, see OPEN). Instruction fetches ignore bit 28 (rvlite_wrapper.vhd:101-106).
5. There is no MMU, PMP, U/S modes, timers in the CPU, or counters.

**Timebase:** ITU ticks derive from a 100 MHz sys clock:
- tick_1us/5 → ITU_TIMER
- tick_1ms → MS timer
- IRQ timer counts sys clocks, since `irq_timer_select=0` is set by the write of 0x01 (itu.vhd:114-125,148-153)

The emulator needs a mapping from instructions to emulated time (OPEN; any stable mapping satisfies H8, H10 and H11).

**Console:** the UART TX byte at 0x10000010 is the boot log (small_printf.cc:236-243 adds `\r` before `\n`). In `ultimate_main` the log is also copied to `custom_outbyte` (ultimate.cc:95; itu.c:284-290).
- newlib fd 0-2 are not backed: `_write` on an fd with no `File` → EBADF (ult_syscalls.cc:111-117).
- `printf`, `puts` and `putchar` are the firmware's own (small_printf.cc:253-259,445-460), so they never reach newlib `_write`.

**Memory management:**
- `malloc`/`free`/`calloc`/`realloc` (plain and `_r` variants) are link-wrapped to FreeRTOS `pvPortMalloc`, with an own size header (Makefile:252; memory_wrap.cc:8-22,76-147).
- `new`/`delete` go to `pvPortMalloc`/`vPortFree` (memory_wrap.cc:44-62).
- `_sbrk` over `__heap_start..__heap_end` exists but is effectively unused. It also has a double-increment bug (riscv_main.c:224-225).
- `_exit` jumps to `__crt0_main_exit`, which writes mscratch, runs destructors, then `wfi` and `j .` (riscv_main.c:197-204; crt0.S:220-263).

**Idle:** there is no WFI in the idle task (`configUSE_IDLE_HOOK 0`, FreeRTOSConfig.h:40; tasks.c:3333). The CPU spins, so the emulator cannot sleep on WFI.

**Code reload:** `FileTypeUpdate::execute` loads a `.u2p` into RAM, sets `ITU_IRQ_GLOBAL=0` and jumps (filetype_u2p.cc:93-140). mstatus.MIE stays 1, but the ITU line is gated off. Any decoded-instruction cache must be invalidated on RAM writes.

## Emulator model tiers

**T0: boot to the running UI loop without hanging**
- **CPU:** RV32I, MUL group, Zicsr, ECALL/MRET, WFI=NOP, M-mode only.
  - CSRs: mstatus (MIE/MPIE only), mie.MEIE, mip.MEIP (RO), mtvec (direct, mask 1:0), mepc, mcause, mscratch; mtval RAZ; all others RAZ/WI with no trap (H2-H7).
- **Loader:** ELF PT_LOAD segments, PC=`e_entry` (0x00030000), registers 0.
- **RAM:** 64 MB at 0x00000000.
- **IO bus:** 0x10000000-0x10FFFFFF byte-wise with LE decomposition; default RAZ/WI and no faults (H15, H16).
- **ITU minimum:**
  - UART: FLAGS=0, DATA write → host stdout.
  - ITU_TIMER countdown or 0.
  - Stable ms counter.
  - IRQ core: GLOBAL, ENABLE, DISABLE, CLEAR, ACTIVE, TIMER_EN, TIMER_HI/LO, bit-0 edge flag every 5 ms, HIGH_EN/HIGH_ACT.
  - CAPABILITIES with 0x04000200 at least.
  - FPGA_VERSION, BUTTONS=0.
- **CPU irq line** derived from the ITU as above (H8, H9).
- **Diagnostics hooks** by symbol from the ELF: `C_exception_handler`, `vAssertCalled`, `get_mem` PANIC, `as_yet_unhandled` (not compiled here).
- Other peripherals (I2C, USB, HDMI, overlay, C64) only as needed so their own polls terminate (see their docs).

**T1: faithful**
- Full itu.vhd semantics: edge/level per `g_edge_init`, UART RX and IRQ, `irq_timer_select`, the timer register readback of the live counter.
- rvlite quirks where observable: DIV decoded as MUL, zimm sign extension, `inhibit_irq` latency.
- RAM mirroring per the real top.
- Optional boot ROM execution from 0x80000000 with DDR2/SPI-flash models and the `{dest,length,run}` record loader.
- Decoded-block cache invalidation for `.u2p` jumps.
- Idle-loop detection (PC in `prvIdleTask`) to throttle the host CPU.
- Emulated-time mapping calibrated to 100 MHz.

## Open questions

1. **OPEN QUESTION:** The U64-II FPGA top is closed. Is the CPU really rvlite with `g_mult=true`, `g_icache=true`, `g_start_addr=0x80000000`, and is ITU `irq_out` its only interrupt source? The evidence is indirect (bootloader/Makefile:11; ultimate Makefile:8-9; the open tops).
2. **OPEN QUESTION:** The U64-II ITU generics are unknown: `g_capabilities` (exact 32-bit value at 0x1000000C-F), `g_version` (0x1000000B), `g_edge_init`/`g_edge_write` (assumed `"10000101"`/false from ultimate_logic_32.vhd:507-508), and which peripherals drive `irq_in`/`irq_high`.
3. **OPEN QUESTION:** Memory decode on U64-II: DDR size (64 MB implied by linker.x:268-287 and the 26-bit mem bus) and whether addresses ≥0x04000000 alias. IO decode above the ITU: which address bits select the U2P_IO_BASE (0x10100000) and MMCM (0x10200000) blocks, and whether they alias. The open u2p top splits only on bit 20 (u2p_riscv.vhd:353-367).
4. **OPEN QUESTION:** Is the sys clock really 100 MHz? `CLOCK_FREQ` is only a define. This sets the tick at 200 Hz and the ITU timer units.
5. **OPEN QUESTION:** Confirm on the real `ultimate.elf` that riscv_main.c:171 loads the handler's first word into mtvec (C semantics of `extern void *` rvalue). No RISC-V objdump was available here. Also confirm the actual CSR and opcode set.
   - `recovery/u64ii/ultimate.bin` contains only 3 M-extension words in about 246k words, so it is probably an older rv32i build of unknown version. Use it only for layout (load address 0x30000, sp 0x00E8FFFC), not for instruction coverage.
6. **OPEN QUESTION:** Does the current full `ultimate.app` fit the boot ROM limit (single record, `length < 0x180000`, bootloader_u64ii.c:84,195)? The flash partition allows 0x1E0000 (w25q_flash.cc:53). This matters only if booting from a flash image.
7. **OPEN QUESTION:** What registers does the boot ROM leave (`jump_run`, bootloader_u64ii.c:28-34)? Irrelevant for crt0, noted for completeness.
8. **OPEN QUESTION:** Who uses `__updater_start` (0x03000000-0x03BFFFFF)? There is no reference in compiled sources. Ownership and timing of the RAM regions shared with FPGA logic (REU, cart ROM/RAM, drive areas, kernal area) belongs to the C64/drive docs.
9. **OPEN QUESTION:** Polls in `Hw_I2C_Driver`, `nau8822_init` and `initialize_usb_hub` run pre-scheduler (u64ii_init.cc:116-137). Their exact required responses belong in the I2C/USB-hub doc.
10. **OPEN QUESTION:** Instruction-to-emulated-time mapping and idle detection strategy. There is no WFI and no MTIME to key off.
