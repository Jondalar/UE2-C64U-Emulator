#!/usr/bin/env python3
"""S40: TRX64-Ultimate's trx64/main app on the board's engine in UE2 (docs/specs/S40-trx-engine.md §14).

Boots the app headless with `--trx-io`, a fresh flash with the C64 ROMs, the freeze interface and PAL, REST through
`--net user`, and drives it over the control port:

- boot: CAPS EF, the ring's log on in the reset layout;
- a BASIC counter runs; `GET /v1/timewarp:status` counts the frames, every one a target, nothing dropped;
- `PUT /v1/timewarp:rewind?frames=100`: the counter goes back, then on; the load is the snap's state (the bridge says
  so on stderr: the 6510 and CIAs from the snap, RAM and colour RAM equal to the snap's);
- `PUT /v1/timewarp:scrub`: twice onto the same frame gives the same machine; `scrubend` lets it run from there;
- the freeze menu's "C64 Machine" -> "Timewarp": each LEFT a scrub step, RETURN rewinds and the program runs on;
- no turbo on this bitstream: `$D030`/`$D031` read `$FF`.

Exits non-zero at the first failure and keeps the run directory then.

    scripts/smoke-trx-ring.py [--elf run/trx/ultimate-a58f1a6e.elf]
"""

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
sys.path.insert(0, str(HERE))
import cartslot_common as cs  # noqa: E402

PROGRAM = '10 a=a+1:poke1024,aand255:poke1025,int(a/256):goto10'
FRAME = 19_656


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class Fail(Exception):
    pass


def check(ok, what):
    if not ok:
        raise Fail(what)


