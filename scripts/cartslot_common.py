"""Shared pieces of the physical cartridge slot tools (docs/status/cart-slot.md).

- CRT files: build, parse, the linear flash images a cartridge holds, FNV-1a as the dumper uses it.
- The test patterns of scripts/make-slot-crts.py: every byte names its cartridge, bank and window.
- The cartridge families the tools know: CRT header values, bank capacity, how a dumper selects a bank.
- `Rest`: the firmware REST API calls a DMA dumper needs (machine:pause/resume, machine:writemem, machine:readmem).
- `Control`: ue2emu's TCP control protocol (cart-info, cart-save, c64screen, quit).

Only the Python standard library is used.
"""

import socket
import struct
import time
import urllib.error
import urllib.parse
import urllib.request

SIGNATURE = b'C64 CARTRIDGE   '
WINDOW = 0x2000
EEPROM_LOAD = 0xDE00

# --- CRT files ------------------------------------------------------------------------------------------------------


def crt_bytes(hw_type, exrom, game, name, chips, subtype=0):
    """A CRT (format 1.1): 0x40-byte header, then CHIP packets from `chips` = [(bank, load, data, chip_type)]."""
    header = SIGNATURE + struct.pack('>IHHBBB5x', 0x40, 0x0101, hw_type, exrom, game, subtype)
    header += name.upper().encode()[:32].ljust(32, b'\0')
    body = bytearray()
    for bank, load, data, chip_type in chips:
        body += b'CHIP' + struct.pack('>IHHHH', 0x10 + len(data), chip_type, bank, load, len(data)) + bytes(data)
    return header + bytes(body)


def parse_crt(data):
    """{'hw_type', 'exrom', 'game', 'subtype', 'name', 'chips': [(bank, load, bytes, chip_type)]}; ValueError if not a CRT."""
    if len(data) < 0x40 or data[:16] != SIGNATURE:
        raise ValueError('not a CRT file')
    header_len = max(struct.unpack('>I', data[0x10:0x14])[0], 0x40)
    hw_type, exrom, game, subtype = struct.unpack('>HBBB', data[0x16:0x1B])
    name = data[0x20:0x40].split(b'\0')[0].decode('latin-1').strip()
    chips, off = [], header_len
    while off + 16 <= len(data) and data[off:off + 4] == b'CHIP':
        packet, chip_type, bank, load, size = struct.unpack('>IHHHH', data[off + 4:off + 16])
        if off + 16 + size > len(data):
            raise ValueError(f'CHIP packet at {off:#x} runs past the end of the file')
        chips.append((bank, load, bytes(data[off + 16:off + 16 + size]), chip_type))
        off += max(packet, 16 + size)
    return {'hw_type': hw_type, 'exrom': exrom, 'game': game, 'subtype': subtype, 'name': name, 'chips': chips}


def window(crt, bank, which):
    """The 8 K a CRT holds for `bank` in `which` ('roml' at $8000, 'romh' at $A000/$E000 or the second half of a 16 K
    $8000 chip), short chips padded with 0xFF; None if it has none. A later packet replaces an earlier one."""
    found = None
    for b, load, data, _ in crt['chips']:
        if b != bank:
            continue
        if which == 'roml' and load == 0x8000:
            found = data[:WINDOW]
        elif which == 'romh' and load in (0xA000, 0xE000):
            found = data[:WINDOW]
        elif which == 'romh' and load == 0x8000 and len(data) > WINDOW:
            found = data[WINDOW:2 * WINDOW]
    return None if found is None else bytes(found).ljust(WINDOW, b'\xFF')


def banks(crt):
    """Bank numbers that carry a ROM chip, ascending."""
    return sorted({b for b, load, _, _ in crt['chips'] if load != EEPROM_LOAD})


def linear(crt, count, which):
    """`count` banks of `which`, bank n at n * 8 K, absent banks 0xFF: what a flash chip holds."""
    out = bytearray(b'\xFF' * (count * WINDOW))
    for b in banks(crt):
        data = window(crt, b, which)
        if data is not None and b < count:
            out[b * WINDOW:(b + 1) * WINDOW] = data
    return out


def eeprom(crt):
    """The GMod2 EEPROM chip ($DE00), padded to 2 K with 0xFF, or None."""
    for _, load, data, _ in crt['chips']:
        if load == EEPROM_LOAD:
            return bytes(data[:0x800]).ljust(0x800, b'\xFF')
    return None


def fnv1a(buf):
    """32-bit FNV-1a, as cartlib_hash_bank (appsdk/cartlib/cartlib.c) hashes an 8 K bank."""
    h = 0x811C9DC5
    for b in buf:
        h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
    return h


# --- test patterns --------------------------------------------------------------------------------------------------

