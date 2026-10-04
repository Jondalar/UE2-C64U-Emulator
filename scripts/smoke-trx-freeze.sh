#!/usr/bin/env bash
# S37 phase B: freeze and unfreeze TRX64-Ultimate's trx64/main firmware headless (docs/specs/S37-trx-pixel-layer.md
# §5.9). Stops at the first failure and exits non-zero.
#
#   scripts/smoke-trx-freeze.sh
#
# TRX firmware: $UE2_TRX_FIRMWARE (default: ../TRX64-Ultimate/software/ultimate-app beside this repo) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf. ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), the
# flash is a fresh copy of run/flash.bin when there is one, else a blank image. The pngs stay in the run directory
# it prints (frozen.png for a look by eye).

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

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-trx-freeze.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"
if [[ -f $repo/run/flash.bin ]]; then
    cp "$repo/run/flash.bin" run/flash.bin
fi

fail() {
    echo "FAIL: $1"
    tail -n 40 run/freeze.log run/freeze.err 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

# png_size FILE: "W H" from the IHDR chunk.
png_size() {
    od -An -tu1 -j16 -N8 "$1" | tr -s ' \n' ' ' | awk '{print $1*16777216+$2*65536+$3*256+$4, $5*16777216+$6*65536+$7*256+$8}'
}

# black_bars FILE: whether columns 0-79 and 880-959 of a 960-wide RGB png are black on every line.
black_bars() {
    python3 - "$1" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], 'rb').read()
w, h = struct.unpack('>II', data[16:24])
raw, pos = b'', 8
while pos < len(data):
    n, kind = struct.unpack('>I4s', data[pos:pos + 8])
    if kind == b'IDAT':
        raw += data[pos + 8:pos + 8 + n]
    pos += 12 + n
raw = zlib.decompress(raw)
stride, prev, ok = 3 * w, bytearray(3 * w), True
for y in range(h):
    f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
    for i in range(stride):
        a = line[i - 3] if i >= 3 else 0
        b, c = prev[i], prev[i - 3] if i >= 3 else 0
        p = a + b - c
        pred = [0, a, b, (a + b) // 2, a if abs(p - a) <= abs(p - b) and abs(p - a) <= abs(p - c) else b if abs(p - b) <= abs(p - c) else c][f]
        line[i] = (line[i] + pred) & 0xFF
    ok &= not any(line[:240]) and not any(line[2640:])
    prev = line
sys.exit(0 if ok else 1)
PY
}

# block N FILE: the Nth block of `--- c64 ---` delimited monitor or screen output in FILE.
block() {
    awk -v n="$1" '/^--- c64 ---$/ {k++; next} k == 2 * n - 1' "$2"
}

"$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash run/flash.bin --c64-roms \
    --settings "$repo/scripts/smoke-trx-freeze.cfg" --script "$repo/scripts/smoke-trx-freeze.ctl" \
    >run/freeze.log 2>run/freeze.err || fail "exit code $?"
log=run/freeze.log

# Boot.
grep -q "trx-io: on (TRX firmware: trx_lv_open)" run/freeze.err || fail "auto did not find the TRX firmware"
grep -q "\[TRX\] IO block present, CAPS E1" $log || fail "CAPS is not E1"
grep -q "\[TRX\] freeze menu on the pixel layer: still at .*, text in hardware, commit list" $log ||
    fail "the freeze menu is not on the pixel layer with the commit list"
block 1 $log | grep -q "READY." || fail "no READY. before the freeze"

# Frozen.
for line in "[TRX] pixel layer: 2 buffer(s) of 864000 bytes" "[C64] goUltimax() — C64_MODE = ULTIMAX" "Init IO."; do
    grep -qF "$line" $log || fail "no '$line' on the console"
done
! grep -q "\[TRX\] pixel layer: no flip" $log || fail "the flip did not come"
monitors=$(grep -c "^  id            TRX2" $log)
[[ $monitors == 3 ]] || fail "$monitors monitor dumps, not 3"
frozen=$(awk '/^  id            TRX2/ {k++} k == 1' $log)
for want in "gate_ctrl     0x00000301" "sh_status     0x00010001" "tx_ctrl       0x01000001" "(shown: yes)" \
    "px_ctrl       0x23" "px_size       800 x 540  stride 1600" "px_pos        x0 160 y0 0" "valid yes"; do
    grep -qF "$want" <<<"$frozen" || fail "frozen: no '$want' in monitor trx"
done
block 2 $log | grep -q "DISK FILE BROWSER" || fail "frozen: the menu is not on the C64 text screen"
! block 2 $log | grep -q "READY." || fail "frozen: READY. on the text screen"
[[ $(png_size run/frozen.png) == "960 540" ]] || fail "frozen.png is $(png_size run/frozen.png)"
black_bars run/frozen.png || fail "frozen.png: the bars are not black"
frames1=$(awk '/^  vic frames/ {print $3}' $log | sed -n 1p)
frames2=$(awk '/^  vic frames/ {print $3}' $log | sed -n 2p)
[[ $frames1 == "$frames2" ]] || fail "the VIC frame count moved while frozen: $frames1, $frames2"
cmp -s run/frozen.png run/frozen2.png || fail "frozen2.png differs from frozen.png"
echo "PASS frozen"

# After the unfreeze.
after=$(awk '/^  id            TRX2/ {k++} k == 3' $log)
for want in "gate_ctrl     0x00000000" "sh_status     0x00010000" "tx_ctrl       0x01000000" "px_ctrl       0x22"; do
    grep -qF "$want" <<<"$after" || fail "after: no '$want' in monitor trx"
done
frames3=$(awk '/^  vic frames/ {print $3}' $log | sed -n 3p)
((frames3 > frames2)) || fail "the VIC frame count stands after the unfreeze: $frames2, $frames3"
[[ "$(block 1 $log)" == "$(block 3 $log)" ]] || fail "the text screen after the unfreeze differs from before"
block 4 $log | grep -q "^ 42$" || fail "print 6*7 did not give 42 after the unfreeze"
echo "PASS unfrozen"

echo "TRX freeze smoke passed; pngs in $rundir/run"
