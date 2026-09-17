#!/usr/bin/env python3
"""S17: a PSID v3 tune whose second SID alone plays a 1000 Hz tone (docs/status/sid-audio.md).

    scripts/make-stereo-sid.py <output .sid>

Header byte $7A = $42 puts the second SID at $D420, so the firmware's SID player maps UltiSID 1 at $D400 and UltiSID 2
at $D420 (filetype_sid.cc:110-119, u64_config.cc:2031-2041). Loaded at $1000 (address in the first two data bytes);
play at $1000 is an RTS, init at $1001 leaves SID 1 alone and starts voice 1 of SID 2: a sawtooth at F = 17029
(17029 * 985248 / 2^24 = 1000.1 Hz at PAL), attack 0, sustain 15, volume 15. scripts/smoke-sid-stereo.ctl plays it.
"""

import struct
import sys
from pathlib import Path

LOAD = 0x1000
PLAY = LOAD
INIT = LOAD + 1

# (register of SID 2 at $D420, value), in the order init writes them; the gate goes on last.
TONE = [(0x05, 0x00), (0x06, 0xF0), (0x01, 66), (0x00, 133), (0x18, 0x0F), (0x04, 0x21)]


def code():
    """RTS (play), then LDA #v / STA $D4rr per register and RTS (init)."""
    out = bytearray([0x60])
    for reg, val in TONE:
        out += bytes([0xA9, val, 0x8D, 0x20 + reg, 0xD4])
    out.append(0x60)
    return bytes(out)


def psid():
    def field(text):
        return text.encode().ljust(32, b'\0')

    # Version 3, data at $7C, load address in the data, one song, CIA speed flags 0.
    header = b'PSID' + struct.pack('>HHHHHHHI', 3, 0x7C, 0x0000, INIT, PLAY, 1, 1, 0)
    header += field('UE2EMU STEREO TONE') + field('UE2EMU') + field('2026 UE2EMU')
    # Flags: PAL (bits 2-3 = 01), 6581 (bits 4-5 = 01), second SID as the first (bits 6-7 = 00); start page and
    # length 0; second SID at $D420; no third SID.
    header += struct.pack('>HBBBB', 0x0014, 0, 0, 0x42, 0)
    assert len(header) == 0x7C
    return header + struct.pack('<H', LOAD) + code()


def main():
    if len(sys.argv) != 2:
        sys.exit(f'usage: {sys.argv[0]} <output .sid>')
    out = Path(sys.argv[1])
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(psid())
    print(out)


if __name__ == '__main__':
    main()
