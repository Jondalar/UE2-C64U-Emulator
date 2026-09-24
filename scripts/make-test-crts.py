#!/usr/bin/env python3
"""Build test cartridges (.crt), a PSID tune and a MUS file for the U64 cartridge logic (docs/status/carts.md).

    scripts/make-test-crts.py <output dir>

Each CRT holds a tiny 6502 program assembled here. It sets up the VIC itself (no KERNAL calls, so 8K, 16K and
ULTIMAX starts behave the same), prints `<NAME> START` on the top line, copies a test stub to RAM at $0C00 (visible in
every memory mode) and runs it there. The stub switches banks and modes through the cartridge's registers, checks
the bytes it expects and prints one line per check, then `<NAME> PASS` or `<NAME> FAIL` on line 24 and loops.

Every bank carries the same code, so a bank switch never pulls it away. Unused ROM bytes are fillers that name
their place: ROML of bank n is 0x40+n, ROMH 0x80+n (0x40/0x80 for bank 0). The ROML string `ROML BANK nn` sits at
$9F00 and the ROMH string at $BF00/$FF00.

c28-c31 are freeze tests: they print `PRESS FREEZE` and loop (c29-c31 switch their cart off first). Bank 0 carries a
freeze handler, in the window where the freezer maps itself in, that prints `<NAME> FROZEN` on line 22 when the freeze
button (USB F11, MATRIX_KEYB[10]) switches the cart in. c32-twomegabyter.crt holds four sparse 16 K banks (0, 1, $4D,
$FF) that each carry their own number.

File names sort in test order (c01-..., s01-..., s02-...); scripts/smoke-c64-carts.ctl runs them from the file
browser in that order.
"""

import struct
import sys
from pathlib import Path

# --- a small two-pass 6502 assembler ------------------------------------------------------------------------------

OPCODES = {
    'ADC': {'imm': 0x69}, 'AND': {'imm': 0x29}, 'ASL': {'acc': 0x0A, 'zp': 0x06}, 'BCC': {'rel': 0x90},
    'BCS': {'rel': 0xB0}, 'BEQ': {'rel': 0xF0}, 'BIT': {'abs': 0x2C}, 'BMI': {'rel': 0x30}, 'BNE': {'rel': 0xD0},
    'BPL': {'rel': 0x10}, 'CLC': {'imp': 0x18}, 'CLI': {'imp': 0x58}, 'CMP': {'imm': 0xC9, 'zp': 0xC5, 'abs': 0xCD},
    'CPX': {'imm': 0xE0}, 'CPY': {'imm': 0xC0}, 'DEC': {'zp': 0xC6, 'abs': 0xCE}, 'DEX': {'imp': 0xCA},
    'DEY': {'imp': 0x88}, 'EOR': {'imm': 0x49}, 'INC': {'zp': 0xE6, 'abs': 0xEE}, 'INX': {'imp': 0xE8},
    'INY': {'imp': 0xC8}, 'JMP': {'abs': 0x4C}, 'JSR': {'abs': 0x20},
    'LDA': {'imm': 0xA9, 'zp': 0xA5, 'abs': 0xAD, 'absx': 0xBD, 'absy': 0xB9}, 'LDX': {'imm': 0xA2, 'abs': 0xAE},
    'LDY': {'imm': 0xA0, 'abs': 0xAC}, 'LSR': {'acc': 0x4A}, 'NOP': {'imp': 0xEA}, 'ORA': {'imm': 0x09},
    'PHA': {'imp': 0x48}, 'PLA': {'imp': 0x68}, 'ROL': {'acc': 0x2A, 'zp': 0x26}, 'RTI': {'imp': 0x40},
    'RTS': {'imp': 0x60}, 'SEC': {'imp': 0x38}, 'SEI': {'imp': 0x78},
    'STA': {'zp': 0x85, 'abs': 0x8D, 'absx': 0x9D, 'absy': 0x99}, 'STX': {'zp': 0x86, 'abs': 0x8E},
    'STY': {'zp': 0x84, 'abs': 0x8C}, 'TAX': {'imp': 0xAA}, 'TXA': {'imp': 0x8A}, 'TXS': {'imp': 0x9A},
}
SIZE = {'imp': 1, 'acc': 1, 'imm': 2, 'zp': 2, 'rel': 2, 'abs': 3, 'absx': 3, 'absy': 3}


