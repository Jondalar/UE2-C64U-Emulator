#!/usr/bin/env python3
"""Dump the cartridge in the physical slot over the firmware's REST API, like a DMA dumper (docs/status/cart-slot.md).

    scripts/cart-dump-rest.py --url http://127.0.0.1:8080 [--source SRC.crt | --family KEY] [--out DUMP.crt]
                              [--probe] [--roms DIR] [--eeprom-words N] [--json REPORT.json]

Only REST calls reach the C64 (software/api/route_machine.cc): PUT /v1/machine:pause, then every bank select is a
PUT /v1/machine:writemem and every window a GET /v1/machine:readmem, then PUT /v1/machine:resume. The firmware does
each of them as DMA on the cartridge bus (C64_DMA_RAW_WRITE / C64_DMA_RAW_READ, c64_subsys.cc `dma_load_raw_buffer`),
decoded by the PLA with the cartridge's EXROM/GAME and the CPU port, as a CPU access would be.

Steps:
1. Pause. Save $00/$01 and set them to $2F/$37 (the appsdk's `c64_cart_setup_bus` does the same).
2. `--probe`: the detection sequences of the fork's dumper (appsdk/cartlib/cartlib.c, cartlib_write.c) in their order:
   the cartridge mode U64_CART_DETECT would show (emulated over REST, see `cart_mode`), $DE00 banking (write 0/1,
   compare 256 bytes at $8000), the EasyFlash $DF00 RAM test ($A5), the FNV-1a bank count at 8/16/32/64 with
   128/256/512 skipped as cartlib does, the MegaByter test ($DE02=$03, $E000 changes), the C64MegaCart test
   ($DF00=$C0, $E000 changes) and the GMod2 M93C86 read of words 0, 1, 0 bit-banged through $DE00 (CS $40, CLK $20,
   DI $10, DO in bit 7 of a $DE00 read). It prints the raw results and what `cartlib_probe` and `cartlib_find_flash`
   decide from them.
3. The family comes from `--family`, else from the `--source` CRT, else from the probe.
4. Bank count: FNV-1a of every window of bank 0 against the banks at 2, 4, 8, ... below the board's capacity; the first
   equal one is where the bank register wraps. Banks below the count are read: select (FAMILIES[...]['select']), then
   8 K at $8000 and, for 16 K families, at $A000 ($E000 for ULTIMAX and for EasyFlash, which is read in its ULTIMAX
   mode $DE02=$05 as cartlib does). GMod2: `--eeprom-words` words of the EEPROM (default 8; 1024 is the whole chip,
   about 90 requests a word).
5. Restore $00/$01, resume. Write `--out` (a CRT with a chip per window that is not all 0xFF, bank 0 always).
6. `--source`: compare every dumped window with the source CRT (absent banks are 0xFF on the board). Exit 1 on any
   difference, 2 on an error.
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cartslot_common as cs  # noqa: E402

KERNAL = 'kernal.901227-03.bin'
BASIC = 'basic.901226-01.bin'

EEPROM_CS, EEPROM_CLK, EEPROM_DI, EEPROM_DO = 0x40, 0x20, 0x10, 0x80


def default_roms():
    for base in (os.environ.get('UE2_FIRMWARE'), HERE.parent / 'firmware' / '1541ultimate'):
        if base and (Path(base) / 'roms' / KERNAL).is_file():
            return Path(base) / 'roms'
    return None


# --- probes (appsdk/cartlib) ----------------------------------------------------------------------------------------

def cart_mode(rest, roms):
    """What U64_CART_DETECT & 3 would say ('8k', '16k', 'ultimax', 'none'), from the bus alone, since REST cannot read
    the register: with $01=$37 the KERNAL at $E000 is gone only under ULTIMAX, BASIC at $A000 only in 16 K mode, and a
    write to $8000 does not read back while ROML is mapped (the write goes to the RAM underneath)."""
    if roms is None:
        return None
    kernal = (roms / KERNAL).read_bytes()
    basic = (roms / BASIC).read_bytes()
    if rest.peek(0xE000, 16) != kernal[:16]:
        return 'ultimax'
    if rest.peek(0xA000, 16) != basic[:16]:
        return '16k'
    old = rest.peek(0x8000, 16)
    rest.poke(0x8000, bytes(b ^ 0xFF for b in old))
    new = rest.peek(0x8000, 16)
    rest.poke(0x8000, old)
    return '8k' if new == old else 'none'


def banking_test(rest):
    """cartlib_probe: $DE00=0, 256 bytes of $8000, $DE00=1, again, $DE00=0."""
    rest.poke(0xDE00, 0)
    ref = rest.peek(0x8000, 256)
    rest.poke(0xDE00, 1)
    buf = rest.peek(0x8000, 256)
    rest.poke(0xDE00, 0)
    return ref != buf


def easyflash_test(rest):
    """cartlib_internal_detect_easyflash: $DF00 old, write $A5, read back, restore."""
    old = rest.peek1(0xDF00)
    rest.poke(0xDF00, 0xA5)
    back = rest.peek1(0xDF00)
    rest.poke(0xDF00, old)
    return back == 0xA5, back


def cartlib_count_banks(rest):
    """cartlib_internal_count_banks: FNV of bank 0 against $DE00 = 8, 16, 32, 64; 128, 256 and 512 are not written
    (bit 7 would switch a Magic Desk off) and count as a wrap."""
    boundaries = [8, 16, 32, 64, 128, 256, 512]
    rest.poke(0xDE00, 0)
    ref = cs.fnv1a(rest.peek(0x8000, cs.WINDOW))
    hashes = []
    for b in boundaries:
        if b >= 0x80:
            hashes.append(ref)
            continue
        rest.poke(0xDE00, b)
        hashes.append(cs.fnv1a(rest.peek(0x8000, cs.WINDOW)))
    rest.poke(0xDE00, 0)
    result = 512
    for b, h in zip(boundaries, hashes):
        if h == ref:
            result = b
            break
    if result == 512:
        for i in range(5, -1, -1):
            if hashes[i] == hashes[i + 1]:
                result = boundaries[i]
            else:
                break
    return result, {str(b): f'{h:08x}' for b, h in zip(boundaries, hashes)}, f'{ref:08x}'


def e000_change(rest, addr, value, back):
    """cartlib_find_flash: 16 bytes at $E000, write `value` to `addr`, again, write `back`."""
    before = rest.peek(0xE000, 16)
    rest.poke(addr, value)
    after = rest.peek(0xE000, 16)
    rest.poke(addr, back)
    return before != after, before.hex(), after.hex()


def eeprom_read_word(rest, address):
    """cartlib_internal_eeprom_read_word: CS, start bit + READ (110) + 10 address bits clocked in, 16 bits clocked out,
    each sampled from bit 7 of a $DE00 read; CS off. One REST request per register access."""
    rest.poke(0xDE00, EEPROM_CS)
    cmd = (0x6 << 10) | (address & 0x3FF)
    for bit in range(12, -1, -1):
        val = EEPROM_CS | (EEPROM_DI if (cmd >> bit) & 1 else 0)
        rest.poke(0xDE00, val)
        rest.poke(0xDE00, val | EEPROM_CLK)
        rest.poke(0xDE00, val)
    data = 0
    for bit in range(15, -1, -1):
        rest.poke(0xDE00, EEPROM_CS)
        rest.poke(0xDE00, EEPROM_CS | EEPROM_CLK)
        if rest.peek1(0xDE00) & EEPROM_DO:
            data |= 1 << bit
    rest.poke(0xDE00, 0x00)
    return data


def gmod2_test(rest):
    """cartlib_internal_detect_gmod2: words 0, 1 and 0 again."""
    w0 = eeprom_read_word(rest, 0)
    w1 = eeprom_read_word(rest, 1)
    w0v = eeprom_read_word(rest, 0)
    rest.poke(0xDE00, 0)
    found = (w0 == w0v and w0 != w1) or (w0 == w0v and w0 not in (0xFFFF, 0x0000))
    return found, [f'{w0:04x}', f'{w1:04x}', f'{w0v:04x}']


def probe(rest, roms):
    r = {}
    r['cart_mode'] = cart_mode(rest, roms)
    r['de00_banking'] = banking_test(rest)
    r['df00_ram'], back = easyflash_test(rest)
    r['df00_readback'] = f'{back:02x}'
    if r['df00_ram']:
        rest.poke(0xDE02, 0x05)
        rest.poke(0xDE00, 0)
    count, hashes, ref = cartlib_count_banks(rest)
    r['bank_count'], r['bank_hashes'], r['bank0_hash'] = count, hashes, ref
    r['megabyter_e000_changes'], *_ = e000_change(rest, 0xDE02, 0x03, 0x00)
    r['c64megacart_e000_changes'], *_ = e000_change(rest, 0xDF00, 0xC0, 0x00)
    r['gmod2_eeprom'], r['gmod2_words'] = gmod2_test(rest)

    mode = r['cart_mode']
    ef = r['de00_banking'] and r['df00_ram']
    banks = 64 if ef else (count if r['de00_banking'] else 1)
    # cartlib_probe
    if ef:
        decided = 'EasyFlash'
    elif mode == '8k':
        if r['de00_banking']:
            decided = ('MegaByter' if banks >= 128 else 'C64MegaCart' if banks > 64
                       else 'GMod2' if r['gmod2_eeprom'] else 'Ocean/Magic Desk')
        else:
            decided = 'Normal 8K'
    elif mode == '16k':
        decided = 'Ocean 16K' if r['de00_banking'] else 'Normal 16K'
    elif mode == 'ultimax':
        decided = 'Ultimax'
    else:
        decided = 'Normal (no cartridge lines)'
    r['cartlib_probe'] = decided
    # cartlib_find_flash
    flash = None
    if r['de00_banking']:
        if r['df00_ram']:
            flash = 'EasyFlash'
        elif mode == '8k':
            if count >= 128 and r['megabyter_e000_changes']:
                flash = 'MegaByter'
            elif count >= 256 and r['c64megacart_e000_changes']:
                flash = 'C64MegaCart'
            elif count <= 64 and r['gmod2_eeprom']:
                flash = 'GMod2'
    r['cartlib_find_flash'] = flash
    return r


PROBE_TO_FAMILY = {'EasyFlash': 'easyflash', 'MegaByter': 'megabyter', 'C64MegaCart': 'c64megacart',
                   'GMod2': 'gmod2', 'Ocean/Magic Desk': 'magicdesk', 'Ocean 16K': 'ocean', 'Normal 8K': 'normal8k',
                   'Normal 16K': 'normal16k', 'Ultimax': 'ultimax'}


# --- dump -----------------------------------------------------------------------------------------------------------

def window_address(key, which):
    fam = cs.FAMILIES[key]
    if which == 'roml':
        return 0x8000
    return 0xE000 if key == 'ultimax' else fam.get('romh_at', 0xA000)


def select(rest, key, bank):
    for addr, val in cs.FAMILIES[key].get('select', lambda n: [])(bank):
        rest.poke(addr, val)


def read_bank(rest, key, bank):
    select(rest, key, bank)
    return {w: rest.peek(window_address(key, w), cs.WINDOW) for w in cs.FAMILIES[key]['windows']}


def count_banks(rest, key):
    """Where the bank register wraps: the first power of two below the capacity whose bank reads like bank 0."""
    cap = cs.FAMILIES[key]['capacity']
    ref = read_bank(rest, key, 0)
    b = 2
    while b < cap:
        if read_bank(rest, key, b) == ref:
            return b
        b *= 2
    return cap


def dump(rest, key, eeprom_words):
    count = count_banks(rest, key)
    data = {}
    for bank in range(count):
        for which, blob in read_bank(rest, key, bank).items():
            data[(bank, which)] = blob
    eeprom = None
    if key == 'gmod2' and eeprom_words:
        words = [eeprom_read_word(rest, a) for a in range(eeprom_words)]
        eeprom = b''.join(w.to_bytes(2, 'big') for w in words)
        rest.poke(0xDE00, 0)
    return count, data, eeprom


def dump_crt(key, data, eeprom):
    fam = cs.FAMILIES[key]
    romh_load = 0xE000 if key == 'ultimax' else 0xA000
    chips = []
    for (bank, which), blob in sorted(data.items()):
        if bank != 0 and blob == b'\xFF' * cs.WINDOW:
            continue
        chips.append((bank, 0x8000 if which == 'roml' else romh_load, blob, fam.get('chip_type', 0)))
    if eeprom is not None and len(eeprom) == 0x800:
        chips.append((0, cs.EEPROM_LOAD, eeprom, 0))
    return cs.crt_bytes(fam['hw'], fam['exrom'], fam['game'], 'DUMP', chips)


def compare(source, key, count, data, eeprom):
    """Every dumped window against the source; absent source banks are 0xFF on the board."""
    mismatches, compared = [], 0
    for (bank, which), blob in sorted(data.items()):
        want = cs.window(source, bank, which) or b'\xFF' * cs.WINDOW
        compared += len(blob)
        if blob != want:
            diff = [i for i in range(cs.WINDOW) if blob[i] != want[i]]
            mismatches.append({'bank': bank, 'window': which, 'bytes': len(diff), 'first': f'{diff[0]:#06x}',
                               'got': f'{blob[diff[0]]:02x}', 'want': f'{want[diff[0]]:02x}'})
    top = max(cs.banks(source), default=0)
    if top >= count:
        mismatches.append({'banks': f'the source has bank {top}, the dump found {count} banks'})
    if eeprom is not None:
        want = (cs.eeprom(source) or b'\xFF' * 0x800)[:len(eeprom)]
        compared += len(eeprom)
        if eeprom != want:
            mismatches.append({'eeprom': f'got {eeprom[:16].hex()}, want {want[:16].hex()}'})
    return compared, mismatches


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--url', required=True, help='firmware web server, e.g. http://127.0.0.1:8080')
    ap.add_argument('--password', help='X-Password for a protected web server')
    ap.add_argument('--source', type=Path, help='CRT the dump must equal')
    ap.add_argument('--family', choices=sorted(cs.FAMILIES), help='cartridge family (default: from --source, else probe)')
    ap.add_argument('--out', type=Path, help='write the dump as a CRT')
    ap.add_argument('--probe', action='store_true', help="run the fork's detection sequences first")
    ap.add_argument('--roms', type=Path, default=default_roms(), help='KERNAL/BASIC images for the cartridge mode probe')
    ap.add_argument('--eeprom-words', type=int, default=8, help='GMod2 EEPROM words to read (1024 = all)')
    ap.add_argument('--json', type=Path, help='write the report as JSON')
    ap.add_argument('--no-dump', action='store_true', help='probe only')
    args = ap.parse_args()

    rest = cs.Rest(args.url, args.password)
    report = {'url': args.url}
    t0 = time.time()
    source = cs.parse_crt(args.source.read_bytes()) if args.source else None
    rest.wait_ready()
    rest.pause()
    saved = rest.peek(0x0000, 2)
    try:
        rest.poke(0x0000, bytes([0x2F, 0x37]))
        if args.probe:
            report['probe'] = probe(rest, args.roms)
            print('probe: ' + json.dumps(report['probe']))
        key = args.family or (cs.family_of(source) if source else None)
        if key is None and 'probe' in report:
            key = PROBE_TO_FAMILY.get(report['probe']['cartlib_probe'])
        report['family'] = key
        if not args.no_dump:
            if key is None:
                raise SystemExit('cartridge family unknown: pass --family or --source')
            count, data, eeprom = dump(rest, key, args.eeprom_words)
            report.update(banks=count, windows=len(data))
            if args.out:
                args.out.write_bytes(dump_crt(key, data, eeprom))
            if source is not None:
                compared, mismatches = compare(source, key, count, data, eeprom)
                report.update(bytes_compared=compared, mismatches=mismatches)
    finally:
        rest.poke(0x0000, saved)
        rest.resume()
    report['rest_calls'] = rest.calls
    report['seconds'] = round(time.time() - t0, 1)
    if args.json:
        args.json.write_text(json.dumps(report, indent=1) + '\n')
    line = f"{report.get('family')}: {report.get('banks', '-')} banks, {report.get('windows', '-')} windows"
    if 'mismatches' in report:
        state = 'EXACT' if not report['mismatches'] else f"{len(report['mismatches'])} MISMATCHES"
        line += f", {report['bytes_compared']} bytes compared: {state}"
        for m in report['mismatches'][:8]:
            line += f'\n  {m}'
    print(line + f" ({report['rest_calls']} REST calls, {report['seconds']} s)")
    return 1 if report.get('mismatches') else 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (cs.RestError, OSError, ValueError) as e:
        print(f'error: {e}', file=sys.stderr)
        sys.exit(2)
