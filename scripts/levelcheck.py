#!/usr/bin/env python3
"""Fader check for a recording of the test pattern's beeps.

The test pattern beeps once a second as system audio, at 0.5 (-6 dBFS).
This measures the peak of every beep in an audio track's first channel, in
dB against a reference, and groups consecutive beeps at the same level. The
groups must match the expected levels in order: dB against the reference,
or "mute" for silence. A beep that lands on a fader move and sits between
two levels is skipped, but only one, and only between two different
levels. The reference is the loudest beep, or --ref dBFS (-6 for the beep
as generated).

With --tail only the last groups must match. A replay clip starts before the
recording, and the app fills the gap while it swaps capture sessions with
silence, which can swallow a beep.

Usage: levelcheck.py FILE --expect 0,-12,mute,0 [--tail] [--ref -6] [--audio 0] [--tol 2]
Exit status 1 if the groups don't match. Its own tests:
python3 -m doctest scripts/levelcheck.py
"""
import argparse, math, struct, subprocess, sys

RATE = 48000
BEEP = 0.04  # seconds (synth::BEEP_SECS)

def samples(path, idx):
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-map", f"0:a:{idx}", "-af", "pan=mono|c0=c0",
                          "-ar", str(RATE), "-f", "f32le", "-"], check=True, capture_output=True).stdout
    return struct.unpack(f"<{len(raw) // 4}f", raw)

def beep_phase(s):
    """Where in each second the beeps start: the median onset phase, leaving out
    the first beep (the file may start in the middle of it)."""
    onsets, last = [], -RATE
    for i, v in enumerate(s):
        if abs(v) > 0.02:
            if i - last > RATE // 5:
                onsets.append(i)
            last = i
    phases = sorted((i % RATE) / RATE for i in onsets[1:] or onsets)
    return phases[len(phases) // 2] if phases else None

def classify(rel, expect, tol):
    """The expected level a beep at `rel` dB matches: a number, None for
    silence (40 dB or more under the reference), or "skip"."""
    return next((e for e in expect if (e is None and rel < -40) or (e is not None and abs(rel - e) <= tol)), "skip")

def check(rels, expect, tol=2.0, tail=False):
    """Group beeps (dB against the reference) by level and compare the
    groups with `expect`. Returns (groups, ok).

    >>> check([0, 0, -12, -12, -140, -140, 0], [0, -12, None, 0])
    ([0, -12, None, 0], True)

    A beep on a fader move, between two levels, is skipped:

    >>> check([0, -7, -12], [0, -12])
    ([0, -12], True)

    A level that is wrong for good is not a fader move:

    >>> check([-12, 0, 0, 0], [-12])
    ([-12], False)
    >>> check([0, -7, -7, -12], [0, -12])
    ([0, -12], False)
    >>> check([0, -12, -12, -7], [0, -12])
    ([0, -12], False)
    >>> check([0, -7, 0], [0, -12])
    ([0], False)

    With `tail`, only the last groups count, and only their skips:

    >>> check([-7, 0, -140, 0, -12, -140, 0], [0, -12, None, 0], tail=True)
    ([0, None, 0, -12, None, 0], True)
    >>> check([0, -12, -140, 0], [0, -12, None, 0], tail=True)
    ([0, -12, None, 0], True)
    """
    matches = [classify(r, expect, tol) for r in rels]
    groups, starts = [], []
    for k, m in enumerate(matches):
        if m != "skip" and (not groups or groups[-1] != m):
            groups.append(m)
            starts.append(k)
    first = starts[-len(expect)] if tail and len(groups) >= len(expect) else 0
    for k in range(first, len(matches)):
        if matches[k] != "skip":
            continue
        prev = matches[k - 1] if k > first else "skip"
        nxt = matches[k + 1] if k + 1 < len(matches) else "skip"
        if "skip" in (prev, nxt) or prev == nxt:
            return groups, False
    return groups, (groups[-len(expect):] if tail else groups) == expect

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--expect", required=True, help="levels in order, e.g. 0,-12,mute,0")
    ap.add_argument("--audio", type=int, default=0, help="audio track index")
    ap.add_argument("--ref", type=float, help="reference level in dBFS (default: the loudest beep)")
    ap.add_argument("--tol", type=float, default=2.0, help="dB")
    ap.add_argument("--tail", action="store_true", help="only the last groups must match")
    a = ap.parse_args()
    expect = [None if x == "mute" else float(x) for x in a.expect.split(",")]
    s = samples(a.file, a.audio)
    phase = beep_phase(s)
    if phase is None:
        peak = max((abs(v) for v in s), default=0.0)
        ok = expect == [None] and peak < 1e-4
        print(f"audio {a.audio}: no beeps, peak {peak:.6f}, expected {a.expect} -> {'ok' if ok else 'FAIL'}")
        sys.exit(0 if ok else 1)
    levels = []
    t = phase
    while (t + BEEP) * RATE < len(s):
        i0, i1 = max(0, int((t - 0.005) * RATE)), int((t + BEEP + 0.005) * RATE)
        peak = max(abs(v) for v in s[i0:i1])
        levels.append((t, 20 * math.log10(peak) if peak > 1e-7 else -140.0))
        t += 1.0
    ref = a.ref if a.ref is not None else max(db for _, db in levels)
    for t, db in levels:
        match = classify(db - ref, expect, a.tol)
        label = "mute" if match is None else ("no expected level" if match == "skip" else f"{match:+g} dB")
        print(f"  beep {t:6.2f}s  {db:7.1f} dBFS  {db - ref:+6.1f} dB  {label}")
    groups, ok = check([db - ref for _, db in levels], expect, a.tol, a.tail)
    fmt = lambda g: ",".join("mute" if x is None else f"{x:g}" for x in g)
    print(f"audio {a.audio}: reference {ref:.1f} dBFS, levels {fmt(groups)}, expected {'... ' if a.tail else ''}{fmt(expect)} -> {'ok' if ok else 'FAIL'}")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
