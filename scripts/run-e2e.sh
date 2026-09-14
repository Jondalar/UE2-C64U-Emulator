#!/bin/bash
# Run the upstream E2E suite (firmware/1541ultimate/tests, ./run-tests) against the emulator.
# Results and triage: docs/status/e2e.md.
#
#   scripts/run-e2e.sh [PROFILE [RUN-TESTS ARGS...]]   boot, run the profile (default smoke), report, quit
#   scripts/run-e2e.sh up                              boot and wait for REST, leave the emulator running
#   scripts/run-e2e.sh down                            quit the emulator started by `up`
#
# The emulator runs headless and realtime with --net user. REST 80, FTP 21, Telnet 23 and the DMA port 64 are
# forwarded from 127.0.0.1:18080, 18021, 18023 and 18064, and the matching U64_*_PORT variables are exported for
# the suites (tests/lib/targets.py:77-80). macOS refuses an unprivileged bind of a port below 1024 on 127.0.0.1
# (EACCES) and allows it only on 0.0.0.0, which would put the device on the LAN. Set E2E_REST_PORT, E2E_FTP_PORT,
# E2E_TELNET_PORT or E2E_DMA_PORT to move a host port. FTP passive ports are forwarded 1:1, because ftplib
# connects the data channel to the control connection's address with the port from the PASV reply. The firmware
# hands them out in sequence from 51000 after boot and wraps at 61000 (ftpd.cc:324-327,407-410), but that range
# lies in the macOS ephemeral range, where some port is usually taken and one failed hostfwd stops the machine.
# Only the first 2000 are forwarded, so one boot serves 2000 passive transfers.
#
# E2E_REST_SHIM=1 puts a sitecustomize.py on PYTHONPATH that sends connections to 127.0.0.1:80 to the REST
# forward instead. Six suites build their REST URLs without the port override (docs/status/e2e.md), so without
# it they only show "connection refused"; with it they show what the emulator does. A shimmed run writes to
# runs/<profile>-shim.
#
# Everything lives under $E2E_OUT (default run/e2e): the venv with tests/requirements.txt, the SD image
# (scripts/make-sd-image.sh), the persistent flash image, the firmware console (emu-<run>.log, stderr in
# emu-<run>.err) and the run-tests output tree (runs/<run>/127.0.0.1/, plus index.md from tools/e2e_report.py).
# The firmware tree is not written to.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FW=${UE2_FIRMWARE:-$ROOT/firmware/1541ultimate}
OUT=${E2E_OUT:-$ROOT/run/e2e}
REST_PORT=${E2E_REST_PORT:-18080}
FTP_PORT=${E2E_FTP_PORT:-18021}
TELNET_PORT=${E2E_TELNET_PORT:-18023}
DMA_PORT=${E2E_DMA_PORT:-18064}
CONTROL=127.0.0.1:${E2E_CONTROL_PORT:-16400}
PASV_FIRST=51000
PASV_LAST=52999
BOOT_TIMEOUT=120
EMU=$ROOT/target/release/ue2emu
VENV=$OUT/venv
HOST=127.0.0.1

export UE2_FIRMWARE=$FW
export U64_REST_PORT=$REST_PORT U64_FTP_PORT=$FTP_PORT U64_TELNET_PORT=$TELNET_PORT U64_DMA_PORT=$DMA_PORT
# The lint suite runs ruff over the tree, which otherwise leaves .ruff_cache in the firmware checkout.
export PYTHONDONTWRITEBYTECODE=1 RUFF_CACHE_DIR=$OUT/ruff-cache

prepare() {
    if [ ! -f "$FW/run-tests" ]; then
        echo "run-e2e: no firmware tree at $FW (set UE2_FIRMWARE)" >&2
        exit 1
    fi
    mkdir -p "$OUT"
    (cd "$ROOT" && cargo build --release --quiet)
    if [ ! -f "$VENV/installed" ]; then
        python3 -m venv "$VENV"
        "$VENV/bin/pip" install --quiet --disable-pip-version-check -r "$FW/tests/requirements.txt"
        touch "$VENV/installed"
    fi
    [ -f "$OUT/sd.img" ] || "$ROOT/scripts/make-sd-image.sh" "$OUT/sd.img"
}

