# SD card interface (SPI) and storage mounting (SD, RAM disk, RTC timer)

Firmware: GideonZ/1541ultimate @ b617777c, target `target/u64ii/riscv/ultimate`
(`-DRISCV -DU64=2 -DOS -DIOBASE=0x10000000 -DU2P_IO_BASE=0x10100000 -DCLOCK_FREQ=100000000`).
All paths below are relative to `firmware/1541ultimate/`. `software/` is left out of source paths
(e.g. `io/sd_card/sdio.cc` means `software/io/sd_card/sdio.cc`).
ELF addresses are from `target/u64ii/riscv/ultimate/result/ultimate.elf`, checked with `riscv64-elf-nm/objdump`.

Summary: SD is an **SPI-mode** card behind Gideon's 8-bit `spi_peripheral_io`, not SDIO. The firmware
bit-bangs nothing. Every read/write of the DATA register runs one full-duplex 8-bit SPI transfer. Card
detect is polled by a FreeRTOS task every 100 ms. The init sequence is CMD0, CMD8, (CMD55+ACMD41)*, CMD58.
There is no CMD2/3/7/16 and no ACMD6. Reads use CMD17 and writes use CMD24, one sector at a time. CMD9/CMD10
run only when the sector count is queried. Without a card nothing blocks boot.

---

## Sources read (files + key functions)

| File | Key content |
|---|---|
| `target/u64ii/riscv/ultimate/Makefile` | SRCS: `sd_card.cc sdio.cc sdcard_manager.cc blockdev*.cc disk.cc partition.cc diskio.cc file_device.cc file_partition.cc filesystem_root.cc filesystem_fat.cc filesystem_iso9660.cc ramdisk.cc rtc_dummy.cc time.c filemanager.cc`. `rtc.cc`, `rtc_i2c.cc`, `timezones.cc` are **not** compiled. `LFLAGS --gc-sections` |
| `target/common/environment.mk` | VPATH/include order. `io/rtc` holds `rtc.h` and `rtc_dummy.h`. The only `ffconf.h` on the path is `chan_fat/full/ffconf.h` |
| `system/iomap.h` | `SDCARD_BASE`, `RTC_TIMER_BASE`, `RTC_BASE`, `FLASH_BASE` |
| `io/sd_card/sdio.h`, `sdio.cc` | register macros, `sdio_sense/init/send_command/set_speed/read_block/write_block` |
| `io/sd_card/sd_card.h`, `sd_card.cc` | `SdCard::init/status/read/write/ioctl/Resp8b/get_drive_size` |
| `io/sd_card/sdcard_manager.cc/.h` | global `sd_card_manager`, `poll_sdcard` task, detect state machine |
| `filesystem/blockdev.h/.cc` | `t_device_state`, BlockDevice base |
| `filesystem/disk.cc`, `partition.cc`, `diskio.cc`, `chanfat_manager.h` | MBR/EBR/GPT parsing, FatFs glue |
| `filemanager/file_device.cc`, `filemanager.h/.cc`, `filesystem/filesystem_root.cc` | root tree, lazy probe/mount |
| `filesystem/filesystem_fat.cc` | FAT test/mount/format, weak `get_fattime` |
| `filesystem/ramdisk.cc`, `blockdev_ram.cc`, `target/u64ii/riscv/ultimate/linker.x` | "Temp" RAM disk |
| `io/rtc/rtc_dummy.cc/.h`, `rtc.h`, `rtc_epoch.h`, `network/sntp_time.cc`, `network/time.c` | time source |
| `userinterface/home_directory.cc`, `application/ultimate/ultimate.cc`, `components/init_function.cc`, `components/config.cc`, `filesystem/blockdev_flash.cc` | boot order, home dir wait |
| `portable/riscv/riscv_main.c`, `FreeRTOS/Source/FreeRTOSConfig.h`, `system/itu.h` | tick rate, `ENTER_SAFE_SECTION` |
| `chan_fat/ff14/source/ff.c` | `mount_volume` status checks |
| VHDL `fpga/io/spi/vhdl_source/spi_peripheral_io.vhd`, `spi.vhd` | register semantics (open IP) |
| VHDL `fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd` | U2/U2+ decode and wiring (reference only; U64-II top is closed) |
| VHDL `fpga/cpu_unit/rvlite/vhdl_source/bus_converter.vhd`, `fpga/cpu_unit/vhdl_source/wishbone2io.vhd` | 32-bit to 8-bit I/O splitting |
| VHDL `fpga/ip/clock/vhdl_source/real_time_clock.vhd`, `fpga/io/.../io_dummy.vhd` | RTC timer, absent-peripheral behaviour |

---

## Address map

