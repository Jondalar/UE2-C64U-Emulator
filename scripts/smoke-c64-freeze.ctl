# S14 A5: the Freeze UI on the C64 screen (CFG_USERIF_ITYPE = 0, the default).
# Run with --no-overlay-ui --flash run/flash-freeze.bin on a fresh file (docs/specs/S14-c64-trx64.md §12).
# Expected: the second c64screen dump shows the menu (SD Card, Flash Disk, RAM Disk); the console has
# "Frozen on Bad line." and no "Hard stop!!!"; run/c64-freeze.png shows the menu; the third dump equals the first.
wait 6000
c64screen
button
wait 1500
c64screen
png run/c64-freeze.png
key down
wait 300
button
wait 1500
c64screen
quit
