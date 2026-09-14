# S13 smoke test (USB): a USB stick shows up in the file browser; the matrix keyboard and the USB keyboard browse it.
# Image: scripts/make-sd-image.sh run/usb.img 48; run with --usb run/usb.img --usb-keyboard --flash run/flash.bin.
# Expected (docs/status/usb.md): root lists "USB0    UE2EMU   USB Disk Imag Ready"; /USB0/ lists demo.d64,
# hello.prg, readme.txt; /USB0/demo.d64/ lists the D64 file HELLO.
wait 7000
button
wait 800
screen
# SD, Flash, Temp, USB0: move to the stick and enter it with the C64 matrix keys.
key down
key down
key down
wait 300
key right
wait 1500
screen
# Cursor is on demo.d64: enter the disk image from the USB keyboard.
usbkey right
wait 1500
screen
quit
