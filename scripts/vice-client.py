"""S23 §9.6: a real client conversation over the VICE binary monitor (`--vice-monitor`).

Speaks the protocol the way C64 Studio, IceBro Lite and VS64 speak it: first contact, the discovery commands, the
registers, memory through a named bank, a checkpoint set and listed, a step with its event order, and the errors
for a checkpoint that is not there and a command we do not have. Exits non-zero on the first thing that is wrong.

Usage: vice-client.py [HOST:PORT]   (default 127.0.0.1:6502)
"""

import socket, struct, sys, time

HOST, PORT = "127.0.0.1", 6502
if len(sys.argv) > 1:
    HOST, _, port = sys.argv[1].rpartition(":")
    PORT = int(port)
STX, API = 0x02, 0x02

def req(sock, rid, cmd, body=b""):
    sock.sendall(bytes([STX, API]) + struct.pack("<II", len(body), rid) + bytes([cmd]) + body)

def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise SystemExit("server closed the connection")
        buf += chunk
    return buf

def response(sock):
    head = recv_exact(sock, 12)
    assert head[0] == STX, f"bad magic {head[0]:#x}"
    length, rtype, err = struct.unpack("<I", head[2:6])[0], head[6], head[7]
    rid = struct.unpack("<I", head[8:12])[0]
    return rid, rtype, err, recv_exact(sock, length)

fails = []
def check(name, cond, detail=""):
    print(f"{'ok  ' if cond else 'FAIL'} {name} {detail}")
    if not cond:
        fails.append(name)

for attempt in range(60):
    try:
        s = socket.create_connection((HOST, PORT), timeout=5); break
    except OSError:
        time.sleep(0.5)
else:
    raise SystemExit("could not connect")
s.settimeout(10)

req(s, 1, 0x81)                                   # PING
rid, rtype, err, body = response(s)
check("ping", (rid, rtype, err, body) == (1, 0x81, 0, b""), f"{rtype:#x}")

req(s, 2, 0x85)                                   # VICE_INFO
rid, rtype, err, body = response(s)
check("vice_info", rtype == 0x85 and err == 0 and body[0] == 4 and len(body) == 10, body.hex())

req(s, 3, 0x82)                                   # BANKS_AVAILABLE
rid, rtype, err, body = response(s)
count = struct.unpack("<H", body[:2])[0]
names, cur = [], 2
for _ in range(count):
    size = body[cur]; num = struct.unpack("<H", body[cur+1:cur+3])[0]
    nlen = body[cur+3]; names.append((num, body[cur+4:cur+4+nlen].decode()))
    cur += 1 + size
check("banks", names == [(0,"default"),(0,"cpu"),(1,"ram"),(2,"rom"),(3,"io"),(4,"cart")], str(names))

req(s, 4, 0x83, bytes([0]))                       # REGISTERS_AVAILABLE
rid, rtype, err, body = response(s)
count = struct.unpack("<H", body[:2])[0]
regs, cur = {}, 2
for _ in range(count):
    size = body[cur]; rid_, bits, nlen = body[cur+1], body[cur+2], body[cur+3]
    regs[body[cur+4:cur+4+nlen].decode()] = (rid_, bits)
    cur += 1 + size
check("registers_available", regs.get("PC") == (3,16) and regs.get("A") == (0,8) and "CYC" in regs, str(regs))

req(s, 5, 0x31, bytes([0]))                       # REGISTERS_GET
rid, rtype, err, body = response(s)
count = struct.unpack("<H", body[:2])[0]
values = {}
for i in range(count):
    off = 2 + i*4
    values[body[off+1]] = struct.unpack("<H", body[off+2:off+4])[0]
check("registers_get", rtype == 0x31 and err == 0 and 3 in values, f"pc={values.get(3):#06x}")

req(s, 6, 0x01, bytes([0]) + struct.pack("<HH", 0xfffc, 0xfffd) + bytes([0]) + struct.pack("<H", 2))
rid, rtype, err, body = response(s)                # MEM_GET, the reset vector through the rom bank
length = struct.unpack("<H", body[:2])[0]
check("mem_get", rtype == 0x01 and err == 0 and length == 2, body[2:].hex())

req(s, 7, 0x12, struct.pack("<HH", 0xe5cd, 0xe5cd) + bytes([1, 1, 4, 0]))   # CHECKPOINT_SET, exec, stop
rid, rtype, err, body = response(s)
number = struct.unpack("<I", body[:4])[0]
check("checkpoint_set", rtype == 0x11 and err == 0 and len(body) == 23, f"#{number}")

req(s, 8, 0x14)                                   # CHECKPOINT_LIST
seen = 0
while True:
    rid, rtype, err, body = response(s)
    if rtype == 0x14:
        break
    seen += 1
check("checkpoint_list", seen == 1 and struct.unpack("<I", body[:4])[0] == 1, f"{seen} listed")

req(s, 9, 0x71, bytes([0]) + struct.pack("<H", 2))  # ADVANCE_INSTRUCTIONS
got = {}
for _ in range(3):
    rid, rtype, err, body = response(s)
    got[rtype] = (rid, err)
check("advance", 0x71 in got and 0x31 in got and 0x62 in got, str(sorted(got)))
check("event ids", all(got[t][0] == 0xffffffff for t in (0x31, 0x62)), "register info + stopped are events")

req(s, 10, 0x13, struct.pack("<I", number))        # CHECKPOINT_DELETE
rid, rtype, err, body = response(s)
check("checkpoint_delete", rtype == 0x13 and err == 0)

req(s, 11, 0x13, struct.pack("<I", 999))           # a checkpoint that is not there
rid, rtype, err, body = response(s)
check("missing checkpoint", err == 0x01, f"err={err:#x}")

req(s, 12, 0x99)                                   # a command we do not have
rid, rtype, err, body = response(s)
check("unknown command", err == 0x83, f"err={err:#x}")

req(s, 13, 0xaa)                                   # EXIT: resume
rid, rtype, err, body = response(s)
check("exit", rtype == 0xaa and err == 0)
rid, rtype, err, body = response(s)
check("resumed event", rtype == 0x63 and rid == 0xffffffff)

s.close()
print("FAILED:" if fails else "all vice checks passed", ", ".join(fails))
sys.exit(1 if fails else 0)
