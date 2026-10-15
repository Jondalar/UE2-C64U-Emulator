# S42: the rocker's events reach TRX64-Ultimate's trx64/main app over the ESP32's WiFi UART
# (docs/specs/S42-esp32-rocker-events.md). Run with --board c64u, a fresh --flash, --c64-roms and
# --settings smoke-trx-overlay.cfg; scripts/smoke-trx-rocker.sh checks the two C64 screens.
expect-console "[TRX] IO block present" 8000
wait 6000
type hello
expect-c64 HELLO 2000
c64screen
rocker up-long
wait 3000
expect-c64 READY. 3000
c64screen
rocker up
wait 2000
expect "DISK FILE BROWSER" 3000
screen
quit
