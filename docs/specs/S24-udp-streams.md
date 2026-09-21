# S24 — The U64's UDP streams: the VIC picture and the audio, on the wire

**Owns:**
- `crates/ue2-core/src/devices/streams.rs` (new): the `U64_UDP_BASE` header templates and the packet builder
- `crates/ue2-core/src/devices/u64io.rs`: `ETHSTREAM_ENA` read-back for the streamer
- `crates/ue2-core/src/c64host.rs`: `C64Frame::frame`, the VIC's own frame counter, so an emitter can see a new frame
- `crates/c64-bridge/src/video.rs`, `src/sid.rs`: the frame counter and an audio sink for the stream
- `crates/ue2emu/src/net.rs`: the streamer's frames handed to the net backend beside the MAC's
- `docs/status/e2e.md`: E2 closed for video and audio

**Reads:** `firmware/1541ultimate/software/io/network/data_streamer.cc` (the firmware half of the contract),
`firmware/1541ultimate/software/system/u64.h` (the registers), `firmware/1541ultimate/tests/e2e/lib/streams.py`
(the receiver, which is the only specification of the application header the FPGA generates), S08 (the control
protocol), S11-S14 §S12 (the net backend), S20 (the SID thread).

The FPGA has four stream generators that send UDP by themselves, without the firmware touching a socket: VIC
pictures, audio, the bus debug stream and IEC. The firmware only says where to send — it builds a complete
42-byte Ethernet/IP/UDP header per stream and writes it into `U64_UDP_BASE`, then sets the stream's bit in
`ETHSTREAM_ENA`. The emulator models neither, which is `docs/status/e2e.md`'s E2 and the reason three upstream
suites cannot run here: `ultimax-cartridge` (a golden-image comparison of a whole VIC frame), `av/stream_test`
and `freezer-audio`.

Both halves of what is missing already exist. TRX64 hands us a finished 384×272 frame of colour indices every
frame, and the net backend takes raw Ethernet frames from the guest through `Rmii::exchange`. What is missing is
the generator between them.

## 1. Stages

| Stage | Result |
|---|---|
| M1 | The registers: the four 64-byte header templates and `ETHSTREAM_ENA`, readable by the host. |
| M2 | The VIC stream: one frame becomes 68 datagrams, handed to the net backend. `ultimax-cartridge` and `av/stream_test` reachable. |
| M3 | The audio stream: the SID's samples become 770-byte datagrams. `freezer-audio` reachable. |
| M4 | The bus and IEC streams: not built. They carry cycle-level bus traces the emulator has no equivalent for, and no suite asks for them. `ETHSTREAM_ENA` bits 2 and 3 are accepted and ignored, as they are today. |

## 2. The firmware half, which is the part we must not invent

`DataStreamer::calculate_udp_headers` (`data_streamer.cc:309-404`) builds the wire header for stream `id` and
copies 42 bytes to `U64_UDP_BASE + 64 * id`:

| Offset | Field | Set from |
|---|---|---|
| 0-5 | destination MAC | the IP's multicast MAC for 224.0.0.0/4, else broadcast, else the resolved MAC |
| 6-11 | source MAC | the interface's |
| 12-13 | `0x0800` | — |
| 14-17 | IP version/TOS, **total length 28** | a placeholder: the generator owns the length |
| 18-25 | ID, flags, TTL, protocol 17, **checksum** | computed over the placeholder length |
| 26-29 | source IP | the interface's |
| 30-33 | destination IP | `streams:start` |
| 34-35 | source port | per stream: 53248 VIC, 54272 audio, 6510 bus, 1541 IEC |
| 36-37 | destination port | `streams:start` |
| 38-39 | **UDP length 8** | a placeholder, as above |
| 40-41 | UDP checksum 0 | unused, and stays 0 |

So the generator has to finish the header it is given: the IP total length, the UDP length and the IP checksum
belong to the packet, not to the template. Everything else is copied byte for byte, including a destination MAC
we would otherwise have to derive and a source IP we do not own.

`ETHSTREAM_ENA` (`U64_IO_BASE + 0x0F`) carries one enable bit per stream in its low nibble, and the bus stream's
mode in its high nibble. The firmware clears the bit when a stream stops and when its timer expires, so the
enable bit alone says whether a stream is running.

## 3. The application header, and why the receiver defines it

The 12 bytes in front of a VIC datagram are generated in the FPGA. Nothing in the firmware writes them, so the
firmware sources cannot say what they are; the receiver in `tests/e2e/lib/streams.py:262-300` is the only
statement of the format that exists, and it treats every field as fixed rather than negotiated — a datagram
advertising anything else "is not a packet from this stream and gets dropped as malformed". We therefore take the
receiver as the specification and write the constants exactly as it reads them:

    "<HHHHBBH":  seq(2) frame(2) line + last-flag(2) width(2) lines(1) bits(1) encoding(2)

    width 384, lines 4, bits 4, encoding 0; bit 15 of the line field marks a frame's last packet
    768 bytes of payload: 4 lines × 192 bytes, two pixels per byte, **low nibble first**
    780 bytes per datagram, 68 datagrams per 272-line PAL frame

`seq` and `frame` are independent 16-bit counters that wrap. Audio is simpler: a 2-byte sequence and 192
interleaved stereo s16le samples, 770 bytes, and no timestamp (`streams.py:613-646`).

## 4. Where it sits

The generator is a device in `ue2-core`, not in the bridge and not in `ue2emu`:

- The registers are the machine's, and `--c64 none` must still accept the writes.
- `C64Frame` is already `ue2-core`'s type, and the bridge already fills it.
- `Rmii::exchange` already hands guest frames to the backend; the streamer's queue drains through the same call,
  so the packets take the path the MAC's frames take and need no second backend.

The emitter runs when a frame arrives, which is what `C64Frame::frame` is for: the VIC's own counter, so one
picture becomes one burst of datagrams whatever the host's display rate is, and a host that never asks for a
display frame still streams.

Audio is a `SampleSink` beside the one `--audio` uses (S20 §1), so the samples reach the stream on the SID's
thread without the emitter having to pull them.

## 5. What this is not

- **Not a socket.** The emulator does not open a UDP socket of its own and must not: the address, the port and
  the source MAC are the guest's, and a suite that receives a packet from anywhere else is right to drop it.
- **Not paced.** The generator sends a frame's datagrams as one burst when the frame is complete. Real hardware
  spreads them across the frame; nothing in the receiver depends on the spacing, and `FrameAssembler` is built
  for loss, duplication and reordering.
- **Not the bus streams.** Stage M4 says why.

## 6. Verification

- **`tests/e2e/io/c64/jupiter_lander.png`** is a whole 384×272 frame of colour indices, compared index for index
  without tolerance by the upstream `ultimax-cartridge` suite. It is in the tree, so the emulator can be held to
  it directly rather than to a stream that merely arrives.
- Unit tests for the packet builder: the header fields, 68 packets with the last one flagged, the nibble order,
  and the three fields the generator owns (IP length, UDP length, IP checksum) against a hand-computed packet.
- `av/stream_test.py` and `freezer-audio` from the upstream suite, which is what E2 blocks today.
