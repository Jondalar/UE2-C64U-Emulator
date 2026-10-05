#!/usr/bin/env bash
# S39: trxmon (TRX64-Ultimate's monitor app) on the board's debug block (docs/specs/S39-trxmon.md §9). Builds the
# release binary and runs scripts/smoke-trxmon.py with the t21-p4-app firmware, trxmon.u2a and trxmon_client.py.
#
#   scripts/smoke-trxmon.sh [--elf ELF] [--u2a trxmon.u2a] [--client trxmon_client.py]

set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$repo/Cargo.toml"
exec python3 "$repo/scripts/smoke-trxmon.py" "$@"
