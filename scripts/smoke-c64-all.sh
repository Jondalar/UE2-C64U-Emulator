#!/usr/bin/env bash
# Build ue2emu, then run every C64 smoke headless in a fresh temporary run directory, each on the images and flash
# copies its header names, built here from nothing. Stops at the first failure, prints that run's log tail, and exits
# non-zero. Part of the release gate (scripts/gate.sh, docs/specs/S35-release-gate.md).
#
#   scripts/smoke-c64-all.sh
#
# Firmware: $UE2_FIRMWARE (default: firmware/1541ultimate under the repo root) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf and roms/ (with 1541.bin and 1581.bin). A fresh flash boots the
# firmware's default System Mode, NTSC, so the SID tone checks expect 1038 Hz (docs/specs/S25-ntsc.md).
# scripts/make-sd-image.sh needs a macOS login session.
#
# Runs, in order:
#   ready       smoke-c64-ready.ctl    --flash run/flash.bin --c64-roms
#   type        smoke-c64-type.ctl     same flash
#   prg         smoke-c64-prg.ctl      --sd run/sd.img
#   freeze      smoke-c64-freeze.ctl   --no-overlay-ui on its own flash
#   sid-tone    smoke-sid-tone.ctl     --sid-socket1 armsid --audio-wav; wav-tone.py --expect 1038
#   carts       smoke-c64-carts.ctl    --sd run/carts.img (make-test-crts.py) --usb-keyboard: 28 PASS; AR, KCS,
#                                      SS5 and FC frozen
#   georam      smoke-georam.ctl       --settings smoke-georam.cfg: GeoRAM pages through $DE00, $DFFE/$DFFF
#   reu         smoke-reu.ctl          --settings smoke-reu.cfg --usb-dir: preload, DMA, Save REU Memory; the saved
#                                      file equals the preload image plus the 4 bytes the C64 stashed
#   drive       smoke-c64-drive.ctl    --sd run/drive-sd.img: the SAVEd NEW equals TEST in the D64
#   1581        smoke-1581.py          on the drive run's flash (1541 ROM set), the 1581 ROM added to its SD card
#   soft-iec    smoke-soft-iec.py      LOAD/SAVE on the Software IEC drive
#   usb         smoke-usb.ctl          --usb run/usb.img --usb-keyboard

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
fw=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}
elf=$fw/target/u64ii/riscv/ultimate/result/ultimate.elf
roms=$fw/roms
if [[ ! -f $elf || ! -f $roms/chars.bin || ! -f $roms/1541.bin || ! -f $roms/1581.bin ]]; then
    echo "firmware not found: need $elf and $roms with 1541.bin and 1581.bin (set UE2_FIRMWARE)" >&2
    exit 2
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-c64-smoke.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"

# emu <name> <script> [ue2emu run options]: run one script headless; stdout to run/<name>.log, stderr to
# run/<name>.err; returns ue2emu's exit code.
emu() {
    local name=$1 script=$2
    shift 2
    "$bin" run --headless --firmware "$elf" --roms "$roms" "$@" --script "$script" \
        >"run/$name.log" 2>"run/$name.err"
}

