#!/usr/bin/env python3
"""Build test cartridges for the physical cartridge slot (`ue2emu run --cart-slot`, docs/status/cart-slot.md).

    scripts/make-slot-crts.py <output dir>

One CRT per cartridge family the slot serves (FAMILIES in scripts/cartslot_common.py), plus `manifest.json` that
names each file's family, banks, windows and how it boots. Every ROM byte is a pattern that names its cartridge
(salt), bank and window (`cartslot_common.pattern`): banks differ from each other at every byte, so a dump that
selects the wrong bank, reads the wrong window or misses a wrap cannot match. Some flash cartridges leave banks out
(they read 0xFF on the board) and carry a bank at the top of the chip.

How each boots, so the firmware's boot DMA (SID detection, the boot hotkey scan) never meets a crashed C64:
- 8 K cartridges without an autostart header boot BASIC;
- `s01-normal-8k.crt` autostarts and prints `CART SLOT NORMAL 8K` / `PHYSICAL CARTRIDGE BOOTED` (no KERNAL calls);
- cartridges that come up in 16 K mode carry a CBM80 header in bank 0 whose code is `SEI` and a jump to itself;
- `s03-ultimax.crt` and `s08-easyflash.crt` reset into the same loop in ROMH and stay in ULTIMAX, like a cartridge
  whose program is running;
- `s09-easyflash-eapi.crt` resets into a stub in bank 0 ROMH that copies `LDA #$04 : STA $DE02 : JMP ($FFFC)` to $0100
  and runs it: the cartridge switches itself off and the KERNAL boots BASIC, as a game started from an EasyFlash does.
  Its EXROM and GAME are then released, so it is invisible to U64_CART_DETECT and to a probe until it is reset.

`s09-easyflash-eapi.crt` also carries an `eapi` block at bank 0 ROMH $1800 (offset $3800 of the ROM): the firmware and
TRX64 replace such a block with their own driver, a physical cartridge does not, so a dump shows whether it was.
"""

import importlib.util
import json
import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cartslot_common as cs  # noqa: E402


def assembler():
    """The two-pass 6502 assembler of scripts/make-test-crts.py."""
    spec = importlib.util.spec_from_file_location('make_test_crts', HERE / 'make-test-crts.py')
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.Asm, mod.screen_codes


# --- boot code ------------------------------------------------------------------------------------------------------

AUTOSTART_TEXT = ('CART SLOT NORMAL 8K', 'PHYSICAL CARTRIDGE BOOTED')

# CBM80 header ($8000: cold and warm start $8009) and `$8009 SEI : $800A JMP $800A`.
IDLE_CBM80 = struct.pack('<HH', 0x8009, 0x8009) + b'\xC3\xC2\xCD80' + bytes([0x78, 0x4C, 0x0A, 0x80])

# ULTIMAX ROMH at $F000: SEI, JMP $F001, RTI at $F004; vectors NMI/RESET/IRQ.
ULTIMAX_IDLE = (0x1000, bytes([0x78, 0x4C, 0x01, 0xF0, 0x40]))
ULTIMAX_VECTORS = struct.pack('<HHH', 0xF004, 0xF000, 0xF004)

# EasyFlash ROMH at $FF00 (bank 0): SEI; LDX #$FF; TXS; LDX #7; copy the 8 bytes at $FF12 to $0100; JMP $0100.
# At $0100: LDA #$04; STA $DE02 (cartridge off); JMP ($FFFC) (the KERNAL's reset). RTI at $FF1A.
EF_STUB = (0x1F00, bytes([0x78, 0xA2, 0xFF, 0x9A, 0xA2, 0x07, 0xBD, 0x12, 0xFF, 0x9D, 0x00, 0x01, 0xCA, 0x10, 0xF7,
                          0x4C, 0x00, 0x01, 0xA9, 0x04, 0x8D, 0x02, 0xDE, 0x6C, 0xFC, 0xFF, 0x40]))
