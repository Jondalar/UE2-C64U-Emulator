#!/usr/bin/env bash
# S42: TRX64-Ultimate's trx64/main firmware gets the C64U rocker as the board does, as ESP32 event packets on the WiFi
# UART (docs/specs/S42-esp32-rocker-events.md). Stops at the first failure and exits non-zero.
#
#   scripts/smoke-trx-rocker.sh
#
# TRX firmware: $UE2_TRX_FIRMWARE (default: ../TRX64-Ultimate/software/ultimate-app beside this repo) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf. ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), the
# flash is a fresh copy of run/flash.bin when there is one, else a blank image.
#
# Runs:
#   rocker   smoke-trx-rocker.ctl: up long resets the C64 (HELLO typed before is gone, READY. again), up opens the menu
#   off      --trx-io=off: `rocker up` is refused
#   button   `button 30` is refused with --trx-io: the board holds the button 200 ms

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
app=${UE2_TRX_FIRMWARE:-$repo/../TRX64-Ultimate/software/ultimate-app}
elf=$app/target/u64ii/riscv/ultimate/result/ultimate.elf
roms=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}/roms
if [[ ! -f $elf || ! -f $roms/chars.bin ]]; then
    echo "need the TRX firmware ELF $elf and $roms (set UE2_TRX_FIRMWARE, UE2_FIRMWARE)" >&2
    exit 2
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-trx-rocker.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"
if [[ -f $repo/run/flash.bin ]]; then
    cp "$repo/run/flash.bin" run/flash.bin
fi

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "run/$1.log" "run/$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

# c64_screen N: the Nth `c64screen` dump of run/rocker.log.
c64_screen() {
    awk -v n="$1" '/^--- c64 ---$/ { k++; on = (k == 2 * n - 1); next } on' run/rocker.log
}

run() {
    local name=$1 flash=$2
    shift 2
    "$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash "$flash" --c64-roms \
        --settings "$repo/scripts/smoke-trx-overlay.cfg" "$@" >"run/$name.log" 2>"run/$name.err"
}

cp run/flash.bin run/flash-off.bin 2>/dev/null || true
cp run/flash.bin run/flash-button.bin 2>/dev/null || true
run rocker run/flash.bin --script "$repo/scripts/smoke-trx-rocker.ctl" || fail rocker "exit code $?"
grep -q "trx-io: on (TRX firmware: trx_lv_open)" run/rocker.err || fail rocker "auto did not find the TRX firmware"
c64_screen 1 | grep -q HELLO || fail rocker "HELLO is not on the C64 before the reset"
! c64_screen 2 | grep -q HELLO || fail rocker "up long did not reset the C64"
c64_screen 2 | grep -q "READY." || fail rocker "the C64 is not at READY. after the reset"
grep -q "DISK FILE BROWSER" run/rocker.log || fail rocker "up did not open the menu"
echo "PASS rocker"

printf 'rocker up\nquit\n' >run/off.ctl
! run off run/flash-off.bin --trx-io=off --script run/off.ctl || fail off "rocker ran without the TRX block"
grep -q "rocker: only the TRX bitstream decodes the ESP32's events (--trx-io)" run/off.err || fail off "not refused by name"
echo "PASS off"

printf 'button 30\nquit\n' >run/button.ctl
! run button run/flash-button.bin --script run/button.ctl || fail button "button 30 ran with --trx-io"
grep -q "the board holds the menu button 200 ms" run/button.err || fail button "not refused by name"
echo "PASS button"

rm -rf "$rundir"
echo "all TRX rocker smoke tests passed"
