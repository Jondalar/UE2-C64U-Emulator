# W4-DRIVE: drive A as a 1541 on the C64 (docs/status/drive.md, docs/specs/S14-c64-trx64.md §W4-DRIVE).
# Image: scripts/make-sd-image.sh run/drive-sd.img
#        scripts/d64tool.py build run/drive.d64 --title "UE2 DRIVE" --id U2 --print "TEST=DRIVE A LOADED OK" \
#            --print "SECOND=SECOND FILE" --filler BIG=30000
#        scripts/add-sd-files.sh run/drive-sd.img $UE2_FIRMWARE/roms/1541.bin run/drive.d64
# Run with --flash run/flash.bin (C64 ROMs installed: S14 §12 A2 setup) --sd run/drive-sd.img.
# Self-checking: the browser rows, the ROM install, the mount and the write-back below. The c64screen dumps hold
# the directory (0 "UE2 DRIVE       " U2 2A, the three files, 543 BLOCKS FREE.), then DRIVE A LOADED OK, then
# SAVING NEW and READY. After the run, NEW on the D64 in the SD image equals TEST:
#   scripts/d64tool.py sd-get run/drive-sd.img drive.d64 run/after.d64
#   cmp <(scripts/d64tool.py extract run/after.d64 NEW) <(scripts/d64tool.py extract run/after.d64 TEST)
expect "F3=HELP" 10000
# The C64 reaches READY. about 6 s after power-on (S14 §12 A2).
wait 5000
button
expect "SD      SD Card                Ready"
key right
expect "1541.bin                      BIN   16K"
expect "drive.d64                     D64  171K"
# The cursor is on 1541.bin. A blank flash has no drive ROM, so drive A is off (c1541.cc:202-204, 921-943):
# "Set as 1541 ROM" copies it to /flash/roms and effectuates the settings, which powers the drive on
# (filetype_bin.cc:111-120, 207-215).
key return
expect "Set as 1541 ROM"
key down
wait 300
key return
expect-console "Copying 1541.bin to /flash/roms"
expect-console "Writing config store 'Drive A Settings' to flash"
# drive.d64 is two rows down; Mount Disk is the first item of its menu (filetype_d64.cc:67-78). "Leave Menu on
# Mount" closes the menu (c1541.cc:1041-1043).
key down
wait 300
key down
wait 300
key return
expect "Mount Disk Read Only"
key return
expect-console "Tracks: 35. Errors: No"
expect-console "MENU HIDE / EXIT."
wait 2000
type load"$",8
key return
wait 6000
type list
key return
wait 1500
c64screen
type load"test",8
key return
wait 6000
type run
key return
wait 1000
c64screen
# SAVE writes the file on track 19 and the directory and BAM on track 18; the firmware's drive task decodes the
# dirty tracks back into drive.d64 (c1541.cc:779-818).
type save"new",8
key return
expect-console "Writing back binary track 19..." 30000
expect-console "Writing back binary track 18..." 10000
wait 1000
c64screen
quit
