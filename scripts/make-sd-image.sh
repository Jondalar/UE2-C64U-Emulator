#!/usr/bin/env bash
# Create a FAT32 SD card image for `ue2emu run --sd <path>` on macOS, without root.
#
#   scripts/make-sd-image.sh <path> [sizeMB]      (default 64 MB, minimum 40 MB)
#
# Layout: an MBR with one FAT32-LBA partition (type 0x0C) at sector 2048, like a card formatted by a camera
# or SD Card Formatter. The firmware walks the MBR table in Disk::Init (filesystem/disk.cc:100-117). The size
# is whole MiB, so the SDHC CSD reports it exactly (docs/hw/07 §CSD).
#
# Sample files in the root directory:
#   hello.prg   BASIC: 10 PRINT"HELLO FROM UE2EMU"
#   demo.d64    35-track D64 ("UE2EMU DEMO") holding HELLO as a PRG, built byte by byte
#   readme.txt  plain text
#
# Tools: dd, xxd, hdiutil (attach the raw file as a disk without mounting), newfs_msdos (format the
# partition node, which belongs to the attaching user), diskutil (mount at a private mount point, copy,
# unmount), all stock macOS. The partition is never mounted under /Volumes.

set -euo pipefail

usage() {
    echo "usage: $0 <image path> [sizeMB (>= 40, default 64)]" >&2
    exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
image=$1
size_mb=${2:-64}
[[ $size_mb =~ ^[0-9]+$ ]] && ((size_mb >= 40)) || usage

sectors=$((size_mb * 2048))
part_start=2048
part_sectors=$((sectors - part_start))

# Write hex bytes (whitespace allowed) into $1 at byte offset $2.
poke() {
    local file=$1 offset=$2
    shift 2
    echo "$*" | xxd -r -p | dd of="$file" bs=1 seek="$offset" conv=notrunc status=none
}

# 32-bit value as little-endian hex bytes.
le32() {
    local v
    v=$(printf '%08x' "$1")
    echo "${v:6:2} ${v:4:2} ${v:2:2} ${v:0:2}"
}

# ASCII string as hex bytes; with $2 = pad byte (hex) and $3 = length, padded to that many bytes.
text() {
    local hex
    hex=$(printf '%s' "$1" | xxd -p | tr -d '\n')
    if (($# == 3)); then
        while ((${#hex} < $3 * 2)); do hex+=$2; done
    fi
    echo "$hex"
}

work=$(mktemp -d "${TMPDIR:-/tmp}/ue2-sd.XXXXXX")
dev=
mounted=
cleanup() {
    if [[ -n $mounted ]]; then diskutil unmount "$work/mnt" >/dev/null 2>&1 || true; fi
    if [[ -n $dev ]]; then hdiutil detach "$dev" -quiet >/dev/null 2>&1 || true; fi
    rm -rf "$work"
}
trap cleanup EXIT

# --- sample files -------------------------------------------------------------------------------------------
files=$work/files
mkdir -p "$files"

# PRG: load address $0801, line 10 at $0801 linking to $081A, PRINT token $99, quoted text, end of program.
prg="01 08 1a 08 0a 00 99 22 $(text 'HELLO FROM UE2EMU' '' 17) 22 00 00 00"
echo "$prg" | xxd -r -p >"$files/hello.prg"
prg_len=$(wc -c <"$files/hello.prg" | tr -d ' ')

# D64: 683 sectors of 256 bytes. Track t starts at sector sum(sectors of tracks < t).
d64=$files/demo.d64
dd if=/dev/zero of="$d64" bs=256 count=683 status=none
track_sectors() {
    local t=$1
    if ((t <= 17)); then echo 21; elif ((t <= 24)); then echo 19; elif ((t <= 30)); then echo 18; else echo 17; fi
}
# BAM, track 18 sector 0: link to 18/1, DOS version 'A', then free count + 3-byte free bitmap per track.
bam="12 01 41 00"
for t in $(seq 1 35); do
    n=$(track_sectors "$t")
    used=0
    ((t == 17)) && used=0x1 # 17/0: file data
    ((t == 18)) && used=0x3 # 18/0: BAM, 18/1: directory
    bits=$(((1 << n) - 1 & ~used))
    free=$n
    for ((s = 0; s < n; s++)); do ((used >> s & 1)) && free=$((free - 1)); done
    bam+=$(printf ' %02x %02x %02x %02x' "$free" $((bits & 0xff)) $((bits >> 8 & 0xff)) $((bits >> 16 & 0xff)))
done
bam+=" $(text 'UE2EMU DEMO' a0 16) a0 a0 $(text UE '' 2) a0 $(text 2A '' 2) a0 a0 a0 a0"
poke "$d64" $((357 * 256)) "$bam"
# Directory, track 18 sector 1: last sector; entry 0 = closed PRG at 17/0, one block.
poke "$d64" $((358 * 256)) "00 ff 82 11 00 $(text HELLO a0 16) 00 00 00 00 00 00 00 00 00 01 00"
# Data, track 17 sector 0: last block, last used byte index, PRG bytes.
poke "$d64" $((336 * 256)) "00 $(printf '%02x' $((prg_len + 1))) $prg"

cat >"$files/readme.txt" <<'EOF'
UE2-C64U-Emulator sample SD card.
Created by scripts/make-sd-image.sh.
EOF

# --- image --------------------------------------------------------------------------------------------------
mkdir -p "$(dirname "$image")"
rm -f "$image"
dd if=/dev/zero of="$image" bs=1048576 count="$size_mb" status=none
# MBR entry 0 at 0x1BE: inactive, CHS unused (FE FF FF), type 0C (FAT32 LBA), LBA start, sector count; 55 AA.
poke "$image" $((0x1be)) "00 fe ff ff 0c fe ff ff $(le32 $part_start) $(le32 $part_sectors)"
poke "$image" $((0x1fe)) "55 aa"

dev=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount "$image" 2>/dev/null | awk 'NR == 1 { print $1 }')
[[ $dev == /dev/disk* ]] || { echo "hdiutil attach failed" >&2; exit 1; }
part=${dev}s1
for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -e $part ]] && break; sleep 0.2; done
[[ -e $part ]] || { echo "partition node $part did not appear" >&2; exit 1; }

newfs_msdos -F 32 -S 512 -v UE2SD "${part/disk/rdisk}" >/dev/null

mkdir -p "$work/mnt"
diskutil mount -mountPoint "$work/mnt" "$part" >/dev/null
mounted=1
# Keep fseventsd and Spotlight off the card, copy without AppleDouble files, then drop the markers.
mkdir -p "$work/mnt/.fseventsd"
touch "$work/mnt/.fseventsd/no_log" "$work/mnt/.metadata_never_index"
COPYFILE_DISABLE=1 cp -X "$files"/* "$work/mnt/"
rm -rf "$work/mnt/.fseventsd" "$work/mnt/.metadata_never_index" "$work/mnt"/._*
diskutil unmount "$work/mnt" >/dev/null
mounted=
hdiutil detach "$dev" -quiet
dev=

echo "$image: ${size_mb} MB, MBR + FAT32 \"UE2SD\": hello.prg demo.d64 readme.txt"