class Asm:
    """Instructions as text ('LDA #$12', 'STA $0400,X', 'BNE loop'), labels, byte data; resolved by `assemble`."""

    def __init__(self, org):
        self.org = org
        self.items = []  # (kind, payload)
        self.serial = 0

    def unique(self, stem):
        self.serial += 1
        return f'_{stem}{self.serial}'

    def label(self, name):
        self.items.append(('label', name))

    def __call__(self, line):
        for part in line.split(';'):
            part = part.strip()
            if part:
                self.items.append(('op', part))

    def data(self, blob):
        self.items.append(('data', bytes(blob)))

    def _parse(self, text):
        mnem, _, operand = text.partition(' ')
        mnem, operand = mnem.upper(), operand.strip()
        modes = OPCODES[mnem]
        if not operand:
            return mnem, ('acc' if 'acc' in modes else 'imp'), None
        if operand.upper() == 'A':
            return mnem, 'acc', None
        if operand.startswith('#'):
            return mnem, 'imm', operand[1:]
        if 'rel' in modes:
            return mnem, 'rel', operand
        index = None
        if operand.upper().endswith(',X') or operand.upper().endswith(',Y'):
            index, operand = operand[-1].upper(), operand[:-2]
        if index:
            return mnem, 'abs' + index.lower(), operand
        zp = operand.startswith('$') and len(operand) == 3 and 'zp' in modes
        return mnem, 'zp' if zp else 'abs', operand

    def _value(self, expr, labels):
        lo_hi = None
        if expr[0] in '<>':
            lo_hi, expr = expr[0], expr[1:]
        total = 0
        for term in expr.replace('-', '+-').split('+'):
            term = term.strip()
            neg = term.startswith('-')
            term = term.lstrip('-')
            if term.startswith('$'):
                v = int(term[1:], 16)
            elif term[0].isdigit():
                v = int(term)
            else:
                v = labels[term]
            total += -v if neg else v
        if lo_hi == '<':
            return total & 0xFF
        if lo_hi == '>':
            return (total >> 8) & 0xFF
        return total

    def assemble(self):
        labels, pc = {}, self.org
        for kind, payload in self.items:
            if kind == 'label':
                labels[payload] = pc
            elif kind == 'data':
                pc += len(payload)
            else:
                pc += SIZE[self._parse(payload)[1]]
        out, pc = bytearray(), self.org
        for kind, payload in self.items:
            if kind == 'data':
                out += payload
                pc += len(payload)
            elif kind == 'op':
                mnem, mode, operand = self._parse(payload)
                out.append(OPCODES[mnem][mode])
                if mode in ('imm', 'zp'):
                    out.append(self._value(operand, labels) & 0xFF)
                elif mode == 'rel':
                    delta = self._value(operand, labels) - (pc + 2)
                    assert -128 <= delta <= 127, f'branch out of range: {payload}'
                    out.append(delta & 0xFF)
                elif mode.startswith('abs'):
                    out += struct.pack('<H', self._value(operand, labels) & 0xFFFF)
                pc += SIZE[mode]
        self.labels = labels
        return bytes(out)


def screen_codes(text):
    """Upper-case screen codes: '@A-Z[\\]^_' -> 0x00-0x1F, ' '..'?' unchanged."""
    out = bytearray()
    for ch in text.upper():
        c = ord(ch)
        out.append(c - 0x40 if 0x40 <= c <= 0x5F else c)
    return bytes(out)


# --- test stub helpers (all code runs at $0C00 unless noted) -----------------------------------------------------

FAILS = '$FB'  # zero-page failure counter
STUB = 0x0C00


def say(a, row, text):
    """Print `text` at `row`."""
    string, done = a.unique('str'), a.unique('said')
    a(f'LDX #0')
    loop, end = a.unique('say'), a.unique('end')
    a.label(loop)
    a(f'LDA {string},X; BEQ {end}; STA ${0x0400 + row * 40:04X},X; INX; BNE {loop}')
    a.label(end)
    a(f'JMP {done}')
    a.label(string)
    a.data(screen_codes(text) + b'\0')
    a.label(done)


def say_from(a, row, addr, length=12):
    """Copy `length` screen codes from `addr` (a ROM window) to `row`, column 20."""
    loop = a.unique('copy')
    a('LDX #0')
    a.label(loop)
    a(f'LDA ${addr:04X},X; STA ${0x0400 + row * 40 + 20:04X},X; INX; CPX #{length}; BNE {loop}')


def check(a, row, addr, value, text, equal=True):
    """Compare the byte at `addr` with `value`; print `text OK` or `text BAD` at `row` and count failures."""
    bad, done = a.unique('bad'), a.unique('chk')
    a(f'LDA ${addr:04X}; CMP #${value:02X}')
    a(f'{"BNE" if equal else "BEQ"} {bad}')
    say(a, row, f'{text} OK')
    a(f'JMP {done}')
    a.label(bad)
    a(f'INC {FAILS}')
    say(a, row, f'{text} BAD')
    a.label(done)


def result(a, name):
    fail, hang = a.unique('fail'), a.unique('hang')
    a(f'LDA {FAILS}; BNE {fail}')
    say(a, 24, f'{name} PASS')
    a(f'JMP {hang}')
    a.label(fail)
    say(a, 24, f'{name} FAIL')
    a.label(hang)
    a(f'JMP {hang}')


