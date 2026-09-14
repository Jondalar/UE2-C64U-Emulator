# --usb-dir smoke test. Run it with scripts/smoke-usb-dir.sh, which prepares $SHARE and runs the "#!" lines on the host.
expect "F3=HELP" 10000
button
expect "USB0    UE2EMU   USB Disk Imag Ready" 20000
key down
key down
key down
key right
expect "hello.prg                     PRG" 5000
expect "A Long File Name For The Emul" 1000
expect "games                         DIR" 1000
expect-not ".ue2-trash" 0
screen

# Nested directories.
key right
expect "demo.d64" 5000
expect "sub                           DIR" 1000
key left
expect "hello.prg" 5000

# The firmware writes: F5 -> Create -> D64 Image -> name.
key f5
wait 500
key down
key return
wait 500
key return
expect "Give name for new disk" 3000
type newdisk
key return
expect "newdisk.d64" 15000
screen

# The firmware deletes readme.txt: quick seek, RETURN -> Delete -> Yes.
type readme
key return
wait 500
screen
key d
key return
expect "Are you sure" 3000
key y
expect-not "readme.txt" 15000
screen

usb-sync 1
#! test "$(stat -f %z "$SHARE/newdisk.d64")" -eq 174848
#! test ! -e "$SHARE/readme.txt"
#! cmp "$SCRATCH/readme.orig" "$SHARE"/.ue2-trash/*/readme.txt

# A file added on the host reaches the guest: the watcher reports it, and once the guest has been quiet the stick is
# unplugged (the firmware removes USB0), synced, rebuilt from the host and plugged back in.
#! printf '\x01\x08\x00\x00' >"$SHARE/added.prg"
expect-console "-> Disconnect done" 120000
expect-console "Installing USB0" 60000
expect "USB0    UE2EMU   USB Disk Imag Ready" 20000
type usb
key right
expect "added.prg                     PRG    4" 10000
expect "newdisk.d64                   D64  171K" 1000
expect-not "readme.txt" 0
expect-not ".ue2-trash" 0
screen

# A last firmware write, synced when the emulator quits. quit comes right after RETURN, while the firmware is still
# writing the D64: the emulator keeps running until the guest is quiet, so the last sync takes the whole file.
key f5
wait 500
key down
key return
wait 500
key return
expect "Give name for new disk" 3000
type lastdisk
key return
wait 50
quit
#! grep -q "waiting for the guest to finish writing before the last sync" "$SCRATCH/stderr.log"
#! test "$(stat -f %z "$SHARE/lastdisk.d64")" -eq 174848
#! test "$(stat -f %z "$SHARE/newdisk.d64")" -eq 174848
#! test -f "$SHARE/added.prg" && test ! -e "$SHARE/readme.txt"
#! ! ls "$WORK"/*/unsynced 2>/dev/null
#! test "$(find "$SHARE" -name '.ue2-tmp-*' | wc -l)" -eq 0
