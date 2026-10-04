#!/usr/bin/env bash
# S37 phase B3: TRX64-Ultimate's trx64/main firmware opens its menu on overlay.v ("Overlay on HDMI") headless, and the
# box is where overlay.v puts it (docs/specs/S37-trx-pixel-layer.md §5.10). Stops at the first failure and exits
# non-zero.
#
#   scripts/smoke-trx-overlay.sh
#
# TRX firmware: $UE2_TRX_FIRMWARE (default: ../TRX64-Ultimate/software/ultimate-app beside this repo) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf. ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), the
# flash is a fresh copy of run/flash.bin when there is one, else a blank image. The pngs stay in the run directory
# it prints (menu.png for a look by eye).

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

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-trx-overlay.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"
if [[ -f $repo/run/flash.bin ]]; then
    cp "$repo/run/flash.bin" run/flash.bin
fi

fail() {
    echo "FAIL: $1"
    tail -n 40 run/overlay.log run/overlay.err 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

"$bin" run --headless --speed max --board c64u --firmware "$elf" --roms "$roms" --flash run/flash.bin --c64-roms \
    --settings "$repo/scripts/smoke-trx-overlay.cfg" --script "$repo/scripts/smoke-trx-overlay.ctl" \
    >run/overlay.log 2>run/overlay.err || fail "exit code $?"
grep -q "trx-io: on (TRX firmware: trx_lv_open)" run/overlay.err || fail "auto did not find the TRX firmware"
grep -q "DISK FILE BROWSER" run/overlay.log || fail "the menu is not in the overlay's screen RAM"

# The firmware's SD settings (40 x 25 cells of 8 x 9, transparent index 0) give overlay.v's box 320 x 225 at 4 x 3:
# 1280 x 675 from (320, 202). Outside it the picture is the one before the menu; inside it a pixel is the C64 below
# at half brightness or one of the 16 overlay colours.
python3 - run/before.png run/menu.png run/closed.png <<'PY' || fail "menu.png: the box is not where overlay.v puts it"
import struct, sys, zlib

def png(path):
    data = open(path, 'rb').read()
    w, h = struct.unpack('>II', data[16:24])
    raw, pos = b'', 8
    while pos < len(data):
        n, kind = struct.unpack('>I4s', data[pos:pos + 8])
        if kind == b'IDAT':
            raw += data[pos + 8:pos + 8 + n]
        pos += 12 + n
    raw = zlib.decompress(raw)
    stride, prev, rows = 3 * w, bytearray(3 * w), []
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - 3] if i >= 3 else 0
            b, c = prev[i], prev[i - 3] if i >= 3 else 0
            p = a + b - c
            pred = [0, a, b, (a + b) // 2,
                    a if abs(p - a) <= abs(p - b) and abs(p - a) <= abs(p - c) else b if abs(p - b) <= abs(p - c) else c][f]
            line[i] = (line[i] + pred) & 0xFF
        rows.append([int.from_bytes(line[3 * x:3 * x + 3], 'big') for x in range(w)])
        prev = line
    return w, h, rows

(bw, bh, before), (mw, mh, menu), (cw, ch, closed) = (png(p) for p in sys.argv[1:4])
assert (bw, bh, mw, mh, cw, ch) == (960, 540, 1920, 1080, 960, 540), "sizes"
x0, y0, x1, y1 = 320, 202, 1600, 877
under = lambda x, y: before[y // 2][x // 2]
outside = [(x, y) for y in range(1080) for x in range(1920) if not (x0 <= x < x1 and y0 <= y < y1)]
bad = [(x, y) for x, y in outside if menu[y][x] != under(x, y)]
assert not bad, f"outside the box changed at {bad[:5]}"
dim = colours = 0
other = set()
for y in range(y0, y1):
    for x in range(x0, x1):
        if menu[y][x] == (under(x, y) >> 1) & 0x7F7F7F:
            dim += 1
        else:
            other.add(menu[y][x])
assert len(other) <= 16, f"{len(other)} colours in the box"
assert dim > 1000, f"only {dim} pixels of the C64 at half brightness"
for edge in ([(x0, y) for y in range(y0, y1)], [(x1 - 1, y) for y in range(y0, y1)],
             [(x, y0) for x in range(x0, x1)], [(x, y1 - 1) for x in range(x0, x1)]):
    assert any(menu[y][x] != under(x, y) for x, y in edge), f"nothing drawn on the edge at {edge[0]}"
print(f"box 1280 x 675 at (320, 202): {dim} dimmed pixels, {len(other)} overlay colours")
PY
echo "PASS overlay"
echo "TRX overlay smoke passed; pngs in $rundir/run"
