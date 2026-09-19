#!/usr/bin/env python3
"""Build uci-probe.prg, the UCI round-trip probe of scripts/smoke-uci.ctl (issue #2).

    scripts/make-uci-probe.py <output .prg>

A BASIC line `10 SYS2061` and a 6502 program that does, 32 times, what UltimateDemo2026's UCI library does on
start-up: an ABORT (`uii_detect()`), then at once CTRL_CMD_GET_HWINFO (`04 28 00`) with the PUSH written as
"read the control register, OR 1, write it back" (`uii_sendcommand()`), then data, status and DATA_ACC. A reply
whose status does not start with `00` counts as a failure. It prints `UCI OK` or `UCI FAIL` and the number of
failed replies in hex, then returns to BASIC.

The ABORT reaches the firmware as an interrupt, and its HANDSHAKE_RESET rewinds the command buffer. When the
firmware handled it only after the C64 had written the next command, that command arrived empty (`Null command.` on
the console) and the reply had no status.
"""
import importlib.util
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location('make_test_crts', Path(__file__).with_name('make-test-crts.py'))
crts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(crts)

CONTROL, COMMAND, RESPONSE, STATUS = '$DF1C', '$DF1D', '$DF1E', '$DF1F'
ROUNDS = 32
CHROUT = '$FFD2'


def program():
    a = crts.Asm(0x080D)
    fails, rounds, status = '$FB', '$FC', '$C100'
    a('LDA #0; STA ' + fails + f'; LDA #{ROUNDS}; STA ' + rounds)
    a.label('round')
    # uii_abort(): control |= $04.
    a(f'LDA {CONTROL}; ORA #$04; STA {CONTROL}')
    # Wait for the idle state (bits 5:4), then the command bytes and PUSH_CMD as control |= $01.
    a.label('idle')
    a(f'LDA {CONTROL}; AND #$30; BNE idle')
    a(f'LDA #$04; STA {COMMAND}; LDA #$28; STA {COMMAND}; LDA #$00; STA {COMMAND}')
    a(f'LDA {CONTROL}; ORA #$01; STA {CONTROL}')
    a.label('busy')
    a(f'LDA {CONTROL}; AND #$30; CMP #$10; BEQ busy')
    # Data (b7 = data available) is read and dropped; the status (b6) goes to $C100.
    a.label('data')
    a(f'BIT {CONTROL}; BPL nodata; LDA {RESPONSE}; JMP data')
    a.label('nodata')
    a('LDY #0; STY ' + status)
    a.label('status')
    a(f'LDA {CONTROL}; AND #$40; BEQ nostatus; LDA {STATUS}; STA {status},Y; INY; BNE status')
    a.label('nostatus')
    # DATA_ACC, then wait until the firmware took it.
    a(f'LDA {CONTROL}; ORA #$02; STA {CONTROL}')
    a.label('accept')
    a(f'LDA {CONTROL}; AND #$02; BNE accept')
    a(f'CPY #2; BCC bad; LDA {status}; CMP #$30; BNE bad; LDA {status}+1; CMP #$30; BEQ good')
    a.label('bad')
    a('INC ' + fails)
    a.label('good')
    a('DEC ' + rounds + '; BNE round')
    # The result line.
    a('LDA ' + fails + '; BNE failed; LDX #0')
    a.label('ok')
    a(f'LDA text_ok,X; BEQ done; JSR {CHROUT}; INX; BNE ok')
    a.label('failed')
    a('LDX #0')
    a.label('fail')
    a(f'LDA text_fail,X; BEQ count; JSR {CHROUT}; INX; BNE fail')
    a.label('count')
    a('LDA ' + fails + f'; LSR A; LSR A; LSR A; LSR A; TAX; LDA hex,X; JSR {CHROUT}')
    a('LDA ' + fails + f'; AND #$0F; TAX; LDA hex,X; JSR {CHROUT}')
    a.label('done')
    a(f'LDA #13; JSR {CHROUT}; RTS')
    a.label('text_ok')
    a.data(b'UCI OK\0')
    a.label('text_fail')
    a.data(b'UCI FAIL \0')
    a.label('hex')
    a.data(b'0123456789ABCDEF')
    return a.assemble()


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[2].strip())
    # $0801: 10 SYS2061, then the end of the BASIC program; the code follows at $080D.
    basic = bytes([0x0B, 0x08, 0x0A, 0x00, 0x9E]) + b'2061' + bytes([0, 0, 0])
    Path(sys.argv[1]).write_bytes(bytes([0x01, 0x08]) + basic + program())


if __name__ == '__main__':
    main()