`IOBASE = 0x10000000`. `SDCARD_BASE = IOBASE+0x60000` (iomap.h:23). `RTC_TIMER_BASE = IOBASE+0x60400` (iomap.h:27).
On U2+ the I/O splitter gives each 0x10060x00 slot 0x100 bytes (`io_bus_splitter g_range_lo=8, g_range_hi=11`,
ultimate_logic_32.vhd:1010-1024). The SPI block decodes only `address(3 downto 2)` (spi_peripheral_io.vhd:91,116),
so each register is 4 bytes wide (+0..+3 DATA, +4..+7 SPEED, +8..+B CTRL, +C..+F CRC) and the block of four repeats every
16 bytes across 0x10060000-0x100600FF. Offsets +1..+3 decoding as DATA is how 32-bit access works.

### SD SPI peripheral, window 0x10060000-0x100600FF

| Absolute addr | Width | R/W | Name (sdio.h) | Meaning |
|---|---|---|---|---|
| 0x10060000 | 8 | W | `SDIO_DATA` (sdio.h:7) | Start an SPI transfer: MOSI = value. The MISO byte is latched internally and lost (spi_peripheral_io.vhd:92-94). The card state still advances. |
| 0x10060000 | 8 | R | `SDIO_DATA` | Start an SPI transfer with MOSI = 0xFF and return the MISO byte (spi_peripheral_io.vhd:117-120, 142-147). **Reads have side effects.** |
| 0x10060000 | 32 | R/W | `SDIO_DATA_32` (sdio.h:8) | The bridge splits it into 4 byte accesses at +0,+1,+2,+3, little-endian: byte 1 goes to bits 7:0 (bus_converter.vhd:56,159-168,178; write data mapping 82-90). That is **4 SPI transfers**, and the first byte on the wire lands in bits 7:0. Used in the sector data loops (sdio.cc:64, 88; ELF 0x6a250 `lw 0(0x10060000)`, 0x6a2cc `sw`). |
| 0x10060004 | 8 | W | `SDIO_SPEED` (sdio.h:9) | SPI clock divider, `rate(7:0)=d, rate(8)=d(7)` (spi_peripheral_io.vhd:96-100). Firmware writes 254, 200, 1. |
| 0x10060004 | 8 | R | — | `rate(7:0)` (spi_peripheral_io.vhd:122-124). Reset value 500 reads as 0xF4. Firmware never reads it. |
| 0x10060008 | 8 | W | `SDIO_CTRL` (sdio.h:10) | bit0 `SPI_FORCE_SS` (0x01), bit1 `SPI_LEVEL_SS` (0x02) (sdio.h:14-15; spi_peripheral_io.vhd:102-104). force=1 drives SSn to the level bit. force=0 means SSn is asserted automatically during each byte (spi.vhd:54,62,100,128-130). |
| 0x10060008 | 8 | R | `SDIO_SWITCH` (sdio.h:12) | `0000 & WP & CD & level_ss & force_ss` with WP = not SD_WRPROTn and CD = not SD_DETECTn (spi_peripheral_io.vhd:127-128). Firmware computes `sense = val>>2`: bit0 = `SD_CARD_DETECT`, bit1 = `SD_CARD_PROTECT` (sdio.cc:11, sdio.h:17-18). Reset: force_ss=0, level_ss=1, so it reads 0x02 with no card (spi_peripheral_io.vhd:157-158). |
| 0x1006000C | 8 | W | `SDIO_CRC` (sdio.h:11) | Any write clears the CRC7 accumulator (spi_peripheral_io.vhd:106-107; spi.vhd:118-120). |
| 0x1006000C | 8 | R | `SDIO_CRC` | `crc7 & '1'` over all MOSI bits sent since the clear (spi.vhd:42-47,64,87,134). Firmware sends this byte as the command CRC (sdio.cc:36). |

Every register access waits for any running transfer to finish (`busy_i='0'` guard, spi_peripheral_io.vhd:87,115), so
software sees synchronous transfers. No busy/status bit is ever polled.

If the FPGA leaves out the SD block (`g_sdcard=false`), `io_dummy` answers every read with 0x00
(io_dummy.vhd:16; ultimate_logic_32.vhd:1102-1112). On U2+ the WP pin is hard-wired `SD_WRPROTn => '1'`, so the WP bit is always 0
(ultimate_logic_32.vhd:1087).

### RTC seconds timer, window 0x10060400-0x100604FF

