#!/usr/bin/env bash
# Build the U64-II (UE2 / C64U) ultimate firmware ELF natively on macOS arm64.
#
# Prereqs (not tracked in git):
#   firmware/1541ultimate   clone of GideonZ/1541ultimate (submodules neorv32, software/lwip, software/httpd)
#   tools/bin               riscv32-unknown-elf-* symlinks to xPack riscv-none-elf-gcc 11.3.0-1 darwin-arm64
#
# Output: firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FW="${FW:-$ROOT/firmware/1541ultimate}"
JOBS="${JOBS:-8}"
export PATH="$ROOT/tools/bin:$PATH"

command -v riscv32-unknown-elf-gcc >/dev/null || { echo "riscv32-unknown-elf-gcc not found (tools/bin)"; exit 1; }

# Host tools may have been built for another OS in the same tree (e.g. a Linux container build leaves ELF
# binaries that macOS cannot execute, "Error 126"). If any existing tool does not match this host, rebuild them all.
host_binary_ok() {
    local kind
    kind="$(file -b "$1")"
    case "$(uname -s)" in
        Darwin) [[ $kind == Mach-O* ]] ;;
        Linux)  [[ $kind == ELF* ]] ;;
        *)      true ;;
    esac
}
for tool in bin2hex hex2bin make_array make_mem makeappl promgen checksum dump_vcd dump_bus_trace dump_rtos_trace \
            swap svf_dump 64tass/64tass; do
    if [[ -f "$FW/tools/$tool" ]] && ! host_binary_ok "$FW/tools/$tool"; then
        echo "build-firmware: tools/$tool was built for another OS ($(file -b "$FW/tools/$tool" | cut -c1-40)); rebuilding host tools"
        make -C "$FW/tools" clean
        break
    fi
done
make -C "$FW/tools"
# Pre-create output dirs and pre-build 6502 payloads: the upstream Makefiles race under -j.
for d in target/libs/riscv/lwip target/u64ii/riscv/ultimate; do
    mkdir -p "$FW/$d/output" "$FW/$d/result"
done
make -C "$FW/software/6502/sidcrt"
# AR must be the cross ar: macOS ar writes BSD archives (#1/NN long names) GNU ld cannot read.
make -C "$FW/target/libs/riscv/lwip" -j"$JOBS" AR=riscv32-unknown-elf-ar
make -C "$FW/target/u64ii/riscv/ultimate" -j"$JOBS"

ls -la "$FW/target/u64ii/riscv/ultimate/result/"
