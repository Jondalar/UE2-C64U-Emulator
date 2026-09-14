#!/usr/bin/env python3
"""End-to-end acceptance of the physical cartridge slot (`ue2emu run --cart-slot`, docs/status/cart-slot.md).

    scripts/cart-slot-acceptance.py --flash FLASH.bin --work DIR [--bin target/release/ue2emu]
                                    [--firmware ELF] [--roms DIR] [--only s08,s11] [--skip-flash] [--skip-dump]

FLASH.bin is a flash image with the C64 ROMs installed (docs/status/c64.md A2 setup); every run boots a copy of it, so
it is never written. For each test CRT of scripts/make-slot-crts.py (built into DIR/crts) the script:

1. copies the CRT to DIR/slot/ and records its SHA-256;
2. boots `ue2emu run --headless --speed max --net user --hostfwd ... --control ... --cart-slot DIR/slot/<file>`;
3. runs scripts/cart-dump-rest.py --probe --source <file>: the REST dump (PUT machine:pause, writemem, readmem,
   machine:resume) must equal the CRT;
4. keeps `cart-info`; for s01 the C64 screen must show the autostart text; quits cleanly;
5. checks the CRT in the slot is unchanged (the default mode never writes it).

Flash runs (unless --skip-flash), each with scripts/cart-flash-rest.py (autoselect, erase with DQ7/DQ6/DQ5 polling,
program, read back):
- EasyFlash with `,save=DIR/ef-exit.crt`: ROML sector erase and ROMH chip erase, programming ROML and ROMH (in ULTIMAX
  via $DE02), `cart-save DIR/ef-cartsave.crt` compared by the flash script; after quit ef-exit.crt must hold the same
  flash and the source must be unchanged.
- MegaByter with `,rw`: the CRT must change on disk within a few seconds emulated while the emulator runs (debounced
  write-back), equal the flash after quit, and FILE.crt.bak must hold the original.
- GMod2 with `flash-decode=both` (cartlib's short unlock, the long one must work too) and with `flash-decode=15` (the
  long unlock; the short one must not), and EasyFlash with `flash-decode=15`.
- The Normal 8K cartridge: boots with Cartridge Preference Auto; after PUT /v1/configs Cartridge Preference=Internal
  (not saved) and PUT /v1/machine:reboot it must be gone from the C64 (BASIC boots).

Every result goes to DIR/results.json; the exit code is the number of failed checks.
"""

import argparse
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
sys.path.insert(0, str(HERE))
import cartslot_common as cs  # noqa: E402


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Emu:
    """One ue2emu instance with REST and the control port."""

    def __init__(self, args, work, name, slot_spec):
        self.http, self.ctl_port = free_port(), free_port()
        self.flash = work / f'flash-{name}.bin'
        shutil.copyfile(args.flash, self.flash)
        self.log = open(work / f'{name}.console.log', 'wb')
        self.err = open(work / f'{name}.stderr.log', 'wb')
        argv = [str(args.bin), 'run', '--headless', '--speed', 'max', '--firmware', str(args.firmware), '--roms',
                str(args.roms), '--flash', str(self.flash), '--net', 'user', '--hostfwd',
                f'tcp:127.0.0.1:{self.http}:80', '--control', f'127.0.0.1:{self.ctl_port}']
        if slot_spec:
            argv += ['--cart-slot', slot_spec]
        self.argv = argv
        env = dict(os.environ, UE2_FIRMWARE=str(Path(args.roms).parent))
        self.proc = subprocess.Popen(argv, stdout=self.log, stderr=self.err, cwd=work, env=env)
        self.url = f'http://127.0.0.1:{self.http}'
        self.ctl, self.name, self.work = None, name, work
        self.connect()
        cs.Rest(self.url).wait_ready(180)

    def connect(self):
        deadline = time.time() + 60
        while self.ctl is None:
            if self.proc.poll() is not None:
                raise RuntimeError(f'{self.name}: ue2emu exited with {self.proc.returncode}: '
                                   f'{(self.work / f"{self.name}.stderr.log").read_text()[-2000:]}')
            try:
                self.ctl = cs.Control(f'127.0.0.1:{self.ctl_port}')
            except OSError:
                if time.time() > deadline:
                    raise
                time.sleep(0.2)

    def release(self):
        """Close the control connection: the control server serves one client at a time, so a script that uses the
        control port needs it free."""
        if self.ctl is not None:
            self.ctl.close()
            self.ctl = None

    def cmd(self, line):
        self.connect()
        return self.ctl.cmd(line)

    def quit(self, timeout=120):
        try:
            self.cmd('quit')
        except (RuntimeError, OSError):
            pass
        self.release()
        try:
            code = self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            code = 'killed'
        self.log.close()
        self.err.close()
        return code


