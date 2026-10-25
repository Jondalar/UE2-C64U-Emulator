#!/usr/bin/env bash
# S43: the TRX64 Ultimate release app sees the bitstream's reSID8 control, a 1541-only drive A and the expansion
# port's slot block (docs/specs/S43-trx-resid8-drive-slot.md). Stops at the first failure and exits non-zero.
#
#   UE2_TRX_RELEASE=<dir with trx64_update_*.ue2 and trex_crt.u2a> scripts/smoke-trx-s43.sh
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), the flash is a fresh copy of run/flash.bin when there
# is one, else a blank image. The cartridges are scripts/make-slot-crts.py's EasyFlash.
#
# Runs:
#   menus   smoke-trx-s43-menus.ctl: reSID8 menus at the reset values, Drive Type offers the 1541 only (S1, S5)
#   dump    smoke-trx-s43-dump.ctl: trex_crt dumps the --cart-slot EasyFlash on the TRX core; the dump equals the CRT
#   write   smoke-trx-s43-write.ctl: trex_crt writes and verifies another image; the cartridge then holds it (S12)

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
rel=${UE2_TRX_RELEASE:-}
roms=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}/roms
ue2=$(ls "$rel"/trx64_update_*.ue2 2>/dev/null | head -n 1 || true)
if [[ -z $ue2 || ! -f $rel/trex_crt.u2a || ! -f $roms/chars.bin ]]; then
    echo "need UE2_TRX_RELEASE with trx64_update_*.ue2 and trex_crt.u2a, and $roms (set UE2_FIRMWARE)" >&2
    exit 2
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-trx-s43.XXXXXX")
cd "$rundir"
mkdir run crts usb usb/apps
echo "run directory: $rundir"
python3 -I "$repo/scripts/make-slot-crts.py" crts >/dev/null
cp "$rel/trex_crt.u2a" usb/apps/
# The image the writer flashes: the EasyFlash with every byte past each window's first 64 changed.
python3 -I - crts/s08-easyflash.crt usb/flashme.crt <<'EOF'
import struct, sys
b = bytearray(open(sys.argv[1], 'rb').read())
p = struct.unpack('>I', b[16:20])[0]
while p < len(b):
    size = struct.unpack('>H', b[p + 14:p + 16])[0]
    for i in range(p + 16 + 0x40, p + 16 + size):
        b[i] ^= 0x5A
    p += struct.unpack('>I', b[p + 4:p + 8])[0]
open(sys.argv[2], 'wb').write(b)
EOF

fail() {
    echo "FAIL $1: $2"
    tail -n 30 "run/$1.log" "run/$1.err" 2>/dev/null || true
    echo "kept: $rundir"
    exit 1
}

run() {
    local name=$1 cfg=$2
    shift 2
    if [[ -f $repo/run/flash.bin ]]; then
        cp "$repo/run/flash.bin" "run/flash-$name.bin"
    fi
    "$bin" run --headless --speed max --board c64u --caps 25E40202 --firmware "$ue2" --roms "$roms" \
        --flash "run/flash-$name.bin" --c64-roms --settings "$repo/scripts/$cfg" \
        --script "$repo/scripts/smoke-trx-s43-$name.ctl" "$@" >"run/$name.log" 2>"run/$name.err"
}

# same A B: the two CRTs hold the same type and the same bytes in every 8 K window, an absent window as $FF.
same() {
    python3 -I - "$1" "$2" <<EOF
import sys
sys.path.insert(0, '$repo/scripts')
import cartslot_common as c
a, b = (c.parse_crt(open(p, 'rb').read()) for p in sys.argv[1:3])
banks = sorted(set(c.banks(a)) | set(c.banks(b)))
ff = b'\xFF' * 0x2000
bad = [(n, w) for n in banks for w in ('roml', 'romh') if (c.window(a, n, w) or ff) != (c.window(b, n, w) or ff)]
sys.exit(1 if bad or a['hw_type'] != b['hw_type'] else 0)
EOF
}

screen() {
    awk -v n="$2" '/^--- screen ---$/ { k++; on = (k == 2 * n - 1); next } on' "run/$1.log"
}

run menus smoke-trx-overlay.cfg || fail menus "exit code $?"
grep -q "\[TRX\] CAPS3 01" run/menus.log || fail menus "the app did not find CAPS3 b0"
for line in "reSID8 1 Filter Curve       Chip Model" "reSID8 2 6581 Base                 245" \
    "reSID8 1 6581 Offset              -780" "reSID8 2 Digis Level              Auto"; do
    screen menus 1 | grep -qF "$line" || fail menus "no '$line'"
done
screen menus 2 | grep -qF "|Drive Type                      +1541||" || fail menus "Drive Type offers more than the 1541"
screen menus 2 | grep -qF "|Drive Bus ID                    +----+|" || fail menus "Drive Type offers more than the 1541"
echo "PASS menus"

run dump smoke-trx-ultimate.cfg --usb-dir usb --usb-dir-work run/work --cart-slot crts/s08-easyflash.crt ||
    fail dump "exit code $?"
same crts/s08-easyflash.crt usb/cartridge.crt || fail dump "the dump differs from the CRT"
# Off the stick: the writer's run picks the second entry of USB0, flashme.crt.
mv usb/cartridge.crt run/dump.crt
rm -rf usb/.ue2-trash
echo "PASS dump"

run write smoke-trx-ultimate.cfg --usb-dir usb --usb-dir-work run/work \
    --cart-slot "crts/s08-easyflash.crt,save=$rundir/run/written.crt" || fail write "exit code $?"
grep -q "ROM_WR_EN did not" run/write.log && fail write "the write lock did not follow"
same usb/flashme.crt run/written.crt || fail write "the cartridge does not hold the written image"
echo "PASS write"

rm -rf "$rundir"
echo "all TRX S43 smoke tests passed"
