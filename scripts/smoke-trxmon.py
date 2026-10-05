#!/usr/bin/env python3
"""S39: trxmon, TRX64-Ultimate's monitor app, on the board's debug block in UE2 (docs/specs/S39-trxmon.md §9).

Boots the t21-p4-app firmware headless with `--trx-io`, a fresh flash with the C64 ROMs, PAL, Navigation Style
"Quick Search" (W, A, S, D can be typed on the monitor screen), an SD image with `trxmon.u2a`, `--net user` with the
RPC port forwarded, and drives it over the control port and trxmon's RPC (TRX64-Ultimate's tests/trxmon_client.py):

- boot: CAPS EF;
- the file browser, `trxmon.u2a`, Run App: the monitor screen (`PAUSED IB $xxxx`), the RPC server on 4312;
- `r`, `z`, `z 5`, `d e5cd` typed on the screen: the registers are UE2's own (`monitor r`), each step one stop record
  of REASON 2 and INSTRS + n; the listing is UE2's bytes;
- RPC: `selftest`; `read c000 256 cpu` equals `monitor m c000`; `m io dc0d` twice the same (no flag cleared);
- RPC: after the selftest and each step a debugger stop with a session though the loader's C64_STOP stands (OWNER,
  t21-p4 3302c85): the colour RAM reads UE2's values;
- RPC: `trace on`, three steps: `trace status` has records (S40 phase E); `rstep` lands on the frame since the
  selftest's breakpoint (the land turns sessions off until its commit: INFO);
- RPC: `wr d020 05`, then a step: the border colour is 5 (in the foreground the loader's C64_STOP holds the window,
  so the write is a bus cycle);
- typed: `r pc=c000` (the CPU write path's redirect with IPC_SET), `r a=42` on `STA $0400`, `z`: $0400 = $42 (a
  redirect and a bus cycle while halted are no part of a frame's replay, so `rstep` comes before them);
- `/quit` on the screen: OWNER 0, the C64 runs again; `print 6*7` gives 42;
- Run App again, `bk e5cd`, `x`: trxmon goes on in the background and closes the menu (ui->close_menu), the
  breakpoint fires (REASON 3 at $e5cd) and opens the monitor screen by itself ("breakpoint 1 hit at .C:e5cd");
- RPC, C64_STOP off: continue and pause, a debugger stop with a session (SH_STATUS b0 | b2): the colour RAM reads
  UE2's values; `wr d020 03` goes to the commit list (SH_COUNTS; the chip keeps its value) and RUN writes it.

The screen is typed with the control port's `type`.

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

    def mem(self, addr, n, lens=''):
        """`monitor m`: its rows start at a multiple of $10 and show from there, so each byte is taken by address."""
        text = '\n'.join(self.cmd(f'monitor m {lens} {addr:04x} {addr + n - 1:04x}'))
        got = {}
        for line in text.splitlines():
            m = re.match(r'\s*>[A-Z]:([0-9a-f]{4})\s+((?:[0-9a-f]{2} ?)+)', line)
            if m:
                for i, b in enumerate(m.group(2).split()):
                    got[int(m.group(1), 16) + i] = int(b, 16)
        return bytes(got[a] for a in range(addr, addr + n))

    def mem_io(self, addr, n):
        return self.mem(addr, n, 'io')

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


def run_app(run):
    """The browser on the SD card, `trxmon.u2a`, Run App: the monitor screen."""
    check('trxmon.u2a' in run.screen(), f'trxmon.u2a not in the browser: {run.screen()}')
    run.cmd('key down', 'key down', 'key down', 'key return', 'wait 800')
    check('Run App' in run.screen(), 'no Run App')
    run.cmd('key return', 'wait 2000')
    if 'Copy TRX64 Monitor to Flash' in run.screen():
        run.cmd('key n', 'wait 3000')
    run.cmd('expect-console "trxmon: serving the TRX64 runtime contract on TCP 4312" 10000')
    s = run.screen()
    check(re.search(r'PAUSED IB \$[0-9a-f]{4}', s), f'the monitor screen: {s}')


def colour_ram(run, c, put=True):
    """The colour RAM read through trxmon's lens (the window's D page in a session) equals UE2's; with `put` a pattern
    UE2 puts there first (not in the foreground before `rstep`: the frame's replay holds no write from outside)."""
    if put:
        run.cmd('monitor wr io d800 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f 00')
    want = run.mem_io(0xD800, 16)
    got = c.read(0xD800, 16, 'io')
    check(bytes(b & 0x0F for b in got) == bytes(b & 0x0F for b in want),
          f'colour RAM: trxmon {got.hex()} vs UE2 {want.hex()}')
    return got


def session(run, what):
    sh = next(l for l in run.trx().splitlines() if 'sh_status' in l)
    check('session: yes' in sh, f'{what}: the debugger stop has no session: {" ".join(sh.split())}')


def smoke(run, client):
    run.cmd('expect-console "[TRX] IO block present, CAPS EF" 30000', 'wait 4000')
    check('READY.' in '\n'.join(run.cmd('c64screen')), 'no READY.')
    print('PASS boot: CAPS EF, READY.')

    run.cmd('button', 'wait 1500', 'key return', 'wait 800', 'key right', 'wait 800')
    run_app(run)
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

    # W, A, S, D typed (Quick Search): the listing of the KERNAL's keyboard wait.
    run.type_line('d e5cd')
    lda = run.mem(0xE5CD, 2)
    check(re.search(rf'\$e5cd  {lda[0]:02x} {lda[1]:02x} +LDA \${lda[1]:02x}', run.screen()), f'd e5cd (UE2 {lda.hex()}): {run.screen()}')
    print(f'PASS d e5cd typed: $e5cd  {lda.hex(" ")}  LDA ${lda[1]:02x}')

    c = client.Client('127.0.0.1', run.rpc_port, timeout=10.0)
    try:
        client.selftest(c)
        print('PASS RPC selftest')
        # t21-p4 3302c85 (freeze_resp.v:550): with OWNER the loader's C64_STOP no longer keeps the block, so every
        # foreground debugger stop has its session and the colour RAM reads (hw_session_read: SH_STATUS b0).
        session(run, 'after the selftest')
        colour_ram(run, c, put=False)
        # Right after the selftest's breakpoint: the frame from its snap holds no write from outside.
        # S40 phase E: the instruction trace (CAPS2 b7, stream 3).
        print(f"INFO {c.mon('trace on').strip()}")
        for _ in range(3):
            c.call('debug/step')
            c.wait_note('debug/stopped', 2.0)
            session(run, 'a step')
            got = colour_ram(run, c, put=False)
        print(f'PASS colour RAM in every foreground debugger stop (session, C64_STOP held): {got[:8].hex(" ")}')
        tr = c.mon('trace status')
        print(f'INFO trace status: {" ".join(tr.split())[:200]}')
        check('needs the instruction trace' not in tr and re.search(r'ON .* [1-9]\d* of \d+ instructions', tr), f'trace: {tr}')
        print('PASS trace on: the trace has records')
        c.mon('trace off')
        rs = c.mon('rstep')
        check('landed' in rs, f'rstep: {rs}')
        print(f'PASS rstep: {rs.splitlines()[0][:100]}')
        c.call('debug/pause', {'source': 'llm'})
        check(c.read(0xC000, 256, 'cpu') == run.mem(0xC000, 256), 'read c000 256 cpu differs from monitor m c000')
        print('PASS RPC read c000 256 cpu equals monitor m c000')
        t1, t2 = c.mon('m io dc0d dc0d'), c.mon('m io dc0d dc0d')
        check(t1 == t2, f'm io dc0d twice: {t1!r} / {t2!r}')
        print(f'PASS m io dc0d twice the same: {t1.split()[1:3]}')

        # In the foreground the loader's C64_STOP holds the window (STATUS b5): the write is a bus cycle.
        w = c.mon('wr d020 05')
        c.call('debug/step')
        c.wait_note('debug/stopped', 2.0)
        border = c.read(0xD020, 1, 'io')[0] & 0x0F
        check(border == 5, f'wr d020 05 ({w.strip()}), then a step: the border is {border}')
        print(f'PASS wr d020 05 ({w.strip()}), a step: the border is 5')
        # After `rstep` the machine is landed: the ring's land turned sessions off (trx_ring.cc land_locked ->
        # scrub_begin_locked -> stop_machine: SH_CTRL 0) until its commit puts SH_CTRL back (commit_locked).
        sh = next(l for l in run.trx().splitlines() if 'sh_ctrl' in l)
        print(f'INFO colour RAM after rstep (landed): {" ".join(c.mon("m io d800 d807").split()[1:9])}  ({" ".join(sh.split())})')
    finally:
        c.close()

    pc0, a0 = run.regs()[:2]
    run.cmd('monitor wr ram c000 8d 00 04 4c 03 c0')
    run.type_line('r pc=c000')
    run.type_line('r a=42')
    check(re.search(r'\.;c000 42 ', run.screen(), re.I), f'r a=42 on the screen: {run.screen()}')
    run.type_line('z')
    check(run.mem(0x0400, 1)[0] == 0x42 and run.regs()[0] == 0xC003,
          f'r pc=c000, r a=42, z over STA $0400: $0400 = {run.mem(0x0400, 1)[0]:02x}, pc ${run.regs()[0]:04x}: '
          f'{run.screen()}')
    print('PASS typed r pc=c000, r a=42, z over STA $0400: $0400 = $42')
    run.type_line(f'r pc={pc0:04x}')
    run.type_line(f'r a={a0:02x}')

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

    # The background: `x` with a breakpoint armed leaves trxmon serving in a task of its own (trx->detach); the
    # loader lets the C64 go (MENU_C64_RESUME) and leaves the browser up.
    run.cmd('button', 'wait 1500')
    for _ in range(4):
        run.cmd('key up')
    run.cmd('wait 500')
    run_app(run)
    ends = run.console().count('trxmon: ends')
    run.type_line('bk e5cd')
    run.type_line('x', 1500)
    run.cmd('wait 1000')
    reason, ipc = run.record()
    check(reason == 3 and ipc == 0xE5CD and 'debugger stop yes' in run.trx(),
          f'bk e5cd, x: the record {reason} at ${ipc:04x}: {run.trx()}')
    check(run.console().count('trxmon: ends') == ends, 'trxmon ended after x')
    print('PASS bk e5cd, x: trxmon serves in the background, the breakpoint fires (REASON 3 at $e5cd)')
    # trxmon 165c22fe: `x` asks for the overlay to close (ui->close_menu, run_app returns MENU_HIDE), the C64 runs,
    # and the break opens the monitor screen by itself (trx->ui_request); no menu button.
    run.cmd('wait 2000')
    s = run.screen()
    check(re.search(r'PAUSED IB \$e5cd', s) and 'breakpoint 1 hit at .C:e5cd' in s,
          f'x closed the menu, the break should open the monitor screen: {s}')
    print('PASS x closed the menu, the break opened the monitor screen by itself ("breakpoint 1 hit at .C:e5cd")')

    c = client.Client('127.0.0.1', run.rpc_port, timeout=10.0)
    try:
        # C64_STOP is off now: a run lets the session's block go (freeze_resp.v:550), the next stop has a session.
        c.call('debug/break_del', {})
        c.call('debug/continue', {'source': 'llm'})
        run.cmd('wait 200')
        c.call('debug/pause', {'source': 'llm'})
        sh = [l for l in run.trx().splitlines() if 'sh_status' in l]
        check(sh and 'session: yes' in sh[0], f'the debugger stop has no session: {sh}')
        got = colour_ram(run, c)
        print(f'PASS colour RAM in a debugger stop (session): {got.hex(" ")}')
        old = run.mem_io(0xD020, 1)[0] & 0x0F
        new = 3 if old != 3 else 4
        w = c.mon(f'wr d020 {new:02x}')
        # trxmon reads the VIC from the state bus (the chip), so the list is seen in SH_COUNTS.
        lst = re.search(r'list (\d+)', next(l for l in run.trx().splitlines() if 'sh_counts' in l))
        check(lst and int(lst.group(1)) >= 1 and run.mem_io(0xD020, 1)[0] & 0x0F == old,
              f'wr d020 {new:02x} ({w.strip()}): the list {lst and lst.group(1)}, the chip {run.mem_io(0xD020, 1)[0]:02x}')
        c.call('debug/continue', {'source': 'llm'})
        run.cmd('wait 200')
        border = run.mem_io(0xD020, 1)[0] & 0x0F
        check(border == new, f'wr d020 {new:02x}, RUN: the border is {border}')
        print(f'PASS wr d020 {new:02x} in a session: on the commit list, the chip keeps {old}, RUN writes it')
        c.call('debug/pause', {'source': 'llm'})
        c.mon('/quit')
    finally:
        c.close()


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
    shutil.copyfile(args.u2a, work / 'trxmon.u2a')  # the browser steps look for this name
    subprocess.run([str(HERE / 'add-sd-files.sh'), str(work / 'sd.img'), str(work / 'trxmon.u2a')], check=True,
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
