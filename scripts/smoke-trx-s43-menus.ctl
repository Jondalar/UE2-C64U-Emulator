# S43 S5 and S1: the release app's menus with --trx-io (docs/specs/S43-trx-resid8-drive-slot.md), in the C64U's
# Commodore interface (smoke-trx-overlay.cfg): Audio Setup > UltiSID Configuration shows reSID8 at its reset values,
# Built-in Drive A's Drive Type offers the 1541 only. scripts/smoke-trx-s43.sh checks the screens.
expect-console "[TRX] IO block present" 15000
wait 6000
rocker up
wait 2000
key down
key down
key down
key down
key down
key return
wait 1000
key down
key down
key down
key return
wait 1500
screen
key left
wait 500
key left
wait 1000
key down
key down
key down
key down
key down
key down
key down
key down
key down
key return
wait 1500
key down
key return
wait 800
screen
quit
