#!/bin/sh
# Record short clips through every pipeline and verify them with AVFoundation
# (rec probe) and ffmpeg. Usage: scripts/selftest.sh [--no-screen]
set -eu
cd "$(dirname "$0")/.."
REC=dist/rec-rs
# Test recordings never go inside the (synced) repo.
OUT="${REC_TEST_DIR:-$HOME/Movies/rec-rs-tests}/selftest"
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0; skip=0
PERMS=$("$REC" permissions)
has() { echo "$PERMS" | grep -q "^$1 *: granted"; }
skipped() { echo "SKIP  $1  ($2 permission not granted)"; skip=$((skip+1)); }
check() {
  name=$1; shift
  case "$name" in *.mp4) f="$OUT/$name"; name=${name%.mp4} ;; *) f="$OUT/$name.mov" ;; esac
  if "$REC" record -q -o "$f" "$@" >/dev/null 2>"$OUT/$name.log" \
     && "$REC" probe "$f" >"$OUT/$name.probe" 2>&1 \
     && { ! command -v ffmpeg >/dev/null || ffmpeg -v error -i "$f" -f null - 2>>"$OUT/$name.log"; }; then
    frames=$(awk '/vide/{getline; print $3; exit}' "$OUT/$name.probe" | tr -d '(')
    tracks=$(grep -c '^track' "$OUT/$name.probe")
    echo "PASS  $name  ($tracks tracks, $frames video frames)"; pass=$((pass+1))
  else
    echo "FAIL  $name  (see $OUT/$name.log / .probe)"; fail=$((fail+1))
  fi
}
check pattern-1080-h264   --test-pattern -t 3 --no-mic
check pattern-4k-hevc     --test-pattern --noise -r 4k -t 3 --no-mic
check pattern-prores      --test-pattern -t 2 --codec prores --no-mic
if has camera && has microphone; then check pattern-webcam-mic --test-pattern --webcam -t 3; else skipped pattern-webcam-mic "camera/microphone"; fi
if has camera; then check pattern-cam-scene --test-pattern --scene cam -t 2 --no-mic; else skipped pattern-cam-scene camera; fi
# A/V sync: the test pattern flashes white and beeps at the same instant.
if "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/sync.mov" >/dev/null 2>"$OUT/sync.log" \
   && scripts/synccheck.py "$OUT/sync.mov" --max-ms 1 >"$OUT/sync.check" 2>&1; then
  echo "PASS  sync  ($(grep -m1 'audio 0' "$OUT/sync.check" | sed 's/.*offset //'))"; pass=$((pass+1))
else
  echo "FAIL  sync  (see $OUT/sync.check)"; fail=$((fail+1))
fi
# A glitching device (every 50th audio buffer lost) must not shift audio.
if REC_SYNTH_DROP_AUDIO=50 "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/gaps.mov" >/dev/null 2>"$OUT/gaps.log" \
   && scripts/synccheck.py "$OUT/gaps.mov" --max-ms 1 >"$OUT/gaps.check" 2>&1; then
  echo "PASS  audio-gaps  (dropped device buffers filled, still in sync)"; pass=$((pass+1))
else
  echo "FAIL  audio-gaps  (see $OUT/gaps.check)"; fail=$((fail+1))
fi
# Pause/resume: 5 s of output with a pause in the middle, still in sync.
"$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/pause.mov" >/dev/null 2>"$OUT/pause.log" &
sleep 2.3; "$REC" pause || true; sleep 2; "$REC" resume || true; wait || true
if scripts/synccheck.py "$OUT/pause.mov" --max-ms 1 >"$OUT/pause.check" 2>&1 \
   && [ "$(ffprobe -v error -select_streams v -show_entries stream=nb_frames -of csv=p=0 "$OUT/pause.mov")" = 300 ]; then
  echo "PASS  pause-resume  (300 frames, audio in sync across the pause)"; pass=$((pass+1))
else
  echo "FAIL  pause-resume  (see $OUT/pause.check)"; fail=$((fail+1))
