# S17: the firmware's SID player maps a PSID v3 onto UltiSID 1 ($D400) and UltiSID 2 ($D420), and the tune's second
# SID alone plays a 1000 Hz tone (docs/status/sid-audio.md).
# Image: python3 scripts/make-stereo-sid.py run/sid-stereo/s03-stereo.sid; scripts/make-sd-image.sh run/sid-stereo.img;
#        scripts/add-sd-files.sh run/sid-stereo.img run/sid-stereo/s03-stereo.sid
# Run:   --flash <copy of run/flash.bin> --c64-roms --sd run/sid-stereo.img --usb-keyboard --audio-wav run/sid-stereo.wav
# Pass:  exit 0; the console has "Trying to map SID 1 … at address $D420", "Resulting address map: … Emu1: 40/FE
#        Emu2: 42/FE" and "Sid 1 was mapped to slot 3"; `scripts/wav-tone.py run/sid-stereo.wav --expect 1000` passes.
# The browser lists demo.d64, hello.prg, readme.txt, s03-stereo.sid.
wait 6000
button
wait 2000
key right
wait 1000
key down
key down
key down
wait 300
# Play Main Tune is the first context-menu entry (filetype_sid.cc:405).
key return
wait 500
key return
wait 8000
c64screen
quit