class Run:
    def __init__(self, args, work):
        self.work = work
        self.http, ctl = free_port(), free_port()
        self.err_path = work / 'stderr.log'
        self.log, self.err = open(work / 'console.log', 'wb'), open(self.err_path, 'wb')
        argv = [str(args.bin), 'run', '--headless', '--speed', 'max', '--board', 'c64u', '--trx-io', '--firmware',
                str(args.elf), '--roms', str(args.roms), '--flash', str(work / 'flash.bin'), '--c64-roms', '--settings',
                str(HERE / 'smoke-trx-ring.cfg'), '--net', 'user', '--hostfwd', f'tcp:127.0.0.1:{self.http}:80',
                '--control', f'127.0.0.1:{ctl}']
        self.proc = subprocess.Popen(argv, stdout=self.log, stderr=self.err, cwd=work)
        deadline = time.time() + 60
        while True:
            try:
                self.ctl = cs.Control(f'127.0.0.1:{ctl}')
                break
            except OSError:
                if time.time() > deadline or self.proc.poll() is not None:
                    raise Fail('no control port')
                time.sleep(0.2)
        self.rest = cs.Rest(f'http://127.0.0.1:{self.http}')

    def cmd(self, line):
        return self.ctl.cmd(line)

    def get(self, path, **params):
        return json.loads(self.rest.request('GET', path, params or None))

    def put(self, path, **params):
        return json.loads(self.rest.request('PUT', path, params or None, idempotent=False))

    def counter(self):
        lo, hi = self.rest.peek(0x0400, 2)
        return lo | hi << 8

    def loads(self):
        return re.findall(r'SB_LOAD: the 6510 and the CIAs of the snap (\d+) cycles back; RAM and colour RAM (\w+)',
                          self.err_path.read_text(errors='replace'))

    def console(self):
        return (self.work / 'console.log').read_text(errors='replace')

    def quit(self):
        try:
            self.cmd('quit')
        except (RuntimeError, OSError):
            pass
        try:
            self.proc.wait(120)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def smoke(run):
    run.cmd('expect-console "[TRX] IO block present, CAPS EF" 20000')
    run.cmd('expect-console "[RING] log on: streams at 7000000/6000000/5C00000" 30000')
    run.rest.wait_ready(120)
    print('PASS boot: CAPS EF, the ring on in the reset layout')

    run.cmd(f'type {PROGRAM}')
    run.cmd('key return')
    run.cmd('type run')
    run.cmd('key return')
    run.cmd('wait 5000')
    st = run.get('/v1/timewarp:status')
    check(st['on'] and st['frames'] >= 250, f'status: {st}')
    check(st['targets'] >= st['frames'] - 1 and st['dropped'] == 0 and st['log_stat'] & 1, f'status: {st}')
    check(st['stream0'] > 0 and st['stream2'] >= 35 * st['targets'], f'status: {st}')
    print(f"PASS status: {st['frames']} frames, {st['targets']} targets, {st['stream0']} RAM writes, 0 dropped")

    c0 = run.counter()
    r = run.put('/v1/timewarp:rewind', frames=100)
    check(r['frames_back'] == 100 and not r['errors'], f'rewind: {r}')
    c1 = run.counter()
    run.cmd('wait 1000')
    c2 = run.counter()
    check(c1 < c0 and c2 > c1, f'the counter: {c0} before, {c1} after the rewind, {c2} a second later')
    loads = run.loads()
    check(len(loads) == 1 and loads[0][1] == 'equal', f'the load: {loads}')
    back = int(loads[0][0])
    check(abs(back - 100 * FRAME) <= 2 * FRAME, f'the load {back} cycles back, not 100 frames')
    print(f'PASS rewind: the counter {c0} -> {c1} -> {c2}; the snap {back} cycles back, RAM and colour RAM equal')

    run.cmd('wait 2000')
    states = []
    # The first step opens the scrub and parses up to the stop, so "frames back" moves once; from the second on the
    # timeline stands.
    for frames in (50, 50, 60, 50):
        r = run.put('/v1/timewarp:scrub', frames=frames)
        check(not r.get('errors'), f'scrub {frames}: {r}')
        regs = run.cmd('monitor r')
        mem = run.rest.peek(0x0000, 256) + run.rest.peek(0x0400, 256)
        states.append((regs, mem))
    if states[1] != states[3]:
        diff = [hex(a if a < 256 else 0x400 + a - 256) for a in range(512) if states[1][1][a] != states[3][1][a]]
        raise Fail(f'two scrubs onto the same frame left different machines: {states[1][0]} / {states[3][0]}; '
                   f'bytes {diff}')
    check(states[1] != states[2], 'scrubs onto different frames left the same machine')
    check(len(re.findall(r'\[RING\] scrub to seq', run.console())) == 4, 'four scrub steps on the console')
    r = run.put('/v1/timewarp:scrubend')
    c3 = run.counter()
    run.cmd('wait 1000')
    c4 = run.counter()
    check(c4 > c3, f'after scrubend the counter stands: {c3}, {c4}')
    loads = run.loads()
    check(len(loads) == 6 and all(m == 'equal' for _, m in loads), f'the loads: {loads}')
    print('PASS scrub: the same frame twice is the same machine; scrubend runs on; every load the snap\'s')

    run.cmd('button')
    run.cmd('wait 1500')
    run.cmd('key f1')
    run.cmd('wait 800')
    run.cmd('key down')
    run.cmd('key return')
    run.cmd('wait 800')
    run.cmd('key return')
    run.cmd('wait 1500')
    for _ in range(25):
        run.cmd('key left')
        run.cmd('wait 200')
    steps = len(re.findall(r'\[RING\] scrub to seq', run.console())) - 4
    check(steps == 25, f'{steps} scrub steps for 25 LEFTs')
    c5 = run.counter()
    run.cmd('key return')
    run.cmd('wait 2000')
    run.cmd('expect-console "[RING] scrub rewind to seq" 5000')
    c6 = run.counter()
    run.cmd('wait 1000')
    c7 = run.counter()
    check(c7 > c6, f'the program does not run on after the Timewarp: {c5}, {c6}, {c7}')
    loads = run.loads()
    check(all(m == 'equal' for _, m in loads), f'the loads: {loads}')
    print(f'PASS Timewarp menu: 25 scrub steps, RETURN rewinds, the counter runs on ({c6} -> {c7}); '
          f'{len(loads)} loads, every one the snap\'s')

    d030, d031 = run.rest.peek(0xD030, 2)
    check((d030, d031) == (0xFF, 0xFF), f'$D030/$D031 read {d030:02X} {d031:02X}')
    print('PASS no turbo: $D030/$D031 read $FF')


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    fw = Path(os.environ.get('UE2_FIRMWARE', REPO / 'firmware' / '1541ultimate'))
    ap.add_argument('--bin', type=Path, default=Path(os.environ.get('CARGO_TARGET_DIR', REPO / 'target')) / 'release' / 'ue2emu')
    ap.add_argument('--elf', type=Path, default=REPO / 'run' / 'trx' / 'ultimate-a58f1a6e.elf')
    ap.add_argument('--roms', type=Path, default=fw / 'roms')
    args = ap.parse_args()
    for f in (args.bin, args.elf, args.roms / 'chars.bin'):
        if not f.exists():
            print(f'need {f}', file=sys.stderr)
            return 2
    work = Path(tempfile.mkdtemp(prefix='ue2-trx-ring.'))
    print(f'run directory: {work}')
    run = Run(args, work)
    try:
        smoke(run)
    except (Fail, cs.RestError, RuntimeError) as e:
        run.quit()
        print(f'FAIL: {e}')
        print(f'kept: {work}')
        return 1
    run.quit()
    shutil.rmtree(work)
    print('TRX ring smoke passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
