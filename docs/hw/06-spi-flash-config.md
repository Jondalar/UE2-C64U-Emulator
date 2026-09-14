# SPI flash, flash layout and persistent config storage (U64-II, RISC-V build)

All paths are relative to `firmware/1541ultimate/`. Active defines: `RISCV U64=2 OS IOBASE=0x10000000 U2P_IO_BASE=0x10100000`.
`IOBASE`-relative macros: `software/system/iomap.h:10-35`.

## Sources read

| File | Key content |
|---|---|
| `target/u64ii/riscv/ultimate/Makefile` | SRCS_CC includes `flash.cc w25q_flash.cc s25fl_flash.cc s25fl_l_flash.cc prog_flash.cc config.cc blockdev_flash.cc embedded_*.cc filetype_u2p.cc`. `at45_flash.cc` and `at49_flash.cc` are **not** built. |
| `software/io/flash/flash.h`, `flash.cc` | `Flash` base class; self-registering type list; `get_flash()` probe loop |
| `software/io/flash/w25q_flash.h/.cc` | SPI register macros, U64-II layout tables, `tester()`, `read_dev_addr`, `read_page`, `write_page`, `erase_sector`, `wait_ready`, `read_serial`, config-page mapping, `reboot()` (ICAP) |
| `software/io/flash/s25fl_flash.h/.cc` | Spansion S25FLxxxK (JEDEC 01 40 xx) tester, which shadows the geometry members |
| `software/io/flash/s25fl_l_flash.h/.cc` | Infineon S25FLxxxL (JEDEC 01 60 17/18) tester, 4-byte read/program/erase, 24 config pages |
| `software/io/flash/prog_flash.cc` | `flash_buffer_at` (used only by update apps, not by the app boot) |
| `software/filesystem/blockdev_flash.h/.cc` | FAT block device on the flash, `init_flash_disk`, auto-format |
| `software/components/config.h/.cc` | `ConfigManager/ConfigPage/ConfigStore/ConfigItem`: page discovery, pack/unpack format, file fallback |
| `software/filesystem/embedded_{d64,t64,iso,fat}.cc` | **Not flash related.** File-in-file filesystem factories for `.D64/.D71/.D81/.DNP/.T64/.ISO/.FAT` files |
| `software/system/itu.c`, `itu.h` | `getFpgaCapabilities`, `getFpgaType`, `wait_ms`, `getMsTimer` |
| `software/system/u64.h` | `U64_RESTORE_REG` |
| `software/io/icap/icap.h` | ICAP register macros (reboot) |
| `software/io/network/rmii_interface.cc`, `software/system/product.cc`, `software/io/c64/c64.cc`, `c64_subsys.cc`, `software/userinterface/configio.cc`, `software/network/network_config.cc`, `software/u64/u64_config.cc` | Flash consumers: MAC/serial, config store registration, flash dump, reboot |
| `software/portable/riscv/bootloader_u64ii.c` | Not in this ELF; shows the APPL image header and the preamble before the app runs |
| `software/application/u64ii_prepare_fat/flash_disk_prep.cc` | Factory FAT contents (not in this ELF) |
| `software/application/u64ii_tester/u64ii_programmer.cc` | Factory programming addresses per FPGA type (not in this ELF) |
| `fpga/io/spi/vhdl_source/spi_peripheral_io.vhd`, `spi.vhd` | Register and CS semantics of the SPI master (open U2/U2+ IP) |
| `fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd:1009-1033,1115-1134` | Flash instance at IO+0x60200: `g_fixed_rate=>true, g_init_rate=>1, g_crc=>false`, `SD_DETECTn=>'0', SD_WRPROTn=>'1'` |
| `fpga/cpu_unit/rvlite/vhdl_source/bus_converter.vhd` | How 32-bit CPU accesses become 8-bit IO accesses (byte order) |
| `fpga/io/itu/vhdl_source/itu.vhd:80,107-109,223-226` | ms timer is a free-running hardware counter |
| `fpga/io/icap/vhdl_source/icap-spartan.vhd` | ICAP register semantics (Spartan-3A version; U64-II version is closed) |

## Address map

### SPI flash controller: `FLASH_BASE = IOBASE+0x60200 = 0x10060200` (`iomap.h:25`)

