# S37 phase B: the freeze menu of TRX64-Ultimate's trx64/main app on the pixel layer (docs/specs/S37-trx-pixel-layer.md
# §5.9). Run with --board c64u, a fresh --flash, --c64-roms and --settings smoke-trx-freeze.cfg (Interface Type Freeze,
# System Mode PAL) and --audio-wav run/freeze.wav; scripts/smoke-trx-freeze.sh checks what it prints and writes.
expect-console "[TRX] freeze menu on the pixel layer" 8000
wait 6000
# A 1000 Hz SID tone (smoke-sid-tone.ctl) under the freeze: top.v:1059 mutes it, the unfreeze brings it back.
type poke 54296,15:poke 54277,0:poke 54278,240
key return
type poke 54273,66:poke 54272,133:poke 54276,33
key return
wait 500
c64screen
png run/before.png
button
wait 1500
monitor trx
c64screen
png run/frozen.png
wait 1000
monitor trx
png run/frozen2.png
key down
wait 300
button
wait 1500
monitor trx
c64screen
png run/after.png
type print 6*7
key return
wait 500
c64screen
quit
