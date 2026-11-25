# PERFORMANCE

All numbers come from this machine: a MacBook Pro with an M4 Pro, macOS 27, and a Retina
display at 4112×2658 pixels. Builds are release builds of `rec-rs`.

## How the numbers are measured

- **CPU and memory.** The recorder process's `ps` `%cpu` (100% = one core) and resident memory
  are sampled every 3–5 s (`scripts/soak.sh`). The figures cover the recorder only. The hardware
  video encoder and the WindowServer's capture work run outside the process and are not
  included.
- **ffprobe checks.**
  - Frame count against the expected `duration × 60`.
  - Every gap between consecutive video timestamps; at constant 60 fps each must be 16.667 ms.
  - Per-stream durations, which shows how far apart audio and video end.
- **AVFoundation.** `rec-rs probe` opens each file with QuickTime's engine and decodes every
  sample.
- **A/V sync** (`scripts/synccheck.py`). The test pattern flashes white and beeps at the same
  host-clock instant once a second. The script pairs each flash frame with the nearest beep
  onset in each audio track and reports the offset.

## 30-minute soaks (milestone 7)

| Run | Setup | Frames | Dropped | Frame gaps | A/V end | CPU mean / max | Memory |
|---|---|---|---|---|---|---|---|
| Soak 1, 08:34–09:04 | 4K60 HEVC, screen + system audio + mic + webcam overlay, 3 audio tracks | 107,955 / 108,000 | **45**, all in one 5-s window (see below) | 16.667 ms except 8 gaps of 100–133 ms at 13:27 | all 3 tracks end within 0.1 ms of the video, at 1800.000 s | 34.4% / 45.0% | 64–78 MB steady (max 151 MB at start), no growth |
| Soak 2 (after the fix), 09:29–09:59 | 4K60 HEVC, screen + system audio (the mic and camera grants had been reset; see PROGRESS) | **108,000 / 108,000** | **0** (0 encoder retries) | 16.667 ms everywhere (0 irregular) | 0.0 ms | 6.0% / 8.8% | 31–41 MB, no growth |
| Soak 3 (after the fix), 10:17–10:47 | 4K60 HEVC, screen + system audio + mic + webcam overlay, 3 audio tracks | 106,623 / 108,000 | 1,377, almost all in one 21.7-s freeze at 10:44 (see below) | 16.667 ms except 7 gaps (366–417 ms at 10:24:43 and 10:24:56; 21.7 s at 10:44:18) | 0.0 ms | 25.4% / 39.8% | 27–63 MB, no growth |

Both files played in AVFoundation (`playable: true`, every sample decoded) with no ffmpeg
decode errors.

**Soak 1 frame drops.** Every one of the 45 drops fell between 08:48:10 and 08:48:15, in pairs
about every 1.5 s. The other 29 min 55 s had none. During that window the hardware encoder
reported "not ready" (`isReadyForMoreMediaData == false`). Something else on the machine was
loading the media engine at the time; the Dock icon files were being created about then. The
writer used to drop a frame slot the moment the encoder was busy.

**Fix** (commit `fa747f4`). A busy encoder now makes the writer retry the same slot, with the
same timestamp, on the next pacer pass, about 8 ms later. A slot is dropped only once it is
more than 0.5 s overdue, which only happens under sustained overload. With one 4K60 stream at
worst-case content, the encoder was never busy: 0 retries and 0 drops (table below).

**Soak 3 was disturbed by the machine, not the recorder.** At about 10:44 the MacBook's lid
was closed with two 5K displays attached (clamshell mode; `AppleClamshellState = Yes`).

- **Display reconfiguration.** macOS reconfigured the displays, so ScreenCaptureKit delivered
  no frames for 21.7 s. The recorder's CPU fell to 0% for those samples. The writer held the
  last frame and resumed on the exact grid; the file stayed playable and the audio was
  untouched (all tracks end at 1800.000 s).
- **Camera.** The built-in camera is unavailable with the lid closed, so the overlay delivered
  0 camera frames after that point.
- **Earlier hiccups.** The two shorter gaps at 10:24 coincide with a system NFC/Bluetooth stack
  load (`nfcd` power assertions).

Soak 1, whose only stall was the encoder's (now handled), and soak 2, a clean 108,000 of
108,000, are the representative runs. A clean webcam soak needs the lid open.

