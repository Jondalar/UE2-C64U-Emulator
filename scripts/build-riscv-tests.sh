#!/usr/bin/env bash
# Build the official riscv-tests rv32ui-p-* and rv32um-p-* ELFs for crates/rv32/tests/riscv_tests.rs.
#
# Prereqs (not tracked in git):
#   tools/riscv-tests   clone of riscv-software-src/riscv-tests with its env submodule initialised
#   tools/bin           riscv32-unknown-elf-* symlinks to xPack riscv-none-elf-gcc 11.3.0-1 darwin-arm64
# A git worktree without its own tools/ uses the main checkout's. Override with TOOLS=<dir>.
# The checkout is only read: sources compile in place with -I/-T pointing at env/p and isa/macros/scalar.
#
# Output: target/riscv-tests/<suite>-p-<test> (RISCV_TESTS_DIR overrides it, as it does for the test harness)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "${TOOLS:-}" ]; then
    TOOLS="$ROOT/tools"
    if [ ! -d "$TOOLS" ]; then
        COMMON="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
        [ -n "$COMMON" ] && TOOLS="$(dirname "$COMMON")/tools"
    fi
fi
SRC="$TOOLS/riscv-tests"
OUT="${RISCV_TESTS_DIR:-$ROOT/target/riscv-tests}"
CC="$TOOLS/bin/riscv32-unknown-elf-gcc"

[ -x "$CC" ] || { echo "riscv32-unknown-elf-gcc not found in $TOOLS/bin"; exit 1; }
[ -f "$SRC/env/p/riscv_test.h" ] || { echo "riscv-tests (with env submodule) not found at $SRC"; exit 1; }

# rvlite is RV32I + Zicsr + M (docs/hw/01-cpu-boot-memory.md); fence_i also needs Zifencei. GCC 11 (ISA spec 2.2)
# still folds both into rv32im, newer toolchains need them spelled out.
PROBE="$(mktemp -d)"
trap 'rm -rf "$PROBE"' EXIT
printf 'csrr a0, mstatus\nfence.i\n' > "$PROBE/probe.S"
MARCH=
for m in rv32im_zicsr_zifencei rv32im_zicsr rv32im; do
    if "$CC" -march="$m" -mabi=ilp32 -c "$PROBE/probe.S" -o "$PROBE/probe.o" 2>/dev/null; then
        MARCH="$m"
        break
    fi
done
[ -n "$MARCH" ] || { echo "$CC accepts none of the rv32im -march variants"; exit 1; }

# Same options as isa/Makefile compile_template for the p environment.
CFLAGS=(-march="$MARCH" -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib -nostartfiles
        -I"$SRC/env/p" -I"$SRC/isa/macros/scalar" -T"$SRC/env/p/link.ld")

mkdir -p "$OUT"
count=0
for suite in rv32ui rv32um; do
    # Test names: the `<suite>_sc_tests = \` continuation list in the suite's Makefrag.
    names="$(awk -v var="${suite}_sc_tests" '
        $1 == var { on = 1; next }
        on { cont = /\\[[:space:]]*$/; gsub(/\\/, ""); print; if (!cont) exit }' "$SRC/isa/$suite/Makefrag")"
    for t in $names; do
        "$CC" "${CFLAGS[@]}" "$SRC/isa/$suite/$t.S" -o "$OUT/$suite-p-$t"
        count=$((count + 1))
    done
done
echo "built $count riscv-tests ELFs (-march=$MARCH) in $OUT"
