mod app;
mod camera;
mod compose;
mod hotkey;
mod live;
mod mixer;
mod outlatency;
mod play;
mod probe;
mod record;
mod scale;
mod screen;
mod settings;
mod synth;
mod tcc;
mod util;
mod writer;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use compose::{Corner, PipSpec, Shape};
use writer::Codec;

#[derive(Parser)]
#[command(name = "rec-rs", version, about = "Native macOS screen recorder (ScreenCaptureKit + VideoToolbox + AVAssetWriter)")]
struct Cli {
    /// Without a subcommand, rec starts the menu bar app.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

// RecordArgs is large but parsed once; boxing it buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Record the screen (plus system audio, mic, and optional webcam overlay).
    Record(RecordArgs),
    /// The app: window with live preview and sources, settings, menu bar
    /// item and a global Cmd+Shift+R hotkey (what `open dist/Rec.app` starts).
    Gui {
        /// Start recording immediately (e.g. from a Login Item or script).
        #[arg(long)]
        start_recording: bool,
        /// Open the settings window at launch.
        #[arg(long)]
        settings: bool,
        /// Capture this window (ID from `rec-rs devices`) instead of the display.
        #[arg(long, value_name = "ID")]
        window: Option<u32>,
    },
    /// List displays, windows, cameras and microphones.
    Devices,
    /// Stop a running recording (CLI or app) and print its file.
    Stop,
    /// Save the replay buffer of the running recorder (CLI or app) to MP4.
    SaveReplay,
    /// Store a stream key in the macOS Keychain for a destination (reads the
    /// key from stdin; never echoed or logged).
    StreamKey {
        /// Stream URL (rtmp://, rtmps://, https://.../whip) or a service:
        /// youtube, twitch, kick.
        dest: String,
        /// Remove the stored key instead.
        #[arg(long)]
        delete: bool,
    },
    /// Pause the running recording (resume with `rec-rs resume`).
    Pause,
    /// Resume a paused recording.
    Resume,
    /// Show (and optionally request) screen, microphone and camera permissions.
    Permissions {
        /// Trigger the system permission prompts for anything not yet granted.
        #[arg(long)]
        request: bool,
        /// Seconds to wait for prompt answers when requesting.
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// Request Screen & System Audio Recording (run by `permissions --request`).
    #[command(hide = true)]
    RequestScreen,
    /// Check a recording with AVFoundation (the engine QuickTime uses).
    Probe { file: PathBuf },
    /// Play a file in an AVPlayer window (reference player for sync tests).
    #[command(hide = true)]
    Play {
        file: PathBuf,
        #[arg(long, default_value_t = 15.0)]
        secs: f64,
        /// Window content size in points, WIDTHxHEIGHT.
        #[arg(long, default_value = "640x360")]
        size: String,
    },
}

#[derive(Args)]
pub struct RecordArgs {
    /// Output file (.mov or .mp4). Default: ~/Movies/rec-<timestamp>.mov
    #[arg(short, long, visible_alias = "out")]
    output: Option<PathBuf>,
    /// Display number from `rec-rs devices` (1 = main display).
    #[arg(short, long, default_value_t = 1)]
    display: usize,
    /// Record only this region of the display: X,Y,WIDTH,HEIGHT in points
    /// from the display's top-left corner.
    #[arg(long, value_name = "X,Y,W,H", conflicts_with = "window")]
    region: Option<String>,
    /// Record a single window: window ID or app/title substring (`rec devices`).
    #[arg(long, value_name = "WINDOW")]
    window: Option<String>,
    /// Output size: 720p, 1080p, 1440p, 4k, native, or WIDTHxHEIGHT. The
    /// frame keeps the display's (or window's) aspect ratio and fits inside
    /// this size (1080p of a 16:10 display is 1728x1080), so there are never
    /// black bars; it is never larger than the source.
    #[arg(short, long, default_value = "1080p")]
    resolution: String,
    /// Accepted for compatibility: the frame always keeps the source's
    /// aspect ratio now.
    #[arg(long, hide = true)]
    keep_aspect: bool,
    /// Frames per second.
    #[arg(short, long, default_value_t = 60)]
    fps: u32,
    /// Video codec. Default: h264 up to 1080p, hevc above.
    #[arg(long, value_enum)]
    codec: Option<Codec>,
    /// Video bitrate in Mb/s. Default scales with resolution and fps.
    #[arg(long)]
    bitrate: Option<f64>,
    /// Record system audio (the default; see --no-system-audio).
    #[arg(long, overrides_with = "no_system_audio")]
    system_audio: bool,
    /// Do not record system audio.
    #[arg(long)]
    no_system_audio: bool,
    /// Microphone: `default`, a unique ID or a name substring
    /// (default: the system default mic).
    #[arg(long, value_name = "DEVICE")]
    mic: Option<String>,
    /// Display name of the --mic device, used to find it again when its
    /// unique ID changed.
    #[arg(long, hide = true, value_name = "NAME")]
    mic_name: Option<String>,
    /// Do not record the microphone.
    #[arg(long, conflicts_with = "mic")]
    no_mic: bool,
    /// Overlay a webcam (optionally: unique ID or name substring).
    #[arg(short, long, visible_alias = "cam", value_name = "DEVICE", num_args = 0..=1, default_missing_value = "")]
    webcam: Option<String>,
    /// Scene: screen, screen-cam (picture-in-picture) or cam (webcam full
    /// frame). Default: screen-cam with a webcam, else screen. A camera
    /// scene turns the webcam on.
    #[arg(long, value_enum)]
    scene: Option<compose::Scene>,
    /// Webcam overlay width as a fraction of the frame width.
    #[arg(long, default_value_t = 0.22)]
    pip_size: f64,
    /// Snap the webcam overlay to a corner.
    #[arg(long, value_enum, default_value_t = Corner::BottomRight)]
    pip_corner: Corner,
    /// Place the overlay's center anywhere: X,Y as fractions of the frame
    /// from the top-left (e.g. 0.5,0.5). Overrides --pip-corner.
    #[arg(long, value_name = "X,Y")]
    pip_pos: Option<String>,
    #[arg(long, value_enum, default_value_t = Shape::Rounded)]
    pip_shape: Shape,
    /// Corner roundness 0..1 (rounded, square and squircle shapes).
    #[arg(long, default_value_t = 0.2)]
    pip_roundness: f64,
    /// Overlay border width in pixels at 1080p (0 = none).
    #[arg(long, default_value_t = 0.0)]
    pip_border: f64,
    /// Border color as hex RGB.
    #[arg(long, default_value = "ffffff", value_name = "RRGGBB")]
    pip_border_color: String,
    /// Soft drop shadow under the overlay.
    #[arg(long)]
    pip_shadow: bool,
    /// Do not mirror the webcam.
    #[arg(long)]
    no_mirror: bool,
    /// Hide the mouse cursor.
    #[arg(long)]
    no_cursor: bool,
    /// Highlight mouse clicks.
    #[arg(long)]
    clicks: bool,
    /// Stop automatically after this many seconds.
    #[arg(short = 't', long)]
    duration: Option<f64>,
    /// Do not print live status.
    #[arg(short, long)]
    quiet: bool,
    /// Audio tracks when recording both system audio and mic.
    #[arg(long, value_enum, default_value_t = writer::AudioLayout::Both)]
    audio_tracks: writer::AudioLayout,
    /// Shift system audio later by this many ms. Default: the output
    /// device's latency (system audio is captured before it is heard).
    #[arg(long, value_name = "MS", allow_hyphen_values = true)]
    system_audio_offset_ms: Option<f64>,
    /// Shift the microphone later by this many ms (e.g. a Bluetooth mic).
    #[arg(long, value_name = "MS", default_value_t = 0.0, allow_hyphen_values = true)]
    mic_offset_ms: f64,
    /// Keep the last SECS seconds (15-300) of encoded video and audio in
    /// memory; `rec-rs save-replay` writes them to an MP4.
    #[arg(long, value_name = "SECS")]
    replay: Option<f64>,
    /// Stream to an RTMP(S) URL or a WHIP endpoint (http(s)://.../whip).
    /// youtube, twitch and kick are shortcuts. Repeat it to stream to
    /// several at once. The key comes from $REC_STREAM_KEY or the Keychain
    /// (`rec-rs stream-key URL`).
    #[arg(long, value_name = "URL")]
    stream: Vec<String>,
    /// Do not write a recording file (replay buffer / streaming only).
    #[arg(long)]
    no_record: bool,
    /// Live encode (replay and streams) size.
    #[arg(long, default_value = "1080p")]
    live_resolution: String,
    /// Live video bitrate in kb/s. Default: the platform's recommendation
    /// for the size and frame rate (YouTube 4K60 45 Mb/s H.264 or 35 HEVC,
    /// Twitch at most 6 Mb/s, Kick at most 8), else YouTube's. With several
    /// destinations, the lowest of them.
    #[arg(long)]
    stream_bitrate: Option<u32>,
    /// Live video codec: auto (HEVC when every destination is YouTube),
    /// h264 or hevc (enhanced RTMP / WHIP H.265; falls back to H.264 for
    /// servers that refuse it).
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264", "hevc"])]
    stream_codec: String,
    /// Live keyframe interval in seconds.
    #[arg(long, default_value_t = 2.0)]
    keyframe: f64,
    /// Variable bitrate for the live encode (default: constant).
    #[arg(long)]
    vbr: bool,
    /// Live audio bitrate in kb/s (AAC; Opus for WHIP).
    #[arg(long, default_value_t = 192)]
    audio_bitrate: u32,
    /// System audio level in dB, in every track and the stream (-60 or
    /// lower is silence, at most +24).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    system_gain: f32,
    /// Microphone level in dB, in every track and the stream (-60 or lower
    /// is silence, at most +24).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    mic_gain: f32,
    /// Microphone capture backend.
    #[arg(long, value_enum, default_value_t = record::MicBackend::Sck)]
    mic_backend: record::MicBackend,
    /// Record a generated test pattern instead of the screen (self-test;
    /// needs no Screen Recording permission). The frame flashes white once a
    /// second and the "system audio" beeps at the same instant, for sync
    /// checks. Implies --mic-backend avcapture.
    #[arg(long)]
    test_pattern: bool,
    /// With --test-pattern: add random noise blocks to stress the encoder.
    #[arg(long, requires = "test_pattern")]
    noise: bool,
}

fn even(x: f64) -> usize {
    ((x / 2.0).round() as usize * 2).max(2)
}

/// Output size for `s` (a preset or WxH, the bounding box) and a source of
/// `native` pixels: the source's aspect ratio fitted inside the box, never
/// larger than the source, even dimensions. A frame with the source's shape
/// fills edge to edge: no letterbox or pillarbox bars, nothing cropped.
fn parse_size(s: &str, native: (usize, usize)) -> Result<(usize, usize)> {
    let lower = s.to_lowercase();
    // The capture's own size is the native size rounded down to even.
    let (nw, nh) = ((native.0 & !1).max(2) as f64, (native.1 & !1).max(2) as f64);
    let (bw, bh) = match lower.as_str() {
        "720p" => (1280.0, 720.0),
        "1080p" => (1920.0, 1080.0),
        "1440p" => (2560.0, 1440.0),
        "4k" | "2160p" | "uhd" => (3840.0, 2160.0),
        "native" => (nw, nh),
        _ => {
            let Some((w, h)) = lower.split_once('x') else {
                bail!("bad --resolution {s:?}: use 720p/1080p/1440p/4k/native/WxH")
            };
            (w.trim().parse::<usize>()? as f64, h.trim().parse::<usize>()? as f64)
        }
    };
    // Presets name the frame's height for landscape sources and its width
    // for portrait ones (a 1080p portrait window is 1080 wide).
    let (bw, bh) = if nh > nw && matches!(lower.as_str(), "720p" | "1080p" | "1440p" | "4k" | "2160p" | "uhd") { (bh, bw) } else { (bw, bh) };
    let k = (bw / nw).min(bh / nh).min(1.0);
    Ok((even(nw * k), even(nh * k)))
}

/// The live encode's bounding box for a preset or WxH (the encoder fits the
/// frame inside it, keeping its aspect ratio).
fn live_box(s: &str) -> Result<(usize, usize)> {
    Ok(match s.to_lowercase().as_str() {
        "720p" => (1280, 720),
        "1080p" => (1920, 1080),
        "1440p" => (2560, 1440),
        "4k" | "2160p" | "uhd" => (3840, 2160),
        other => {
            let Some((w, h)) = other.split_once('x') else {
                bail!("bad --live-resolution {s:?}: use 720p/1080p/1440p/4k/WxH")
            };
            (even(w.trim().parse::<f64>()?), even(h.trim().parse::<f64>()?))
        }
    })
}

/// `rec-rs stream-key DEST`: store (or delete) a stream key in the Keychain.
fn stream_key(dest: &str, delete: bool) -> Result<()> {
    let url = live::destination_url(dest);
    if delete {
        live::keychain::delete(&url)?;
        println!("removed the stream key for {url}");
        return Ok(());
    }
    let tty = unsafe { libc::isatty(0) } == 1;
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if tty {
        eprint!("stream key for {url}: ");
        unsafe {
            libc::tcgetattr(0, &mut old);
            let mut t = old;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
    let mut key = String::new();
    let r = std::io::stdin().read_line(&mut key);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
        eprintln!();
    }
    r?;
    let key = key.trim();
    if key.is_empty() {
        bail!("no key given");
    }
    live::keychain::set(&url, key)?;
    println!("saved the stream key for {url} in the Keychain");
    Ok(())
}

fn parse_pos(s: &str) -> Result<[f64; 2]> {
    let v: Vec<f64> = s.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>()?;
    match v[..] {
        [x, y] if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) => Ok([x, y]),
        _ => bail!("--pip-pos must be X,Y with both between 0 and 1"),
    }
}

fn parse_hex(s: &str) -> Result<[u8; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        bail!("color must be RRGGBB hex");
    }
    let c = |i: usize| u8::from_str_radix(&s[i..i + 2], 16);
    Ok([c(0)?, c(2)?, c(4)?])
}

