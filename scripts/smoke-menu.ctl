# M3 smoke test: open the overlay menu, move the selection, capture screen + PNG.
# Run with --flash run/flash.bin (S06 seeds the overlay UI). Self-checking: a failed expect stops the script
# with its line number and the screen, and ue2emu exits non-zero (scripts/smoke-all.sh runs every smoke test).
# The menu button is taken as soon as the browser has drawn its help line.
expect "F3=HELP" 10000
button
expect "Flash   Flash Disk             Ready"
screen
png run/menu1.png
key down
wait 300
key down
wait 300
screen
png run/menu2.png
# The selection is colour-only in the text dump (docs/hw/05 T1); entering the selected entry proves where it is.
key right
expect "/Temp/"
quit