fi
# Levels: --system-gain reaches the system audio's own track (here the only
# one). In the app, the system fader moves while recording (0 dB, -12 dB,
# muted, 0 dB) and the file and the replay buffer follow it.
if "$REC" record -q --test-pattern --no-mic --system-gain=-12 -t 3 -o "$OUT/gain.mov" >/dev/null 2>"$OUT/gain.log" \
   && scripts/levelcheck.py "$OUT/gain.mov" --ref=-6 --expect=-12 >"$OUT/gain.check" 2>&1; then
  echo "PASS  gain  (--system-gain -12 on the system audio track)"; pass=$((pass+1))
else
  echo "FAIL  gain  (see $OUT/gain.check)"; fail=$((fail+1))
fi
mkdir -p "$OUT/guilevels"
echo '{"scene": "screen", "mic_enabled": false, "replay_enabled": true, "replay_secs": 30}' >"$OUT/guilevels/settings.json"
if REC_GUI_SELFTEST=levels REC_TEST_PATTERN=1 REC_GUI_BACKGROUND=1 REC_OUTPUT_DIR="$OUT/guilevels" \
     REC_SETTINGS="$OUT/guilevels/settings.json" "$REC" gui >"$OUT/guilevels.log" 2>&1 \
   && f=$(sed -n 's/.*selftest: saved=Some("\(.*\)").*/\1/p' "$OUT/guilevels.log") && [ -n "$f" ] \
   && r=$(sed -n 's/.*replay saved=Some("\(.*\)") error.*/\1/p' "$OUT/guilevels.log") && [ -n "$r" ] \
   && scripts/levelcheck.py "$f" --expect 0,-12,mute,0 >"$OUT/guilevels.check" 2>&1 \
   && scripts/levelcheck.py "$r" --tail --expect 0,-12,mute,0 >>"$OUT/guilevels.check" 2>&1; then
  echo "PASS  gui-levels  (fader moved while recording: file and replay at 0, -12 dB, muted, 0)"; pass=$((pass+1))
else
  echo "FAIL  gui-levels  (see $OUT/guilevels.log, $OUT/guilevels.check)"; fail=$((fail+1))
fi
# Replay buffer: no file recorded, `rec-rs save-replay` mid-run writes the
# last seconds to an MP4 that is in sync.
mkdir -p "$OUT/replay"
"$REC" record -q --test-pattern --no-mic --no-record --replay 15 -t 9 -o "$OUT/replay/none.mov" >/dev/null 2>"$OUT/replay.log" &
sleep 7; "$REC" save-replay >/dev/null 2>&1 || true; wait || true
sleep 1
r=$(ls "$OUT"/replay/replay-*.mp4 2>/dev/null | head -1 || true)
if [ -n "$r" ] && "$REC" probe "$r" >"$OUT/replay.probe" 2>&1 \
   && scripts/synccheck.py "$r" --max-ms 1 >"$OUT/replay.check" 2>&1 \
   && [ ! -e "$OUT/replay/none.mov" ]; then
  echo "PASS  replay  (saved without stopping, $(grep -m1 'audio 0' "$OUT/replay.check" | sed 's/.*offset //'))"; pass=$((pass+1))
else
  echo "FAIL  replay  (see $OUT/replay.log / .check)"; fail=$((fail+1))