def boot(org, stub, name, ultimax):
    """ROM code at `org`: CBM80 header (8K/16K) or reset vector target (ULTIMAX), VIC setup, screen clear, the start
    line, then the stub copied to $0C00 and entered."""
    a = Asm(org)
    if not ultimax:
        a.data(struct.pack('<HH', org + 9, org + 9) + b'\xC3\xC2\xCD80')
    a.label('start')
    a('SEI; LDX #$FF; TXS')
    a('LDA #$1B; STA $D011; LDA #$08; STA $D016; LDA #$15; STA $D018; LDA #$06; STA $D020; STA $D021')
    a('LDA #0; STA $FB; LDX #0')
    a.label('clear')
    a('LDA #$20; STA $0400,X; STA $0500,X; STA $0600,X; STA $0700,X')
    a('LDA #$01; STA $D800,X; STA $D900,X; STA $DA00,X; STA $DB00,X; INX; BNE clear')
    a('LDX #0')
    a.label('title')
    a('LDA name,X; BEQ copy; STA $0400,X; INX; BNE title')
    a.label('copy')
    a('LDX #0')
    a.label('copyloop')
    a('LDA stub,X; STA $0C00,X; LDA stub+256,X; STA $0D00,X; LDA stub+512,X; STA $0E00,X; INX; BNE copyloop')
    a('JMP $0C00')
    a.label('rti')
    a('RTI')
    a.label('name')
    a.data(screen_codes(f'{name} START') + b'\0')
    a.label('stub')
    assert len(stub) <= 768, f'{name}: stub of {len(stub)} bytes'
    a.data(stub.ljust(768, b'\xEA'))
    code = a.assemble()
    return code, a.labels


# --- cartridge images ---------------------------------------------------------------------------------------------

def bank_image(bank, code, ultimax):
    """16 K of bank `bank`: fillers, bank strings and marker bytes, the code at $8000 (or $E000 for ULTIMAX)."""
    img = bytearray([0x40 + bank]) * 0x2000 + bytearray([0x80 + bank]) * 0x2000
    for base, name in ((0x0000, 'ROML'), (0x2000, 'ROMH')):
        text = screen_codes(f'{name} BANK {bank:02}')
        img[base + 0x1F00:base + 0x1F00 + len(text)] = text
    start = 0x2000 if ultimax else 0x0000
    assert len(code) <= 0x1E00, f'code of {len(code)} bytes'
    img[start:start + len(code)] = code
    return img


def crt_file(hw_type, exrom, game, name, chips, subtype=0):
    """A CRT: header (crt format 1.1) and CHIP packets (bank, load address, bytes, chip type)."""
    header = b'C64 CARTRIDGE   ' + struct.pack('>IHHBBB5x', 0x40, 0x0101, hw_type, exrom, game, subtype)
    header += name.upper().encode()[:32].ljust(32, b'\0')
    body = b''
    for bank, load, data, chip_type in chips:
        body += b'CHIP' + struct.pack('>IHHHH', 0x10 + len(data), chip_type, bank, load, len(data)) + bytes(data)
    return header + body


def build(name, hw_type, exrom, game, stub_fn, banks=1, layout='8k', ultimax=False, subtype=0, extra=(), chip_type=0,
          patch=None):
    """layout: '8k' one $8000 8 K chip per bank; '16k' one $8000 16 K chip; 'split' $8000 and $A000 8 K chips;
    'romh' one $E000 8 K chip. `patch(image, bank)` edits each 16 K bank image before it is cut into chips."""
    stub_asm = Asm(STUB)
    stub_fn(stub_asm)
    stub = stub_asm.assemble()
    code, _ = boot(0xE000 if ultimax else 0x8000, stub, name, ultimax)
    chips = []
    for bank in range(banks):
        img = bank_image(bank, code, ultimax)
        if ultimax:
            # RESET to the code, NMI and IRQ to an RTI (the one byte after the code's JMP $0C00).
            rti = 0xE000 + code.index(bytes([0x4C, 0x00, 0x0C, 0x40])) + 3
            img[0x3FFA:0x4000] = struct.pack('<HHH', rti, 0xE000, rti)
        if patch:
            patch(img, bank)
        if layout == '8k':
            chips.append((bank, 0x8000, img[:0x2000], chip_type))
        elif layout == '16k':
            chips.append((bank, 0x8000, img, chip_type))
        elif layout == 'split':
            chips.append((bank, 0x8000, img[:0x2000], chip_type))
            chips.append((bank, 0xA000, img[0x2000:], chip_type))
        elif layout == 'romh':
            chips.append((bank, 0xE000, img[0x2000:], chip_type))
    return crt_file(hw_type, exrom, game, name, list(chips) + list(extra), subtype)


# --- the tests ----------------------------------------------------------------------------------------------------

def normal8k(a):
    check(a, 2, 0x9FF0, 0x40, 'ROML $8000')
    say_from(a, 2, 0x9F00)
    result(a, 'NORMAL 8K')


def normal16k(a):
    check(a, 2, 0x9FF0, 0x40, 'ROML $8000')
    check(a, 3, 0xBFF0, 0x80, 'ROMH $A000')
    result(a, 'NORMAL 16K')


def ultimax(a):
    check(a, 2, 0xFFF0, 0x80, 'ROMH $E000')
    a('LDA #$55; STA $2000')
    check(a, 3, 0x2000, 0x55, 'OPEN BUS $2000', equal=False)
    result(a, 'ULTIMAX')


def ocean(name):
    def test(a):
        a('LDA #$05; STA $DE00')
        check(a, 2, 0x9FF0, 0x45, 'BANK 5')
        say_from(a, 2, 0x9F00)
        a('LDA #$02; STA $DE00')
        check(a, 3, 0x9FF0, 0x42, 'BANK 2')
        result(a, name)
    return test


