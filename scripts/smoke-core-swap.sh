#!/usr/bin/env bash
# S41 §5: the core swapped while the RC1 app runs (docs/specs/S41-core-dev-mode.md). The app has no code for a swap
# yet, so the smoke asserts what the emulator does and only logs how the unmodified app reacts. Stops at the first
# failure and exits non-zero.
#
#   scripts/smoke-core-swap.sh [ELF]     (default run/trx/ultimate-3c8e4aa.elf, the RC1 app)
#
# ROMs come from $UE2_FIRMWARE (default firmware/1541ultimate), each run has a fresh flash image.
#
# `core absent` is the host's decoupler (`core_decouple` = 1, ultimate_sys.vhd a75c356): on its rise the latches behind
# the cart registers and the core config go back to power-up once, with the C64's reset held, and the firmware senses
# the reset (ITU low bit 7); bytes that read back from the page RAM keep their values; the core's windows answer ack0
# at once. `core present` loads a fresh core that stays in reset until
# the firmware releases it, as at power-up, and raises nothing. `--absent-mode core=noack` is a host without the
# decoupler, where an access into the core never completes and nothing is reset.
#
# Runs:
#   swap    the decoupler, twice: right after `core absent` C64_MODE reads the reset held ($04), C64_STOP 0, the
#           reset sensed ($11) and the cartridge type 0, every other cart register byte as before; the app's reset
#           handler runs (the reset sense); after `core present` the
#           reset is still held and the host's pages 1, 2, 4 and 5 (MATRIX_KEYB, the U64 board IO, C64_PLD_ACC,
#           U64_DEBUG_REGISTER) read what they read before the swap; `monitor c64 reset`, standing in for the app's
#           release (it writes C64_MODE as the firmware does), boots the fresh C64 to READY
#   rest    the decoupler with the network: the REST API answers while the core is absent, and after
#           `core present` `PUT /v1/machine:reset` (the app's own reset path) boots the new core to READY
#   log     core=noack with --absent-continue: the C64 screen is empty and the picture black without the core, every
#           access of the app into the core's windows is logged and counted (the app's reaction, printed)
#   halt    core=noack: `core absent` halts the RISC-V on its first access into the core's windows, with the report
#           (the board without the decoupler and before bc97bbe's timeout, without the JTAG console)
#   start   core=noack, --absent core: absent from the first instruction; the app halts in its init on the first core
#           window it touches; the cart registers and the core config, which ultimate_sys acks itself, are not one of
#           them
#   boot    the decoupler, --absent core: absent from the first instruction; the app runs its whole init, the C64 held
#           in reset (C64_MODE 04 in its cart init line)

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

pages='monitor fw m 10100300 1010030f
monitor fw m 10100400 1010040f
monitor fw m 10181000 10181003
monitor fw m 10181800 10181803'
cart='monitor fw m 10040000 1004000f'
swap="$booted
$pages
$cart
core absent
$cart
expect-console \"U64 reset handler\" 2000
wait 2000
core present
$cart
$pages
monitor c64 reset
expect-c64 READY. 8000
c64screen
png present.png
core absent
wait 1000
core present
monitor c64 reset
expect-c64 READY. 8000
wait 500"
run swap "$swap" || fail swap "exit code $?"
[[ $(grep -c "^core absent (ack0): " swap.err) == 2 && $(grep -c "^core present: a fresh core" swap.err) == 2 ]] ||
    fail swap "not two swaps"
carts=$(grep "^  10040000 " swap.log)
# Before the swap, then on entry: bytes 0, 1, 3, 5 and 6 read latches (reset held, no stop, reset sensed, no
# cartridge, not active), the rest the page RAM.
want=$(sed -n 1p <<<"$carts" | awk '{ $2 = "04"; $3 = "00"; $5 = "11"; $7 = "00"; $8 = "00"; print }' |
    sed 's/^10040000 /  10040000  /')
[[ $(sed -n 2p <<<"$carts") == "$want" ]] ||
    fail swap "on entry the cart registers read $(sed -n 2p <<<"$carts"), not $want"
[[ $(sed -n 3p <<<"$carts" | cut -c13-14) == 04 ]] || fail swap "the fresh C64 is not held in reset after core present"
for page in 10100300 10100400 10181000 10181800; do
    [[ $(grep -c "^  $page " swap.log) == 2 && $(grep "^  $page " swap.log | sort -u | wc -l) -eq 1 ]] ||
        fail swap "host page $page changed over the swap"