## Single-run measurements (60 s each, after the fix)

| Source | Resolution / codec | Frames | Dropped | Encoder-busy retries | CPU mean / max | Memory max |
|---|---|---|---|---|---|---|
| Real screen + system audio | 1080p60 H.264, 19 Mb/s | 3600 / 3600 | 0 | 0 | **5.9%** / 8.2% | 39 MB |
| Real screen + system audio | 4K60 HEVC, 50 Mb/s | 3600 / 3600 | 0 | 0 | **5.9%** / 6.2% | 40 MB |
| Real screen + system audio + mic + webcam overlay (soak 1) | 4K60 HEVC | — | — | — | 34.4% / 45.0% | 78 MB |
| Test pattern (software-drawn) + beep audio | 1080p60 H.264 | 3600 / 3600 | 0 | 0 | 12.7% / 14.3% | 53 MB |
| Test pattern | 4K60 HEVC | 3600 / 3600 | 0 | 0 | 27.1% / 28.8% | 107 MB |
| Test pattern with full-frame noise (encoder worst case, about 50 Mb/s) | 4K60 HEVC | 3600 / 3600 | 0 | 0 | 47.4% / 49.0% | 110 MB |

How to read these:

- **Screen-only capture is about 6% of one core at 4K60.** ScreenCaptureKit hands over
  IOSurface-backed frames, and rec-rs passes them to VideoToolbox without copying or converting
  them.
- **The webcam overlay is the expensive part, at about 34%.** Each output frame is composited by
  CoreImage on the GPU into a BGRA buffer, and the 4:2:0 conversion then happens in the encoder.
  This is still far below OBS, which renders every frame through its own GPU pipeline and then
  copies it back to the CPU before encoding (`analysis/OBS_ARCHITECTURE.md`, section 6).
- **Test-pattern CPU is mostly the pattern itself,** which is drawn in software at 4K60. It is
  a stress source, not a measure of the recorder's own cost.

## Overload behaviour (deliberate)

Several simultaneous 4K60 recordings of full-frame noise exceed what the hardware HEVC encoder
can sustain.

| Concurrent 4K60 noise recordings | Frames written per stream (of 1200 in 20 s) | Notes |
|---|---|---|
| 2 at default bitrate | 1042–1043 (13% dropped) | about 1600 retries each before slots aged past 0.5 s |
| 3 at 150 Mb/s | 598–603 (50% dropped) | |

In both cases the files stayed valid and correctly timed:

- Durations were exactly 20.000 s.
- Timestamps never went backwards.
- Audio stayed in sync, because it does not depend on video.

Under overload, frames are dropped at the 0.5 s limit rather than letting latency and memory
grow without bound.

## Crash safety

`kill -9` of a 4K60 recording at about 8.5 s left a file that AVFoundation plays and decodes
completely: 8.000 s of video and audio, everything up to the last 2 s fragment. ffmpeg also
decoded it with no errors.

## A/V sync

| Test | Result |
|---|---|
| Test pattern, 8–30 s, mix and system tracks | Offset **0.0 ms** on every flash, in every run, after the fix |
| Sync across two pauses | 0.0 ms |
| Every 50th audio buffer dropped (a simulated glitching device) | 0.0 ms. The 20 gaps were filled with silence, so audio stays on the capture clock. |
| Soak 1, 30 min, real devices | All audio tracks end within 0.1 ms of the 1800.000 s video, so there is no drift |
| End to end through the real capture path: AVPlayer plays a flash/beep clip, rec-rs records its window and system audio | Before the output-latency fix: audio early by 25.0, 15.9 and 16.7 ms (mean about -19 ms). After: +4.0, -0.9, -10.4 and -16.0 ms (mean about -6 ms). |

**Reading the end-to-end numbers.** Flash detection is quantized to the 60 fps capture grid,
and AVPlayer's frame presentation shifts by up to a frame from run to run. Single runs therefore
vary by about ±15 ms, so compare means. All results are inside the roughly 40 ms threshold where
audio leading video becomes noticeable. With ffplay as the player instead of AVPlayer, audio
came out 64–81 ms late; ffplay's own A/V sync is looser, so AVPlayer is the reference.