fn default_output() -> PathBuf {
    let mut buf = [0u8; 64];
    let n = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        libc::strftime(buf.as_mut_ptr() as *mut _, buf.len(), c"rec-%Y-%m-%d-%H%M%S.mov".as_ptr(), &tm)
    };
    let name = String::from_utf8_lossy(&buf[..n]).to_string();
    let dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Movies")).unwrap_or_default();
    dir.join(name)
}

/// Turn record arguments into session options and a capture target
/// (shared by the CLI and the GUI).
pub fn prepare(a: RecordArgs) -> Result<(record::Options, Option<screen::Target>)> {
    // A missing mic falls back to the default; audio never blocks a session.
    let mut mic_warning = None;
    let mic = if a.no_mic {
        None
    } else {
        Some(a.mic.as_deref().filter(|m| *m != "default").and_then(|sel| {
            let p = camera::pick_mic(sel, a.mic_name.as_deref());
            if let Some(w) = p.warning {
                eprintln!("rec: warning: {w}");
                mic_warning = Some(w);
            }
            p.id
        }))
    };
    let mut webcam = a.webcam.map(|s| if s.is_empty() || s == "default" { None } else { Some(s) });
    let scene = a.scene.unwrap_or(if webcam.is_some() { compose::Scene::ScreenCam } else { compose::Scene::Screen });
    if scene.uses_cam() && webcam.is_none() {
        webcam = Some(None);
    }
    let mut opts = record::Options {
        output: a.output.unwrap_or_else(default_output),
        size: (0, 0),
        native: (0, 0),
        fps: a.fps,
        codec: Codec::H264,
        bitrate: 0,
        system_audio: !a.no_system_audio,
        mic,
        mic_warning,
        webcam,
        pip: PipSpec {
            size: a.pip_size.clamp(0.05, 0.9),
            place: match &a.pip_pos {
                Some(p) => compose::Place::At(parse_pos(p)?),
                None => compose::Place::Corner(a.pip_corner),
            },
            shape: a.pip_shape,
            roundness: a.pip_roundness.clamp(0.0, 1.0),
            border: a.pip_border.clamp(0.0, 64.0),
            border_color: parse_hex(&a.pip_border_color)?,
            shadow: a.pip_shadow,
            mirror: !a.no_mirror,
        },
        cursor: !a.no_cursor,
        clicks: a.clicks,
        duration: a.duration,
        quiet: a.quiet,
        test_pattern: a.test_pattern.then_some(a.noise),
        audio_layout: a.audio_tracks,
        volumes: [a.system_gain, a.mic_gain].map(|db| mixer::Volume { db, muted: false }),
        mic_backend: if a.test_pattern { record::MicBackend::Avcapture } else { a.mic_backend },
        scene,
        system_offset_ms: a.system_audio_offset_ms,
        mic_offset_ms: a.mic_offset_ms,
        write: !a.no_record,
        live: None,
        streams: vec![],
    };
    record::check_permissions(&opts)?;
    if !(1..=240).contains(&a.fps) || 60000 % a.fps != 0 {
        bail!("--fps must divide 60000 (e.g. 24, 25, 30, 50, 60, 120)");
    }
    let (target, native) = if a.test_pattern {
        (None, (3840, 2160))
    } else {
        let mut displays = screen::displays()?;
        if a.display == 0 || a.display > displays.len() {
            bail!("display {} not found: displays are numbered 1..={} (see `rec-rs devices`)", a.display, displays.len());
        }
        let d = displays.swap_remove(a.display - 1);
        let scale = d.pixels.0 as f64 / d.points.0.max(1) as f64;
        if let Some(sel) = &a.window {
            let w = screen::find_window(sel)?;
            let native = ((w.size.0 * scale) as usize, (w.size.1 * scale) as usize);
            (Some(screen::Target::Window(w)), native)
        } else if let Some(r) = &a.region {
            let v: Vec<f64> = r.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>()?;
            if v.len() != 4 || v[2] <= 0.0 || v[3] <= 0.0 {
                bail!("--region must be X,Y,WIDTH,HEIGHT (points)");
            }
            let rect = objc2_core_foundation::CGRect::new(
                objc2_core_foundation::CGPoint::new(v[0], v[1]),
                objc2_core_foundation::CGSize::new(v[2], v[3]),
            );
            let native = ((v[2] * scale) as usize, (v[3] * scale) as usize);
            (Some(screen::Target::Display(d, Some(rect))), native)
        } else {
            let native = d.pixels;
            (Some(screen::Target::Display(d, None)), native)
        }
    };
    let (w, h) = parse_size(&a.resolution, native)?;
    let codec = a.codec.unwrap_or(if w * h > 1920 * 1080 { Codec::Hevc } else { Codec::H264 });
    // Bits per pixel per frame tuned for screen content (sharp text).
    let bpp = match codec {
        Codec::H264 => 0.16,
        Codec::Hevc => 0.10,
        Codec::Prores => 0.0, // fixed quality; bitrate unused
    };
    let bitrate = a
        .bitrate
        .map(|m| (m * 1e6) as u64)
        .unwrap_or_else(|| ((w * h) as f64 * a.fps as f64 * bpp) as u64)
        .clamp(1_000_000, 400_000_000);
    if codec == Codec::Prores && opts.output.extension().is_some_and(|e| e == "mp4") {
        bail!("ProRes needs a .mov output");
    }
    opts.size = (w, h);
    opts.native = (native.0 & !1, native.1 & !1);
    if a.no_record && a.replay.is_none() && a.stream.is_empty() {
        bail!("--no-record needs --replay or --stream");
    }
    if a.replay.is_some() || !a.stream.is_empty() {
        let live = live::Live::new();
        let (lw, lh) = live_box(&a.live_resolution)?;
        let platforms: Vec<live::Platform> = a.stream.iter().map(|d| live::platform(&live::destination_url(d))).collect();
        let (preset_kbps, max_h, hevc) = live::shared_preset(&platforms, lh.min(h), a.fps.min(60), &a.stream_codec);
        let kbps = a.stream_bitrate.unwrap_or(preset_kbps);
        let (lw, lh) = if lh.min(h) > max_h { (max_h * 16 / 9, max_h) } else { (lw.min(w), lh.min(h)) };
        live.set_config(live::LiveConfig {
            width: lw,
            height: lh,
            bitrate: kbps.clamp(500, 80_000) as u64 * 1000,
            keyframe_secs: a.keyframe.clamp(0.5, 4.0),
            cbr: !a.vbr,
            audio_bitrate: a.audio_bitrate.clamp(64, 320) * 1000,
            hevc,
        });
        if !a.quiet {
            eprintln!(
                "rec: live encode {lw}x{lh} {} {:.1} Mb/s, keyframe every {} s",
                if hevc { "HEVC" } else { "H.264" },
                kbps as f64 / 1000.0,
                a.keyframe
            );
        }
        if let Some(secs) = a.replay {
            if !(15.0..=300.0).contains(&secs) {
                bail!("--replay must be 15-300 seconds");
            }
            live.set_replay(Some(secs));
        }
        for dest in &a.stream {
            opts.streams.push(live::resolve_destination(dest)?);
        }
        opts.live = Some(live);
    }
    opts.codec = codec;
    opts.bitrate = bitrate;
    if let Some(parent) = opts.output.parent()
        && !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    Ok((opts, target))
}

