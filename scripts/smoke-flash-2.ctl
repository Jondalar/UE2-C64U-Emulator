# M4 smoke test (flash), run 2 of 2: after scripts/smoke-flash-1.ctl with the same --flash run/flash.bin,
# started with --no-overlay-ui so nothing is seeded and only the saved flash content enables the overlay.
# Self-checking: the browser is drawn on the overlay (Interface Type = Overlay on HDMI survived the firmware's
# rewrite of the user-interface page), the root lists "Flash   Flash Disk             Ready", and User Interface
# Settings shows "Color Scheme                C128 Style".
expect "F3=HELP" 10000
button
expect "Flash   Flash Disk             Ready"
key f2
expect "|User Interface Settings"
key down
key down
key down
key down
key down
key down
key down
key down
key down
key right
expect "|Interface Type         Overlay on HDMI|"
expect "|Color Scheme                C128 Style|"
screen
png run/flash2.png
quit