The controller decodes only `io_req.address(3 downto 2)` (`spi_peripheral_io.vhd:91,116`). The 4 registers therefore repeat every 0x10 inside the 0x100 window that `i_split3` selects (`ultimate_logic_32.vhd:1009-1022`); aliasing on the U64-II is an OPEN QUESTION. The firmware only uses +0x00 and +0x08.

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10060200 | 8 | W | `SPI_FLASH_DATA` (`w25q_flash.h:27`) | Start one 8-bit full-duplex transfer with MOSI=value (`spi_peripheral_io.vhd:92-94`). The write ack waits for any previous transfer to finish (`:87-89`). The received byte is latched but not returned. |
| 0x10060200 | 8 | R | `SPI_FLASH_DATA` | Start one transfer with MOSI=0xFF and return the MISO byte of **that** transfer. The bus stalls until it is done (`:117-120,142-147`). |
| 0x10060200..03 | 32 | R/W | `SPI_FLASH_DATA_32` (`w25q_flash.h:28`) | Becomes 4 sequential 8-bit accesses at offsets 0,1,2,3 (`bus_converter.vhd:56,160-184`). Offset 0 carries bits 7:0 (`:82-93,160-168`). Result: 4 SPI bytes, **first byte = LSB** of the word, so memory order on little-endian RV32 equals flash order. |
| 0x10060204 | 8 | R/W | RATE | Fixed on the flash instance (`g_fixed_rate=>true`, rate=1). Write is ignored; read returns 0x01 (`spi_peripheral_io.vhd:96-100,122-125`). Never used by the firmware. |
| 0x10060208 | 8 | W | `SPI_FLASH_CTRL` (`w25q_flash.h:29`) | bit0 `SPI_FORCE_SS`, bit1 `SPI_LEVEL_SS` (`w25q_flash.h:31-32`, `spi_peripheral_io.vhd:102-104`). Values used: `0x00` = auto CS (CS low only while a byte shifts); `0x01` = CS forced low; `0x03` = CS forced high. |
| 0x10060208 | 8 | R | CTRL readback | `0000 & !WRPROTn & !DETECTn & level_ss & force_ss` = `0x04 \| level<<1 \| force` with the U2+ tie-offs (`spi_peripheral_io.vhd:127-128`, `ultimate_logic_32.vhd:1128-1129`). **Never read** by the firmware (all `SPI_FLASH_CTRL` uses are assignments). |
| 0x1006020C | 8 | W/R | CRC | Write clears the CRC; read returns 0x00 because `g_crc=false` (`spi.vhd:134`). Unused. |

Reset state: `force_ss=0`, `level_ss=1`, so CS is idle high (`spi_peripheral_io.vhd:153-160`, `spi.vhd:53-54`).

### Other registers this block reads or writes (owned by other blocks)

| Absolute addr | Width | R/W | Name | Use in this block |
|---|---|---|---|---|
| 0x1000000C..0F | 8 each | R | `CAPABILITIES_0..3` (`itu.c:5-8`) | Assembled big-endian into a u32 (`itu.c:19-29`). `getFpgaType() = (cap & 0x30000000) >> 28` (`itu.c:40-47`, `itu.h:77,81`), i.e. bits 5:4 of 0x1000000C. It selects the flash layout (`w25q_flash.cc:81-83`). |
| 0x10000006 | 8 | R/W | `ITU_TIMER` (`itu.h:17`) | `wait_ms()` writes 200, then polls until 0 (`itu.c:63-71`). Called 3x in the S25FL-L probe. |
| 0x10000022 / 0x10000023 | 8 | R | `ITU_MS_TIMER_HI/LO` (`itu.h:23-24`) | `getMsTimer()` re-reads until two consecutive 16-bit samples match (`itu.c:80-88`). Used for flash busy timeouts. |
| 0x10100402 | 8 | R | `U64_RESTORE_REG` (`u64.h:15,67`) | `==1` enables safe mode (`config.cc:47-50`) and skips the flash disk (`blockdev_flash.cc:160-163`). |
| 0x10060604 | 8 | W | `ICAP_PULSE` (`icap.h:7`, `iomap.h:29`) | Only in `W25Q_Flash::reboot` (`w25q_flash.cc:380-386`) |
| 0x10060608 | 8 | W | `ICAP_WRITE` (`icap.h:8`) | Only in `W25Q_Flash::reboot` |

### CS / frame semantics (what the flash chip sees)

- `spi.vhd:59-62`: on each transfer start SSn goes '0'. At the end of the byte (`done`, `:96-101`) SSn goes back to '1'. Every clock, `if force_ss='1' then SPI_SSn <= level_ss` (`:128-130`) overrides this.
- Therefore: CTRL=0x00 means **each byte is its own CS frame**. CTRL=0x01 means CS stays low across bytes (one frame). CTRL=0x03 means CS stays high, and bytes shift out with the chip deselected (the chip ignores them).
- A frame ends when effective CS rises: a CTRL write to 0x03 or 0x00 while a forced-low frame is open.

