#!/usr/bin/env bash
# S44 S11: the MultiCore host app starts a .cor and takes it back (docs/specs/S44-multicore-cores.md §7). Stops at the first
# failure and exits non-zero.
#
#   scripts/smoke-s44-host.sh KIT     (TRX64U-MultiCore's emulator kit, not in this repo: hostapp_emu.elf built with
#                                      HOSTAPP_EMU_FLASH, coreapp.elf, vc20.cor (a DFX core) and amiga1200.cor (a large one))
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate); the flash starts from run/flash.bin every run. The cores get
# zero-filled ROM files on the stick, so their application releases the core and its hotkey opens the menu.
#
# Runs:
#   vc20     F3 > USB0 > cores > vc20.cor > S: the partial is found by its SHA-256, coreapp identifies VC20; CTRL+RESTORE,
#            F8, "Return to host menu": IPROG WBSTAR 0, the host app boots again and takes the menu marker
#   a1200    the same with amiga1200.cor: IPROG to its flash file, the boot magic; C=+RUN/STOP+RESTORE, back to the host
#   control  the vc20 run without --core-dir: the run must halt (no profile for the core)

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
kit=${1:-}
roms=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}/roms
for f in hostapp_emu.elf coreapp.elf vc20.cor amiga1200.cor; do
    if [[ -z $kit || ! -f $kit/$f ]]; then
        echo "usage: $0 KIT (a directory with hostapp_emu.elf, coreapp.elf, vc20.cor, amiga1200.cor)" >&2
        exit 2
    fi
done
kit=$(cd "$kit" && pwd)

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-s44-host.XXXXXX")
cd "$rundir"
echo "run directory: $rundir"

mkdir -p cors profiles stick/cores/vc20/roms stick/cores/amiga1200/roms
cp "$kit/vc20.cor" "$kit/amiga1200.cor" cors/
cp "$kit/vc20.cor" "$kit/amiga1200.cor" stick/cores/
cp "$kit/coreapp.elf" profiles/
cp "$repo/docs/examples/core-vic20.toml" profiles/VIC-20.toml
sed 's|^name = "amiga1200"|name = "amiga1200"\nsymbols = "coreapp.elf"|' "$repo/docs/examples/core-amiga1200.toml" >profiles/amiga1200.toml
head -c 8192 /dev/zero >stick/cores/vc20/roms/basic.bin
head -c 8192 /dev/zero >stick/cores/vc20/roms/kernal.bin
head -c 4096 /dev/zero >stick/cores/vc20/roms/char.bin
head -c 524288 /dev/zero >stick/cores/amiga1200/roms/kick31.rom

fail() {
    echo "FAIL $1: $2"
    grep -a -E "icap|halted|\[core\]|returning|direct boot" "$1.log" | tail -n 20 || true
    echo "kept: $rundir"
    exit 1
}

# The host's file browser: past the countdown, F3, USB0, into cores; the list is .., amiga1200/, vc20/, amiga1200.cor, vc20.cor.
nav() {
    printf 'wait 15000\nkey space\nwait 800\nkey f3\nwait 1500\nkey down\nwait 500\nkey down\nwait 500\nkey return\nwait 2000\n'
    printf 'key down\nwait 500\nkey return\nwait 2000\n'
    for ((i = 0; i < $1; i++)); do printf 'key down\nwait 500\n'; done
    printf 'key s\nwait %s\n' "$2"
}
back() {   # the hotkey's modifier, RESTORE, F8 System, the last entry
    printf 'hold %s\nwait 300\nrestore\nwait 300\nrelease %s\nwait 2000\nkey f8\nwait 1500\n' "$1" "$1"
    for ((i = 0; i < 15; i++)); do printf 'key down\nwait 300\n'; done
    printf 'key return\nwait 30000\npng %s-host.png\nquit\n' "$2"
}

# run NAME ARGS...: the host app headless at full speed, the script NAME.ctl
run() {
    local name=$1
    shift
    cp "$repo/run/flash.bin" "$name-flash.bin"
    rm -rf usbwork
    nice -n 15 "$bin" run --board c64u --trx-io=on --icap --cor-dir cors --firmware "$kit/hostapp_emu.elf" --roms "$roms" \
        --flash "$name-flash.bin" --usb-dir stick --usb-dir-work usbwork --headless --speed max --script "$name.ctl" \
        --max-seconds 380 "$@" >"$name.log" 2>&1
}

check_round() {   # NAME CORE-LINE: started, identified, back with the marker
    local name=$1 core=$2
    grep -q "phase core identified: \[core\] $core" "$name.log" || fail "$name" "coreapp did not identify $core"
    grep -q "^icap: IPROG WBSTAR 0: the host bitstream, .* the menu marker set" "$name.log" || fail "$name" "no IPROG back to the host"
    grep -q "direct boot: decision .*marker 1" "$name.log" || fail "$name" "the host did not take the menu marker"
}

{ nav 4 30000; back ctrl vc20; } >vc20.ctl
run vc20 --core-dir profiles || fail vc20 "exit code $?"
grep -q '^icap: the partial of "VIC-20"' vc20.log || fail vc20 "the partial was not found"
check_round vc20 "VC20 VIC-20, cor /USB0/cores/vc20.cor"
echo "PASS vc20"

{ nav 3 60000; back cbm+runstop a1200; } >a1200.ctl
run a1200 --core-dir profiles || fail a1200 "exit code $?"
grep -q '^icap: IPROG WBSTAR 0x[0-9a-f]*: the large core "Amiga 1200 (AGA, 68020)" .*boot magic: 0x00030000' a1200.log ||
    fail a1200 "no IPROG into the large core"
check_round a1200 "AMIG Amiga 1200 (AGA, 68020), cor /USB0/cores/amiga1200.cor"
echo "PASS a1200"

cp vc20.ctl control.ctl
if run control; then
    fail control "the run without --core-dir did not halt"
fi
grep -q 'halted: core "VIC-20": no --core-dir to find its profile in' control.log || fail control "not halted for the missing profile"
echo "PASS control (halts as it must)"

cd /
rm -rf "$rundir"
echo "all S44 host smoke tests passed"