EF_VECTORS = struct.pack('<HHH', 0xFF1A, 0xFF00, 0xFF1A)


def autostart_8k():
    """ROML code: CBM80 header, VIC set up without KERNAL calls, screen cleared, two lines of text, endless loop."""
    Asm, screen_codes = assembler()
    a = Asm(0x8000)
    a.data(struct.pack('<HH', 0x8009, 0x8009) + b'\xC3\xC2\xCD80')
    a('SEI; LDX #$FF; TXS')
    a('LDA #$1B; STA $D011; LDA #$08; STA $D016; LDA #$15; STA $D018; LDA #$0E; STA $D020; LDA #$06; STA $D021')
    a('LDX #0')
    a.label('clear')
    a('LDA #$20; STA $0400,X; STA $0500,X; STA $0600,X; STA $0700,X')
    a('LDA #$01; STA $D800,X; STA $D900,X; STA $DA00,X; STA $DB00,X; INX; BNE clear')
    for row, text in ((0, AUTOSTART_TEXT[0]), (2, AUTOSTART_TEXT[1])):
        loop, end = f'line{row}', f'end{row}'
        a('LDX #0')
        a.label(loop)
        a(f'LDA text{row},X; BEQ {end}; STA ${0x0400 + row * 40:04X},X; INX; BNE {loop}')
        a.label(end)
    a.label('hang')
    a('JMP hang')
    for row, text in ((0, AUTOSTART_TEXT[0]), (2, AUTOSTART_TEXT[1])):
        a.label(f'text{row}')
        a.data(screen_codes(text) + b'\0')
    return a.assemble()


# --- cartridges -----------------------------------------------------------------------------------------------------

# (file, family, name, ROML banks, ROMH banks, boot, extra)
SPECS = [
    ('s01-normal-8k.crt', 'normal8k', 'NORMAL 8K AUTOSTART', [0], [], 'text', {}),
    ('s02-normal-16k.crt', 'normal16k', 'NORMAL 16K', [0], [0], 'idle', {}),
    ('s03-ultimax.crt', 'ultimax', 'ULTIMAX', [0], [0], 'ultimax', {}),
    # 128 K: TRX64's Ocean mapper is in 16 K mode with ROML mirrored at $A000; the bank register wraps at 16.
    ('s04-ocean.crt', 'ocean', 'OCEAN 128K', list(range(16)), [], 'idle', {}),
    # 512 K: 8 K mode.
    ('s05-ocean-512k.crt', 'ocean', 'OCEAN 512K', list(range(64)), [], 'basic', {}),
    ('s06-magic-desk.crt', 'magicdesk', 'MAGIC DESK', list(range(16)), [], 'basic', {}),
    ('s07-magic-desk-16.crt', 'magicdesk16', 'MAGIC DESK 16', list(range(8)), list(range(8)), 'idle', {'chip16k': True}),
    ('s08-easyflash.crt', 'easyflash', 'EASYFLASH', list(range(10)) + [37], list(range(10)) + [63], 'ultimax', {}),
    ('s09-easyflash-eapi.crt', 'easyflash', 'EASYFLASH EAPI', list(range(4)), list(range(4)), 'ef', {'eapi': True}),
    ('s10-gmod2.crt', 'gmod2', 'GMOD2', list(range(16)), [], 'basic', {'eeprom': True}),
    ('s11-megabyter.crt', 'megabyter', 'MEGABYTER', list(range(12)) + [127], [], 'basic', {}),
    ('s12-c64megacart.crt', 'c64megacart', 'C64MEGACART', list(range(12)) + [255], [], 'basic', {}),
    # The U64 cart logic serves these (TRX64 has no mapper for them).
    ('s13-super-games.crt', 'supergames', 'SUPER GAMES', list(range(4)), list(range(4)), 'idle', {'chip16k': True}),
    ('s14-c64-game-system.crt', 'c64gs', 'C64 GAME SYSTEM', list(range(8)), [], 'basic', {}),
    ('s15-action-replay.crt', 'actionreplay', 'ACTION REPLAY', list(range(4)), [], 'basic', {}),
]


