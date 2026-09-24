# S35: REU preload and "Save REU Memory", end to end through the firmware.
# Run:   --flash run/flash-reu.bin (C64 ROMs installed) --c64-roms --settings scripts/smoke-reu.cfg
#        --usb-dir <share> holding preload.reu: 128 KB, byte a = (a + (a >> 8) * 7 + (a >> 16) * 13) & $FF
# Pass:  exit 0, "REU PASS" on the C64 screen, and after quit <share>/memory.reu equals preload.reu with bytes
#        $100-$103 = 1, 2, 3, 4 (scripts/smoke-c64-all.sh checks the file).
# The preloader loads the image when USB0 appears (reu_preloader.cc:52-79, 81-117). BASIC then fetches 8 bytes from
# REU $010203 to $C000 by DMA ($DF01 = $91) and checks them against the formula, and stashes 1-4 from $C100 to REU
# $000100 ($DF01 = $90). "Save REU Memory" (c64_subsys.cc:104, 281-327) writes the REU's configured size to a file in
# the browser's directory.
expect-console "REU Load: Loaded 131072 bytes" 30000
wait 3000
type 10 poke57090,0:poke57091,192:poke57092,3:poke57093,2:poke57094,1
key return
type 20 poke57095,8:poke57096,0:poke57089,145:f=0
key return
type 30 fori=0to7:a=66051+i:x=a+int(a/256)*7+int(a/65536)*13
key return
type 40 e=x-int(x/256)*256:p=peek(49152+i):printp;:ifp<>ethenf=1
key return
type 50 next:print:fori=1to4:poke49407+i,i:next
key return
type 60 poke57090,0:poke57091,193:poke57092,0:poke57093,1:poke57094,0
key return
type 70 poke57095,4:poke57096,0:poke57089,144
key return
type 80 iff=0thenprint"reu ""pass":end
key return
type 90 print"reu ""fail"
key return
type run
key return
expect-c64 "REU PASS" 10000
c64screen
# F5 in /USB0/ -> C64 Machine (third) -> Save REU Memory (seventh) -> RETURN on the default name "memory"
# (c64_subsys.cc:202, 285-287).
button
expect "F3=HELP" 5000
type usb
key right
expect "preload.reu" 5000
key f5
wait 500
key down
key down
key return
wait 500
key down
key down
key down
key down
key down
key down
key return
expect "Save REU memory as" 3000
key return
expect-console "written: 131072" 30000
expect "Bytes saved: 131072" 5000
screen
key return
wait 1000
quit
