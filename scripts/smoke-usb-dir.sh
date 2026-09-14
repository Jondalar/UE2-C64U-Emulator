#!/usr/bin/env bash
# --usb-dir smoke test (docs/status/usb-dir.md): a scratch directory as a USB stick. The firmware lists it, writes to
# it (a new D64, a deletion), usb-sync writes that back, a file added on the host shows up after the automatic replug,
# and quit syncs the rest.
#
#   scripts/smoke-usb-dir.sh
#
# Drives scripts/smoke-usb-dir.ctl over the TCP control protocol, one line at a time. Lines starting with "#!" run on
# the host with bash at that point (stdin closed), with SHARE (the shared directory), SCRATCH and WORK exported; a
# failing one fails the test. After `quit` the driver waits for ue2emu to exit (its final sync) before going on.
#
# Everything lives in a new scratch directory, run/smoke-usb-dir.XXXXXX under the repo root (UE2_SMOKE_DIR sets
# another parent); nothing outside it is read or written. It is removed after a pass (UE2_SMOKE_KEEP=1 keeps it) and
# kept after a failure. Firmware: $UE2_FIRMWARE as for scripts/smoke-all.sh. UE2_SMOKE_NO_BUILD=1 skips cargo build.

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
fw=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}
elf=$fw/target/u64ii/riscv/ultimate/result/ultimate.elf
if [[ ! -f $elf || ! -f $fw/roms/chars.bin ]]; then
    echo "firmware not found: need $elf and $fw/roms (set UE2_FIRMWARE)" >&2
    exit 2
fi
if [[ ${UE2_SMOKE_NO_BUILD:-} != 1 ]]; then
    cargo build --release --manifest-path "$repo/Cargo.toml"
fi
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu
ctl=$repo/scripts/smoke-usb-dir.ctl

parent=${UE2_SMOKE_DIR:-$repo/run}
mkdir -p "$parent"
SCRATCH=$(mktemp -d "$parent/smoke-usb-dir.XXXXXX")
SHARE=$SCRATCH/share
WORK=$SCRATCH/work
export SCRATCH SHARE WORK
echo "scratch directory: $SCRATCH"

# --- the shared directory: nested directories, a long file name, a PRG, a D64, and enough files that one deletion
# stays below the mass-deletion guard.
mkdir -p "$SHARE/games/sub"
printf '\x01\x08\x14\x08\x0a\x00\x99\x22HELLO FROM THE HOST\x22\x00\x00\x00' >"$SHARE/hello.prg"
printf 'This file is deleted by the firmware and must end up in .ue2-trash.\n' >"$SHARE/readme.txt"
cp "$SHARE/readme.txt" "$SCRATCH/readme.orig"
printf 'A long file name on a FAT32 stick.\n' >"$SHARE/A Long File Name For The Emulated Stick.txt"
head -c 174848 /dev/zero >"$SHARE/games/demo.d64"
printf '\x01\x08\x00\x00' >"$SHARE/games/sub/deep.prg"
for i in 1 2 3 4 5 6; do printf 'note %s\n' "$i" >"$SHARE/notes$i.txt"; done
touch -t 202001021530 "$SHARE/hello.prg"

emu=
fail() {
    echo "FAIL: $*"
    if [[ -n $emu ]] && kill -0 "$emu" 2>/dev/null; then
        kill "$emu" 2>/dev/null || true
        wait "$emu" 2>/dev/null || true
    fi
    echo "--- console (last 40 lines)"
    tail -n 40 "$SCRATCH/console.log" 2>/dev/null || true
    echo "--- stderr"
    cat "$SCRATCH/stderr.log" 2>/dev/null || true
    echo "kept: $SCRATCH"
    exit 1
}

"$bin" run --headless --speed max --firmware "$elf" --roms "$fw/roms" --flash "$SCRATCH/flash.bin" \
    --usb-dir "$SHARE" --usb-dir-work "$WORK" --control 127.0.0.1:0 \
    >"$SCRATCH/console.log" 2>"$SCRATCH/stderr.log" &
emu=$!

port=
for _ in $(seq 1 600); do
    port=$(sed -n 's/^control: listening on 127\.0\.0\.1:\([0-9][0-9]*\)$/\1/p' "$SCRATCH/stderr.log")
    [[ -n $port ]] && break
    kill -0 "$emu" 2>/dev/null || fail "ue2emu exited during startup"
    sleep 0.05
done
[[ -n $port ]] || fail "ue2emu did not open its control port"
exec 3<>"/dev/tcp/127.0.0.1/$port"

start=$SECONDS
lineno=0
while IFS= read -r line || [[ -n $line ]]; do
    lineno=$((lineno + 1))
    case $line in
    '#!'*)
        cmd=${line#'#!'}
        echo "host: $cmd"
        bash -c "set -euo pipefail; $cmd" </dev/null || fail "line $lineno: host command failed:$cmd"
        ;;
    '' | '#'*) ;;
    *)
        echo "ctl: $line"
        printf '%s\n' "$line" >&3
        screen=0
        answered=
        while IFS= read -r -t 900 reply <&3; do
            reply=${reply%$'\r'}
            if [[ $reply == '--- screen ---' ]]; then
                screen=$((1 - screen))
            elif ((screen == 0)) && [[ $reply == ok ]]; then
                answered=1
                break
            elif ((screen == 0)) && [[ $reply == 'error line '* ]]; then
                fail "line $lineno: ${reply#error line * }"
            fi
            echo "  $reply"
        done
        [[ -n $answered ]] || fail "line $lineno: no answer from ue2emu"
        if [[ $line == quit ]]; then
            exec 3<&-
            rc=0
            wait "$emu" || rc=$?
            emu=
            ((rc == 0)) || fail "ue2emu exited with $rc"
        fi
        ;;
    esac
done <"$ctl"
[[ -z $emu ]] || fail "the script did not end with quit"

emulated=$(grep -o '[0-9.]* s emulated' "$SCRATCH/stderr.log" | tail -n 1)
echo "PASS smoke-usb-dir (${emulated:-? s emulated}, $((SECONDS - start)) s wall)"
if [[ ${UE2_SMOKE_KEEP:-} == 1 ]]; then
    echo "kept: $SCRATCH"
else
    rm -rf "$SCRATCH"
fi
