# S38 §7: a `.c64rering` the TRX64-Ultimate board wrote (smoke-c64-all.sh unpacks the c64-bridge fixture into the
# run directory as board.c64rering). `ringload` stands the C64 on the board's current anchor (a PAL C64 at READY)
# with notes for what the file lacks; `goto` reaches an older one; `ringdump ... board` writes the board's format
# back. After `run` the C64 goes on from the board's state: a typed line runs, so the keyboard scan (CIA 1's timer
# interrupt, from the board's CIA record) works. smoke-c64-all.sh checks the log.
wait 6000
monitor ringload board.c64rering
png run/ring-board.png
monitor goto 4
monitor ringdump board-out.c64rering board
monitor run
wait 500
type poke1024,1
key return
wait 1000
monitor m ram 0400 0400
quit
