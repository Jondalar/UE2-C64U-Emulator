# S38 §4.1: going back across REU transfers puts the REU's bytes back with the C64's (docs/specs/S38-trx64-ring.md).
# A BASIC loop keeps its counter in the REU: fetch REU $000000 into $0400, add one, stash it back ($DF01 = $B1 and
# $B0, autoload on, one byte). After `goto` a mark the C64 runs on from the mark's count only if the REU went back
# with it; with the live REU its first fetch would bring the later count. smoke-c64-all.sh compares the `m` lines:
# the live count before `goto`, the mark's count, and the count after 100 ms of running on.
# Run:   --flash <copy of run/flash.bin> --c64-roms --settings scripts/smoke-c64-ring-reu.cfg
wait 6000
type 10 poke57090,0:poke57091,4:poke57092,0:poke57093,0:poke57094,0
key return
type 15 poke57095,1:poke57096,0
key return
type 20 poke57089,177:poke1024,(peek(1024)+1)and255:poke57089,176:goto20
key return
type run
key return
wait 1000
monitor mark here
wait 2000
monitor m ram 0400 0400
monitor goto here
monitor m ram 0400 0400
monitor run
wait 100
monitor m ram 0400 0400
monitor rewind
quit
