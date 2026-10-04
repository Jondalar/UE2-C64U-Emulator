# S37 phase A: TRX64-Ultimate's trx64/main app on the pixel layer (docs/specs/S37-trx-pixel-layer.md §3).
# Run with --trx-io (or auto) --board c64u --flash run/flash.bin --c64-roms.
# Expected: the console says "LVGL 9.6.0 on the pixel layer"; run/trx-splash.png is the 960x540 composite with the
# splash; "splash done; UI parked" follows and `monitor trx` shows PX_CTRL 0x22 (layer off); run/trx-parked.png is the C64 between the bars.
expect-console "[TRX UI] LVGL 9.6.0 on the pixel layer" 8000
wait 1000
monitor trx
png run/trx-splash.png
expect-console "[TRX UI] splash done; UI parked" 8000
wait 200
monitor trx
png run/trx-parked.png
quit
