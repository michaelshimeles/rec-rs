#!/usr/bin/env python3
"""Check that the screen's and the camera's files of one `--separate`
recording line up.

Both files: the same frame count (and --frames if given), durations within
one frame, the same start time, and the same audio (the offset between
their beeps at the start and the end).

With the test sources (`--test-pattern --test-camera`), every frame carries
its capture time as a code (`synth::code_bits`), and the script reads it
back from each frame of both files. For each camera frame (the first file
frame that shows it) it reports the offset between the capture times the
two files show at that instant, at the start and at the end of the take:
the sync drift. It also counts the distinct camera frames (none lost) and
checks that the screen file shows a new frame in every slot.

A screen frame that reaches the writer too late for its slot leaves the
previous frame in that slot (in the composed video too). Up to 1% of the
slots may do that; --strict allows none.

Usage: trackcheck.py SCREEN CAMERA [--frames N] [--window SECS] [--max-ms 1] [--strict] [--no-codes]
Exit status 1 if any check fails.
"""
import argparse, json, struct, subprocess, sys

TIMESCALE = 60000
BLOCKS = 36  # start marker (3), 32 bits, end (1)


def run(cmd):
    return subprocess.run(cmd, check=True, capture_output=True).stdout


def streams(path):
    j = json.loads(run(["ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json", path]))
    v = next(s for s in j["streams"] if s["codec_type"] == "video")
    a = [s for s in j["streams"] if s["codec_type"] == "audio"]
    return v, a


def fps_of(v):
    n, d = map(int, v["r_frame_rate"].split("/"))
    return n / d


