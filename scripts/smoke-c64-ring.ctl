# S38: TRX64's rings on the C64 behind the running firmware (docs/specs/S38-trx64-ring.md §7). A BASIC loop counts
# the top-left character; `goto` a mark puts the C64 back on it, twice to the same byte, while the firmware runs on.
# Then the ring goes to a file and back, and `asm-file` assembles ring.asm from the monitor directory (the run
# directory; smoke-c64-all.sh writes the file). smoke-c64-all.sh compares the `m` lines in the log.
wait 6000
type 10 poke1024,(peek(1024)+1)and255:goto10
key return
type run
key return
wait 1000
monitor mark here
monitor rewind
wait 1000
monitor m ram 0400 0400
monitor goto here
monitor m ram 0400 0400
wait 500
monitor m ram 0400 0400
monitor run
wait 1000
monitor goto here
monitor m ram 0400 0400
monitor marks
monitor whowrote 0400 1
monitor ringdump ring.c64rering
monitor ringload ring.c64rering
monitor run
monitor asm-file ring.asm
monitor m c000 c002
quit
