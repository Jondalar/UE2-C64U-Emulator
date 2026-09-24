# S35: GeoRAM started by the firmware from the REU setting "GeoRAM Mode" (c64.cc:1256-1259, cart type $1F).
# Run:   --flash run/flash-georam.bin (C64 ROMs installed) --c64-roms --settings scripts/smoke-georam.cfg
# Pass:  exit 0 and "GEORAM PASS" on the C64 screen (printed from two strings, so the listing never matches).
# BASIC writes a byte to block 0 page 0 and two to block 5 page 63 through the $DE00 window, with the page in $DFFE
# (57342) and the block in $DFFF (57343), reads all three back (A, B, C), then reads block 37 page 63 (D): at 512 KB
# the block is masked to 5 bits, so block 37 is block 5 (all_carts_v5.vhd:144-152, 639-646, 738-741, 770).
expect-console "Name: GeoRAM Cartridge" 15000
wait 4000
type 10 poke57343,0:poke57342,0:poke56832,11:poke57343,5:poke57342,63
key return
type 20 poke56832,22:poke56833,33:poke57343,0:poke57342,0:a=peek(56832)
key return
type 30 poke57343,5:poke57342,63:b=peek(56832):c=peek(56833)
key return
type 40 poke57343,37:d=peek(56832):printa;b;c;d
key return
type 50 ifa=11andb=22andc=33andd=22thenprint"georam ""pass":end
key return
type 60 print"georam ""fail"
key return
type run
key return
expect-c64 "GEORAM PASS" 5000
c64screen
quit