def codes(path, w, h, mirrored):
    """The time code of every frame (None where it can't be read)."""
    b = max(4, w // 48)
    x = w - b - BLOCKS * b if mirrored else b
    vf = f"crop={BLOCKS * b}:{2 * b}:{x}:{b},scale={BLOCKS}:1:flags=area,format=gray"
    raw = run(["ffmpeg", "-v", "error", "-i", path, "-map", "0:v:0", "-vf", vf,
               "-fps_mode", "passthrough", "-f", "rawvideo", "-"])
    out = []
    for i in range(0, len(raw) - BLOCKS + 1, BLOCKS):
        bits = [v > 128 for v in raw[i:i + BLOCKS]]
        if mirrored:
            bits.reverse()
        if bits[:3] != [True, True, False] or bits[-1]:
            out.append(None)
            continue
        val = 0
        for on in bits[3:3 + 32]:
            val = val << 1 | on
        out.append(val)
    return out


def read_codes(path, v):
    """Codes, trying the picture as is and mirrored (the camera's file
    follows the overlay's Mirror switch)."""
    w, h = int(v["width"]), int(v["height"])
    best = None
    for mirrored in (False, True):
        c = codes(path, w, h, mirrored)
        ok = sum(x is not None for x in c)
        if best is None or ok > best[1]:
            best = (c, ok, mirrored)
        if ok > 0.9 * len(c):
            break
    return best


def unwrap(c, ref):
    """32-bit tick code as ticks relative to `ref` (codes wrap every ~20 h)."""
    d = (c - ref) % (1 << 32)
    return d - (1 << 32) if d >= 1 << 31 else d


def onsets(path, idx, thresh=0.1, quiet=0.2):
    raw = run(["ffmpeg", "-v", "error", "-i", path, "-map", f"0:a:{idx}", "-ac", "1",
               "-ar", "48000", "-f", "f32le", "-"])
    s = struct.unpack(f"<{len(raw) // 4}f", raw)
    out, last = [], -1e9
    for i, v in enumerate(s):
        if abs(v) > thresh:
            if (i - last) / 48000 > quiet:
                out.append(i / 48000)
            last = i
    return out


def stats(xs, what="frames"):
    if not xs:
        return "none"
    return f"mean {sum(xs) / len(xs):+.3f} ms, min {min(xs):+.3f}, max {max(xs):+.3f} ({len(xs)} {what})"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("screen")
    ap.add_argument("camera")
    ap.add_argument("--frames", type=int, help="expected frame count (fps x --duration)")
    ap.add_argument("--window", type=float, default=10.0, help="seconds measured at the start and the end")
    ap.add_argument("--max-ms", type=float, default=1.0, help="largest offset allowed between the files")
    ap.add_argument("--no-codes", action="store_true", help="real sources: no time codes to read")
    ap.add_argument("--strict", action="store_true", help="fail if any slot of the screen file repeats a late frame")
    ap.add_argument("--json", help="write the measurements here")
    a = ap.parse_args()
    ok = True
    result = {}

    def check(cond, msg):
        nonlocal ok
        print(("ok    " if cond else "FAIL  ") + msg)
        ok &= bool(cond)

    (sv, sa), (cv, ca) = streams(a.screen), streams(a.camera)
    fps = fps_of(sv)
    frame = 1 / fps
    sn, cn = int(sv["nb_frames"]), int(cv["nb_frames"])
    sd, cd = float(sv["duration"]), float(cv["duration"])
    print(f"screen: {sv['width']}x{sv['height']} {sv['codec_name']} {fps:g} fps, {sn} frames, {sd:.3f} s, "
          f"start {float(sv.get('start_time', 0)):.3f}, {len(sa)} audio track(s)")
    print(f"camera: {cv['width']}x{cv['height']} {cv['codec_name']} {fps_of(cv):g} fps, {cn} frames, {cd:.3f} s, "
          f"start {float(cv.get('start_time', 0)):.3f}, {len(ca)} audio track(s)")
    result.update(screen_frames=sn, camera_frames=cn, screen_secs=sd, camera_secs=cd, fps=fps,
                  camera_size=[int(cv["width"]), int(cv["height"])])
    check(fps_of(cv) == fps, "same frame rate")
    check(sn == cn, f"same frame count ({sn} and {cn})")
    if a.frames:
        check(sn == a.frames and cn == a.frames, f"frame count is fps x duration ({a.frames})")
    check(abs(sd - cd) <= frame + 1e-6, f"durations within one frame ({1000 * (cd - sd):+.3f} ms)")
    check(sv.get("start_time") == cv.get("start_time"), f"same start time ({sv.get('start_time')})")
    check(bool(sa) == bool(ca), "audio in both files" if sa else "no audio in either file (no audio source)")

    if not a.no_codes:
        n_frames = sn
        sc, sok, _ = read_codes(a.screen, sv)
        cc, cok, mirrored = read_codes(a.camera, cv)
        check(sok >= 0.99 * sn, f"screen time codes read on {sok} of {sn} frames")
        check(cok >= 0.99 * cn, f"camera time codes read on {cok} of {cn} frames"
              + (" (mirrored picture)" if mirrored else ""))
        ref = next(x for x in sc if x is not None)
        s_ticks = [None if x is None else unwrap(x, ref) for x in sc]
        c_ticks = [None if x is None else unwrap(x, ref) for x in cc]
        # The screen pattern runs at the file's rate: a new frame every slot,
        # each captured exactly one slot after the last.
        # Each slot's nominal capture time: the screen frame captured at the
        # slot's instant. A pause cuts whole slots out (the grid jumps
        # forward for good); a screen frame that came in too late for its
        # slot leaves the previous one in it (the grid looks one slot behind
        # for that slot only). So the grid is the running maximum.
        slot = TIMESCALE / fps
        grid, late, cuts, g_max = [], 0, [], None
        for i, s in enumerate(s_ticks):
            if s is None:
                grid.append(None)
                continue
            g = s - i * slot
            if g_max is not None and g > g_max + 1e-6:
                cuts.append((i, round((g - g_max) / slot)))
            if g_max is not None and g < g_max - 1e-6:
                late += 1
            g_max = g if g_max is None else max(g_max, g)
            grid.append(i * slot + g_max)
        whole = all(abs((s - i * slot) % slot) < 1e-6 or abs((s - i * slot) % slot - slot) < 1e-6
                    for i, s in enumerate(s_ticks) if s is not None)
        allowed = 0 if a.strict else max(1, n_frames // 100)
        check(whole and late <= allowed,
              f"screen file: a new frame in every slot but {late} (late frames left the one before"
              + (f"; at most {allowed} allowed)" if not a.strict else "; --strict allows none)")
              + (f"; {len(cuts)} pause cut(s) of {', '.join(str(k) for _, k in cuts)} slots" if cuts else ""))
        # Each camera frame where it first shows, against its slot's nominal
        # capture time. Right after a pause the camera can still show its
        # newest frame from before the cut, captured inside the pause.
        n = min(len(s_ticks), len(c_ticks))
        cut_starts = {i: grid[i] for i, _ in cuts}
        fresh, ages, prev, held, cut_at = [], [], None, 0, None
        for i in range(n):
            g, c = grid[i], c_ticks[i]
            if i in cut_starts:
                cut_at = cut_starts[i]
            if g is None or c is None:
                continue
            ages.append(1000 * (c - g) / TIMESCALE)
            if c != prev:
                if cut_at is not None and c < cut_at:
                    held += 1
                else:
                    fresh.append((i / fps, 1000 * (c - g) / TIMESCALE))
            prev = c
        if held:
            print(f"camera frames from inside a pause, shown until the next one: {held}")
        cam_frames = len({c for c in c_ticks if c is not None})
        dur = n / fps
        start = [o for t, o in fresh if t < a.window]
        end = [o for t, o in fresh if t >= dur - a.window]
        print(f"camera frames: {cam_frames} distinct (about {dur * 30:.0f} expected at 30 fps)")
        print(f"offset, camera minus screen, where each camera frame first shows:")
        print(f"  first {a.window:g} s: {stats(start)}")
        print(f"  last {a.window:g} s:  {stats(end)}")
        print(f"  whole take: {stats([o for _, o in fresh])}")
        print(f"camera frame age in every slot (a 30 fps camera in a {fps:g} fps file): {stats(ages)}")
        result.update(late_screen_frames=late, pause_cuts=[k for _, k in cuts])
        mean = lambda xs: sum(xs) / len(xs) if xs else float("nan")
        drift = mean(end) - mean(start)
        print(f"drift over the take: {drift:+.3f} ms")
        result.update(camera_distinct_frames=cam_frames, offset_start_ms=mean(start), offset_end_ms=mean(end),
                      offset_max_ms=max((abs(o) for _, o in fresh), default=None), drift_ms=drift,
                      mirrored=mirrored)
        check(cam_frames >= round(dur * 30) - 1, "no camera frames lost")
        check(fresh and max(abs(o) for _, o in fresh) <= a.max_ms,
              f"every camera frame within {a.max_ms:g} ms of the screen")

    # The same audio in both files: beeps (the test pattern's, or claps)
    # at the same instants, at the start and the end.
    if sa and ca:
        so, co = onsets(a.screen, 0), onsets(a.camera, 0)
        pairs = []
        for t in so:
            m = min(co, key=lambda x: abs(x - t), default=None)
            if m is not None and abs(m - t) < 0.2:
                pairs.append((t, 1000 * (m - t)))
        if pairs:
            st = [o for t, o in pairs if t < a.window]
            en = [o for t, o in pairs if t >= sd - a.window]
            print(f"audio, camera file minus screen file: start {stats(st, 'beeps')}; end {stats(en, 'beeps')}")
            result.update(audio_offset_start_ms=sum(st) / len(st) if st else None,
                          audio_offset_end_ms=sum(en) / len(en) if en else None)
            check(max(abs(o) for _, o in pairs) <= a.max_ms, f"audio within {a.max_ms:g} ms ({len(pairs)} beeps)")
        else:
            print("audio: no beeps to compare (skipped)")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(result, f, indent=1)
    print("TRACKS OK" if ok else "TRACKS FAIL")
    sys.exit(0 if ok else 1)


main()