| Absolute addr | Width | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x10060400 | 32 | R | `RTC_TIMER_SECONDS` (rtc_dummy.h:17) | Seconds counter, Unix epoch, interpreted with `localtime_r` (rtc_dummy.cc:25-31; ELF 0x4d65c `lw 1024(0x10060000)`). In VHDL: bytes 0..3 little-endian (real_time_clock.vhd:40-53). |
| 0x10060400 | 32 | W | `RTC_TIMER_SECONDS` | Set the time (rtc_dummy.cc:109, 114). In VHDL a write to byte 0 sets `lock`, which stops the ms prescaler, and a write to byte 3 clears it (real_time_clock.vhd:54-69). A 32-bit write goes out as bytes 0..3 in order, so the update is atomic. |
| 0x10060404-0x1006040F | 8 | R | — | VHDL: ack with data 0 (`others => null`, real_time_clock.vhd:51-52) |

VHDL counter: initial value `0x67829644` = 2025-01-11 16:03:16 UTC (real_time_clock.vhd:20). It counts +1 every 1000
`tick_1kHz` pulses (real_time_clock.vhd:27-37).

### RAM used as a block device

| Absolute range | Size | R/W | Name | Meaning |
|---|---|---|---|---|
| 0x02000000-0x02FFFFFF | 16 MiB | R/W | `__ram_disk_start/limit` (linker.x:280-281) | Backing store of the "Temp" RAM disk: 32768 sectors of 512 bytes, FAT-formatted at every boot (ramdisk.cc:25-36) |

Not touched by this block: `RTC_BASE` 0x10060100 (I2C RTC chip). No compiled source outside `application/`, `test/` and `io/rtc/`
references it, and `rtc_dummy.cc` does not use it.

---

## Init / boot sequence as seen from the bus

### A. Before the scheduler (static constructors)
1. `_GLOBAL__sub_I_sd_card_manager` (ELF 0x30cb8) constructs the global `SdCardManager sd_card_manager`
   (sdcard_manager.cc:11,22-26). `init()` does `new SdCard` (mutex only, sd_card.cc:43-51),
   `new FileDevice(sd_card,"SD","SD Card")` and `add_root_entry`, then creates the task
   `"SD Card Manager"` at PRIO_BACKGROUND=0 (sdcard_manager.cc:36-42; FreeRTOSConfig.h:6). **No bus access.**
   `BlockDevice` state starts as `e_device_unknown` (blockdev.cc:6).
2. `FileManager::getFileManager()` is a static local. It creates the `"RootNode"` tree node and `FileSystem_Root`
   (filemanager.h:105-112,166-173). No bus access.

### B. `ultimate_main` then `InitFunction::executeAll()` (ultimate.cc:94; sorted by ordering, init_function.cc:29-37)
3. Order 1, **RAM disk** (ramdisk.cc:16-44):
   `BlockDevice_Ram(0x02000000, 512, 32768)` sets state ready (blockdev_ram.cc:6-12).
   `Partition(blk,0,0,0)` gets its length from `ioctl(GET_SECTOR_COUNT)` = 32768 (partition.cc:18-20).
   `FileSystemFAT::format("RamDisk")` runs `f_mkfs` with `FM_SFD|FM_ANY`, `n_fat=1` and a 4096-byte work buffer
   (filesystem_fat.cc:107-120). It memcpy-writes into 0x02000000.. (blockdev_ram.cc:41-48) and calls `get_fattime()`, which reads
   0x10060400 (rtc_dummy.cc:119-123, 73-76, 25).
   Then `FileDevice("Temp")`, `attach_disk(512)` and `probe()`. Probe runs `Disk::Init` (reads sector 0 and finds the "FAT" string,
   giving 1 partition, disk.cc:75-90) and `attach_filesystem` (FAT `f_mount`, filesystem_fat.cc:235-258, 31-35).
   `add_root_entry` is called **whatever probe returns** (ramdisk.cc:38-41).
4. Flash disk mounting runs from `ConfigManager` (`init_flash_disk()`, config.cc:66; blockdev_flash.cc:148-183).
   It is skipped if `U64_RESTORE_REG` (0x10100402, u64.h:15,67) == 1. The flash side is covered in the flash doc.
5. After the init functions: `rtc.get_long_date()` and `rtc.get_time_string()` each read 0x10060400 once (ultimate.cc:97-98).
   The value only goes into printf.
6. A64 (order 30) and FTP (order 31) virtual roots are added (filesystem_a64.cc:133-135, filesystem_ftp.cc:479-483).
   They do not use SD.

### C. SD poll task (after `vTaskStartScheduler`, riscv_main.c:159-162)
Tick = 200 Hz (FreeRTOSConfig.h:21; riscv_main.c:173-186), so `vTaskDelay(20)` = 100 ms.

