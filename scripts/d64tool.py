#!/usr/bin/env python3
"""Build and inspect 1541 D64 images, and read a file out of a FAT32 SD card image, without mounting anything.

    scripts/d64tool.py build OUT.d64 [--title NAME] [--id ID] [--print NAME=TEXT]... [--file NAME=PATH]...
                                     [--filler NAME=SIZE]...
    scripts/d64tool.py list IMAGE.d64
    scripts/d64tool.py extract IMAGE.d64 NAME [OUT]
    scripts/d64tool.py sd-get SD.img FILE OUT

build    35-track D64 with a BAM, a directory and one PRG per option, in order. `--print NAME=TEXT` is the BASIC
         program `10 PRINT"TEXT"`; `--file NAME=PATH` stores a PRG file as it is; `--filler NAME=SIZE` is
         `10 PRINT"NAME"` followed by SIZE counting bytes, a long load. Blocks are taken from track 17
         down, then from track 19 up, the way the 1541 DOS allocates; the BAM marks them used.
list     the directory as the C64 lists it: blocks, "NAME", type.
extract  the bytes of file NAME, load address included; to OUT or stdout.
sd-get   a file from the root directory of the first MBR partition of a FAT32 image (scripts/make-sd-image.sh),
         matched by its 8.3 name, case-insensitive.

Used by scripts/smoke-c64-drive.ctl (docs/status/drive.md).
"""

import argparse
import struct
import sys

SECTOR = 256
TRACKS = 35
DIR_TRACK = 18


def sectors_in(track):
    return 21 if track <= 17 else 19 if track <= 24 else 18 if track <= 30 else 17


def offset(track, sector):
    return (sum(sectors_in(t) for t in range(1, track)) + sector) * SECTOR


def petscii_name(name):
    raw = name.upper().encode("ascii")
    if len(raw) > 16:
        sys.exit(f"{name}: longer than 16 characters")
    return raw + b"\xa0" * (16 - len(raw))


def print_program(text):
    """PRG bytes of 10 PRINT"TEXT": load address $0801, one line, end of program."""
    body = bytes([10, 0, 0x99]) + b'"' + text.upper().encode("ascii") + b'"' + b"\x00"
    link = 0x0801 + 2 + len(body)
    return struct.pack("<HH", 0x0801, link) + body + b"\x00\x00"


def build(out, title, disk_id, files):
    image = bytearray(offset(TRACKS + 1, 0))
    used = {t: set() for t in range(1, TRACKS + 1)}
    used[DIR_TRACK] |= {0, 1}
    order = [(t, s) for t in list(range(17, 0, -1)) + list(range(19, TRACKS + 1)) for s in range(sectors_in(t))]
    free = iter(order)
    entries = []
    for name, data in files:
        chunks = [data[i : i + SECTOR - 2] for i in range(0, len(data), SECTOR - 2)] or [b""]
        blocks = [next(free) for _ in chunks]
        for i, ((t, s), chunk) in enumerate(zip(blocks, chunks)):
            used[t].add(s)
            link = bytes(blocks[i + 1]) if i + 1 < len(blocks) else bytes([0, len(chunk) + 1])
            start = offset(t, s)
            image[start : start + 2 + len(chunk)] = link + chunk
        entries.append((name, blocks[0], len(blocks)))
    if len(entries) > 8:
        sys.exit("at most 8 files (one directory sector)")

    bam = bytearray(SECTOR)
    bam[0:4] = bytes([DIR_TRACK, 1, 0x41, 0])
    for t in range(1, TRACKS + 1):
        bits = sum(1 << s for s in range(sectors_in(t)) if s not in used[t])
        bam[4 * t : 4 * t + 4] = bytes([sectors_in(t) - len(used[t]), bits & 0xFF, bits >> 8 & 0xFF, bits >> 16])
    bam[0x90:0xA0] = petscii_name(title)
    bam[0xA0:0xAB] = b"\xa0\xa0" + disk_id.upper().encode("ascii")[:2].ljust(2, b"\xa0") + b"\xa0" + b"2A" + b"\xa0" * 4
    image[offset(DIR_TRACK, 0) : offset(DIR_TRACK, 1)] = bam

    directory = bytearray(SECTOR)
    directory[0:2] = bytes([0, 0xFF])
    for i, (name, (t, s), blocks) in enumerate(entries):
        e = 2 + 32 * i
        directory[e : e + 3] = bytes([0x82, t, s])
        directory[e + 3 : e + 19] = petscii_name(name)
        directory[e + 28 : e + 30] = struct.pack("<H", blocks)
    image[offset(DIR_TRACK, 1) : offset(DIR_TRACK, 2)] = directory

    with open(out, "wb") as f:
        f.write(image)


