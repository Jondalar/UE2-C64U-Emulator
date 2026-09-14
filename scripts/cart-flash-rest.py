#!/usr/bin/env python3
"""Erase and program flash of the cartridge in the physical slot over the firmware's REST API (docs/status/cart-slot.md).

    scripts/cart-flash-rest.py --url http://127.0.0.1:8080 --family easyflash|megabyter|c64megacart|gmod2
                               [--unlock short|long] [--bank 8] [--bytes 64] [--source SRC.crt] [--chip-erase]
                               [--state PREFIX] [--control 127.0.0.1:6400 --save OUT.crt] [--json REPORT.json]

Every flash command byte is its own PUT /v1/machine:writemem and every status read a GET /v1/machine:readmem while
the C64 is paused (PUT /v1/machine:pause): the command sequences of the fork's writer (appsdk/cartlib/cartlib_write.c)
as the cartridge's flash sees them on the bus.

- Flash mode (cartlib_write.c descriptors): EasyFlash $DE02=$05 (ULTIMAX: ROML $8000, ROMH $E000), MegaByter
  $DE02=$03, C64MegaCart $DF00=$C0, GMod2 $DE00=$C0|bank. GMod2 serves its flash only in 8 K mode, so every GMod2 read
  switches to $DE00=bank and back.
- Command addresses (`--unlock`): `short` is cartlib's, AA/55 at flash offsets $555/$2AA (AM29F040B, EasyFlash and
  GMod2) or $AAA/$555 (MX29F800CB MegaByter, M29F160FT C64MegaCart), written in the current bank: EasyFlash $8555/$82AA
  and $E555/$E2AA, GMod2 $8555/$82AA, MegaByter $8AAA/$8555, C64MegaCart $EAAA/$E555. `long` uses the full offsets
  $5555/$2AAA ($AAAA/$5555 for the byte-mode chips): the bank register is set to the offset's bank around each
  command byte ($5555 is bank 2 + $1555) and back. The other unlock is tried too, for autoselect only, and reported:
  with `--cart-slot ...,flash-decode=15` the short one must fail.
- Autoselect: AA, 55, 90; manufacturer and device id at +0 and +1 (+2 for the M29F160); F0 back to read.
- Sector erase of the sector holding `--bank` (64 K: 8 banks of 8 K): AA 55 80 AA 55, 30 at the bank's first byte.
  The status is polled until 0xFF: DQ7 low and DQ6 toggling while it erases, DQ5 is the timeout bit (cartlib
  `flash_poll`). EasyFlash: ROMH is chip-erased too (AA 55 80 AA 55 10); `--chip-erase` does that for the other
  families' only chip instead of the sector erase.
- Program: `--bytes` bytes spread over the erased sector (and ROMH bank `--bank` for EasyFlash) with AA 55 A0 and the
  data at its address, each followed by the DQ7/DQ5 poll; EasyFlash unlocks in bank 0 and writes the data in the target
  bank, as cartlib does.
- Verify: the whole chip (and ROMH) read back and compared with what it must hold: erased except the programmed bytes;
  with `--source`, every bank outside the sector equals the source CRT.
- `--state PREFIX`: the whole chip as read back afterwards, PREFIX.roml.bin (and PREFIX.romh.bin); the data a
  `cart-save` or `--cart-slot ...,rw` CRT must contain.
- `--control ADDR --save OUT.crt`: ue2emu's `cart-save OUT.crt`, then the CRT's flash compared with the state.

Exit 0 when every check passes, 1 when one fails, 2 on an error.
"""

import argparse
import json
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cartslot_common as cs  # noqa: E402

SECTOR_BANKS = 8

# base: the window the flash is written through; magic: (short, long) command offsets; ids and where they read.
CARTS = {
    'easyflash': dict(enter=[(0xDE02, 0x05)], leave=[(0xDE02, 0x04)], base=0x8000, romh_base=0xE000,
                      magic={'short': (0x555, 0x2AA), 'long': (0x5555, 0x2AAA)}, ids=(0x01, 0xA4), id_at=(0, 1),
                      banks=64),
    'megabyter': dict(enter=[(0xDE02, 0x03)], leave=[(0xDE02, 0x00)], base=0x8000,
                      magic={'short': (0xAAA, 0x555), 'long': (0xAAAA, 0x5555)}, ids=(0xC2, 0x58), id_at=(0, 1),
                      banks=128),
    'c64megacart': dict(enter=[(0xDF00, 0xC0)], leave=[(0xDF00, 0x00)], base=0xE000,
                        magic={'short': (0xAAA, 0x555), 'long': (0xAAAA, 0x5555)}, ids=(0x01, 0xD2), id_at=(0, 2),
                        banks=256),
    'gmod2': dict(enter=[(0xDE00, 0xC0)], leave=[(0xDE00, 0x00)], base=0x8000,
                  magic={'short': (0x555, 0x2AA), 'long': (0x5555, 0x2AAA)}, ids=(0x01, 0xA4), id_at=(0, 1),
                  banks=64),
}