def magicdesk(a):
    a('LDA #$03; STA $DE00')
    check(a, 2, 0x9FF0, 0x43, 'BANK 3')
    a('LDA #$5D; STA $9E10')
    a('LDA #$80; STA $DE00')
    check(a, 3, 0x9E10, 0x5D, 'BIT 7 OFF')
    a('LDA #$06; STA $DE00')
    check(a, 4, 0x9FF0, 0x46, 'BANK 6 ON')
    result(a, 'MAGIC DESK')


def easyflash(a):
    a('LDA #$07; STA $DE02; LDA #$05; STA $DE00')
    check(a, 2, 0x9FF0, 0x45, 'ROML BANK 5')
    check(a, 3, 0xBFF0, 0x85, 'ROMH BANK 5')
    say_from(a, 3, 0xBF00)
    # Written by an earlier start of this CRT since the last load: the flash lives on in the firmware's DDR.
    a('LDA #$01; STA $DE00')
    kept, fresh = a.unique('kept'), a.unique('fresh')
    a(f'LDA $8123; CMP #$A5; BNE {fresh}')
    say(a, 4, 'FLASH KEPT OVER RESET')
    a(f'JMP {kept}')
    a.label(fresh)
    say(a, 4, 'FLASH FRESH')
    a.label(kept)
    a('LDA #$05; STA $DE02; LDA #$65; STA $DE09; LDA #$A5; STA $8123; LDA #$3C; STA $F456')
    a('LDA #$07; STA $DE02')
    check(a, 5, 0x8123, 0xA5, 'FLASH WRITE ROML')
    check(a, 6, 0xB456, 0x3C, 'FLASH WRITE ROMH')
    a('LDA #$05; STA $DE02; LDA #$11; STA $8124; LDA #$07; STA $DE02')
    check(a, 7, 0x8124, 0x41, 'NO WRITE WITHOUT KEY')
    a('LDA #$5A; STA $DF80')
    check(a, 8, 0xDF80, 0x5A, 'IO2 RAM')
    a('LDA #$04; STA $DE02')
    result(a, 'EASYFLASH')


def gmod2(a):
    a('LDA #$05; STA $DE00')
    check(a, 2, 0x9FF0, 0x45, 'BANK 5')
    a('JMP eetest')
    # send: $F2/$F3 hold the bits left-aligned, X the count; CS high, DI with CLK low, then CLK high.
    a.label('send')
    a('LDA #$40; ASL $F3; ROL $F2; BCC send0; ORA #$10')
    a.label('send0')
    a('STA $DE00; ORA #$20; STA $DE00; DEX; BNE send; LDA #$40; STA $DE00; RTS')
    # read16: 16 bits of DO (bit 7 of $DE00) into $F0/$F1, one rising CLK edge each.
    a.label('read16')
    a('LDX #16')
    a.label('readbit')
    a('LDA #$60; STA $DE00; LDA $DE00; ASL A; ROL $F1; ROL $F0; LDA #$40; STA $DE00; DEX; BNE readbit; RTS')
    a.label('deselect')
    a('LDA #$00; STA $DE00; RTS')

    def command(bits, count):
        left = bits << (16 - count)
        a(f'LDA #${left >> 8:02X}; STA $F2; LDA #${left & 0xFF:02X}; STA $F3; LDX #{count}; JSR send')

    a.label('eetest')
    command(0b1_10_0000000000, 13)  # READ word 0
    a('JSR read16; JSR deselect')
    check(a, 3, 0x00F0, 0x55, 'EEPROM WORD 0 HI')
    check(a, 4, 0x00F1, 0x45, 'EEPROM WORD 0 LO')
    command(0b1_00_1100000000, 13)  # EWEN
    a('JSR deselect')
    command(0b1_01_0000000001, 13)  # WRITE word 1
    command(0x4F4B, 16)
    a('JSR deselect')
    command(0b1_10_0000000001, 13)  # READ word 1
    a('JSR read16; JSR deselect')
    check(a, 5, 0x00F0, 0x4F, 'EEPROM WRITE HI')
    check(a, 6, 0x00F1, 0x4B, 'EEPROM WRITE LO')
    a('LDA #$02; STA $DE00')
    check(a, 7, 0x9FF0, 0x42, 'ROM AFTER EEPROM')
    result(a, 'GMOD2')


def action_replay(a):
    a('LDA #$10; STA $DE00')
    check(a, 2, 0x9FF0, 0x42, 'BANK 2')
    check(a, 3, 0xDFF0, 0x42, 'IO2 ROM MIRROR')
    a('LDA #$20; STA $DE00; LDA #$77; STA $9E10')
    check(a, 4, 0x9E10, 0x77, 'RAM MODE')
    a('LDA #$00; STA $DE00')
    check(a, 5, 0x9E10, 0x40, 'ROM AGAIN')
    a('LDA #$04; STA $DE00')
    check(a, 6, 0x9E10, 0x77, 'OFF: C64 RAM')
    result(a, 'ACTION REPLAY')


