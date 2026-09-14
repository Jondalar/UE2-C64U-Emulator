#!/usr/bin/env bash
# Copy files into the root directory of an SD card image made by scripts/make-sd-image.sh, on macOS without root.
#
#   scripts/add-sd-files.sh <image> <file>...
#
# Same tools and precautions as make-sd-image.sh: hdiutil attaches the raw file without mounting, diskutil mounts
# the FAT32 partition at a private mount point (never under /Volumes), fseventsd and Spotlight stay off the card
# and no AppleDouble files are written. docs/specs/S14-c64-trx64.md §12 uses it for the C64 system ROMs.

set -euo pipefail

(($# >= 2)) || { echo "usage: $0 <image> <file>..." >&2; exit 2; }
image=$1
shift
[[ -f $image ]] || { echo "$image: no such image" >&2; exit 1; }
for file in "$@"; do
    [[ -f $file ]] || { echo "$file: no such file" >&2; exit 1; }
done

work=$(mktemp -d "${TMPDIR:-/tmp}/ue2-sd.XXXXXX")
dev=
mounted=
cleanup() {
    if [[ -n $mounted ]]; then diskutil unmount "$work/mnt" >/dev/null 2>&1 || true; fi
    if [[ -n $dev ]]; then hdiutil detach "$dev" -quiet >/dev/null 2>&1 || true; fi
    rm -rf "$work"
}
trap cleanup EXIT

dev=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount "$image" 2>/dev/null | awk 'NR == 1 { print $1 }')
[[ $dev == /dev/disk* ]] || { echo "hdiutil attach failed" >&2; exit 1; }
part=${dev}s1
for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -e $part ]] && break; sleep 0.2; done
[[ -e $part ]] || { echo "partition node $part did not appear" >&2; exit 1; }

mkdir -p "$work/mnt"
diskutil mount -mountPoint "$work/mnt" "$part" >/dev/null
mounted=1
mkdir -p "$work/mnt/.fseventsd"
touch "$work/mnt/.fseventsd/no_log" "$work/mnt/.metadata_never_index"
COPYFILE_DISABLE=1 cp -X "$@" "$work/mnt/"
rm -rf "$work/mnt/.fseventsd" "$work/mnt/.metadata_never_index" "$work/mnt"/._*
diskutil unmount "$work/mnt" >/dev/null
mounted=
hdiutil detach "$dev" -quiet
dev=

echo "$image: added $(for file in "$@"; do basename "$file"; done | tr '\n' ' ')"