class Flash:
    def __init__(self, rest, family, unlock):
        self.rest, self.family, self.c, self.unlock = rest, family, CARTS[family], unlock
        self.banked = 0

    def poke(self, addr, val):
        self.rest.poke(addr, val)

    def peek(self, addr):
        """A read in flash mode; GMod2 reads in 8 K mode."""
        if self.family != 'gmod2':
            return self.rest.peek1(addr)
        self.poke(0xDE00, self.banked & 0x3F)
        v = self.rest.peek1(addr)
        self.poke(0xDE00, 0xC0 | (self.banked & 0x3F))
        return v

    def enter(self):
        for addr, val in self.c['enter']:
            self.poke(addr, val)

    def leave(self):
        for addr, val in self.c['leave']:
            self.poke(addr, val)

    def bank(self, n):
        """Bank in `n` in flash mode (cartlib_switch_bank: EasyFlash also re-writes $DE02=$05)."""
        if self.family == 'easyflash':
            self.poke(0xDE00, n)
            self.poke(0xDE02, 0x05)
        elif self.family == 'megabyter':
            self.poke(0xDE00, n)
        elif self.family == 'c64megacart':
            self.poke(0xDF00, 0xC0 | ((n >> 8) & 0x3F))
            self.poke(0xDE00, n & 0xFF)
        else:
            self.poke(0xDE00, 0xC0 | (n & 0x3F))
        self.banked = n

    def magic_write(self, offset, val, base, unlock):
        """One command byte at flash `offset`: in the current bank (short), or with the bank register set to the
        offset's bank and put back (long)."""
        if unlock == 'short':
            self.poke(base + offset, val)
            return
        target = self.banked
        self.bank(offset >> 13)
        self.poke(base + (offset & 0x1FFF), val)
        self.bank(target)

    def command(self, byte, at=None, romh=False, unlock=None):
        """AA, 55, `byte`; the last byte at CPU address `at` in the current bank when given."""
        unlock = unlock or self.unlock
        base = self.c['romh_base'] if romh else self.c['base']
        m1, m2 = self.c['magic'][unlock]
        self.magic_write(m1, 0xAA, base, unlock)
        self.magic_write(m2, 0x55, base, unlock)
        if at is None:
            self.magic_write(m1, byte, base, unlock)
        else:
            self.poke(at, byte)

    def autoselect(self, romh=False, unlock=None):
        base = self.c['romh_base'] if romh else self.c['base']
        self.bank(0)
        self.command(0x90, romh=romh, unlock=unlock)
        ids = (self.peek(base + self.c['id_at'][0]), self.peek(base + self.c['id_at'][1]))
        self.poke(base, 0xF0)
        return ids

    def wait_erase(self, addr, timeout):
        """Poll until the chip reads 0xFF: returns (polls, DQ6 toggles, seconds). DQ5 set twice is a failure."""
        t0, polls, toggles, last = time.time(), 0, 0, None
        while True:
            v = self.peek(addr)
            polls += 1
            if last is not None and (v ^ last) & 0x40:
                toggles += 1
            if v == 0xFF:
                return polls, toggles, round(time.time() - t0, 2)
            if v & 0x20:
                v = self.peek(addr)
                if v == 0xFF:
                    return polls, toggles, round(time.time() - t0, 2)
                raise RuntimeError(f'erase timeout bit DQ5 at ${addr:04X}: {v:02x}')
            if time.time() - t0 > timeout:
                raise RuntimeError(f'erase at ${addr:04X} did not finish in {timeout} s (last {v:02x})')
            last = v

    def poll(self, addr, expected):
        """cartlib flash_poll: until DQ7 shows the data's bit 7; DQ5 set means a failed program."""
        for _ in range(200):
            v = self.peek(addr)
            if (v & 0x80) == (expected & 0x80):
                return v
            if v & 0x20:
                v = self.peek(addr)
                if (v & 0x80) == (expected & 0x80):
                    return v
                raise RuntimeError(f'program ${addr:04X}: DQ5 set, {v:02x} for {expected:02x}')
        raise RuntimeError(f'program ${addr:04X}: DQ7 never showed {expected:02x}')

    def program(self, bank, addr, value, romh=False):
        """AA 55 A0, the data at `addr` of `bank`, then the DQ7/DQ5 poll. EasyFlash unlocks in bank 0 and writes the
        data in the target bank (cartlib_flash_write_bank)."""
        if self.family == 'easyflash':
            self.bank(0)
            self.command(0xA0, romh=romh)
            self.bank(bank)
        else:
            self.bank(bank)
            self.command(0xA0, romh=romh)
        self.poke(addr, value)
        return self.poll(addr, value)

    def read_chip(self, which, banks):
        """8 K per bank as the flash holds it: ROML at $8000 in 8 K mode (EasyFlash in ULTIMAX), ROMH at $E000."""
        out = bytearray()
        for n in range(banks):
            if self.family == 'c64megacart':
                self.poke(0xDF00, (n >> 8) & 0x3F)
                self.poke(0xDE00, n & 0xFF)
            elif self.family == 'gmod2':
                self.poke(0xDE00, n & 0x3F)
            else:
                self.bank(n)
            out += self.rest.peek(0x8000 if which == 'roml' else 0xE000, cs.WINDOW)
        if self.family in ('c64megacart', 'gmod2'):
            self.enter()
        return out


