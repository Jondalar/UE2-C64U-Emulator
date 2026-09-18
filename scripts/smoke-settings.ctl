# S21 smoke test: --settings scripts/smoke-settings.cfg on a fresh --flash (docs/specs/S21-settings.md).
# Self-checking: the firmware's cart init reports the REU (16 MB = size 7) and the Command Interface, and its own
# configuration menu shows the three settings.
expect-console "REU: 01. REU_SZ: 07, UCI: 01" 20000
expect "F3=HELP" 10000
button
expect "Flash   Flash Disk             Ready"
key f2
expect "Memory Configuration"
key down
key down
key down
key down
key right
expect "|RAM Expansion Unit             Enabled|"
expect "|Size                             16 MB|"
expect "|Command Interface              Enabled|"
screen
quit
