# S43 S12: trex_crt's writer and verify on the TRX core: the app runs once so the registry has it, then
# USB0 > flashme.crt > Write to Cartridge, F3 (chip erase, program), F5 (verify), RUN/STOP (the session's end).
expect-console "[TRX] IO block present" 15000
wait 6000
rocker up
wait 2000
key down
key down
key down
key right
wait 1000
key right
wait 1000
key return
wait 800
key return
wait 2000
key n
wait 4000
key runstop
wait 2000
key left
wait 1000
key down
key return
wait 800
key down
key down
key down
key down
key down
key down
key down
key down
key return
expect-console "[CartBase] core: TRX64 Ultimate" 5000
expect "STATUS : Ready." 15000
key f3
expect "Write complete!" 180000
key f5
expect "All banks verified OK!" 60000
key runstop
expect-console "[CartBase] Resetting C64 (the cartridge was touched)" 10000
wait 2000
quit