def value_at(i):
    """Programmed data: never 0xFF (cartlib skips 0xFF), distinct neighbours."""
    return (0x5A + i * 0x3B) & 0x7F


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--url', required=True)
    ap.add_argument('--password')
    ap.add_argument('--family', required=True, choices=sorted(CARTS))
    ap.add_argument('--unlock', choices=('short', 'long'), default='short', help='command addresses (default short)')
    ap.add_argument('--bank', type=int, default=8, help='a bank of the sector to erase (default 8: sector 1)')
    ap.add_argument('--bytes', type=int, default=64, help='bytes to program per chip')
    ap.add_argument('--chip-erase', action='store_true', help='chip erase instead of the sector erase')
    ap.add_argument('--source', type=Path, help='CRT in the slot: banks outside the sector must be unchanged')
    ap.add_argument('--state', help='write PREFIX.roml.bin / PREFIX.romh.bin with the chips as read back')
    ap.add_argument('--control', help="ue2emu control address for cart-save, e.g. 127.0.0.1:6400")
    ap.add_argument('--save', type=Path, help='with --control: cart-save to this path and compare it with the chips')
    ap.add_argument('--timeout', type=float, default=300.0, help='wall-clock seconds an erase may take')
    ap.add_argument('--json', type=Path)
    args = ap.parse_args()

    rest = cs.Rest(args.url, args.password)
    report = {'family': args.family, 'unlock': args.unlock, 'checks': {}}
    checks = report['checks']
    c = CARTS[args.family]
    sector0 = args.bank - args.bank % SECTOR_BANKS
    sector = list(range(sector0, sector0 + SECTOR_BANKS))
    other = 'long' if args.unlock == 'short' else 'short'
    t0 = time.time()
    rest.wait_ready()
    rest.pause()
    saved = rest.peek(0x0000, 2)
    f = Flash(rest, args.family, args.unlock)
    romh = None
    try:
        rest.poke(0x0000, bytes([0x2F, 0x37]))
        f.enter()
        ids = f.autoselect()
        report['ids'] = [f'{b:02x}' for b in ids]
        checks['autoselect'] = ids == c['ids']
        other_ids = f.autoselect(unlock=other)
        report[f'ids_with_{other}_unlock'] = [f'{b:02x}' for b in other_ids]
        report[f'{other}_unlock_works'] = other_ids == c['ids']
        if args.family == 'easyflash':
            ids = f.autoselect(romh=True)
            report['romh_ids'] = [f'{b:02x}' for b in ids]
            checks['autoselect_romh'] = ids == c['ids']

        # erase
        base = c['base']
        if args.chip_erase:
            f.bank(0)
            f.command(0x80)
            f.command(0x10)
            report['erase'] = dict(zip(('polls', 'dq6_toggles', 'seconds'), f.wait_erase(base, args.timeout)))
            erased = list(range(c['banks']))
        else:
            f.bank(args.bank)
            f.command(0x80)
            f.command(0x30, at=base)
            report['erase'] = dict(zip(('polls', 'dq6_toggles', 'seconds'), f.wait_erase(base, args.timeout)))
            erased = sector
        checks['erase_status_toggled'] = report['erase']['dq6_toggles'] > 0
        romh_erased = False
        if args.family == 'easyflash':
            f.bank(0)
            f.command(0x80, romh=True)
            f.command(0x10, romh=True)
            report['romh_chip_erase'] = dict(zip(('polls', 'dq6_toggles', 'seconds'),
                                                 f.wait_erase(c['romh_base'], args.timeout)))
            romh_erased = True

        # program: first bytes of the target bank, then one byte near the end of each bank of the sector
        spread = min(8, args.bytes)
        places = [(args.bank, i) for i in range(args.bytes - spread)]
        places += [(sector[k], 0x1FFF - k * 0x111) for k in range(spread)]
        programmed, failed = {}, []
        for i, (bank, off) in enumerate(places):
            val = value_at(i)
            got = f.program(bank, base + off, val)
            programmed[('roml', bank, off)] = val
            if got != val:
                failed.append(f'roml bank {bank} +{off:04x}: {got:02x} for {val:02x}')
        if romh_erased:
            for i in range(args.bytes):
                val = value_at(i + 100)
                got = f.program(args.bank, c['romh_base'] + i, val, romh=True)
                programmed[('romh', args.bank, i)] = val
                if got != val:
                    failed.append(f'romh bank {args.bank} +{i:04x}: {got:02x} for {val:02x}')
        checks['program_poll'] = not failed
        report['program_failures'] = failed[:8]
        report['programmed_bytes'] = len(programmed)

        # read back
        roml = f.read_chip('roml', c['banks'])
        romh = f.read_chip('romh', c['banks']) if args.family == 'easyflash' else None
        source = cs.parse_crt(args.source.read_bytes()) if args.source else None
        bad = []
        for n in range(c['banks']):
            for which, chip in (('roml', roml), ('romh', romh)):
                if chip is None:
                    continue
                got = chip[n * cs.WINDOW:(n + 1) * cs.WINDOW]
                if (which == 'roml' and n in erased) or (which == 'romh' and romh_erased):
                    want = bytearray(b'\xFF' * cs.WINDOW)
                    for (w, b, off), val in programmed.items():
                        if w == which and b == n:
                            want[off] = val
                elif source is not None:
                    want = cs.window(source, n, which) or b'\xFF' * cs.WINDOW
                else:
                    continue
                if got != want:
                    diff = [i for i in range(cs.WINDOW) if got[i] != want[i]]
                    bad.append(f'{which} bank {n}: {len(diff)} bytes, first +{diff[0]:04x} {got[diff[0]]:02x} for '
                               f'{want[diff[0]]:02x}')
        checks['read_back'] = not bad
        report['read_back_differences'] = bad[:8]
        if args.state:
            Path(args.state + '.roml.bin').write_bytes(roml)
            if romh is not None:
                Path(args.state + '.romh.bin').write_bytes(romh)
        f.leave()
    finally:
        rest.poke(0x0000, saved)
        rest.resume()

    if args.control and args.save:
        ctl = cs.Control(args.control)
        try:
            report['cart_save'] = ctl.cmd(f'cart-save {args.save.resolve()}')
        finally:
            ctl.close()
        saved_crt = cs.parse_crt(args.save.read_bytes())
        same = cs.linear(saved_crt, c['banks'], 'roml') == roml
        if romh is not None:
            same = same and cs.linear(saved_crt, c['banks'], 'romh') == romh
        checks['cart_save_equals_flash'] = same

    report['rest_calls'] = rest.calls
    report['seconds'] = round(time.time() - t0, 1)
    if args.json:
        args.json.write_text(json.dumps(report, indent=1) + '\n')
    print(json.dumps(report, indent=1))
    ok = all(checks.values())
    print(f"{args.family}: {'PASS' if ok else 'FAIL'} " + ' '.join(f'{k}={v}' for k, v in checks.items())
          + f" {other}_unlock_works={report[f'{other}_unlock_works']}")
    return 0 if ok else 1


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (cs.RestError, RuntimeError, OSError, ValueError) as e:
        print(f'error: {e}', file=sys.stderr)
        sys.exit(2)