## Flash chip identity and geometry

### Probe mechanism

- Each chip driver is a global object whose base constructor appends `this` to a function-static list (`flash.h:25-32`): `w25q_flash` (`w25q_flash.cc:18`), `s25fl_flash` (`s25fl_flash.cc:18`), `s25flxxxl_flash` (`s25fl_l_flash.cc:18`). The constructors run in the crt0 loop (`crt0.S:194-206`) and do no bus I/O.
- List order follows `.init_array` input order (unsorted `KEEP(*(.init_array ...))`, `linker.x:89`), i.e. SRCS_CC link order: W25Q, S25FL, S25FL-L. (This is derived from link order; see Open questions.)
- `get_flash()` runs **every** tester, in list order, on **every call** and returns the first match (`flash.cc:16-22`). If none matches it returns `new Flash()`. That constructor appends yet another stub to the list, so the list grows on every failed call (`flash.cc:24`, `flash.h:30-32`).

### Tester sequences (bus level)

| Tester | Sequence | Accepts |
|---|---|---|
| W25Q (`w25q_flash.cc:125-180`) | CTRL=03; DATA=FF (deselected); CTRL=01; DATA=9F; 3x read DATA; CTRL=03 | manuf 0xEF, type 0x40, cap 0x14/15/16/17/18 (8..128 Mbit) |
| S25FL K (`s25fl_flash.cc:33-78`) | same as W25Q | manuf 0x01, type 0x40, cap 0x15/16/17 |
| S25FL L (`s25fl_l_flash.cc:32-95`) | CTRL=01; 66; 99; CTRL=03; wait_ms(1); CTRL=01; 66; CTRL=03; CTRL=01; 99; CTRL=03; wait_ms(1); CTRL=01; FF; CTRL=03; wait_ms(1); CTRL=01; 9F; 3x read; CTRL=03 | manuf 0x01, type 0x60, cap 0x17/0x18 |

### Which chip to emulate

**Recommended: Infineon S25FL128L, JEDEC `01 60 18`, 16 MiB.**
- The U64-II layout tables reserve `CONFIG = 0xFE8000, len 0x18000` (`w25q_flash.cc:55,62`). That is exactly 24 x 4 KiB, which equals `S25FLXXXL_NUM_CONFIG_PAGES = 24` (`s25fl_l_flash.h:9`) on a 4096-sector chip (`s25fl_l_flash.cc:79-83`).
- With this identity, `get_type_string()` returns "Infineon S25FL128L" (`s25fl_l_flash.cc:97-109`).
- Alternative: Winbond W25Q128, `EF 40 18` (`w25q_flash.cc:172-176`). It works too, but it has only 16 config pages, so config lives at 0xFF0000..0xFFFFFF (`w25q_flash.h:8`, `w25q_flash.cc:256-267`). The firmware does not care which; a flash image made for one identity will not show its config under the other.
- **Do not emulate `01 40 xx`** (S25FL K). `S25FL_Flash` declares its own private `sector_size/sector_count/total_size` (`s25fl_flash.h:31-33`). The inherited W25Q code (config pages, erase) keeps the W25Q defaults of 512 sectors / 8192 pages (`w25q_flash.cc:65-70`), which puts config pages at 0x1F0000, inside the FAT area.

Geometry for the S25FL128L identity: page = 256 B (`s25fl_l_flash.h:11`), sector = 16 pages = 4096 B, `sector_count=4096`, `total_size=65536` pages (`s25fl_l_flash.cc:79-83`, `w25q_flash.cc:200-208`). `get_number_of_pages()` = 65536 (`w25q_flash.h:61`).

## Flash layout (U64-II)

`W25Q_Flash::get_image_addresses` (inherited by both S25FL classes): `U64==2` selects `(getFpgaType() >= 3) ? u64ii_100t : u64ii_50t` (`w25q_flash.cc:78-106`).