7. Loop: `R 0x10060008` (sdio_sense, sdcard_manager.cc:46), run the state machine, `vTaskDelay(20)` (sdcard_manager.cc:16-19).
8. State `e_device_unknown`:
   - CD=0: go to `e_device_no_media` and send `eNodeUpdated "/" "SD"` (sdcard_manager.cc:57-60). **Done. No other SD access
     happens until the CD bit rises.**
   - CD=1: `SdCard::init()`. On nonzero return the state becomes `e_device_error`. On success it becomes `e_device_ready`
     and `attach_disk(512)` is called (sdcard_manager.cc:49-56).
9. `SdCard::init()` bus trace (sd_card.cc:91-208, sdio.cc:14-42). `cmd(n,x,y)` below stands for:
   `W DATA=FF; W CRC=00; W DATA=40|n; W DATA=x>>8; W DATA=x&FF; W DATA=y>>8; W DATA=y&FF; R CRC; W DATA=<crc>`
   (sdio.cc:28-36; ELF 0x6a1c0-0x6a204). `R1` means: read DATA up to 8 times, stop at the first value != 0xFF (sd_card.cc:409-424).
   1. `W SPEED=0xFE`, `W CTRL=0x03` (CS forced high), `100 x W DATA=0xFF`, `W CTRL=0x00` (sdio.cc:16-23)
   2. `W SPEED=0xC8` (200) (sd_card.cc:106)
   3. Up to 101 times: `cmd(0,0,0)` then R1, until R1==0x01 (sd_card.cc:110-116). If R1 != 1, return `STA_NOINIT` (sd_card.cc:120-128)
   4. `cmd(8,0x0000,0x01AA)` then R1 (sd_card.cc:130-131)
      - `R1 & 0x04`: V1 card, `sd_type=1` (sd_card.cc:133-135)
      - else: `R DATA` (b1), `W DATA=FF` (b2, discarded), `R DATA` (b3), `R DATA` (b4), and b4 must be 0xAA, otherwise `STA_NOINIT` (sd_card.cc:137-147)
   5. Up to 32001 times: `cmd(55,0,0)` then R1 (ignored); `cmd(41,0x4000,0)` (arg 0x40000000, HCS) then R1, while R1==0x01.
      The final R1 must be 0x00, otherwise `STA_NOINIT` (sd_card.cc:153-170)
   6. If sd_type==2: `cmd(58,0,0)` then R1; `R DATA` = OCR[31:24]; `3 x W DATA=FF`. `OCR[31:24] & 0x40` sets `sdhc=true`
      (sd_card.cc:172-184)
   7. `W SPEED=0x01` (`RW_SPEED`=1 because CLOCK_FREQ > 50 MHz, sd_card.cc:35-39,186). `initialized=true`, return 0.
   CMD16 set-block-length and CMD13 are commented out (sd_card.cc:194-205). CMD2/3/7/ACMD6/CMD59 are never sent.
10. Nothing reads media at init. The first `FileDevice::probe()` (tree browser, FileSystem_Root walk/dir_open
    filesystem_root.cc:37,73, REST/FTP path lookup) calls `Disk::Init` (file_device.cc:56-109, disk.cc:34-164):
    - read sector 0 (disk.cc:53)
    - no 0x55AA signature: one partition of `GET_SECTOR_COUNT` sectors (disk.cc:64-70)
    - "FAT" at offset 54 or 82 (superfloppy): one partition, type 0x06/0x0C, size from `GET_SECTOR_COUNT` (disk.cc:75-90)
    - else the 4 MBR entries: type 0xEE is skipped, 0x05/0x0F are followed as an EBR chain (disk.cc:101-117,166-202). With no partitions it reads sector 1
      and tries GPT (disk.cc:120-162)
    - 1 partition: the filesystem is attached directly to the "SD" node. More than 1: children "Part0".. (file_device.cc:82-108)
    - FS factory: FAT (`f_mount`, filesystem_fat.cc:235-260) and ISO9660 (filesystem_iso9660.cc:391)
    - `GET_SECTOR_COUNT` on SD runs `get_drive_size()`, which sends CMD10 then CMD9 (sd_card.cc:371-372, 577-674)

---

## Boot hazards