**Output-latency fix** (commit `f56977a`). ScreenCaptureKit captures system audio when it is
mixed. It is heard one output latency later, and players schedule audio ahead by that amount so
it matches the picture. rec-rs now shifts system audio later by the default output device's
presentation latency, as reported by the CoreAudio HAL: 27.3 ms on the built-in speakers here.
On Bluetooth headphones the latency is far larger, often 150 ms or more, and without the shift
the recording would have audio that early. `--system-audio-offset-ms` and `--mic-offset-ms`
override the shift. The latency is read once, when recording starts.

**Bug found while measuring, now fixed** (commit `fa747f4`). AVAssetWriter lays audio buffers
end to end and ignores gaps in their timestamps. A device buffer that arrived before the first
video frame, or one that was lost, therefore shifted all later audio earlier by its length (10
ms). This was seen intermittently as -10 ms. The writer now tracks each audio stem's written
position and fills gaps with silence or trims overlaps to the exact sample, so the error stays
under 1.5 ms. Audio that arrives before the first frame is held and trimmed rather than dropped.

## Live encode: replay buffer and streaming

30 s of 1080p60 test pattern with noise (worst case for the encoder), CLI, measured with
`/usr/bin/time -l`:

| Run | CPU time (user + sys) | Peak memory |
|---|---|---|
| Recording only | 7.5 s (25% of one core) | 59 MB |
| Recording + 60 s replay buffer + RTMP stream | 7.1 s (24%) | 169 MB |
| No file; replay + RTMP + WHIP | 7.1 s (24%) | 123 MB |

The extra H.264 encode runs on the VideoToolbox hardware encoder and costs no measurable CPU.
Memory grows with the replay buffer: the payload is about bitrate × seconds (60 s at 6 Mb/s is
about 45 MB, shown in the app). On top of that come the encoder's and the RTMP queue's buffers.

Stream timing, measured on MediaMTX's own recordings with `scripts/synccheck.py`:

| Output | A/V offset |
|---|---|
| RTMP | −0.6 to +0.4 ms |
| WHIP | −0.5 to −12.7 ms, including MediaMTX's 6.5 ms Opus pre-skip; the rest is onset detection on lossy Opus |
| Replay clip (AVFoundation and ffmpeg) | 0.0 ms |

With a busy CBR stream, WHIP sends only the real picture data. str0m drops the H.264 filler
units VideoToolbox pads CBR with, so a static screen streams at about 0.5 Mb/s over WebRTC
and at the full 6 Mb/s over RTMP.

## Separate screen and camera files (2026-11-24)

`--separate` writes the screen and the camera to files of their own (see the README). These
numbers come from a release build of the feature branch on the same machine. The machine was
busy with other work during the runs: load average 6 to 9, with the installed Rec Dev app using
about 40% of a core.

### Sync

The test sources (`--test-pattern --test-camera`) draw each frame's capture time, on the host
clock in 1/60000 s, as a row of black and white blocks. `scripts/trackcheck.py` reads it back
from every frame of both files. For each camera frame it takes the first slot that shows it and
compares its capture time with the screen frame's in the same slot. 1080p60 H.264, system audio
(the test beeps), no mic.

| Take | Files | Frames, screen / camera | Length | Camera frames in the file | Offset, first 10 s | Offset, last 10 s | Drift | Audio, camera file vs screen file | A/V in each file |
|---|---|---|---|---|---|---|---|---|---|
| 1 min | screen, camera | 3600 / 3600 | 60.000 s each, both start at 0 | 1800 of 1800 | 0.000 ms (300 frames) | 0.000 ms (300 frames) | 0.000 ms | 0.000 ms at the start and the end | 0.0 ms, 60 of 60 flashes |
| 10 min | composed, screen, camera | 36000 / 36000, composed 36000 | 600.000 s each, both start at 0 | 18000 of 18000 | 0.000 ms (300 frames) | 0.000 ms (300 frames) | 0.000 ms | 0.000 ms at the start and the end | 0.0 ms, 600 of 600 flashes, all three files |

- Both takes pass `trackcheck.py --strict`: every slot of the screen file shows a new screen
  frame, every frame gap is 16.667 ms, and every camera frame shows up in the camera file.
- The camera sends 30 fps into a 60 fps file, so every other slot repeats its previous frame.
  Over all slots the camera picture is 8.3 ms older than the screen's on average (0 or 16.7
  ms), the same as in the composed video.