fail() {
    local name=$1
    shift
    echo "FAIL $name: $*"
    echo "--- run/$name.log (last 40 lines)"
    tail -n 40 "run/$name.log" 2>/dev/null || true
    echo "--- run/$name.err"
    cat "run/$name.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

# smoke <name> <script> [ue2emu run options]: run a script that must pass.
smoke() {
    local name=$1 start=$SECONDS rc=0
    emu "$@" || rc=$?
    ((rc == 0)) || fail "$name" "exit code $rc"
    pass "$name" "$start"
}

pass() {
    local emulated
    emulated=$(grep -o '[0-9.]* s emulated' "run/$1.err" 2>/dev/null | tail -n 1 || true)
    echo "PASS $1 (${emulated:-? s emulated}, $((SECONDS - $2)) s wall)"
}

sd() { "$repo/scripts/make-sd-image.sh" "$@" >/dev/null; }
add() { "$repo/scripts/add-sd-files.sh" "$@" >/dev/null; }

smoke ready "$repo/scripts/smoke-c64-ready.ctl" --flash run/flash.bin --c64-roms
grep -q "READY." run/ready.log || fail ready "no READY. on the C64 screen"
smoke type "$repo/scripts/smoke-c64-type.ctl" --flash run/flash.bin --c64-roms
grep -q " 42" run/type.log || fail type "the C64 did not print 42"

sd run/sd.img
smoke prg "$repo/scripts/smoke-c64-prg.ctl" --flash run/flash.bin --c64-roms --sd run/sd.img
grep -q "HELLO FROM UE2EMU" run/prg.log || fail prg "the PRG did not run"

smoke freeze "$repo/scripts/smoke-c64-freeze.ctl" --no-overlay-ui --flash run/flash-freeze.bin --c64-roms
grep -q "Frozen on Bad line." run/freeze.log || fail freeze "the C64 was not frozen"
! grep -q "Hard stop!!!" run/freeze.log || fail freeze "a hard stop"

start=$SECONDS
cp run/flash.bin run/flash-tone.bin
emu sid-tone "$repo/scripts/smoke-sid-tone.ctl" --flash run/flash-tone.bin --c64-roms --sid-socket1 armsid \
    --audio-wav run/sid-tone.wav || fail sid-tone "exit code $?"
python3 "$repo/scripts/wav-tone.py" run/sid-tone.wav --expect 1038 >>run/sid-tone.log || fail sid-tone "no 1038 Hz tone"
pass sid-tone "$start"

start=$SECONDS
python3 "$repo/scripts/make-test-crts.py" run/carts >/dev/null
sd run/carts.img
add run/carts.img run/carts/*
emu carts "$repo/scripts/smoke-c64-carts.ctl" --flash run/flash.bin --c64-roms --sd run/carts.img --usb-keyboard \
    --speed max || fail carts "exit code $?"
passes=$(grep -c " PASS" run/carts.log || true)
((passes == 28)) || fail carts "$passes of 28 carts passed"
for frozen in "ACTION REPLAY" KCS "SUPER SNAPSHOT" "FINAL CARTRIDGE"; do
    grep -q "$frozen FROZEN" run/carts.log || fail carts "no $frozen FROZEN"
done
! grep -q "Time out" run/carts.log || fail carts "a time-out"
pass carts "$start"

cp run/flash.bin run/flash-georam.bin
smoke georam "$repo/scripts/smoke-georam.ctl" --flash run/flash-georam.bin --c64-roms \
    --settings "$repo/scripts/smoke-georam.cfg" --speed max

start=$SECONDS
cp run/flash.bin run/flash-reu.bin
mkdir -p run/reu-share
python3 -c 'import sys; sys.stdout.buffer.write(bytes((a + (a >> 8) * 7 + (a >> 16) * 13) & 255 for a in range(1 << 17)))' \
    >run/reu-share/preload.reu
cp run/reu-share/preload.reu run/reu-expected.reu
printf '\x01\x02\x03\x04' | dd of=run/reu-expected.reu bs=1 seek=256 conv=notrunc 2>/dev/null
emu reu "$repo/scripts/smoke-reu.ctl" --flash run/flash-reu.bin --c64-roms --settings "$repo/scripts/smoke-reu.cfg" \
    --usb-dir run/reu-share --usb-dir-work run/reu-work --speed max || fail reu "exit code $?"
cmp run/reu-share/memory.reu run/reu-expected.reu || fail reu "the saved REU differs from the preload plus the stash"
pass reu "$start"

start=$SECONDS
sd run/drive-sd.img
python3 "$repo/scripts/d64tool.py" build run/drive.d64 --title "UE2 DRIVE" --id U2 --print "TEST=DRIVE A LOADED OK" \
    --print "SECOND=SECOND FILE" --filler BIG=30000 >/dev/null
add run/drive-sd.img "$roms/1541.bin" run/drive.d64
cp run/flash.bin run/flash-drive.bin
emu drive "$repo/scripts/smoke-c64-drive.ctl" --flash run/flash-drive.bin --c64-roms --sd run/drive-sd.img \
    || fail drive "exit code $?"
python3 "$repo/scripts/d64tool.py" sd-get run/drive-sd.img drive.d64 run/after.d64 >/dev/null
cmp <(python3 "$repo/scripts/d64tool.py" extract run/after.d64 NEW) \
    <(python3 "$repo/scripts/d64tool.py" extract run/after.d64 TEST) || fail drive "NEW differs from TEST"
pass drive "$start"

start=$SECONDS
cp run/flash-drive.bin run/flash-1581.bin
sd run/sd-1581.img
add run/sd-1581.img "$roms/1541.bin" run/drive.d64 "$roms/1581.bin"
python3 "$repo/scripts/smoke-1581.py" --bin "$bin" --firmware "$elf" --roms "$roms" --flash run/flash-1581.bin \
    --sd run/sd-1581.img >run/1581.out 2>&1 || { cp run/1581.out run/1581.log; fail 1581 "see run/smoke-1581.log"; }
echo "PASS 1581 ($((SECONDS - start)) s wall)"

start=$SECONDS
cp run/flash.bin run/flash-iec.bin
python3 "$repo/scripts/smoke-soft-iec.py" --bin "$bin" --firmware "$elf" --roms "$roms" --flash run/flash-iec.bin \
    >run/soft-iec.out 2>&1 || { cp run/soft-iec.out run/soft-iec.log; fail soft-iec "see run/soft-iec.log"; }
echo "PASS soft-iec ($((SECONDS - start)) s wall)"

sd run/usb.img 48
smoke usb "$repo/scripts/smoke-usb.ctl" --flash run/flash.bin --c64-roms --usb run/usb.img --usb-keyboard

cd /
rm -rf "$rundir"
echo "all C64 smoke tests passed"