/// Parse a `rec record ...` argument list (used by the GUI).
pub fn parse_record_args(args: &[&str]) -> Result<RecordArgs> {
    let mut v = vec!["rec-rs", "record"];
    v.extend_from_slice(args);
    match Cli::try_parse_from(v)?.cmd {
        Some(Cmd::Record(a)) => Ok(a),
        _ => bail!("internal: not a record command"),
    }
}

fn record(a: RecordArgs) -> Result<()> {
    let (opts, target) = prepare(a)?;
    let out = opts.output.clone();
    let quiet = opts.quiet;
    let wrote = opts.write;
    let stats = record::run(opts, target)?;
    if !wrote {
        if !quiet {
            eprintln!("rec: stopped ({} video frames, {} dropped)", stats.video_frames_out, stats.video_drops);
        }
        return Ok(());
    }
    if !quiet {
        eprintln!(
            "rec: saved {} ({} video frames, {} duplicated, {} dropped; audio buffers sys {} mic {} mix {}, {} dropped, {} clipped samples)",
            out.display(),
            stats.video_frames_out,
            stats.video_dups,
            stats.video_drops,
            stats.audio_sys_buffers,
            stats.audio_mic_buffers,
            stats.audio_mix_buffers,
            stats.audio_drops,
            stats.audio_clipped_samples
        );
    }
    if std::env::var_os("REC_DEBUG").is_some() {
        let avg = |sum: f64, n: u64| if n == 0 { 0.0 } else { sum / n as f64 * 1000.0 };
        eprintln!(
            "rec: debug: encoder busy retries {}, audio gaps filled {}",
            stats.video_retries,
            stats.audio_gaps_filled
        );
        eprintln!(
            "rec: debug: frame arrival latency  screen avg {:.1} ms max {:.1} ms ({} frames)  camera avg {:.1} ms max {:.1} ms ({} frames)",
            avg(stats.screen_latency_sum, stats.screen_frames_in),
            stats.screen_latency_max * 1000.0,
            stats.screen_frames_in,
            avg(stats.cam_latency_sum, stats.cam_frames_in),
            stats.cam_latency_max * 1000.0,
            stats.cam_frames_in
        );
    }
    println!("{}", out.display());
    Ok(())
}

fn devices() -> Result<()> {
    println!("Displays:");
    match screen::displays() {
        Ok(ds) => {
            for (i, d) in ds.iter().enumerate() {
                println!(
                    "  [{}] id {}  {}x{} pixels ({}x{} points)",
                    i + 1,
                    d.id,
                    d.pixels.0, d.pixels.1, d.points.0, d.points.1
                );
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Windows (for --window):");
    match screen::windows() {
        Ok(ws) => {
            for w in ws {
                println!("  {:>6}  {} — {}  ({}x{} pt)", w.id, w.app, w.title, w.size.0, w.size.1);
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Cameras:");
    for c in camera::cameras() {
        println!("  {}  [{}]", c.name, c.id);
    }
    println!("Microphones:");
    for m in camera::microphones() {
        println!("  {}  [{}]", m.name, m.id);
    }
    Ok(())
}

fn main() {
    // Behave like a normal CLI when piped into `head` etc.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    tcc::disclaim_and_reexec();
    let cli = Cli::parse();
    let r = match cli.cmd.unwrap_or(Cmd::Gui { start_recording: false, settings: false, window: None }) {
        Cmd::Gui { start_recording, settings, window } => app::run(start_recording, settings, window),
        Cmd::Record(a) => record(a),
        Cmd::Devices => {
            let r = devices();
            if let Some(l) = outlatency::output_latency() {
                println!("Output latency (system audio is shifted by this): {:.1} ms", l * 1000.0);
            }
            r
        }
        Cmd::Stop => record::stop_running(),
        Cmd::Pause => record::signal_running(libc::SIGUSR1),
        Cmd::Resume => record::signal_running(libc::SIGUSR2),
        Cmd::SaveReplay => record::signal_running(libc::SIGINFO),
        Cmd::StreamKey { dest, delete } => stream_key(&dest, delete),
        Cmd::Permissions { request, wait } => {
            if request {
                tcc::request_all(true, true, true, std::time::Duration::from_secs(wait));
            }
            let (s, m, c) = tcc::report();
            println!("screen recording : {s}");
            println!("microphone       : {m}");
            println!("camera           : {c}");
            Ok(())
        }
        Cmd::RequestScreen => {
            tcc::request_screen_here();
            Ok(())
        }
        Cmd::Probe { file } => probe::probe(&file),
        Cmd::Play { file, secs, size } => play::play(&file, secs, &size),
    };
    if let Err(e) = r {
        eprintln!("rec: error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_command_line_parses() {
        let a = parse_record_args(&[
            "--display", "1", "--mic", "default", "--system-audio", "--cam", "--out", "file.mp4", "--duration", "10",
        ])
        .unwrap();
        assert_eq!(a.display, 1);
        assert_eq!(a.mic.as_deref(), Some("default"));
        assert!(a.system_audio && !a.no_system_audio);
        assert_eq!(a.webcam.as_deref(), Some(""));
        assert_eq!(a.output, Some(PathBuf::from("file.mp4")));
        assert_eq!(a.duration, Some(10.0));
    }

    #[test]
    fn system_audio_flags_override_each_other() {
        let a = parse_record_args(&["--system-audio", "--no-system-audio"]).unwrap();
        assert!(a.no_system_audio);
        let a = parse_record_args(&["--no-system-audio", "--system-audio"]).unwrap();
        assert!(!a.no_system_audio);
    }

    #[test]
    fn sizes() {
        // Every size keeps the source's aspect ratio (no bars), even sizes.
        let native = (3456, 2234);
        assert_eq!(parse_size("1080p", native).unwrap(), (1670, 1080));
        assert_eq!(parse_size("4K", native).unwrap(), (3342, 2160));
        assert_eq!(parse_size("native", native).unwrap(), native);
        assert_eq!(parse_size("1280x720", native).unwrap(), (1114, 720));
        assert_eq!(parse_size("1080p", (2880, 1800)).unwrap(), (1728, 1080));
        assert_eq!(parse_size("1080p", (3840, 2160)).unwrap(), (1920, 1080));
        // Never upscaled; odd sources round down to even, like the capture.
        assert_eq!(parse_size("4k", (1201, 801)).unwrap(), (1200, 800));
        // Portrait: the preset names the width.
        assert_eq!(parse_size("1080p", (2160, 3840)).unwrap(), (1080, 1920));
        for (sz, n) in [("1080p", (3456, 2234)), ("720p", (4112, 2658)), ("4k", (5120, 2880)), ("1440p", (3144, 1588))] {
            let (w, h) = parse_size(sz, n).unwrap();
            assert!(w % 2 == 0 && h % 2 == 0);
            assert!(compose::same_aspect(n.0 as f64, n.1 as f64, w as f64, h as f64), "{sz} {n:?} -> {w}x{h}");
        }
        assert!(parse_size("huge", native).is_err());
    }

    #[test]
    fn overlay_options() {
        let a = parse_record_args(&["--cam", "--pip-pos", "0.25,0.75", "--pip-shape", "squircle", "--pip-roundness", "0.6",
            "--pip-border", "8", "--pip-border-color", "#ff8800", "--pip-shadow"]).unwrap();
        assert_eq!(parse_pos(a.pip_pos.as_deref().unwrap()).unwrap(), [0.25, 0.75]);
        assert_eq!(a.pip_shape, Shape::Squircle);
        assert_eq!(parse_hex(&a.pip_border_color).unwrap(), [255, 136, 0]);
        assert!(a.pip_shadow);
        assert!(parse_pos("1.5,0").is_err());
        assert!(parse_hex("fff").is_err());
        for s in ["circle", "rounded", "square", "pill", "squircle", "rect"] {
            assert!(parse_record_args(&["--pip-shape", s]).is_ok(), "{s}");
        }
    }

    #[test]
    fn scene_option() {
        let a = parse_record_args(&["--scene", "cam"]).unwrap();
        assert_eq!(a.scene, Some(compose::Scene::Cam));
        assert!(parse_record_args(&["--scene", "nope"]).is_err());
    }
}
