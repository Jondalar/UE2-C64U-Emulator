# S23 §9.5: the monitor over the control port. TRX64's own verbs against our C64, our verbs on the Ultimate side,
# and the whole config cycle through the firmware's command interface.
#
# Run with a fresh --flash and --settings scripts/smoke-settings.cfg, which turns the Command Interface on and puts
# REU Size=16 MB in the pages. Every `monitor` line that answers with an error fails the script, so this is an
# assertion on each one; smoke-all.sh also greps the log for the value the config cycle must leave behind.
expect "F3=HELP" 10000

# The library's verbs, on the machine's own C64.
monitor r
monitor m 0400 0407
monitor d e000 e001
monitor device

# Ours: the firmware's RISC-V, its tasks, the two clocks, and the settings as the flash holds them.
monitor fw
monitor fw tasks
monitor clock

# The Ultimate's own hardware, as the firmware's registers stand. This run has only --flash, so `sd` and `usb`
# would refuse by name and are left out; `itu`, `cart`, `flash`, `net` and `audio` always answer.
monitor itu
monitor cart
monitor flash
monitor net
monitor audio
# Run control (S23 M3). The C64's halt is the machine's own stop, so it reads back out of C64_STOP; holding the
# firmware holds the C64 with it, and `fw go` has to bring both back or the rest of this script would not run.
monitor status
monitor c64 halt
monitor c64 step 4
monitor c64 go
monitor fw halt
monitor fw step 3
monitor fw go
monitor status

monitor config flash
monitor config "C64 and Cartridge Settings" "REU Size"

# The config cycle. `set` hands a two-line .cfg to the running firmware, which applies and effectuates it; `write`
# then puts the same item in the config pages, where the next boot finds it.
monitor config set "C64 and Cartridge Settings" "REU Size" "2 MB"
monitor config write
monitor config "C64 and Cartridge Settings" "REU Size"

# A .cfg of every store, written by the firmware itself into its RAM disk, and handed straight back to it: the
# firmware must accept its own spelling without a single line it cannot apply.
monitor config write /temp/all.cfg
monitor config read /temp/all.cfg
quit