def run_script(argv, log):
    t0 = time.time()
    p = subprocess.run([sys.executable] + [str(a) for a in argv], capture_output=True, text=True,
                       env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1'))
    Path(log).write_text(p.stdout + p.stderr)
    return p.returncode, p.stdout + p.stderr, round(time.time() - t0, 1)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    fw = Path(os.environ.get('UE2_FIRMWARE', REPO / 'firmware' / '1541ultimate'))
    ap.add_argument('--bin', type=Path, default=REPO / 'target' / 'release' / 'ue2emu')
    ap.add_argument('--firmware', type=Path, default=fw / 'target/u64ii/riscv/ultimate/result/ultimate.elf')
    ap.add_argument('--roms', type=Path, default=fw / 'roms')
    ap.add_argument('--flash', type=Path, required=True, help='flash image with the C64 ROMs installed (copied per run)')
    ap.add_argument('--work', type=Path, required=True)
    ap.add_argument('--only', help='comma-separated file prefixes or flash run names, e.g. s01,s08,gmod2-15')
    ap.add_argument('--skip-dump', action='store_true')
    ap.add_argument('--skip-flash', action='store_true')
    args = ap.parse_args()
    # The emulators run in the work directory.
    for name in ('bin', 'firmware', 'roms', 'flash'):
        setattr(args, name, getattr(args, name).resolve())

    work = args.work.resolve()
    (work / 'crts').mkdir(parents=True, exist_ok=True)
    (work / 'slot').mkdir(exist_ok=True)
    subprocess.run([sys.executable, str(HERE / 'make-slot-crts.py'), str(work / 'crts')], check=True,
                   stdout=subprocess.DEVNULL, env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1'))
    manifest = json.loads((work / 'crts' / 'manifest.json').read_text())
    only = args.only.split(',') if args.only else None
    wanted = lambda name: only is None or any(name.startswith(o) for o in only)  # noqa: E731
    results, failures = [], 0

    def check(entry, name, ok, detail=None):
        nonlocal failures
        entry.setdefault('checks', {})[name] = ok
        if detail is not None:
            entry.setdefault('details', {})[name] = detail
        failures += 0 if ok else 1
        print(f"  {'PASS' if ok else 'FAIL'} {name}" + (f': {detail}' if detail is not None and not ok else ''),
              flush=True)

    for m in manifest:
        if args.skip_dump or not wanted(m['file']):
            continue
        name = m['file'][:-4]
        print(f"{m['file']} ({m['family']}, {m['model']})", flush=True)
        slot = work / 'slot' / m['file']
        shutil.copyfile(work / 'crts' / m['file'], slot)
        before = sha(slot)
        entry = {'file': m['file'], 'family': m['family'], 'model': m['model']}
        results.append(entry)
        try:
            t0 = time.time()
            emu = Emu(args, work, name, str(slot))
            entry['boot_seconds'] = round(time.time() - t0, 1)
            entry['cart_info'] = emu.cmd('cart-info')
            code, out, secs = run_script([HERE / 'cart-dump-rest.py', '--url', emu.url, '--probe', '--roms', args.roms,
                                          '--source', slot, '--out', work / f'{name}.dump.crt',
                                          '--json', work / f'{name}.dump.json'], work / f'{name}.dump.log')
            report = json.loads((work / f'{name}.dump.json').read_text()) if code in (0, 1) else {}
            entry['dump'] = {k: report.get(k) for k in ('banks', 'windows', 'bytes_compared', 'rest_calls', 'seconds')}
            entry['probe'] = report.get('probe')
            check(entry, 'rest_dump_exact', code == 0, out.strip().splitlines()[-3:])
            if m['boot'] == 'text':
                screen = '\n'.join(emu.cmd('c64screen'))
                entry['screen'] = screen
                check(entry, 'autostart_text_on_screen', all(t in screen for t in m['text']), screen[-400:])
            code = emu.quit()
            check(entry, 'clean_exit', code == 0, code)
            check(entry, 'source_unchanged', sha(slot) == before)
        except Exception as e:  # noqa: BLE001 - report and go on with the next cartridge
            check(entry, 'run', False, repr(e))

    def flash_run(run, crt_file, family, suffix, unlock, other_should_work, save_exit=False, rw=False, chips=64,
                  cart_save=False):
        """One flash script run on a copy of `crt_file` with `--cart-slot COPY<suffix>`."""
        if args.skip_flash or not wanted(run):
            return
        print(f'flash: {run} ({family}, cart-slot suffix {suffix!r}, unlock {unlock})', flush=True)
        entry = {'test': run, 'file': crt_file, 'family': family, 'cart_slot_suffix': suffix, 'unlock': unlock}
        results.append(entry)
        try:
            rundir = work / run
            if rundir.exists():
                shutil.rmtree(rundir)
            rundir.mkdir()
            slot = rundir / crt_file
            shutil.copyfile(work / 'crts' / crt_file, slot)
            original = slot.read_bytes()
            exit_crt = rundir / 'exit.crt'
            spec = f'{slot}{suffix}' + (f',save={exit_crt}' if save_exit else '')
            emu = Emu(args, work, run, spec)
            argv = [HERE / 'cart-flash-rest.py', '--url', emu.url, '--family', family, '--unlock', unlock,
                    '--source', slot, '--state', rundir / 'state', '--json', rundir / 'flash.json']
            if cart_save:
                argv += ['--control', f'127.0.0.1:{emu.ctl_port}', '--save', rundir / 'cartsave.crt']
                emu.release()
            code, out, secs = run_script(argv, rundir / 'flash.log')
            report = json.loads((rundir / 'flash.json').read_text()) if code in (0, 1) else None
            entry['flash'] = report
            check(entry, 'flash_script', code == 0, out.strip().splitlines()[-2:])
            if report is not None:
                other = 'long' if unlock == 'short' else 'short'
                check(entry, f'{other}_unlock_works_is_{other_should_work}',
                      report.get(f'{other}_unlock_works') == other_should_work, report.get(f'ids_with_{other}_unlock'))
            if rw:
                written = False
                for _ in range(15):
                    emu.cmd('wait 1000')
                    if slot.read_bytes() != original:
                        written = True
                        break
                check(entry, 'written_back_while_running', written)
            entry['cart_info_after'] = emu.cmd('cart-info')
            code = emu.quit()
            check(entry, 'clean_exit', code == 0, code)
            lo = (rundir / 'state.roml.bin').read_bytes()
            hi_path = rundir / 'state.romh.bin'
            hi = hi_path.read_bytes() if hi_path.is_file() else None

            def holds_flash(path):
                crt = cs.parse_crt(path.read_bytes())
                return cs.linear(crt, chips, 'roml') == lo and (hi is None or cs.linear(crt, chips, 'romh') == hi)

            if save_exit:
                check(entry, 'save_on_exit_equals_flash', exit_crt.is_file() and holds_flash(exit_crt))
            if rw:
                check(entry, 'rw_file_equals_flash', holds_flash(slot))
                bak = rundir / (crt_file + '.bak')
                check(entry, 'bak_holds_the_original', bak.is_file() and bak.read_bytes() == original)
                left = sorted(p.name for p in rundir.iterdir() if p.name.startswith('.'))
                check(entry, 'no_temporary_file_left', not left, left)
            else:
                check(entry, 'source_unchanged', slot.read_bytes() == original)
        except Exception as e:  # noqa: BLE001
            check(entry, 'run', False, repr(e))

    flash_run('ef-save', 's08-easyflash.crt', 'easyflash', '', 'short', True, save_exit=True, cart_save=True)
    flash_run('mb-rw', 's11-megabyter.crt', 'megabyter', ',rw', 'short', True, rw=True, chips=128)
    flash_run('gmod2-both', 's10-gmod2.crt', 'gmod2', ',flash-decode=both', 'short', True, cart_save=True)
    flash_run('gmod2-15', 's10-gmod2.crt', 'gmod2', ',flash-decode=15', 'long', False)
    flash_run('ef-15', 's08-easyflash.crt', 'easyflash', ',flash-decode=15', 'long', False)

    if not args.skip_flash and wanted('preference'):
        print('preference: Internal hides the physical Normal 8K cartridge', flush=True)
        entry = {'test': 'preference-internal', 'file': 's01-normal-8k.crt'}
        results.append(entry)
        try:
            slot = work / 'slot' / 'pref-normal-8k.crt'
            shutil.copyfile(work / 'crts' / 's01-normal-8k.crt', slot)
            emu = Emu(args, work, 'pref', str(slot))
            rest = cs.Rest(emu.url)
            emu.cmd('wait 2000')
            first = '\n'.join(emu.cmd('c64screen'))
            check(entry, 'auto_shows_cartridge', 'PHYSICAL CARTRIDGE BOOTED' in first, first[-300:])
            entry['cart_info_auto'] = emu.cmd('cart-info')
            rest.request('PUT', '/v1/configs/C64%20and%20Cartridge%20Settings/Cartridge%20Preference',
                         {'value': 'Internal'}, idempotent=False)
            rest.request('PUT', '/v1/machine:reboot', idempotent=False)
            emu.cmd('wait 6000')
            second = '\n'.join(emu.cmd('c64screen'))
            entry['screen_after_reboot'] = second
            check(entry, 'internal_hides_cartridge', 'PHYSICAL CARTRIDGE BOOTED' not in second and 'READY.' in second,
                  second[-300:])
            entry['cart_info_internal'] = emu.cmd('cart-info')
            emu.quit()
        except Exception as e:  # noqa: BLE001
            check(entry, 'run', False, repr(e))

    (work / 'results.json').write_text(json.dumps(results, indent=1) + '\n')
    print(f'{failures} failed check(s); results in {work / "results.json"}')
    return failures


if __name__ == '__main__':
    sys.exit(main())
