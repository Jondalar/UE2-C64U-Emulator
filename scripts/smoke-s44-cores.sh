#!/usr/bin/env bash
# S44 S7: the MultiCore core application as the A1200 (docs/specs/S44-multicore-cores.md). Stops at the first failure and
# exits non-zero.
#
#   scripts/smoke-s44-cores.sh COREAPP     (coreapp.elf or coreapp.app of TRX64U-MultiCore; not in this repo)
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), each run has a fresh flash image.
#
# Runs:
#   a1200-480p, -720p, -1080p   docs/examples/core-amiga1200.toml with the hand-over's video at that mode: the multicore
#                               profile with a valid page, the A1200 descriptor, the menu in the pixel layer, a png
#   a500                        the same ID with AM_FEAT 0x01 and the hand-over's id "amiga": the A500 descriptor
#   control                     the A1200 run expecting the A500: it must fail
#   none                        no ICAP, no page (--core with board = "monolith", no handover): the monolith profile

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
app=${1:-}
roms=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}/roms
if [[ -z $app || ! -f $app || ! -f $roms/chars.bin ]]; then
    echo "usage: $0 COREAPP (and $roms, set UE2_FIRMWARE)" >&2
    exit 2
fi
app=$(cd "$(dirname "$app")" && pwd)/$(basename "$app")

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu
profile=$repo/docs/examples/core-amiga1200.toml

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-s44.XXXXXX")
cd "$rundir"
echo "run directory: $rundir"

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "$1.log" "$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

# run NAME SCRIPT-LINES ARGS...: coreapp headless at full speed on a fresh flash, the script from the lines.
run() {
    local name=$1 lines=$2
    shift 2
    printf '%s\nquit\n' "$lines" >"$name.ctl"
    nice -n 15 "$bin" run --headless --speed max --board c64u --caps 25E40202 --trx-io on --c64 none --firmware "$app" \
        --roms "$roms" --flash "$name-flash.bin" --script "$name.ctl" --max-seconds 150 "$@" >"$name.log" 2>"$name.err"
}

a1200='expect-console "board: multicore (ICAP 0x7A, hand-over valid)" 60000
expect-console "AMIG Amiga 1200 (AGA, 68020), cor /Usb0/cores/amiga1200.cor" 10000
wait 3000'

for mode in 480p 720p 1080p; do
    run "a1200-$mode" "$a1200
png a1200-$mode.png" --core "$profile" --handover "video=$mode" || fail "a1200-$mode" "exit code $?"
    grep -q "^handover: 0x03eff000: core \"Amiga 1200 (AGA, 68020)\", id \"amiga1200\"" "a1200-$mode.err" || fail "a1200-$mode" "no hand-over line"
    ! grep -q "no pixel layer" "a1200-$mode.log" || fail "a1200-$mode" "the menu has no pixel layer"
    [[ -s a1200-$mode.png ]] || fail "a1200-$mode" "no png"
    echo "PASS a1200-$mode"
done

cat >a500.toml <<'A500'
id = "AMIG"
[ro]
0x00C = 0x01
[handover]
core = "Amiga (OCS/ECS, 68000)"
id = "amiga"
path = "/Usb0/cores/amiga.cor"
A500
run a500 'expect-console "AMIG Amiga (OCS/ECS, 68000), cor /Usb0/cores/amiga.cor" 60000' --core a500.toml || fail a500 "exit code $?"
echo "PASS a500"

if run control 'expect-console "AMIG Amiga (OCS/ECS, 68000)" 60000' --core "$profile"; then
    fail control "the A1200 run passed as the A500"
fi
echo "PASS control (fails as it must)"

printf 'id = "AMIG"\nboard = "monolith"\n' >mono.toml
run none 'expect-console "board: monolith (ICAP absent, no hand-over)" 60000' --core mono.toml || fail none "exit code $?"
echo "PASS none"

cd /
rm -rf "$rundir"
echo "all S44 core smoke tests passed"