def retro_replay(a):
    a('LDA #$88; STA $DE00')
    check(a, 2, 0x9FF0, 0x45, 'BANK 5')
    a('LDA #$02; STA $DE01; LDA #$A8; STA $DE00; LDA #$66; STA $9E10')
    check(a, 3, 0x9E10, 0x66, 'RAM BANK 5')
    a('LDA #$A0; STA $DE00')
    check(a, 4, 0x9E10, 0x66, 'RAM BANK 4 DIFFERS', equal=False)
    a('LDA #$00; STA $DE00')
    check(a, 5, 0x9FF0, 0x40, 'ROM BANK 0')
    result(a, 'RETRO REPLAY')


def nordic(a):
    a('LDA #$10; STA $DE00')
    check(a, 2, 0x9FF0, 0x42, 'BANK 2')
    check(a, 3, 0xDE00, 0x50, 'STATUS REGISTER')
    a('LDA #$22; STA $DE00; LDA #$5A; STA $A010')
    check(a, 4, 0xA010, 0x5A, 'MODE 110 RAM $A000')
    check(a, 5, 0x9FF0, 0x40, 'MODE 110 ROML')
    result(a, 'ATOMIC POWER')


def fc3(a):
    a('LDA #$99; STA $9E10')
    a('LDA #$42; STA $DFFF')
    check(a, 2, 0x9FF0, 0x42, 'ROML BANK 2')
    check(a, 3, 0xBFF0, 0x82, 'ROMH BANK 2')
    a('LDA #$53; STA $DFFF')
    check(a, 4, 0xFFF0, 0x83, 'ULTIMAX BANK 3')
    a('LDA #$70; STA $DFFF')
    check(a, 5, 0x9E10, 0x99, 'LINES OFF: C64 RAM')
    a('LDA #$C0; STA $DFFF')
    result(a, 'FINAL CARTRIDGE III')


def ss5(a):
    a('LDA #$06; STA $DE00')
    check(a, 2, 0x9FF0, 0x41, 'ROML BANK 1')
    check(a, 3, 0xBFF0, 0x81, 'ROMH BANK 1')
    a('LDA #$00; STA $DE00; LDA #$5C; STA $9E10')
    check(a, 4, 0x9E10, 0x5C, 'RAM AT $8000')
    a('LDA #$12; STA $DE00')
    check(a, 5, 0x9FF0, 0x42, 'BANK 2')
    a('LDA #$0A; STA $DE00')
    result(a, 'SUPER SNAPSHOT 5')


def kcs(a):
    a('LDA #$31; STA $9E10')
    check(a, 2, 0xBFF0, 0x80, '16K')
    a('LDA $DE00')
    check(a, 3, 0xBFF0, 0x80, 'IO1 READ: 8K', equal=False)
    a('LDA #$00; STA $DE80')
    check(a, 4, 0xBFF0, 0x80, 'DE80 WRITE: 16K')
    a('LDA #$5A; STA $DF05')
    check(a, 5, 0xDF85, 0x5A, 'IO2 RAM MIRROR')
    a('LDA $DE02')
    check(a, 6, 0x9E10, 0x31, 'IO1 READ: OFF')
    result(a, 'KCS POWER')


def final1(a):
    a('LDA #$33; STA $9E10; LDA $DE00')
    check(a, 2, 0x9E10, 0x33, 'IO1: OFF')
    a('LDA $DF00')
    check(a, 3, 0x9FF0, 0x40, 'IO2: ON')
    result(a, 'FINAL CARTRIDGE')


def epyx(a):
    a('LDA $DE00')
    check(a, 2, 0xDFF0, 0x40, 'IO2 ROM')
    a('LDA #$21; STA $9E10; LDY #4')
    delay = a.unique('delay')
    a.label(delay)
    a(f'DEX; BNE {delay}; DEY; BNE {delay}')
    check(a, 3, 0x9E10, 0x21, 'TIMED OUT')
    a('LDA $DE00')
    check(a, 4, 0x9FF0, 0x40, 'IO1 READ: ON')
    result(a, 'EPYX FASTLOAD')


def westermann(a):
    check(a, 2, 0xBFF0, 0x80, '16K')
    a('LDA $DF00')
    check(a, 3, 0xBFF0, 0x80, 'IO2 READ: 8K', equal=False)
    check(a, 4, 0x9FF0, 0x40, 'ROML STAYS')
    result(a, 'WESTERMANN')


def sbasic(a):
    check(a, 2, 0xBFF0, 0x80, '8K AT RESET', equal=False)
    a('STA $DE00')
    check(a, 3, 0xBFF0, 0x80, 'IO1 WRITE: 16K')
    a('LDA $DE00')
    check(a, 4, 0xBFF0, 0x80, 'IO1 READ: 8K', equal=False)
    result(a, 'SIMONS BASIC')


def system3(a):
    a('LDA #$03; STA $DE00')
    check(a, 2, 0x9FF0, 0x43, 'BANK 3')
    a('LDA $DE00')
    check(a, 3, 0x9FF0, 0x40, 'IO1 READ: BANK 0')
    result(a, 'C64 GAME SYSTEM')


def zaxxon(a):
    a('LDA $9000')
    check(a, 2, 0xBFF0, 0x81, '$9000 READ: ROMH 1')
    a('LDA $8000')
    check(a, 3, 0xBFF0, 0x80, '$8000 READ: ROMH 0')
    result(a, 'ZAXXON')


