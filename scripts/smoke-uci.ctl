# Issue #2: UCI replies to a program that aborts and sends at once, right after a DMA start at 16 MHz.
# Run with a fresh --flash, --c64-roms, --settings scripts/smoke-uci.cfg and --usb-dir on a directory that holds only
# uci-probe.prg (scripts/make-uci-probe.py). The probe does UltimateDemo2026's ABORT + GET_HWINFO 32 times and
# prints UCI OK when every reply had status 00, else UCI FAIL and the number of bad replies.
expect "F3=HELP" 10000
button
expect "USB0    UE2EMU" 3000
key down
key down
key down
key right
expect "uci-probe.prg" 3000
# RETURN opens the context menu with Run first (filetype_prg.cc:70), a second RETURN runs it through the boot cart.
key return
expect "|Run" 2000
key return
expect-c64 "UCI OK" 10000
c64screen
quit
