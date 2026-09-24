# S14 A2: the C64 boots to BASIC READY, first without and then behind the overlay menu.
# Run with --flash run/flash.bin (docs/specs/S14-c64-trx64.md §12).
# Expected: the c64screen dump holds "**** COMMODORE 64 BASIC V2 ****", "64K RAM SYSTEM  38911 BASIC BYTES FREE" and
# "READY."; run/c64-ready.png is 640x480 (border 6D6AEF at 4,4, background 2C29B1 at 340,220); the screen dump after
# the button is the M3 menu; run/c64-overlay.png shows the menu over the C64 picture.
wait 6000
c64screen
png run/c64-ready.png
button
wait 800
screen
png run/c64-overlay.png
quit