| Region | 50T table (`w25q_flash.cc:51-56`) | 100T table (`w25q_flash.cc:58-63`) | Contents |
|---|---|---|---|
| `FLASH_ID_BOOTFPGA` 0x00 | 0x000000, len 0x220000 (bitstream 0x2172F9) | 0x000000, len 0x3C0000 (0x3A60EA) | FPGA bitstream. Not needed by the emulator. |
| `FLASH_ID_APPL` 0x02 | 0x220000, len 0x1E0000 | 0x3C0000, len 0x1C0000 | `ultimate.app` (`hex2bin -r`, `target/common/rules.mk:57-59`). `has_header=0`. Starts with 3 LE u32 words: load addr, length, run addr; length 0xFFFFFFFF means empty (`bootloader_u64ii.c:176-196`). Not needed when loading the ELF directly. |
| `FLASH_ID_FLASHDRIVE` 0xFD | 0x400000, len 0xBE8000 = 3048 x 4 KiB | 0x580000, len 0xA68000 = 2664 x 4 KiB | FAT "FlashDisk", mounted as `/Flash` |
| `FLASH_ID_CONFIG` 0xFE | 0xFE8000, len 0x18000 | same | 24 config sectors (S25FL-L); W25Q uses 0xFF0000..0xFFFFFF |
| `FLASH_ID_LIST_END` 0xFF | 0xFFE000 | same | sentinel only |

- The factory programmer uses the same split: `appl = (type==3) ? 0x3C0000 : 0x220000`, `fat = (type==3) ? 0x580000 : 0x400000` (`u64ii_programmer.cc` around `fpgatype_id`, ~line 440).
- `W25Q_Flash::read_image` uses `flash_addresses_u64` (U64 **I** table; `w25q_flash.cc:210-229`). It is wrong for U64-II, but it has no callers in this build (only update and 2nd_boot apps call it).

### FAT flash disk (`blockdev_flash.cc`)

- `BlockDevice_Flash` constructor (`:14-41`): `sector_size = get_sector_size() = 4096`, `pages_per_sector = 16`, `first_page = start/256` (0x4000 on 50T, 0x5800 on 100T). `number_of_sectors = max_length/4096` (3048 / 2664). State is `e_device_ready` only if `addr.id == 0xFD`.
- `read` (`:62-78`): 16x `read_page_power2`, then `read_page`. On S25FL-L that is opcode 0x13 with a 4-byte address, using `DATA_32` (`s25fl_l_flash.cc:111-131`).
- `write` (`:80-102`): `erase_sector` (0x21) of the whole 4 KiB sector, then 16x `write_page` (0x12). There is **no verify**. Only `wait_ready` failure yields `RES_ERROR`.
- `init_flash_disk` (`:148-183`): `attach_disk(4096)`, then `probe()`. If `< 1`: `format_flash()` (`f_mkfs`, `FM_SFD|FM_ANY`, 1 FAT, `filesystem_fat.cc:107-120`), then probe again. On success: `add_root_entry` "Flash".
- `Disk::Init` accepts an MBR-less volume if sector 0 has the 0xAA55 signature and "FAT" at `BS_FilSysType`/`BS_FilSysType32` (`disk.cc:64-90`). A blank (0xFF) sector 0 gives a single type-0 partition; its filesystem attach fails, so probe returns -1 (`file_device.cc:82-93`) and the disk is **auto-formatted**.
- FatFs is built with `FF_MAX_SS 4096` (`chan_fat/full/ffconf.h:196`).
- Factory contents (`flash_disk_prep.cc:81-100`):
  - `/roms/{1581.rom,1571.rom,1541.rom,snds1541.bin,snds1571.bin,snds1581.bin}`
  - `/carts/`
  - `/html/{index.html,api.html,openapi.yaml}`
  - `/config/iec_partitions.ipr`
- Runtime paths on this disk:
  - `/flash/config` (`config.h:30`)
  - `/flash/roms` (`c64.h:12`; loads at `c64.cc:1091-1112`)
  - `/flash/carts` (`c64_crt.h:8`)
  - `/flash/data` (`u64_config.h:21`)
  - `/Flash/html` (`middleware.h:14`)
  - `/Flash/apps` (`user_file_interaction.cc:59`)
  - `/flash/*.txt` (`modem.cc:83-85`)

## Persistent config storage (`config.cc`)

### Mapping

`read_config_page(p, len, buf)`:
1. `page = (p + sector_count - n_cfg_pages) * sector_size`
2. `addr = page << 8`
3. `read_dev_addr(addr)` (`w25q_flash.cc:261-267`)

The read uses opcode **0x03 with a 3-byte address** (`w25q_flash.cc:108-123`), even on S25FL-L.

On S25FL-L, config page `p` (0..23) is at `0xFE8000 + p*0x1000`.

### Page size and write

- Logical config page = 512 B (`get_config_page_size = 2*256`, `w25q_flash.cc:251-254`).
- `write_config_page`: erase the 4 KiB sector, then program 2 flash pages (`w25q_flash.cc:269-277`). Return values are ignored.
- `clear_config_page`: erase only (`:279-284`).

### Page format

