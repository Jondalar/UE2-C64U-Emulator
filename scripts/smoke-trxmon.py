#!/usr/bin/env python3
"""S39: trxmon, TRX64-Ultimate's monitor app, on the board's debug block in UE2 (docs/specs/S39-trxmon.md §9).

Boots the t21-p4-app firmware headless with `--trx-io`, a fresh flash with the C64 ROMs, PAL, an SD image with
`trxmon.u2a`, `--net user` with the RPC port forwarded, and drives it over the control port and trxmon's RPC
(TRX64-Ultimate's tests/trxmon_client.py):

- boot: CAPS EF;
- the file browser, `trxmon.u2a`, Run App: the monitor screen (`PAUSED IB $xxxx`), the RPC server on 4312;
- `r`, `z`, `z 5` typed on the screen: the registers are UE2's own (`monitor r`), each step one stop record of REASON 2
  and INSTRS + n;
- RPC: `selftest`; `read c000 256 cpu` equals `monitor m c000`; `m io dc0d` twice the same (no flag cleared);
- RPC: `trace status` says the bitstream has no trace former (until S40 phase E) or reports the trace; `rstep` lands
  on the frame since the selftest's breakpoint;
- RPC: `wr d020 05`, then a step: the border colour is 5 (or the refusal of a trxmon older than t21-p4-app 553e2e17
  without the menu's C64_STOP);
- RPC: `r pc=c000` (the CPU write path's redirect with IPC_SET) and `r a=42` on `STA $0400`, then a step: $0400 = $42
  (a redirect and a bus cycle while halted are no part of a frame's replay, so `rstep` comes before them);
- `/quit` on the screen: OWNER 0, the C64 runs again; `print 6*7` gives 42.

The screen is typed with the control port's `type`; the firmware's overlay keyboard takes W, A, S and D for cursor
keys, so lines with those letters go over the RPC's `monitor/exec`.

Exits non-zero at the first failure and keeps the run directory then.

    scripts/smoke-trxmon.py [--elf ELF] [--u2a trxmon.u2a] [--client tests/trxmon_client.py]
"""

import argparse
import importlib.util
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

APP = Path('/Users/alex/Development/TRX64-Ultimate-app-wt-t21')
T21 = Path('/Users/alex/Development/TRX64-Ultimate-wt-t21')


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
        self.rpc_port, ctl = free_port(), free_port()
        self.err_path = work / 'stderr.log'
        self.log, self.err = open(work / 'console.log', 'wb'), open(self.err_path, 'wb')
        argv = [str(args.bin), 'run', '--headless', '--board', 'c64u', '--trx-io', '--firmware', str(args.elf),
                '--roms', str(args.roms), '--flash', str(work / 'flash.bin'), '--c64-roms', '--settings',
                str(HERE / 'smoke-trxmon.cfg'), '--sd', str(work / 'sd.img'), '--net', 'user', '--hostfwd',
                f'tcp:127.0.0.1:{self.rpc_port}:4312', '--control', f'127.0.0.1:{ctl}']
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

    def cmd(self, *lines):
        out = []
        for line in lines:
            out += self.ctl.cmd(line)
        return out

    def screen(self):
        return '\n'.join(self.cmd('screen'))

    def type_line(self, text, wait=800):
        self.cmd(f'type {text}', 'wait 300', 'key return', f'wait {wait}')

    def trx(self):
        return '\n'.join(self.cmd('monitor trx'))

    def counters(self):
        m = re.search(r'counters\s+cycles (\d+)\s+instrs (\d+)', self.trx())
        return int(m.group(1)), int(m.group(2))

    def record(self):
        m = re.search(r'stop record\s+reason (\d+) unit (\d+) flags (\S+)\s+ipc \$([0-9a-f]{4})', self.trx())
        return int(m.group(1)), int(m.group(4), 16)

    def regs(self):
        text = '\n'.join(self.cmd('monitor r'))
        m = re.search(r';([0-9a-fA-F]{4}) ([0-9a-fA-F]{2}) ([0-9a-fA-F]{2}) ([0-9a-fA-F]{2}) ([0-9a-fA-F]{2})', text)
        check(m, f'monitor r: {text}')
        return tuple(int(g, 16) for g in m.groups())

    def mem(self, addr, n):
        text = '\n'.join(self.cmd(f'monitor m {addr:04x} {addr + n - 1:04x}'))
        out = []
        for line in text.splitlines():
            m = re.match(r'\s*>C:([0-9a-f]{4})\s+((?:[0-9a-f]{2} ?)+)', line)
            if m:
                out += [int(b, 16) for b in m.group(2).split()]
        return bytes(out[:n])

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


