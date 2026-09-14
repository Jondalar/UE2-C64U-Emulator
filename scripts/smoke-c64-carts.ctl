# W4-CART: run every test cartridge of scripts/make-test-crts.py from the file browser, then the SID and MUS
# players (docs/status/carts.md).
# Image: scripts/make-sd-image.sh run/carts.img; scripts/make-test-crts.py run/carts;
#        scripts/add-sd-files.sh run/carts.img run/carts/*
# Run:   --flash run/flash.bin (C64 ROMs installed, docs/status/c64.md A2) --sd run/carts.img --usb-keyboard
# Pass:  exit 0; the c64screen dump after each cartridge c01-c27 ends with "<NAME> PASS", 27 in all; the dump after
#        F11 has "ACTION REPLAY FROZEN"; the
#        console has "Loading SID..", "Bytes loaded" twice and no "Time out!"; the SID dumps show the player screen.
# The browser lists the CRTs first, in file-name order, then demo.d64, hello.prg, readme.txt, s01-tune.sid,
# s02-tune.mus. The overlay menu stays open on the file it last ran, so each cart is one `key down` further.
wait 6000
button
wait 2000
key right
wait 1000
# c01-normal-8k.crt ("NORMAL 8K PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c02-normal-16k.crt ("NORMAL 16K PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c03-ultimax.crt ("ULTIMAX PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c04-ocean.crt ("OCEAN PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c05-magic-desk.crt ("MAGIC DESK PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c06-easyflash.crt ("EASYFLASH PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c07-gmod2.crt ("GMOD2 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c08-action-replay.crt ("ACTION REPLAY PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c09-retro-replay.crt ("RETRO REPLAY PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c10-final-cartridge-3.crt ("FINAL CARTRIDGE III PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c11-super-snapshot-5.crt ("SUPER SNAPSHOT 5 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c12-kcs-power.crt ("KCS POWER PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c13-final-cartridge.crt ("FINAL CARTRIDGE PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c14-epyx-fastload.crt ("EPYX FASTLOAD PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c15-westermann.crt ("WESTERMANN PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c16-simons-basic.crt ("SIMONS BASIC PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c17-c64-game-system.crt ("C64 GAME SYSTEM PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c18-zaxxon.crt ("ZAXXON PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c19-megabyter.crt ("MEGABYTER PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c20-super-games.crt ("SUPER GAMES PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c21-comal-80.crt ("COMAL 80 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c22-pagefox.crt ("PAGEFOX PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c23-blackbox-v3.crt ("BLACKBOX V3 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c24-blackbox-v4.crt ("BLACKBOX V4 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c25-blackbox-v8.crt ("BLACKBOX V8 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c26-blackbox-v9.crt ("BLACKBOX V9 PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c27-atomic-power.crt ("ATOMIC POWER PASS"): Run Cart is the first context-menu entry (filetype_crt.cc:47).
key return
wait 500
key return
wait 3000
c64screen
key down
wait 300
# c28-ar-freeze.crt ("PRESS FREEZE", then "ACTION REPLAY FROZEN" on line 22 after F11).
key return
wait 500
key return
wait 3000
c64screen
# F11 reaches MATRIX_KEYB[10] only with the menu closed (keyboard_usb.cc:228, 419-435); the button reopens it on c28.
button
wait 2000
usbkey f11 300
wait 1500
c64screen
png run/c64-freeze-ar.png
button
wait 2000
# s01-tune.sid: past demo.d64, hello.prg and readme.txt. Play Main Tune is first (filetype_sid.cc:517).
key down
key down
key down
key down
wait 300
key return
wait 500
key return
wait 8000
c64screen
png run/c64-sid.png
wait 3000
c64screen
# s02-tune.mus
key down
wait 300
key return
wait 500
key return
wait 8000
c64screen
png run/c64-mus.png
quit
