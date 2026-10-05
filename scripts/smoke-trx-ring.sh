#!/usr/bin/env bash
# S40: the board's engine under TRX64-Ultimate's trx64/main app (docs/specs/S40-trx-engine.md §14): the ring, REST
# rewind and scrub, the Timewarp menu. Builds the release binary and runs scripts/smoke-trx-ring.py.
#
#   scripts/smoke-trx-ring.sh [--elf ELF]     (default run/trx/ultimate-a58f1a6e.elf)

set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$repo/Cargo.toml"
exec python3 "$repo/scripts/smoke-trx-ring.py" "$@"