done
c64 swap 1 | grep -q "READY." || fail swap "the fresh C64 did not boot after its release"
echo "PASS swap: on entry $(sed -n 2p <<<"$carts" | cut -c13-), the app's reset handler ran; held in reset after"
echo "     core present; pages 1, 2, 4, 5 kept; READY after the release, twice"

# rest: a swap with the network on, driven over the control port while the REST API is called.
python3 - "$bin" "$elf" "$roms" "$repo/scripts" <<'PY' >rest.out 2>&1 || fail rest "$(tail -n 5 rest.out)"
import socket, subprocess, sys, time
bin_, elf, roms, scripts = sys.argv[1:5]
sys.path.insert(0, scripts)
import cartslot_common as cs

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

http, ctl = free_port(), free_port()
proc = subprocess.Popen([bin_, 'run', '--headless', '--speed', 'max', '--board', 'c64u', '--firmware', elf, '--roms', roms,
                         '--flash', 'rest-flash.bin', '--c64-roms', '--net', 'user', '--hostfwd',
                         f'tcp:127.0.0.1:{http}:80', '--control', f'127.0.0.1:{ctl}'],
                        stdout=open('rest.log', 'wb'), stderr=open('rest.err', 'wb'))
deadline = time.time() + 60
while True:
    try:
        c = cs.Control(f'127.0.0.1:{ctl}')
        break
    except OSError:
        if time.time() > deadline or proc.poll() is not None:
            sys.exit('no control port')
        time.sleep(0.2)
rest = cs.Rest(f'http://127.0.0.1:{http}')
try:
    c.cmd('expect-console "[TRX UI] splash done; UI parked" 15000')
    rest.wait_ready(120)
    c.cmd('core absent')
    info = rest.request('GET', '/v1/info')
    c.cmd('wait 1000')
    c.cmd('core present')
    mode = c.cmd('monitor fw m 10040000 10040000')
    if not mode or mode[0].split()[-1] != '04':
        sys.exit(f'not held in reset after core present: {mode}')
    rest.request('PUT', '/v1/machine:reset', idempotent=False)
    c.cmd('expect-c64 READY. 8000')
    print(f'REST answered while the core was absent ({len(info)} bytes of /v1/info); after core present '
          'PUT /v1/machine:reset booted the new core to READY')
    c.cmd('quit')
finally:
    c.close()
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
PY
echo "PASS rest: $(tail -n 1 rest.out)"

log="$booted
core absent
wait 3000
c64screen
png absent.png
core present
wait 500"
run log "$log" --absent-mode core=noack --absent-continue || fail log "exit code $?"
[[ -z $(c64 log 1 | tr -d ' \n') ]] || fail log "the C64 screen is not empty without the core"
black absent.png || fail log "the picture is not black without the core"
grep -q "^absent: core noack " log.err || fail log "the app touched none of the core's windows"
grep -q "^absent summary: [1-9][0-9]* addresses stalled" log.err || fail log "no summary"
echo "PASS log: the app's accesses into the absent core:"
sed -n '/^absent summary: /,$p' log.err | grep -E "^absent summary|^  0x" | head -n 40

if run halt "$booted
core absent
wait 3000" --absent-mode core=noack; then
    fail halt "the run did not halt"
fi
grep -q "^halted: absent core (noack): " halt.err || fail halt "no halt on the core's window"
echo "PASS halt: $(sed -n 's/^halted: //p' halt.err | head -n 1)"
grep -E "^  (pc |ra |last console line:)" halt.err | head -n 3

if run start "$booted" --absent core --absent-mode core=noack; then
    fail start "the run did not halt"
fi
grep -q "^core absent (noack): " start.err && grep -q "^halted: absent core (noack): " start.err ||
    fail start "the core was not absent from the start"
grep -qE "^halted: absent core \(noack\): .* of 0x(1018[0-7]|10040)" start.err && fail start "halted on a window ultimate_sys acks itself"
echo "PASS start: $(sed -n 's/^halted: //p' start.err | head -n 1)"
grep -E "^  (pc |last console line:)" start.err | head -n 2

run boot 'expect-console "All Init functions called." 20000
wait 3000' --absent core || fail boot "exit code $?"
grep -q "^core absent (ack0): " boot.err || fail boot "the core was not absent from the start"
grep -q "Mode: 04" boot.log || fail boot "the C64 is not held in reset"
echo "PASS boot: $(grep -m1 "^End of cart init" boot.log)"

cd /
rm -rf "$rundir"
echo "all core swap smoke tests passed"
