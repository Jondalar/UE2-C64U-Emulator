# S14 A3: host keys reach CIA1. Run with --flash run/flash.bin holding the ROMs (docs/specs/S14-c64-trx64.md §12).
# Expected: the c64screen dump holds a line " 42".
# `type` follows the firmware keymap, where an upper-case letter is SHIFT + key (keyboard_c64.cc:49-58); the C64
# prints unshifted letters as capitals, so the BASIC text is typed in lower case.
wait 6000
type print 6*7
key return
wait 500
c64screen
quit