- Both test sources run on the host clock, so this measures the recorder: one start time, one
  frame grid, the same pauses, no lost frames. A real camera's frames carry AVFoundation
  timestamps on the same clock. Their delivery delay (70 to 120 ms for webcams) is what the
  emission lag already absorbs for the overlay.
- A 5 s take with a 2 s pause has 300 frames in each file and the same 122-slot cut in both.
- `trackcheck.py` fails a camera file shifted by one frame: +16.7 ms, 239 frames against 240.

**Real camera.** 8 s with a DJI Osmo Pocket 3 and the test pattern as the screen: Rec picked
the camera's 1280x720 landscape format, and the camera file is 1280x720. Both files have 480
frames, last 8.000 s and start at 0. The test camera's letterbox mode, a 720x1280 frame with a
16:9 picture, gives a 720x400 camera file with no bars.

### CPU

The recorder's `ps` `%cpu` every 2 s over 60 s (`SOAK_EVERY=2 scripts/soak.sh 60 ...`), mean /
max in % of one core, and its peak memory. 1080p60 H.264.

| Files | Test pattern + test camera, system audio | Real screen (5K display scaled to 1080p) + test camera, no audio |
|---|---|---|
| Composed video only, as before | 33.6% / 58.5%, 140 MB | 25.3% / 36.2%, 70 MB |
| Separate files only | 26.2% / 39.5%, 114 MB | 19.8% / 25.9%, 70 MB |
| Separate files and the composed video | 46.1% / 62.5%, 150 MB | 43.8% / 54.8%, 75 MB |
| Separate files and the composed video, 10 min, `ps` every 5 s | 56.4% / 70.6%, 151 MB | |

- Separate files alone cost less than the composed video, because nothing is composited. The
  screen frame goes to its encoder as captured and scaled. CoreImage draws the camera's picture
  once per camera frame, 30 times a second, not once per output frame.
- Writing all three files costs about 13 to 18 points of a core more than the composed video
  alone.
- The test sources are drawn in software inside the recorder, so that column runs higher than
  with a real screen.
- On this shared machine the numbers move between runs: the 10-minute take with all three files
  averaged 56.4%, the 60 s take 46.1%. After the review fixes, a repeat of the 60 s separate
  take used 31.4% / 35.5% and again passed `trackcheck.py --strict` at 0.000 ms.
