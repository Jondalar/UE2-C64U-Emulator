# S43 S12: trex_crt's dumper on the TRX core, the EasyFlash in --cart-slot, the Ultimate interface
# (smoke-trx-ultimate.cfg): USB0 > apps > trex_crt.u2a > Run App, Dump Cartridge, F3, type 32, path USB0.
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
key down
key return
expect-console "[CartBase] core: TRX64 Ultimate" 5000
expect "STATUS : EasyFlash - F3=Dump" 10000
key f3
wait 3000
key return
wait 3000
key down
key down
key down
key right
wait 1500
key return
wait 2000
key return
expect "Done: 64 banks, 1024KB saved" 120000
key runstop
expect-console "[CartBase] Resetting C64 (the cartridge was touched)" 10000
wait 2000
quit
