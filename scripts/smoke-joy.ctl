# S36: physical joysticks on both control ports reach the C64's CIA1 and the overlay menu.
# Run:   --flash run/flash-joy.bin (C64 ROMs installed) --c64-roms
# Pass:  exit 0. BASIC waits for a line on port 1 ($DC01, 56321) and then port 2 ($DC00, 56320) and prints the five
#        lines: up+fire on port 1 is 14, down+fire on port 2 is 13. Then port 2 moves the menu cursor to Temp and
#        enters it, as scripts/smoke-menu.ctl does with the keyboard.
expect-c64 READY. 10000
type 10 a=peek(56321)and31:if a=31 goto 10
key return
type 20 print "p1";a
key return
type 30 b=peek(56320)and31:if b=31 goto 30
key return
type 40 print "p2";b
key return
type run
key return
wait 500
joy-hold 1 up+fire
expect-c64 "P1 14" 2000
joy-release 1
joy-hold 2 down+fire
expect-c64 "P2 13" 2000
joy-release 2
c64screen
button
expect "Flash   Flash Disk             Ready"
joy 2 down
wait 300
joy 2 down
wait 300
joy 2 right
expect "/Temp/"
quit