Built by `ConfigPage::pack` (`config.cc:275-290`) from `ConfigStore::pack` (`:305-325`) and `ConfigItem::pack` (`:706-758`):

| Offset | Content |
|---|---|
| 0..3 | page id, u32 **native little-endian** (`config.cc:280`). Example: `STORE_PAGE_ID 0x55363443` is stored as `43 34 36 55` (`u64_config.cc:99`). |
| 4.. | items: `id(1) type(1) len(1) payload(len)` |
| | VALUE (type 1): len=4, **big-endian** (`:724-729`) |
| | ENUM (type 2): len=1 (`:736-738`) |
| | STRING/STRFUNC/STRPASS (3/7/8): len=strlen, bytes (`:747-749`) |
| | FUNC/SEP/INFO: not stored |
| end | `0xFF` after the last store (`:322`); the rest is filled with 0xFF (`:277`) |

- Several stores with the same page id share one page, packed one after another (`config.cc:117-123,283-288`).
- `unpack` (`:398-424`, `:671-704`) stops at id 0xFF, rejects a len that does not fit, skips items on type mismatch, and resets out-of-range values to the default.

### Discovery (`ConfigManager::register_store`, `config.cc:89-184`)

1. For i in 0..23: read 4 bytes of config page i; if equal to `page_id`, use it (`:127-137`).
2. Otherwise try the file `/flash/config/page_%08x.bin` (`:139-152`).
3. Otherwise take the first page whose id reads `0xFFFFFFFF` and **write it immediately** (`:154-167,179-181`).
4. Otherwise use a file-backed page (`:169-174`); `ConfigPage::write` then goes to FAT (`:366-387`).
5. Finally `s->read(safeMode)` reads the 512 B page (or fills 0xFF in safe mode) (`:433-464`).

### Explicit writes

- `ConfigIO::S_save` writes stale stores (`configio.cc:131-141`).
- `S_reset` resets and writes every store (`:145-160`).
- `S_clear` erases all 24 pages (`:162-180`).

Known page ids: `0x55363443` (U64 stores, `u64_config.cc:99,480,523,824,853,906`), `0x43363420` (`c64.cc:132`), `0x4E455400` (`network_config.cc:48`), `0x4E657477` (`network_interface.cc:177`), `0x57494649` (`network_esp32.cc:119`), `0x54415045`, `0x4D4F444D`, `0x4d505300`, `0x49454300`, `0x4C454453`, `0x44617461`. Drives use their register base as id (`c1541.cc:126`).

## Init / boot sequence as seen from the bus

1. crt0 constructor loop (`crt0.S:194-206`): flash drivers register; **no SPI I/O**. `NetworkConfig`'s constructor is empty (`network_config.cc:36-38`).
2. `main` then `custom_hardware_init` (no flash access), then `xTaskCreate(ultimate_main)` and the scheduler (`riscv_main.c:146-160`).
3. `ultimate_main` then `InitFunction::executeAll()` (`ultimate.cc:79-94`), sorted by ordering (`init_function.cc:29-36`). The `flashdisk_init` InitFunction is commented out (`blockdev_flash.cc:197`).
4. **Ordering 1, "U64 Config"** (`u64_config.cc:89-93`): the `U64Config` constructor calls `register_store` (`u64_config.cc:892-906`). This constructs `ConfigManager` for the first time (`config.h:291-294`), which calls:
   1. `get_flash()` (`config.cc:37`) with the full tester sequence above. First SPI traffic: CTRL=03, FF, CTRL=01, 9F, 3 reads, ...
   2. `num_pages = 24` (`config.cc:42`).
   3. Read `U64_RESTORE_REG` 0x10100402 (`config.cc:48`).
   4. `init_flash_disk()` (`config.cc:66`), which does:
      - `get_flash()` again (full probe);
      - read 0x10100402 again (`blockdev_flash.cc:161`);
      - read CAPABILITIES 0x1000000C-F (`w25q_flash.cc:81`);
      - read FAT sector 0: 16 frames of `CTRL=01, 13, 00, 00|40|58, xx, 00, 64x DATA_32 read, CTRL=03`;
      - FatFs mount reads; if the probe fails, format writes (erase and program frames, see Functional model); root entry "Flash".
   5. `create_dir("/flash/config")` (`config.cc:67`), which writes FAT if the directory is missing.
   6. Config discovery reads: up to 24 frames of `CTRL=01, 03, FE, 8x|9x|Ax|..., 00, 4 reads, CTRL=03`. Then the file lookup, and on an empty slot: `CTRL=00, 06 | CTRL=01, 21, 00,FE,8x,00, CTRL=03 | CTRL=01, 05, poll... CTRL=03 | CTRL=00, 04, CTRL=03`, then the same with `12` + 256 bytes, twice. Then a 512-byte `03` read.
5. The following InitFunctions and constructors call `register_store` in turn, each repeating step 4.6 (for example `c64.cc:130-132` calls `get_flash()` again first). `NetworkConfig::init` (ordering 20) → `getProductDefaultHostname` → `get_flash()` + `read_serial` (`network_config.cc:44-48`, `product.cc:185-190`).
6. RMII task: `get_flash()` + `read_serial`. The MAC is `02:15:41:uid1^uid5:uid2^uid6:uid3^uid7` (`rmii_interface.cc:117-128`).
7. On demand:
   - `getProductUniqueId` (`product.cc:220-237`)
   - System info type string (`system_info.cc:178-180`)
   - Flash dump of all 65536 pages (`c64_subsys.cc:375-392`)
   - Network flash page read (`socket_dma.cc:313-342` → `c64_subsys.cc:453-465`)
   - Hard boot via ICAP (`c64_subsys.cc:450-452`)

`protect_disable/protect_configure/0x65/0x01` are called only by the update and tester apps, **never by this ELF**.

## Boot hazards

| # | Where | Condition | Consequence | Required response |
|---|---|---|---|---|
| H1 | `w25q_flash.cc:132-144`, `s25fl_l_flash.cc:56-84`, `flash.cc:16-24` | JEDEC reads 00/FF/unknown | Base `Flash` stub: 0 config pages (`flash.h:61`), so no persistence. The flash disk is built with an uninitialized `addr` (`blockdev_flash.cc:17`, `flash.h:45` no-op) and normally ends in no media, so no `/flash` (no roms/html/config files). `read_serial` is a no-op, so the MAC comes from an uninitialized stack buffer (`rmii_interface.cc:119-128`). The type list grows on every `get_flash()`. | `01 60 18` (S25FL128L; recommended) or `EF 40 18` (W25Q128). Never `01 40 xx`. |
| H2 | `s25fl_l_flash.cc:40,48,53` → `itu.c:63-70` | S25FL-L probe is reached (always, unless the chip answers `EF 40 xx`) and `ITU_TIMER` 0x10000006 never reaches 0 | **Hang** in the first `get_flash()` (InitFunction "U64 Config") | ITU_TIMER must count down after a write (cross-block, ITU) |
| H3 | `w25q_flash.cc:489-506` (`wait_ready`), called with interrupts masked (`:313,347`) | SR1 bit0 (BUSY) stays 1, and/or the ms timer 0x10000022/23 is frozen or unstable across two consecutive reads (`itu.c:83-86`) | **Hang**; or 15 ms per page / 1000 ms per sector stalls and failed writes. The ms counter must be free-running hardware, not tick-IRQ based (`itu.vhd:107-109`). Note: `(now-start) > time_out` on uint16 values promoted to int, so a 16-bit wrap during a BUSY wait delays the timeout by ~65 s (`w25q_flash.cc:494-498`). | After `05` in a CS-low frame, return SR1 on **every** clocked byte with BUSY=0 (or clear BUSY after the modelled time). ms timer: +1/ms, stable between reads. |
| H4 | `config.cc:47-50`, `blockdev_flash.cc:160-163` | `U64_RESTORE_REG` 0x10100402 reads 1 | Safe mode (flash config ignored, defaults) and **no flash disk** | Return 0 (anything except 1) for a normal boot |
| H5 | `w25q_flash.cc:81-83`, `itu.c:40-47` | FPGA type (bits 5:4 of 0x1000000C) does not match the provided image | FAT not found at 0x400000 / 0x580000, so the firmware auto-formats at the wrong offset and overwrites the APPL/bitstream areas of the image. Also `filetype_u2p.cc:82` (type >=3: only `.cfw` updates). | Capabilities FPGA type consistent with the image layout (>=3 = 100T layout) |
| H6 | `blockdev_flash.cc:174-178`, `disk.cc:64-90`, `filesystem_fat.cc:107-120` | FAT area blank or unreadable, or writes not retained | `f_mkfs` at every boot. If writes are not persisted, the second probe fails and there is no `/flash`: config falls back to nothing, and `/flash/roms` (`c64.cc:1091-1104`) and `/Flash/html` are missing. | Pre-formatted FAT (VBR at the region start, 4096-byte sectors) **or** working erase/program with a readback that matches |
| H7 | `spi.vhd:53-62,96-101,128-130`; WREN/WRDI at `w25q_flash.cc:314-315,336-338,348-349,358-360`, `s25fl_l_flash.cc:140-141,175-176` | Emulator treats CTRL=0x00 as "chip deselected", or passes bytes sent with CTRL=0x03 to the chip | WREN never latched, so PP/SE are ignored silently. `write_page` still returns true; config is not persisted; FAT is corrupt, leading to a format loop. Forwarding the deselected `FF` (`w25q_flash.cc:128-129`) corrupts the command stream. | CTRL=0: each byte is a complete frame. CTRL=1: bytes join the open frame. CTRL=3: bytes are dropped. A CTRL write that raises effective CS ends the frame. |
| H8 | `s25fl_l_flash.cc:111-191` vs `w25q_flash.cc:108-123` | S25FL-L identity but only the 3-byte opcodes are implemented | `0x13` reads return FF, so FAT reads blank and the disk is reformatted at every boot. `0x12/0x21` are ignored, so config writes are lost. | Implement `03` (3-byte), `13`/`12`/`21` (4-byte), `05`, `06`, `04`, `9F`, `4B`, `66`, `99`, `FF` |
| H9 | `bus_converter.vhd:56,82-93,160-184` | `DATA_32` byte order reversed | Page reads (`w25q_flash.cc:298-300`, `s25fl_l_flash.cc:124-126`) byte-swapped against the 8-bit config reads, so FAT is unrecognized and the disk is formatted | 32-bit read = `b0 \| b1<<8 \| b2<<16 \| b3<<24`, where b0 is clocked first; writes send bits 7:0 first |
| H10 | `w25q_flash.cc:236-249` → `rmii_interface.cc:119-128`, `product.cc:185-190,224-235` | `4B` unimplemented (returns FF or 00) | No hang. MAC becomes `02:15:41:00:00:00`, and the hostname/unique ID are identical on every emulator instance. | `4B`, then 4 dummy bytes (`DATA_32=0`), then 8 stable UID bytes (configurable) |

