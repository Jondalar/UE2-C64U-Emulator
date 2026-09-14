# S14 A4: run a PRG from the SD image through the boot cartridge (DMA load).
# Image: scripts/make-sd-image.sh run/sd.img; run with --flash run/flash.bin --sd run/sd.img
# (docs/specs/S14-c64-trx64.md §12).
# Expected: the console has "DMA load complete: $0801-" and "Cart got disabled, now restoring.", not
# "Error.. cart did not get disabled."; the c64screen dump holds "HELLO FROM UE2EMU" followed by "READY.".
wait 6000
button
wait 800
# Cursor is on SD: enter the card; demo.d64 is selected, hello.prg is next.
key right
wait 1000
key down
wait 300
# RETURN opens the context menu of hello.prg with Run first (tree_browser.cc:118-128, filetype_prg.cc:70).
key return
wait 500
screen
key return
wait 5000
c64screen
png run/c64-prg.png
quit