def entries_of(image):
    t, s = DIR_TRACK, 1
    seen = set()
    while t and (t, s) not in seen:
        seen.add((t, s))
        sector = image[offset(t, s) : offset(t, s) + SECTOR]
        for i in range(8):
            e = sector[2 + 32 * i : 34 + 32 * i]
            if e[0] & 0x07:
                yield e[3:19].rstrip(b"\xa0").decode("latin-1"), e[0], (e[1], e[2]), struct.unpack("<H", e[28:30])[0]
        t, s = sector[0], sector[1]


def file_bytes(image, first):
    data = bytearray()
    t, s = first
    for _ in range(683):
        sector = image[offset(t, s) : offset(t, s) + SECTOR]
        if sector[0] == 0:
            return bytes(data + sector[2 : sector[1] + 1])
        data += sector[2:]
        t, s = sector[0], sector[1]
    sys.exit("file chain does not end")


def fat_get(sd, name, out):
    with open(sd, "rb") as f:
        disk = f.read()
    lba = struct.unpack_from("<I", disk, 0x1BE + 8)[0]
    part = lba * 512
    bps, spc, reserved, fats = struct.unpack_from("<HBHB", disk, part + 11)
    fat_sectors, root = struct.unpack_from("<I4xI", disk, part + 36)
    fat = part + reserved * bps
    data = fat + fats * fat_sectors * bps
    cluster_size = spc * bps

    def chain(cluster):
        while 2 <= cluster < 0x0FFFFFF8:
            yield disk[data + (cluster - 2) * cluster_size : data + (cluster - 1) * cluster_size]
            cluster = struct.unpack_from("<I", disk, fat + 4 * cluster)[0] & 0x0FFFFFFF

    base, _, ext = name.upper().partition(".")
    want = base.encode().ljust(8) + ext.encode().ljust(3)
    root_dir = b"".join(chain(root))
    for i in range(0, len(root_dir), 32):
        e = root_dir[i : i + 32]
        if e[0] == 0:
            break
        if e[0] == 0xE5 or e[11] & 0x0F == 0x0F or e[11] & 0x18:
            continue
        if e[:11] == want:
            first = struct.unpack_from("<H", e, 20)[0] << 16 | struct.unpack_from("<H", e, 26)[0]
            size = struct.unpack_from("<I", e, 28)[0]
            with open(out, "wb") as f:
                f.write(b"".join(chain(first))[:size])
            return
    sys.exit(f"{name}: not in the root directory of {sd}")


def pair(value):
    name, sep, rest = value.partition("=")
    if not sep:
        raise argparse.ArgumentTypeError(f"{value}: expected NAME=VALUE")
    return name, rest


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("out")
    b.add_argument("--title", default="UE2 DRIVE")
    b.add_argument("--id", default="U2")
    b.add_argument("--print", dest="files", action="append", type=pair, default=[], metavar="NAME=TEXT")
    b.add_argument("--file", dest="files", action="append", type=lambda v: ("file",) + pair(v), metavar="NAME=PATH")
    b.add_argument("--filler", dest="files", action="append", type=lambda v: ("filler",) + pair(v), metavar="NAME=SIZE")
    ls = sub.add_parser("list")
    ls.add_argument("image")
    ex = sub.add_parser("extract")
    ex.add_argument("image")
    ex.add_argument("name")
    ex.add_argument("out", nargs="?")
    sg = sub.add_parser("sd-get")
    sg.add_argument("sd")
    sg.add_argument("file")
    sg.add_argument("out")
    a = p.parse_args()

    if a.cmd == "build":
        files = []
        for f in a.files:
            if f[0] == "file":
                with open(f[2], "rb") as src:
                    files.append((f[1], src.read()))
            elif f[0] == "filler":
                files.append((f[1], print_program(f[1]) + bytes(i & 0xFF for i in range(int(f[2])))))
            else:
                files.append((f[0], print_program(f[1])))
        build(a.out, a.title, a.id, files)
    elif a.cmd == "list":
        with open(a.image, "rb") as f:
            image = f.read()
        for name, kind, _, blocks in entries_of(image):
            print(f'{blocks:<5}"{name}"'.ljust(24) + ["DEL", "SEQ", "PRG", "USR", "REL"][kind & 7])
    elif a.cmd == "extract":
        with open(a.image, "rb") as f:
            image = f.read()
        for name, _, first, _ in entries_of(image):
            if name == a.name.upper():
                data = file_bytes(image, first)
                if a.out:
                    with open(a.out, "wb") as f:
                        f.write(data)
                else:
                    sys.stdout.buffer.write(data)
                return
        sys.exit(f"{a.name}: not on {a.image}")
    else:
        fat_get(a.sd, a.file, a.out)


if __name__ == "__main__":
    main()
