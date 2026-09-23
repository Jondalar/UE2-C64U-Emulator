#!/usr/bin/env python3
"""W4-SID: level and dominant frequency at the end of a 16-bit WAV written by `ue2emu run --audio-wav`.

    scripts/wav-tone.py FILE [--channel mix|left|right] [--expect HZ | --silent]

A stereo file (S29) is analysed as (L + R) / 2 unless --channel picks one side; a mono file as it is.

Analyses the last 32768 samples (0.68 s at 48 kHz, 0.74 s at 44.1 kHz): peak-to-peak level and the dominant
frequency (mean removed, Hann window, FFT, parabolic interpolation of the peak bin). Pure Python, no numpy.
  --expect HZ  exit 1 unless peak-to-peak >= 5 % of full scale and the dominant frequency is within 1 % of HZ
  --silent     exit 1 unless peak-to-peak < 0.5 % of full scale
"""

import argparse
import cmath
import math
import struct
import sys
import wave

N = 1 << 15


def fft(x):
    """Iterative radix-2 FFT of a list of complex numbers whose length is a power of two."""
    n = len(x)
    a = list(x)
    j = 0
    for i in range(1, n):
        bit = n >> 1
        while j & bit:
            j ^= bit
            bit >>= 1
        j |= bit
        if i < j:
            a[i], a[j] = a[j], a[i]
    size = 2
    while size <= n:
        step = cmath.exp(-2j * math.pi / size)
        half = size // 2
        roots = [step**k for k in range(half)]
        for start in range(0, n, size):
            for k in range(half):
                u = a[start + k]
                v = a[start + k + half] * roots[k]
                a[start + k] = u + v
                a[start + k + half] = u - v
        size *= 2
    return a


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("file")
    p.add_argument("--channel", choices=["mix", "left", "right"], default="mix")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--expect", type=float, metavar="HZ")
    g.add_argument("--silent", action="store_true")
    args = p.parse_args()

    with wave.open(args.file, "rb") as w:
        channels = w.getnchannels()
        if channels not in (1, 2) or w.getsampwidth() != 2:
            sys.exit(f"{args.file}: need mono or stereo 16-bit PCM")
        rate = w.getframerate()
        frames = w.getnframes()
        raw = struct.unpack(f"<{frames * channels}h", w.readframes(frames))
    if channels == 1:
        pcm = raw
    elif args.channel == "left":
        pcm = raw[0::2]
    elif args.channel == "right":
        pcm = raw[1::2]
    else:
        pcm = [(l + r) // 2 for l, r in zip(raw[0::2], raw[1::2])]
    if frames < N:
        sys.exit(f"{args.file}: {frames} samples, need at least {N}")
    tail = pcm[-N:]
    p2p = max(tail) - min(tail)
    mean = sum(tail) / N
    spectrum = fft([(s - mean) * (0.5 - 0.5 * math.cos(2 * math.pi * i / (N - 1))) for i, s in enumerate(tail)])
    mags = [abs(c) for c in spectrum[: N // 2]]
    k = max(range(1, N // 2 - 1), key=mags.__getitem__)
    a, b, c = mags[k - 1], mags[k], mags[k + 1]
    shift = 0.5 * (a - c) / (a - 2 * b + c) if a - 2 * b + c else 0.0
    hz = (k + shift) * rate / N
    print(f"{args.file}: {rate} Hz, {frames} samples ({frames / rate:.2f} s); last {N}: "
          f"peak-to-peak {p2p} ({100 * p2p / 65535:.2f} % of full scale), dominant {hz:.1f} Hz")

    if args.silent and p2p >= 0.005 * 65535:
        sys.exit("FAIL: not silent")
    if args.expect is not None:
        if p2p < 0.05 * 65535:
            sys.exit("FAIL: too quiet for a tone")
        if abs(hz - args.expect) > 0.01 * args.expect:
            sys.exit(f"FAIL: dominant {hz:.1f} Hz, expected {args.expect:.1f} Hz")
    if args.silent or args.expect is not None:
        print("PASS")


if __name__ == "__main__":
    main()