def smoke(run, client):
    run.cmd('expect-console "[TRX] IO block present, CAPS EF" 30000', 'wait 4000')
    check('READY.' in '\n'.join(run.cmd('c64screen')), 'no READY.')
    print('PASS boot: CAPS EF, READY.')

    run.cmd('button', 'wait 1500', 'key return', 'wait 800', 'key right', 'wait 800')
    check('trxmon.u2a' in run.screen(), f'trxmon.u2a not in the browser: {run.screen()}')
    run.cmd('key down', 'key down', 'key down', 'key return', 'wait 800')
    check('Run App' in run.screen(), 'no Run App')
    run.cmd('key return', 'wait 2000')
    if 'Copy TRX64 Monitor to Flash' in run.screen():
        run.cmd('key n', 'wait 3000')
    run.cmd('expect-console "trxmon: serving the TRX64 runtime contract on TCP 4312" 10000')
    s = run.screen()
    check(re.search(r'PAUSED IB \$[0-9a-f]{4}', s), f'the monitor screen: {s}')
    print('PASS Run App: the monitor screen ("PAUSED IB"), the RPC server on 4312')

    pc, a, x, y, sp = run.regs()
    run.type_line('r')
    check(re.search(rf'\.;{pc:04x} {a:02x} {x:02x} {y:02x} {sp:02x}', run.screen()), f'r on the screen vs monitor r '
          f'{pc:04x} {a:02x} {x:02x} {y:02x} {sp:02x}: {run.screen()}')
    # The first step takes run control (the firmware's acquire: PAUSE_CB, OWNER); an interrupt that waited while the
    # menu held the 6510 is taken on the way (S39 §5 T2). The next steps are counted.
    run.type_line('z')
    check('owner yes' in run.trx(), 'trxmon did not take run control')
    for n in (1, 5):
        before = run.counters()
        run.type_line('z' if n == 1 else f'z {n}')
        after = run.counters()
        reason, ipc = run.record()
        pc = run.regs()[0]
        check(reason == 2 and ipc == pc, f'z {n}: the record {reason} at ${ipc:04x}, the 6510 at ${pc:04x}')
        check(after[1] - before[1] == n and after[0] > before[0], f'z {n}: instrs {before} -> {after}')
        check(re.search(rf'\${pc:04x} ', run.screen()), f'z {n}: ${pc:04x} not on the screen')
    print('PASS r, z, z 5: the registers are UE2\'s, each step a stop record of REASON 2, INSTRS + n')

    c = client.Client('127.0.0.1', run.rpc_port, timeout=10.0)
    try:
        client.selftest(c)
        print('PASS RPC selftest')
        # Right after the selftest's breakpoint: the frame from its snap holds no write from outside.
        try:
            tr = c.mon('trace status')
        except client.TrxmonError as e:
            tr = str(e)
        print(f'INFO trace status: {tr.strip()[:160]}')
        rs = c.mon('rstep')
        check('landed' in rs, f'rstep: {rs}')
        print(f'PASS rstep: {rs.splitlines()[0][:100]}')
        c.call('debug/pause', {'source': 'llm'})
        check(c.read(0xC000, 256, 'cpu') == run.mem(0xC000, 256), 'read c000 256 cpu differs from monitor m c000')
        print('PASS RPC read c000 256 cpu equals monitor m c000')
        t1, t2 = c.mon('m io dc0d dc0d'), c.mon('m io dc0d dc0d')
        check(t1 == t2, f'm io dc0d twice: {t1!r} / {t2!r}')
        print(f'PASS m io dc0d twice the same: {t1.split()[1:3]}')

        # trxmon before t21-p4-app 553e2e17 takes an I/O write only under the menu's C64_STOP (STATUS b5); from
        # 553e2e17 on also in a debugger stop (STATUS b1, the D page the RTL opens for it, df6d71c).
        try:
            w = c.mon('wr d020 05')
        except client.TrxmonError as e:
            w = str(e)
        if 'needs the machine paused (and the menu' in w:
            print(f'PASS wr d020 05 refused by this trxmon build (no C64_STOP): {w.strip()}')
        else:
            c.call('debug/step')
            c.wait_note('debug/stopped', 2.0)
            border = c.read(0xD020, 1, 'io')[0] & 0x0F
            check(border == 5, f'wr d020 05 ({w.strip()}), then a step: the border is {border}')
            print(f'PASS wr d020 05 ({w.strip()}), a step: the border is 5')

        pc0, a0 = run.regs()[:2]
        run.cmd('monitor wr ram c000 8d 00 04 4c 03 c0')
        r1 = c.mon('r pc=c000')
        r2 = c.mon('r a=42')
        c.call('debug/step')
        c.wait_note('debug/stopped', 2.0)
        check(run.mem(0x0400, 1)[0] == 0x42 and run.regs()[0] == 0xC003,
              f'r pc=c000 ({r1.strip()}), r a=42 ({r2.strip()}), a step over STA $0400: '
              f'$0400 = {run.mem(0x0400, 1)[0]:02x}, pc ${run.regs()[0]:04x}')
        print('PASS the CPU write path: r pc=c000, r a=42, a step over STA $0400 stores $42')
        c.mon(f'r pc={pc0:04x}')
        c.mon(f'r a={a0:02x}')

    finally:
        c.close()

    run.type_line('/quit', 2000)
    check('owner no' in run.trx(), f'OWNER after /quit: {run.trx()}')
    # Back in the file browser the app was started from: the menu button closes the overlay.
    run.cmd('button', 'wait 1500')
    run.cmd('type print 6*7', 'key return', 'wait 1500')
    if ' 42' not in '\n'.join(run.cmd('c64screen')):
        diag = [l for l in run.trx().splitlines() if re.search(r'debug|counters|gate|stop rec', l)]
        run.cmd('wait 500')
        diag += [l for l in run.trx().splitlines() if 'counters' in l]
        raise Fail(f'print 6*7: {run.cmd("c64screen")[:8]} / {diag} / {run.cmd("monitor r")} / {run.screen()[:300]}')
    print('PASS /quit: OWNER 0, the C64 runs, print 6*7 gives 42')


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    fw = Path(os.environ.get('UE2_FIRMWARE', REPO / 'firmware' / '1541ultimate'))
    ap.add_argument('--bin', type=Path, default=Path(os.environ.get('CARGO_TARGET_DIR', REPO / 'target')) / 'release' / 'ue2emu')
    ap.add_argument('--elf', type=Path, default=APP / 'target/u64ii/riscv/ultimate/result/ultimate.elf')
    ap.add_argument('--u2a', type=Path, default=APP / 'apps/trxmon/trxmon.u2a')
    ap.add_argument('--client', type=Path, default=T21 / 'tests/trxmon_client.py')
    ap.add_argument('--roms', type=Path, default=fw / 'roms')
    args = ap.parse_args()
    for f in (args.bin, args.elf, args.u2a, args.client, args.roms / 'chars.bin'):
        if not f.exists():
            print(f'need {f}', file=sys.stderr)
            return 2
    spec = importlib.util.spec_from_file_location('trxmon_client', args.client)
    client = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(client)
    work = Path(tempfile.mkdtemp(prefix='ue2-trxmon.'))
    print(f'run directory: {work}')
    subprocess.run([str(HERE / 'make-sd-image.sh'), str(work / 'sd.img')], check=True, stdout=subprocess.DEVNULL)
    subprocess.run([str(HERE / 'add-sd-files.sh'), str(work / 'sd.img'), str(args.u2a)], check=True,
                   stdout=subprocess.DEVNULL)
    run = Run(args, work)
    try:
        smoke(run, client)
    except (Fail, RuntimeError, OSError, client.TrxmonError) as e:
        run.quit()
        print(f'FAIL: {e}')
        print(f'kept: {work}')
        return 1
    run.quit()
    shutil.rmtree(work)
    print('trxmon smoke passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