- After merging main with the camera look (#19), the same 60 s take passed `--strict` at 0.000
  ms at the start and the end with 0.000 ms drift, at 41.7% / 45.4%. Exact CPU time over
  alternating 20 s takes put this about 5 points above the build before the merge, with the
  merge alone (32.8 to 41.9%) and with the look wired into the camera file (36.1 to 39.8%)
  alike, against 30.6 to 34.0% before. The bump came with the merge, not the look wiring, and
  the numbers vary from run to run.

### A stall seen once

One 60 s take with all three files had a single stall at 40.8 s: both capture threads waited
140 ms to hand over a frame. That stretched the emission lag past what the 7-frame screen queue
covers at 60 fps (about 115 ms), so the screen file repeated its last frame for 32 slots and
then skipped ahead. The composed file shows the same. The camera file lost nothing.

The capture threads hand frames to the same lock the pacer holds while it composites and
encodes a slot, so a long pacer pass delays them. A debug counter now prints the longest pass
(`REC_DEBUG=1`, "longest pacer pass"). Over six more 60 s takes, three composed-only and three
with all three files, the longest pass was 19.9 to 34.2 ms with all three files and 25.5 to
81.1 ms with the composed video alone. No take repeated a screen frame, and neither did the
10-minute take (worst arrival delay 52.7 ms). The composed-only path has the same exposure:
any pass over about 115 ms costs screen frames.

A single late frame does the same on a small scale. A screen frame that reaches the writer after
its slot went out leaves the previous frame in that slot, and the next slot catches up. Two
selftest takes on the busy machine had one such slot each, in the screen file and the composed
file alike, with the camera still at 0.000 ms. `trackcheck.py` therefore measures each camera
frame against its slot's own instant, allows late screen frames in up to 1% of the slots, and
allows none with `--strict`, which the takes above passed.

## Picture quality (2026-10-12)

Measured with real metrics against lossless references (ffmpeg 8.1:
`ssim`, `psnr`, `libvmaf` with the 4K model at 2160p), on an M4 Pro.
Values are SSIM-Y / PSNR-Y (dB) / VMAF.

**Reference.** `scripts/testcard.swift` draws a static 5120x2880 text card
(UI text at 9-24 pt, code, colored text on colored backgrounds, 1 px lines,
gradients) and shows it 1:1 on a 5K display. A lossless `screencapture` of
it, converted to sRGB (64 dB from the drawn card, so the display path is
exact), is the reference, scaled to each output size with Lanczos in RGB.
`scripts/quality.sh` records or streams that window and scores the middle
second.

### Recording: capture and scaling (5K display to 4K, 1440p, 1080p)

ProRes, so the scaler is measured rather than the encoder:

| Scaler | 4K | 1440p | 1080p |
|---|---|---|---|
| Before: ScreenCaptureKit scales (`scalesToFit`) | 0.9948 / 35.0 / 90.2 | 0.9943 / 35.0 / 91.9 | 0.9948 / 35.3 / 88.8 |
| VideoToolbox pixel transfer, area average (tried) | 0.9967 / 36.6 / 96.4 | 0.9869 / 30.0 / 95.2 | 0.9737 / 27.9 / 79.2 |
| **After: native capture + Metal Lanczos per plane** | **0.9971 / 40.9 / 100** | **0.9955 / 40.7 / 100** | **0.9934 / 39.6 / 98.5** |
| Native 5K, no scaling (capture path alone) | 0.9995 / 48.4 / 100 | | |

Default settings, encoded: 4K HEVC 0.9973 / 42.7 / 100, 1080p H.264
0.9935 / 40.7 / 97.3 (before: 0.9948 / 35.3 / 87.6). Capturing at native
5K and scaling on the GPU costs about 3% of one core more (CPU time 0.96 s
-> 1.57 s over 20 s of 4K recording) and 4 MB of memory.

### Streaming: 4K into a local MediaMTX, scored on its own recording

| Stream | Result |
|---|---|
| Before: SCK scaling, old encoder settings, 6 Mb/s H.264 (old default), 4K | 0.9942 / 33.5 / 87.6 |
| Before: same at 1080p | 0.9931 / 33.5 / 86.3 |
| After: RTMP H.264 High, 45 Mb/s (YouTube 4K60 preset) | 0.9974 / 41.8 / 100 |
| After: enhanced RTMP HEVC Main10, 35 Mb/s (YouTube HEVC preset) | 0.9947 / 41.6 / 100 |
| After: WHIP H.265 Main10 | 0.9947 / 41.6 / 100 |
| After: 1080p stream from a 4K capture (Lanczos, 12 Mb/s preset) | 0.9938 / 40.1 / 96.6 |

A static card hides encoder differences (it compresses to almost
nothing), so the encoder itself was measured on motion with
`scripts/encquality.sh`: 2 s lossless 4K60 clips (the card scrolling 6 px
a frame, and ffmpeg's testsrc2), encoded by the live encoder in real time,
PSNR-Y / VMAF:

| Encode | Scrolling text | Motion graphics |
|---|---|---|
| H.264, 6 Mb/s (old default) | 43.9 / 99.5 | 33.9 / 76.1 |
| H.264, 45 Mb/s (YouTube 4K60 preset) | 54.1 / 99.98 | 40.7 / 94.4 |
| HEVC Main, 35 Mb/s | 49.5 / 99.95 | 39.7 / 93.9 |
| **HEVC Main10, 35 Mb/s (YouTube HEVC preset)** | 47.6 / 99.98 | **41.1 / 95.3** |
| HEVC Main10, 25 Mb/s (4K30 preset) | 46.9 / 99.94 | 39.2 / 93.0 |

- **Bitrate is what matters at 4K.** 6 Mb/s gives VMAF 76 on motion, and
  the platform presets give 94-95.
- **HEVC at 35 Mb/s beats H.264 at 45.**
- **Main10 is worth it.** It gains 1.4 dB / 1.4 VMAF on motion over Main.
- **Some settings had no effect.** Quality-over-speed and power hints made
  no measurable difference on this chip, since real-time mode already
  favors quality.
- **Look-ahead was dropped.** VideoToolbox's look-ahead overshot the
  target bitrate (39 vs 34 Mb/s) without a clear gain.
- **Color tags.** Streams carry BT.709 limited-range tags. A gray ramp
  encodes to Y 16-234, so there's no range error.

### App preview

The preview pixels, measured offline (`cargo test --release preview_pixels --
--ignored`, from a real 4K frame of the card), against the card scaled
ideally to the preview's size:

| Preview size | Before: 960 px image stretched by egui | After: Lanczos to the exact pixel size, 1:1 |
|---|---|---|
| Default window (1436x808 px) | 0.952 / 27.0 / 45.9 | 0.991 / 36.5 / 94.0 |
| Large window (2780x1564 px) | 0.927 / 24.0 / 16.6 | 0.994 / 36.7 / 94.2 |

- **CPU per frame:** 1.3-1.9 ms before (CoreImage render, CPU readback and
  swizzle), not counting the texture upload. After: 0.25-0.39 ms. The
  scaled frame's IOSurface is the texture, with no copies.
- **GPU wait:** the 2.6-6.5 ms wait for the GPU happens on a worker
  thread, not the UI thread.
- **Low-power mode** halves the resolution and the frame rate.
- **Hidden windows:** while the window is covered, minimized or the screen
  is locked, the app no longer draws. It went from about 100% of a core to
  2% with the screen locked.
- **Not measured on screen.** An on-screen screenshot of the running app
  couldn't be taken during this work: the screen locked, then the 5K
  displays were disconnected. The numbers above are the texture's pixels,
  which the UI draws 1:1.

## Camera adjustments and preview guides (2026-11-24)

The compositor with a 1920x1080 camera into a 1080p frame, 240 frames per case after a warm-up
(`cargo test --release adjustment_cost -- --ignored --nocapture`). Wall time is the render
call, which waits for the GPU; CPU is this process's CPU time.

| Camera adjustments | Camera scene: wall / CPU per frame | Screen + Cam: wall / CPU per frame |
|---|---|---|
| Neutral (no filters run) | 0.47 / 0.33 ms | 0.66 / 0.68 ms |
| White balance, exposure, contrast, saturation | 0.46 / 0.34 ms | 0.71 / 0.72 ms |
| Osmo studio preset (sharpness -1: a blur) | 1.29 / 0.72 ms | 1.53 / 1.30 ms |
| Everything, with sharpening and noise reduction | 2.03 / 1.26 ms | 2.52 / 1.69 ms |

- The color adjustments cost nothing measurable: CoreImage folds them into the pass that
  draws the camera.
- Softening, sharpening and noise reduction sample neighboring pixels and add about 0.8 to
  1.6 ms of GPU time per frame. At 60 fps, the Osmo studio preset's blur adds about 2.3% of
  one core of CPU, and everything at once about 5.6%.

Zebras run on the preview worker, on its own copy of a 2780x1564 preview frame, at most
30 times a second and only while they're on (`cargo test --release zebra_cost -- --ignored
--nocapture`): 1.00 ms wall and 0.61 ms CPU per frame for the full-frame camera, 2.53 ms wall
and 1.13 ms CPU for a rounded overlay. The thirds grid is four lines drawn by the UI.

## Reproduce

Quality: `scripts/quality.sh` (needs a 5K 2x display and `mediamtx` for
the stream cases), `scripts/encquality.sh`, and `cargo test --release
preview_pixels -- --ignored --nocapture` with `REC_PV_SRC`, `REC_PV_OUT`,
`REC_PV_AREA`.


```sh
scripts/soak.sh 1800                    # 4K60 + webcam + mic + system audio, 30 min
scripts/soak.sh 1800 -r 4k --no-mic     # screen + system audio only
scripts/selftest.sh                     # includes sync, pause, audio-gap and app cases
REC_DEBUG=1 dist/rec-rs record ...      # prints latencies, encoder retries, gaps filled, longest pacer pass
# separate files: 1-minute sync take and CPU, then the frame-by-frame check
SOAK_EVERY=2 scripts/soak.sh 60 --test-pattern --no-mic --test-camera --separate
scripts/trackcheck.py NAME-screen.mov NAME-camera.mov --frames 3600 --strict   # NAME: the soak file it printed
```
