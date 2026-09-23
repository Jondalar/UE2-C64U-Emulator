# W4-SID: a BASIC voice reaches the SID sample stream (docs/status/sid-audio.md).
# Run with --flash run/flash.bin holding the C64 ROMs (docs/status/c64.md setup) and --audio-wav run/sid-tone.wav.
# Expected: `scripts/wav-tone.py run/sid-tone.wav --expect 1000` passes: the last second is a 1000 Hz sawtooth
# (F = 256*66+133 = 17029; 17029 * 985248 / 2^24 = 1000.1 Hz at PAL), volume 15, attack 0, sustain 15.
# Under System Mode NTSC, the firmware's default, the same register plays 17029 * 1022730 / 2^24 = 1038.1 Hz
# (docs/specs/S25-ntsc.md): check with --expect 1038, or set System Mode PAL first.
# `type` follows the firmware keymap, so BASIC is typed in lower case (smoke-c64-type.ctl).
wait 6000
type poke 54296,15:poke 54277,0:poke 54278,240
key return
type poke 54273,66:poke 54272,133:poke 54276,33
key return
wait 1500
c64screen
quit