def megabyter(a):
    a('LDA #$06; STA $DE00')
    check(a, 2, 0x9FF0, 0x46, 'BANK 6')
    a('LDA #$27; STA $9E10; LDA #$02; STA $DE02')
    check(a, 3, 0x9E10, 0x27, 'EXROM OFF')
    a('LDA #$00; STA $DE02')
    check(a, 4, 0x9FF0, 0x46, 'ON AGAIN')
    result(a, 'MEGABYTER')


def supergames(a):
    a('LDA #$02; STA $DF00')
    check(a, 2, 0x9FF0, 0x42, 'ROML BANK 2')
    check(a, 3, 0xBFF0, 0x82, 'ROMH BANK 2')
    a('LDA #$29; STA $9E10; LDA #$04; STA $DF00')
    check(a, 4, 0x9E10, 0x29, 'OFF')
    a('LDA #$00; STA $DF00')
    check(a, 5, 0x9FF0, 0x40, 'ON BANK 0')
    result(a, 'SUPER GAMES')


def comal80(a):
    a('LDA #$03; STA $DE00')
    check(a, 2, 0x9FF0, 0x43, 'ROML BANK 3')
    check(a, 3, 0xBFF0, 0x83, 'ROMH BANK 3')
    result(a, 'COMAL 80')


def pagefox(a):
    a('LDA #$24; STA $9E20; LDA #$02; STA $DE80')
    check(a, 2, 0x9FF0, 0x41, 'BANK 1')
    a('LDA #$08; STA $DE80; LDA #$42; STA $9E10')
    check(a, 3, 0x9E10, 0x42, 'RAM MODE')
    a('LDA #$10; STA $DE80')
    check(a, 4, 0x9E20, 0x24, 'OFF: C64 RAM')
    result(a, 'PAGEFOX')


def blackbox3(a):
    check(a, 2, 0x9FF0, 0x40, '8K')
    a('LDA #$34; STA $9E10; STA $DE00')
    check(a, 3, 0x9E10, 0x34, 'IO1 WRITE: OFF')
    a('STA $DF00')
    check(a, 4, 0x9FF0, 0x40, 'IO2 WRITE: ON')
    result(a, 'BLACKBOX V3')


def blackbox4(a):
    check(a, 2, 0xBFF0, 0x80, '16K')
    a('LDA #$35; STA $9E10; LDA $DF00')
    check(a, 3, 0x9E10, 0x35, 'IO2 READ: OFF')
    a('LDA $DE00')
    check(a, 4, 0xBFF0, 0x80, 'IO1 READ: ON')
    result(a, 'BLACKBOX V4')


def blackbox8(a):
    a('LDA #$08; STA $DF00')
    check(a, 2, 0x9FF0, 0x42, 'ROML BANK 2')
    check(a, 3, 0xBFF0, 0x82, 'ROMH BANK 2')
    a('LDA #$02; STA $DF00')
    check(a, 4, 0xBFF0, 0x80, '8K BANK 0', equal=False)
    result(a, 'BLACKBOX V8')


def blackbox9(a):
    a('STA $DE00')
    check(a, 2, 0x9FF0, 0x41, '16K BANK 1')
    a('STA $DE80')
    check(a, 3, 0x9FF0, 0x40, 'WRITE DE80: BANK 0')
    a('LDA $DE80')
    check(a, 4, 0x9FF0, 0x41, 'READ DE80: BANK 1')
    a('STA $DE01')
    check(a, 5, 0xBFF0, 0x81, 'WRITE DE01: 8K', equal=False)
    result(a, 'BLACKBOX V9')


def freeze_ready(off=''):
    """Switch the cart off with `off` (nothing for the AR), print PRESS FREEZE and loop."""
    def stub(a):
        if off:
            a(off)
        say(a, 2, 'PRESS FREEZE')
        hang = a.unique('hang')
        a.label(hang)
        a(f'JMP {hang}')
    return stub


def freezer(text, window, vectors=(0xF800, 0xF800, 0xF800)):
    """A freeze handler at $F800 printing `text` on line 22. freeze_act switches bank 0 in as ULTIMAX
    (all_carts_v5.vhd:174-179), so $E000-$FFFF is bank 0's `window`: offset $0000 for 8 K banks (the AR,
    rom_mode "00", 530-533), $2000 (ROMH) for 16 K banks (KCS 600-605, SS5 562-563, FC 625-627; rom_mode "01").
    The handler touches neither IO1 nor IO2: an FC access there would unfreeze it (615-623). `vectors` are NMI, RESET
    and IRQ at $FFFA."""
    def patch(img, bank):
        if bank:
            return
        a = Asm(0xF800)
        a('SEI; LDX #0')
        a.label('loop')
        a(f'LDA text,X; BEQ done; STA ${0x0400 + 22 * 40:04X},X; INX; BNE loop')
        a.label('done')
        a('JMP done')
        a.label('text')
        a.data(screen_codes(text) + b'\0')
        code = a.assemble()
        at = window + 0x1800
        assert len(set(img[at:at + len(code)])) == 1, 'the freeze handler overlaps code'
        img[at:at + len(code)] = code
        img[window + 0x1FFA:window + 0x2000] = struct.pack('<HHH', *vectors)
    return patch


