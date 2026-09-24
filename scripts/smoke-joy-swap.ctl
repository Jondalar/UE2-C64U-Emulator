# S36: with the joystick swapper on, the menu reads port 1: the firmware writes U64II_KEYB_JOY = swap & 1
# (u64_config.cc:1064), which selects the port the register reads.
# Run:   --flash run/flash-joy-swap.bin --c64-roms --settings scripts/smoke-joy-swap.cfg
# Pass:  exit 0: port 1 moves the menu cursor to Temp and enters it.
expect "F3=HELP" 10000
button
expect "Flash   Flash Disk             Ready"
joy 1 down
wait 300
joy 1 down
wait 300
joy 1 right
expect "/Temp/"
quit