def pattern(salt, bank, which):
    """8 K for `bank`/`which` of the cartridge with `salt`: page and offset mixed in, the bank added with an odd factor
    (and its high byte separately), so every bank differs from every other at every byte, pages differ, no bank is all
    0xFF, and the FNV hashes of banks are distinct. A readable tag at offset 0x10 names the place."""
    win = 0 if which == 'roml' else 0x80
    add = (bank * 0x35 + (bank >> 8) * 0x9D + salt + win) & 0xFF
    out = bytearray(((i * 7) + (i >> 8) * 13 + add) & 0xFF for i in range(WINDOW))
    tag = f'S{salt:02X} B{bank:03d} {which.upper()}'.encode()
    out[0x10:0x10 + len(tag)] = tag
    return out


# --- cartridge families ---------------------------------------------------------------------------------------------
#
# key: CRT header (hardware type, EXROM, GAME), the model that serves it in the slot (TRX64's mapper or the ported U64
# cart logic, docs/status/cart-slot.md), the windows a bank has, the bank capacity of the board, and `select`: how a DMA
# dumper banks in bank n, as (address, value) writes.

FAMILIES = {
    'normal8k': dict(hw=0, exrom=0, game=1, model='trx64', windows=('roml',), capacity=1),
    'normal16k': dict(hw=0, exrom=0, game=0, model='trx64', windows=('roml', 'romh'), capacity=1),
    'ultimax': dict(hw=0, exrom=1, game=0, model='trx64', windows=('roml', 'romh'), capacity=1),
    'ocean': dict(hw=5, exrom=0, game=0, model='trx64', windows=('roml',), capacity=64,
                  select=lambda n: [(0xDE00, n)]),
    'magicdesk': dict(hw=19, exrom=0, game=1, model='trx64', windows=('roml',), capacity=128,
                      select=lambda n: [(0xDE00, n)]),
    'magicdesk16': dict(hw=85, exrom=0, game=0, model='trx64', windows=('roml', 'romh'), capacity=128,
                        select=lambda n: [(0xDE00, n)]),
    # Bank register $DE00, mode register $DE02 = 5: ULTIMAX, ROML at $8000, ROMH at $E000 (cartlib_switch_bank).
    'easyflash': dict(hw=32, exrom=1, game=0, model='trx64', windows=('roml', 'romh'), capacity=64, chip_type=2,
                      select=lambda n: [(0xDE00, n), (0xDE02, 0x05)], romh_at=0xE000),
    # $DE00 bits 5:0 bank, bit 6 clear: 8 K mode with the EEPROM deselected.
    'gmod2': dict(hw=60, exrom=0, game=1, model='trx64', windows=('roml',), capacity=64,
                  select=lambda n: [(0xDE00, n & 0x3F)]),
    'megabyter': dict(hw=86, exrom=0, game=1, model='trx64', windows=('roml',), capacity=128, chip_type=2,
                      select=lambda n: [(0xDE02, 0x00), (0xDE00, n)]),
    # $DE00 bank bits 7:0, $DF00 bank bits 13:8 and mode 7:6 (00 = 8 K).
    'c64megacart': dict(hw=61, exrom=0, game=1, model='trx64', windows=('roml',), capacity=256, chip_type=2,
                        select=lambda n: [(0xDF00, (n >> 8) & 0x3F), (0xDE00, n & 0xFF)]),
    # Types TRX64 has no mapper for: the U64 cart logic (all_carts_v5.vhd) serves them from the CRT.
    'supergames': dict(hw=8, exrom=0, game=0, model='u64-logic', windows=('roml', 'romh'), capacity=4,
                       select=lambda n: [(0xDF00, n & 0x03)]),
    # C64 Game System: the board decodes the bank from the address, the U64 logic from the data; write both.
    'c64gs': dict(hw=15, exrom=0, game=1, model='u64-logic', windows=('roml',), capacity=64,
                  select=lambda n: [(0xDE00 + (n & 0x3F), n)]),
    # Action Replay: $DE00 bits 4:3 bank, 1:0 = 00 8 K mode.
    'actionreplay': dict(hw=1, exrom=0, game=1, model='u64-logic', windows=('roml',), capacity=4,
                         select=lambda n: [(0xDE00, (n & 3) << 3)]),
}

HW_TO_FAMILY = {}
for _key, _f in FAMILIES.items():
    HW_TO_FAMILY.setdefault(_f['hw'], _key)


def family_of(crt):
    """The family key for a parsed CRT (hardware type 0 by EXROM/GAME)."""
    if crt['hw_type'] == 0:
        return {(0, 1): 'normal8k', (0, 0): 'normal16k', (1, 0): 'ultimax'}.get((crt['exrom'], crt['game']))
    return HW_TO_FAMILY.get(crt['hw_type'])