| # | Where | What | Required emulator response |
|---|---|---|---|
| H1 | io/sd_card/sdcard_manager.cc:46-60, io/sd_card/sdio.cc:11 | CD bit (bit 2 of 0x10060008) decides whether init runs. If unmapped I/O reads 0xFF, the firmware sees CD=1 and WP=1. Init then fails because DATA returns 0xFF, the state sticks at `e_device_error` (sdcard_manager.cc:91-98) and the UI shows "SD Error!". No hang. | **T0:** return 0x00 (like `io_dummy`) or 0x02 (VHDL reset) for 0x10060008. **With a card:** `0x04 \| (level_ss<<1) \| force_ss`, bit 3 = 0 |
| H2 | io/sd_card/sd_card.cc:66-69, filesystem/filesystem_fat.cc:51-58, chan_fat/ff14/source/ff.c:3402,3418,5935 | WP bit gives `STA_PROTECT`: FAT reports not writable, and mkfs/write-mode mounts fail. | bit 3 of 0x10060008 = 0 unless write protection is wanted |
| H3 | io/sd_card/sd_card.cc:110-128 | CMD0 needs R1 = 0x01 within 8 bytes. With 0xFF, up to 101 retries, then `STA_NOINIT`. | R1 0x01 on the first DATA read after the CRC byte |
| H4 | io/sd_card/sd_card.cc:130-148 | CMD8: if R1 bit 2 = 0, exactly 4 more bytes are clocked and the 4th must be 0xAA. Otherwise init fails. | SDHC: `01 00 00 01 AA`. V1 emulation: R1 0x05, no trailing bytes |
| H5 | io/sd_card/sd_card.cc:153-170 | ACMD41 loops up to 32001 times while R1==0x01, which is about 64k commands if the card never leaves idle. Any other R1 != 0 fails init. | CMD55 R1 0x01 (or 0x00), ACMD41 R1 0x00 (first try is fine) |
| H6 | io/sd_card/sd_card.cc:172-184, 255, 333 | OCR[31:24] bit 6 picks the addressing. sdhc: arg = LBA. Otherwise arg = LBA<<9 (byte address, uint32, max 4 GiB). This must match the card model's decode and the CSD. | CMD58: `00` + `C0 FF 80 00` (CCS=1) and LBA addressing |
| H7 | io/sd_card/sdio.cc:48-57, io/sd_card/sd_card.cc:257 | Read data token: up to 240000 DATA reads with **interrupts disabled** (`ENTER_SAFE_SECTION`=`portENTER_CRITICAL`, system/itu.h:86). A non-0xFE token becomes an error. No card data means 240k bus ops per sector with the tick masked. | The 0xFE token on the read right after R1, then 512 data bytes, then 2 CRC bytes |
| H8 | io/sd_card/sdio.cc:95-113, io/sd_card/sd_card.cc:334-346 | Write busy wait: after 3 dummy writes, up to 600000 DATA reads (IRQs off) until 0xFF, otherwise `RES_ERROR`. The data-response token is never checked. | 0xFF (not busy) by the 1st read after the 3 trailing 0xFF writes |
| H9 | io/sd_card/sd_card.cc:590-645 | CID/CSD: up to 200 `Resp8b` calls (1600 bytes max) looking for 0xFE. CSD missing gives `RES_ERROR` for `GET_SECTOR_COUNT`, so superfloppy/unpartitioned media fail (`Disk::Init` returns -3/-4, disk.cc:65-69,76-81). | CMD9/CMD10: R1 00, FE, 16 bytes, 2 CRC. CSD[0] bits 7:6 = 01 (v2) |
| H10 | io/sd_card/sd_card.cc:581, 663-670 | `c_size` is `uint16_t`, so CSD v2 C_SIZE bits 21:16 (iob[7]&0x3F) are lost. Media > 32 GiB report a wrong sector count. That breaks unpartitioned media (partition length) and `Partition::read` bounds (filesystem/partition.cc:58). MBR partition lengths are unaffected. | Use images <= 32 GiB (C_SIZE <= 0xFFFF), or partition them with an MBR |
| H11 | io/sd_card/sdcard_manager.cc:63-80, 91-98 | After a failed init, error is sticky until CD reads 0. Insert: CD 0 to 1, then a 250 ms delay (`vTaskDelay(50)`), then init. Removal is seen at the next 100 ms poll. | Hot-plug: hold CD=0 for at least one poll (>= 100 ms emulated) before reasserting it |
| H12 | io/rtc/rtc_dummy.cc:25-39, 73-86 | RTC value has no control-flow effect. 0 means 1970: `y = -10`, garbage FAT timestamps (`y<<25`), UI shows 1970. No crash. | Return host UTC epoch seconds, advancing 1/s. Accept 32-bit writes |
| H13 | filesystem/ramdisk.cc:25-41, target/u64ii/riscv/ultimate/linker.x:280-281, filesystem/blockdev_ram.cc:37,46 | `f_mkfs` and memcpy write to 0x02000000-0x02FFFFFF. This lies outside the linker `memory` region (0x30000-0xE8FFFF, linker.x:7). If it is not RAM, the emulator bus faults during init (crash), or "Temp" is broken if writes are silently dropped. | Map 0x02000000-0x02FFFFFF as plain R/W RAM (linker.x:268-287 places further RAM users up to 0x03FFFFFF; see the memory doc) |
| H14 | filesystem/blockdev_flash.cc:160-163, components/config.cc:66 | `U64_RESTORE_REG` (0x10100402) == 1 skips mounting the "Flash" disk, which is where config files live (config.cc:67). | Return != 1 (see flash/U64 control doc) |
| H15 | network/time.c:3-7 | `time()` writes `*t` unconditionally, so `time(NULL)` would write to address 0. **Not a hazard in this build:** no `time` symbol is in the ELF (`--gc-sections`), and `httpd/c-version/lib/server.c:37-38` uses `sys_now()` because `LWIP==1` (server.h:15-18). | none |

With **no card** (CD=0) or the SD block absent (all reads 0x00), the only SD bus activity is one byte read of
0x10060008 every 100 ms. Boot, UI and filesystem root work; the "SD" node shows "No media" (file_device.cc:113-121).

---

## Interrupts

- The SD SPI peripheral has **no interrupt**. The `busy` output only feeds an activity LED stretcher on U2+ (ultimate_logic_32.vhd:1084,1094-1099).
  No ITU IRQ bit or high-IRQ number is used (riscv_main.c:86-129 does not dispatch SD).
- The RTC timer has no interrupt (real_time_clock.vhd port list :9-14).
- Relevant for timing: each sector read/write and each CID/CSD read run inside `portENTER_CRITICAL()`
  (sd_card.cc:257-276, 334-346, 590-604, 616-633; itu.h:86-87). The emulator cannot count on the FreeRTOS tick advancing inside the
  240000/600000-iteration polling loops, so card responses must not depend on time passing.

---

## Functional model (enough to back SD with a raw .img)

### Peripheral (per spi_peripheral_io.vhd / spi.vhd)
```
state: rate(9b)=500, force_ss=0, level_ss=1, crc7=0
cs_active = !(force_ss && level_ss)     // force_ss=1 && level_ss=1 means the card is deselected
write @+0x0 (also +1..+3): miso = card.xfer(val, cs_active); crc7 = crc7_update(crc7, val)   // miso dropped
read  @+0x0 (also +1..+3): miso = card.xfer(0xFF, cs_active); crc7 = crc7_update(crc7, 0xFF); return miso
write @+0x4..+0x7: rate = {d7, d}    read @+0x4..+0x7: rate & 0xFF
write @+0x8..+0xB: force_ss=d0, level_ss=d1
read  @+0x8..+0xB: (wp<<3)|(cd<<2)|(level_ss<<1)|force_ss
write @+0xC..+0xF: crc7 = 0          read @+0xC..+0xF: (crc7<<1)|1
all offsets modulo 0x10 (only address(3 downto 2) is decoded, spi_peripheral_io.vhd:91,116)
32-bit access at +0: 4 byte accesses at +0,+1,+2,+3 (LE): read value = b0 | b1<<8 | b2<<16 | b3<<24, b0 clocked first;
                     write sends bits 7:0 first.
crc7_update: for each bit MSB first: c6 = crc>>6; crc = ((crc<<1)&0x7F) ^ (din^c6 ? 0x09 : 0)   (poly x^7+x^3+1, spi.vhd:42-47)
```
CMD0 comes out as `40 00 00 00 00 95` and CMD8 `48 00 00 01 AA 87`. The card model may ignore CRC.
If `cs_active` is false, the card returns 0xFF and ignores MOSI (the 100 x 0xFF preamble, sdio.cc:17-21).