## Interrupts

None. The SPI flash controller has no interrupt output. The flash instance leaves the `busy` port unconnected in the U2+ top (`ultimate_logic_32.vhd:1116-1133`). All access is synchronous: the IO ack is withheld while a byte shifts (`spi_peripheral_io.vhd:86-89,114-147`). There is no ITU IRQ bit or high-IRQ number. The emulator can finish each byte transfer inside the bus access.

## Functional model

### Controller

- `ctrl = {force, level}`, reset `{0,1}`.
- `cs_low = force ? !level : during_transfer`.
- On a DATA access (write: v; read: 0xFF): if `force && level`, return 0xFF and do not touch the chip. Otherwise:
  - if `!force`, begin a frame;
  - `miso = chip.xfer(v)`;
  - if `!force`, end the frame;
  - reads return `miso`.
- A 32-bit DATA access is 4 such bytes, LSB first.
- A CTRL write that changes effective CS from low to high ends the open frame. A change from high to low begins a new frame.
- RATE reads 0x01; CTRL reads `0x04|level<<1|force`; CRC reads 0x00.

### Chip (S25FL128L personality; add W25Q128 as an option)

- Backing store: 16 MiB, erased = 0xFF, persisted to a host file.
- Frame parser: the first byte is the opcode.

| Opcode | Frame | Effect |
|---|---|---|
| `9F` | out: `01 60 18` (W25Q: `EF 40 18`), then FF | JEDEC ID |
| `4B` | 4 dummy in, then 8 UID bytes out | unique ID |
| `03` | 3 addr bytes, then data out, auto-increment, wrap at 16 MiB | read |
| `13` | 4 addr bytes, then data out | read (4-byte) |
| `05` | SR1 out, repeated for every further byte | status. SR1 = `SRP0 SEC TB BP2 BP1 BP0 WEL BUSY` (`w25q_flash.cc:396-397`) |
| `06` | single byte | WEL=1 |
| `04` | single byte | WEL=0 |
| `12` (W25Q: `02`) | 4 (3) addr bytes, then data in | if WEL: `mem[a++] &= byte` at frame end (or as received), within one 256-byte page. Clear WEL. BUSY=0 (or BUSY for about 1 ms). |
| `21` (W25Q: `20`) | 4 (3) addr bytes | if WEL: fill `addr & ~0xFFF`, 4 KiB with 0xFF. Clear WEL. |
| `66`, `99` | single bytes | reset enable / reset: clear WEL and BUSY. A frame containing `66 99` together (`s25fl_l_flash.cc:35-38`) is a no-op. |
| `FF` | single byte | no-op |
| `65`, `01`, `31`, `11` | — | not used by this ELF; optional |

