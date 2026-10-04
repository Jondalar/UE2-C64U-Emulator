# S37 phase B3: TRX64-Ultimate's trx64/main app opens its menu on overlay.v (docs/specs/S37-trx-pixel-layer.md §5.10).
# Run with --board c64u, a fresh --flash, --c64-roms and --settings smoke-trx-overlay.cfg; scripts/smoke-trx-overlay.sh
# checks the pngs.
expect-console "[TRX] IO block present" 8000
wait 6000
png run/before.png
button
wait 2000
screen
png run/menu.png
button
wait 1500
png run/closed.png
quit