def eeprom_image():
    """2 K GMod2 EEPROM, 16-bit words big-endian: word 0 `UE`, word 1 `2E`, then a counting pattern (never 0xFFFF)."""
    data = bytearray(b'UE2E')
    data += bytes(((i * 11) + 0x21) & 0xFF for i in range(4, 0x800))
    return bytes(data)


def build(salt, spec):
    file, key, name, roml, romh, boot, extra = spec
    fam = cs.FAMILIES[key]
    windows = {}
    for bank in roml:
        windows[(bank, 'roml')] = cs.pattern(salt, bank, 'roml')
    for bank in romh:
        windows[(bank, 'romh')] = cs.pattern(salt, bank, 'romh')
    if boot == 'text':
        code = autostart_8k()
        windows[(0, 'roml')][:len(code)] = code
    elif boot == 'idle':
        windows[(0, 'roml')][:len(IDLE_CBM80)] = IDLE_CBM80
    elif boot == 'ultimax':
        at, code = ULTIMAX_IDLE
        windows[(0, 'romh')][at:at + len(code)] = code
        windows[(0, 'romh')][0x1FFA:0x2000] = ULTIMAX_VECTORS
    elif boot == 'ef':
        at, code = EF_STUB
        windows[(0, 'romh')][at:at + len(code)] = code
        windows[(0, 'romh')][0x1FFA:0x2000] = EF_VECTORS
    if extra.get('eapi'):
        # c64_crt.cc:393-404 and TRX64 cart.rs look for "eapi" at bank 0 ROMH $1800.
        windows[(0, 'romh')][0x1800:0x1804] = b'eapi'
    chip_type = fam.get('chip_type', 0)
    romh_load = 0xE000 if key == 'ultimax' else 0xA000
    chips = []
    for bank in sorted({b for b, _ in windows}):
        lo, hi = windows.get((bank, 'roml')), windows.get((bank, 'romh'))
        if extra.get('chip16k') and lo is not None and hi is not None:
            chips.append((bank, 0x8000, bytes(lo + hi), chip_type))
            continue
        if lo is not None:
            chips.append((bank, 0x8000, bytes(lo), chip_type))
        if hi is not None:
            chips.append((bank, romh_load, bytes(hi), chip_type))
    if extra.get('eeprom'):
        chips.append((0, cs.EEPROM_LOAD, eeprom_image(), 0))
    crt = cs.crt_bytes(fam['hw'], fam['exrom'], fam['game'], name, chips)
    entry = {
        'file': file, 'family': key, 'name': name, 'hw_type': fam['hw'], 'exrom': fam['exrom'], 'game': fam['game'],
        'model': fam['model'], 'salt': salt, 'roml_banks': roml, 'romh_banks': romh, 'boot': boot,
        'eeprom': bool(extra.get('eeprom')), 'eapi': bool(extra.get('eapi')),
    }
    if boot == 'text':
        entry['text'] = list(AUTOSTART_TEXT)
    return crt, entry


def main():
    if len(sys.argv) != 2:
        sys.exit(f'usage: {sys.argv[0]} <output dir>')
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    manifest = []
    for i, spec in enumerate(SPECS):
        crt, entry = build((0x11 * (i + 1)) & 0xFF, spec)
        # Round trip through the parser the dump and flash tools use.
        parsed = cs.parse_crt(crt)
        assert cs.family_of(parsed) == entry['family'], entry['file']
        (out / entry['file']).write_bytes(crt)
        manifest.append(entry)
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=1) + '\n')
    print(' '.join(e['file'] for e in manifest))


if __name__ == '__main__':
    main()