### Card (SPI mode), byte-stream state machine
Each `xfer(mosi)` returns the next queued output byte (0xFF when the queue is empty) and consumes `mosi`.
```
RX_CMD   : if (mosi & 0xC0) == 0x40 : clear the output queue, start collecting 6 bytes
           (clearing covers the unconsumed CID CRC: sd_card.cc:600-604 sends no trailing 0xFF writes)
           after 6 bytes: exec(cmd = b0&0x3F, arg = b1..b4 big-endian)
exec:
  CMD0  : idle=1, app=0            -> queue 01
  CMD8  :                          -> queue 01 00 00 01 <arg&0xFF>        (R7; 4th trailing byte = 0xAA)
  CMD55 : app=1                    -> queue (idle?01:00)
  ACMD41: idle=0 (when app)        -> queue 00
  CMD58 :                          -> queue 00 C0 FF 80 00                 (OCR, CCS=1 => sdhc)
  CMD9  :                          -> queue 00 FE <CSD 16 bytes> <crc16 2 bytes>
  CMD10 :                          -> queue 00 FE <CID 16 bytes> <crc16 2 bytes>
           (CSD and CID byte 15 = CRC7 over bytes 0-14, shifted left, bit0 = 1)
  CMD17 : lba = arg (sdhc)         -> if lba >= sectors: queue 40 ; else queue 00 FE <512 bytes of img[lba*512]> <2 CRC bytes>
  CMD24 : lba = arg                -> if lba >= sectors: queue 40 ; else queue 00, state = WAIT_TOKEN
  CMD13 : queue 00 00 ; CMD16/CMD59: queue 00 ; any other: queue 04 (illegal)
WAIT_TOKEN : ignore 0xFF; 0xFE -> RX_DATA(n=0)            (data bytes 0x40..0x7F must NOT start a command here)
RX_DATA    : store 512 bytes, then 2 CRC bytes -> write img[lba*512], queue 05 (data accepted) [, 00 busy...], 0xFF ; state RX_CMD
```
How the firmware lines up with the queue:
- After a command, the first DATA read returns R1 (`Resp8b` tolerates up to 7 leading 0xFF, sd_card.cc:416-423).
- CMD17: the next read after R1 must be 0xFE (sdio.cc:51-57). Then 512 bytes, as 128 x `lw` when the buffer is word-aligned or 512 x `lbu` otherwise
  (sdio.cc:62-70). Then 2 x `W FF` for the CRC (sdio.cc:73-74). Multi-sector reads are repeated CMD17 (sd_card.cc:254-280). No CMD18.
- CMD24: R1 is read but only printed (sd_card.cc:342-343). Then `W FE`, 512 bytes (128 x `sw` or 512 x `sb`), `W FF` x 3
  (2 CRC bytes plus the byte that carries the data-response token), then reads until 0xFF (sdio.cc:85-113). No CMD25.
- CMD8 V2 path clocks exactly 4 bytes after R1 (read, write FF, read, read; sd_card.cc:138-144).
- CMD58 clocks exactly 4 bytes after R1 (read + 3 x write FF; sd_card.cc:176-179). Only OCR[31:24] is used.
- CMD9: 2 trailing `W FF` consume the CRC (sd_card.cc:630-631). CMD10 has none, so the next command's leading `W FF` plus the command byte flush them.

### CSD for an image of `S` sectors (SDHC)
The firmware uses only CSD[0] bits 7:6 and bytes 7..9 (sd_card.cc:647-670):
- `c_size = S/1024 - 1`, which must be <= 0xFFFF (H10). Reported count = `(c_size+1) << 10`, so the image should be a multiple of 512 KiB.
- Suggested bytes: `40 0E 00 32 5B 59 00 (c_size>>16)&3F (c_size>>8)&FF c_size&FF 7F 80 0A 40 00 <crc>`. As in every CSD and CID register, the last byte is
  `CRC7(bytes 0-14) << 1 | 1` (SD Physical Layer spec §5.2-5.3; emulator `with_crc7`, crates/ue2-core/src/devices/sdcard.rs:72-76).
- CID contents are only printed (sd_card.cc:606-613).

For an SDSC/V1 alternative, the firmware would need: CMD8 R1 0x05, no CMD58, byte addressing `arg>>9`, and a CSD v1 with
`sectors = (C_SIZE+1) << (C_SIZE_MULT+2+READ_BL_LEN-9)` (sd_card.cc:647-661). Not recommended; use SDHC.

### Filesystem side (for choosing image layout)
- FatFs config (chan_fat/full/ffconf.h): exFAT on (:233), LFN=2 (:102), FF_MAX_SS 4096 (:196), FF_LBA64 0 (:205),
  FF_MULTI_PARTITION 0 (:186), FF_VOLUMES 10 (:168), FF_FS_REENTRANT 1 (:278). FAT12/16/32/exFAT with 32-bit LBA work.
- Recommended image: MBR with one FAT32 or exFAT partition, or a superfloppy FAT, <= 32 GiB.
  GPT is supported for "Microsoft basic data" entries with 32-bit LBAs (disk.cc:123-161).
- Mount is lazy, on first `probe()` (file_device.cc:56-95). A detect removal runs `eNodeMediaRemoved`, `detach_disk`, `invalidate`
  (sdcard_manager.cc:82-88).
- `SdCard::status()` returns `STA_NOINIT` until init succeeds, plus `STA_NODISK`/`STA_PROTECT` from the sense bits (sd_card.cc:62-76). FatFs mount
  fails on `STA_NOINIT` (ff.c:3414-3416).

### Home directory
Only created when `CFG_USERIF_START_HOME` is set (default 0, userinterface.cc:129; ultimate.cc:142-145,157-160). It then runs in its own task and
waits at most 10 s for `eNodeAdded/eNodeUpdated` of the first path element, e.g. "SD" (home_directory.cc:51-88). It never blocks boot.
The "SD" device raises `eNodeUpdated` on every init outcome (sdcard_manager.cc:56,59,66,74,78).

### Time source (U64-II build)
- Compiled RTC = `rtc_dummy.cc` (Makefile SRCS_CC). Callers include `rtc.h` (e.g. ultimate.cc:20, sntp_time.cc:6), whose class
  declaration differs (rtc.h:36-61). Only methods that exist in `rtc_dummy.cc` are linked (ELF: `Rtc::get_time`,
  `get_fat_time`, `set_time`, `set_time_utc`, `get_time_string`, `get_long_date`, `get_correction`, `set_time_in_chip`).
  No RTC `InitFunction` is linked (rtc.cc:427 / rtc_i2c.cc:408 are not compiled).
- Readers: `get_time()` (FatFs timestamps via `get_fattime`, ff.c:272, rtc_dummy.cc:119-123; UI date/time; D64 fs
  filesystem_d64.cc:1454; ftpd.cc:363; cbmdos `get_current_time` rtc_dummy.cc:125-129).
- Writers in compiled sources: `sntp_time_received()` calls `set_time_utc(sec)` (sntp_time.cc:11-15; lwipopts.h:1451), and the
  DOS command interface `set_time()` (dos.cc:561; `mktime` in local TZ, rtc_dummy.cc:98-110).
- TZ is set only in `start_sntp()` when NTP is enabled (sntp_time.cc:21-27). Otherwise `localtime_r` runs with no TZ (UTC).

---

## Emulator model tiers

**T0: boot without hang**
- 0x10060000-0x100600FF: reads return 0x00 (matches `io_dummy`); writes are accepted and ignored. At most, reflect CTRL
  bits in the 0x10060008 readback, with the CD bit = 0.
- 0x10060400: 32-bit read returns host UTC seconds; writes are accepted and stored as an offset.
- 0x02000000-0x02FFFFFF: plain RAM (the "Temp" RAM disk formats and mounts itself).
- Result: "SD" root node shows "No media", "Temp" is usable, and there is one SD sense read per 100 ms.

**T1: functional**
- Full `spi_peripheral_io` register model: DATA transfers with read side effects, 32-bit reads/writes as 4 LE transfers, SPEED
  readback, CTRL bits in and out, CRC7 register.
- SPI-mode SD card model backed by a raw `.img`, as above: CMD0/8/55/41/58/9/10/17/24, SDHC addressing, CSD v2 sized from the
  image (<= 32 GiB), data-response 0x05 with no busy phase, persistent writes (flush on `CTRL_SYNC` is not signalled to the card;
  write through immediately).
- Card detect and write protect as host-controlled inputs (insert/eject/WP), respecting H11 timing.
- RTC timer: 32-bit counter with VHDL byte-0 lock / byte-3 unlock semantics, counting from the emulated 1 kHz tick (or host clock).
  SNTP writes land here.
- Optional: activity indication from transfer count (U2+ drives an LED from `busy`, ultimate_logic_32.vhd:1094-1099).

---

## Open questions

1. **Does the closed U64-II top instantiate `spi_peripheral_io` at 0x10060000 (`g_sdcard`), or `io_dummy`? Does U64-II / C64U have a
   physical SD slot at all?** The firmware registers "SD" unconditionally (sdcard_manager.cc:11,36-42), and there is no SD capability bit
   (itu.h:50-79).
2. U64-II polarity and wiring of card-detect and write-protect. The U2+ reference ties WP inactive (ultimate_logic_32.vhd:1087).
   The sdio.cc:11 comment "invert, should be changed in HW" hints that polarity once differed.
3. The U64-II CPU-to-I/O bridge behaviour for 32-bit accesses to 0x10060000 is assumed to be 4 sequential LE byte accesses, like
   `bus_converter.vhd`/`wishbone2io.vhd`. The U64-II CPU wrapper is not in the open tree.
4. Is `real_time_clock` (0x10060400) present on U64-II, what is its initial/reset value and 1 kHz tick source, and is it
   battery-backed? Only SNTP and the DOS command write it in compiled code.
5. Whether `sntp_time_received` is linked in this ELF: `start_sntp` is present but the `sntp_time_received` symbol did not
   show in `nm`, possibly because it lives or was stripped inside `liblwip.a` linkage. Not checked further.
6. `SDIO_SPEED` values 254/200/1 are U2+ divider semantics (half-bit = rate+1 system clocks, spi.vhd:56,73-94). The actual SPI
   frequency on U64-II is irrelevant for emulation but unconfirmed.
