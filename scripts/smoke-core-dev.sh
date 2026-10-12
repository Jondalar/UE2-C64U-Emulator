#!/usr/bin/env bash
# S41 §6: core dev mode, FPGA blocks absent (docs/specs/S41-core-dev-mode.md), with the RC1 app. Stops at the first
# failure and exits non-zero.
#
#   scripts/smoke-core-dev.sh [ELF]     (default run/trx/ultimate-3c8e4aa.elf, the RC1 app)
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), each run has a fresh flash image.
#
# Runs:
#   noack   --absent sampler --absent-mode sampler=noack: halts; the report names the sampler, an address in
#           0x10048000-0x100480FF, the PC and the time
#   ack0    --absent sampler, the trial's ack0: runs past that PC to "[TRX UI] splash done; UI parked"
#   cont    the noack run with --absent-continue: every stall site logged once, the summary counts per address, the app
#           boots on
#   all     reu,sampler,uci,jtag,turbo,apply absent in their default modes: the app boots, CAPS2 reads $9D (APPLY clear)
#   none    --absent none: no "absent:" line, the app boots as without the flag

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
elf=${1:-$repo/run/trx/ultimate-3c8e4aa.elf}
roms=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}/roms
if [[ ! -f $elf || ! -f $roms/chars.bin ]]; then
    echo "need the app ELF $elf and $roms (set UE2_FIRMWARE)" >&2
    exit 2
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-core-dev.XXXXXX")
cd "$rundir"
echo "run directory: $rundir"

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "$1.log" "$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

booted='expect-console "[TRX UI] splash done; UI parked" 15000'

# run NAME SCRIPT-LINES ARGS...: the app headless at full speed on a fresh flash, the script from the lines.
run() {
    local name=$1 lines=$2
    shift 2
    printf '%s\nquit\n' "$lines" >"$name.ctl"
    "$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash "$name-flash.bin" --c64-roms \
        --script "$name.ctl" "$@" >"$name.log" 2>"$name.err"
}

if run noack "$booted" --absent sampler --absent-mode sampler=noack; then
    fail noack "the run did not halt"
fi
grep -q "^halted: absent sampler (noack): .* 0x100480[0-9a-f][0-9a-f] never completes" noack.err ||
    fail noack "no halt on the sampler's window"
pc=$(sed -n 's/^  pc \(0x[0-9a-f]*\) .*/\1/p' noack.err | head -n 1)
[[ -n $pc ]] || fail noack "no PC in the report"
grep -q "^  backtrace" noack.err && grep -q "^  last console line: " noack.err &&
    grep -q " s emulated since boot, " noack.err || fail noack "the report lacks the backtrace, console line or time"
! grep -q "splash done; UI parked" noack.log || fail noack "the app went on"
echo "PASS noack (pc $pc: $(sed -n 's/^  pc 0x[0-9a-f]* //p' noack.err | head -n 1))"

run ack0 "$booted" --absent sampler || fail ack0 "exit code $?"
grep -q "^absent: sampler: ack0 0x10048000-0x100480ff" ack0.err || fail ack0 "the sampler is not absent with ack0"
! grep -q "^halted" ack0.err || fail ack0 "halted"
echo "PASS ack0 (past $pc to the parked UI)"

run cont "$booted" --absent sampler --absent-mode sampler=noack --absent-continue || fail cont "exit code $?"
grep -q "^absent: sampler noack .* @$pc .*(completed as ack0)" cont.err || fail cont "the stall at $pc is not logged"
grep -q "^absent summary: [1-9][0-9]* addresses stalled" cont.err || fail cont "no summary"
[[ $(grep -c "^absent: sampler noack " cont.err) == $(sed -n 's/^absent summary: \([0-9]*\) .*/\1/p' cont.err) ]] ||
    fail cont "not one line per stall site"
echo "PASS cont ($(sed -n 's/^absent summary: //p' cont.err))"

run all "$booted
monitor trx" --absent reu,sampler,uci,jtag,turbo,apply || fail all "exit code $?"
[[ $(grep -c "^absent: " all.err) == 6 ]] || fail all "not six absent blocks"
grep -qi "caps2 *0x9d" all.log || fail all "CAPS2 does not read \$9D"
echo "PASS all"

run none "$booted" --absent none || fail none "exit code $?"
! grep -q "^absent" none.err || fail none "an absent line without absent blocks"
echo "PASS none"

cd /
rm -rf "$rundir"
echo "all core dev smoke tests passed"