def twomegabyter(a):
    for row, bank in ((2, 0x01), (3, 0x4D), (4, 0xFF)):
        a(f'LDA #${bank:02X}; STA $DE00')
        check(a, row, 0x9FF0, bank ^ 0x5A, 'ROML')
        say_from(a, row, 0x9F00, 13)
    check(a, 5, 0xBFF0, 0xFF ^ 0xA5, 'ROMH')
    say_from(a, 5, 0xBF00, 13)
    a('LDA #$02; STA $DE00')
    check(a, 6, 0x9FF0, 0xFF, 'BANK 2 EMPTY')
    a('LDA #$4D; STA $DE00; LDA #$01; STA $DE02')
    check(a, 7, 0xBFF0, 0x4D ^ 0xA5, 'MODE 1: 8K', equal=False)
    a('LDA #$37; STA $9E10; LDA #$03; STA $DE02')
    check(a, 8, 0x9E10, 0x37, 'MODE 3: OFF')
    a('LDA #$00; STA $DE02')
    check(a, 9, 0xBFF0, 0x4D ^ 0xA5, 'MODE 0: 16K')
    result(a, 'TWOMEGABYTER')


def twomegabyter_crt():
    """CRT type 87, TwoMegabyter: C64_CARTRIDGE_TYPE MEGABYTER variant 1 (c64_crt.cc:97, 612-613), 256 banks of 16 K
    (all_carts_v5.vhd:364-384). Banks 0, 1, $4D and $FF only; a bank carries `bank ^ $5A` at $9FF0 and `bank ^ $A5`
    at $BFF0 and its number in the strings; the firmware fills the rest with $FF (c64_crt.cc:336)."""
    stub = Asm(STUB)
    twomegabyter(stub)
    code, _ = boot(0x8000, stub.assemble(), 'TWOMEGABYTER', False)
    chips = []
    for bank in (0x00, 0x01, 0x4D, 0xFF):
        img = bytearray([0xEA]) * 0x4000
        for base, name, mark in ((0x0000, 'ROML', 0x5A), (0x2000, 'ROMH', 0xA5)):
            text = screen_codes(f'{name} BANK {bank:03}')
            img[base + 0x1F00:base + 0x1F00 + len(text)] = text
            img[base + 0x1FF0] = bank ^ mark
        if bank == 0:
            img[:len(code)] = code
        chips.append((bank, 0x8000, img, 0))
    return crt_file(87, 0, 0, 'TWOMEGABYTER', chips)


EEPROM_CHUNK = (0, 0xDE00, b'UE' + b'\xFF' * 0x7FE, 0)