forwards() {
    local list="tcp:$REST_PORT:80,tcp:$FTP_PORT:21,tcp:$TELNET_PORT:23,tcp:$DMA_PORT:64" port
    for ((port = PASV_FIRST; port <= PASV_LAST; port++)); do
        list+=",tcp:$port:$port"
    done
    echo "$list"
}

rest_up() {
    curl -fsS -m 2 "http://$HOST:$REST_PORT/v1/version" >/dev/null 2>&1
}

# up NAME: boot with the console in emu-NAME.log and wait for REST.
up() {
    if [ -f "$OUT/emu.pid" ] && kill -0 "$(cat "$OUT/emu.pid")" 2>/dev/null; then
        echo "run-e2e: emulator already running (pid $(cat "$OUT/emu.pid"))" >&2
        exit 1
    fi
    prepare
    nohup "$EMU" run --headless --speed realtime \
        --firmware "$FW/target/u64ii/riscv/ultimate/result/ultimate.elf" --roms "$FW/roms" \
        --flash "$OUT/flash.bin" --sd "$OUT/sd.img" \
        --net user --hostfwd "$(forwards)" --control "$CONTROL" \
        >"$OUT/emu-$1.log" 2>"$OUT/emu-$1.err" </dev/null &
    echo $! >"$OUT/emu.pid"
    local waited=0
    until rest_up; do
        if ! kill -0 "$(cat "$OUT/emu.pid")" 2>/dev/null; then
            echo "run-e2e: emulator exited during boot, see $OUT/emu-$1.err" >&2
            exit 1
        fi
        if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
            echo "run-e2e: REST did not answer on $HOST:$REST_PORT within ${BOOT_TIMEOUT}s" >&2
            down
            exit 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "run-e2e: emulator up (pid $(cat "$OUT/emu.pid")), REST answered after ${waited}s" >&2
}

# `quit` on the control port, so the flash image is flushed (docs/status/storage.md, Known gaps).
down() {
    [ -f "$OUT/emu.pid" ] || return 0
    local pid waited=0
    pid=$(cat "$OUT/emu.pid")
    printf 'quit\n' | nc -w 5 "${CONTROL%:*}" "${CONTROL#*:}" >/dev/null 2>&1 || true
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 10 ]; do
        sleep 1
        waited=$((waited + 1))
    done
    kill "$pid" 2>/dev/null || true
    rm -f "$OUT/emu.pid"
}

shim() {
    mkdir -p "$OUT/shim"
    cat >"$OUT/shim/sitecustomize.py" <<'EOF'
"""Written by scripts/run-e2e.sh (E2E_REST_SHIM=1): 127.0.0.1:80 goes to the REST forward, U64_REST_PORT."""
import os
import socket

_rest_port = int(os.environ["U64_REST_PORT"])
_create_connection = socket.create_connection


def create_connection(address, *args, **kwargs):
    if address[0] in ("127.0.0.1", "localhost") and int(address[1]) == 80:
        address = (address[0], _rest_port)
    return _create_connection(address, *args, **kwargs)


socket.create_connection = create_connection
EOF
    export PYTHONPATH="$OUT/shim${PYTHONPATH:+:$PYTHONPATH}"
}

case "${1:-smoke}" in
up)
    up manual
    ;;
down)
    down
    ;;
*)
    PROFILE=${1:-smoke}
    [ $# -gt 0 ] && shift
    RUN=$PROFILE
    if [ "${E2E_REST_SHIM:-0}" = 1 ]; then
        shim
        RUN=$PROFILE-shim
    fi
    up "$RUN"
    trap down EXIT
    status=0
    rm -rf "$OUT/runs/$RUN"
    "$VENV/bin/python" "$FW/run-tests" --profile "$PROFILE" -o "$OUT/runs/$RUN" "$@" "$HOST" || status=$?
    "$VENV/bin/python" "$FW/tools/e2e_report.py" "$OUT/runs/$RUN" >/dev/null || true
    echo "run-e2e: run-tests exited $status; report $OUT/runs/$RUN/index.md" >&2
    exit "$status"
    ;;
esac