- Address mode: the firmware uses explicit 4-byte opcodes, so the chip stays in 3-byte default mode. Do not switch modes.
- Timing: the firmware accepts BUSY=0 immediately. Real limits it tolerates: page program ≤15 ms, 4 KiB erase ≤1000 ms, WRSR ≤50 ms (`w25q_flash.cc:335,357,409`).

### Image

To be firmware-identical, provide a 16 MiB file:
- FAT (type per `get_image_addresses`, 4096-byte sectors, no MBR) at 0x400000 (50T) or 0x580000 (100T), with the factory tree listed above;
- `0xFF` in the config area 0xFE8000..0xFFFFFF (a fresh device);
- bitstream and APPL regions may be 0xFF (the ELF is loaded directly).

Alternatively start fully erased and let the firmware format it (H6).

### Reboot

`W25Q_Flash::reboot(addr)` (`w25q_flash.cc:370-388`) writes:
1. `ICAP_PULSE` x2
2. 20 bytes to `ICAP_WRITE`: `AA 99 32 61 hi lo 32 81 03 up 32 A1 00 4F 30 A1 00 0E 20 00` (Xilinx sync, WBSTAR=addr, IPROG)
3. `ICAP_PULSE` x2

The emulator should map this to a full system reset (restart the ELF). If nothing happens, the code simply returns (`:387`). Called from `MENU_C64_HARD_BOOT` (`c64_subsys.cc:450-452`).

## Emulator model tiers

**T0: boot without hang**
- SPI register decode at 0x10060200/0x10060208 with the CS rules above.
- JEDEC `01 60 18`.
- `05` returns 0x00 on every byte.
- `4B` returns a fixed 8-byte UID.
- All other reads 0xFF; program/erase ignored.
- Cross-block prerequisites: ITU_TIMER countdown (H2), free-running stable ms timer (H3), `U64_RESTORE_REG != 1` (H4), consistent FPGA type (H5).
- Result: boot completes; each boot "formats" the flash disk without effect; no `/flash` root; config uses defaults. All stores see config slot 0 as empty.

**T1: functional**
- Everything in T0, plus a full NOR model with WEL gating and AND-programming.
- 3-byte and 4-byte read/program/erase (H8), correct `DATA_32` byte order (H9), persistent host-file backing.
- Layout consistent with `getFpgaType()` (H5).
- Optional prebuilt FAT image with the factory tree.
- Configurable UID (MAC/hostname) and ICAP IPROG mapped to reset.
- Result: config saves and restores in 0xFE8000 pages and `/flash/config/page_*.bin`; ROMs, carts, palettes and html come from `/flash`; the flash dump menu and the network flash read work.

## Open questions

1. **Physical part on U64-II boards.** S25FL128L (24 config pages at 0xFE8000) or W25Q128 (16 pages at 0xFF0000)? The layout table suggests S25FL-L. A real flash dump settles it: look for page ids such as `43 34 36 55` at `0xFE8000 + n*0x1000`.
2. **U64-II top is closed.** Is the flash controller the same `spi_peripheral_io` (fixed rate, no CRC) at 0x10060200? Does the U64-II CPU bus bridge split 32-bit IO accesses LSB-first like `rvlite/bus_converter.vhd`? The firmware's use of `DATA_32` (and `bootloader_u64ii.c:184-186`) is consistent with it. Register aliasing within 0x10060200-0x100602FF and the CTRL readback tie-offs are unverified (both are unused by the firmware).
3. **FPGA type values.** Which CAPABILITIES bits 29:28 does a real U64-II 50T vs 100T report? Only `==3` / `>=3` is visible in the code (`w25q_flash.cc:83`, `bootloader_u64ii.c:176`, `u64ii_programmer.cc`). Owned by the ITU/capabilities doc.
4. **ICAP on U64-II (7-series).** The open `icap-spartan.vhd` is Spartan-3A. Offsets +4/+8 are taken from `icap.h:6-8`. What exactly triggers the reconfiguration, and should the emulator reset on the final `ICAP_PULSE`?
5. **Tester order.** `.init_array` order is derived from SRCS_CC link order. It only matters because the S25FL-L probe (with `wait_ms`) is reached whenever the chip is not Winbond.
6. **Malformed reset frame.** Real S25FL128L behaviour for the single frame `66 99` at `s25fl_l_flash.cc:35-38` is assumed to be a no-op.
7. **Second flash / selector pins.** U2+ has `FLASH_SEL`/`FLASH_SELCK` (`u2p_riscv.vhd:107-108`). No software reference exists in this build. Does the U64-II have a flash selector invisible to the app?
