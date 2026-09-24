#!/usr/bin/env python3
"""S30: Software IEC end to end. Enables "IEC Drive" over the firmware's REST API, then on the C64:
LOAD"$",11 / LIST (the RAM disk), NEW / 10 PRINT"SOFT IEC OK" / SAVE"T",11 / NEW / LOAD"T",11 / RUN.

    scripts/smoke-soft-iec.py [--flash run/flash-iec.bin] [--bin target/release/ue2emu]

The flash needs the C64 ROMs installed (docs/status/c64.md A2); it is changed by the run, so pass a copy.
Exit 0 when the directory, the save and the program's output are on the C64 screen.
"""

import argparse
import socket
import subprocess
import sys
import time
import urllib.parse
import urllib.request

CONTROL = ("127.0.0.1", 6400)
WEB = "http://127.0.0.1:8080"


def ctl(line, timeout=120):
    with socket.create_connection(CONTROL, timeout=timeout) as s:
        f = s.makefile("rw")
        f.write(line + "\n")
        f.flush()
        out = []
        while (l := f.readline()):
            out.append(l)
            if l.startswith(("ok", "err")):
                break
        return "".join(out)


def c64(line, wait_ms):
    ctl("type " + line)
    ctl("key return")
    ctl(f"wait {wait_ms}")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--flash", default="run/flash-iec.bin")
    p.add_argument("--bin", default="target/release/ue2emu")
    p.add_argument("--firmware", default="firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf")
    p.add_argument("--roms", default="firmware/1541ultimate/roms")
    a = p.parse_args()
    emu = subprocess.Popen(
        [a.bin, "run", "--firmware", a.firmware, "--roms", a.roms, "--flash", a.flash, "--c64-roms", "--headless",
         "--net", "user", "--control", f"{CONTROL[0]}:{CONTROL[1]}"],
        stdout=open("run/soft-iec.log", "w"), stderr=subprocess.STDOUT)
    try:
        for _ in range(100):
            try:
                ctl("wait 1", 5)
                break
            except OSError:
                time.sleep(0.2)
        ctl('expect "F3=HELP" 15000')
        for _ in range(60):
            try:
                urllib.request.urlopen(WEB + "/v1/version", timeout=2).read()
                break
            except OSError:
                time.sleep(0.5)
        cat = urllib.parse.quote("SoftIEC Drive Settings")
        req = urllib.request.Request(f"{WEB}/v1/configs/{cat}/{urllib.parse.quote('IEC Drive')}?value=Enabled", method="PUT")
        urllib.request.urlopen(req, timeout=10).read()
        ctl("wait 3000")
        c64('load"$",11', 6000)
        c64("list", 1500)
        listing = ctl("c64screen")
        for line in ["new", '10 print"soft iec ok"', 'save"t",11', "new", 'load"t",11', "run"]:
            c64(line, 3000)
        screen = ctl("c64screen")
    finally:
        emu.terminate()
        emu.wait()
    print(listing, screen, sep="")
    want = [(listing, '"RAMDISK         " 00 2A'), (listing, "BLOCKS FREE."), (screen, "SAVING T"),
            (screen, "SEARCHING FOR T"), (screen, "\nSOFT IEC OK")]
    missing = [w for text, w in want if w not in text]
    if missing:
        sys.exit(f"FAIL: not on the screen: {missing}")
    print("PASS")


if __name__ == "__main__":
    main()
