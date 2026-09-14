# M4 smoke test (SD): the SD card image shows up in the file browser.
# Image: scripts/make-sd-image.sh run/sd.img; run with --sd run/sd.img --flash run/flash.bin.
# Self-checking: root lists "SD      SD Card                Ready"; /SD/ lists demo.d64, hello.prg, readme.txt;
# /SD/demo.d64/ lists the D64 file HELLO.
expect "F3=HELP" 10000
button
expect "SD      SD Card                Ready"
screen
# Cursor is on SD: enter the card.
key right
expect-console "3 children fetched from SD."
expect "demo.d64                      D64  171K"
expect "hello.prg                     PRG   29"
expect "readme.txt                    TXT   71"
expect "/SD/ "
screen
png run/sd.png
# Cursor is on demo.d64: enter the disk image.
key right
expect-console "2 children fetched from demo.d64."
expect "UE2EMU DEMO       UE 2A       VOLUME"
expect "HELLO                         PRG  254"
expect "/SD/demo.d64/"
screen
quit
