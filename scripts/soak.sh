#!/bin/sh
# Soak test: record for N seconds while sampling the recorder's CPU and
# memory, then verify the file with ffprobe (frame count, exact CFR,
# per-stream durations, end-of-file A/V gap) and AVFoundation. With
# --separate it checks each file written (NAME.mov, NAME-screen.mov,
# NAME-camera.mov, whichever exist).
# Usage: scripts/soak.sh SECONDS [record args...]   (default args: 4K60 with
# webcam, mic and system audio). Results go to ~/Movies/rec-rs-tests/soak.
# SOAK_EVERY sets the seconds between CPU/memory samples (default 5).
set -eu
cd "$(dirname "$0")/.."
DUR=${1:-1800}; shift || true
[ $# -gt 0 ] || set -- -r 4k --webcam
OUT="${REC_TEST_DIR:-$HOME/Movies/rec-rs-tests}/soak"
mkdir -p "$OUT"
NAME="soak-$(date +%Y%m%d-%H%M%S)"
F="$OUT/$NAME.mov"
REC_DEBUG=1 dist/rec-rs record -t "$DUR" -o "$F" "$@" >"$OUT/$NAME.out" 2>"$OUT/$NAME.err" &
sleep 3
# The recorder is the disclaimed child; its pid is in the pidfile.
PID=$(head -1 "${TMPDIR:-/tmp}/rec.pid")
echo "recording $F (pid $PID) for ${DUR}s"
echo "t_s cpu_pct rss_mb" >"$OUT/$NAME.samples"
t=0
while kill -0 "$PID" 2>/dev/null; do
  ps -o %cpu=,rss= -p "$PID" 2>/dev/null | awk -v t=$t '{printf "%d %.1f %.0f\n", t, $1, $2/1024}' >>"$OUT/$NAME.samples" || true
  sleep "${SOAK_EVERY:-5}"; t=$((t+${SOAK_EVERY:-5}))
done
wait || true
tr '\r' '\n' <"$OUT/$NAME.err" | grep -E "saved|debug|error|warning" || true
python3 - "$F" "$OUT/$NAME.samples" "$DUR" <<'PY' | tee "$OUT/$NAME.report"
import json, os, subprocess, sys
base, samples, dur = sys.argv[1], sys.argv[2], float(sys.argv[3])
stem, ext = os.path.splitext(base)
files = [f for f in (base, f"{stem}-screen{ext}", f"{stem}-camera{ext}") if os.path.exists(f)]
for f in files:
  j = json.loads(subprocess.run(["ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json", f],
                                check=True, capture_output=True).stdout)
  v = next(s for s in j["streams"] if s["codec_type"] == "video")
  print(f"file      : {f}  ({int(j['format']['size'])/1e9:.2f} GB, {float(j['format']['bit_rate'])/1e6:.1f} Mb/s)")
  print(f"video     : {v['codec_name']} {v['width']}x{v['height']} {v['avg_frame_rate']} fps, "
        f"{v['nb_frames']} frames, {float(v['duration']):.3f} s (expected {int(dur*60)} frames)")
  pts = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "packet=pts_time",
                        "-of", "csv=p=0", f], check=True, capture_output=True, text=True).stdout.split()
  t = sorted(float(x) for x in pts if x)
  d = [b - a for a, b in zip(t, t[1:])]
  print(f"frame gaps: min {min(d)*1000:.3f} ms  max {max(d)*1000:.3f} ms  "
        f"(irregular: {sum(1 for x in d if abs(x - 1/60) > 0.0005)})")
  for s in j["streams"]:
      if s["codec_type"] == "audio":
          print(f"audio {s['index']}   : {s['codec_name']} {s['sample_rate']} Hz {s['channels']} ch, "
                f"{float(s['duration']):.3f} s  (end vs video {1000*(float(s['duration'])-float(v['duration'])):+.1f} ms)")
rows = [l.split() for l in open(samples).read().splitlines()[1:] if l.strip()]
cpu = [float(r[1]) for r in rows]; rss = [float(r[2]) for r in rows]
if cpu:
    n = len(cpu); q = n // 4 or 1
    print(f"cpu       : mean {sum(cpu)/n:.1f}%  max {max(cpu):.1f}%  (% of one core, {n} samples)")
    print(f"memory    : first-quarter mean {sum(rss[:q])/q:.0f} MB, last-quarter mean {sum(rss[-q:])/q:.0f} MB, max {max(rss):.0f} MB")
PY
for f in "$F" "${F%.mov}-screen.mov" "${F%.mov}-camera.mov"; do
  [ -e "$f" ] && dist/rec-rs probe "$f" | head -4
done
