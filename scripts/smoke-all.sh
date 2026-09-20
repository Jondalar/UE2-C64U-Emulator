#!/usr/bin/env bash
# Build ue2emu, then run every smoke script headless at --speed max in a fresh temporary run directory. Stops at
# the first failure, prints that run's log tail, and exits non-zero.
#
#   scripts/smoke-all.sh
#
# Firmware: $UE2_FIRMWARE (default: firmware/1541ultimate under the repo root) must hold
# target/u64ii/riscv/ultimate/result/ultimate.elf and roms/. The run directory (logs, SD image, flash images,
# PNGs) is removed after a pass and kept after a failure. scripts/make-sd-image.sh needs a macOS login session.
#
# Runs, in order:
#   menu        smoke-menu.ctl      --flash run/flash.bin (seeded on first use)
#   sd          smoke-sd.ctl        --flash run/flash.bin --sd run/sd.img
#   flash-1     smoke-flash-1.ctl   --flash run/flash-ui.bin (fresh: Color Scheme must not be C128 Style yet)
#   flash-2     smoke-flash-2.ctl   --flash run/flash-ui.bin --no-overlay-ui
#   settings    smoke-settings.ctl  --flash run/flash-settings.bin (fresh) --settings smoke-settings.cfg
#   monitor     smoke-monitor.ctl   --flash run/flash-monitor.bin (fresh) --settings smoke-settings.cfg
#   uci         smoke-uci.ctl       --flash run/flash-uci.bin (fresh) --c64-roms --settings smoke-uci.cfg
#                                   --usb-dir run/uci (uci-probe.prg from make-uci-probe.py)
#   negative    an expect that cannot match; passes only when ue2emu exits non-zero and names its line

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
fw=${UE2_FIRMWARE:-$repo/firmware/1541ultimate}
elf=$fw/target/u64ii/riscv/ultimate/result/ultimate.elf
if [[ ! -f $elf || ! -f $fw/roms/chars.bin ]]; then
    echo "firmware not found: need $elf and $fw/roms (set UE2_FIRMWARE)" >&2
    exit 2
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
bin=${CARGO_TARGET_DIR:-$repo/target}/release/ue2emu

rundir=$(mktemp -d "${TMPDIR:-/tmp}/ue2-smoke.XXXXXX")
cd "$rundir"
mkdir run
echo "run directory: $rundir"

# emu <name> <script> [ue2emu run options]: run one script headless; stdout to run/<name>.log, stderr to
# run/<name>.err; returns ue2emu's exit code.
emu() {
    local name=$1 script=$2
    shift 2
    "$bin" run --headless --speed max --firmware "$elf" --roms "$fw/roms" "$@" --script "$script" \
        >"run/$name.log" 2>"run/$name.err"
}

fail() {
    local name=$1
    shift
    echo "FAIL $name: $*"
    echo "--- run/$name.log (last 40 lines)"
    tail -n 40 "run/$name.log"
    echo "--- run/$name.err"
    cat "run/$name.err"
    echo "kept: $rundir"
    exit 1
}

# smoke <name> <script> [ue2emu run options]: run a script that must pass.
smoke() {
    local name=$1 start=$SECONDS rc=0
    emu "$@" || rc=$?
    ((rc == 0)) || fail "$name" "exit code $rc"
    # The emulation thread's stats line on stderr: "<n> instructions, <t> s emulated, ...".
    local emulated
    emulated=$(grep -o '[0-9.]* s emulated' "run/$name.err" | tail -n 1)
    echo "PASS $name (${emulated:-? s emulated}, $((SECONDS - start)) s wall)"
}

smoke menu "$repo/scripts/smoke-menu.ctl" --flash run/flash.bin
"$repo/scripts/make-sd-image.sh" run/sd.img
smoke sd "$repo/scripts/smoke-sd.ctl" --flash run/flash.bin --sd run/sd.img
smoke flash-1 "$repo/scripts/smoke-flash-1.ctl" --flash run/flash-ui.bin
smoke flash-2 "$repo/scripts/smoke-flash-2.ctl" --flash run/flash-ui.bin --no-overlay-ui
smoke settings "$repo/scripts/smoke-settings.ctl" --flash run/flash-settings.bin \
    --settings "$repo/scripts/smoke-settings.cfg"
smoke monitor "$repo/scripts/smoke-monitor.ctl" --flash run/flash-monitor.bin \
    --settings "$repo/scripts/smoke-settings.cfg"
# The config cycle must have left the new value in the pages, which `config` reads back out of the flash.
grep -q 'REU Size=2 MB   (flash)' run/monitor.log || fail monitor "the config pages do not hold the new REU size"

mkdir run/uci
python3 "$repo/scripts/make-uci-probe.py" run/uci/uci-probe.prg
smoke uci "$repo/scripts/smoke-uci.ctl" --flash run/flash-uci.bin --c64-roms --settings "$repo/scripts/smoke-uci.cfg" \
    --usb-dir run/uci --usb-dir-work run/uci-work

printf '# must fail\nexpect "NO SUCH TEXT ON THE SCREEN" 500\nquit\n' >run/negative.ctl
rc=0
emu negative run/negative.ctl --flash run/flash.bin || rc=$?
((rc != 0)) || fail negative "a failing expect exited 0"
grep -q 'line 2' "run/negative.err" || fail negative "the error does not name the failing line"
echo "PASS negative (exit code $rc)"

cd /
rm -rf "$rundir"
echo "all smoke tests passed"
