#!/usr/bin/env python3
"""S31: 1581 drives at positions A and B, end to end, through the firmware's own menu and REST API.

    scripts/smoke-1581.py [--flash run/flash-1581.bin] [--sd run/sd-1581.img] [--bin target/release/ue2emu]

Setup (the drive smoke's, plus the 1581 ROM on the SD card):
    cp run/flash-drv.bin run/flash-1581.bin      # after smoke-c64-drive.ctl: C64 ROMs, drive A on with 1541.bin
    cp run/drive-sd.img run/sd-1581.img
    scripts/add-sd-files.sh run/sd-1581.img $UE2_FIRMWARE/roms/1581.bin

Steps: "Set as 1581 ROM" on 1581.bin in the menu; the firmware creates two D81s; A as a 1581 with a.d81:
LOAD"$",8, SAVE/LOAD/RUN a program; B enabled as a 1581 with b.d81: both directories; A back to a 1541 with
drive.d64 beside B. Exit 0 when every screen shows what it should; the flash and the SD image are changed.
"""

import argparse
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

CONTROL = ("127.0.0.1", 6400)
WEB = "http://127.0.0.1:8080"
q = urllib.parse.quote


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


def rest(method, path):
    try:
        return urllib.request.urlopen(urllib.request.Request(WEB + path, method=method), timeout=20).read().decode()
    except urllib.error.HTTPError as e:
        return f"HTTP {e.code}: {e.read().decode()}"


def c64(line, wait_ms):
    ctl("type " + line)
    ctl("key return")
    ctl(f"wait {wait_ms}")


def directories(units):
    c64("print chr$(147)", 500)
    for unit in units:
        c64(f'load"$",{unit}', 8000)
        c64("list", 1500)
    return ctl("c64screen")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--flash", default="run/flash-1581.bin")
    p.add_argument("--sd", default="run/sd-1581.img")
    p.add_argument("--bin", default="target/release/ue2emu")
    p.add_argument("--firmware", default="firmware/1541ultimate/target/u64ii/riscv/ultimate/result/ultimate.elf")
    p.add_argument("--roms", default="firmware/1541ultimate/roms")
    a = p.parse_args()
    emu = subprocess.Popen(
        [a.bin, "run", "--firmware", a.firmware, "--roms", a.roms, "--flash", a.flash, "--sd", a.sd, "--c64-roms",
         "--headless", "--net", "user", "--control", f"{CONTROL[0]}:{CONTROL[1]}"],
        stdout=open("run/smoke-1581.log", "w"), stderr=subprocess.STDOUT)
    screens = {}
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
        # The menu: the SD root lists 1541.bin, 1581.bin, drive.d64; a 32 K .bin offers Load Drive ROM, then
        # Set as 1541/1571/1581 ROM (filetype_bin.cc:107-123).
        ctl("wait 5000")
        ctl("button")
        ctl('expect "SD Card" 5000')
        ctl("key right")
        ctl('expect "1581.bin" 5000')
        ctl("key down")
        ctl("wait 300")
        ctl("key return")
        ctl('expect "Set as 1581 ROM" 3000')
        for _ in range(3):
            ctl("key down")
            ctl("wait 300")
        ctl("key return")
        rom = ctl('expect-console "Copying 1581.bin to /flash/roms" 5000')
        ctl("wait 2000")
        ctl("button")
        ctl("wait 1000")

        for path, name in [("SD/a.d81", "A SIDE"), ("SD/b.d81", "B SIDE")]:
            rest("PUT", f"/v1/files/{q(path)}:create_d81?diskname={q(name)}")
        rest("PUT", "/v1/drives/a:set_mode?mode=1581")
        rest("PUT", "/v1/drives/a:mount?image=" + q("/SD/a.d81"))
        ctl("wait 3000")
        for line in ["new", '10 print"1581 ok"', 'save"t",8', "new", 'load"t",8', "run"]:
            c64(line, 4000)
        screens["save"] = ctl("c64screen")
        screens["a"] = directories([8])

        drive_b = q("Drive B Settings")
        rest("PUT", f"/v1/configs/{drive_b}/{q('ROM for 1541 mode')}?value=1541.bin")
        rest("PUT", f"/v1/configs/{drive_b}/{q('ROM for 1581 mode')}?value=1581.bin")
        rest("PUT", f"/v1/configs/{drive_b}/Drive?value=Enabled")
        rest("PUT", "/v1/drives/b:set_mode?mode=1581")
        rest("PUT", "/v1/drives/b:mount?image=" + q("/SD/b.d81"))
        ctl("wait 3000")
        screens["both"] = directories([8, 9])

        rest("PUT", "/v1/drives/a:set_mode?mode=1541")
        rest("PUT", "/v1/drives/a:mount?image=" + q("/SD/drive.d64"))
        ctl("wait 3000")
        screens["mixed"] = directories([8, 9])
    finally:
        emu.terminate()
        emu.wait()
    for name, text in screens.items():
        print(f"[{name}]\n{text}")
    want = [
        ("save", "SAVING T"), ("save", "\n1581 OK"),
        ("a", '0 "A SIDE          "    3D'), ("a", '"T"                PRG'), ("a", "3159 BLOCKS FREE."),
        ("both", '0 "A SIDE          "    3D'), ("both", '0 "B SIDE          "    3D'),
        ("mixed", '0 "UE2 DRIVE       " U2 2A'), ("mixed", '0 "B SIDE          "    3D'),
    ]
    missing = [f"{name}: {w}" for name, w in want if w not in screens.get(name, "")]
    if "ok" not in rom:
        missing.append("the 1581 ROM was not installed")
    if missing:
        sys.exit("FAIL: " + "; ".join(missing))
    print("PASS")


if __name__ == "__main__":
    main()
