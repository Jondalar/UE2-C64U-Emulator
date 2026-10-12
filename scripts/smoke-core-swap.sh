#!/usr/bin/env bash
# S41 §5: the core swapped while the RC1 app runs (docs/specs/S41-core-dev-mode.md). The app has no code for a swap
# yet, so the smoke asserts what the emulator does and only logs how the unmodified app reacts. Stops at the first
# failure and exits non-zero.
#
#   scripts/smoke-core-swap.sh [ELF]     (default run/trx/ultimate-3c8e4aa.elf, the RC1 app)
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), each run has a fresh flash image.
#
# Runs:
#   swap    --absent-continue: the app boots; `core absent`: the C64 screen is empty, the picture black, the RISC-V runs
#           on; every access of the app into the core's windows is logged and the summary counts them (the app's
#           reaction, printed); `core present`: a fresh C64 boots to READY on its own; a second swap works the same
#   halt    the core's default noack: `core absent` halts the RISC-V on its first access into the core's windows, with
#           the report (what the board does without the JTAG console)
#   start   --absent core: absent from the first instruction; the app halts in its init on the first core window it
#           touches
#   ack0    --absent-mode core=ack0: the app runs on with the core gone and the C64 comes back with `core present`

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

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-core-swap.XXXXXX")
cd "$rundir"
echo "run directory: $rundir"

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "$1.log" "$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

booted='expect-console "[TRX UI] splash done; UI parked" 15000
wait 500'

# run NAME SCRIPT-LINES ARGS...: the app headless at full speed on a fresh flash, the script from the lines.
run() {
    local name=$1 lines=$2
    shift 2
    printf '%s\nquit\n' "$lines" >"$name.ctl"
    "$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash "$name-flash.bin" --c64-roms \
        --script "$name.ctl" "$@" >"$name.log" 2>"$name.err"
}

# black PNG: every pixel of an 8-bit RGB PNG is 0.
black() {
    python3 - "$1" <<'EOF'
import struct, sys, zlib
data = open(sys.argv[1], "rb").read()
w, h = struct.unpack(">II", data[16:24])
raw, pos = b"", 8
while pos < len(data):
    n, kind = struct.unpack(">I4s", data[pos:pos + 8])
    if kind == b"IDAT":
        raw += data[pos + 8:pos + 8 + n]
    pos += 12 + n
pixels = zlib.decompress(raw)
sys.exit(0 if all(b == 0 for i, b in enumerate(pixels) if i % (3 * w + 1)) else 1)
EOF
}

# c64 NAME N: the Nth `c64screen` block (1-based) of NAME.log.
c64() {
    awk -v n="$2" '/^--- c64 ---$/ { k++; next } k == 2 * n - 1' "$1.log"
}

swap="$booted
core absent
wait 3000
c64screen
png absent.png
core present
expect-c64 READY. 8000
c64screen
png present.png
core absent
wait 1000
core present
expect-c64 READY. 8000
wait 500"
run swap "$swap" --absent-continue || fail swap "exit code $?"
[[ $(grep -c "^core absent (noack): " swap.err) == 2 && $(grep -c "^core present: a fresh core" swap.err) == 2 ]] ||
    fail swap "not two swaps"
[[ -z $(c64 swap 1 | tr -d ' \n') ]] || fail swap "the C64 screen is not empty without the core"
black absent.png || fail swap "the picture is not black without the core"
c64 swap 2 | grep -q "READY." || fail swap "the fresh C64 did not boot"
grep -q "^absent: core noack " swap.err || fail swap "the app touched none of the core's windows"
grep -q "^absent summary: [1-9][0-9]* addresses stalled" swap.err || fail swap "no summary"
echo "PASS swap: the app's accesses into the absent core:"
sed -n '/^absent summary: /,$p' swap.err | grep -E "^absent summary|^  0x" | head -n 40

if run halt "$booted
core absent
wait 3000" ; then
    fail halt "the run did not halt"
fi
grep -q "^halted: absent core (noack): " halt.err || fail halt "no halt on the core's window"
echo "PASS halt: $(sed -n 's/^halted: //p' halt.err | head -n 1)"
grep -E "^  (pc |ra |last console line:)" halt.err | head -n 3

if run start "$booted" --absent core; then
    fail start "the run did not halt"
fi
grep -q "^core absent (noack): " start.err && grep -q "^halted: absent core (noack): " start.err ||
    fail start "the core was not absent from the start"
echo "PASS start: $(sed -n 's/^halted: //p' start.err | head -n 1)"
grep -E "^  (pc |last console line:)" start.err | head -n 2

run ack0 "$booted
core absent
wait 3000
core present
expect-c64 READY. 8000" --absent-mode core=ack0 || fail ack0 "exit code $?"
grep -q "^core absent (ack0): " ack0.err || fail ack0 "the core's mode is not ack0"
echo "PASS ack0"

cd /
rm -rf "$rundir"
echo "all core swap smoke tests passed"