def tests():
    """(file name, CRT bytes)."""
    # Zaxxon: one 4 K ROML chip (mirrored to 8 K and into bank 1 by the firmware) and a ROMH chip per bank.
    stub = Asm(STUB)
    zaxxon(stub)
    code, _ = boot(0x8000, stub.assemble(), 'ZAXXON', False)
    zaxxon_img = [bank_image(b, code, False) for b in range(2)]
    zaxxon_crt = crt_file(18, 0, 0, 'ZAXXON', [
        (0, 0x8000, zaxxon_img[0][:0x1000], 0), (0, 0xA000, zaxxon_img[0][0x2000:], 0),
        (1, 0xA000, zaxxon_img[1][0x2000:], 0)])
    return [
        ('c01-normal-8k.crt', build('NORMAL 8K', 0, 0, 1, normal8k)),
        ('c02-normal-16k.crt', build('NORMAL 16K', 0, 0, 0, normal16k, layout='split')),
        ('c03-ultimax.crt', build('ULTIMAX', 0, 1, 0, ultimax, layout='romh', ultimax=True)),
        ('c04-ocean.crt', build('OCEAN', 5, 0, 1, ocean('OCEAN'), banks=8)),
        ('c05-magic-desk.crt', build('MAGIC DESK', 19, 0, 1, magicdesk, banks=8)),
        ('c06-easyflash.crt', build('EASYFLASH', 32, 1, 0, easyflash, banks=8, layout='split', ultimax=True,
                                    chip_type=2)),
        ('c07-gmod2.crt', build('GMOD2', 60, 0, 1, gmod2, banks=8, extra=[EEPROM_CHUNK])),
        ('c08-action-replay.crt', build('ACTION REPLAY', 1, 0, 1, action_replay, banks=4)),
        ('c09-retro-replay.crt', build('RETRO REPLAY', 36, 0, 1, retro_replay, banks=8)),
        ('c10-final-cartridge-3.crt', build('FINAL CARTRIDGE III', 3, 0, 0, fc3, banks=4, layout='16k')),
        ('c11-super-snapshot-5.crt', build('SUPER SNAPSHOT 5', 20, 1, 0, ss5, banks=4, layout='16k', ultimax=True)),
        ('c12-kcs-power.crt', build('KCS POWER', 2, 0, 0, kcs, layout='16k')),
        ('c13-final-cartridge.crt', build('FINAL CARTRIDGE', 13, 0, 0, final1, layout='16k')),
        ('c14-epyx-fastload.crt', build('EPYX FASTLOAD', 10, 0, 1, epyx)),
        ('c15-westermann.crt', build('WESTERMANN', 11, 0, 0, westermann, layout='16k')),
        ('c16-simons-basic.crt', build('SIMONS BASIC', 4, 0, 0, sbasic, layout='split')),
        ('c17-c64-game-system.crt', build('C64 GAME SYSTEM', 15, 0, 1, system3, banks=4)),
        ('c18-zaxxon.crt', zaxxon_crt),
        ('c19-megabyter.crt', build('MEGABYTER', 86, 0, 1, megabyter, banks=8)),
        ('c20-super-games.crt', build('SUPER GAMES', 8, 0, 0, supergames, banks=4, layout='16k')),
        ('c21-comal-80.crt', build('COMAL 80', 21, 0, 0, comal80, banks=4, layout='16k')),
        ('c22-pagefox.crt', build('PAGEFOX', 53, 0, 0, pagefox, banks=4, layout='16k')),
        ('c23-blackbox-v3.crt', build('BLACKBOX V3', 65, 0, 1, blackbox3)),
        ('c24-blackbox-v4.crt', build('BLACKBOX V4', 66, 0, 0, blackbox4, layout='16k')),
        ('c25-blackbox-v8.crt', build('BLACKBOX V8', 64, 0, 0, blackbox8, banks=4, layout='16k')),
        ('c26-blackbox-v9.crt', build('BLACKBOX V9', 71, 1, 0, blackbox9, banks=2, layout='16k', ultimax=True)),
        ('c27-atomic-power.crt', build('ATOMIC POWER', 9, 0, 1, nordic, banks=8)),
        ('c28-ar-freeze.crt', build('AR FREEZE', 1, 0, 1, freeze_ready(), banks=4,
                                    patch=freezer('ACTION REPLAY FROZEN', 0x0000))),
        # KCS off by an IO1 read with address bit 1 set (576-579); frozen: mode "010", ULTIMAX (600-605).
        ('c29-kcs-freeze.crt', build('KCS FREEZE', 2, 0, 0, freeze_ready('LDA $DE02'), layout='16k',
                                     patch=freezer('KCS FROZEN', 0x2000))),
        # SS5 off by bit 3 of $DE00 (558-560); frozen: cart_en on, mode "000", ULTIMAX (174-178, 562-563). It starts in
        # ULTIMAX, so its RESET vector stays on the boot code.
        ('c30-ss5-freeze.crt', build('SS5 FREEZE', 20, 1, 0, freeze_ready('LDA #$08; STA $DE00'), layout='16k',
                                     ultimax=True, patch=freezer('SUPER SNAPSHOT FROZEN', 0x2000,
                                                                 (0xF800, 0xE000, 0xF800)))),
        # FC off by an IO1 access (615-618); frozen: ULTIMAX while freeze_act (625-627).
        ('c31-fc-freeze.crt', build('FC FREEZE', 13, 0, 0, freeze_ready('LDA $DE00'), layout='16k',
                                    patch=freezer('FINAL CARTRIDGE FROZEN', 0x2000))),
        ('c32-twomegabyter.crt', twomegabyter_crt()),
    ]


# --- SID and MUS ---------------------------------------------------------------------------------------------------

def psid():
    """PSID v2, one song, loaded at $1000 (address in the first two data bytes): init sets voice 1 (triangle, gate), play sweeps its frequency every frame."""
    a = Asm(0x1000)
    a('JMP init; JMP play')
    a.label('init')
    a('LDA #$0F; STA $D418; LDA #$09; STA $D405; LDA #$F0; STA $D406')
    a('LDA #$1C; STA $D401; LDA #$00; STA $D400; STA counter; LDA #$11; STA $D404; RTS')
    a.label('play')
    a('INC counter; LDA counter; STA $D400; LDA counter; AND #$3F; BNE done')
    a('LDA $D401; EOR #$04; STA $D401')
    a.label('done')
    a('RTS')
    a.label('counter')
    a.data(b'\0')
    data = a.assemble()

    def field(text):
        return text.encode().ljust(32, b'\0')

    header = b'PSID' + struct.pack('>HHHHHHHI', 2, 0x7C, 0x0000, 0x1000, 0x1003, 1, 1, 0)
    header += field('UE2EMU TEST TUNE') + field('UE2EMU') + field('2026 UE2EMU')
    header += struct.pack('>HBBBB', 0x0014, 0, 0, 0, 0)
    assert len(header) == 0x7C
    return header + struct.pack('<H', 0x1000) + data


def mus():
    """Compute's Sidplayer MUS: load address, three voice lengths, each voice a HLT command, then empty text."""
    voice = bytes([0x01, 0x4F])
    return struct.pack('<HHHH', 0x0000, len(voice), len(voice), len(voice)) + voice * 3 + b'\0'


def main():
    if len(sys.argv) != 2:
        sys.exit(f'usage: {sys.argv[0]} <output dir>')
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    files = tests() + [('s01-tune.sid', psid()), ('s02-tune.mus', mus())]
    for name, blob in files:
        (out / name).write_bytes(blob)
    print(' '.join(name for name, _ in files))


if __name__ == '__main__':
    main()