fi
# A private MediaMTX (spare ports) that records every path under $1/rec:
# streams are checked through the server's own recordings.
mediamtx_start() {
  mkdir -p "$1"
  cat >"$1/mediamtx.yml" <<YML
logLevel: warn
rtsp: false
hls: false
srt: false
moq: false
rtmpAddress: :19350
webrtcAddress: :18889
webrtcLocalUDPAddress: :18189
pathDefaults:
  record: yes
  recordPath: $1/rec/%path/%Y-%m-%d_%H-%M-%S-%f
  recordFormat: fmp4
paths:
  all_others:
YML
  (cd "$1" && exec mediamtx mediamtx.yml >"$1/server.log" 2>&1) &
  mpid=$!; sleep 2
}
mediamtx_stop() { sleep 2; kill "$mpid" 2>/dev/null || true; wait "$mpid" 2>/dev/null || true; }
# Streaming: RTMP and WHIP from one encode.
if command -v mediamtx >/dev/null; then
  M="$OUT/mediamtx"; mediamtx_start "$M"
  REC_STREAM_KEY=selftest "$REC" record -q --test-pattern --no-mic --no-record -t 10 -o "$M/none.mov" \
    --stream rtmp://127.0.0.1:19350/live --stream http://127.0.0.1:18889/whiptest/whip >/dev/null 2>"$OUT/stream.log" || true
  mediamtx_stop
  for kind in rtmp:live/selftest:1 whip:whiptest:20; do
    name=${kind%%:*}; rest=${kind#*:}; path=${rest%:*}; max=${rest##*:}
    f=$(ls "$M/rec/$path"/*.mp4 2>/dev/null | head -1 || true)
    if [ -n "$f" ] && scripts/synccheck.py "$f" --max-ms "$max" >"$OUT/stream-$name.check" 2>&1; then
      echo "PASS  stream-$name  ($(grep -m1 'audio 0' "$OUT/stream-$name.check" | sed 's/.*offset //'))"; pass=$((pass+1))
    else
      echo "FAIL  stream-$name  (see $OUT/stream.log, $OUT/stream-$name.check)"; fail=$((fail+1))
    fi
  done
else
  echo "SKIP  stream-rtmp, stream-whip  (install mediamtx: brew install mediamtx)"; skip=$((skip+1))
fi
# Quit like the Dock does (-[NSApp terminate:]) mid-recording: the app must
# finalize the file first. Screen only, no mic or camera.
if [ "${1:-}" != "--no-screen" ] && has "screen recording"; then
  mkdir -p "$OUT/term"; echo '{"mic_enabled": false, "scene": "screen"}' >"$OUT/term/settings.json"
  if REC_GUI_SELFTEST=terminate REC_OUTPUT_DIR="$OUT/term" REC_SETTINGS="$OUT/term/settings.json" \
       "$REC" gui >"$OUT/term.log" 2>&1 \
     && f=$(sed -n 's/.*saved on quit=Some("\(.*\)") ok=true.*/\1/p' "$OUT/term.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/term.probe" 2>&1; then
    echo "PASS  app-dock-quit  (quit via terminate: mid-recording saved a playable file)"; pass=$((pass+1))
  else
    echo "FAIL  app-dock-quit  (see $OUT/term.log)"; fail=$((fail+1))
  fi
fi
if [ "${1:-}" != "--no-screen" ] && ! { has camera && has microphone; }; then
  echo "SKIP  screen and app cases  (they record the mic and webcam; grant both first)"; skip=$((skip+1))
elif [ "${1:-}" != "--no-screen" ]; then
  check screen-1080-all   -t 3
  check screen-4k-webcam  -r 4k --webcam -t 3
  check screen-mix.mp4    -t 2 --audio-tracks mix
  # The faders on real devices, in their own sample formats: the file plays,
  # and the mic at the bottom of its fader leaves its track (the third) silent.
  if "$REC" record -q -t 2 --system-gain=-12 --mic-gain=-60 -o "$OUT/screen-levels.mov" >/dev/null 2>"$OUT/screen-levels.log" \
     && "$REC" probe "$OUT/screen-levels.mov" >"$OUT/screen-levels.probe" 2>&1 \
     && [ "$(grep -c '^track' "$OUT/screen-levels.probe")" = 4 ] \
     && scripts/levelcheck.py "$OUT/screen-levels.mov" --audio 2 --expect mute >"$OUT/screen-levels.check" 2>&1; then
    echo "PASS  screen-levels  (system -12 dB, mic at the bottom of its fader: mic track silent)"; pass=$((pass+1))
  else
    echo "FAIL  screen-levels  (see $OUT/screen-levels.log, .probe, .check)"; fail=$((fail+1))
  fi
  # App: scripted preview -> record (the hotkey's command) -> pause/resume
  # -> stop -> settings window -> hide window -> quit.
  mkdir -p "$OUT/gui"; echo '{"scene": "screen-cam"}' >"$OUT/gui/settings.json"
  if REC_GUI_SELFTEST=1 REC_OUTPUT_DIR="$OUT/gui" REC_SETTINGS="$OUT/gui/settings.json" \
       "$REC" gui >"$OUT/gui.log" 2>&1 \
     && grep -q 'idle preview session=true frame_shown=true' "$OUT/gui.log" \
     && grep -q 'paused=true' "$OUT/gui.log" \
     && grep -q 'settings window=true' "$OUT/gui.log" \
     && grep -q 'overlay change live=true applied=true' "$OUT/gui.log" \
     && f=$(sed -n 's/.*saved=Some("\(.*\)").*/\1/p' "$OUT/gui.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/gui.probe" 2>&1 \
     && python3 - "$OUT/gui.log" "$f" <<'PY'
import re, subprocess, sys
log = open(sys.argv[1]).read()
rec, wall = map(float, re.search(r"recorded=([\d.]+)s wall=([\d.]+)s", log).groups())
dur = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", sys.argv[2]],
                           capture_output=True, text=True).stdout)
# The 1.5 s pause must be cut out of the file, and the file must match
# the recorded clock (it keeps running a few frames until the stop).
ok = dur < wall - 1.2 and abs(dur - rec) < 0.4
print(f"pause check: file {dur:.2f}s, recorded clock {rec:.2f}s, wall {wall:.2f}s -> {'ok' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
PY
  then
    echo "PASS  gui-app  (preview, record, pause, saved and probed, pause cut from file, settings window, live overlay change)"; pass=$((pass+1))
  else
    echo "FAIL  gui-app  (see $OUT/gui.log)"; fail=$((fail+1))
  fi
fi
# App: replay buffer kept across window close and a recording, saved with
# the hotkey's command. With mediamtx, one Go Live also streams to three
# destinations at once (two RTMP, one WHIP; YouTube listed but off), and
# they keep going across the window close and the recording.
if [ "${1:-}" != "--no-screen" ] && has "screen recording"; then
  G="$OUT/guilive"; mkdir -p "$G"
  dests='[]'
  if command -v mediamtx >/dev/null; then
    mediamtx_start "$G/mediamtx"
    dests='[{"dest": "youtube", "enabled": false}, {"dest": "rtmp://127.0.0.1:19350/gui1", "enabled": true},
      {"dest": "rtmp://127.0.0.1:19350/gui2", "enabled": true}, {"dest": "http://127.0.0.1:18889/guiwhip/whip", "enabled": true}]'
  fi
  echo "{\"scene\": \"screen\", \"mic_enabled\": false, \"replay_enabled\": true, \"replay_secs\": 15, \"stream_dests\": $dests}" >"$G/settings.json"
  ok=0
  REC_GUI_SELFTEST=live REC_GUI_BACKGROUND=1 REC_STREAM_KEY=selftest REC_OUTPUT_DIR="$G" REC_SETTINGS="$G/settings.json" \
    "$REC" gui >"$OUT/guilive.log" 2>&1 || ok=1
  [ "$dests" = '[]' ] || mediamtx_stop
  if [ $ok = 0 ] \
     && grep -q 'window hidden, live session=true' "$OUT/guilive.log" \
     && grep -q 'after recording live session=true' "$OUT/guilive.log" \
     && f=$(sed -n 's/.*replay saved=Some("\(.*\)") held.*/\1/p' "$OUT/guilive.log") && [ -n "$f" ] \
     && "$REC" probe "$f" >"$OUT/guilive.probe" 2>&1; then
    echo "PASS  gui-replay  (kept across window close and a recording, saved with the hotkey's command)"; pass=$((pass+1))
  else
    echo "FAIL  gui-replay  (see $OUT/guilive.log)"; fail=$((fail+1))
  fi
  if [ "$dests" != '[]' ]; then
    # Each path has video and audio for most of the ~9 s the app streamed.
    got=""
    for path in gui1/selftest gui2/selftest guiwhip; do
      r=$(ls "$G/mediamtx/rec/$path"/*.mp4 2>/dev/null | head -1 || true)
      [ -n "$r" ] || continue
      kinds=$(ffprobe -v error -show_entries stream=codec_type -of csv=p=0 "$r" | sort -u | tr '\n' ' ')
      dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$r")
      [ "$kinds" = "audio video " ] && awk -v d="$dur" 'BEGIN { exit !(d > 6) }' && got="$got $path"
    done
    if grep -q 'live on 3 of 3 destinations, window open' "$OUT/guilive.log" \
       && [ "$(grep -c 'selftest: stream .* live=true' "$OUT/guilive.log")" = 3 ] \
       && [ "$got" = " gui1/selftest gui2/selftest guiwhip" ]; then
      echo "PASS  gui-stream-all  (one Go Live streamed to 3 destinations, all kept across window close and a recording)"; pass=$((pass+1))
    else
      echo "FAIL  gui-stream-all  (received:$got; see $OUT/guilive.log)"; fail=$((fail+1))
    fi
  fi
fi
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
