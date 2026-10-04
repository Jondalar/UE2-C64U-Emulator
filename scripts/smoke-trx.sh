#!/usr/bin/env bash
# S37 phase A: run TRX64-Ultimate's trx64/main firmware headless on the pixel layer, then the same ELF as on
# Gideon's bitstream. Stops at the first failure and exits non-zero.
#
#   scripts/smoke-trx.sh
#
# TRX firmware: $UE2_TRX_FIRMWARE (default: ../TRX64-Ultimate/software/ultimate-app beside this repo) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf. ROMs and the flash come from $UE2_FIRMWARE (default
# firmware/1541ultimate) and a fresh flash image.
#
# Runs:
#   on    smoke-trx.ctl  --board c64u, trx-io auto: "trx-io: on", the splash png 960x540, then the layer off
#   off   --trx-io=off: no LVGL line, the C64 reaches READY.

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

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-trx-smoke.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "run/$1.log" "run/$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

# png_size FILE: "W H" from the IHDR chunk.
png_size() {
    od -An -tu1 -j16 -N8 "$1" | tr -s ' \n' ' ' | awk '{print $1*16777216+$2*65536+$3*256+$4, $5*16777216+$6*65536+$7*256+$8}'
}

"$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash run/flash.bin --c64-roms \
    --script "$repo/scripts/smoke-trx.ctl" >run/on.log 2>run/on.err || fail on "exit code $?"
grep -q "trx-io: on (TRX firmware: trx_lv_open)" run/on.err || fail on "auto did not find the TRX firmware"
[[ $(png_size run/trx-splash.png) == "960 540" ]] || fail on "the splash png is $(png_size run/trx-splash.png)"
[[ $(png_size run/trx-parked.png) != "960 540" ]] || fail on "the layer is still on after the splash"
echo "PASS on"

cp run/flash.bin run/flash-off.bin
printf 'wait 6000\nexpect-c64 READY.\nquit\n' >run/off.ctl
"$bin" run --headless --speed max --board c64u --trx-io=off --firmware "$elf" --roms "$roms" --flash run/flash-off.bin \
    --c64-roms --script run/off.ctl >run/off.log 2>run/off.err || fail off "exit code $?"
! grep -q "LVGL" run/off.log || fail off "LVGL started without the TRX block"
echo "PASS off"

rm -rf "$rundir"
echo "all TRX smoke tests passed"