# --- firmware REST --------------------------------------------------------------------------------------------------

class RestError(RuntimeError):
    pass


class Rest:
    """The REST calls of a DMA dumper: PUT machine:pause / machine:resume, PUT writemem (hex, up to 128 bytes), POST
    writemem (raw body), GET readmem (software/api/route_machine.cc; doc/api/rest_api_openapi_u64.yaml)."""

    def __init__(self, url, password=None, timeout=30.0, retries=20):
        self.url = url.rstrip('/')
        self.password = password
        self.timeout = timeout
        self.retries = retries
        self.calls = 0

    def request(self, method, path, params=None, body=None, content_type=None, idempotent=True):
        """One request. A refused connection (nothing was sent) is retried; other transport errors only for an
        `idempotent` request, so a flash command byte is never sent twice."""
        query = ('?' + urllib.parse.urlencode(params)) if params else ''
        headers = {}
        if self.password:
            headers['X-Password'] = self.password
        if content_type:
            headers['Content-Type'] = content_type
        data = body if body is not None else (b'' if method in ('PUT', 'POST') else None)
        last = None
        for attempt in range(self.retries):
            req = urllib.request.Request(self.url + path + query, data=data, method=method, headers=headers)
            try:
                with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                    self.calls += 1
                    return resp.read()
            except urllib.error.HTTPError as e:
                raise RestError(f'{method} {path}{query}: HTTP {e.code} {e.read()[:200]!r}') from e
            except (urllib.error.URLError, ConnectionError, socket.timeout, TimeoutError) as e:
                last = e
                reason = getattr(e, 'reason', e)
                if not idempotent and not isinstance(reason, ConnectionRefusedError):
                    break
                time.sleep(min(0.25 * (attempt + 1), 2.0))
        raise RestError(f'{method} {path}{query}: no answer ({last})')

    def wait_ready(self, timeout=120.0):
        """Wait until the web server answers GET /v1/info (networking starts a few seconds after boot)."""
        deadline = time.time() + timeout
        while True:
            try:
                return self.request('GET', '/v1/info')
            except RestError:
                if time.time() > deadline:
                    raise
                time.sleep(0.5)

    def pause(self):
        self.request('PUT', '/v1/machine:pause')

    def resume(self):
        self.request('PUT', '/v1/machine:resume')

    def poke(self, addr, data):
        """DMA write of `data` (an int or bytes) at `addr`: PUT with hex up to 128 bytes, POST with a multipart file part
        above (the split upstream tests/lib/api.py `writemem` uses). Never retried after it may have been sent."""
        if isinstance(data, int):
            data = bytes([data])
        data = bytes(data)
        if len(data) <= 128:
            self.request('PUT', '/v1/machine:writemem', {'address': f'{addr:04X}', 'data': data.hex().upper()},
                         idempotent=False)
        else:
            boundary = 'ue2cartslot' + data[:8].hex()
            body = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="data.bin"\r\n'
                    'Content-Type: application/octet-stream\r\n\r\n').encode() + data + f'\r\n--{boundary}--\r\n'.encode()
            self.request('POST', '/v1/machine:writemem', {'address': f'{addr:04X}'}, body,
                         f'multipart/form-data; boundary={boundary}', idempotent=False)

    def peek(self, addr, length=1):
        """DMA read of `length` bytes at `addr`."""
        data = self.request('GET', '/v1/machine:readmem', {'address': f'{addr:04X}', 'length': length})
        if len(data) != length:
            raise RestError(f'readmem ${addr:04X} length {length}: got {len(data)} bytes')
        return data

    def peek1(self, addr):
        return self.peek(addr, 1)[0]


# --- ue2emu control protocol ----------------------------------------------------------------------------------------

class Control:
    """ue2emu's TCP control protocol (docs/status/mcp.md, last section): one line in, result lines and `ok` out."""

    def __init__(self, addr, timeout=120.0):
        host, port = addr.rsplit(':', 1)
        self.sock = socket.create_connection((host, int(port)), timeout=timeout)
        self.file = self.sock.makefile('rw', encoding='utf-8', newline='\n')

    def cmd(self, line):
        self.file.write(line + '\n')
        self.file.flush()
        out, block = [], None
        while True:
            text = self.file.readline()
            if not text:
                raise RuntimeError(f'control: connection closed after {line!r}')
            text = text.rstrip('\n')
            if block is None and text.startswith('--- ') and text.endswith(' ---'):
                block = text
                out.append(text)
            elif block is not None and text == block:
                block = None
                out.append(text)
            elif block is None and text == 'ok':
                return out
            elif block is None and text.startswith('error line '):
                raise RuntimeError(f'control: {line!r}: {text}')
            else:
                out.append(text)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass
