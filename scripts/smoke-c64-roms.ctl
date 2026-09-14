# S14 A2 setup: install the C64 system ROMs into /flash/roms the way the firmware's own welcome screen says
# (default_kernal.tas:195-204): browse to each ROM file and choose "Set as ... ROM" (filetype_bin.cc:78-95, 148-195).
# Blank U64-II flash holds no C64 ROMs (flash_disk_prep.cc:93-95), so the firmware uploads its placeholder KERNAL.
# Image: scripts/make-sd-image.sh run/roms.img, then
#   scripts/add-sd-files.sh run/roms.img $UE2_FIRMWARE/roms/{kernal.901227-03.bin,basic.901226-01.bin,characters.901225-01.bin}
# Run with --flash run/flash.bin --sd run/roms.img on a fresh flash (docs/specs/S14-c64-trx64.md §12).
# Expected: the first c64screen dump is the welcome screen ("Welcome to the Ultimate-64 Elite-II!"); the console has
# three "Copying ... to /flash/roms" and three "Writing config store 'C64 and Cartridge Settings'" lines; a later boot
# on the same flash reaches READY.
wait 6000
c64screen
png run/c64-welcome.png
button
wait 800
# /SD/ lists basic.901226-01.bin, characters.901225-01.bin, demo.d64, hello.prg, kernal.901227-03.bin, readme.txt.
key right
wait 1000
# basic: the 8 K menu is Set as Kernal ROM, Set as Basic ROM, Load Kernal, ...
key return
wait 500
key down
wait 300
key return
wait 3000
# characters: the 4 K menu starts with Set as Char ROM.
key down
wait 300
key return
wait 500
key return
wait 3000
# kernal.
key down
wait 300
key down
wait 300
key down
wait 300
key return
wait 500
key return
wait 3000
screen
quit
