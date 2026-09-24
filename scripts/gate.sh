#!/usr/bin/env bash
# The release gate (docs/specs/S35-release-gate.md): everything that has to pass before a minor or a major release.
# Stops at the first failing part.
#
#   scripts/gate.sh
#
# Needs the firmware build ($UE2_FIRMWARE, default firmware/1541ultimate), a macOS login session for the SD images,
# and the ports the smokes use (6400, 8080, 18021-18080).

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
start=$SECONDS

part() {
    local name=$1 t=$SECONDS
    shift
    echo "=== $name"
    if ! "$@"; then
        echo "GATE FAILED: $name"
        exit 1
    fi
    echo "GATE PASS $name ($((SECONDS - t)) s)"
}

part "workspace tests" cargo test --workspace --all-features -q
part "firmware smokes" scripts/smoke-all.sh
part "C64 smokes" scripts/smoke-c64-all.sh
part "usb-dir" scripts/smoke-usb-dir.sh
part "upstream e2e smoke" env E2E_REST_SHIM=1 scripts/run-e2e.sh smoke

echo "GATE PASSED ($((SECONDS - start)) s)"
