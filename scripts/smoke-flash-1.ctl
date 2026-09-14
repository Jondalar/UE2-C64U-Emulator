# M4 smoke test (flash), run 1 of 2: change a setting through the config menu and save it to flash.
# Run with a fresh --flash run/flash.bin (starting without the file tests the overlay-UI seed as well), then
# scripts/smoke-flash-2.ctl with the same flash file. On a flash that already holds C128 Style no save popup
# appears and the "Save changes to Flash?" expect fails.
# Key path from the firmware: F2 opens the config browser (userinterface.cc:858, tree_browser.cc:510); the
# cursor skips separators (tree_browser_state.cc:218-236); RIGHT enters a store (config_menu.cc:283-286);
# RETURN on an enum opens its choices (config_menu.cc:103-105, 276-281), where a typed letter seeks the
# first matching choice case-insensitively and RETURN takes it (context_menu.cc:324-326, 337-339, 347-362);
# LEFT leaves the store and then the browser, whose "Save changes to Flash?" popup (Auto Save Config = Ask,
# config_menu.cc:168-200) takes RETURN as its first button, Yes (ui_elements.cc:106,140).
# Selecting the choice by name keeps the result independent of the stored value (default Ultimate Black).
expect "F3=HELP" 10000
button
expect "Flash   Flash Disk             Ready"
key f2
expect "|User Interface Settings"
# Video Configuration .. Power Settings, then User Interface Settings.
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
expect "|Color Scheme"
# Interface Type, Navigation Style, Color Scheme -> "C128 Style".
key down
key down
key return
expect "C128 Style"
key c
key return
expect "|Color Scheme                C128 Style|"
screen
key left
expect "|User Interface Settings"
key left
expect "Save changes to Flash?"
screen
key return
expect-console "Writing config store 'User Interface Settings' to flash"
expect-console "Page: 0 done."
expect-not "Save changes to Flash?"
expect "Flash   Flash Disk             Ready"
# Leave the menu; the flash image is written back when the emulator stops.
key runstop
wait 800
png run/flash1.png
quit
