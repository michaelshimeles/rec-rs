# rec-rs

A simple, fast, reliable screen and camera recorder for macOS, written in Rust, as an alternative
to OBS for recording YouTube videos. It records the screen, system audio, microphone and webcam
into one `.mov` or `.mp4` that plays in QuickTime and imports into Final Cut, Premiere and
DaVinci Resolve. It can also keep a **replay buffer** of the last seconds and **stream** over
RTMP/RTMPS (YouTube, Twitch, Kick, any server) or WHIP (WebRTC) to as many destinations as you
like at once, from one shared hardware encode, with or without recording.

It uses only Apple frameworks:

- **ScreenCaptureKit** for display and window capture, system audio and the microphone. Frames
  are IOSurface-backed `CVPixelBuffer`s that are never copied. The screen is captured at its
  native resolution (a 5K display at 5120x2880) and scaled to the output by a Lanczos filter in
  Metal Performance Shaders, which is much sharper than ScreenCaptureKit's own scaling (see
  [Picture quality](PERFORMANCE.md#picture-quality-2026-10-12)).
- **AVCaptureSession** for the webcam, and optionally the microphone.
- **CoreImage on Metal** for compositing: webcam picture-in-picture and scenes.
- **VideoToolbox through AVAssetWriter** for hardware H.264, HEVC or ProRes 422, plus AAC
  audio.
- **VideoToolbox and AudioConverter directly** for the live encode (H.264, AAC, Opus) that feeds
  the replay buffer and streams. RTMP is a built-in client (TLS through the system's
  Security framework); WHIP uses [str0m](https://github.com/algesten/str0m) (MIT/Apache) for
  WebRTC with Apple CryptoKit. Stream keys live in the macOS Keychain.

There is no FFmpeg, OBS or Electron at runtime. The UI is egui on wgpu in the paper-rs style:
transparent titlebar and the Inter font.

It needs macOS 15 or later, and the download runs on Apple Silicon and Intel. It was developed
and tested on macOS 27 on an M4 Pro MacBook Pro.

The code is a clean-room implementation, licensed MIT OR Apache-2.0. OBS (GPL-2.0) was studied
for its architecture only and no OBS code was copied. See
[analysis/OBS_ARCHITECTURE.md](analysis/OBS_ARCHITECTURE.md).

## Screenshots

![Rec main window: live preview of the recorded screen, scene picker, sources, record button and level meters](docs/screenshots/main.png)

*The main window: a live preview of exactly what gets recorded (here a window showing a test
card), the scene picker (Screen, Screen + Cam, Camera), sources, audio, replay buffer and
stream, record/pause, the timer and level meters.*

| Screen + Cam, squircle with a white border | Screen + Cam, circle moved off the corner |
|---|---|
| ![Camera overlay as a squircle with a white border, snapped to the bottom-right corner](docs/screenshots/cam-squircle.png) | ![Camera overlay as a circle with a yellow border, dragged to a custom position](docs/screenshots/cam-circle.png) |

*Drag the camera anywhere in the preview, or snap it to a corner. It can be a circle, rounded
rectangle, square, pill or squircle, with adjustable roundness, border and shadow. Every change
is live and baked into the recording. The camera here is a virtual camera with a blank feed
(these two are from an earlier build; the overlay controls are unchanged).*

| Settings | Menu bar |
|---|---|
| ![Settings window: output folder, format, resolution, frame rate, codec, bitrate, audio and camera options](docs/screenshots/settings.png) | ![Menu bar menu: Start Recording (⇧⌘R), Pause, Show Rec Window, Settings, Reveal Last Recording, Open Recordings Folder, Quit](docs/screenshots/menu.png) |

## Reverse-engineered origin

rec-rs is an independent, clean-room-style reimplementation. It was reverse engineered from
[OBS Studio](https://obsproject.com) by studying its architecture and behavior with
[REA](https://github.com/morluto/rea) and Claude Code. It is not affiliated with or endorsed by
the OBS Project. No original OBS code or assets are included in this repository.

## Build

```sh
./scripts/build.sh          # cargo build --release, then assemble and sign dist/Rec.app
dist/rec-rs --help          # dist/rec is a short alias
```

Build output goes to `~/.cache/rec-rs-target` (`CARGO_TARGET_DIR`), never into the repo,
because the repo is synced between machines. The script signs `dist/Rec.app` with your Apple
Development identity, so the macOS privacy grants survive rebuilds. Override it with
`REC_SIGN_IDENTITY`. The build is named **Rec Dev** (`com.heyfabrika.rec.dev`), so its grants
stay apart from those of an installed release, which is signed with another certificate.

`scripts/release-macos.sh` builds the universal (Apple Silicon + Intel) app for macOS 15 and
later, signs it with a Developer ID certificate, notarizes it and packages `Rec.dmg`, plus the
zip and signed `appcast.xml` that installed copies update from (one-time credential setup is at
the top of the script). Installed copies check that feed once a day through
[Sparkle](https://sparkle-project.org) and offer to install a newer release; Rec › Check for
Updates…, also in the menu bar item's menu, checks at once.

Sparkle can't replace a copy on the disk image, or one that macOS runs from a temporary
read-only folder. macOS does that to a downloaded app that was opened where it landed and never
moved in Finder (App Translocation). Rec checks for both when it starts and offers to fix it:
it clears the download's quarantine flag on a copy already in Applications, or moves itself
there, and reopens. Until then Sparkle stays off, and Check for Updates… makes the same offer.

Tests:

```sh
cargo test                       # unit tests: pause timeline, mixer, CLI, settings, scenes
scripts/selftest.sh              # records through every pipeline and verifies each file
scripts/selftest.sh --no-screen  # skip the cases that need Screen Recording
scripts/soak.sh 1800             # 30-minute 4K60 soak with CPU/memory sampling and ffprobe checks
scripts/synccheck.py FILE        # A/V offset of a flash + beep recording
scripts/levelcheck.py FILE --expect 0,-12,mute,0  # beep levels of a test-pattern recording
python3 -m doctest scripts/levelcheck.py           # the level checker's own tests
```

Test recordings go to `~/Movies/rec-rs-tests/`.

## Permissions (one time)

```sh
dist/rec-rs permissions --request
```

Then enable **Rec Dev** (or **Rec**, for the installed app) under **System Settings → Privacy &
Security → Screen & System Audio Recording**. Microphone and camera are approved from the
system prompts. Requesting again, here or with the app's Request Permissions button, first clears
any earlier Screen Recording decision so that macOS asks again.

- `rec-rs` re-launches itself with responsibility disclaimed. macOS therefore asks about and
  remembers the signed **Rec** app rather than your terminal.
- If macOS asks whether Rec may "bypass the system private window picker", click **Allow**.
- `dist/rec-rs permissions` shows the current status.

## The app

```sh
open dist/Rec.app          # or: dist/rec-rs   (or: dist/rec-rs gui [--settings] [--start-recording] [--window ID])
```

The **main window** contains:

- **Live preview** of exactly what will be recorded, including the webcam overlay, edge to
  edge in the source's shape. Rec's own windows are excluded from capture. It is rendered at
  the preview's exact size in pixels (2x on Retina) with the Lanczos scaler and drawn 1:1,
  without copying frames through the CPU. Settings has a low-power option (half resolution, 15 fps),
  and nothing is drawn while the window is hidden or the screen is locked.
- **Scene**:
  - Screen.
  - Screen + Cam: the webcam as an overlay. **Drag it in the preview** to place it anywhere;
    released near a corner, it snaps there and stays in that corner when you resize it.
    Shape: circle, rounded rectangle, square, pill or squircle, with a roundness slider for
    the rounded, square and squircle shapes. Size, border (width and color), drop shadow and
    mirroring are in the sidebar and the settings window.
  - All overlay changes apply live, including while recording. They are baked into the
    recorded video by the CoreImage compositor, so there is no extra encode. The shape mask,
    border and shadow are rendered once per size and style; moving the overlay only changes a
    transform.
  - Camera: the webcam full frame.

  While recording you can switch scenes live, provided the recording started with a camera
  scene.
- **Sources**:
  - Display.
  - Entire display, or a single window.
  - Camera.
  - Microphone.
  - System audio.
- **Record / Stop** (⇧⌘R from anywhere), **Pause / Resume** and the recording timer.
- **Audio mixer**: for system audio and the mic, a level meter with peak hold above a fader,
  a mute button and the level in dB.
  - The fader runs from −∞ to +24 dB, with 0 dB marked. Drag it, click the track to jump,
    scroll for fine steps, double-click for 0 dB, or click the value and type one.
  - Changes apply at once, also while recording, to every track of the file and to the replay
    buffer and streams.
  - The meters show the level after the fader. A muted source's meter turns grey and keeps
    moving, so you can see the mic still hears you.
- **Reveal Last** and **Open Folder**.
- **Replay buffer**: keep the last 15–300 s (default 60) in memory, shown with its memory use.
  **Save Replay** or **⇧⌘S** from anywhere writes it to an MP4 in the output folder without
  stopping anything. ⇧⌘S is only taken while the buffer is on, because it is Save As in most
  apps.
- **Stream**: a switch for each of YouTube, Twitch and Kick, plus **Add Server** for any other
  RTMP/RTMPS server or WHIP endpoint. **Go Live** streams to every destination that's on at
  once, and **Stop Streaming** stops them all. If one can't start (a missing key, say), none
  start. Each destination's stream key is typed once and stored in the Keychain (per
  destination URL), never in the settings file or logs. While live, each destination shows its
  duration, bitrate and dropped frames, or its reconnect state and last error. The LIVE badge
  reads LIVE 2/3 when only some of them are up.

  One encode feeds every destination, so it fits all of their limits: YouTube with Twitch
  streams 1080p H.264 at 6 Mb/s to both. The card shows the shared encode when more than one
  destination is on.

  The replay buffer and the stream run with or without a recording. Starting or stopping a
  recording, or closing the window, doesn't interrupt them: the capture keeps running for them
  and short gaps while the app swaps capture sessions are bridged (the last frame repeats,
  silence fills the audio).

The **settings window** (⌘, or the Settings… button) covers:

- Output folder and container (`.mov` or `.mp4`).
- Resolution: 720p, 1080p, 1440p, 4K or native. The frame always has the shape of the display
  or window (4K of a 16:10 display is 3456x2160): no black bars, nothing cropped.
- Frame rate, codec (auto, H.264, HEVC, ProRes) and bitrate.
- Cursor and click highlighting.
- Audio devices and track layout. Levels are set with the faders in the main window.
- Camera device, plus the overlay: shape, roundness, size, border, shadow, corner presets and
  mirroring.
- Live encode for the replay buffer and streams: resolution, video bitrate, CBR, keyframe
  interval and audio bitrate (locked while streaming).

Settings persist in `~/Library/Application Support/rec-rs/settings.json`.

Rec is a regular app with a Dock icon and an app menu (Settings… ⌘,, Hide ⌘H, Quit ⌘Q, Window ▸
Minimize, Close, Show Rec Window). Closing the main window keeps Rec running, in the Dock and the
menu bar, and stops the preview. Clicking the Dock icon brings the window back. Quitting from
the Dock, the menu or a logout finalizes a running recording first.

The **menu bar item** shows a red dot and the elapsed time while recording. Its menu has
Start/Stop, Pause/Resume, Save Replay, Go Live/Stop Streaming, Show Window, Settings, Reveal
Last, Open Folder and Quit. While streaming without recording it shows LIVE.

You can also control a running recording, from the app or the CLI, from any shell:

```sh
rec-rs pause      # SIGUSR1
rec-rs resume     # SIGUSR2
rec-rs stop       # finalizes, prints the file
rec-rs save-replay  # SIGINFO: write the replay buffer to an MP4
```

Quitting the app or sending it SIGTERM mid-recording finalizes the file first.

⇧⌘R is registered system-wide while Rec runs. A browser's hard reload therefore won't see it
until Rec quits. If another app already owns the shortcut, the UI says so.

## Command line

```sh
# the canonical example
rec-rs record --display 1 --mic default --system-audio --cam --out file.mp4 --duration 10

# 4K60 HEVC, screen + webcam, Ctrl-C (or `rec-rs stop`) to finish
rec-rs record -r 4k --cam

# one window at native resolution, camera-only scene, ProRes
rec-rs record --window "Safari" -r native --codec prores
rec-rs record --scene cam -o talking-head.mov
```

The default output is `~/Movies/rec-YYYY-MM-DD-HHMMSS.mov`. A live status line shows the
recorded time, frame counts and peak levels. `--duration` is exact: it is counted in output
frames, and pauses don't count.

| Option | Default | Notes |
|---|---|---|
| `-o, --out, --output` | `~/Movies/rec-….mov` | `.mov` or `.mp4`. |
| `-d, --display N` | `1` | Display number from `rec-rs devices` (1 = main). |
| `--window W` | | Window ID or app/title substring. |
| `--region X,Y,W,H` | | Crop a display region (points from top-left). |
| `-r, --resolution` | `1080p` | `720p`, `1080p`, `1440p`, `4k`, `native`, `WxH`: the largest frame with the display's (or window's) shape that fits that size, e.g. 1670x1080 for a 4112x2658 display. Never black bars, never cropped, never upscaled. |
| `-f, --fps` | `60` | 24, 25, 30, 50, 60, 120. |
| `--codec` | auto | `h264` up to 1080p, `hevc` above, `prores` (422, `.mov`). |
| `--bitrate` | auto | Mb/s. About 20 for 1080p60 H.264, about 50 for 4K60 HEVC. |
| `--system-audio` / `--no-system-audio` | on | |
| `--mic DEVICE` / `--no-mic` | `default` | Unique ID or name substring. |
| `-w, --cam, --webcam [DEVICE]` | off | `default` or ID or name. Turns on the Screen + Cam scene. |
| `--scene` | `screen-cam` with a camera, else `screen` | `screen`, `screen-cam`, `cam`. |
| `--pip-size`, `--pip-corner`, `--pip-shape`, `--no-mirror` | `0.22`, bottom-right, rounded | Shape: `circle`, `rounded`, `square`, `pill`, `squircle` (or `rect`). |
| `--pip-pos X,Y` | | Center the overlay anywhere, as fractions of the frame from the top-left (overrides `--pip-corner`). |
| `--pip-roundness`, `--pip-border`, `--pip-border-color`, `--pip-shadow` | `0.2`, `0`, `ffffff`, off | Roundness 0–1; border width in pixels at 1080p; hex color; soft drop shadow. |
| `--audio-tracks` | `both` | `both` = mix plus system and mic stems; `mix`; `stems`. |
| `--system-gain`, `--mic-gain` | `0` | Level in dB on every track and the stream. −60 or lower is silence, and +24 is the top. |
| `--system-audio-offset-ms`, `--mic-offset-ms` | output latency, `0` | Sync offsets. System audio is shifted by the output device's latency by default (see Output format). |
| `--no-cursor`, `--clicks` | cursor shown | |
| `-t, --duration` | | Seconds of output. |
| `--mic-backend` | `sck` | `sck` (ScreenCaptureKit) or `avcapture`. |
| `--test-pattern [--noise]` | | Synthetic source: the frame flashes white and the "system audio" beeps once a second, for self-tests and sync checks. |
| `--replay SECS` | off | Keep the last 15–300 s of the live encode in memory; `rec-rs save-replay` writes them to an MP4 next to the output. |
| `--stream DEST` | | Stream to `youtube`, `twitch`, `kick`, an `rtmp://`/`rtmps://` URL or a WHIP `http(s)://` URL. Repeatable: one encode feeds every destination. |
| `--no-record` | | Write no file (replay buffer and/or streaming only). |
| `--live-resolution` | `1080p` | Live encode bounding size: the frame keeps its shape and is never above the capture. |
| `--stream-bitrate` | platform preset | Live video kb/s. By default the platform's recommendation for the size and frame rate: YouTube 2160p60 45 Mb/s H.264 or 35 HEVC (2160p30 35/25, 1440p 24/16, 1080p 12/10); Twitch at most 6 Mb/s and 1080p; Kick at most 8 Mb/s and 1080p; others like YouTube. With several destinations, the lowest bitrate and size among them. |
| `--stream-codec` | `auto` | `auto` (HEVC when every destination is YouTube), `h264` or `hevc` (HEVC Main10 over enhanced RTMP or WHIP H.265; a server that refuses it gets H.264). |
| `--keyframe` | `2` | Live keyframe interval in seconds (0.5–4). |
| `--vbr` | CBR | Average bitrate instead of constant. |
| `--audio-bitrate` | `192` | Live AAC/Opus kb/s. |

Other commands:

- `rec-rs devices` lists displays, windows, cameras and mics.
- `rec-rs probe FILE` checks a file with AVFoundation, QuickTime's engine, and decodes every
  sample.
- `rec-rs permissions [--request]` shows or requests the privacy grants.
- `rec-rs stop`, `rec-rs pause` and `rec-rs resume` control a running recording.
- `rec-rs save-replay` saves the replay buffer of the running recorder (CLI or app).
- `rec-rs stream-key DEST [--delete]` stores a stream key in the Keychain (read from the
  terminal with echo off). `REC_STREAM_KEY` overrides it for one run. The app reads and writes
  the same keys.

### Replay buffer and streaming

```sh
rec-rs stream-key youtube                         # once per destination; key goes to the Keychain
rec-rs record --stream youtube --replay 60        # record, stream and keep a replay buffer
rec-rs record --stream youtube --stream twitch --stream kick   # three platforms, one encode
rec-rs record --no-record --stream rtmp://localhost/live --stream http://localhost:8889/live/whip
```

- **One encode.** Every composited frame goes once through a real-time VideoToolbox session,
  together with its own audio mix (AAC; Opus for WHIP). The replay buffer and every stream share
  it, and it keeps running while a recording is paused.
  - Video is H.264 High or HEVC Main10, with no B-frames, a keyframe every 2 s, and CBR or
    average bitrate. The bitstream carries BT.709 limited-range tags.
  - A live size smaller than the capture (record 4K, stream 1080p) is scaled with the same
    Lanczos scaler.
  - HEVC goes out as enhanced RTMP (`hvc1`), as YouTube accepts, or as WHIP H.265. If any
    destination can't take HEVC, the encode switches to H.264.
- **Replay buffer.** Packets are kept from a keyframe, trimmed a GOP at a time. A saved clip
  passes the video through and re-encodes the audio from the decoded AAC, so QuickTime and
  ffmpeg agree on its sync. 60 s at 6 Mb/s uses about 45 MB.
- **RTMP/RTMPS.** Handshake, AMF0 connect/publish, FLV tags, TLS for `rtmps://`. When the
  network falls behind, video is dropped a GOP at a time (queue over 3 s). Reconnects with
  backoff (1 s up to 30 s).
- **WHIP.** SDP offer POSTed with the key as a Bearer token, ICE/DTLS/SRTP on one UDP socket,
  H.264 + Opus. Packets are sent at their capture time and the session starts with audio and
  video aligned, so servers that time tracks by arrival (like MediaMTX) stay in sync. The
  session is DELETEd on stop; a server silent for 5 s triggers a reconnect.
- **Tested** against a local MediaMTX (`brew install mediamtx`), RTMP, RTMPS and WHIP at
  once from one encode: A/V offset in the server's own recordings −0.5 ms (RTMP) and within
  −13 ms (WHIP, of which 6.5 ms is the Opus pre-skip MediaMTX writes into its MP4). Restarting
  the server reconnects within a few seconds. In the app, one Go Live streams to two RTMP paths
  and a WHIP endpoint at once and keeps all three across a window close and a recording.
  `scripts/selftest.sh` runs these checks when `mediamtx` is installed. YouTube, Twitch and
  Kick use the same RTMPS path but were not streamed to in testing.

## Output format

| Track | Contents |
|---|---|
| 1 video | H.264 High, HEVC Main (`hvc1`) or ProRes 422. Constant frame rate, BT.709 tagged, keyframe every 2 s. |
| 2 audio | **Mix** of system and mic: AAC-LC 48 kHz stereo, 256 kb/s. The default track, so players and uploaders use it. |
| 3 audio | System audio stem: AAC 48 kHz stereo, 256 kb/s. |
| 4 audio | Mic stem: AAC 48 kHz mono, 192 kb/s. |

- **Track grouping.** With one audio source there is a single audio track. The audio tracks
  share an alternate group with the mix as default, so QuickTime plays the mix and editors see
  every track.
- **Constant frame rate.** ScreenCaptureKit only sends a frame when pixels change. rec-rs
  writes on an exact 1/fps grid and repeats the newest frame when needed. Measured frame spacing
  is exactly 16.667 ms.
- **Single clock.** Screen, system audio, mic and webcam are all stamped on the host clock, and
  audio is never stretched. The flash/beep sync test measures 0.0 ms between video and audio in
  the file, including across pauses.
- **Levels.** Each source's fader and mute apply before anything else uses its audio, so the
  mix, the source's own track and the live encode get the same level. A change takes at most
  20 ms to complete, so moving a fader or muting doesn't click. At 0 dB a source's own track
  keeps the device's samples untouched.
- **System audio latency.** ScreenCaptureKit captures system audio when it is mixed, which is
  one output latency before you hear it. rec-rs shifts it later by the output device's latency,
  read from CoreAudio (about 27 ms on built-in speakers, more on Bluetooth), so the audio lines
  up with the picture you saw. `rec-rs devices` prints the latency.
- **Webcam sync.** Emission runs a little behind real time, so the webcam frame captured at each
  instant is the one used. Webcams deliver frames 70–120 ms late, and the lag adapts to the
  slowest source.
- **Crash-safe.** Movie fragments are written every 2 s, so a crash or power loss leaves a
  playable file up to the last fragment. A normal stop finalizes a regular movie
  (`ftyp wide mdat moov`).
- **Edge to edge.** The frame has the source's aspect ratio, so the picture fills it with no
  letterbox or pillarbox bars. A single window's transparent rounded corners are filled with
  the window's own edge color instead of encoding as black.
- **Camera.** In the app the running camera passes from the preview to the recording and
  back, so a recording starts with the exposure already settled. A camera started cold (CLI)
  is given 2 s to settle before the file starts, so it never opens on black or dark frames.
- **Audio quality.** System audio and the mic are captured at 48 kHz; a device at another rate
  (44.1 kHz USB, 16/24 kHz Bluetooth) is resampled with a windowed-sinc filter that is
  continuous across buffers. The mix runs through a limiter that keeps it under −1 dBFS
  instead of clipping. Measured with a sweep, tones and noise played as system audio: flat
  within 0.1 dB from 50 Hz to 20 kHz, no clicks.
- **Pause.** Pauses cut whole frame slots out of the timeline. Audio is cut at the exact sample
  and shifted with the video, so there is no gap and no drift.

## Feature comparison with OBS (macOS)

| Feature | OBS 32 | rec-rs |
|---|---|---|
| Display capture | ✅ | ✅ ScreenCaptureKit, zero-copy |
| Window capture | ✅ | ✅ |
| Region / crop | ✅ crop filter | ✅ `--region` (CLI) |
| Application capture (all windows of an app) | ✅ | ❌ |
| Scenes | ✅ unlimited, free layout, transitions | ✅ three fixed scenes (screen, screen + cam, cam), switchable live, cut only |
| Multiple displays/sources at once | ✅ | ❌ one screen source plus the webcam |
| System audio | ✅ | ✅ |
| Microphone | ✅ | ✅ |
| Webcam overlay | ✅ any layout | ✅ drag anywhere with corner snapping; circle, rounded, square, pill or squircle; roundness, border, shadow, mirror; live while recording |
| Separate audio tracks | ✅ up to 6 | ✅ mix plus system and mic stems |
| Per-source gain | ✅ | ✅ live faders from −∞ to +24 dB and mute, also while recording |
| Audio filters (noise suppression, compressor) | ✅ | ❌ |
| Hardware H.264 / HEVC | ✅ | ✅ |
| ProRes | ✅ | ✅ 422 |
| 10-bit / HDR | ✅ | ❌ 8-bit SDR, BT.709 |
| Constant frame rate | ✅ | ✅ exact grid |
| Crash-safe files | ✅ hybrid/fragmented MP4 | ✅ fragments every 2 s |
| Pause / resume | ✅ | ✅ sample-accurate, app and CLI; the pause is cut from the file, audio stays in sync |
| Replay buffer | ✅ | ✅ 15–300 s in memory, ⇧⌘S or `save-replay` writes an MP4 without stopping, memory use shown |
| Streaming | ✅ RTMP, WHIP, SRT, multitrack | ✅ RTMP/RTMPS (YouTube, Twitch, Kick, custom) and WHIP, any number at once from one encode and one Go Live, H.264 or HEVC Main10 (enhanced RTMP), platform bitrate presets up to 4K60, auto-reconnect, live status; key in the Keychain |
| Screen scaling | GPU bilinear/bicubic/Lanczos | ✅ native capture, Metal Lanczos per plane (5K→4K VMAF 100 vs 90 for ScreenCaptureKit) |
| Live preview | ✅ | ✅ |
| Level meters | ✅ | ✅ peak with hold, after the fader |
| Global hotkey | ✅ configurable | ✅ ⇧⌘R record, ⇧⌘S save replay (fixed) |
| Menu bar control | ❌ | ✅ menu bar item plus Dock app |
| Headless CLI | ❌ (obs-cli via websocket) | ✅ `rec-rs record …`, `stop`, `pause`, `resume` |
| Footprint | about 300 MB app | about 22 MB app; about 6% of one core at 4K60 screen-only, about 34% with the webcam overlay (see [PERFORMANCE.md](PERFORMANCE.md)) |
| Encoder input | frames copied back to the CPU for encoding | IOSurface passed straight to VideoToolbox |

## Known gaps

- **Scenes.** There are three fixed scenes with no free layout editor and no transitions. You
  can only switch live into a camera scene if the recording started with the camera on.
- **Capture sources.** There is no application capture or multi-display capture. Region capture
  is CLI-only.
- **Missing features.** There are no audio filters, and the hotkeys aren't configurable.
- **Streaming.** RTMP/RTMPS (with enhanced-RTMP HEVC) and WHIP only, with no SRT or
  multitrack. Every destination gets the same encode, at the lowest limits among them, so
  adding Twitch caps a 4K YouTube stream at 1080p. The live encode is never larger than the
  recording size, so a 4K stream needs the 4K output setting. Changing the live encode settings
  or the destinations needs streaming stopped. HEVC to YouTube was tested only against a local
  server, not YouTube itself.
- **8-bit SDR only.** HDR displays are tone-mapped by ScreenCaptureKit.
- **Webcam overlay.** The overlay is burned into the video; there is no separate camera track.
- **Output latency read once.** It is read when recording starts. Switching output devices
  mid-recording (to AirPods, say) leaves the old offset in place.
- **Mix track resampling.** A mic that isn't 48 kHz is resampled linearly per buffer for the mix
  track. The mic stem is converted by AVFoundation.
- **Re-confirmation prompt.** macOS periodically asks you to re-confirm screen recording for
  apps that bypass the system picker. This happens to every recorder.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI |
| `src/app/` | App: windows and UI (`ui.rs`), menu bar item and app menu, Dock quit/reopen hooks, preview scaler, theme, macOS chrome, Sparkle updates and moving into Applications (`install.rs`) |
| `src/settings.rs` | Persisted settings → record arguments |
| `src/hotkey.rs` | Global hotkey (Carbon `RegisterEventHotKey`; needs no Accessibility permission) |
| `src/record.rs` | Sessions, pacing thread, signals, `stop`/`pause`/`resume` |
| `src/writer.rs` | AVAssetWriter sink, constant-frame-rate grid, pause timeline, track layout |
| `src/screen.rs` | ScreenCaptureKit capture, displays and windows |
| `src/camera.rs` | Webcam and AVCapture mic, device lists |
| `src/compose.rs` | CoreImage scenes and webcam overlay |
| `src/mixer.rs` | Real-time audio mixer, faders (level, mute, gain ramps) |
| `src/live/` | Shared live encode (VideoToolbox H.264, AAC/Opus), replay buffer, RTMP/RTMPS and WHIP outputs, Keychain |
| `src/tcc.rs` | Permissions and responsibility disclaim |
| `src/probe.rs` | AVFoundation verification |
| `src/synth.rs` | Test pattern with flash + beep |
| `src/scale.rs` | Native-resolution screen scaling (Metal Performance Shaders Lanczos, VideoToolbox fallback) |
| `scripts/` | Build, self-test, soak, sync checks (`synccheck.py` with ffmpeg, `avsync.swift` with AVFoundation), level checks (`levelcheck.py`) and quality metrics (`quality.sh`, `encquality.sh`, `testcard.swift`) |
| `site/` | Download page: SvelteKit, deployed to Vercel (see [site/README.md](site/README.md)) |
| `AGENTS.md`, `.agents/skills/` | Agent workflow, checks and skills from [michaelshimeles/skills](https://github.com/michaelshimeles/skills); `.claude/skills/` links to the same skills |
| `analysis/OBS_ARCHITECTURE.md` | OBS study |
